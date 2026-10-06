//! Pipe integration test: real server + real client, synthetic index, no NTFS.
//!
//! Starts [`floki_service::server::serve`] in-process on a unique pipe name
//! backed by a small hand-built [`floki_core::Index`], then exercises
//! `Hello`, `Status`, and `Search` through [`floki_proto::Client`]. Runs
//! without admin rights: nothing here touches a volume handle.

#![cfg(windows)]

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use floki_core::{Index, Volume, DIRECTORY};
use floki_proto::{Client, Request, Response, PROTOCOL_VERSION};
use floki_service::state::{request_shutdown, Shared, SHUTDOWN};

/// Build a three-file synthetic index on `C:`:
///
/// ```text
/// C:\
/// C:\docs\report.txt
/// C:\photo.jpg
/// ```
fn synthetic_index() -> Index {
    let mut index = Index::new();
    index.add_volume(Volume {
        letter: 'C',
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
    index.push(0, 103, 100, "photo.jpg", 0);
    index.rebuild_by_name();
    index
}

fn unique_pipe() -> String {
    format!(r"\\.\pipe\floki-test-{}", std::process::id())
}

/// Serializes the tests in this file: every one retargets the process-global
/// `FLOKI_PIPE` variable, which the pipe client resolves per call.
static PIPE_ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn connect_retry() -> Client {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Client::connect() {
            Ok(client) => return client,
            Err(e) => {
                assert!(Instant::now() < deadline, "pipe server never appeared: {e}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
fn pipe_serves_synthetic_index() {
    let _env_guard = PIPE_ENV_GUARD.lock().expect("env lock poisoned");
    // Each test in this file shuts the server down via the process-global
    // latch; reset it so this server actually serves.
    SHUTDOWN.store(false, Ordering::Relaxed);
    // The server's explicit security descriptor must build: it is what lets
    // an unelevated client connect to the elevated daemon.
    floki_service::server::pipe_security_descriptor().expect("pipe security descriptor must build");

    let pipe = unique_pipe();
    std::env::set_var("FLOKI_PIPE", &pipe);

    let index_path = std::env::temp_dir().join(format!("floki-test-{}.bin", std::process::id()));
    let shared = Arc::new(Shared::new(index_path, pipe));
    *shared.index.write().expect("index lock poisoned") = synthetic_index();

    let worker = Arc::clone(&shared);
    let server = std::thread::Builder::new()
        .name("flokid-test-server".to_owned())
        .spawn(move || floki_service::server::serve(&worker))
        .expect("spawn server thread");

    let mut client = connect_retry();

    // Hello handshake.
    match client.call(&Request::Hello {}).expect("hello") {
        Response::Hello {
            protocol,
            service_version,
        } => {
            assert_eq!(protocol, PROTOCOL_VERSION);
            assert!(!service_version.is_empty());
        }
        other => panic!("expected Hello, got {other:?}"),
    }

    // Status over the synthetic index.
    match client.call(&Request::Status {}).expect("status") {
        Response::Status {
            entries,
            volumes,
            state,
            ..
        } => {
            assert_eq!(entries, 4);
            assert_eq!(volumes.len(), 1);
            assert_eq!(volumes[0].letter, 'C');
            assert_eq!(volumes[0].entries, 4);
            assert_eq!(volumes[0].next_usn, 1234);
            assert_eq!(state, floki_proto::IndexState::Ready);
        }
        other => panic!("expected Status, got {other:?}"),
    }

    // Search finds the file with its parent directory path.
    match client
        .call(&Request::Search {
            query: "report".to_owned(),
            max_results: 100,
            offset: 0,
            sort: floki_proto::Sort::NameAsc,
            client_id: 7,
            meta: true,
        })
        .expect("search")
    {
        Response::Results {
            total,
            hits,
            elapsed_us: _,
        } => {
            assert_eq!(total, 1);
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].name, "report.txt");
            assert_eq!(hits[0].path, r"C:\docs");
            assert!(!hits[0].is_dir);
        }
        other => panic!("expected Results, got {other:?}"),
    }

    // A follow-up query extending the previous one reuses the `prev` cache
    // (same result through the narrowing path).
    match client
        .call(&Request::Search {
            query: "report.".to_owned(),
            max_results: 100,
            offset: 0,
            sort: floki_proto::Sort::NameAsc,
            client_id: 7,
            meta: true,
        })
        .expect("narrowed search")
    {
        Response::Results { total, hits, .. } => {
            assert_eq!(total, 1);
            assert_eq!(hits[0].name, "report.txt");
        }
        other => panic!("expected Results, got {other:?}"),
    }

    // Unknown queries return no hits, not an error.
    match client
        .call(&Request::Search {
            query: "zzz-no-such-file".to_owned(),
            max_results: 100,
            offset: 0,
            sort: floki_proto::Sort::NameAsc,
            client_id: 7,
            meta: true,
        })
        .expect("empty search")
    {
        Response::Results { total, hits, .. } => {
            assert_eq!(total, 0);
            assert!(hits.is_empty());
        }
        other => panic!("expected Results, got {other:?}"),
    }

    drop(client);
    request_shutdown(&shared);
    server.join().expect("server thread").expect("serve ok");
    std::env::remove_var("FLOKI_PIPE");
}

fn search(
    client: &mut Client,
    query: &str,
    max_results: u32,
    offset: u32,
    client_id: u64,
) -> Response {
    client
        .call(&Request::Search {
            query: query.to_owned(),
            max_results,
            offset,
            sort: floki_proto::Sort::NameAsc,
            client_id,
            meta: true,
        })
        .expect("search")
}

/// The `prev` cache is keyed to the index epoch: after a `compact()` renumbers
/// every id, a narrowed follow-up must transparently fall back to a full scan
/// instead of re-evaluating dangling ids (F1). Paged (`offset > 0`) results
/// are never cached as complete, so narrowing a page still searches the whole
/// index (F9).
#[test]
fn prev_cache_survives_compact_epoch() {
    let _env_guard = PIPE_ENV_GUARD.lock().expect("env lock poisoned");
    // See above: the global shutdown latch must be reset per test.
    SHUTDOWN.store(false, Ordering::Relaxed);

    let pipe = unique_pipe();
    std::env::set_var("FLOKI_PIPE", &pipe);

    let index_path = std::env::temp_dir().join(format!("floki-test-{}.bin", std::process::id()));
    let shared = Arc::new(Shared::new(index_path, pipe));
    *shared.index.write().expect("index lock poisoned") = synthetic_index();

    let worker = Arc::clone(&shared);
    let server = std::thread::Builder::new()
        .name("flokid-test-server".to_owned())
        .spawn(move || floki_service::server::serve(&worker))
        .expect("spawn server thread");

    let mut client = connect_retry();

    // Prime the `prev` cache for client 11: "report" -> [id 2].
    match search(&mut client, "report", 100, 0, 11) {
        Response::Results { total, hits, .. } => {
            assert_eq!(total, 1);
            assert_eq!(hits.len(), 1);
        }
        other => panic!("expected Results, got {other:?}"),
    }

    // Tombstone the root (id 0) and compact: the epoch bumps and every id is
    // renumbered, so the cached id 2 now points at `photo.jpg`. A narrowed
    // follow-up must still find `report.txt`.
    {
        let mut index = shared.index.write().expect("index lock poisoned");
        index.entries[0].flags |= floki_core::TOMBSTONE;
        index.compact();
    }
    match search(&mut client, "report.", 100, 0, 11) {
        Response::Results { total, hits, .. } => {
            assert_eq!(total, 1);
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].name, "report.txt");
            assert_eq!(hits[0].path, r"C:\docs");
        }
        other => panic!("expected Results, got {other:?}"),
    }

    // A paged result (`offset > 0`) is an empty page here; narrowing it must
    // search the whole index rather than the cached page.
    match search(&mut client, "rep", 100, 1, 12) {
        Response::Results { total, hits, .. } => {
            assert_eq!(total, 1);
            assert!(hits.is_empty());
        }
        other => panic!("expected Results, got {other:?}"),
    }
    match search(&mut client, "repo", 100, 0, 12) {
        Response::Results { total, hits, .. } => {
            assert_eq!(total, 1);
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].name, "report.txt");
        }
        other => panic!("expected Results, got {other:?}"),
    }

    drop(client);
    request_shutdown(&shared);
    server.join().expect("server thread").expect("serve ok");
    std::env::remove_var("FLOKI_PIPE");
}

