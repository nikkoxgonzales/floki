//! `flokid run`: logging, index load, volume bootstrap, journal tails,
//! persistence, pipe server, Ctrl-C handling.
//!
//! Startup never blocks serving: the pipe listener starts immediately while
//! each target volume is bootstrapped (journal replay or full scan) on its
//! own thread. All index mutation goes through [`Shared::index`] under short
//! write locks; searches and status read concurrently.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use floki_core::{Index, Volume, PENDING_MAX};
use floki_ntfs::{
    is_elevated, list_indexable_volumes, JournalInfo, NtfsError, RawRecord, UsnEvent, VolumeHandle,
};
use windows_sys::Win32::Foundation::{FALSE, TRUE};
use windows_sys::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT,
};

use crate::mapping::{apply_usn_event, attrs_to_flags, is_ntfs_metafile};
use crate::paths::{ensure_data_dir, index_path};
use crate::server;
use crate::state::{is_shutdown, request_shutdown, try_claim_shutdown_save, ScanProgress, Shared};

/// Journal tail poll cadence.
pub const TAIL_INTERVAL: Duration = Duration::from_millis(750);
/// Index persistence cadence. Each save rewrites the whole index file
/// (~500 MB at 9M entries), so it is kept well inside the journal's replay
/// window (256 MB of USN records, hours of activity) without hammering the
/// SSD; unchanged indexes are not rewritten at all.
pub const SAVE_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Scan records pushed per write lock.
pub const SCAN_BATCH: usize = 10_000;
/// Journal size created when a volume has none, and the floor enforced on
/// every open: 256 MiB max, 16 MiB delta (what Everything uses). A dev box
/// running cargo builds wraps a smaller journal in about an hour, forcing a
/// full rescan on every boot.
pub const JOURNAL_MAX_SIZE: u64 = 256 * 1024 * 1024;
/// Journal allocation delta for [`JOURNAL_MAX_SIZE`].
pub const JOURNAL_DELTA: u64 = 16 * 1024 * 1024;

/// Volumes whose latest scan did not run to completion (aborted at shutdown).
/// In-memory only: while this set (or [`Shared::active_scans`]) is non-empty,
/// [`save_index`] refuses to persist, so a partial volume is never stored as
/// ready. A volume leaves the set on its next successful scan commit.
/// Core offers no per-volume save, so whole-save refusal is the correct
/// granularity: persisting would bless a stale `next_usn` that the next boot
/// would replay as if complete, silently dropping the never-scanned files.
static INCOMPLETE_VOLS: LazyLock<Mutex<HashSet<char>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Serializes [`save_index`]: `Index::save` writes a fixed `<path>.tmp`
/// before renaming, so two concurrent saves interleave writes and can
/// persist a corrupt `index.bin`. The shutdown path is single-winner via
/// [`try_claim_shutdown_save`], but the periodic saver, pipe handlers
/// (`VolumesRemove`, `TargetsConfigSet`, offline-volume drop), and the
/// shutdown save can still overlap — this mutex makes overlap impossible.
static SAVE_LOCK: Mutex<()> = Mutex::new(());

/// What the last successful save persisted: entry mutation counter plus
/// the volume records and targets policy (cursors and flags change without
/// bumping the counter). A save whose fingerprint matches is skipped.
type SaveFingerprint = (u64, Vec<Volume>, floki_core::TargetsConfig);
static LAST_SAVED: Mutex<Option<SaveFingerprint>> = Mutex::new(None);

/// Flag `letter` as not fully scanned (aborted scan). Public so integration
/// tests can drive the save-guard without a real volume.
pub fn mark_volume_incomplete(letter: char) {
    INCOMPLETE_VOLS
        .lock()
        .expect("incomplete lock poisoned")
        .insert(letter);
}

/// Clear `letter`'s incomplete flag after a successful scan commit. Public so
/// integration tests can drive the save-guard without a real volume.
pub fn clear_volume_incomplete(letter: char) {
    INCOMPLETE_VOLS
        .lock()
        .expect("incomplete lock poisoned")
        .remove(&letter);
}

/// Why [`save_index`] must refuse right now, if it must: a scan in progress,
/// or a volume whose scan never completed.
fn save_blocked_reason(shared: &Shared) -> Option<String> {
    {
        let scans = shared.active_scans.lock().expect("scans lock poisoned");
        if let Some(scan) = scans.first() {
            return Some(format!(
                "volume {} scan in progress ({} records)",
                scan.letter,
                scan.done.load(Ordering::Relaxed)
            ));
        }
    }
    let incomplete = INCOMPLETE_VOLS.lock().expect("incomplete lock poisoned");
    if incomplete.is_empty() {
        None
    } else {
        let mut letters: Vec<char> = incomplete.iter().copied().collect();
        letters.sort_unstable();
        Some(format!(
            "volumes not fully scanned: {}",
            letters.iter().collect::<String>()
        ))
    }
}

/// Message printed when `run` is started without elevation.
pub const NOT_ELEVATED_MSG: &str = concat!(
    "flokid run requires elevation (Administrator): the NTFS change journal cannot be read otherwise.\n",
    "Start it elevated: right-click your terminal and choose \"Run as administrator\", then run: flokid run\n",
    "To start flokid automatically at logon without a UAC prompt, run once from an elevated shell: flokid install",
);

/// Search-thread budget: half the cores (at least 2), so a search burst may
/// use up to half the machine for the life of one query without starving it.
fn search_thread_budget() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores / 2).max(2)
}

/// Drop the whole process to below-normal priority at startup: a scan must
/// never lag the desktop. Best-effort — failure only logs.
fn lower_process_priority() {
    use windows_sys::Win32::Foundation::FALSE;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS,
    };
    // SAFETY: pseudo-handle needs no cleanup; the call is synchronous and
    // harmless on failure.
    let ok = unsafe { SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS) };
    if ok == FALSE {
        tracing::warn!(
            target: "flokid",
            "SetPriorityClass(BELOW_NORMAL) failed; running at normal priority"
        );
    }
}

/// Global mutex prefix serializing `flokid run` per pipe name: the second
/// instance on the same pipe exits instead of serving a rival pipe that
/// would orphan the first indexer's in-flight scans and journal tails.
pub const SINGLETON_MUTEX: &str = r"Global\FlokiIndexer";

/// Mutex name for `pipe`: the default pipe uses [`SINGLETON_MUTEX`]; custom
/// pipes (tests, `--pipe`) hash the name so they never collide with the
/// production indexer or each other.
#[must_use]
pub fn singleton_name(pipe: &str) -> String {
    if pipe == floki_proto::PIPE_NAME {
        SINGLETON_MUTEX.to_owned()
    } else {
        // FNV-1a: no extra deps for a test/pipe tag.
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0100_0000_01b3;
        let mut h = OFFSET;
        for b in pipe.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(PRIME);
        }
        format!("{SINGLETON_MUTEX}-{h:x}")
    }
}

/// Narrow result of [`claim_singleton`]: held handle or "already running".
#[derive(Debug, PartialEq, Eq)]
pub enum Singleton {
    /// This process owns the indexer slot; keep the handle alive in `run`.
    Held {
        /// Raw mutex handle; closed by [`release_singleton`].
        handle: usize,
    },
    /// Another `flokid` already holds the slot.
    AlreadyRunning,
}

/// Try to become the single indexer. Windowless (works from `--hidden`);
/// never blocks. Public so unit tests drive the contract without a volume.
#[cfg(windows)]
#[must_use]
pub fn claim_singleton(name: &str) -> Singleton {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: NUL-terminated name; default security; no initial owner
    // (ownership is implied by "created", not by holding the mutex).
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
    if handle.is_null() {
        // Cannot prove exclusivity (e.g. handle quota): fail closed.
        return Singleton::AlreadyRunning;
    }
    // SAFETY: just created; reading the thread-local last-error code.
    let owned = unsafe { GetLastError() } != ERROR_ALREADY_EXISTS;
    if owned {
        Singleton::Held {
            handle: handle as usize,
        }
    } else {
        // SAFETY: handle from a successful create; not used afterwards.
        unsafe {
            CloseHandle(handle);
        }
        Singleton::AlreadyRunning
    }
}

/// Non-Windows stub: single-process tests only.
#[cfg(not(windows))]
#[must_use]
pub fn claim_singleton(_name: &str) -> Singleton {
    Singleton::Held { handle: 1 }
}

/// Release a handle from [`claim_singleton`] (end of `run`).
#[cfg(windows)]
pub fn release_singleton(handle: usize) {
    use windows_sys::Win32::Foundation::CloseHandle;
    // SAFETY: handle came from `claim_singleton`; called once.
    unsafe {
        CloseHandle(handle as *mut core::ffi::c_void);
    }
}

/// Non-Windows stub: nothing to close.
#[cfg(not(windows))]
pub fn release_singleton(_handle: usize) {}

/// Keeps the singleton mutex alive for the whole `run`; closing the last
/// handle is what frees the name for the next launch.
struct SingletonGuard(usize);

impl Drop for SingletonGuard {
    fn drop(&mut self) {
        release_singleton(self.0);
    }
}

/// Hide the console window for UI-owned launches (`--hidden`). Plain
/// `flokid run` keeps its console; only the UI passes this flag.
#[cfg(windows)]
fn hide_console() {
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    // SAFETY: handle from the OS (possibly null); call is synchronous.
    unsafe {
        let hwnd = GetConsoleWindow();
        if !hwnd.is_null() {
            ShowWindow(hwnd, SW_HIDE);
        }
    }
}

#[cfg(not(windows))]
fn hide_console() {}

/// Lower the calling scan thread to the lowest priority. Scan threads are
/// dedicated (boot threads exit; the rescan worker only rescans), so no
/// restore is needed.
fn lower_scan_thread_priority() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_LOWEST,
    };
    // SAFETY: pseudo-handle, synchronous, no cleanup.
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
    }
}

/// Lower a journal tail thread to below-normal priority (tails are periodic
/// background polls, not latency-sensitive).
fn lower_tail_thread_priority() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };
    // SAFETY: pseudo-handle, synchronous, no cleanup.
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// RAII background-processing mode for an enumeration body: lowers disk and
/// memory priority too. Each BEGIN pairs with exactly one END — the [`Drop`]
/// impl guarantees it even on early return.
struct BackgroundMode;

impl BackgroundMode {
    fn enter() -> Self {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
        };
        // SAFETY: pseudo-handle, synchronous; a 0 return only means normal
        // I/O priority, which is safe to ignore.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
        }
        BackgroundMode
    }
}

impl Drop for BackgroundMode {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_END,
        };
        // SAFETY: paired with the BEGIN in `enter`.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END);
        }
    }
}

