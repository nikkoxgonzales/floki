//! floki-proto: serde pipe-protocol types shared by service/cli/ui.
//!
//! Wire format: newline-delimited JSON over the named pipe from [`pipe_name`].
//! Every frame is one JSON object terminated by `'\n'`; each object carries a
//! `"type"` tag naming its variant (see `README.md` for an example per variant).

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{self, BufRead, Read as _, Write};

/// Protocol version negotiated with [`Request::Hello`] / [`Response::Hello`].
///
/// v2 adds per-volume `enabled`/`monitor` flags and the NTFS-targets
/// operations (`volumes_set_enabled`, `volumes_set_monitor`,
/// `volumes_remove`, `targets_config_get`/`targets_config_set`).
/// v3 adds `size`/`modified_ms`/`created_ms` to [`HitRow`] and the
/// `modified_*`/`created_*` [`Sort`] orders.
pub const PROTOCOL_VERSION: u32 = 3;

/// Default named-pipe path. Overridden at runtime by `FLOKI_PIPE` (see [`pipe_name`]).
pub const PIPE_NAME: &str = r"\\.\pipe\floki";

/// Maximum accepted size of one newline-delimited JSON frame (64 MiB).
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Resolve the pipe path to connect to / listen on.
///
/// Honors the `FLOKI_PIPE` environment variable; falls back to [`PIPE_NAME`]
/// when it is unset or empty.
#[must_use]
pub fn pipe_name() -> String {
    std::env::var("FLOKI_PIPE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| PIPE_NAME.to_owned())
}

/// Sort order for [`Request::Search`]. Mirrors `floki-core`'s `Sort`
/// (duplicated here so this crate stays dependency-free).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    NameAsc,
    NameDesc,
    PathAsc,
    PathDesc,
    /// By filesystem last-modified time (stat at search time; the index
    /// stores no timestamps).
    ModifiedAsc,
    ModifiedDesc,
    /// By filesystem creation time (stat at search time).
    CreatedAsc,
    CreatedDesc,
}

/// Pipe request: exactly one is sent, exactly one [`Response`] comes back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Handshake: client announces itself; server replies with [`Response::Hello`].
    /// (Approved addition on top of SPEC section 6.)
    Hello {},
    Search {
        query: String,
        max_results: u32,
        offset: u32,
        sort: Sort,
        client_id: u64,
        /// Fill each row's size and times. `false` returns names and paths
        /// only (the window reads metadata for visible rows itself); time
        /// sorts fill them regardless. Absent on the wire = `true`.
        #[serde(default = "default_true", skip_serializing_if = "is_true")]
        meta: bool,
    },
    Status {},
    Rescan {
        volume: Option<char>,
    },
    Shutdown {},
    /// Enable or disable an indexed volume (search visibility only; the
    /// volume stays in the index and keeps its journal tail).
    VolumesSetEnabled {
        volume: char,
        enabled: bool,
    },
    /// Turn the volume's journal tail on (`true`) or off (`false`).
    VolumesSetMonitor {
        volume: char,
        monitor: bool,
    },
    /// Drop the volume record and all its entries from the index.
    VolumesRemove {
        volume: char,
    },
    /// Read the global NTFS-targets policy (see [`TargetsConfig`]).
    TargetsConfigGet {},
    /// Replace the global NTFS-targets policy (see [`TargetsConfig`]).
    TargetsConfigSet {
        auto_include_fixed: bool,
        auto_include_removable: bool,
        auto_remove_offline: bool,
    },
}
/// Pipe response: one per [`Request`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Handshake reply: protocol + service versions.
    /// (Approved addition on top of SPEC section 6.)
    Hello {
        protocol: u32,
        service_version: String,
    },
    Results {
        total: u64,
        hits: Vec<HitRow>,
        elapsed_us: u64,
    },
    Status {
        entries: u64,
        volumes: Vec<VolumeStatus>,
        rss_bytes: u64,
        uptime_s: u64,
        state: IndexState,
    },
    Ok {},
    Error {
        message: String,
    },
    /// Global NTFS-targets policy (reply to `TargetsConfigGet`, and the
    /// stored shape after `TargetsConfigSet`).
    TargetsConfig {
        auto_include_fixed: bool,
        auto_include_removable: bool,
        auto_remove_offline: bool,
    },
}

