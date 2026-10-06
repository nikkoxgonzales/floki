//! Fake `flokid` for UI development: listens on [`floki_proto::pipe_name`]
//! (override with env `FLOKI_PIPE`) and answers `Hello`/`Status`/`Search`
//! with canned data.
//!
//! Run: `FLOKI_PIPE=\\.\pipe\floki-test cargo run -p floki-ui --example fake_server`
//! then in another shell with the same `FLOKI_PIPE`: `cargo run -p floki-ui`.
//! `FAKE_SCANNING=G` reports a scan of G: in progress (items read grow with
//! uptime) so the indexing notice and the Drives tab's scanning state can be
//! checked. `FAKE_ROWS=5000` grows the catalog (default 320 files) so
//! paging can be checked.

use std::io::{BufReader, BufWriter};
use std::time::Instant;

use floki_proto::{
    pipe_name, read_frame, write_frame, HitRow, IndexState, Request, Response, VolumeStatus,
};
use interprocess::local_socket::{prelude::*, GenericFilePath, ListenerOptions};
use interprocess::TryClone;

fn catalog() -> Vec<HitRow> {
    let mut v = Vec::new();
    let exts = ["txt", "rs", "md", "png", "exe", "dll", "log", "toml"];
    // Deterministic fake metadata: sizes/times vary per row so every sort
    // order is visibly different; a few rows carry `None` to exercise the
    // "--" placeholders.
    let base_ms: i64 = 1_700_000_000_000;
    let files: usize = std::env::var("FAKE_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(320);
    for i in 0..files {
        let ext = exts[i % exts.len()];
        let dir_no = i % 24;
        v.push(HitRow {
            name: format!("file_{i:05}.{ext}"),
            path: format!(r"C:\data\project_{dir_no:02}"),
            is_dir: false,
            size: Some(512 + (i as u64) * 137 % 4_000_000),
            modified_ms: Some(base_ms + (i as i64) * 3_600_000),
            created_ms: Some(base_ms - (i as i64) * 7_200_000),
        });
    }
    for i in 0..24 {
        v.push(HitRow {
            name: format!("project_{i:02}"),
            path: r"C:\data".to_owned(),
            is_dir: true,
            size: None,
            modified_ms: Some(base_ms + (i as i64) * 86_400_000),
            created_ms: Some(base_ms - (i as i64) * 43_200_000),
        });
    }
    v.push(HitRow {
        name: "notes about floki.txt".to_owned(),
        path: r"C:\docs".to_owned(),
        is_dir: false,
        size: Some(1_234),
        modified_ms: Some(base_ms),
        created_ms: None,
    });
    v.push(HitRow {
        name: "FlokiDesign.md".to_owned(),
        path: r"C:\docs".to_owned(),
        is_dir: false,
        size: Some(98_765),
        modified_ms: None,
        created_ms: Some(base_ms),
    });
    v
}

/// Apply the requested sort to the match list (name/path fold to lowercase;
/// `None` timestamps sort last in both directions).
fn sort_rows(rows: &mut [HitRow], sort: floki_proto::Sort) {
    use floki_proto::Sort::*;
    let desc = matches!(sort, NameDesc | PathDesc | ModifiedDesc | CreatedDesc);
    rows.sort_by(|a, b| {
        let ord = match sort {
            NameAsc | NameDesc => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            PathAsc | PathDesc => a.path.to_lowercase().cmp(&b.path.to_lowercase()),
            ModifiedAsc | ModifiedDesc => a.modified_ms.cmp(&b.modified_ms),
            CreatedAsc | CreatedDesc => a.created_ms.cmp(&b.created_ms),
        };
        if desc {
            ord.reverse()
        } else {
            ord
        }
    });
}

fn handle(req: Request, rows: &[HitRow], started: Instant) -> Response {
    match req {
        Request::Hello {} => Response::Hello {
            protocol: floki_proto::PROTOCOL_VERSION,
            service_version: "fake-0.1.0".to_owned(),
        },
        Request::Status {} => {
            let mut volumes = vec![VolumeStatus {
                letter: 'C',
                entries: rows.len() as u64,
                next_usn: 123_456,
                live: true,
                enabled: true,
                monitor: true,
            }];
            let scanning = std::env::var("FAKE_SCANNING")
                .ok()
                .and_then(|v| v.chars().next())
                .map(|c| c.to_ascii_uppercase());
            let state = match scanning {
                Some(letter) => {
                    let done = started.elapsed().as_secs() * 11_000;
                    volumes.push(VolumeStatus {
                        letter,
                        entries: done,
                        next_usn: 0,
                        live: true,
                        enabled: true,
                        monitor: true,
                    });
                    IndexState::Scanning {
                        volume: letter,
                        done,
                    }
                }
                None => IndexState::Ready,
            };
            Response::Status {
                entries: volumes.iter().map(|v| v.entries).sum(),
                volumes,
                rss_bytes: 41 * 1024 * 1024,
                uptime_s: started.elapsed().as_secs(),
                state,
            }
        }
        Request::Search {
            query,
            max_results,
            offset,
            sort,
            ..
        } => {
            let t0 = Instant::now();
            let needle = query.to_lowercase();
            let matches: Vec<HitRow> = rows
                .iter()
                .filter(|h| {
                    needle.is_empty()
                        || h.name.to_lowercase().contains(&needle)
                        || h.path.to_lowercase().contains(&needle)
                })
                .cloned()
                .collect();
            let mut matches = matches;
            sort_rows(&mut matches, sort);
            let total = matches.len() as u64;
            let page = matches
                .into_iter()
                .skip(offset as usize)
                .take(max_results.min(1000) as usize)
                .collect();
            Response::Results {
                total,
                hits: page,
                elapsed_us: t0.elapsed().as_micros() as u64,
            }
        }
        Request::Rescan { .. } | Request::Shutdown {} => Response::Ok {},
        Request::VolumesSetEnabled { .. }
        | Request::VolumesSetMonitor { .. }
        | Request::VolumesRemove { .. } => Response::Ok {},
        Request::TargetsConfigGet {} => Response::TargetsConfig {
            auto_include_fixed: true,
            auto_include_removable: false,
            auto_remove_offline: true,
        },
        Request::TargetsConfigSet {
            auto_include_fixed,
            auto_include_removable,
            auto_remove_offline,
        } => Response::TargetsConfig {
            auto_include_fixed,
            auto_include_removable,
            auto_remove_offline,
        },
    }
}

fn serve_one(
    stream: interprocess::local_socket::Stream,
    rows: &[HitRow],
    started: Instant,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    loop {
        let req: Option<Request> = read_frame(&mut reader)?;
        let Some(req) = req else { return Ok(()) };
        let resp = handle(req, rows, started);
        write_frame(&mut writer, &resp)?;
    }
}

fn main() -> anyhow::Result<()> {
    let name = pipe_name();
    println!("fake flokid listening on {name}");
    let listener = ListenerOptions::new()
        .name(name.as_str().to_fs_name::<GenericFilePath>()?)
        .create_sync()?;
    let rows = catalog();
    let started = Instant::now();
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let rows = rows.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve_one(stream, &rows, started) {
                        eprintln!("client error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
    Ok(())
}
