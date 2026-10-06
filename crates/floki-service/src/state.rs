//! Shared daemon state: the index plus the coordination primitives
//! around it (scan progress, per-volume locks, rescan queue, per-client
//! `prev` caches, shutdown flag).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Instant;

use floki_core::{Index, Query};
use floki_proto::IndexState;

/// Cap on concurrent in-flight searches: extra `Search` requests wait on
/// [`Shared::search_slot_cv`] instead of piling rayon work onto the machine.
pub const MAX_IN_FLIGHT_SEARCHES: usize = 2;

/// Process-wide shutdown latch, set by the console handler and by the
/// `Shutdown` pipe request. All daemon loops poll [`is_shutdown`].
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// True once shutdown was requested (console signal or pipe request).
#[must_use]
pub fn is_shutdown(shared: &Shared) -> bool {
    SHUTDOWN.load(Ordering::Relaxed) || shared.shutdown.load(Ordering::Relaxed)
}

/// Flag shutdown on both the global latch and `shared`.
pub fn request_shutdown(shared: &Shared) {
    SHUTDOWN.store(true, Ordering::Relaxed);
    shared.shutdown.store(true, Ordering::Relaxed);
}

/// One-shot claim for the shutdown save: the first caller wins and is the
/// only thread allowed to join background threads and persist the index.
/// A pipe `Shutdown` reaches the save path twice — the serving client
/// thread (`shutdown_save_and_exit`) and the main thread after `serve()`
/// returns — and two concurrent `save_index` calls interleave writes on
/// the same `.tmp` file, which can persist a corrupt `index.bin`.
pub static SHUTDOWN_SAVED: AtomicBool = AtomicBool::new(false);

/// Try to become the thread that performs the shutdown join + save.
/// `true` exactly once per process; every later caller gets `false` and
/// must not save.
#[must_use]
pub fn try_claim_shutdown_save() -> bool {
    SHUTDOWN_SAVED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// Progress of one in-flight full-volume scan.
pub struct ScanProgress {
    /// Volume letter being scanned.
    pub letter: char,
    /// Records pushed so far (reported as `done` in [`IndexState::Scanning`]).
    pub done: AtomicU64,
}

/// Last result set of one pipe client, for `prev` narrowing.
pub struct PrevEntry {
    /// Parsed form of the query the cached hits were computed for
    /// (its `raw` string feeds [`floki_core::prev_reusable`]).
    pub query: Query,
    /// Full previous page of hits.
    pub hits: Vec<floki_core::Hit>,
    /// Whether the cached page was complete (total <= requested max).
    /// Never true for a paged (`offset > 0`) request (F9).
    pub complete: bool,
    /// [`Index::epoch`](floki_core::Index::epoch) the cached hits were
    /// computed at; the entry is usable only while the epoch is unchanged
    /// (`compact()` renumbers every id, F1).
    pub epoch: u64,
}

/// Everything the daemon threads share.
pub struct Shared {
    /// The index. Readers: pipe searches, status. Writers: scans, tails.
    pub index: RwLock<Index>,
    /// When the daemon started (for `uptime_s`).
    pub start: Instant,
    /// Where the index is persisted.
    pub index_path: PathBuf,
    /// Pipe name the server listens on.
    pub pipe_name: String,
    /// Set by Ctrl-C / `Shutdown`; all loops poll this.
    pub shutdown: AtomicBool,
    /// True only while the initial `index.bin` load is in flight.
    pub loading: AtomicBool,
    /// Currently running full-volume scans (empty when idle).
    pub active_scans: Mutex<Vec<std::sync::Arc<ScanProgress>>>,
    /// One mutex per target volume; serializes tail polls against rescans.
    pub vol_locks: Mutex<HashMap<char, std::sync::Arc<Mutex<()>>>>,
    /// Pending rescan requests (`None` = all volumes). Drained by `run`.
    pub rescan_queue: Mutex<Vec<Option<char>>>,
    /// Global NTFS-targets policy (auto-include / auto-remove). Mirrors
    /// [`Index::targets`](floki_core::Index::targets); the daemon keeps this
    /// copy outside the index lock for the arrival/offline polls, and every
    /// pipe `TargetsConfigSet` writes both (plus a save) so they never
    /// diverge.
    pub targets: RwLock<floki_core::TargetsConfig>,
    /// Last result set per `client_id`, for `prev` reuse.
    pub prev_cache: Mutex<HashMap<u64, PrevEntry>>,
    /// Volumes with a claimed or running tail thread (`VolumeStatus::live`
    /// combines this with [`Shared::open_vols`]). Inserted atomically by
    /// `spawn_tail` before the thread spawns; removed by the tail's exit
    /// guard (panic-safe) or by the reconcile pass reaping a dead handle.
    pub live_vols: Mutex<HashSet<char>>,
    /// Volumes whose tail currently holds an open `VolumeHandle`. A tail
    /// retrying an unopenable volume is claimed (in `live_vols`) but not
    /// open, so `VolumeStatus::live` reports the volume's real reachability
    /// instead of mere thread liveness.
    pub open_vols: Mutex<HashSet<char>>,
    /// Concurrently connected pipe clients (capped; see `server::MAX_CLIENTS`).
    pub active_clients: AtomicUsize,
    /// Tail thread handles by volume, joined on graceful shutdown (F4).
    pub tail_handles: Mutex<HashMap<char, JoinHandle<()>>>,
    /// Boot / persist / rescan thread handles, joined on shutdown (F4).
    pub aux_handles: Mutex<Vec<JoinHandle<()>>>,
    /// Serializes full-volume scans across the bootstrap thread, the rescan
    /// worker, and journal-wrap rescans: concurrent scans blew peak RSS
    /// (staging buffers per scan), so only one scan runs at a time.
    pub scan_lock: Mutex<()>,
    /// Searches currently holding a slot (capped at
    /// [`MAX_IN_FLIGHT_SEARCHES`]).
    pub search_in_flight: Mutex<usize>,
    /// Signalled whenever a search slot frees up.
    pub search_slot_cv: Condvar,
}

impl Shared {
    /// Fresh state around an (initially empty) index.
    pub fn new(index_path: PathBuf, pipe_name: String) -> Self {
        Self {
            index: RwLock::new(Index::new()),
            start: Instant::now(),
            index_path,
            pipe_name,
            shutdown: AtomicBool::new(false),
            loading: AtomicBool::new(false),
            active_scans: Mutex::new(Vec::new()),
            vol_locks: Mutex::new(HashMap::new()),
            rescan_queue: Mutex::new(Vec::new()),
            targets: RwLock::new(floki_core::TargetsConfig::default()),
            prev_cache: Mutex::new(HashMap::new()),
            live_vols: Mutex::new(HashSet::new()),
            open_vols: Mutex::new(HashSet::new()),
            active_clients: AtomicUsize::new(0),
            tail_handles: Mutex::new(HashMap::new()),
            aux_handles: Mutex::new(Vec::new()),
            scan_lock: Mutex::new(()),
            search_in_flight: Mutex::new(0),
            search_slot_cv: Condvar::new(),
        }
    }

    /// Volume index for `letter`, or `None` when not indexed.
    pub fn find_vol_idx(&self, letter: char) -> Option<u8> {
        let index = self.index.read().expect("index lock poisoned");
        index
            .volumes
            .iter()
            .position(|v| v.letter == letter)
            .map(|i| i as u8)
    }

    /// Live (non-tombstone) entry count.
    pub fn live_count(&self) -> u64 {
        let index = self.index.read().expect("index lock poisoned");
        (index.len() - index.tombstone_count()) as u64
    }

    /// Current lifecycle state for `Status` responses.
    pub fn current_state(&self) -> IndexState {
        if self.loading.load(std::sync::atomic::Ordering::Relaxed) {
            return IndexState::Loading;
        }
        let scans = self.active_scans.lock().expect("scans lock poisoned");
        if let Some(scan) = scans.last() {
            return IndexState::Scanning {
                volume: scan.letter,
                done: scan.done.load(std::sync::atomic::Ordering::Relaxed),
            };
        }
        IndexState::Ready
    }

    /// Per-volume mutex, created on first use.
    pub fn vol_lock(&self, letter: char) -> std::sync::Arc<Mutex<()>> {
        let mut locks = self.vol_locks.lock().expect("vol lock poisoned");
        locks
            .entry(letter)
            .or_insert_with(|| std::sync::Arc::new(Mutex::new(())))
            .clone()
    }

    /// Block until fewer than [`MAX_IN_FLIGHT_SEARCHES`] searches are
    /// running, then hold a slot until the guard drops. Extra `Search`
    /// requests wait here instead of oversubscribing the machine.
    pub fn acquire_search_slot(&self) -> SearchSlot<'_> {
        let mut in_flight = self.search_in_flight.lock().expect("search lock poisoned");
        while *in_flight >= MAX_IN_FLIGHT_SEARCHES {
            in_flight = self
                .search_slot_cv
                .wait(in_flight)
                .expect("search lock poisoned");
        }
        *in_flight += 1;
        SearchSlot { shared: self }
    }

    /// Current in-flight search count (scan pacing consults this to yield).
    pub fn search_in_flight_count(&self) -> usize {
        *self.search_in_flight.lock().expect("search lock poisoned")
    }
}