/// One search hit: file name plus its parent directory path.
///
/// `size`/`modified_ms`/`created_ms` are populated by the service from a
/// filesystem stat of the hit (the index itself stores no metadata); `None`
/// means unknown — deleted between search and stat, no permission, or a
/// directory's size. They are `skip_serializing_if` so a v2-shaped row
/// (fields absent) still parses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HitRow {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    /// File size in bytes; always `None` for directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Last-modified time as Unix epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_ms: Option<i64>,
    /// Creation time as Unix epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_ms: Option<i64>,
}

/// Indexer lifecycle state reported in [`Response::Status`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IndexState {
    Loading,
    Scanning { volume: char, done: u64 },
    Ready,
}

/// Per-volume indexer status reported in [`Response::Status`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct VolumeStatus {
    pub letter: char,
    pub entries: u64,
    pub next_usn: i64,
    pub live: bool,
    /// Search visibility: `false` hides the volume's entries from results.
    /// Defaults to `true` for volumes indexed before this flag existed.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Journal-tail monitoring: `false` means scan-only (index once, no
    /// live tail). Defaults to `true` for volumes indexed before this flag
    /// existed.
    #[serde(default = "default_true")]
    pub monitor: bool,
}

/// Serde default for [`VolumeStatus`] v1-compat flags and
/// [`TargetsConfig`] v1-compat loads.
fn default_true() -> bool {
    true
}

/// `skip_serializing_if` twin of [`default_true`]: a `true` flag stays off
/// the wire, so v1 frames are byte-identical.
#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes `&T`
fn is_true(b: &bool) -> bool {
    *b
}

/// Global NTFS-targets policy: which newly arrived volumes the daemon
/// auto-indexes, and whether volumes that stay unopenable are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TargetsConfig {
    /// Auto-index newly arrived `DRIVE_FIXED` NTFS volumes.
    pub auto_include_fixed: bool,
    /// Auto-index newly arrived `DRIVE_REMOVABLE` NTFS volumes.
    pub auto_include_removable: bool,
    /// Drop indexed volumes that stay unopenable (offline media).
    pub auto_remove_offline: bool,
}

impl Default for TargetsConfig {
    fn default() -> Self {
        Self {
            auto_include_fixed: true,
            auto_include_removable: false,
            auto_remove_offline: true,
        }
    }
}
/// Serialize `value` as one newline-delimited JSON frame.
pub fn write_frame<W: Write>(w: &mut W, value: &impl Serialize) -> io::Result<()> {
    let mut buf = serde_json::to_vec(value).map_err(io::Error::other)?;
    buf.push(b'\n');
    w.write_all(&buf)?;
    w.flush()
}

/// Read one newline-delimited JSON frame, deserializing it as `T`.
///
/// Returns `Ok(None)` on clean EOF (no bytes pending). A line whose payload
/// exceeds [`MAX_FRAME_BYTES`] is an error, as is malformed JSON.
pub fn read_frame<R: BufRead, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut buf = Vec::new();
    let n = (&mut *r)
        .take(MAX_FRAME_BYTES as u64 + 1)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    let has_newline = buf.last() == Some(&b'\n');
    if !has_newline && buf.len() > MAX_FRAME_BYTES {
        drain_line(r)?;
        return Err(oversize_error());
    }
    if has_newline {
        buf.pop();
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    if buf.len() > MAX_FRAME_BYTES {
        return Err(oversize_error());
    }
    let value =
        serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(value))
}

fn oversize_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("frame exceeds {MAX_FRAME_BYTES} bytes"),
    )
}

/// Discard bytes up to and including the next newline (or EOF) so a rejected
/// oversize line does not poison subsequent reads.
fn drain_line<R: BufRead>(r: &mut R) -> io::Result<()> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = r.read_until(b'\n', &mut buf)?;
        if n == 0 || buf.last() == Some(&b'\n') {
            return Ok(());
        }
    }
}

/// Thin sync client over the named pipe (Windows only).
///
/// The service implements the server side itself; this is just a convenience
/// for `flk` / `floki` so they share one handshake + framing path.
#[cfg(windows)]
pub struct Client {
    reader: io::BufReader<interprocess::local_socket::Stream>,
}