/// One-line RAM breakdown in MB for observability; logged at startup and per
/// save (see [`log_memory_breakdown`]).
fn memory_breakdown_line(index: &Index) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let b = index.memory_breakdown();
    format!(
        "entries={:.1}MB arena={:.1}MB by_name={:.1}MB frn_index={:.1}MB pending={:.1}MB tombstones={:.1}MB total={:.1}MB",
        b.entries_bytes as f64 / MB,
        b.arena_bytes as f64 / MB,
        b.by_name_bytes as f64 / MB,
        b.frn_index_bytes as f64 / MB,
        b.pending_bytes as f64 / MB,
        b.tombstone_bytes as f64 / MB,
        b.total_bytes() as f64 / MB,
    )
}

/// Log the index RAM breakdown at info level.
fn log_memory_breakdown(index: &Index) {
    tracing::info!(target: "flokid", "memory: {}", memory_breakdown_line(index));
}

/// Log fold/compact/sort work at info when it exceeds 200 ms (a stall the
/// user could feel); shorter work is covered by the caller's debug logs.
fn log_slow_op(op: &'static str, letter: Option<char>, started: Instant) {
    let elapsed_ms = started.elapsed().as_millis();
    if elapsed_ms > 200 {
        match letter {
            Some(letter) => tracing::info!(
                target: "flokid",
                op,
                volume = %letter,
                elapsed_ms,
                "slow index op"
            ),
            None => tracing::info!(target: "flokid", op, elapsed_ms, "slow index op"),
        }
    }
}

/// Options for [`run`] (mirrors the `flokid run` CLI flags).
pub struct RunOptions {
    /// Restrict to these volumes (`None` = all NTFS volumes).
    pub volumes: Option<Vec<char>>,
    /// Skip loading `index.bin` even when present.
    pub no_load: bool,
    /// Override the pipe name (`None` = `FLOKI_PIPE` env or default).
    pub pipe: Option<String>,
    /// Hide the console window at startup (UI-owned launch).
    pub hidden: bool,
}

/// Foreground indexer: load, bootstrap volumes, serve the pipe. Returns on
/// clean shutdown (Ctrl-C / `Shutdown`); the process exits 0 via `main`.
pub fn run(opts: RunOptions) -> anyhow::Result<()> {
    if !is_elevated() {
        eprintln!("{NOT_ELEVATED_MSG}");
        std::process::exit(1);
    }
    if let Some(pipe) = &opts.pipe {
        // `floki-proto` resolves the pipe name from the environment.
        std::env::set_var("FLOKI_PIPE", pipe);
    }
    // Second instance on the same pipe exits here: the listener below would
    // otherwise either steal the name (orphaning the first indexer's scans)
    // or fail with a bare OS error. The handle stays alive for the whole
    // run; the OS releases the mutex when the last handle closes. Claimed
    // after pipe resolution so `--pipe`/FLOKI_PIPE tests never collide with
    // the production `Global\FlokiIndexer` name.
    let pipe = floki_proto::pipe_name();
    let _singleton = match claim_singleton(&singleton_name(&pipe)) {
        Singleton::Held { handle } => SingletonGuard(handle),
        Singleton::AlreadyRunning => {
            eprintln!("another flokid is already running on {pipe} — stop it or use --pipe");
            std::process::exit(3);
        }
    };
    if opts.hidden {
        hide_console();
    }
    // flokid must never lag the machine: below-normal process priority,
    // a bounded rayon pool for searches, low-priority scan/tail threads.
    lower_process_priority();
    let search_threads = search_thread_budget();
    floki_core::set_search_threads(search_threads);
    ensure_data_dir()?;
    let path = index_path()?;
    init_logging()?;
    tracing::info!(target: "flokid", version = env!("CARGO_PKG_VERSION"), "starting");
    tracing::info!(
        target: "flokid",
        "cpu policy: below-normal priority, {search_threads} search threads"
    );

    register_ctrl_handler()?;

    let index = load_index(&path, opts.no_load);
    log_memory_breakdown(&index);
    let shared = Arc::new(Shared::new(path, floki_proto::pipe_name()));

    if !index.is_empty() || !index.volumes.is_empty() {
        *shared.index.write().expect("index lock poisoned") = index;
    }
    // The persisted `targets` block is the source of truth: mirror it into
    // the lock-free copy the arrival/offline polls read.
    {
        let targets = shared.index.read().expect("index lock poisoned").targets;
        *shared.targets.write().expect("targets lock poisoned") = targets;
    }

    let targets = match opts.volumes {
        Some(vols) => vols,
        None => list_indexable_volumes(),
    };
    if targets.is_empty() {
        tracing::warn!(target: "flokid", "no NTFS or ReFS volumes found; serving empty index");
    }
    // One bootstrap thread walks every target volume sequentially in letter
    // order (C first): concurrent full scans staged hundreds of MB each and
    // blew peak RSS past 1.4 GB. Tails still start as each volume finishes
    // (see `bootstrap_volume`); rescans from other paths serialize on the
    // global scan lock inside `full_rescan`.
    {
        let worker = Arc::clone(&shared);
        match std::thread::Builder::new()
            .name("flokid-bootstrap".to_owned())
            .spawn(move || {
                run_volume_queue(&worker, targets, bootstrap_volume);
                // Volumes whose bootstrap returned before `spawn_tail`
                // (transient open/journal failure at boot) get their tail
                // here instead of waiting for the first poll slice.
                reconcile_tails(&worker);
            }) {
            Ok(handle) => shared
                .aux_handles
                .lock()
                .expect("aux lock poisoned")
                .push(handle),
            Err(e) => tracing::warn!(target: "flokid", error = %e, "cannot spawn bootstrap thread"),
        }
    }

    spawn_persist(&shared);
    spawn_rescan_worker(&shared);
    spawn_arrival_poll(&shared);
    // The pipe server starts immediately and serves whatever is indexed so
    // far; scans/replays fill the index in the background.
    server::serve(&shared)?;
    // Ctrl-C path: serve returned after the shutdown flag was set. Join the
    // tail/scan/rescan threads first (their loops check the flag between
    // batches), then save the settled index — never a half-rescanned one.
    // A pipe `Shutdown` already claimed the save on its client thread
    // (`shutdown_save_and_exit` exits the process when done); only the
    // claim winner may join + save, so this path parks instead of racing
    // a second save on the same `.tmp` file.
    if try_claim_shutdown_save() {
        join_background_threads(&shared);
        save_index(&shared);
        tracing::info!(target: "flokid", "stopped");
    } else {
        // The winner's `std::process::exit` ends the process; park so this
        // thread can never return early and kill the in-flight save.
        loop {
            std::thread::park();
        }
    }
    Ok(())
}

/// Load `index.bin` unless `--no-load`; corrupt/missing files yield a fresh
/// index (a full scan follows) rather than a failed start.
fn load_index(path: &std::path::Path, no_load: bool) -> Index {
    if no_load {
        tracing::info!(target: "flokid", "skipping index load (--no-load)");
        return Index::new();
    }
    if !path.exists() {
        tracing::info!(target: "flokid", "no saved index; starting fresh");
        return Index::new();
    }
    match Index::load(path) {
        Ok(index) => {
            tracing::info!(
                target: "flokid",
                entries = index.len(),
                volumes = index.volumes.len(),
                "loaded saved index"
            );
            index
        }
        Err(e) => {
            tracing::warn!(target: "flokid", error = %e, "saved index unreadable; starting fresh");
            Index::new()
        }
    }
}

/// Initialize `tracing`: stderr plus `%LOCALAPPDATA%\Floki\flokid.log`.
pub fn init_logging() -> anyhow::Result<()> {
    let dir = ensure_data_dir()?;
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("flokid.log"))?;
    let file_layer = tracing_subscriber::fmt::layer().with_writer(std::sync::Mutex::new(log_file));
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .try_init()
        .map_err(|e| anyhow::anyhow!("logging already initialized: {e}"))?;
    Ok(())
}

/// Install the console handler that turns Ctrl-C/Break/Close into shutdown.
fn register_ctrl_handler() -> anyhow::Result<()> {
    // SAFETY: stateless handler, only sets an atomic flag; valid for the
    // process lifetime once registered.
    let ok = unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), TRUE) };
    if ok == FALSE {
        return Err(anyhow::anyhow!("SetConsoleCtrlHandler failed"));
    }
    Ok(())
}

/// Console control handler: signal shutdown, let the main loops save + exit.
unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> windows_sys::core::BOOL {
    if ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT || ctrl_type == CTRL_CLOSE_EVENT {
        crate::state::SHUTDOWN.store(true, Ordering::Relaxed);
        TRUE
    } else {
        FALSE
    }
}

/// Bring volumes up to date strictly one at a time, in letter order.
///
/// The per-volume work (`bootstrap_volume` in production: journal replay or
/// full scan, then tail start) runs sequentially so scan staging buffers
/// never pile up across volumes. Takes the work as a parameter so tests can
/// inject a fake and observe the concurrency contract.
fn run_volume_queue(shared: &Arc<Shared>, letters: Vec<char>, work: impl Fn(&Arc<Shared>, char)) {
    let mut letters = letters;
    letters.sort_unstable();
    for letter in letters {
        if is_shutdown(shared) {
            return;
        }
        work(shared, letter);
    }
}

/// Bring one volume up to date, then start its tail thread.
fn bootstrap_volume(shared: &Arc<Shared>, letter: char) {
    if is_shutdown(shared) {
        return;
    }
    let handle = match VolumeHandle::open(letter) {
        Ok(handle) => handle,
        Err(e) => {
            tracing::warn!(target: "flokid", volume = %letter, error = %e, "cannot open volume");
            return;
        }
    };
    ensure_journal_size(&handle, letter);
    let journal = match query_or_create(&handle, letter) {
        Some(journal) => journal,
        None => return,
    };
    let existing: Option<(u64, i64)> = {
        let index = shared.index.read().expect("index lock poisoned");
        index
            .volumes
            .iter()
            .find(|v| v.letter == letter)
            .map(|v| (v.journal_id, v.next_usn))
    };
    match existing {
        Some((journal_id, from)) if journal_id == journal.journal_id => {
            replay_journal(shared, &handle, letter, journal.journal_id, from);
        }
        Some((old_id, _)) => {
            tracing::info!(
                target: "flokid",
                volume = %letter,
                old_id,
                new_id = journal.journal_id,
                "journal id changed; full rescan"
            );
            full_rescan(shared, letter);
        }
        None => {
            // Covers volumes missing from the saved index — including ones
            // whose previous scan never completed: `save_index` refuses to
            // persist those, so the next boot full-scans them here.
            tracing::info!(target: "flokid", volume = %letter, "new volume; full scan");
            full_rescan(shared, letter);
        }
    }
    drop(handle);
    // Scan-only volumes (monitor=false) get one full scan at add/rescan and
    // no live tail; re-enabling monitor spawns the tail via the pipe op.
    // Volumes that failed to register (open/journal failure above) get no
    // tail here — the arrival poll retries their full_rescan every
    // ARRIVAL_INTERVAL, and `reconcile_tails` covers indexed volumes whose
    // tail died or never started.
    let monitored = shared
        .index
        .read()
        .expect("index lock poisoned")
        .volumes
        .iter()
        .find(|v| v.letter == letter)
        .is_some_and(|v| v.monitor);
    if monitored {
        spawn_tail(shared, letter);
    }
}

