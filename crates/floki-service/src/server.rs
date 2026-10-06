//! Named-pipe server: listener setup, per-client threads, request handlers.
//!
//! The pipe is created with an explicit security descriptor that grants
//! authenticated users read/write and carries a low mandatory-integrity label,
//! so an unelevated UI/CLI of the same user can connect to the elevated daemon
//! (documented trade-off, same as Everything). Each client gets one thread
//! (capped at [`MAX_CLIENTS`]) that loops `read_frame` → handle →
//! `write_frame` until EOF or `Shutdown`.

use std::collections::HashSet;
use std::io::{self, BufReader};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use floki_core::{SearchOptions, SearchResult, Sort as CoreSort, TOMBSTONE};
use floki_ntfs::FileMeta;
use floki_proto::{HitRow, Request, Response, VolumeStatus, PROTOCOL_VERSION};
use interprocess::local_socket::{prelude::*, Listener, ListenerOptions, Stream};
#[cfg(windows)]
use interprocess::os::windows::{
    local_socket::ListenerOptionsExt, security_descriptor::SecurityDescriptor,
};

use crate::state::{is_shutdown, request_shutdown, rss_bytes, PrevEntry, Shared};

/// SDDL for the pipe: generic read+write for authenticated users plus a low
/// integrity label (`NW` = no-write-up) so unelevated clients of the same
/// user can connect to the elevated daemon.
/// Accepted risk: any authenticated local user can send Rescan/Shutdown to
/// this single-user desktop daemon (documented trade-off, same as Everything).
pub const PIPE_SDDL: &str = "D:(A;;GRGW;;;AU)S:(ML;;NW;;;LW)";

/// Upper bound for the per-`client_id` `prev` cache.
pub const MAX_PREV_CLIENTS: usize = 128;

/// Cap on concurrent client threads; connections past it get an `Error`
/// frame instead of a thread (F13).
pub const MAX_CLIENTS: usize = 64;

/// Build the pipe security descriptor from [`PIPE_SDDL`].
#[cfg(windows)]
pub fn pipe_security_descriptor() -> io::Result<SecurityDescriptor> {
    let sddl = widestring::U16CString::from_str(PIPE_SDDL)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad SDDL"))?;
    SecurityDescriptor::deserialize(sddl.as_ucstr())
}

/// Create the pipe listener on `pipe_name`, with the open security descriptor.
///
/// Never falls back to the default descriptor: the default carries the
/// daemon's high integrity level, which would silently refuse unelevated
/// (medium-IL) UI/CLI clients with `ERROR_ACCESS_DENIED` while the indexer
/// looks healthy. A descriptor failure is returned so startup fails loud
/// instead of serving an unreachable pipe.
pub fn listen(pipe_name: &str) -> io::Result<Listener> {
    let name = pipe_name.to_fs_name::<interprocess::local_socket::GenericFilePath>()?;
    #[cfg(windows)]
    {
        let sd = pipe_security_descriptor()?;
        ListenerOptions::new()
            .name(name)
            .security_descriptor(sd)
            .create_sync()
    }
    #[cfg(not(windows))]
    {
        ListenerOptions::new().name(name).create_sync()
    }
}

