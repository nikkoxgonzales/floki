//! Pending-fold guard: a burst of `PENDING_MAX + 10` journal creates applied
//! through the tail's mapping path must leave `by_name_is_fresh()` true
//! afterwards — the per-batch fold runs before searches fall back to the
//! slow collect+sort path past the bound. Also covers the snapshot/install
//! retry the fold (and scan commits) use when a tail applies in between.

#![cfg(windows)]

use std::sync::Arc;

use floki_core::{Volume, DIRECTORY, PENDING_MAX};
use floki_ntfs::{RawRecord, UsnEvent};
use floki_service::daemon::{commit_sorted_snapshot, maybe_fold_pending};
use floki_service::mapping::apply_usn_event;
use floki_service::state::Shared;

fn burst_shared(tag: &str) -> Arc<Shared> {
    let path = std::env::temp_dir().join(format!("floki-pending-{tag}-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let shared = Arc::new(Shared::new(path, format!("floki-pending-{tag}")));
    {
        let mut index = shared.index.write().expect("index lock poisoned");
        index.add_volume(Volume {
            letter: 'C',
            guid: [0; 16],
            journal_id: 1,
            next_usn: 0,
            root_frn: 5,
            enabled: true,
            monitor: true,
        });
        index.push(0, 5, 5, "", DIRECTORY);
        index.rebuild_by_name();
    }
    shared
}

/// Apply `count` creates through the exact mapping the tail loop applies per
/// journal event, under one write lock (one batch).
fn apply_burst(shared: &Arc<Shared>, prefix: &str, base_frn: u64, count: usize) {
    let mut index = shared.index.write().expect("index lock poisoned");
    for i in 0..count {
        let record = RawRecord {
            frn: base_frn + i as u64,
            parent_frn: 5,
            attrs: 0,
            name: format!("{prefix}{i}.txt"),
        };
        assert!(apply_usn_event(&mut index, 0, &UsnEvent::Create(record), 5));
    }
}

fn is_fresh(shared: &Arc<Shared>) -> bool {
    shared
        .index
        .read()
        .expect("index lock poisoned")
        .by_name_is_fresh()
}

#[test]
fn tail_burst_folds_pending_before_slow_path() {
    let shared = burst_shared("fold");
    assert!(is_fresh(&shared));
    // Small bursts stay put: hysteresis, no rebuild per tick.
    assert!(!maybe_fold_pending(&shared));

    // Burst of PENDING_MAX + 10 creates through the tail's mapping.
    let burst = PENDING_MAX + 10;
    apply_burst(&shared, "f", 1_000, burst);
    {
        let index = shared.index.read().expect("index lock poisoned");
        assert_eq!(index.pending_len(), burst);
        assert!(
            !index.by_name_is_fresh(),
            "burst past PENDING_MAX must trip the freshness bound"
        );
    }

    // The fold the tail runs after each batch restores the fast path.
    assert!(maybe_fold_pending(&shared));
    assert!(is_fresh(&shared));
    {
        let index = shared.index.read().expect("index lock poisoned");
        assert_eq!(index.pending_len(), 0);
        // Spot-check: a burst file resolves and searches find it.
        let id = index.lookup(0, 1_000).expect("first burst file indexed");
        assert_eq!(index.name(id), Some("f0.txt"));
        let query = floki_core::parse(&format!("f{}.txt", burst - 1));
        let hits = floki_core::search(
            &index,
            &query,
            &floki_core::SearchOptions::new(100, 0, floki_core::Sort::NameAsc),
            None,
        );
        let want = format!("f{}.txt", burst - 1);
        assert!(
            hits.iter().any(|h| index.name(h.id) == Some(want.as_str())),
            "last burst file must be searchable"
        );
    }
}

/// A concurrent `apply` between snapshot and install must fail the install
/// (never install torn arrays); retrying with a fresh snapshot succeeds and
/// the final index holds every entry from both sides of the race.
#[test]
fn snapshot_install_retry_recovers_from_race() {
    let shared = burst_shared("retry");
    apply_burst(&shared, "r", 1_000, 200);

    // Snapshot, then a tail-side apply lands "between" snapshot and install.
    let stale = {
        shared
            .index
            .read()
            .expect("index lock poisoned")
            .sorted_snapshot()
    };
    apply_burst(&shared, "z", 900_000, 1);
    // The stale install fails instead of committing torn arrays; the index
    // is untouched by the attempt.
    {
        let mut index = shared.index.write().expect("index lock poisoned");
        assert!(
            !index.install_sorted(stale),
            "stale snapshot must not install"
        );
    }

    // Retry path (what the commit helper automates): fresh snapshot installs
    // at once, write hold stays microsecond-scale.
    let hold_ms = commit_sorted_snapshot(&shared, "test-retry", Some('C'));
    eprintln!("test-retry: commit_hold_ms={hold_ms}");
    assert!(
        hold_ms < 300,
        "snapshot install held the write lock {hold_ms}ms"
    );

    // Final index correct: fresh arrays, both sides of the race present.
    {
        let index = shared.index.read().expect("index lock poisoned");
        assert!(index.by_name_is_fresh());
        assert_eq!(index.pending_len(), 0);
        let race = index.lookup(0, 900_000).expect("racy apply indexed");
        assert_eq!(index.name(race), Some("z0.txt"));
        let base = index.lookup(0, 1_000).expect("burst file indexed");
        assert_eq!(index.name(base), Some("r0.txt"));
        assert_eq!(index.path(race), r"C:\z0.txt");
    }
    let _ = std::fs::remove_file(&shared.index_path);
}

/// A large journal batch (boot replay of a long backlog, or a build burst
/// on one tail tick) goes through `apply_journal_events` in short write
/// locks: a reader polling the index throughout never waits long, every
/// event lands, the cursor advances, and `pending` stays bounded by the
/// between-chunk folds. The old path applied the whole batch under one
/// write lock (27 min observed for a 1.5M-event replay at 9.3M entries).
#[test]
fn large_journal_batch_never_starves_readers() {
    use floki_service::daemon::apply_journal_events;
    use std::time::{Duration, Instant};

    let shared = burst_shared("chunked");
    let events: Vec<UsnEvent> = (0..150_000u64)
        .map(|i| {
            UsnEvent::Create(RawRecord {
                frn: 10_000 + i * 7,
                parent_frn: 5,
                attrs: 0,
                name: format!("c{i}.obj"),
            })
        })
        .collect();

    let writer = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || apply_journal_events(&shared, 'C', 5, &events, 4242))
    };
    let mut max_wait = Duration::ZERO;
    while !writer.is_finished() {
        let asked = Instant::now();
        let index = shared.index.read().expect("index lock poisoned");
        max_wait = max_wait.max(asked.elapsed());
        drop(index);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(writer.join().expect("writer thread"), Some(false));
    eprintln!("chunked apply: max reader wait {max_wait:?}");
    assert!(
        max_wait < Duration::from_millis(1_500),
        "a reader waited {max_wait:?} behind one journal batch"
    );

    let index = shared.index.read().expect("index lock poisoned");
    assert_eq!(index.volumes[0].next_usn, 4242);
    assert!(
        index.pending_len() <= PENDING_MAX,
        "folds keep pending bounded"
    );
    for i in (0..150_000u64).step_by(4_999) {
        let id = index.lookup(0, 10_000 + i * 7).expect("event applied");
        assert_eq!(index.name(id), Some(format!("c{i}.obj").as_str()));
    }
    drop(index);
    let _ = std::fs::remove_file(&shared.index_path);
}