/// RAII guard for one [`MAX_IN_FLIGHT_SEARCHES`] slot: releases and wakes a
/// waiter on drop.
pub struct SearchSlot<'a> {
    shared: &'a Shared,
}

impl Drop for SearchSlot<'_> {
    fn drop(&mut self) {
        let mut in_flight = self
            .shared
            .search_in_flight
            .lock()
            .expect("search lock poisoned");
        *in_flight = in_flight.saturating_sub(1);
        self.shared.search_slot_cv.notify_one();
    }
}
/// Current process resident set size in bytes (0 on failure).
pub fn rss_bytes() -> u64 {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: `GetCurrentProcess` needs no cleanup; `counters` is a live
    // struct of the documented size; the call is synchronous.
    unsafe {
        let process = GetCurrentProcess();
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = GetProcessMemoryInfo(
            process,
            &mut counters,
            size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        );
        if ok == 0 {
            let _ = GetLastError();
            return 0;
        }
        counters.WorkingSetSize as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn test_shared() -> Shared {
        Shared::new(
            std::path::PathBuf::from("test-index.bin"),
            "test-pipe".to_owned(),
        )
    }

    /// The semaphore admits 2 searches and blocks the 3rd until one slot
    /// releases: the waiter must not proceed while both are held, then must
    /// proceed promptly once one drops.
    #[test]
    fn search_slots_admit_two_then_block() {
        let shared = std::sync::Arc::new(test_shared());
        let first = shared.acquire_search_slot();
        let second = shared.acquire_search_slot();
        assert_eq!(shared.search_in_flight_count(), 2);

        let (tx, rx) = mpsc::channel();
        let worker = std::sync::Arc::clone(&shared);
        std::thread::spawn(move || {
            let _third = worker.acquire_search_slot();
            tx.send(()).expect("report acquisition");
        });
        // The 3rd waiter must still be blocked while both slots are held.
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "3rd search proceeded with both slots held"
        );
        drop(first);
        // Releasing one slot unblocks the waiter.
        rx.recv_timeout(Duration::from_secs(5))
            .expect("3rd search stuck after a slot released");
        drop(second);
    }
}