/// Accept loop: one thread per client (up to [`MAX_CLIENTS`]), until
/// [`Shared::shutdown`].
///
/// The listener blocks in `accept`, so a fresh connection is served the
/// moment it arrives (the old 50 ms poll added up to 50 ms per new client).
/// Shutdown unblocks the accept with one throwaway local connection from
/// [`shutdown_watcher`]; interprocess 2.4.4 offers no listener-cancel API
/// (only blocking `accept` / `set_nonblocking`, see
/// <https://docs.rs/interprocess/2.4.4/interprocess/local_socket/traits/trait.Listener.html>).
pub fn serve(shared: &Arc<Shared>) -> anyhow::Result<()> {
    let listener = match listen(&shared.pipe_name) {
        Ok(listener) => listener,
        Err(e) => {
            // `GetLastError` 5 (ACCESS_DENIED) / 183 (ALREADY_EXISTS from
            // `CreateNamedPipe`) / 231 (PIPE_BUSY): the name is already held
            // by a live server. Unchanged by the blocking accept below:
            // `create_sync` fails identically.
            let code = e.raw_os_error();
            if code == Some(5)
                || code == Some(183)
                || code == Some(231)
                || e.kind() == io::ErrorKind::AlreadyExists
                || e.kind() == io::ErrorKind::PermissionDenied
            {
                eprintln!(
                    "another flokid (or a process) already owns {} — stop it or pass --pipe",
                    shared.pipe_name
                );
                std::process::exit(3);
            }
            return Err(anyhow::anyhow!(
                "cannot listen on pipe {}: {e}",
                shared.pipe_name
            ));
        }
    };
    tracing::info!(pipe = shared.pipe_name.as_str(), "listening");
    use interprocess::local_socket::traits::Listener as _;
    let watcher = Arc::clone(shared);
    if let Err(e) = std::thread::Builder::new()
        .name("flokid-shutdown-watch".to_owned())
        .spawn(move || shutdown_watcher(&watcher))
    {
        tracing::warn!(error = %e, "cannot spawn shutdown watcher");
    }
    loop {
        if is_shutdown(shared) {
            break;
        }
        match listener.accept() {
            Ok(stream) => {
                // The shutdown watcher's dummy connection: never serve it,
                // just exit (the flag check above does that next iteration;
                // breaking here skips spawning a pointless client thread).
                if is_shutdown(shared) {
                    break;
                }
                let claimed = shared.active_clients.fetch_add(1, Ordering::Relaxed);
                if claimed >= MAX_CLIENTS {
                    shared.active_clients.fetch_sub(1, Ordering::Relaxed);
                    let mut stream = stream;
                    let _ = floki_proto::write_frame(
                        &mut stream,
                        &Response::Error {
                            message: "too many clients; try again".to_owned(),
                        },
                    );
                    continue;
                }
                let worker = Arc::clone(shared);
                if let Err(e) = std::thread::Builder::new()
                    .name("flokid-client".to_owned())
                    .spawn(move || handle_client(&worker, stream))
                {
                    shared.active_clients.fetch_sub(1, Ordering::Relaxed);
                    tracing::warn!(error = %e, "cannot spawn client thread");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    Ok(())
}

/// Unblocks the blocking `accept` in [`serve`] on shutdown: polls the flag
/// (shutdown latency only, never client latency) and, once set, opens one
/// throwaway client connection to the pipe — the same
/// [`floki_proto::Client::connect`] the CLI uses. The accept loop consumes
/// the connection, sees the flag, and exits without serving it.
fn shutdown_watcher(shared: &Arc<Shared>) {
    while !is_shutdown(shared) {
        std::thread::sleep(Duration::from_millis(50));
    }
    // Best-effort, bounded: if the listener is already gone there is nothing
    // to unblock.
    for _ in 0..100 {
        match floki_proto::Client::connect() {
            Ok(_) => break,
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Whether handling a request asked for daemon shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Normal reply; keep serving.
    Reply,
    /// `Shutdown`: the caller must save the index and exit(0).
    Shutdown,
}

/// One client connection: request/response loop until EOF or error.
fn handle_client(shared: &Arc<Shared>, stream: Stream) {
    let mut reader = BufReader::new(stream);
    // `client_id`s observed on this connection; their `prev` entries are
    // dropped on disconnect to keep the map small.
    let mut seen: HashSet<u64> = HashSet::new();
    loop {
        if is_shutdown(shared) {
            break;
        }
        let request: Option<Request> = match floki_proto::read_frame(&mut reader) {
            Ok(frame) => frame,
            Err(e) => {
                tracing::debug!(error = %e, "bad frame; closing connection");
                break;
            }
        };
        let Some(request) = request else {
            break; // clean EOF
        };
        let (response, action) = handle_request(shared, request, &mut seen);
        let stream = reader.get_mut();
        if floki_proto::write_frame(stream, &response).is_err() {
            break;
        }
        if action == Action::Shutdown {
            // Reply first, then save the index and exit the process. The
            // save is single-winner (`try_claim_shutdown_save`): the main
            // thread runs the same join + save after `serve()` returns, so
            // whichever claims it first owns it; a loser returns here and
            // the loop exits on the shutdown flag. Never reached in tests
            // (no Shutdown is sent there).
            crate::daemon::shutdown_save_and_exit(shared, 0);
        }
    }
    if !seen.is_empty() {
        let mut cache = shared.prev_cache.lock().expect("prev lock poisoned");
        for id in seen {
            cache.remove(&id);
        }
    }
    shared.active_clients.fetch_sub(1, Ordering::Relaxed);
}

/// Dispatch one request (pure handling; `Shutdown` side effects are left to
/// the caller via [`Action`]).
fn handle_request(
    shared: &Arc<Shared>,
    request: Request,
    seen: &mut HashSet<u64>,
) -> (Response, Action) {
    match request {
        Request::Hello {} => (
            Response::Hello {
                protocol: PROTOCOL_VERSION,
                service_version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            Action::Reply,
        ),
        Request::Search {
            query,
            max_results,
            offset,
            sort,
            client_id,
            meta,
        } => {
            seen.insert(client_id);
            (
                handle_search(shared, &query, max_results, offset, sort, client_id, meta),
                Action::Reply,
            )
        }
        Request::Status {} => (handle_status(shared), Action::Reply),
        Request::Rescan { volume } => {
            if let Some(letter) = volume {
                if !letter.is_ascii_alphabetic() {
                    return (
                        Response::Error {
                            message: format!("invalid volume {letter:?}"),
                        },
                        Action::Reply,
                    );
                }
            }
            if let Some(letter) = volume {
                // An explicit rescan of a letter is also how a removed
                // volume is added back: lift its auto-include exclusion.
                set_volume_excluded(shared, letter, false);
            }
            shared
                .rescan_queue
                .lock()
                .expect("rescan lock poisoned")
                .push(volume.map(|c| c.to_ascii_uppercase()));
            (Response::Ok {}, Action::Reply)
        }
        Request::VolumesSetEnabled { volume, enabled } => (
            handle_volumes_set_enabled(shared, volume, enabled),
            Action::Reply,
        ),
        Request::VolumesSetMonitor { volume, monitor } => (
            handle_volumes_set_monitor(shared, volume, monitor),
            Action::Reply,
        ),
        Request::VolumesRemove { volume } => (handle_volumes_remove(shared, volume), Action::Reply),
        Request::TargetsConfigGet {} => (handle_targets_get(shared), Action::Reply),
        Request::TargetsConfigSet {
            auto_include_fixed,
            auto_include_removable,
            auto_remove_offline,
        } => (
            handle_targets_set(
                shared,
                auto_include_fixed,
                auto_include_removable,
                auto_remove_offline,
            ),
            Action::Reply,
        ),
        Request::Shutdown {} => {
            request_shutdown(shared);
            (Response::Ok {}, Action::Shutdown)
        }
    }
}

/// Validate a volume letter the same way `Rescan` does: ASCII alpha in,
/// uppercase out. `None` on invalid input (caller replies `Error`).
fn valid_volume(volume: char) -> Option<char> {
    if volume.is_ascii_alphabetic() {
        Some(volume.to_ascii_uppercase())
    } else {
        None
    }
}

/// Set a volume's search-visibility flag. Unknown letter → `Error`.
fn handle_volumes_set_enabled(shared: &Arc<Shared>, volume: char, enabled: bool) -> Response {
    let Some(letter) = valid_volume(volume) else {
        return Response::Error {
            message: format!("invalid volume {volume:?}"),
        };
    };
    let mut index = shared.index.write().expect("index lock poisoned");
    let Some(vol) = index.volumes.iter_mut().find(|v| v.letter == letter) else {
        return Response::Error {
            message: format!("unknown volume {letter}"),
        };
    };
    vol.enabled = enabled;
    Response::Ok {}
}

/// Set a volume's monitor flag. Disabling stops its live tail (the tail loop
/// exits on its next tick); enabling respawns it via the daemon's tail
/// spawner through a rescan-queue nudge is unnecessary — the loop below
/// spawns directly through the shared tail-handle table.
fn handle_volumes_set_monitor(shared: &Arc<Shared>, volume: char, monitor: bool) -> Response {
    let Some(letter) = valid_volume(volume) else {
        return Response::Error {
            message: format!("invalid volume {volume:?}"),
        };
    };
    {
        let mut index = shared.index.write().expect("index lock poisoned");
        let Some(vol) = index.volumes.iter_mut().find(|v| v.letter == letter) else {
            return Response::Error {
                message: format!("unknown volume {letter}"),
            };
        };
        vol.monitor = monitor;
    }
    if monitor {
        crate::daemon::spawn_tail_for_pipe(shared, letter);
    }
    Response::Ok {}
}

/// Set or clear the user-removed exclusion for `letter` in both policy
/// copies (lock-free mirror + persisted index block). No save here.
fn set_volume_excluded(shared: &Arc<Shared>, letter: char, excluded: bool) {
    let targets = {
        let mut targets = shared.targets.write().expect("targets lock poisoned");
        targets.set_excluded(letter, excluded);
        *targets
    };
    shared.index.write().expect("index lock poisoned").targets = targets;
}

/// Drop a volume record plus all its entries, and exclude its letter from
/// auto-include so the arrival poll does not add it straight back.
/// Unknown letter → `Error`. Refused while a drive scan runs (scans cache
/// their volume index, which removal shifts) rather than blocking the
/// client for the rest of the scan.
fn handle_volumes_remove(shared: &Arc<Shared>, volume: char) -> Response {
    let Some(letter) = valid_volume(volume) else {
        return Response::Error {
            message: format!("invalid volume {volume:?}"),
        };
    };
    let _scan_guard = match shared.scan_lock.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return Response::Error {
                message: format!(
                    "a drive is being scanned; remove {letter}: once the scan finishes"
                ),
            };
        }
    };
    if !crate::daemon::remove_volume_off_lock(shared, letter) {
        return Response::Error {
            message: format!("unknown volume {letter}"),
        };
    }
    set_volume_excluded(shared, letter, true);
    shared
        .prev_cache
        .lock()
        .expect("prev lock poisoned")
        .clear();
    crate::daemon::save_index(shared);
    Response::Ok {}
}

/// Read the global targets policy.
fn handle_targets_get(shared: &Arc<Shared>) -> Response {
    let targets = *shared.targets.read().expect("targets lock poisoned");
    Response::TargetsConfig {
        auto_include_fixed: targets.auto_include_fixed,
        auto_include_removable: targets.auto_include_removable,
        auto_remove_offline: targets.auto_remove_offline,
    }
}

/// Replace the global targets policy (both the lock-free copy and the
/// persisted index copy), then save.
fn handle_targets_set(
    shared: &Arc<Shared>,
    auto_include_fixed: bool,
    auto_include_removable: bool,
    auto_remove_offline: bool,
) -> Response {
    let targets = {
        let mut stored = shared.targets.write().expect("targets lock poisoned");
        // The pipe policy carries the three flags only; user-removed
        // letters are daemon state and survive a policy change.
        *stored = floki_core::TargetsConfig {
            auto_include_fixed,
            auto_include_removable,
            auto_remove_offline,
            excluded: stored.excluded,
        };
        *stored
    };
    {
        let mut index = shared.index.write().expect("index lock poisoned");
        index.targets = targets;
    }
    crate::daemon::save_index(shared);
    Response::TargetsConfig {
        auto_include_fixed,
        auto_include_removable,
        auto_remove_offline,
    }
}

/// Map proto sort order onto the core sort order.
fn map_sort(sort: floki_proto::Sort) -> CoreSort {
    match sort {
        floki_proto::Sort::NameAsc => CoreSort::NameAsc,
        floki_proto::Sort::NameDesc => CoreSort::NameDesc,
        floki_proto::Sort::PathAsc => CoreSort::PathAsc,
        floki_proto::Sort::PathDesc => CoreSort::PathDesc,
        floki_proto::Sort::ModifiedAsc => CoreSort::ModifiedAsc,
        floki_proto::Sort::ModifiedDesc => CoreSort::ModifiedDesc,
        floki_proto::Sort::CreatedAsc => CoreSort::CreatedAsc,
        floki_proto::Sort::CreatedDesc => CoreSort::CreatedDesc,
    }
}

/// Time sorts read each match's times from disk; past this many matches
/// they refuse instead of reading for minutes.
pub const TIME_SORT_MAX: usize = 200_000;

/// Wall-clock budget for a time sort's disk reads (cold or spinning disks).
const TIME_SORT_BUDGET: Duration = Duration::from_secs(4);

/// Which file time a time sort keys on, and whether newest comes first.
#[derive(Clone, Copy)]
enum TimeOrder {
    Modified { desc: bool },
    Created { desc: bool },
}

impl TimeOrder {
    fn of(sort: floki_proto::Sort) -> Option<Self> {
        use floki_proto::Sort;
        match sort {
            Sort::ModifiedAsc => Some(Self::Modified { desc: false }),
            Sort::ModifiedDesc => Some(Self::Modified { desc: true }),
            Sort::CreatedAsc => Some(Self::Created { desc: false }),
            Sort::CreatedDesc => Some(Self::Created { desc: true }),
            _ => None,
        }
    }

    fn key(self, meta: Option<&FileMeta>) -> Option<i64> {
        let meta = meta?;
        match self {
            Self::Modified { .. } => meta.modified_ms,
            Self::Created { .. } => meta.created_ms,
        }
    }

    fn desc(self) -> bool {
        match self {
            Self::Modified { desc } | Self::Created { desc } => desc,
        }
    }
}

/// Time-sort comparator: unreadable (`None`) sorts last in both directions.
fn cmp_time_key(a: Option<i64>, b: Option<i64>, desc: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) if desc => y.cmp(&x),
        (Some(x), Some(y)) => x.cmp(&y),
    }
}

/// Disk-read threads: half the cores (the search CPU budget), 1..=8.
fn stat_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(2, |n| n.get() / 2)
        .clamp(1, 8)
}

/// Read metadata for every path in parallel. `None` when `deadline` passed
/// before all were read (the caller must not sort on a partial set).
fn stat_all(paths: &[String], deadline: Option<Instant>) -> Option<Vec<Option<FileMeta>>> {
    if paths.is_empty() {
        return Some(Vec::new());
    }
    let chunk = paths.len().div_ceil(stat_threads()).max(1);
    let late = std::sync::atomic::AtomicBool::new(false);
    let parts: Vec<Vec<Option<FileMeta>>> = std::thread::scope(|s| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|part| {
                let late = &late;
                s.spawn(move || {
                    let mut out = Vec::with_capacity(part.len());
                    for (i, path) in part.iter().enumerate() {
                        if i % 64 == 0 && deadline.is_some_and(|d| Instant::now() > d) {
                            late.store(true, Ordering::Relaxed);
                        }
                        if late.load(Ordering::Relaxed) {
                            out.push(None);
                            continue;
                        }
                        out.push(floki_ntfs::file_meta(path));
                    }
                    out
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("stat thread panicked"))
            .collect()
    });
    if late.load(Ordering::Relaxed) {
        return None;
    }
    Some(parts.concat())
}

/// Fill size and times on `rows` (off the index lock; one page at most).
fn fill_meta(rows: &mut [HitRow]) {
    let paths: Vec<String> = rows.iter().map(|r| full_path(&r.path, &r.name)).collect();
    let Some(metas) = stat_all(&paths, None) else {
        return;
    };
    for (row, meta) in rows.iter_mut().zip(metas) {
        if let Some(m) = meta {
            row.size = if row.is_dir { None } else { m.size };
            row.modified_ms = m.modified_ms;
            row.created_ms = m.created_ms;
        }
    }
}

/// A result row without metadata.
fn bare_row(name: String, path: String, is_dir: bool) -> HitRow {
    HitRow {
        name,
        path,
        is_dir,
        size: None,
        modified_ms: None,
        created_ms: None,
    }
}

/// Split a full path into (parent folder, name); a drive root keeps its `\`.
fn split_full(full: &str) -> (String, String) {
    match full.rsplit_once('\\') {
        Some((dir, name)) if dir.ends_with(':') => (format!("{dir}\\"), name.to_owned()),
        Some((dir, name)) => (dir.to_owned(), name.to_owned()),
        None => (String::new(), full.to_owned()),
    }
}

/// Search + row building (names and paths only) under ONE read lock; no
/// disk I/O in here. Returns the result, its rows, and the index epoch.
fn indexed_search(
    shared: &Shared,
    query: &floki_core::Query,
    prev: Option<&PrevEntry>,
    opts: &SearchOptions,
) -> (SearchResult, Vec<HitRow>, u64) {
    let index = shared.index.read().expect("index lock poisoned");
    let epoch = index.epoch();
    // F1: `compact()`/`load()` renumber every id, so a cached hit list is
    // usable only while its epoch matches the live index.
    let prev = prev
        .filter(|entry| entry.epoch == epoch)
        .map(|entry| entry.hits.as_slice());
    let result = floki_core::search_paged(&index, query, opts, prev);
    let mut rows = Vec::with_capacity(result.hits.len());
    for hit in &result.hits {
        let Some(entry) = index.entry(hit.id) else {
            continue;
        };
        if entry.flags & TOMBSTONE != 0 {
            continue;
        }
        let name = index.name(hit.id).unwrap_or("").to_owned();
        let vol = index.volume_of(hit.id).unwrap_or(hit.vol);
        // F6: NTFS deletes a directory only once it is empty, so live
        // children always carry their own journal Deletes; a hit whose
        // parent link dangles anyway is dropped instead of returning a
        // bare/truncated name.
        if entry.frn != entry.parent_frn && index.lookup(vol, entry.parent_frn).is_none() {
            continue;
        }
        let path = index
            .lookup(vol, entry.parent_frn)
            .map(|parent| index.path(parent))
            .unwrap_or_else(|| parent_fallback(&index, hit.id));
        rows.push(bare_row(name, path, entry.is_dir()));
    }
    (result, rows, epoch)
}

/// Sort by a file time: collect matches and their paths under the read
/// lock, release it, read every match's times in parallel within
/// [`TIME_SORT_BUDGET`], then sort and page. The index stores no times, so
/// this is the one search that touches the disk per match; it refuses past
/// [`TIME_SORT_MAX`] matches or the budget rather than stall. (It used to
/// stat every match under the lock, freezing every search and journal
/// update for minutes on a broad query.)
fn time_sorted_search(
    shared: &Shared,
    query: &floki_core::Query,
    prev: Option<&PrevEntry>,
    order: TimeOrder,
    offset: u32,
    max_results: u32,
) -> Result<(SearchResult, Vec<HitRow>, u64), String> {
    let (epoch, hits, paths, dirs) = {
        let index = shared.index.read().expect("index lock poisoned");
        let epoch = index.epoch();
        let prev = prev
            .filter(|entry| entry.epoch == epoch)
            .map(|entry| entry.hits.as_slice());
        let hits = floki_core::collect_matches(&index, query, prev);
        if hits.len() > TIME_SORT_MAX {
            return Err(format!(
                "Too many matches to sort by date ({}). Narrow the search, or sort by name.",
                hits.len()
            ));
        }
        let paths = floki_core::paths_of(&index, &hits);
        let dirs: Vec<bool> = hits
            .iter()
            .map(|h| index.entry(h.id).is_some_and(|e| e.is_dir()))
            .collect();
        (epoch, hits, paths, dirs)
    };
    let metas = stat_all(&paths, Some(Instant::now() + TIME_SORT_BUDGET)).ok_or_else(|| {
        format!(
            "Sorting {} matches by date took too long (slow disk). Narrow the search, or \
             sort by name.",
            hits.len()
        )
    })?;
    let mut ranked: Vec<usize> = (0..hits.len()).collect();
    ranked.sort_by(|&a, &b| {
        cmp_time_key(
            order.key(metas[a].as_ref()),
            order.key(metas[b].as_ref()),
            order.desc(),
        )
        .then(hits[a].id.cmp(&hits[b].id))
    });
    let max = if max_results == 0 {
        usize::MAX
    } else {
        max_results as usize
    };
    let page: Vec<usize> = ranked.into_iter().skip(offset as usize).take(max).collect();
    let rows = page
        .iter()
        .map(|&i| {
            let (dir, name) = split_full(&paths[i]);
            let mut row = bare_row(name, dir, dirs[i]);
            if let Some(m) = metas[i] {
                row.size = if dirs[i] { None } else { m.size };
                row.modified_ms = m.modified_ms;
                row.created_ms = m.created_ms;
            }
            row
        })
        .collect();
    let result = SearchResult {
        total: hits.len() as u64,
        hits: page.iter().map(|&i| hits[i]).collect(),
    };
    Ok((result, rows, epoch))
}

/// Search with `prev` reuse when the cached query qualifies and its epoch is
/// current. The index read lock covers only the in-memory search and path
/// building; size/time reads happen after it is released.
fn handle_search(
    shared: &Arc<Shared>,
    query_text: &str,
    max_results: u32,
    offset: u32,
    sort: floki_proto::Sort,
    client_id: u64,
    meta: bool,
) -> Response {
    // Cap concurrent in-flight searches: extra requests wait here instead of
    // oversubscribing the machine.
    let _search_slot = shared.acquire_search_slot();
    let started = Instant::now();
    let query = floki_core::parse(query_text);
    let cached: Option<PrevEntry> = {
        let cache = shared.prev_cache.lock().expect("prev lock poisoned");
        cache
            .get(&client_id)
            .filter(|entry| entry.complete && floki_core::prev_reusable(&entry.query, &query))
            .map(|entry| PrevEntry {
                query: entry.query.clone(),
                hits: entry.hits.clone(),
                complete: entry.complete,
                epoch: entry.epoch,
            })
    };
    let outcome = match TimeOrder::of(sort) {
        Some(order) => {
            time_sorted_search(shared, &query, cached.as_ref(), order, offset, max_results)
        }
        None => {
            let opts = SearchOptions::new(max_results, offset, map_sort(sort));
            let (result, mut rows, epoch) = indexed_search(shared, &query, cached.as_ref(), &opts);
            if meta {
                fill_meta(&mut rows);
            }
            Ok((result, rows, epoch))
        }
    };
    let (result, rows, epoch) = match outcome {
        Ok(found) => found,
        Err(message) => return Response::Error { message },
    };
    let elapsed_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    // F9: a paged (`offset > 0`) or truncated page must never count as
    // complete, or the next narrowing would search inside one page only.
    let complete = offset == 0 && (max_results == 0 || result.total <= u64::from(max_results));
    {
        let mut cache = shared.prev_cache.lock().expect("prev lock poisoned");

        if cache.len() >= MAX_PREV_CLIENTS && !cache.contains_key(&client_id) {
            // Defensive bound: evict one arbitrary entry rather than growing
            // without limit when many distinct client_ids appear.
            if let Some(victim) = cache.keys().next().copied() {
                cache.remove(&victim);
            }
        }
        cache.insert(
            client_id,
            PrevEntry {
                query,
                hits: result.hits.clone(),
                complete,
                epoch,
            },
        );
    }
    Response::Results {
        total: result.total,
        hits: rows,
        elapsed_us,
    }
}

/// Parent directory of `id`'s full path (fallback when the parent entry is
/// not indexed).
fn parent_fallback(index: &floki_core::Index, id: floki_core::EntryId) -> String {
    let full = index.path(id);
    match full.rfind('\\') {
        Some(0) | None => full,
        Some(i) => full[..i].to_owned(),
    }
}
/// Join a parent directory and a file name; `path` may already end with `\`
/// (drive roots like `C:\`), so only add a separator when it is missing.
fn full_path(dir: &str, name: &str) -> String {
    if dir.ends_with('\\') || dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

/// Status snapshot: live entries, per-volume cursors, RSS, uptime, state.
fn handle_status(shared: &Arc<Shared>) -> Response {
    let index = shared.index.read().expect("index lock poisoned");
    let mut per_vol = vec![0u64; index.volumes.len()];
    let mut live_total = 0u64;
    for (id, entry) in index.entries.iter().enumerate() {
        if entry.flags & TOMBSTONE != 0 {
            continue;
        }
        live_total += 1;
        let vol = index.volume_of(id as floki_core::EntryId).unwrap_or(0) as usize;
        if let Some(slot) = per_vol.get_mut(vol) {
            *slot += 1;
        }
    }
    // `live` = tail thread claimed AND its volume handle currently open:
    // a tail retrying an absent/unopenable drive reports Offline (honest),
    // and recovers to Live on the next successful open.
    let live = shared.open_vols.lock().expect("open lock poisoned");
    let volumes: Vec<VolumeStatus> = index
        .volumes
        .iter()
        .enumerate()
        .map(|(i, v)| VolumeStatus {
            letter: v.letter,
            entries: per_vol.get(i).copied().unwrap_or(0),
            next_usn: v.next_usn,
            live: live.contains(&v.letter),
            enabled: v.enabled,
            monitor: v.monitor,
        })
        .collect();
    drop(live);
    drop(index);
    Response::Status {
        entries: live_total,
        volumes,
        rss_bytes: rss_bytes(),
        uptime_s: shared.start.elapsed().as_secs(),
        state: shared.current_state(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floki_core::{Index, Volume, DIRECTORY};

    #[test]
    fn time_order_puts_unreadable_last_both_ways() {
        use std::cmp::Ordering;
        assert_eq!(cmp_time_key(Some(1), Some(2), false), Ordering::Less);
        assert_eq!(cmp_time_key(Some(1), Some(2), true), Ordering::Greater);
        assert_eq!(cmp_time_key(None, Some(2), false), Ordering::Greater);
        assert_eq!(cmp_time_key(None, Some(2), true), Ordering::Greater);
    }

    #[test]
    fn split_full_keeps_the_root_separator() {
        assert_eq!(
            split_full(r"C:\a\b.txt"),
            (r"C:\a".to_owned(), "b.txt".to_owned())
        );
        assert_eq!(
            split_full(r"C:\b.txt"),
            (r"C:\".to_owned(), "b.txt".to_owned())
        );
    }

    #[test]
    fn stat_all_refuses_a_blown_deadline() {
        let paths = vec![env!("CARGO_MANIFEST_DIR").to_owned(); 200];
        assert!(stat_all(&paths, Some(Instant::now() - Duration::from_secs(1))).is_none());
        let read = stat_all(&paths, None).expect("no deadline");
        assert_eq!(read.len(), 200);
        assert!(read.iter().all(|m| m.is_some_and(|m| m.is_dir)));
    }

    /// Index whose entries mirror a real folder on disk: every ancestor of
    /// `dir` plus `files` inside it, so time sorts read real timestamps.
    fn mirror_index(dir: &std::path::Path, files: &[&str]) -> Index {
        let full = dir.to_string_lossy().into_owned();
        let letter = full.chars().next().expect("drive letter");
        let mut ix = Index::new();
        ix.add_volume(Volume {
            letter,
            guid: [0; 16],
            journal_id: 1,
            next_usn: 0,
            root_frn: 5,
            enabled: true,
            monitor: true,
        });
        ix.push(0, 5, 5, "", DIRECTORY);
        let mut parent = 5u64;
        let mut frn = 100u64;
        for part in full[3..]
            .split(std::path::MAIN_SEPARATOR)
            .filter(|p| !p.is_empty())
        {
            ix.push(0, frn, parent, part, DIRECTORY);
            parent = frn;
            frn += 1;
        }
        for name in files {
            ix.push(0, frn, parent, name, 0);
            frn += 1;
        }
        ix.finalize();
        ix.rebuild_by_name();
        ix
    }

    /// Date sort reads real file times off the index lock and orders by
    /// them, newest first for `ModifiedDesc`, with metadata on every row.
    #[test]
    fn time_sort_orders_real_files_by_modified() {
        let dir = std::env::temp_dir().join(format!("floki-timesort-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let base = std::time::SystemTime::now() - Duration::from_secs(3600);
        let names = ["zz-old.flk", "aa-new.flk", "mm-mid.flk"];
        for (name, age) in names.iter().zip([300u64, 0, 100]) {
            let file = std::fs::File::create(dir.join(name)).expect("create");
            file.set_modified(base - Duration::from_secs(age))
                .expect("set mtime");
        }
        let shared = Shared::new(dir.join("unused.bin"), "unused".to_owned());
        *shared.index.write().expect("index lock poisoned") = mirror_index(&dir, &names);
        let query = floki_core::parse("ext:flk");
        let (result, rows, _) = time_sorted_search(
            &shared,
            &query,
            None,
            TimeOrder::Modified { desc: true },
            0,
            10,
        )
        .expect("sorted");
        let order: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(order, ["aa-new.flk", "mm-mid.flk", "zz-old.flk"]);
        assert_eq!(result.total, 3);
        assert!(rows
            .iter()
            .all(|r| r.modified_ms.is_some() && r.size == Some(0)));
        assert_eq!(rows[0].path, dir.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