#[cfg(windows)]
impl Client {
    /// Open the pipe at [`pipe_name`] in sync (blocking) mode.
    pub fn connect() -> io::Result<Self> {
        use interprocess::local_socket::{prelude::*, GenericFilePath};
        let name = pipe_name();
        let name = name.as_str().to_fs_name::<GenericFilePath>()?;
        let stream = LocalSocketStream::connect(name)?;
        Ok(Self {
            reader: io::BufReader::new(stream),
        })
    }

    /// Send one [`Request`], wait for its [`Response`].
    pub fn call(&mut self, request: &Request) -> io::Result<Response> {
        write_frame(self.reader.get_mut(), request)?;
        match read_frame(&mut self.reader)? {
            Some(response) => Ok(response),
            None => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "flokid closed the connection",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let mut wire: Vec<u8> = Vec::new();
        write_frame(&mut wire, value).unwrap();
        let mut cursor = io::BufReader::new(wire.as_slice());
        let back: Option<T> = read_frame(&mut cursor).unwrap();
        back.expect("frame must decode")
    }

    fn wire_json(value: &impl Serialize) -> String {
        let mut wire: Vec<u8> = Vec::new();
        write_frame(&mut wire, value).unwrap();
        String::from_utf8(wire).unwrap()
    }

    #[test]
    fn pipe_name_default_and_env_override() {
        let saved = std::env::var("FLOKI_PIPE").ok();
        std::env::remove_var("FLOKI_PIPE");
        assert_eq!(pipe_name(), PIPE_NAME);
        std::env::set_var("FLOKI_PIPE", r"\\.\pipe\floki-test");
        assert_eq!(pipe_name(), r"\\.\pipe\floki-test");
        std::env::set_var("FLOKI_PIPE", "");
        assert_eq!(pipe_name(), PIPE_NAME);
        match saved {
            Some(v) => std::env::set_var("FLOKI_PIPE", v),
            None => std::env::remove_var("FLOKI_PIPE"),
        }
    }

    #[test]
    fn search_wire_format_is_pinned() {
        let req = Request::Search {
            query: "foo".to_owned(),
            max_results: 100,
            offset: 0,
            sort: Sort::NameAsc,
            client_id: 7,
            meta: true,
        };
        assert_eq!(
            wire_json(&req),
            "{\"type\":\"search\",\"query\":\"foo\",\"max_results\":100,\
             \"offset\":0,\"sort\":\"name_asc\",\"client_id\":7}\n"
        );
        assert_eq!(round_trip(&req), req);
    }

    #[test]
    fn results_wire_format_is_pinned() {
        let res = Response::Results {
            total: 2,
            hits: vec![
                HitRow {
                    name: "foo.txt".to_owned(),
                    path: r"C:\docs".to_owned(),
                    is_dir: false,
                    size: Some(12),
                    modified_ms: Some(1_700_000_000_000),
                    created_ms: Some(1_699_000_000_000),
                },
                HitRow {
                    name: "bar".to_owned(),
                    path: r"C:\".to_owned(),
                    is_dir: true,
                    size: None,
                    modified_ms: None,
                    created_ms: None,
                },
            ],
            elapsed_us: 42,
        };
        assert_eq!(
            wire_json(&res),
            "{\"type\":\"results\",\"total\":2,\"hits\":[\
             {\"name\":\"foo.txt\",\"path\":\"C:\\\\docs\",\"is_dir\":false,\
             \"size\":12,\"modified_ms\":1700000000000,\"created_ms\":1699000000000},\
             {\"name\":\"bar\",\"path\":\"C:\\\\\",\"is_dir\":true}],\
             \"elapsed_us\":42}\n"
        );
        assert_eq!(round_trip(&res), res);
    }

    #[test]
    fn every_variant_round_trips() {
        let requests = vec![
            Request::Hello {},
            Request::Search {
                query: "*.rs".to_owned(),
                max_results: 10,
                offset: 5,
                sort: Sort::PathDesc,
                client_id: 1,
                meta: true,
            },
            Request::Status {},
            Request::Rescan { volume: Some('C') },
            Request::Rescan { volume: None },
            Request::Shutdown {},
            Request::VolumesSetEnabled {
                volume: 'C',
                enabled: false,
            },
            Request::VolumesSetMonitor {
                volume: 'D',
                monitor: true,
            },
            Request::VolumesRemove { volume: 'E' },
            Request::TargetsConfigGet {},
            Request::TargetsConfigSet {
                auto_include_fixed: true,
                auto_include_removable: false,
                auto_remove_offline: true,
            },
        ];
        for req in &requests {
            assert_eq!(&round_trip(req), req);
        }

        let responses = vec![
            Response::Hello {
                protocol: PROTOCOL_VERSION,
                service_version: "0.1.0".to_owned(),
            },
            Response::Results {
                total: 0,
                hits: Vec::new(),
                elapsed_us: 0,
            },
            Response::Status {
                entries: 3,
                volumes: vec![VolumeStatus {
                    letter: 'C',
                    entries: 3,
                    next_usn: 1234,
                    live: true,
                    enabled: true,
                    monitor: false,
                }],
                rss_bytes: 1024,
                uptime_s: 9,
                state: IndexState::Loading,
            },
            Response::Status {
                entries: 3,
                volumes: Vec::new(),
                rss_bytes: 0,
                uptime_s: 0,
                state: IndexState::Scanning {
                    volume: 'D',
                    done: 512,
                },
            },
            Response::Status {
                entries: 0,
                volumes: Vec::new(),
                rss_bytes: 0,
                uptime_s: 0,
                state: IndexState::Ready,
            },
            Response::TargetsConfig {
                auto_include_fixed: true,
                auto_include_removable: false,
                auto_remove_offline: true,
            },
            Response::Ok {},
            Response::Error {
                message: "boom".to_owned(),
            },
        ];
        for res in &responses {
            assert_eq!(&round_trip(res), res);
        }
    }

    #[test]
    fn volume_status_v1_wire_loads_with_flags_true() {
        let json = "{\"letter\":\"C\",\"entries\":3,\"next_usn\":1234,\"live\":true}";
        let status: VolumeStatus = serde_json::from_str(json).unwrap();
        assert!(status.enabled);
        assert!(status.monitor);
    }

    #[test]
    fn targets_config_default_matches_everything_policy() {
        let cfg = TargetsConfig::default();
        assert!(cfg.auto_include_fixed);
        assert!(!cfg.auto_include_removable);
        assert!(cfg.auto_remove_offline);
    }

    #[test]
    fn sort_serializes_as_lowercase_strings() {
        for (sort, wire) in [
            (Sort::NameAsc, "\"name_asc\""),
            (Sort::NameDesc, "\"name_desc\""),
            (Sort::PathAsc, "\"path_asc\""),
            (Sort::PathDesc, "\"path_desc\""),
            (Sort::ModifiedAsc, "\"modified_asc\""),
            (Sort::ModifiedDesc, "\"modified_desc\""),
            (Sort::CreatedAsc, "\"created_asc\""),
            (Sort::CreatedDesc, "\"created_desc\""),
        ] {
            assert_eq!(serde_json::to_string(&sort).unwrap(), wire);
            assert_eq!(round_trip(&sort), sort);
        }
    }
    #[test]
    fn hit_row_v2_wire_loads_with_meta_none() {
        // A v2 service emits no size/time fields; they must default to None.
        let json = r#"{"name":"a.txt","path":"C:\\d","is_dir":false}"#;
        let row: HitRow = serde_json::from_str(json).unwrap();
        assert_eq!(row.size, None);
        assert_eq!(row.modified_ms, None);
        assert_eq!(row.created_ms, None);
    }

    #[test]
    fn read_frame_none_on_empty_input() {
        let mut cursor = io::BufReader::new([].as_slice());
        let out: Option<Request> = read_frame(&mut cursor).unwrap();
        assert_eq!(out, None);
    }

    #[test]
    fn read_frame_errors_on_invalid_json() {
        let mut cursor = io::BufReader::new(b"not json\n".as_slice());
        let err = read_frame::<_, Request>(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_frame_errors_on_oversize_line() {
        let mut input = vec![b'a'; MAX_FRAME_BYTES + 16];
        input.push(b'\n');
        let mut cursor = io::BufReader::new(input.as_slice());
        let err = read_frame::<_, Request>(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
