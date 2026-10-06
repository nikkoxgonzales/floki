//! Background pipe worker: the UI thread never touches the named pipe.
//!
//! One worker thread owns the [`Client`]. The egui thread sends [`ToWorker`]
//! commands through an `mpsc` channel and receives [`FromWorker`] events on
//! another. Every search carries a sequence number so the UI can drop stale
//! responses; the worker itself is stateless apart from the connection.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use floki_proto::{HitRow, IndexState, Request, Response, Sort};

/// Rows per name/path-ordered `Search` page; the UI asks for the next page
/// as the list scrolls near its end.
pub const PAGE_ROWS: u32 = 1000;

/// Rows for a date-ordered search, fetched in one request: the service reads
/// every match's timestamps per request, so paging would repeat that cost.
pub const TIME_SORT_ROWS: u32 = 10_000;

/// Most rows the UI loads by paging. Past the service's deep-page bound a
/// name-ordered page costs a full match pass, so the list stops here.
pub const MAX_LOADED_ROWS: usize = 100_000;

/// Commands from the UI thread to the worker.
#[derive(Debug)]
pub enum ToWorker {
    Search {
        seq: u64,
        query: String,
        sort: Sort,
        client_id: u64,
        /// First row wanted (0 for a new search, > 0 for a further page).
        offset: u32,
        max_results: u32,
    },
    Status {
        seq: u64,
    },
    /// Ask the indexer to rescan one indexed drive (or all drives for None).
    Rescan {
        volume: Option<char>,
    },
    /// Toggle search visibility for a volume (index entry kept).
    SetVolumeEnabled {
        volume: char,
        enabled: bool,
    },
    /// Toggle journal-tail monitoring for a volume.
    SetVolumeMonitor {
        volume: char,
        monitor: bool,
    },
    /// Drop a volume record and all its entries from the index.
    RemoveVolume {
        volume: char,
    },
    /// Read the global NTFS-targets policy.
    TargetsConfigGet,
    /// Replace the global NTFS-targets policy.
    TargetsConfigSet {
        auto_include_fixed: bool,
        auto_include_removable: bool,
        auto_remove_offline: bool,
    },
    /// Ask a UI-owned indexer to save and stop (graceful `Shutdown`).
    Shutdown,
    Stop,
}

/// Search results delivered back to the UI thread.
#[derive(Debug)]
pub struct SearchOutcome {
    /// The request's `offset`: 0 replaces the list, > 0 appends a page.
    pub offset: u32,
    pub total: u64,
    pub hits: Vec<HitRow>,
    /// Service-side query time (`Response::Results.elapsed_us`).
    pub elapsed_us: u64,
}

/// Status snapshot delivered back to the UI thread.
#[derive(Debug, Clone)]
pub struct StatusOutcome {
    pub entries: u64,
    pub volumes: Vec<floki_proto::VolumeStatus>,
    pub rss_bytes: u64,
    /// Service uptime; kept for protocol completeness (not shown in v1 status bar).
    #[allow(dead_code)]
    pub uptime_s: u64,
    pub state: IndexState,
}

/// Events from the worker to the UI thread.
#[derive(Debug)]
pub enum FromWorker {
    SearchDone {
        seq: u64,
        result: Result<SearchOutcome, String>,
    },
    StatusDone {
        seq: u64,
        result: Result<StatusOutcome, String>,
    },
    RescanDone(Result<(), String>),
    /// Result of a volume op (`SetVolumeEnabled`/`SetVolumeMonitor`/`RemoveVolume`).
    VolumeOpDone(Result<(), String>),
    /// Global NTFS-targets policy snapshot.
    TargetsConfigDone(Result<floki_proto::TargetsConfig, String>),
    /// Result of a [`ToWorker::Shutdown`] request.
    ShutdownDone(Result<(), String>),
    /// Handshake result after each (re)connect: service protocol + version.
    HelloDone {
        protocol: u32,
        service_version: String,
    },
    /// Pipe connected (initial connect or reconnect).
    ServiceUp,
    /// Pipe missing or a call failed; the worker already dropped the client
    /// and will retry on the next command.
    ServiceDown,
}

