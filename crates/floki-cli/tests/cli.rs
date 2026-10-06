//! End-to-end CLI tests against a tiny fake `flokid`.
//!
//! The fake server speaks the real `floki-proto` framing on a unique pipe
//! name; the built `flk` binary is driven with `FLOKI_PIPE` pointed at it.

use floki_proto::{HitRow, IndexState, Request, Response, VolumeStatus};
use interprocess::local_socket::{prelude::*, GenericFilePath, ListenerOptions};
use std::io::BufReader;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

const NO_MATCH_QUERY: &str = "zzz-no-match-xyz";
const HIT_NAME: &str = "foo.txt";
const HIT_DIR: &str = r"C:\docs";

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn unique_pipe(tag: &str) -> String {
    let n = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!(
        r"\\.\pipe\floki-test-{tag}-{}-{now}-{n}",
        std::process::id()
    )
}

fn canned_response(request: &Request) -> Response {
    match request {
        Request::Hello {} => Response::Hello {
            protocol: floki_proto::PROTOCOL_VERSION,
            service_version: "test".to_owned(),
        },
        Request::Search { query, .. } => {
            if query == NO_MATCH_QUERY {
                Response::Results {
                    total: 0,
                    hits: Vec::new(),
                    elapsed_us: 7,
                }
            } else {
                Response::Results {
                    total: 1,
                    hits: vec![HitRow {
                        name: HIT_NAME.to_owned(),
                        path: HIT_DIR.to_owned(),
                        is_dir: false,
                        size: Some(42),
                        modified_ms: Some(1_700_000_000_000),
                        created_ms: Some(1_699_000_000_000),
                    }],
                    elapsed_us: 7,
                }
            }
        }
        Request::Status {} => Response::Status {
            entries: 42,
            volumes: vec![VolumeStatus {
                letter: 'C',
                entries: 42,
                next_usn: 1,
                live: true,
                enabled: true,
                monitor: true,
            }],
            rss_bytes: 10 * 1024 * 1024,
            uptime_s: 60,
            state: IndexState::Ready,
        },
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
            auto_include_fixed: *auto_include_fixed,
            auto_include_removable: *auto_include_removable,
            auto_remove_offline: *auto_remove_offline,
        },
    }
}

fn handle_stream(stream: interprocess::local_socket::Stream) -> bool {
    let mut reader = BufReader::new(stream);
    loop {
        let request: Option<Request> = match floki_proto::read_frame(&mut reader) {
            Ok(frame) => frame,
            Err(_) => return false,
        };
        let Some(request) = request else {
            return false; // clean EOF
        };
        if matches!(request, Request::Shutdown {}) {
            let _ = floki_proto::write_frame(reader.get_mut(), &Response::Ok {});
            return true; // stop signal
        }
        let response = canned_response(&request);
        if floki_proto::write_frame(reader.get_mut(), &response).is_err() {
            return false;
        }
    }
}

/// Spawn the fake server; returns its thread handle once it is listening.
/// The server exits when a client sends `Shutdown` (see [`stop_server`]).
fn spawn_fake_server(pipe: &str) -> std::thread::JoinHandle<()> {
    let pipe = pipe.to_owned();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let name = pipe
            .as_str()
            .to_fs_name::<GenericFilePath>()
            .expect("pipe name");
        let listener = ListenerOptions::new()
            .name(name)
            .create_sync()
            .expect("listen");
        ready_tx.send(()).expect("ready signal");
        for conn in listener.incoming() {
            let Ok(stream) = conn else { break };
            if handle_stream(stream) {
                break;
            }
        }
    });
    ready_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("fake server is listening");
    handle
}

fn stop_server(pipe: &str, server: std::thread::JoinHandle<()>) {
    let name = pipe.to_fs_name::<GenericFilePath>().expect("pipe name");
    let stream = LocalSocketStream::connect(name).expect("stop connect");
    let mut rw = BufReader::new(stream);
    floki_proto::write_frame(rw.get_mut(), &Request::Shutdown {}).expect("stop write");
    let response: Option<Response> = floki_proto::read_frame(&mut rw).expect("stop read");
    assert!(matches!(response, Some(Response::Ok {})));
    server.join().expect("server thread exits cleanly");
}

fn run_flk(pipe: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_flk"))
        .env("FLOKI_PIPE", pipe)
        .args(args)
        .output()
        .expect("run flk binary")
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is utf-8")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is utf-8")
}

#[test]
fn search_hit_prints_full_path_and_exits_zero() {
    let pipe = unique_pipe("hit");
    let server = spawn_fake_server(&pipe);

    // Multiple words must be joined into one query; the fake server answers
    // any query except NO_MATCH_QUERY with the canned hit.
    let output = run_flk(&pipe, &["hello", "world"]);
    stop_server(&pipe, server);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout_text(&output).trim(), r"C:\docs\foo.txt");
}