/// Grow undersized journals on open: a journal smaller than
/// [`JOURNAL_MAX_SIZE`] wraps within ~an hour on a build-heavy box, forcing
/// a full rescan after every downtime. Uses `FSCTL_CREATE_USN_JOURNAL`,
/// which resizes an active journal in place; journals are never deleted.
/// Query failures are ignored here — `query_or_create` handles them.
fn ensure_journal_size(handle: &VolumeHandle, letter: char) {
    let info = match handle.query_journal() {
        Ok(info) => info,
        Err(_) => return,
    };
    if info.maximum_size >= JOURNAL_MAX_SIZE {
        return;
    }
    tracing::info!(
        target: "flokid",
        volume = %letter,
        old_max_mb = info.maximum_size / (1024 * 1024),
        new_max_mb = JOURNAL_MAX_SIZE / (1024 * 1024),
        "growing USN journal"
    );
    if let Err(e) = handle.create_journal(JOURNAL_MAX_SIZE, JOURNAL_DELTA) {
        tracing::warn!(target: "flokid", volume = %letter, error = %e, "cannot grow journal; wrap risk remains");
    }
}

/// Log a journal wrap with both cursors: the `next_usn` we saved versus the
/// journal's `first_usn` now, so the lost range is visible in the log.
fn log_journal_wrap(
    shared: &Arc<Shared>,
    handle: &VolumeHandle,
    letter: char,
    context: &'static str,
) {
    let stored: Option<i64> = shared
        .index
        .read()
        .expect("index lock poisoned")
        .volumes
        .iter()
        .find(|v| v.letter == letter)
        .map(|v| v.next_usn);
    match handle.query_journal() {
        Ok(info) => {
            let lost = stored.map(|from| info.first_usn.saturating_sub(from).max(0));
            tracing::warn!(
                target: "flokid",
                volume = %letter,
                context,
                stored_next_usn = ?stored,
                journal_first_usn = info.first_usn,
                journal_next_usn = info.next_usn,
                lost_usn = ?lost,
                "journal wrapped; full rescan"
            );
        }
        Err(e) => {
            tracing::warn!(
                target: "flokid",
                volume = %letter,
                context,
                stored_next_usn = ?stored,
                error = %e,
                "journal wrapped (could not query cursors); full rescan"
            );
        }
    }
}

/// `query_journal`, creating a journal first when the volume has none.
fn query_or_create(handle: &VolumeHandle, letter: char) -> Option<JournalInfo> {
    match handle.query_journal() {
        Ok(info) => Some(info),
        Err(NtfsError::JournalNotActive) => {
            tracing::info!(target: "flokid", volume = %letter, "no journal; creating one");
            if let Err(e) = handle.create_journal(JOURNAL_MAX_SIZE, JOURNAL_DELTA) {
                tracing::warn!(target: "flokid", volume = %letter, error = %e, "cannot create journal");
                return None;
            }
            match handle.query_journal() {
                Ok(info) => Some(info),
                Err(e) => {
                    tracing::warn!(target: "flokid", volume = %letter, error = %e, "journal still unreadable");
                    None
                }
            }
        }
        Err(e) => {
            tracing::warn!(target: "flokid", volume = %letter, error = %e, "cannot query journal");
            None
        }
    }
}

/// Replay journal events from `from` into the index under one write lock.
fn replay_journal(
    shared: &Arc<Shared>,
    handle: &VolumeHandle,
    letter: char,
    journal_id: u64,
    from: i64,
) {
    let Some(root_frn) = ({
        let index = shared.index.read().expect("index lock poisoned");
        index
            .volumes
            .iter()
            .find(|v| v.letter == letter)
            .map(|v| v.root_frn)
    }) else {
        return;
    };
    // Stream the backlog in `APPLY_CHUNK` slices instead of collecting it
    // all and applying it under one write lock: a day-long backlog is
    // millions of events, and searches must keep answering throughout.
    let mut buf: Vec<UsnEvent> = Vec::with_capacity(APPLY_CHUNK);
    let mut streamed = 0usize;
    let mut gone = false;
    let result = handle.read_journal(from, journal_id, &mut |ev| {
        if gone || is_shutdown(shared) {
            return;
        }
        buf.push(ev);
        if buf.len() >= APPLY_CHUNK {
            streamed += buf.len();
            gone = apply_journal_chunk(shared, letter, root_frn, &buf, None).is_none();
            buf.clear();
        }
    });
    match result {
        Ok(next_usn) => {
            if gone || is_shutdown(shared) {
                // Cursor not advanced: the next boot replays from `from`
                // again (re-applying events is idempotent).
                return;
            }
            if streamed == 0 && buf.is_empty() && next_usn == from {
                tracing::info!(target: "flokid", volume = %letter, "journal already caught up");
                return;
            }
            let events = streamed + buf.len();
            let Some(compacted) =
                apply_journal_chunk(shared, letter, root_frn, &buf, Some(next_usn))
            else {
                return;
            };
            if !compacted {
                maybe_fold_pending(shared);
            }
            tracing::info!(
                target: "flokid",
                volume = %letter,
                events,
                next_usn,
                "replayed journal"
            );
        }
        Err(NtfsError::JournalWrapped) => {
            log_journal_wrap(shared, handle, letter, "replay");
            full_rescan(shared, letter);
        }
        Err(NtfsError::JournalNotActive) => {
            tracing::warn!(target: "flokid", volume = %letter, "journal went inactive; recreating + rescan");
            if handle
                .create_journal(JOURNAL_MAX_SIZE, JOURNAL_DELTA)
                .is_ok()
            {
                full_rescan(shared, letter);
            }
        }
        Err(e) => {
            tracing::warn!(target: "flokid", volume = %letter, error = %e, "journal replay failed");
        }
    }
}

/// Journal events applied per write lock (replay and tail). Bounds the
/// write hold to a few ms so searches interleave with a large backlog.
pub const APPLY_CHUNK: usize = 10_000;

/// Apply journal events for `letter` in [`APPLY_CHUNK`] slices, each under
/// its own short write lock, then advance the cursor to `next_usn`. Returns
/// `None` when the volume left the index, else whether a compaction ran.
pub fn apply_journal_events(
    shared: &Arc<Shared>,
    letter: char,
    root_frn: u64,
    events: &[UsnEvent],
    next_usn: i64,
) -> Option<bool> {
    let mut chunks = events.chunks(APPLY_CHUNK).peekable();
    while let Some(chunk) = chunks.next() {
        if chunks.peek().is_some() {
            apply_journal_chunk(shared, letter, root_frn, chunk, None)?;
        } else {
            return apply_journal_chunk(shared, letter, root_frn, chunk, Some(next_usn));
        }
    }
    // No events but the cursor moved (filtered records): advance it.
    apply_journal_chunk(shared, letter, root_frn, &[], Some(next_usn))
}

/// One chunk under one write lock: resolve the volume slot by letter (a
/// `VolumesRemove` may have shifted it), apply, seal the FRN tail so the
/// next chunk's lookups stay logarithmic, and on the final chunk
/// (`next_usn` given) advance the cursor and compact past 10% garbage.
/// Folds an overgrown `pending` off-lock between chunks. `None` when the
/// volume is no longer indexed.
fn apply_journal_chunk(
    shared: &Arc<Shared>,
    letter: char,
    root_frn: u64,
    events: &[UsnEvent],
    next_usn: Option<i64>,
) -> Option<bool> {
    yield_to_searches(shared);
    let started = Instant::now();
    let compacted = {
        let mut index = shared.index.write().expect("index lock poisoned");
        let vol_idx = index.volumes.iter().position(|v| v.letter == letter)? as u8;
        for event in events {
            apply_usn_event(&mut index, vol_idx, event, root_frn);
        }
        index.seal_live();
        match next_usn {
            Some(next_usn) => {
                if let Some(vol) = index.volumes.get_mut(usize::from(vol_idx)) {
                    vol.next_usn = next_usn;
                }
                maybe_compact(&mut index)
            }
            None => false,
        }
    };
    log_slow_op("journal-apply", Some(letter), started);
    if compacted {
        // `compact()` renumbered every id: drop cached `prev` hits.
        shared
            .prev_cache
            .lock()
            .expect("prev lock poisoned")
            .clear();
    } else if next_usn.is_none() {
        maybe_fold_pending(shared);
    }
    Some(compacted)
}

/// Full-volume scan, split by whether old data exists.
///
/// * New volume (nothing indexed yet): stream records straight into the live
///   index in [`SCAN_BATCH`] batches under short write locks, so results are
///   searchable within the first batch (SPEC §7 "serve immediately while
///   scanning"). Peak extra RAM is one batch.
/// * Existing volume (journal id changed, wrap, `flk rescan`): stage the
///   fresh volume into a private [`Index`] off the lock — old data stays
///   searchable — then commit under one short write lock (tombstone stale +
///   copy staged + rebuild + compact).
///
/// Both paths use only public `floki-core` API.
///
/// Holds the global scan lock for the whole scan: staging buffers run into
/// the hundreds of MB, so a second concurrent scan (rescan worker, wrap
/// rescan) would blow peak RSS. Lock order is scan → volume → index, taken
/// nowhere else in a different order.
fn full_rescan(shared: &Arc<Shared>, letter: char) {
    let _scan_guard = shared.scan_lock.lock().expect("scan lock poisoned");
    let guard = shared.vol_lock(letter);
    let _vol_guard = guard.lock().unwrap_or_else(|e| e.into_inner());
    // Scan threads run at the lowest priority (dedicated threads: boot
    // threads exit, the rescan worker only rescans).
    lower_scan_thread_priority();
    if is_shutdown(shared) {
        return;
    }
    let handle = match VolumeHandle::open(letter) {
        Ok(handle) => handle,
        Err(e) => {
            tracing::warn!(target: "flokid", volume = %letter, error = %e, "rescan: cannot open volume");
            return;
        }
    };
    let journal = match query_or_create(&handle, letter) {
        Some(journal) => journal,
        None => return,
    };
    let progress = Arc::new(ScanProgress {
        letter,
        done: AtomicU64::new(0),
    });
    shared
        .active_scans
        .lock()
        .expect("scans lock poisoned")
        .push(Arc::clone(&progress));

    // Resolve (or register) the volume under one short write lock.
    let (vol_idx, is_new): (u8, bool) = {
        let mut index = shared.index.write().expect("index lock poisoned");
        match index
            .volumes
            .iter()
            .position(|v| v.letter == letter)
            .map(|i| i as u8)
        {
            Some(idx) => {
                if let Some(vol) = index.volumes.get_mut(usize::from(idx)) {
                    vol.journal_id = journal.journal_id;
                }
                (idx, false)
            }
            None => (
                index.add_volume(Volume {
                    letter,
                    guid: [0; 16],
                    journal_id: journal.journal_id,
                    next_usn: journal.next_usn,
                    root_frn: 0,
                    enabled: true,
                    monitor: true,
                }),
                true,
            ),
        }
    };

    if is_new {
        scan_new_volume(shared, &handle, letter, vol_idx, &progress);
    } else {
        rescan_existing_volume(shared, &handle, letter, vol_idx, &progress);
    }
    shared
        .active_scans
        .lock()
        .expect("scans lock poisoned")
        .retain(|p| !Arc::ptr_eq(p, &progress));
}