/// Deterministic 64-bit client id for one (user, machine, install) triple.
///
/// FNV-1a over the current-exe path plus the user name. Stable across
/// restarts (so the service's per-`client_id` `prev` narrowing in SPEC §4
/// keeps working after relaunch); only the hash ever leaves the process,
/// never the raw path or name.
#[must_use]
pub fn stable_client_id(exe_path: &str, user: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    let mut h = OFFSET;
    for b in "floki-client-v1"
        .bytes()
        .chain(std::iter::once(0))
        .chain(exe_path.bytes())
        .chain(std::iter::once(0))
        .chain(user.bytes())
    {
        h ^= u64::from(b);
        h = h.wrapping_mul(PRIME);
    }
    if h == 0 {
        1
    } else {
        h
    }
}

/// Generate the client id for this UI process. Same value on every launch by
/// the same user from the same install; the service uses it to reuse the
/// previous result set for query extensions.
#[must_use]
pub fn make_client_id() -> u64 {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "floki-unknown-exe".to_owned());
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown-user".to_owned());
    stable_client_id(&exe, &user)
}

pub struct Worker {
    pub tx: Sender<ToWorker>,
    pub rx: Receiver<FromWorker>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    #[must_use]
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<ToWorker>();
        let (evt_tx, evt_rx) = mpsc::channel::<FromWorker>();
        let handle = thread::Builder::new()
            .name("floki-pipe".to_owned())
            .spawn(move || worker_main(cmd_rx, evt_tx))
            .expect("pipe worker thread must spawn");
        Self {
            tx: cmd_tx,
            rx: evt_rx,
            handle: Some(handle),
        }
    }
}