/// Accept-latency guard: 20 sequential fresh-connect + `Status` round-trips
/// must complete in under 200 ms total in debug. With the old 50 ms accept
/// poll each new connection waited ~25 ms on average (~500 ms total here).
#[test]
fn status_round_trips_have_no_accept_latency() {
    let _env_guard = PIPE_ENV_GUARD.lock().expect("env lock poisoned");
    SHUTDOWN.store(false, Ordering::Relaxed);

    let pipe = unique_pipe();
    std::env::set_var("FLOKI_PIPE", &pipe);

    let index_path = std::env::temp_dir().join(format!("floki-test-{}.bin", std::process::id()));
    let shared = Arc::new(Shared::new(index_path, pipe));
    *shared.index.write().expect("index lock poisoned") = synthetic_index();

    let worker = Arc::clone(&shared);
    let server = std::thread::Builder::new()
        .name("flokid-test-server".to_owned())
        .spawn(move || floki_service::server::serve(&worker))
        .expect("spawn server thread");

    // Warm up: one round-trip to absorb listener setup.
    let mut warm = connect_retry();
    assert!(matches!(
        warm.call(&Request::Status {}).expect("warmup status"),
        Response::Status { .. }
    ));
    drop(warm);

    let started = Instant::now();
    for _ in 0..20 {
        let mut client = Client::connect().expect("connect");
        match client.call(&Request::Status {}).expect("status") {
            Response::Status { entries, .. } => assert_eq!(entries, 4),
            other => panic!("expected Status, got {other:?}"),
        }
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(200),
        "20 connect+Status round-trips took {elapsed:?}; the accept poll is back"
    );

    request_shutdown(&shared);
    server.join().expect("server thread").expect("serve ok");
    std::env::remove_var("FLOKI_PIPE");
}