/// First-boot scan of a volume with no indexed data: stream straight into
/// the live index in [`SCAN_BATCH`] batches so the volume becomes searchable
/// after the first batch, not after the whole ~30 s scan.
fn scan_new_volume(
    shared: &Arc<Shared>,
    handle: &VolumeHandle,
    letter: char,
    vol_idx: u8,
    progress: &Arc<ScanProgress>,
) {
    // Root FRN, discovered from the stream (0 = not seen yet). Flushes read
    // it to filter metafiles; the root MFT record sorts long before the
    // first batch fills, so every flush filters with the root known.
    let root_seen = AtomicU64::new(0);
    let started = Instant::now();
    let mut batch: Vec<RawRecord> = Vec::with_capacity(SCAN_BATCH);
    // Background I/O + memory priority for the enumeration body; the guard
    // ENDs it as soon as `enumerate` returns.
    let scan_result = {
        let _bg = BackgroundMode::enter();
        handle.enumerate(&mut |record: RawRecord| {
            if handle.is_root(record.frn) && root_seen.load(Ordering::Relaxed) == 0 {
                root_seen.store(record.frn, Ordering::Relaxed);
            }
            progress.done.fetch_add(1, Ordering::Relaxed);
            if is_shutdown(shared) {
                return;
            }
            batch.push(record);
            if batch.len() >= SCAN_BATCH {
                flush_scan_batch(shared, vol_idx, &mut batch, &root_seen);
                // Pacing: yield between batches so searches get scheduled.
                std::thread::yield_now();
            }
        })
    };
    if !batch.is_empty() {
        flush_scan_batch(shared, vol_idx, &mut batch, &root_seen);
        std::thread::yield_now();
    }
    let done = progress.done.load(Ordering::Relaxed);

    match scan_result {
        Ok(next_usn) => {
            if is_shutdown(shared) {
                // Partial data stays in memory but is NEVER persisted:
                // the volume is flagged incomplete so `save_index` refuses
                // to store it as ready; the next boot loads the last good
                // file (or nothing) and full-scans the missing volume.
                mark_volume_incomplete(letter);
                tracing::info!(target: "flokid", volume = %letter, "scan aborted at shutdown");
                return;
            }
            let root = root_seen.load(Ordering::Relaxed);
            // Yield to an in-flight search before the commit lock so typing
            // is never starved by the final sort.
            yield_to_searches(shared);
            let commit_total = Instant::now();
            // Other volumes' tails wait out the commit: at 10M+ entries the
            // snapshot build takes seconds, and a tail applying meanwhile
            // refused every install, ending in a 5.6 s in-lock rebuild.
            let hold_ms = with_other_tails_parked(shared, letter, || {
                // Phase A (brief write lock, linear work only): cursors plus the
                // pre-root metafile sweep. No sorts here.
                let (needs_compact, mut hold_ms) = {
                    let lock_started = Instant::now();
                    let mut index = shared.index.write().expect("index lock poisoned");
                    if let Some(vol) = index.volumes.get_mut(usize::from(vol_idx)) {
                        vol.next_usn = next_usn;
                        if root != 0 {
                            vol.root_frn = root;
                        }
                    }
                    if root != 0 {
                        // Belt and braces: flushes already filtered once the
                        // root was known; drop any metafile that slipped
                        // through before that.
                        sweep_metafiles(&mut index, vol_idx, root);
                    }
                    let needs_compact = index.garbage_ratio() > 0.01;
                    (needs_compact, lock_started.elapsed().as_millis())
                };
                if needs_compact {
                    // Real garbage to drop (rare on a fresh scan): compact
                    // rebuilds the arrays itself, so no snapshot needed after.
                    let lock_started = Instant::now();
                    {
                        let mut index = shared.index.write().expect("index lock poisoned");
                        index.compact();
                    }
                    hold_ms += lock_started.elapsed().as_millis();
                } else {
                    // Sorts run off-lock (snapshot under read); install swaps
                    // the arrays under a microsecond-scale write lock.
                    hold_ms += commit_sorted_snapshot(shared, "scan-commit", Some(letter));
                }
                hold_ms
            });
            tracing::info!(
                target: "flokid",
                volume = %letter,
                hold_ms,
                total_ms = commit_total.elapsed().as_millis(),
                "scan commit"
            );
            // Ids may have been renumbered: drop cached `prev` hits.
            shared
                .prev_cache
                .lock()
                .expect("prev lock poisoned")
                .clear();
            clear_volume_incomplete(letter);
            log_scan_complete(letter, done, started);
        }
        Err(e) => {
            tracing::warn!(target: "flokid", volume = %letter, error = %e, "scan failed");
        }
    }
}

/// Rescan of a volume that already has indexed data: stage the fresh volume
/// into a private [`Index`] off the lock (the old entries stay searchable),
/// then swap it in under one short write lock.
///
/// The staging index holds ~24 B + name bytes per entry plus one arena —
/// never a `String` per record — so peak transient RAM stays proportional to
/// the index itself, not to NTFS record overhead.
fn rescan_existing_volume(
    shared: &Arc<Shared>,
    handle: &VolumeHandle,
    letter: char,
    vol_idx: u8,
    progress: &Arc<ScanProgress>,
) {
    let started = Instant::now();
    let mut staged = Index::new();
    staged.add_volume(Volume {
        letter,
        guid: [0; 16],
        journal_id: 0,
        next_usn: 0,
        root_frn: 0,
        enabled: true,
        monitor: true,
    });
    staged.reserve(SCAN_BATCH);
    let mut root_frn: Option<u64> = None;
    // Background I/O + memory priority for the enumeration body; the guard
    // ENDs it as soon as `enumerate` returns.
    let scan_result = {
        let _bg = BackgroundMode::enter();
        handle.enumerate(&mut |record: RawRecord| {
            if handle.is_root(record.frn) && root_frn.is_none() {
                root_frn = Some(record.frn);
            }
            progress.done.fetch_add(1, Ordering::Relaxed);
            if is_shutdown(shared) {
                return;
            }
            staged.push(
                0,
                record.frn,
                record.parent_frn,
                &record.name,
                attrs_to_flags(record.attrs),
            );
        })
    };
    let done = progress.done.load(Ordering::Relaxed);

    match scan_result {
        Ok(next_usn) => {
            if is_shutdown(shared) {
                // Abort the commit: the index keeps its pre-rescan state and
                // the volume is flagged incomplete so `save_index` refuses to
                // persist anything until a scan completes. The staged index
                // is simply dropped.
                mark_volume_incomplete(letter);
                tracing::info!(target: "flokid", volume = %letter, "scan aborted at shutdown");
                return;
            }
            let root = root_frn.or_else(|| {
                shared
                    .index
                    .read()
                    .expect("index lock poisoned")
                    .volumes
                    .iter()
                    .find(|v| v.letter == letter)
                    .map(|v| v.root_frn)
            });
            let root = root.unwrap_or(0);
            let commit_total = Instant::now();
            // The expensive sorts run on the private staged index: no lock,
            // searches and tails untouched.
            staged.rebuild_by_name();
            staged.finalize();
            let skip = |entry: &floki_core::Entry, name: &str| {
                root != 0 && is_ntfs_metafile(entry.parent_frn, name, root)
            };
            let set_cursor = |index: &mut Index| {
                if let Some(vol) = index.volumes.get_mut(usize::from(vol_idx)) {
                    vol.next_usn = next_usn;
                    if let Some(frn) = root_frn {
                        vol.root_frn = frn;
                    }
                }
            };
            // Merge the volume in under a READ lock (searches proceed), then
            // install with an O(1) swap, with every other tail parked.
            // Anything else mutating in between refuses the install: rebuild
            // up to 3 times, then merge under the write lock (linear, no sort).
            let (hold_ms, stale, fresh) = with_other_tails_parked(shared, letter, || {
                let mut hold_ms = 0u128;
                let mut counts = None;
                for attempt in 1..=3 {
                    let swap = {
                        let index = shared.index.read().expect("index lock poisoned");
                        index.volume_swap(vol_idx, &staged, skip)
                    };
                    let (stale, fresh) = (swap.stale, swap.fresh);
                    yield_to_searches(shared);
                    let lock_started = Instant::now();
                    let mut index = shared.index.write().expect("index lock poisoned");
                    let installed = index.install_volume_swap(swap);
                    if installed {
                        set_cursor(&mut index);
                    }
                    drop(index);
                    hold_ms += lock_started.elapsed().as_millis();
                    if installed {
                        counts = Some((stale, fresh));
                        break;
                    }
                    tracing::debug!(target: "flokid", volume = %letter, attempt, "volume swap raced; retrying");
                }
                let (stale, fresh) = counts.unwrap_or_else(|| {
                    tracing::warn!(
                        target: "flokid",
                        volume = %letter,
                        "volume swap raced 3 times; merging under the write lock"
                    );
                    let lock_started = Instant::now();
                    let mut index = shared.index.write().expect("index lock poisoned");
                    let counts = index.replace_volume(vol_idx, &staged, skip);
                    set_cursor(&mut index);
                    hold_ms += lock_started.elapsed().as_millis();
                    counts
                });
                (hold_ms, stale, fresh)
            });
            tracing::info!(
                target: "flokid",
                volume = %letter,
                stale,
                fresh,
                hold_ms,
                total_ms = commit_total.elapsed().as_millis(),
                "rescan commit"
            );
            // `replace_volume` renumbered every id: drop cached `prev` hits.
            shared
                .prev_cache
                .lock()
                .expect("prev lock poisoned")
                .clear();
            clear_volume_incomplete(letter);
            log_scan_complete(letter, done, started);
        }
        Err(e) => {
            tracing::warn!(target: "flokid", volume = %letter, error = %e, "scan failed");
        }
    }
}

/// Run a scan commit with every other indexed volume's tail parked on its
/// volume lock. A tail queued on the write lock blocks new searches behind
/// the read-locked snapshot build (std `RwLock` prefers writers) and makes
/// the install race; parked, its events just wait in the journal for a few
/// seconds. No cycle: the caller holds the scan lock and `letter`'s volume
/// lock, the rest are taken in letter order, and tails take only their own
/// volume lock before the index lock.
fn with_other_tails_parked<R>(shared: &Shared, letter: char, f: impl FnOnce() -> R) -> R {
    with_tails_parked(shared, Some(letter), f)
}