#[test]
fn search_zero_hits_prints_nothing_and_exits_one() {
    let pipe = unique_pipe("empty");
    let server = spawn_fake_server(&pipe);

    let output = run_flk(&pipe, &[NO_MATCH_QUERY]);
    stop_server(&pipe, server);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout_text(&output).trim(), "");
}

#[test]
fn search_json_prints_raw_response_line() {
    let pipe = unique_pipe("json");
    let server = spawn_fake_server(&pipe);

    let output = run_flk(&pipe, &["--json", "hello"]);
    stop_server(&pipe, server);

    assert_eq!(output.status.code(), Some(0));
    let line = stdout_text(&output);
    let parsed: Response = serde_json::from_str(line.trim()).expect("stdout is a Response");
    match parsed {
        Response::Results { hits, .. } => {
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].name, HIT_NAME);
        }
        other => panic!("expected Results, got {other:?}"),
    }
}

#[test]
fn volumes_lists_flags() {
    let pipe = unique_pipe("volumes");
    let server = spawn_fake_server(&pipe);

    let output = run_flk(&pipe, &["volumes"]);
    stop_server(&pipe, server);

    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    assert!(stdout.contains("vol"), "stdout was: {stdout}");
    assert!(stdout.contains('C'), "stdout was: {stdout}");
    assert!(stdout.contains("enabled"), "stdout was: {stdout}");
    assert!(stdout.contains("monitor"), "stdout was: {stdout}");
}

#[test]
fn volume_enable_disable_remove_succeed() {
    let pipe = unique_pipe("volume");
    let server = spawn_fake_server(&pipe);

    for args in [
        vec!["volume", "C", "--disable"],
        vec!["volume", "c", "--enable"],
        vec!["volume", "C", "--no-monitor"],
        vec!["volume", "C", "--monitor"],
        vec!["volume", "C", "--remove"],
    ] {
        let output = run_flk(&pipe, &args);
        assert_eq!(output.status.code(), Some(0), "args were: {args:?}");
        assert_eq!(stdout_text(&output).trim(), "Ok");
    }
    stop_server(&pipe, server);
}

#[test]
fn volume_flags_reject_bad_letter_and_empty_op() {
    let pipe = unique_pipe("volume-err");
    let server = spawn_fake_server(&pipe);

    let bad = run_flk(&pipe, &["volume", "1", "--disable"]);
    assert_eq!(bad.status.code(), Some(3));
    let empty = run_flk(&pipe, &["volume", "C"]);
    assert_eq!(empty.status.code(), Some(3));
    stop_server(&pipe, server);
}

#[test]
fn config_print_and_set_succeed() {
    let pipe = unique_pipe("config");
    let server = spawn_fake_server(&pipe);

    let output = run_flk(&pipe, &["config"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    assert!(
        stdout.contains("auto_include_fixed"),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains("auto_include_removable"),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains("auto_remove_offline"),
        "stdout was: {stdout}"
    );

    let set = run_flk(&pipe, &["config", "set", "--auto-removable"]);
    stop_server(&pipe, server);
    assert_eq!(set.status.code(), Some(0));
    assert!(stdout_text(&set).contains("auto_include_removable"));
}

#[test]
fn status_and_rescan_succeed() {
    let pipe = unique_pipe("misc");
    let server = spawn_fake_server(&pipe);

    let status = run_flk(&pipe, &["status"]);
    assert_eq!(status.status.code(), Some(0));
    assert!(stdout_text(&status).contains("entries: 42"));

    let rescan = run_flk(&pipe, &["rescan", "C"]);
    stop_server(&pipe, server);
    assert_eq!(rescan.status.code(), Some(0));
    assert_eq!(stdout_text(&rescan).trim(), "Ok");
}

#[test]
fn server_down_exits_two_with_hint() {
    // No listener on this pipe name.
    let pipe = unique_pipe("down");

    let output = run_flk(&pipe, &["hello"]);

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("flokid is not running"),
        "stderr was: {stderr}"
    );
    assert!(stderr.contains(&pipe), "stderr was: {stderr}");
}

#[test]
fn shutdown_prints_stopping_and_exits_zero() {
    let pipe = unique_pipe("shutdown");
    let server = spawn_fake_server(&pipe);

    let output = run_flk(&pipe, &["shutdown"]);
    // The fake server stops on `Shutdown`; join it directly.
    server.join().expect("server thread exits cleanly");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout_text(&output).trim(), "flokid stopping");
}

#[test]
fn shutdown_down_exits_two_with_hint() {
    // No listener on this pipe name.
    let pipe = unique_pipe("shutdown-down");

    let output = run_flk(&pipe, &["shutdown"]);

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("flokid is not running"),
        "stderr was: {stderr}"
    );
}
