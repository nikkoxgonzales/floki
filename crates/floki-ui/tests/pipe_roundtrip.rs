//! End-to-end protocol test: a canned in-process pipe server answers
//! `Hello`/`Status`/`Search`; the real `floki-proto::Client` call path must
//! deliver status plus filtered, paged search results.
//!
//! The `Worker` channel/sequence layer on top is covered by the in-crate
//! `worker_reports_down_when_pipe_missing` test and by live runs against
//! `examples/fake_server.rs`.

#![cfg(windows)]

use std::io::{BufReader, BufWriter};
use std::sync::mpsc;
use std::time::Duration;

use floki_proto::{
    pipe_name, read_frame, write_frame, HitRow, IndexState, Request, Response, VolumeStatus,
};
use interprocess::local_socket::{prelude::*, GenericFilePath, ListenerOptions};
use interprocess::TryClone;

// The worker lives in the binary crate, so this test drives the exact same
// `floki-proto::Client` call path the worker uses, against canned data.

fn rows() -> Vec<HitRow> {
    (0..50u32)
        .map(|i| HitRow {
            name: format!("file_{i:03}.txt"),
            path: format!(r"C:\data\project_{:02}", i % 5),
            is_dir: false,
            size: Some(u64::from(i) * 100),
            modified_ms: Some(1_700_000_000_000 + i64::from(i) * 1000),
            created_ms: None,
        })
        .collect()
}

fn serve(listener: interprocess::local_socket::Listener, ready: mpsc::Sender<()>) {
    let _ = ready.send(());
    let rows = rows();
    let Some(conn) = listener.incoming().next() else {
        return;
    };
    let Ok(stream) = conn else { return };
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = BufWriter::new(stream);
    loop {
        let req: Option<Request> = read_frame(&mut reader).unwrap();
        let Some(req) = req else { return };
        let resp = match req {
            Request::Status {} => Response::Status {
                entries: rows.len() as u64,
                volumes: vec![VolumeStatus {
                    letter: 'C',
                    entries: rows.len() as u64,
                    next_usn: 7,
                    live: true,
                    enabled: true,
                    monitor: true,
                }],
                rss_bytes: 10 * 1024 * 1024,
                uptime_s: 3,
                state: IndexState::Ready,
            },
            Request::Search {
                query,
                max_results,
                offset,
                ..
            } => {
                let needle = query.to_lowercase();
                let all: Vec<HitRow> = rows
                    .iter()
                    .filter(|h| h.name.to_lowercase().contains(&needle))
                    .cloned()
                    .collect();
                let total = all.len() as u64;
                let hits = all
                    .into_iter()
                    .skip(offset as usize)
                    .take(max_results as usize)
                    .collect();
                Response::Results {
                    total,
                    hits,
                    elapsed_us: 11,
                }
            }
            Request::Hello {} => Response::Hello {
                protocol: floki_proto::PROTOCOL_VERSION,
                service_version: "test".to_owned(),
            },
            _ => Response::Ok {},
        };
        write_frame(&mut writer, &resp).unwrap();
    }
}

#[test]
fn canned_server_answers_status_and_filtered_search() {
    let saved = std::env::var("FLOKI_PIPE").ok();
    std::env::set_var("FLOKI_PIPE", r"\\.\pipe\floki-ui-integ-test-1");
    assert_eq!(
        pipe_name(),
        r"\\.\pipe\floki-ui-integ-test-1",
        "worker and server must use the same pipe"
    );

    let listener = ListenerOptions::new()
        .name(
            pipe_name()
                .as_str()
                .to_fs_name::<GenericFilePath>()
                .unwrap(),
        )
        .create_sync()
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let server = std::thread::spawn(move || serve(listener, tx));
    rx.recv_timeout(Duration::from_secs(10)).unwrap();

    let mut client = floki_proto::Client::connect().expect("connect to canned server");
    let status = client.call(&Request::Status {}).expect("status call");
    match status {
        Response::Status {
            entries,
            state,
            rss_bytes,
            ..
        } => {
            assert_eq!(entries, 50);
            assert_eq!(state, IndexState::Ready);
            assert_eq!(rss_bytes, 10 * 1024 * 1024);
        }
        other => panic!("expected status, got {other:?}"),
    }

    // Typing "file_00" must narrow 50 rows down to the matching subset.
    let res = client
        .call(&Request::Search {
            query: "file_00".to_owned(),
            max_results: 1000,
            offset: 0,
            sort: floki_proto::Sort::NameAsc,
            client_id: 9,
            meta: true,
        })
        .expect("search call");
    match res {
        Response::Results { total, hits, .. } => {
            assert_eq!(total, 10, "file_000..file_009 match file_00");
            assert_eq!(hits.len(), 10);
            assert!(hits.iter().all(|h| h.name.contains("file_00")));
        }
        other => panic!("expected results, got {other:?}"),
    }

    // max_results pages the hits while total stays complete.
    let res = client
        .call(&Request::Search {
            query: "file_".to_owned(),
            max_results: 7,
            offset: 0,
            sort: floki_proto::Sort::NameAsc,
            client_id: 9,
            meta: true,
        })
        .expect("search call");
    match res {
        Response::Results { total, hits, .. } => {
            assert_eq!(total, 50);
            assert_eq!(hits.len(), 7);
        }
        other => panic!("expected results, got {other:?}"),
    }

    drop(client);
    // The server thread blocks on `accept` after the client disconnects; it is
    // intentionally detached — the test process reaps it on exit.
    std::mem::forget(server);

    match saved {
        Some(v) => std::env::set_var("FLOKI_PIPE", v),
        None => std::env::remove_var("FLOKI_PIPE"),
    }
}