/// [`with_other_tails_parked`] minus `except` (`None` parks every tail).
/// Callers must hold the scan lock: it is what keeps two multi-lock takers
/// from interleaving.
fn with_tails_parked<R>(shared: &Shared, except: Option<char>, f: impl FnOnce() -> R) -> R {
    let mut others: Vec<char> = shared
        .index
        .read()
        .expect("index lock poisoned")
        .volumes
        .iter()
        .map(|v| v.letter)
        .filter(|&l| Some(l) != except)
        .collect();
    others.sort_unstable();
    let locks: Vec<_> = others.iter().map(|&l| shared.vol_lock(l)).collect();
    let _parked: Vec<_> = locks
        .iter()
        .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()))
        .collect();
    f()
}

/// Scan pacing: if a search is in flight, sleep 5 ms before taking a write
/// lock so keystroke searches are never starved by scan batches/commits.
fn yield_to_searches(shared: &Arc<Shared>) {
    if shared.search_in_flight_count() > 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Push one scan batch under a single write lock, filtering NTFS metafiles
/// once the volume root is known.
///
/// Append-only [`Index::push`]: the volume is new so there is nothing to
/// dedupe against, and per-record [`Index::apply`] would binary-search plus
/// `Vec::insert` into `frn_index` per record — O(n²) over a full MFT scan
/// (MFT order is not FRN-sorted). `push` leaves the sorted arrays stale; the
/// single snapshot/install at scan end rebuilds them off-lock. Returns the
/// write-lock hold time in ms (scan pacing budgets it).
pub fn flush_scan_batch(
    shared: &Arc<Shared>,
    vol_idx: u8,
    batch: &mut Vec<RawRecord>,
    root_seen: &AtomicU64,
) -> u128 {
    if batch.is_empty() {
        return 0;
    }
    let root = root_seen.load(Ordering::Relaxed);
    yield_to_searches(shared);
    let lock_started = Instant::now();
    let mut index = shared.index.write().expect("index lock poisoned");
    for record in batch.drain(..) {
        if root != 0 && is_ntfs_metafile(record.parent_frn, &record.name, root) {
            continue;
        }
        index.push(
            vol_idx,
            record.frn,
            record.parent_frn,
            &record.name,
            attrs_to_flags(record.attrs),
        );
    }
    drop(index);
    lock_started.elapsed().as_millis()
}

/// Tombstone staged-before-root metafiles that slipped into the live index:
/// entries directly under `root` whose name starts with `$`.
fn sweep_metafiles(index: &mut Index, vol_idx: u8, root: u64) {
    for id in 0..index.len() {
        let hit = match index.entries.get(id) {
            Some(entry)
                if index.volume_of(id as u32) == Some(vol_idx)
                    && entry.parent_frn == root
                    && !entry.is_tombstone() =>
            {
                index.name(id as u32).is_some_and(|n| n.starts_with('$'))
            }
            _ => false,
        };
        if hit {
            if let Some(entry) = index.entries.get_mut(id) {
                entry.flags |= floki_core::TOMBSTONE;
            }
        }
    }
}

/// Throughput log shared by both scan paths.
fn log_scan_complete(letter: char, done: u64, started: Instant) {
    let elapsed = started.elapsed();
    let rate = done as f64 / elapsed.as_secs_f64().max(0.001);
    tracing::info!(
        target: "flokid",
        volume = %letter,
        entries = done,
        elapsed_s = elapsed.as_secs(),
        rate_s = rate as u64,
        "scan complete"
    );
}

/// Spawn a tail thread from the pipe layer (`VolumesSetMonitor{true}`).
/// Thin wrapper over [`spawn_tail`]: the tail loop itself exits promptly
/// when the flag flips back, so no stop handle is needed for `false`.
pub fn spawn_tail_for_pipe(shared: &Arc<Shared>, letter: char) {
    spawn_tail(shared, letter);
}

/// Start the journal tail thread for `letter` (no-op when one is claimed or
/// the daemon is shutting down). The `live_vols` claim is taken atomically
/// under the lock BEFORE the thread spawns, so two concurrent spawners can
/// never race a double tail; a failed spawn releases the claim so the next
/// caller (or the reconcile pass) retries instead of wedging the volume.
/// `tail_handles` is held across claim + spawn + insert so `reconcile_tails`
/// can't reap a stale finished handle in the gap and clear the fresh claim.
fn spawn_tail(shared: &Arc<Shared>, letter: char) {
    if is_shutdown(shared) {
        return;
    }
    let mut handles = shared.tail_handles.lock().expect("tail lock poisoned");
    {
        let mut live = shared.live_vols.lock().expect("live lock poisoned");
        if !live.insert(letter) {
            return;
        }
    }
    let worker = Arc::clone(shared);
    match std::thread::Builder::new()
        .name(format!("flokid-tail-{letter}"))
        .spawn(move || tail_loop(&worker, letter))
    {
        Ok(handle) => {
            handles.insert(letter, handle);
        }
        Err(e) => {
            shared
                .live_vols
                .lock()
                .expect("live lock poisoned")
                .remove(&letter);
            tracing::warn!(target: "flokid", volume = %letter, error = %e, "cannot spawn tail thread");
        }
    }
}

/// Poll `read_journal` every [`TAIL_INTERVAL`]; each poll's events are
/// applied under a single write lock. `JournalWrapped` triggers a full
/// rescan of the volume.
///
/// Policy exits, all checked per tick so they react within ~750 ms:
/// * monitor disabled via `VolumesSetMonitor{false}` → the loop exits and
///   the volume keeps its scan-only snapshot (no tail).
/// * the volume record disappears from the index (`VolumesRemove`,
///   `remove_offline_volume`) → nothing left to tail; the loop exits.
/// * the volume stays absent (unopenable for reasons other than
///   access-denied) while `auto_remove_offline` is set → the loop drops the
///   volume record plus all its entries (`remove_volume`) so a pulled USB
///   stick stops shadowing drive letters, then exits.
///
/// Every exit path (including a panic) releases the `live_vols` claim via
/// [`TailGuard`] and logs the reason, so a dead tail never leaves the
/// volume permanently Offline — the reconcile pass respawns it.
fn tail_loop(shared: &Arc<Shared>, letter: char) {
    let _exit_guard = TailGuard {
        shared: Arc::clone(shared),
        letter,
    };
    lower_tail_thread_priority();
    tracing::info!(target: "flokid", volume = %letter, "tail started");
    let guard = shared.vol_lock(letter);
    let mut handle: Option<VolumeHandle> = None;
    // Consecutive ticks without an openable handle. Reset on every success.
    let mut offline_ticks: u32 = 0;
    // Consecutive ticks where the volume looks ABSENT (device gone or not
    // NTFS). `AccessDenied` means the drive is present but inaccessible —
    // it never counts toward the offline drop, or a permissions blip would
    // erase a healthy volume's entries.
    let mut absent_ticks: u32 = 0;
    // Why the loop exited; logged (with the letter) on the way out.
    let mut stop_reason = "shutdown requested";
    while !is_shutdown(shared) {
        std::thread::sleep(TAIL_INTERVAL);
        if is_shutdown(shared) {
            break;
        }
        // Monitor disabled mid-run: exit, keep the scan-only snapshot.
        let monitored = shared
            .index
            .read()
            .expect("index lock poisoned")
            .volumes
            .iter()
            .find(|v| v.letter == letter)
            .is_none_or(|v| v.monitor);
        if !monitored {
            stop_reason = "monitor disabled";
            break;
        }
        if handle.is_none() {
            match VolumeHandle::open(letter) {
                Ok(opened) => {
                    if offline_ticks > 0 {
                        tracing::info!(target: "flokid", volume = %letter, offline_ticks, "tail: volume open again");
                    }
                    shared
                        .open_vols
                        .lock()
                        .expect("open lock poisoned")
                        .insert(letter);
                    handle = Some(opened);
                    offline_ticks = 0;
                    absent_ticks = 0;
                }
                Err(e) => {
                    offline_ticks = offline_ticks.saturating_add(1);
                    // AccessDenied = present but inaccessible: keep retrying
                    // forever, never count toward the offline drop.
                    if e != NtfsError::AccessDenied {
                        absent_ticks = absent_ticks.saturating_add(1);
                    }
                    // First failure is warn (the log must explain an Offline
                    // volume); sustained absence stays at debug.
                    if offline_ticks == 1 {
                        tracing::warn!(target: "flokid", volume = %letter, error = %e, "tail: cannot open volume; retrying");
                    } else {
                        tracing::debug!(target: "flokid", volume = %letter, error = %e, offline_ticks, "tail: volume unavailable");
                    }
                    if should_remove_offline(shared, absent_ticks) {
                        remove_offline_volume(shared, letter);
                        stop_reason = "volume absent; removed from index";
                        break;
                    }
                    continue;
                }
            }
        }
        // Serialized against rescans of the same volume.
        let _vol_guard = guard.lock().unwrap_or_else(|e| e.into_inner());
        if is_shutdown(shared) {
            break;
        }
        let Some(h) = handle.as_ref() else {
            continue;
        };
        let cursor: Option<(u8, u64, i64, u64)> = {
            let index = shared.index.read().expect("index lock poisoned");
            index
                .volumes
                .iter()
                .position(|v| v.letter == letter)
                .map(|i| {
                    (
                        i as u8,
                        index.volumes[i].journal_id,
                        index.volumes[i].next_usn,
                        index.volumes[i].root_frn,
                    )
                })
        };
        let Some((_, journal_id, from, root_frn)) = cursor else {
            // Volume dropped from the index (`VolumesRemove` or the offline
            // drop): nothing left to tail, so exit instead of spinning.
            stop_reason = "volume no longer indexed";
            break;
        };
        let mut events = Vec::new();
        match h.read_journal(from, journal_id, &mut |ev| events.push(ev)) {
            Ok(next_usn) => {
                if events.is_empty() && next_usn == from {
                    continue;
                }
                match apply_journal_events(shared, letter, root_frn, &events, next_usn) {
                    // Fold an overgrown pending list off-lock (snapshot under
                    // read, install under a brief write); no renumbering, so
                    // `prev` stays valid.
                    Some(false) => {
                        maybe_fold_pending(shared);
                    }
                    Some(true) => {}
                    None => {
                        stop_reason = "volume no longer indexed";
                        break;
                    }
                }
            }
            Err(NtfsError::JournalWrapped) => {
                log_journal_wrap(shared, h, letter, "tail");
                drop(_vol_guard);
                full_rescan(shared, letter);
            }
            Err(NtfsError::JournalNotActive) => {
                tracing::warn!(target: "flokid", volume = %letter, "journal inactive; recreating + rescan");
                if h.create_journal(JOURNAL_MAX_SIZE, JOURNAL_DELTA).is_ok() {
                    drop(_vol_guard);
                    full_rescan(shared, letter);
                }
            }
            Err(NtfsError::JournalDeleteInProgress) => {
                tracing::debug!(target: "flokid", volume = %letter, "journal delete in progress; retrying");
            }
            Err(e) => {
                tracing::warn!(target: "flokid", volume = %letter, error = %e, "tail read failed; reopening handle");
                shared
                    .open_vols
                    .lock()
                    .expect("open lock poisoned")
                    .remove(&letter);
                handle = None;
            }
        }
    }
    tracing::info!(target: "flokid", volume = %letter, reason = stop_reason, "tail stopped");
}
/// Panic-safe release of a tail's `live_vols`/`open_vols` claims: a tail
/// that dies mid-poll (panic, abort) still frees its slot so the reconcile
/// pass can respawn it instead of the volume sticking on a stale claim.
struct TailGuard {
    shared: Arc<Shared>,
    letter: char,
}

impl Drop for TailGuard {
    fn drop(&mut self) {
        self.shared
            .live_vols
            .lock()
            .expect("live lock poisoned")
            .remove(&self.letter);
        self.shared
            .open_vols
            .lock()
            .expect("open lock poisoned")
            .remove(&self.letter);
    }
}

/// Ticks of unopenable volume before the offline poll drops it: 40 ticks at
/// 750 ms ≈ 30 s of sustained absence. Long enough to ride out a USB
/// re-enumeration blip; short enough a pulled stick stops shadowing its
/// letter within half a minute.
pub const OFFLINE_REMOVE_TICKS: u32 = 40;

/// True when the volume has been unopenable for [`OFFLINE_REMOVE_TICKS`]
/// ticks and the policy says to drop offline volumes. Extracted so unit
/// tests drive the threshold without a volume handle.
fn should_remove_offline(shared: &Arc<Shared>, offline_ticks: u32) -> bool {
    if offline_ticks < OFFLINE_REMOVE_TICKS {
        return false;
    }
    shared
        .targets
        .read()
        .expect("targets lock poisoned")
        .auto_remove_offline
}

/// Drop an offline volume's record plus all its entries and persist.
/// Best-effort: a contended lock simply skips the save (the periodic saver
/// will catch it). The caller's tail exits right after; its finished
/// `tail_handles` slot is reaped by `reconcile_tails` or the shutdown join.
fn remove_offline_volume(shared: &Arc<Shared>, letter: char) {
    // Waits out a running scan: scans cache their volume index, which the
    // removal shifts. This tail holds no volume lock here.
    let _scan_guard = shared.scan_lock.lock().expect("scan lock poisoned");
    if !remove_volume_off_lock(shared, letter) {
        return;
    }
    {
        let mut index = shared.index.write().expect("index lock poisoned");
        index.targets = *shared.targets.read().expect("targets lock poisoned");
    }
    tracing::info!(target: "flokid", volume = %letter, "offline volume removed from index");
    shared
        .prev_cache
        .lock()
        .expect("prev lock poisoned")
        .clear();
    save_index(shared);
}

/// Drop volume `letter` and all its entries without stalling searches: the
/// shrunken arrays are built under a READ lock and swapped in O(1), with
/// every tail parked so none refuses the install (3 tries, then the
/// in-lock `remove_volume`). The old in-lock path held the write lock
/// 1.66 s removing 2.87M entries from 11.7M. Returns `false` when `letter`
/// is not indexed.
///
/// The caller must hold the scan lock: scans cache their volume index,
/// which removal shifts, and parking takes several volume locks.
pub fn remove_volume_off_lock(shared: &Shared, letter: char) -> bool {
    let started = Instant::now();
    let outcome = with_tails_parked(shared, None, || {
        let mut hold_ms = 0u128;
        for attempt in 1..=3 {
            let removal = {
                let index = shared.index.read().expect("index lock poisoned");
                index.volume_removal(letter)
            }?;
            let dropped = removal.dropped();
            let lock_started = Instant::now();
            let installed = shared
                .index
                .write()
                .expect("index lock poisoned")
                .install_volume_removal(removal);
            hold_ms += lock_started.elapsed().as_millis();
            if installed {
                return Some((dropped, hold_ms));
            }
            tracing::debug!(target: "flokid", volume = %letter, attempt, "volume removal raced; retrying");
        }
        tracing::warn!(
            target: "flokid",
            volume = %letter,
            "volume removal raced 3 times; removing under the write lock"
        );
        let lock_started = Instant::now();
        let removed = shared
            .index
            .write()
            .expect("index lock poisoned")
            .remove_volume(letter);
        hold_ms += lock_started.elapsed().as_millis();
        removed.then_some((0, hold_ms))
    });
    let Some((dropped, hold_ms)) = outcome else {
        return false;
    };
    tracing::info!(
        target: "flokid",
        volume = %letter,
        dropped,
        hold_ms,
        total_ms = started.elapsed().as_millis(),
        "volume removed"
    );
    true
}

/// Compact when tombstones exceed 10% of entries. Returns true when a
/// compaction ran (ids were renumbered; the caller must drop `prev` caches).
/// Pending-list folding lives in [`maybe_fold_pending`] (it needs lock
/// juggling the caller does after releasing the write guard).
fn maybe_compact(index: &mut Index) -> bool {
    if index.garbage_ratio() > 0.10 {
        let before = index.len();
        index.compact();
        tracing::info!(
            target: "flokid",
            before,
            after = index.len(),
            "compacted index"
        );
        true
    } else {
        false
    }
}

/// Sort-and-install phase shared by scan commits and the pending fold: build
/// the sorted snapshot under a READ lock (the expensive sorts; searches
/// proceed), then install it under a brief write lock (O(1) swap). Install
/// fails when a tail applied events in between (mutation stamp) — retry the
/// pair up to 3 times, then fall back to the in-lock rebuild with a warn.
/// No id renumbering on any path here, so `prev` caches stay valid.
/// Returns total write-lock hold time in ms. Public so tests drive the exact
/// path commits use.
pub fn commit_sorted_snapshot(shared: &Shared, what: &'static str, letter: Option<char>) -> u128 {
    let mut hold_ms = 0u128;
    for attempt in 1..=3 {
        let snapshot = {
            let index = shared.index.read().expect("index lock poisoned");
            index.sorted_snapshot()
        };
        let lock_started = Instant::now();
        let installed = {
            let mut index = shared.index.write().expect("index lock poisoned");
            index.install_sorted(snapshot)
        };
        hold_ms += lock_started.elapsed().as_millis();
        if installed {
            return hold_ms;
        }
        tracing::debug!(target: "flokid", what, attempt, "snapshot install raced; retrying");
    }
    tracing::warn!(
        target: "flokid",
        what,
        letter = ?letter,
        "snapshot install raced 3 times; falling back to in-lock rebuild"
    );
    let lock_started = Instant::now();
    {
        let mut index = shared.index.write().expect("index lock poisoned");
        index.rebuild_by_name();
    }
    hold_ms += lock_started.elapsed().as_millis();
    hold_ms
}

/// Fold the pending name list back into the snapshot once it passes half of
/// [`PENDING_MAX`], so searches keep using the fast `by_name + pending`
/// merge instead of falling back to collect+sort past the bound.
///
/// Hysteresis is built in: the fold drains pending to zero, so the next fold
/// needs another ~25k live updates — never one per 750 ms tick. Sorts run
/// off-lock via [`commit_sorted_snapshot`]; unlike `compact`, nothing here
/// renumbers ids or bumps the epoch, so cached `prev` hits stay valid and
/// are NOT cleared. Public so integration tests can drive the exact fold the
/// tail runs.
pub fn maybe_fold_pending(shared: &Arc<Shared>) -> bool {
    let pending = shared
        .index
        .read()
        .expect("index lock poisoned")
        .pending_len();
    if pending <= PENDING_MAX / 2 {
        return false;
    }
    let started = Instant::now();
    let hold_ms = commit_sorted_snapshot(shared, "pending-fold", None);
    let elapsed_ms = started.elapsed().as_millis();
    tracing::debug!(
        target: "flokid",
        pending,
        elapsed_ms,
        hold_ms,
        "folded pending names into snapshot"
    );
    if elapsed_ms > 200 {
        tracing::info!(
            target: "flokid",
            op = "pending-fold",
            pending,
            elapsed_ms,
            hold_ms,
            "slow index op"
        );
    }
    true
}

/// Periodic persistence thread: save every [`SAVE_INTERVAL`].
fn spawn_persist(shared: &Arc<Shared>) {
    let worker = Arc::clone(shared);
    match std::thread::Builder::new()
        .name("flokid-save".to_owned())
        .spawn(move || {
            while !is_shutdown(&worker) {
                // Sleep in 5 s slices so shutdown reacts promptly.
                let slices = SAVE_INTERVAL.as_secs() / 5;
                for _ in 0..slices {
                    std::thread::sleep(Duration::from_secs(5));
                    if is_shutdown(&worker) {
                        return;
                    }
                }
                save_index(&worker);
            }
        }) {
        Ok(handle) => shared
            .aux_handles
            .lock()
            .expect("aux lock poisoned")
            .push(handle),
        Err(e) => tracing::warn!(target: "flokid", error = %e, "cannot spawn persist thread"),
    }
}

/// True when `letter` is indexed with its journal tail enabled. Unindexed
/// volumes are NOT monitored: a tail on a missing volume record exits
/// immediately ("volume no longer indexed"), so spawning one is churn —
/// the arrival poll retries their `full_rescan` instead.
fn is_monitored(shared: &Arc<Shared>, letter: char) -> bool {
    shared
        .index
        .read()
        .expect("index lock poisoned")
        .volumes
        .iter()
        .find(|v| v.letter == letter)
        .is_some_and(|v| v.monitor)
}

/// New-volume arrival poll cadence: compare `list_indexable_volumes()` against
/// the index every 60 s (in 5 s slices so shutdown reacts promptly and
/// [`reconcile_tails`] runs on each slice).
pub const ARRIVAL_INTERVAL: Duration = Duration::from_secs(60);

/// Arrival poll: every [`ARRIVAL_INTERVAL`], classify volumes present on the
/// machine but missing from the index via `GetDriveTypeW` — `DRIVE_FIXED`
/// arrivals bootstrap when `auto_include_fixed`, `DRIVE_REMOVABLE` when
/// `auto_include_removable`, all other types (network, optical, RAM disk,
/// unknown) ignored — and queue them through the existing `full_rescan`
/// path (which registers the volume, scans, and spawns the tail when
/// monitored). `--volumes`-restricted daemons still poll but only adopt
/// arrivals inside their restriction set. Each 5 s slice also runs
/// [`reconcile_tails`] so a missing tail is restarted within seconds.
fn spawn_arrival_poll(shared: &Arc<Shared>) {
    let worker = Arc::clone(shared);
    match std::thread::Builder::new()
        .name("flokid-arrival".to_owned())
        .spawn(move || {
            while !is_shutdown(&worker) {
                let slices = ARRIVAL_INTERVAL.as_secs() / 5;
                for _ in 0..slices {
                    std::thread::sleep(Duration::from_secs(5));
                    if is_shutdown(&worker) {
                        return;
                    }
                    // Cheap liveness check every 5 s: a dead or never-started
                    // tail is respawned within seconds, not a minute.
                    reconcile_tails(&worker);
                }
                poll_arrivals(&worker);
            }
        }) {
        Ok(handle) => shared
            .aux_handles
            .lock()
            .expect("aux lock poisoned")
            .push(handle),
        Err(e) => tracing::warn!(target: "flokid", error = %e, "cannot spawn arrival thread"),
    }
}

/// Restart missing tails: every indexed, monitored volume must have a live
/// tail thread. Covers the gaps the spawn points can't see — a bootstrap
/// that returned before `spawn_tail` (transient open/journal failure at
/// boot), a spawn that raced a dying tail's exit window, a tail killed by
/// panic, or a `VolumesRemove`→re-add cycle. Finished-but-unreaped handles
/// (a tail that died without running its exit guard) are reaped here so
/// their stale `live_vols` claim can't wedge the volume.
fn reconcile_tails(shared: &Arc<Shared>) {
    // Reap finished tail threads: a panicking tail's guard already cleared
    // `live_vols`, but a thread that died any other way can leave a stale
    // claim; clear it so the respawn below isn't skipped.
    let finished: Vec<char> = {
        let mut handles = shared.tail_handles.lock().expect("tail lock poisoned");
        let finished: Vec<char> = handles
            .iter()
            .filter(|(_, h)| h.is_finished())
            .map(|(l, _)| *l)
            .collect();
        for letter in &finished {
            handles.remove(letter);
        }
        finished
    };
    for letter in finished {
        let was_claimed = shared
            .live_vols
            .lock()
            .expect("live lock poisoned")
            .remove(&letter);
        shared
            .open_vols
            .lock()
            .expect("open lock poisoned")
            .remove(&letter);
        if was_claimed {
            tracing::warn!(target: "flokid", volume = %letter, "tail thread died without cleanup; reaped");
        }
    }
    let missing: Vec<char> = {
        let index = shared.index.read().expect("index lock poisoned");
        let live = shared.live_vols.lock().expect("live lock poisoned");
        index
            .volumes
            .iter()
            .filter(|v| v.monitor && !live.contains(&v.letter))
            .map(|v| v.letter)
            .collect()
    };
    for letter in missing {
        tracing::info!(target: "flokid", volume = %letter, "reconcile: restarting missing tail");
        spawn_tail(shared, letter);
    }
}

/// One arrival-poll pass over currently present NTFS/ReFS volumes.
fn poll_arrivals(shared: &Arc<Shared>) {
    reconcile_tails(shared);
    let targets = *shared.targets.read().expect("targets lock poisoned");
    let known: HashSet<char> = {
        shared
            .index
            .read()
            .expect("index lock poisoned")
            .volumes
            .iter()
            .map(|v| v.letter)
            .collect()
    };
    for letter in list_indexable_volumes() {
        if is_shutdown(shared) {
            return;
        }
        if known.contains(&letter) {
            continue;
        }
        if !parking::should_adopt(letter, &targets) {
            continue;
        }
        tracing::info!(target: "flokid", volume = %letter, "arrival poll: new volume; full scan");
        full_rescan(shared, letter);
        if is_monitored(shared, letter) {
            spawn_tail(shared, letter);
        }
    }
}

/// Drive-type classification for the arrival poll, isolated so unit tests
/// drive the policy matrix without calling `GetDriveTypeW`.
mod parking {
    /// Policy input: mirrors `floki_core::TargetsConfig` without the import
    /// (the daemon maps it at the call site).
    pub struct Policy {
        pub auto_include_fixed: bool,
        pub auto_include_removable: bool,
    }

    /// Decide whether a newcomer of `drive_type` is adopted under `policy`.
    /// `DRIVE_FIXED` (3) → fixed flag, `DRIVE_REMOVABLE` (2) → removable
    /// flag, every other type (including unknown) → never.
    ///
    /// The `Win32_System_WindowsProgramming` feature is not enabled in this
    /// workspace, so the constants are mirrored here with their documented
    /// `GetDriveTypeW` values instead of imported from `windows_sys`.
    pub fn adopt(drive_type: u32, policy: &Policy) -> bool {
        const DRIVE_REMOVABLE: u32 = 2;
        const DRIVE_FIXED: u32 = 3;
        if drive_type == DRIVE_FIXED {
            policy.auto_include_fixed
        } else if drive_type == DRIVE_REMOVABLE {
            policy.auto_include_removable
        } else {
            false
        }
    }

    /// Classify `letter` via `GetDriveTypeW` and apply the adoption policy.
    pub fn should_adopt(letter: char, targets: &floki_core::TargetsConfig) -> bool {
        if targets.is_excluded(letter) {
            // The user removed this volume; only an explicit rescan of
            // the letter adds it back.
            return false;
        }
        let policy = Policy {
            auto_include_fixed: targets.auto_include_fixed,
            auto_include_removable: targets.auto_include_removable,
        };
        adopt(drive_type_of(letter), &policy)
    }

    /// `GetDriveTypeW` for `"<L>:\\"`. Returns the raw drive type
    /// (`DRIVE_UNKNOWN` == 0 when undeterminable).
    fn drive_type_of(letter: char) -> u32 {
        use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
        let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
        // SAFETY: NUL-terminated root path; synchronous read-only query.
        unsafe { GetDriveTypeW(root.as_ptr()) }
    }
}
#[cfg(test)]
mod parking_tests {
    use super::parking::{adopt, Policy};

    fn policy(fixed: bool, removable: bool) -> Policy {
        Policy {
            auto_include_fixed: fixed,
            auto_include_removable: removable,
        }
    }

    #[test]
    fn adopt_fixed_follows_fixed_flag() {
        const DRIVE_FIXED: u32 = 3;
        assert!(adopt(DRIVE_FIXED, &policy(true, false)));
        assert!(!adopt(DRIVE_FIXED, &policy(false, true)));
    }

    #[test]
    fn adopt_removable_follows_removable_flag() {
        const DRIVE_REMOVABLE: u32 = 2;
        assert!(adopt(DRIVE_REMOVABLE, &policy(false, true)));
        assert!(!adopt(DRIVE_REMOVABLE, &policy(true, false)));
    }

    #[test]
    fn adopt_ignores_other_types() {
        // 0 unknown, 1 no-root, 4 remote, 5 cdrom, 6 ramdisk: never adopted.
        for drive_type in [0u32, 1, 4, 5, 6, 99] {
            assert!(!adopt(drive_type, &policy(true, true)), "type {drive_type}");
        }
    }
}

/// Rescan worker: drains [`Shared::rescan_queue`] (`None` = all volumes).
fn spawn_rescan_worker(shared: &Arc<Shared>) {
    let worker = Arc::clone(shared);
    match std::thread::Builder::new()
        .name("flokid-rescan".to_owned())
        .spawn(move || {
            while !is_shutdown(&worker) {
                std::thread::sleep(Duration::from_millis(500));
                if is_shutdown(&worker) {
                    return;
                }
                let requests = {
                    let mut queue = worker.rescan_queue.lock().expect("rescan lock poisoned");
                    std::mem::take(&mut *queue)
                };
                for request in requests {
                    match request {
                        Some(letter) => {
                            tracing::info!(target: "flokid", volume = %letter, "rescan requested");
                            full_rescan(&worker, letter);
                            if is_monitored(&worker, letter) {
                                spawn_tail(&worker, letter);
                            }
                        }
                        None => {
                            let letters: Vec<char> = {
                                let index = worker.index.read().expect("index lock poisoned");
                                index.volumes.iter().map(|v| v.letter).collect()
                            };
                            for letter in letters {
                                if is_shutdown(&worker) {
                                    return;
                                }
                                full_rescan(&worker, letter);
                                if is_monitored(&worker, letter) {
                                    spawn_tail(&worker, letter);
                                }
                            }
                        }
                    }
                }
            }
        }) {
        Ok(handle) => shared
            .aux_handles
            .lock()
            .expect("aux lock poisoned")
            .push(handle),
        Err(e) => tracing::warn!(target: "flokid", error = %e, "cannot spawn rescan thread"),
    }
}

/// Join every background thread (tails + boot/persist/rescan). The shutdown
/// flag is already set when this runs, so each loop exits between batches;
/// no new threads can appear (`spawn_tail` refuses once shutdown is set and
/// all spawners are among the joined threads).
fn join_background_threads(shared: &Arc<Shared>) {
    let tails: Vec<(char, std::thread::JoinHandle<()>)> = shared
        .tail_handles
        .lock()
        .expect("tail lock poisoned")
        .drain()
        .collect();
    let aux: Vec<std::thread::JoinHandle<()>> = shared
        .aux_handles
        .lock()
        .expect("aux lock poisoned")
        .drain(..)
        .collect();
    for (letter, handle) in tails {
        if handle.join().is_err() {
            tracing::warn!(target: "flokid", volume = %letter, "tail thread panicked");
        }
    }
    for handle in aux {
        if handle.join().is_err() {
            tracing::warn!(target: "flokid", "background thread panicked");
        }
    }
}

/// Save the index now (compacts first when garbage exceeds 1%).
///
/// Refuses (and logs why) while any volume is mid-scan or was never scanned
/// to completion: persisting then would bless a partial volume and its stale
/// cursor as ready. The write lock is held only for the `compact()`
/// decision; the file itself is serialized under a READ lock
/// (`Index::save` takes `&self`), so searches keep working while the bytes
/// hit the disk. The guard is re-checked after serialization and before the
/// rename, so a scan that starts (or aborts) mid-save discards the staging
/// file instead of committing it.
pub fn save_index(shared: &Shared) {
    // One save at a time process-wide: `Index::save` writes a fixed
    // `<path>.tmp`, so concurrent callers would interleave writes and can
    // persist a corrupt `index.bin`.
    let _save_guard = SAVE_LOCK.lock().expect("save lock poisoned");
    if let Some(reason) = save_blocked_reason(shared) {
        tracing::warn!(
            target: "flokid",
            path = %shared.index_path.display(),
            reason = %reason,
            "skipping index save"
        );
        return;
    }
    // Fold `pending` and collapse FRN runs off-lock first (snapshot under the
    // read lock, O(1) install), so the serialization below holds the read
    // lock for pure I/O instead of merging millions of ids while tail
    // writers (and the searches queued behind them) wait. Compaction stays
    // with the tail's 10% garbage threshold: it renumbers ids and must not
    // run under the write lock on every save.
    let needs_fold = {
        let index = shared.index.read().expect("index lock poisoned");
        !(index.by_name_is_fresh() && index.pending_len() == 0 && index.frn_is_fresh())
    };
    if needs_fold {
        commit_sorted_snapshot(shared, "save-fold", None);
    }
    let fingerprint: SaveFingerprint = {
        let index = shared.index.read().expect("index lock poisoned");
        (index.mutation(), index.volumes.clone(), index.targets)
    };
    if LAST_SAVED
        .lock()
        .expect("last-saved lock poisoned")
        .as_ref()
        == Some(&fingerprint)
        && shared.index_path.exists()
    {
        tracing::debug!(target: "flokid", "index unchanged since last save; skipping");
        return;
    }
    let staging: PathBuf = format!("{}.staging", shared.index_path.display()).into();
    let (saved, entries) = {
        let index = shared.index.read().expect("index lock poisoned");
        let entries = index.len();
        (index.save(&staging), entries)
    };
    if let Some(reason) = save_blocked_reason(shared) {
        tracing::warn!(
            target: "flokid",
            path = %shared.index_path.display(),
            reason = %reason,
            "discarding just-serialized index"
        );
        let _ = std::fs::remove_file(&staging);
        return;
    }
    match saved {
        Ok(()) => match std::fs::rename(&staging, &shared.index_path) {
            Ok(()) => {
                *LAST_SAVED.lock().expect("last-saved lock poisoned") = Some(fingerprint);
                tracing::info!(
                    target: "flokid",
                    path = %shared.index_path.display(),
                    entries,
                    "saved index"
                );
                log_memory_breakdown(&shared.index.read().expect("index lock poisoned"));
            }
            Err(e) => {
                tracing::warn!(
                    target: "flokid",
                    path = %shared.index_path.display(),
                    error = %e,
                    "index commit rename failed"
                );
                let _ = std::fs::remove_file(&staging);
            }
        },
        Err(e) => {
            tracing::warn!(
                target: "flokid",
                path = %shared.index_path.display(),
                error = %e,
                "index save failed"
            );
            let _ = std::fs::remove_file(&staging);
        }
    }
}

/// Flag shutdown, join the tail/scan threads, save the settled index, and
/// exit the process.
///
/// Used for the `Shutdown` pipe request. The save path is single-winner:
/// the main thread runs the same join + save after `serve()` returns, and
/// two concurrent `save_index` calls interleave writes on the same `.tmp`
/// file. [`try_claim_shutdown_save`] decides once per process — the winner
/// joins, saves, and exits; a loser returns immediately (its connection
/// just closes) and the winner's `std::process::exit` ends the process.
/// Every background thread is joined before the save, and the save itself
/// refuses while any volume is mid-scan or incomplete, so the persisted
/// index is never a half-applied rescan.
pub fn shutdown_save_and_exit(shared: &Arc<Shared>, code: i32) {
    request_shutdown(shared);
    if !try_claim_shutdown_save() {
        return;
    }
    join_background_threads(shared);
    save_index(shared);
    tracing::info!(target: "flokid", "shutdown requested; exiting");
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// The bootstrap queue runs volumes strictly sequentially in letter
    /// order: the injected fake records live concurrency and visit order, so
    /// any future parallelization of the queue fails here (concurrent scans
    /// blew peak RSS past 1.4 GB).
    #[test]
    fn volume_queue_runs_sequentially_in_letter_order() {
        let shared = Arc::new(Shared::new(
            std::path::PathBuf::from("test-index.bin"),
            "test-pipe".to_owned(),
        ));
        let live = Arc::new(AtomicUsize::new(0));
        let max_live = Arc::new(AtomicUsize::new(0));
        let order = Arc::new(Mutex::new(Vec::new()));
        run_volume_queue(&shared, vec!['D', 'F', 'C'], |_, letter| {
            let cur = live.fetch_add(1, Ordering::SeqCst) + 1;
            max_live.fetch_max(cur, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(5));
            order.lock().expect("order lock poisoned").push(letter);
            live.fetch_sub(1, Ordering::SeqCst);
        });
        assert_eq!(
            max_live.load(Ordering::SeqCst),
            1,
            "volumes scanned concurrently"
        );
        assert_eq!(
            *order.lock().expect("order lock poisoned"),
            vec!['C', 'D', 'F']
        );
    }

    /// Scan commits park every *other* volume's tail (their volume locks are
    /// held for the commit, released after) and never touch the committing
    /// volume's own lock, which the caller already holds. Unparked tails
    /// raced the install and forced a 5.6 s in-lock rebuild live.
    #[test]
    fn scan_commits_and_removals_park_the_right_tails() {
        let shared = Shared::new(
            std::path::PathBuf::from("test-index.bin"),
            "test-pipe".to_owned(),
        );
        for letter in ['C', 'D', 'G'] {
            shared
                .index
                .write()
                .expect("index lock poisoned")
                .add_volume(Volume {
                    letter,
                    guid: [0; 16],
                    journal_id: 1,
                    next_usn: 0,
                    root_frn: 5,
                    enabled: true,
                    monitor: true,
                });
        }
        let held = |l: char| shared.vol_lock(l).try_lock().is_err();
        let inside = with_other_tails_parked(&shared, 'G', || (held('C'), held('D'), held('G')));
        assert_eq!(inside, (true, true, false));
        assert_eq!((held('C'), held('D')), (false, false));
        // Removal parks every tail, the removed volume's own included.
        let inside = with_tails_parked(&shared, None, || (held('C'), held('D'), held('G')));
        assert_eq!(inside, (true, true, true));
        assert!(remove_volume_off_lock(&shared, 'D'));
        assert!(!remove_volume_off_lock(&shared, 'D'), "already gone");
        let letters: Vec<char> = shared
            .index
            .read()
            .expect("index lock poisoned")
            .volumes
            .iter()
            .map(|v| v.letter)
            .collect();
        assert_eq!(letters, ['C', 'G']);
        assert_eq!((held('C'), held('D'), held('G')), (false, false, false));
    }

    /// The second `flokid run` in the same session must refuse: two
    /// indexers would fight over one pipe name and orphan each other's
    /// scans. The first claim stays held until its guard drops.
    #[test]
    #[cfg(windows)]
    fn second_singleton_claim_refuses_while_first_held() {
        let name = format!("Local\\FlokiSingletonTest-{}", std::process::id());
        let first = claim_singleton(&name);
        let Singleton::Held { handle } = first else {
            panic!("first claim must hold");
        };
        let guard = SingletonGuard(handle);
        assert_eq!(claim_singleton(&name), Singleton::AlreadyRunning);
        drop(guard);
        let Singleton::Held { handle } = claim_singleton(&name) else {
            panic!("claim must succeed after release");
        };
        release_singleton(handle);
    }

    /// Volume fixture for tail-lifecycle tests (`add_volume` forces both
    /// policy flags on; flip `monitor` afterwards for scan-only volumes).
    fn test_volume(letter: char) -> Volume {
        Volume {
            letter,
            guid: [0; 16],
            journal_id: 0,
            next_usn: 0,
            root_frn: 0,
            enabled: true,
            monitor: true,
        }
    }

    /// `is_monitored` requires an indexed volume with `monitor = true`:
    /// unindexed letters and scan-only volumes both report false so no tail
    /// is spawned for them (a tail on a missing record exits immediately).
    #[test]
    fn is_monitored_requires_indexed_and_monitored() {
        let shared = Arc::new(Shared::new(
            std::path::PathBuf::from("test-index.bin"),
            "test-pipe".to_owned(),
        ));
        {
            let mut index = shared.index.write().expect("index lock poisoned");
            index.add_volume(test_volume('Q'));
            index.add_volume(test_volume('R'));
            index.volumes[1].monitor = false;
        }
        assert!(is_monitored(&shared, 'Q'));
        assert!(!is_monitored(&shared, 'R'), "scan-only volume");
        assert!(!is_monitored(&shared, 'Z'), "unindexed letter");
    }

    /// The `live_vols` claim is taken before the thread spawns: a second
    /// `spawn_tail` while one is claimed is a no-op, so concurrent spawners
    /// (bootstrap, rescan worker, pipe op, reconcile) can never create
    /// duplicate tails for one volume.
    #[test]
    fn spawn_tail_claim_is_atomic() {
        let shared = Arc::new(Shared::new(
            std::path::PathBuf::from("test-index.bin"),
            "test-pipe".to_owned(),
        ));
        spawn_tail(&shared, 'Q');
        spawn_tail(&shared, 'Q');
        assert_eq!(
            shared
                .tail_handles
                .lock()
                .expect("tail lock poisoned")
                .len(),
            1,
            "second spawn must be a no-op"
        );
        assert!(shared
            .live_vols
            .lock()
            .expect("live lock poisoned")
            .contains(&'Q'));
    }

    /// Reconcile restarts tails for indexed+monitored volumes missing one,
    /// leaves scan-only volumes and unindexed letters alone, and reaps a
    /// finished handle's stale `live_vols` claim so it can't wedge a volume.
    #[test]
    fn reconcile_restarts_only_missing_monitored_tails() {
        let shared = Arc::new(Shared::new(
            std::path::PathBuf::from("test-index.bin"),
            "test-pipe".to_owned(),
        ));
        // A spawned tail must never drop a volume mid-test.
        shared
            .targets
            .write()
            .expect("targets lock poisoned")
            .auto_remove_offline = false;
        {
            let mut index = shared.index.write().expect("index lock poisoned");
            index.add_volume(test_volume('Q'));
            index.add_volume(test_volume('R'));
            index.volumes[1].monitor = false;
        }
        // Stale claim + finished handle on an unindexed letter: reaped, and
        // not respawned (reconcile only serves indexed volumes).
        let dead = std::thread::spawn(|| {});
        while !dead.is_finished() {
            std::thread::yield_now();
        }
        shared
            .tail_handles
            .lock()
            .expect("tail lock poisoned")
            .insert('Z', dead);
        shared
            .live_vols
            .lock()
            .expect("live lock poisoned")
            .insert('Z');

        reconcile_tails(&shared);

        let live = shared.live_vols.lock().expect("live lock poisoned");
        assert!(live.contains(&'Q'), "monitored volume must get a tail");
        assert!(!live.contains(&'R'), "scan-only volume must not");
        assert!(!live.contains(&'Z'), "stale claim must be reaped");
        drop(live);
        assert!(!shared
            .tail_handles
            .lock()
            .expect("tail lock poisoned")
            .contains_key(&'Z'));
    }
    /// The default pipe maps to the bare mutex name; custom pipes hash so
    /// `FLOKI_PIPE` tests never contend with the production indexer.
    #[test]
    fn singleton_name_scopes_custom_pipes() {
        assert_eq!(singleton_name(floki_proto::PIPE_NAME), SINGLETON_MUTEX);
        let a = singleton_name(r"\\.\pipe\floki-test-a");
        let b = singleton_name(r"\\.\pipe\floki-test-b");
        assert_ne!(a, b);
        assert!(a.starts_with(SINGLETON_MUTEX));
    }
}
