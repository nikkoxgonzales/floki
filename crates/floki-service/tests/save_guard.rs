//! Save-guard tests: a volume whose scan did not complete must never be
//! persisted as ready.
//!
//! Drives the real [`floki_service::daemon::save_index`] on a synthetic index:
//! with a volume flagged mid-scan (via `active_scans`) or aborted (via
//! `mark_volume_incomplete`), the index file must not appear; once the flags
//! clear, the save lands and the volume loads back.

#![cfg(windows)]

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use floki_core::{Index, Volume, DIRECTORY};
use floki_service::daemon::{clear_volume_incomplete, mark_volume_incomplete, save_index};
use floki_service::state::{ScanProgress, Shared};

/// Serializes these tests: the incomplete set is process-global and
/// `save_index` refuses while ANY volume is flagged, so the two tests must
/// not overlap.
static SAVE_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn synthetic_index_for(letter: char) -> Index {
    let mut index = Index::new();
    index.add_volume(Volume {
        letter,
        guid: [0xC; 16],
        journal_id: 7,
        next_usn: 1234,
        root_frn: 100,
        enabled: true,
        monitor: true,
    });
    index.push(0, 100, 100, "", DIRECTORY);
    index.push(0, 101, 100, "docs", DIRECTORY);
    index.push(0, 102, 101, "report.txt", 0);
    index.rebuild_by_name();
    index
}

fn fresh_shared(tag: &str, letter: char) -> Arc<Shared> {
    let path =
        std::env::temp_dir().join(format!("floki-save-guard-{tag}-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let shared = Arc::new(Shared::new(path, format!("floki-save-guard-{tag}")));
    *shared.index.write().expect("index lock poisoned") = synthetic_index_for(letter);
    shared
}

#[test]
fn save_refused_while_scan_in_progress() {
    let _guard = SAVE_GUARD.lock().expect("save guard lock poisoned");
    // Distinct letter from the aborted-volume test: the incomplete set is
    // process-global and the two tests run in parallel.
    clear_volume_incomplete('C');
    let shared = fresh_shared("midscan", 'C');
    assert!(!shared.index_path.exists());

    // Flag volume C as mid-scan, exactly as `full_rescan` does on entry.
    shared
        .active_scans
        .lock()
        .expect("scans lock poisoned")
        .push(Arc::new(ScanProgress {
            letter: 'C',
            done: AtomicU64::new(1_000_000),
        }));
    save_index(&shared);
    assert!(
        !shared.index_path.exists(),
        "mid-scan save must not persist the index"
    );

    // Scan ends: the save lands and the volume loads back intact.
    shared
        .active_scans
        .lock()
        .expect("scans lock poisoned")
        .clear();
    save_index(&shared);
    assert!(
        shared.index_path.exists(),
        "save must proceed once no scan is in progress"
    );
    let loaded = Index::load(&shared.index_path).expect("saved index loads");
    assert!(loaded.volumes.iter().any(|v| v.letter == 'C'));
    assert_eq!(loaded.len(), 3);
    let _ = std::fs::remove_file(&shared.index_path);
    clear_volume_incomplete('C');
}

#[test]
fn save_refused_for_aborted_volume() {
    let _guard = SAVE_GUARD.lock().expect("save guard lock poisoned");
    // Distinct letter from the mid-scan test (see above).
    clear_volume_incomplete('D');
    let shared = fresh_shared("aborted", 'D');
    assert!(!shared.index_path.exists());

    // Abort path flags the volume; even with no active scan the save must
    // refuse, so a partial volume is never stored as ready.
    mark_volume_incomplete('D');
    save_index(&shared);
    assert!(
        !shared.index_path.exists(),
        "save must not persist while a volume is incomplete"
    );

    // The next successful scan clears the flag; the save lands afterwards.
    clear_volume_incomplete('D');
    save_index(&shared);
    assert!(
        shared.index_path.exists(),
        "save must proceed once the volume completed a scan"
    );
    let loaded = Index::load(&shared.index_path).expect("saved index loads");
    assert!(loaded.volumes.iter().any(|v| v.letter == 'D'));
    let _ = std::fs::remove_file(&shared.index_path);
    clear_volume_incomplete('D');
}
