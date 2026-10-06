//! First-boot scan throughput guard: 200k synthetic records through the real
//! [`floki_service::daemon::flush_scan_batch`] path must complete in under
//! 2 s in debug, with every write-lock hold under 300 ms.
//!
//! Regression test for the O(n²) first-boot scan: per-record `apply` on a
//! fresh `frn_index` binary-searches plus `Vec::insert`s per record, which
//! indexed C: at ~8k records/s. Flushes stay append-only (`Index::push`);
//! the sorts run off-lock via snapshot/install, so the commit holds the
//! write lock only for microsecond-scale swaps.

#![cfg(windows)]

use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use floki_core::Volume;
use floki_ntfs::RawRecord;
use floki_service::daemon::{commit_sorted_snapshot, flush_scan_batch, SCAN_BATCH};
use floki_service::state::Shared;

/// Synthetic MFT-style records: `count` files plus a root dir and a few
/// NTFS metafiles (which the flush path must filter once the root is known).
fn synthetic_records(count: usize, root_frn: u64) -> Vec<RawRecord> {
    let mut records = Vec::with_capacity(count + 8);
    records.push(RawRecord {
        frn: root_frn,
        parent_frn: root_frn,
        attrs: 0x10, // FILE_ATTRIBUTE_DIRECTORY
        name: String::new(),
    });
    for metafile in ["$MFT", "$Bitmap", "$LogFile", "$Extend"] {
        records.push(RawRecord {
            frn: 100 + metafile.len() as u64,
            parent_frn: root_frn,
            attrs: 0x6, // HIDDEN | SYSTEM
            name: metafile.to_owned(),
        });
    }
    for i in 0..count {
        records.push(RawRecord {
            // MFT order is NOT FRN-sorted (sequence number in the high bits),
            // so stride the FRNs to defeat any accidental sorted-input
            // fast path, mirroring the live quadratic.
            frn: 1_000_000 + (i as u64).wrapping_mul(1_000_003),
            parent_frn: root_frn,
            attrs: 0,
            name: format!("f{i}.txt"),
        });
    }
    records
}

#[test]
fn scan_flush_200k_stays_linear() {
    let index_path =
        std::env::temp_dir().join(format!("floki-scan-perf-{}.bin", std::process::id()));
    let shared = Arc::new(Shared::new(index_path, "floki-scan-perf".to_owned()));
    let vol_idx: u8 = {
        let mut index = shared.index.write().expect("index lock poisoned");
        index.add_volume(Volume {
            letter: 'C',
            guid: [0; 16],
            journal_id: 1,
            next_usn: 0,
            root_frn: 5,
            enabled: true,
            monitor: true,
        })
    };
    let root_seen = AtomicU64::new(5);

    let started = Instant::now();
    let mut push_hold_ms = 0u128;
    let mut batch: Vec<RawRecord> = Vec::with_capacity(SCAN_BATCH);
    for record in synthetic_records(200_000, 5) {
        batch.push(record);
        if batch.len() >= SCAN_BATCH {
            push_hold_ms += flush_scan_batch(&shared, vol_idx, &mut batch, &root_seen);
        }
    }
    if !batch.is_empty() {
        push_hold_ms += flush_scan_batch(&shared, vol_idx, &mut batch, &root_seen);
    }
    assert!(
        push_hold_ms < 300,
        "200k pushes held the write lock {push_hold_ms}ms; pushes must stay <300ms total"
    );
    // Commit: sorts off-lock, microsecond-scale install.
    let commit_hold_ms = commit_sorted_snapshot(&shared, "scan-perf", Some('C'));
    eprintln!("scan-perf: push_hold_ms={push_hold_ms} commit_hold_ms={commit_hold_ms}");
    assert!(
        commit_hold_ms < 300,
        "scan commit held the write lock {commit_hold_ms}ms; install must stay <300ms"
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "200k scan path took {elapsed:?}; the O(n²) apply-per-record regression is back"
    );

    // Correctness spot-checks: metafiles filtered, arrays fresh after the
    // install, every synthetic file present and path-resolvable.
    let index = shared.index.read().expect("index lock poisoned");
    assert!(index.by_name_is_fresh());
    assert!(index.frn_is_fresh());
    assert_eq!(index.len(), 200_001); // 200k files + root; 4 metafiles dropped
    assert!(index.lookup(0, 1_000_000).is_some());
    let last = 1_000_000 + 199_999u64.wrapping_mul(1_000_003);
    let id = index.lookup(0, last).expect("last record indexed");
    assert_eq!(index.name(id), Some("f199999.txt"));
    assert_eq!(index.path(id), r"C:\f199999.txt");
    assert!(index.lookup(0, 104).is_none()); // `$MFT` (frn 100+4) filtered
}