/// Next command for the worker, skipping a `Search` or `Status` that a newer
/// queued one of the same kind supersedes (the UI drops stale sequence
/// numbers anyway). While the indexer is slow, keystrokes and the periodic
/// status poll pile up in the channel; answering every stale request in
/// turn left the window lagging far behind the user. Every other command
/// runs, in order. `None` once the UI side hung up.
fn next_command(rx: &Receiver<ToWorker>, backlog: &mut VecDeque<ToWorker>) -> Option<ToWorker> {
    loop {
        let cmd = match backlog.pop_front() {
            Some(cmd) => cmd,
            None => rx.recv().ok()?,
        };
        backlog.extend(rx.try_iter());
        let superseded = match cmd {
            ToWorker::Search { .. } => backlog.iter().any(|c| matches!(c, ToWorker::Search { .. })),
            ToWorker::Status { .. } => backlog.iter().any(|c| matches!(c, ToWorker::Status { .. })),
            _ => false,
        };
        if !superseded {
            return Some(cmd);
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Never join: the worker may be blocked in a synchronous pipe
        // `call()` with no timeout, so joining here would hang Quit when the
        // service is unresponsive. Ask it to stop, detach the thread, and let
        // process exit reap it.
        let _ = self.tx.send(ToWorker::Stop);
        if let Some(h) = self.handle.take() {
            std::mem::forget(h);
        }
    }
}

#[cfg(windows)]
fn worker_main(cmds: Receiver<ToWorker>, events: Sender<FromWorker>) {
    use floki_proto::Client;

    let mut client: Option<Client> = None;
    let mut up = false;

    let set_down = |up: &mut bool, events: &Sender<FromWorker>| {
        if *up {
            *up = false;
            let _ = events.send(FromWorker::ServiceDown);
        }
    };

    let mut backlog = VecDeque::new();
    while let Some(cmd) = next_command(&cmds, &mut backlog) {
        match cmd {
            ToWorker::Stop => break,
            ToWorker::Search {
                seq,
                query,
                sort,
                client_id,
                offset,
                max_results,
            } => {
                // No row metadata: the UI reads it for visible rows only
                // (`crate::meta`), so a page never waits on disk reads.
                let req = Request::Search {
                    query,
                    max_results,
                    offset,
                    sort,
                    client_id,
                    meta: false,
                };
                match call(&mut client, &events, &req) {
                    Ok(Response::Results {
                        total,
                        hits,
                        elapsed_us,
                    }) => {
                        if !up {
                            up = true;
                            let _ = events.send(FromWorker::ServiceUp);
                        }
                        let _ = events.send(FromWorker::SearchDone {
                            seq,
                            result: Ok(SearchOutcome {
                                offset,
                                total,
                                hits,
                                elapsed_us,
                            }),
                        });
                    }
                    Ok(Response::Error { message }) => {
                        let _ = events.send(FromWorker::SearchDone {
                            seq,
                            result: Err(message),
                        });
                    }
                    Ok(other) => {
                        let _ = events.send(FromWorker::SearchDone {
                            seq,
                            result: Err(format!(
                                "unexpected response to search: {}",
                                response_kind(&other)
                            )),
                        });
                    }
                    Err(e) => {
                        client = None;
                        set_down(&mut up, &events);
                        let _ = events.send(FromWorker::SearchDone {
                            seq,
                            result: Err(e),
                        });
                    }
                }
            }
            ToWorker::Rescan { volume } => {
                match call(&mut client, &events, &Request::Rescan { volume }) {
                    Ok(Response::Ok {}) => {
                        if !up {
                            up = true;
                            let _ = events.send(FromWorker::ServiceUp);
                        }
                        let _ = events.send(FromWorker::RescanDone(Ok(())));
                    }
                    Ok(Response::Error { message }) => {
                        let _ = events.send(FromWorker::RescanDone(Err(message)));
                    }
                    Ok(other) => {
                        let _ = events.send(FromWorker::RescanDone(Err(format!(
                            "unexpected response to rescan: {}",
                            response_kind(&other)
                        ))));
                    }
                    Err(e) => {
                        client = None;
                        set_down(&mut up, &events);
                        let _ = events.send(FromWorker::RescanDone(Err(e)));
                    }
                }
            }
            ToWorker::SetVolumeEnabled { volume, enabled } => {
                match call(
                    &mut client,
                    &events,
                    &Request::VolumesSetEnabled { volume, enabled },
                ) {
                    Ok(Response::Ok {}) => {
                        if !up {
                            up = true;
                            let _ = events.send(FromWorker::ServiceUp);
                        }
                        let _ = events.send(FromWorker::VolumeOpDone(Ok(())));
                    }
                    Ok(Response::Error { message }) => {
                        let _ = events.send(FromWorker::VolumeOpDone(Err(message)));
                    }
                    Ok(other) => {
                        let _ = events.send(FromWorker::VolumeOpDone(Err(format!(
                            "unexpected response to volumes_set_enabled: {}",
                            response_kind(&other)
                        ))));
                    }
                    Err(e) => {
                        client = None;
                        set_down(&mut up, &events);
                        let _ = events.send(FromWorker::VolumeOpDone(Err(e)));
                    }
                }
            }
            ToWorker::SetVolumeMonitor { volume, monitor } => {
                match call(
                    &mut client,
                    &events,
                    &Request::VolumesSetMonitor { volume, monitor },
                ) {
                    Ok(Response::Ok {}) => {
                        if !up {
                            up = true;
                            let _ = events.send(FromWorker::ServiceUp);
                        }
                        let _ = events.send(FromWorker::VolumeOpDone(Ok(())));
                    }
                    Ok(Response::Error { message }) => {
                        let _ = events.send(FromWorker::VolumeOpDone(Err(message)));
                    }
                    Ok(other) => {
                        let _ = events.send(FromWorker::VolumeOpDone(Err(format!(
                            "unexpected response to volumes_set_monitor: {}",
                            response_kind(&other)
                        ))));
                    }
                    Err(e) => {
                        client = None;
                        set_down(&mut up, &events);
                        let _ = events.send(FromWorker::VolumeOpDone(Err(e)));
                    }
                }
            }
            ToWorker::RemoveVolume { volume } => {
                match call(&mut client, &events, &Request::VolumesRemove { volume }) {
                    Ok(Response::Ok {}) => {
                        if !up {
                            up = true;
                            let _ = events.send(FromWorker::ServiceUp);
                        }
                        let _ = events.send(FromWorker::VolumeOpDone(Ok(())));
                    }
                    Ok(Response::Error { message }) => {
                        let _ = events.send(FromWorker::VolumeOpDone(Err(message)));
                    }
                    Ok(other) => {
                        let _ = events.send(FromWorker::VolumeOpDone(Err(format!(
                            "unexpected response to volumes_remove: {}",
                            response_kind(&other)
                        ))));
                    }
                    Err(e) => {
                        client = None;
                        set_down(&mut up, &events);
                        let _ = events.send(FromWorker::VolumeOpDone(Err(e)));
                    }
                }
            }
            ToWorker::TargetsConfigGet => {
                match call(&mut client, &events, &Request::TargetsConfigGet {}) {
                    Ok(Response::TargetsConfig {
                        auto_include_fixed,
                        auto_include_removable,
                        auto_remove_offline,
                    }) => {
                        if !up {
                            up = true;
                            let _ = events.send(FromWorker::ServiceUp);
                        }
                        let _ = events.send(FromWorker::TargetsConfigDone(Ok(
                            floki_proto::TargetsConfig {
                                auto_include_fixed,
                                auto_include_removable,
                                auto_remove_offline,
                            },
                        )));
                    }
                    Ok(Response::Error { message }) => {
                        let _ = events.send(FromWorker::TargetsConfigDone(Err(message)));
                    }
                    Ok(other) => {
                        let _ = events.send(FromWorker::TargetsConfigDone(Err(format!(
                            "unexpected response to targets_config_get: {}",
                            response_kind(&other)
                        ))));
                    }
                    Err(e) => {
                        client = None;
                        set_down(&mut up, &events);
                        let _ = events.send(FromWorker::TargetsConfigDone(Err(e)));
                    }
                }
            }
            ToWorker::TargetsConfigSet {
                auto_include_fixed,
                auto_include_removable,
                auto_remove_offline,
            } => {
                match call(
                    &mut client,
                    &events,
                    &Request::TargetsConfigSet {
                        auto_include_fixed,
                        auto_include_removable,
                        auto_remove_offline,
                    },
                ) {
                    Ok(Response::TargetsConfig {
                        auto_include_fixed,
                        auto_include_removable,
                        auto_remove_offline,
                    }) => {
                        if !up {
                            up = true;
                            let _ = events.send(FromWorker::ServiceUp);
                        }
                        let _ = events.send(FromWorker::TargetsConfigDone(Ok(
                            floki_proto::TargetsConfig {
                                auto_include_fixed,
                                auto_include_removable,
                                auto_remove_offline,
                            },
                        )));
                    }
                    Ok(Response::Error { message }) => {
                        let _ = events.send(FromWorker::TargetsConfigDone(Err(message)));
                    }
                    Ok(other) => {
                        let _ = events.send(FromWorker::TargetsConfigDone(Err(format!(
                            "unexpected response to targets_config_set: {}",
                            response_kind(&other)
                        ))));
                    }
                    Err(e) => {
                        client = None;
                        set_down(&mut up, &events);
                        let _ = events.send(FromWorker::TargetsConfigDone(Err(e)));
                    }
                }
            }
            ToWorker::Status { seq } => match call(&mut client, &events, &Request::Status {}) {
                Ok(Response::Status {
                    entries,
                    volumes,
                    rss_bytes,
                    uptime_s,
                    state,
                    ..
                }) => {
                    if !up {
                        up = true;
                        let _ = events.send(FromWorker::ServiceUp);
                    }
                    let _ = events.send(FromWorker::StatusDone {
                        seq,
                        result: Ok(StatusOutcome {
                            entries,
                            volumes,
                            rss_bytes,
                            uptime_s,
                            state,
                        }),
                    });
                }
                Ok(Response::Error { message }) => {
                    let _ = events.send(FromWorker::StatusDone {
                        seq,
                        result: Err(message),
                    });
                }
                Ok(other) => {
                    let _ = events.send(FromWorker::StatusDone {
                        seq,
                        result: Err(format!(
                            "unexpected response to status: {}",
                            response_kind(&other)
                        )),
                    });
                }
                Err(e) => {
                    client = None;
                    set_down(&mut up, &events);
                    let _ = events.send(FromWorker::StatusDone {
                        seq,
                        result: Err(e),
                    });
                }
            },
            ToWorker::Shutdown => match call(&mut client, &events, &Request::Shutdown {}) {
                Ok(Response::Ok {}) => {
                    client = None;
                    set_down(&mut up, &events);
                    let _ = events.send(FromWorker::ShutdownDone(Ok(())));
                }
                Ok(Response::Error { message }) => {
                    let _ = events.send(FromWorker::ShutdownDone(Err(message)));
                }
                Ok(other) => {
                    let _ = events.send(FromWorker::ShutdownDone(Err(format!(
                        "unexpected response to shutdown: {}",
                        response_kind(&other)
                    ))));
                }
                Err(e) => {
                    client = None;
                    set_down(&mut up, &events);
                    let _ = events.send(FromWorker::ShutdownDone(Err(e)));
                }
            },
        }
    }
}

/// Ensure a live connection (with a `Hello` handshake on every fresh
/// connect), then perform one request/response round trip. On success the
/// client stays cached in `slot`; on failure the slot is left empty and a
/// human-readable error is returned.
#[cfg(windows)]
fn call(
    slot: &mut Option<floki_proto::Client>,
    events: &Sender<FromWorker>,
    req: &Request,
) -> Result<Response, String> {
    if slot.is_none() {
        match floki_proto::Client::connect() {
            Ok(mut c) => {
                match c.call(&Request::Hello {}) {
                    Ok(Response::Hello {
                        protocol,
                        service_version,
                    }) if protocol == floki_proto::PROTOCOL_VERSION => {
                        tracing::info!(
                            "flokid hello: protocol {protocol} service {service_version}"
                        );
                        let _ = events.send(FromWorker::HelloDone {
                            protocol,
                            service_version,
                        });
                    }
                    Ok(Response::Hello {
                        protocol,
                        service_version,
                    }) => {
                        let _ = events.send(FromWorker::HelloDone {
                            protocol,
                            service_version,
                        });
                        return Err(format!(
                            "unsupported flokid protocol version {protocol} (expected {})",
                            floki_proto::PROTOCOL_VERSION
                        ));
                    }
                    Ok(other) => {
                        return Err(format!(
                            "unexpected response to hello: {}",
                            response_kind(&other)
                        ));
                    }
                    Err(e) => return Err(format!("hello failed: {e}")),
                }
                *slot = Some(c);
            }
            Err(e) => return Err(format!("indexer not running ({e})")),
        }
    }
    let client = slot.as_mut().expect("just connected");
    client
        .call(req)
        .map_err(|e| format!("pipe call failed: {e}"))
}

#[cfg(not(windows))]
fn worker_main(cmds: Receiver<ToWorker>, events: Sender<FromWorker>) {
    // Non-Windows stub so the crate type-checks anywhere; floki is Windows-only.
    for cmd in cmds {
        match cmd {
            ToWorker::Stop => break,
            ToWorker::Search { seq, .. } => {
                let _ = events.send(FromWorker::ServiceDown);
                let _ = events.send(FromWorker::SearchDone {
                    seq,
                    result: Err("floki is Windows-only".to_owned()),
                });
            }
            ToWorker::Status { seq } => {
                let _ = events.send(FromWorker::ServiceDown);
                let _ = events.send(FromWorker::StatusDone {
                    seq,
                    result: Err("floki is Windows-only".to_owned()),
                });
            }
            ToWorker::Rescan { .. } => {
                let _ = events.send(FromWorker::RescanDone(Err(
                    "floki is Windows-only".to_owned()
                )));
            }
            ToWorker::SetVolumeEnabled { .. }
            | ToWorker::SetVolumeMonitor { .. }
            | ToWorker::RemoveVolume { .. } => {
                let _ = events.send(FromWorker::VolumeOpDone(Err(
                    "floki is Windows-only".to_owned()
                )));
            }
            ToWorker::TargetsConfigGet | ToWorker::TargetsConfigSet { .. } => {
                let _ = events.send(FromWorker::TargetsConfigDone(Err(
                    "floki is Windows-only".to_owned()
                )));
            }
            ToWorker::Shutdown => {
                let _ = events.send(FromWorker::ShutdownDone(Err(
                    "floki is Windows-only".to_owned()
                )));
            }
        }
    }
}

fn response_kind(r: &Response) -> &'static str {
    match r {
        Response::Hello { .. } => "hello",
        Response::Results { .. } => "results",
        Response::Status { .. } => "status",
        Response::TargetsConfig { .. } => "targets_config",
        Response::Ok {} => "ok",
        Response::Error { .. } => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_reports_missing_service() {
        let saved = std::env::var("FLOKI_PIPE").ok();
        std::env::set_var("FLOKI_PIPE", r"\\.\pipe\floki-ui-test-missing-xyz");
        let w = Worker::spawn();
        w.tx.send(ToWorker::Status { seq: 1 })
            .expect("worker alive");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut done = false;
        while std::time::Instant::now() < deadline && !done {
            if let Ok(FromWorker::StatusDone { seq, result }) =
                w.rx.recv_timeout(std::time::Duration::from_millis(200))
            {
                assert_eq!(seq, 1);
                assert!(result.is_err());
                done = true;
            }
        }
        assert!(done, "worker must answer status with an error");
        match saved {
            Some(v) => std::env::set_var("FLOKI_PIPE", v),
            None => std::env::remove_var("FLOKI_PIPE"),
        }
        drop(w);
    }
    #[test]
    fn next_command_drops_superseded_searches_and_polls() {
        let (tx, rx) = mpsc::channel();
        let search = |seq| ToWorker::Search {
            seq,
            query: format!("q{seq}"),
            sort: Sort::NameAsc,
            client_id: 1,
            offset: 0,
            max_results: PAGE_ROWS,
        };
        tx.send(search(1)).unwrap();
        tx.send(ToWorker::Status { seq: 1 }).unwrap();
        tx.send(ToWorker::Rescan { volume: None }).unwrap();
        tx.send(search(2)).unwrap();
        tx.send(ToWorker::Status { seq: 2 }).unwrap();
        drop(tx);
        let mut backlog = VecDeque::new();
        let mut order = Vec::new();
        while let Some(cmd) = next_command(&rx, &mut backlog) {
            order.push(match cmd {
                ToWorker::Search { seq, .. } => format!("search{seq}"),
                ToWorker::Status { seq } => format!("status{seq}"),
                ToWorker::Rescan { .. } => "rescan".to_owned(),
                other => format!("{other:?}"),
            });
        }
        assert_eq!(order, ["rescan", "search2", "status2"]);
    }

    #[test]
    fn stable_client_id_is_deterministic_and_scoped() {
        let id = stable_client_id(r"C:\\Apps\\floki.exe", "Ada");
        assert_ne!(id, 0);
        assert_eq!(id, stable_client_id(r"C:\\Apps\\floki.exe", "Ada"));
        assert_ne!(id, stable_client_id(r"C:\\Apps\\floki.exe", "Grace"));
        assert_ne!(id, stable_client_id(r"D:\\Apps\\floki.exe", "Ada"));
    }
}