/// `VolumesRemove` must stick: the removed letter is excluded from the
/// arrival poll's auto-include (a removed fixed drive used to come straight
/// back within a minute), a policy change keeps the exclusion, and an
/// explicit `Rescan` of the letter (the "add back" path) lifts it.
#[test]
fn removed_volume_stays_excluded_until_rescanned() {
    let _env_guard = PIPE_ENV_GUARD.lock().expect("env lock poisoned");
    SHUTDOWN.store(false, Ordering::Relaxed);

    let pipe = unique_pipe();
    std::env::set_var("FLOKI_PIPE", &pipe);

    let index_path =
        std::env::temp_dir().join(format!("floki-test-excl-{}.bin", std::process::id()));
    let shared = Arc::new(Shared::new(index_path.clone(), pipe));
    *shared.index.write().expect("index lock poisoned") = synthetic_index();

    let worker = Arc::clone(&shared);
    let server = std::thread::Builder::new()
        .name("flokid-test-server".to_owned())
        .spawn(move || floki_service::server::serve(&worker))
        .expect("spawn server thread");
    let mut client = connect_retry();
    let excluded = |shared: &Shared| {
        let live = shared
            .targets
            .read()
            .expect("targets lock poisoned")
            .is_excluded('C');
        let persisted = shared
            .index
            .read()
            .expect("index lock poisoned")
            .targets
            .is_excluded('C');
        assert_eq!(live, persisted, "both policy copies agree");
        live
    };

    assert!(matches!(
        client
            .call(&Request::VolumesRemove { volume: 'c' })
            .expect("remove"),
        Response::Ok {}
    ));
    assert!(excluded(&shared), "removed letter must be excluded");
    assert!(shared
        .index
        .read()
        .expect("index lock poisoned")
        .volumes
        .is_empty());

    assert!(matches!(
        client
            .call(&Request::TargetsConfigSet {
                auto_include_fixed: true,
                auto_include_removable: true,
                auto_remove_offline: true,
            })
            .expect("targets set"),
        Response::TargetsConfig { .. }
    ));
    assert!(excluded(&shared), "a policy change keeps the exclusion");

    assert!(matches!(
        client
            .call(&Request::Rescan { volume: Some('c') })
            .expect("rescan"),
        Response::Ok {}
    ));
    assert!(
        !excluded(&shared),
        "an explicit rescan adds the letter back"
    );

    request_shutdown(&shared);
    server.join().expect("server thread").expect("serve ok");
    std::env::remove_var("FLOKI_PIPE");
    let _ = std::fs::remove_file(&index_path);
}
