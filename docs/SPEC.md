# Floki — architecture contract (v1)

This is the frozen contract the crates are built against: it keeps them compatible with each
other. If a change must deviate, say so in the change — do not silently change a type, name, or
path defined here. Research backing these choices:
`docs/research/ntfs-index.md`, `docs/research/landscape.md`.

## 0. Goal and non-goals

Floki is a Windows-only, Rust, instant file-name search tool (voidtools Everything class) whose
distinguishing constraint is **RAM: ≤ 60 MB resident per 1 million indexed entries** (Everything
1.4 ≈ 100 MB). Results must appear as you type (< 50 ms per keystroke on 1 M entries, warm).

v1 indexes **names, parent links, and attribute flags only**. No sizes, no dates, no content.
(Size/date are fetched from disk for the visible rows only.) Non-goals for v1: content search,
semantic search, ReFS live updates (ReFS is enumerated but treated as tier-2), HTTP/ETP servers.

> **Deviation (2026-10-06):** ReFS is now indexed with live updates. `FSCTL_ENUM_USN_DATA`
> fails on ReFS (`ERROR_INVALID_FUNCTION`), so the full scan is a directory walk
> (`FileIdExtdDirectoryInfo`); the journal is read as `USN_RECORD_V3` and 128-bit ids are
> packed into the 64-bit FRN (`floki_ntfs::pack_file_id`). Nothing above `floki-ntfs` changed.

## 1. Workspace layout (file ownership boundaries)

```
Cargo.toml                 workspace root (members below, shared [workspace.dependencies])
rust-toolchain.toml        stable, x86_64-pc-windows-msvc
crates/floki-core/         pure index + query engine. NO Win32 calls. Unit-testable anywhere.
crates/floki-ntfs/         Win32 volume handle + FSCTL_ENUM_USN_DATA + USN journal tail.
crates/floki-proto/        serde types for the pipe protocol (shared by service/cli/ui). No logic.
crates/floki-service/      bin `flokid`: elevated indexer, owns the index, serves the pipe.
crates/floki-cli/          bin `flk`: es.exe-style command-line client.
crates/floki-ui/           bin `floki`: eframe/egui window + tray + global hotkey; pipe client.
docs/                      this spec + research
```

Every crate has `README.md` (one paragraph: what it owns) and tests next to the code.

## 2. Shared dependencies (pin in `[workspace.dependencies]`)

`windows-sys` (features: Win32_Foundation, Win32_Storage_FileSystem, Win32_System_Ioctl,
Win32_System_IO, Win32_Security, Win32_System_Threading, Win32_System_Pipes),
`memchr`, `rayon`, `memmap2`, `serde` + `serde_json`, `thiserror`, `anyhow` (bins only),
`tracing` + `tracing-subscriber`, `regex`, `interprocess` (named pipes), `clap` (cli),
`eframe`/`egui` + `egui_extras` (ui), `tray-icon`, `global-hotkey`, `dirs`.
No `tokio` in v1 (threads + blocking pipes are enough and cheaper).

## 3. floki-core — index model

```rust
pub type EntryId = u32;                       // index into entries; never usize in stored data

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Entry {
    pub frn: u64,          // NTFS file reference number (48-bit index + 16-bit seq)
    pub parent_frn: u64,   // parent directory FRN; root dir's parent_frn == its own frn
    pub name_off: u32,     // byte offset into NameArena
    pub name_len: u16,     // byte length (UTF-8)
    pub flags: u16,        // EntryFlags bits below
}                          // 24 bytes; keep it Copy + repr(C) so it can be mmapped

bitflags-style consts: DIRECTORY = 1, HIDDEN = 2, SYSTEM = 4, REPARSE = 8, TOMBSTONE = 0x8000

pub struct NameArena { bytes: Vec<u8> }       // UTF-8, original case, append-only, no separators
pub struct Volume { pub letter: char, pub guid: [u8;16], pub journal_id: u64, pub next_usn: i64,
                    pub root_frn: u64 }
pub struct Index {
    pub volumes: Vec<Volume>,
    pub entries: Vec<Entry>,                  // grouped by volume; Volume records its range
    pub names: NameArena,
    frn_map: HashMap<(u8 /*vol idx*/, u64), EntryId>,   // only for live updates; rebuilt on load
    by_name: Vec<EntryId>,                    // sorted case-insensitively by name (prefix search)
}
```

RAM budget per entry: 24 (Entry) + ~30 (name) + 4 (by_name) + ~20 (frn_map) ≈ 78 bytes with the
map, ~58 without. The frn_map is the accepted overshoot for v1; a later pass may replace it with
FRN-sorted binary search. **Never store full paths.** `Index::path(id) -> String` rebuilds by
walking `parent_frn` (bounded loop; break on root or cycle > 512 hops).

Live updates (`Index::apply(vol, UsnEvent)`): Create → push entry; Delete → set TOMBSTONE;
RenameNew (same frn) → overwrite name (append new name to arena, leave old bytes as garbage) and
parent_frn; a compaction pass (`Index::compact()`) rewrites arena + entries without tombstones and
is run by the service when garbage > 10 % or on save.

Persistence (`Index::save(path)` / `Index::load(path)`), file `index.bin`, little-endian:
```
magic b"FLOKIDX1" | u32 header_len | header (serde_json: volumes, entry_count, arena_len)
| entries[] raw bytes | arena bytes | by_name u32[]
```
Written atomically (`.tmp` + rename). `load` may mmap; `frn_map` is rebuilt on load.

## 4. floki-core — query language and matching

Everything-compatible subset (parser in `query.rs`, evaluation in `search.rs`):

- whitespace = AND, `|` = OR, `!term` = NOT, `"exact phrase"`, `<group>` grouping.
- wildcards `*` and `?` inside a term switch that term to glob matching; otherwise substring.
- functions: `ext:rs;toml`, `path:<sub>` (matches on full rebuilt path), `folder:`/`file:`
  (type filter), `regex:<re>`, `case:` (case-sensitive), `wfn:` (whole filename).
  Unknown `xxx:` prefixes are matched literally (as Everything does).
- default matching is **case-insensitive substring on the name only** (not path), ASCII fast path
  + Unicode lowercase fallback for non-ASCII queries.

```rust
pub struct Query { /* parsed AST */ }
pub fn parse(input: &str) -> Query;                       // never fails; bad regex -> literal
pub struct SearchOptions { pub max_results: u32, pub offset: u32, pub sort: Sort }
pub enum Sort { NameAsc, NameDesc, PathAsc, PathDesc,
                ModifiedAsc, ModifiedDesc, CreatedAsc, CreatedDesc }
pub struct Hit { pub id: EntryId, pub vol: u8 }
pub fn search(index: &Index, q: &Query, opts: &SearchOptions, prev: Option<&[Hit]>) -> Vec<Hit>;
```
`prev`: if the new query is a strict extension of the previous one (Everything's trick), search
only inside `prev` instead of the whole index. The service keeps the last result set per client.
Full scans go through `rayon` over entry shards; the substring kernel uses
`memchr::memmem::Finder` on a lowercased needle against a per-entry lowercased window (v1), with a
folded-arena feature flag left for later measurement.

## 5. floki-ntfs — Win32 boundary

```rust
pub struct VolumeHandle { .. }                 // CreateFileW("\\\\.\\C:") GENERIC_READ, share RW
impl VolumeHandle {
    pub fn open(letter: char) -> Result<Self, NtfsError>;
    pub fn query_journal(&self) -> Result<JournalInfo, NtfsError>;    // FSCTL_QUERY_USN_JOURNAL
    pub fn create_journal(&self, max: u64, delta: u64) -> Result<(), NtfsError>;
    pub fn enumerate(&self, sink: &mut dyn FnMut(RawRecord)) -> Result<i64 /*next_usn*/, NtfsError>;
        // FSCTL_ENUM_USN_DATA with MFT_ENUM_DATA_V0, 64 KiB buffer, loops until ERROR_HANDLE_EOF
    pub fn read_journal(&self, from: i64, journal_id: u64, sink: &mut dyn FnMut(UsnEvent))
        -> Result<i64 /*next_usn*/, NtfsError>;                        // FSCTL_READ_USN_JOURNAL V0
}
pub struct RawRecord { pub frn: u64, pub parent_frn: u64, pub attrs: u32, pub name: String }
pub enum UsnEvent { Create(RawRecord), Delete { frn: u64 }, RenameOld { frn: u64 },
                    RenameNew(RawRecord), Overwrite(RawRecord) /* attr/other change */ }
pub enum NtfsError { AccessDenied, NotNtfs, JournalNotActive, JournalDeleteInProgress,
                     JournalWrapped, Io(u32 /*GetLastError*/) }
pub fn list_indexable_volumes() -> Vec<char>;  // GetLogicalDrives + GetVolumeInformationW in {"NTFS","ReFS"} (renamed 2026-10-06)
pub fn is_elevated() -> bool;
```
Hand-written `#[repr(C)]` `USN_RECORD_V2`, `MFT_ENUM_DATA_V0`, `READ_USN_JOURNAL_DATA_V0`,
`USN_JOURNAL_DATA_V0` (do not depend on a journal crate). Set `ReturnOnlyOnClose = 0`. Parse only
V2 records in v1; on V3/V4 major version return `NtfsError::Io` with a clear message.
Journal wrap detection: `FirstUsn > from` → `JournalWrapped`; journal id mismatch → `JournalWrapped`.
Tests that need a real volume are `#[ignore]` and named `admin_*`; run with
`cargo test -p floki-ntfs -- --ignored` from an elevated shell.

## 6. floki-proto — pipe protocol

Named pipe `\\.\pipe\floki` (override with env `FLOKI_PIPE`). Framing: newline-delimited JSON,
one request → one response, connection stays open. Security descriptor allows all local users
read/write (documented trade-off, same as Everything).

```rust
#[serde(tag = "type")] pub enum Request {
    Search { query: String, max_results: u32, offset: u32, sort: Sort, client_id: u64 },
    Status {},
    Rescan { volume: Option<char> },
    Shutdown {},
}
#[serde(tag = "type")] pub enum Response {
    Results { total: u64, hits: Vec<HitRow>, elapsed_us: u64 },
    Status { entries: u64, volumes: Vec<VolumeStatus>, rss_bytes: u64, uptime_s: u64,
             state: IndexState },
    Ok {}, Error { message: String },
}
pub struct HitRow { pub name: String, pub path: String /* parent dir */, pub is_dir: bool,
                    pub size: Option<u64>, pub modified_ms: Option<i64>,
                    pub created_ms: Option<i64> /* stat at search time; index stores none */ }
pub enum IndexState { Loading, Scanning { volume: char, done: u64 }, Ready }
pub struct VolumeStatus { pub letter: char, pub entries: u64, pub next_usn: i64, pub live: bool }
```

## 7. floki-service — `flokid`

- `flokid run` (foreground, must be elevated; exits with a clear message otherwise),
  `flokid install|uninstall` (per-user Run key, `--elevated` via a scheduled task with highest
  privileges so no UAC at login), `flokid status` (prints Status over the pipe).
- Startup: load `%LOCALAPPDATA%\Floki\index.bin` if present → for each volume compare journal id
  and replay from `next_usn`; on `JournalWrapped` / new volume → full enumerate on a background
  thread while already serving (state = Scanning). Scan all NTFS volumes by default; `--volumes C,D`
  restricts.
- Tail loop: one thread per volume, poll `read_journal` every 750 ms; batch events into the index
  under a single `RwLock<Index>` write per poll. Save index every 10 min and on Shutdown/Ctrl-C.
- Serves the pipe with one thread per client. Keeps per-`client_id` last hits for `prev` reuse.
- Logs via `tracing` to stderr and `%LOCALAPPDATA%\Floki\flokid.log`.
- Reports its own RSS in Status (`GetProcessMemoryInfo`), which is the RAM gate's data source.

## 8. floki-cli — `flk`

`flk <query...>` prints matching full paths (one per line), `-n <max>`, `-s name|path`,
`--json`, `flk status`, `flk rescan [C]`, `flk bench` (runs 20 canned queries, prints p50/p95
latency and service RSS — this is how the RAM/latency gate is measured). Exit codes: 0 ok,
1 no results, 2 service not running, 3 error.

## 9. floki-ui — `floki`

eframe/egui single window: search box on top (focused on show), virtualized results table
(`egui_extras::TableBuilder` with `body.rows`) showing name / path / size / modified / created
(metadata statted service-side per returned row; the index stores none), sortable headers for
name/path/modified/created, status bar (entry count, state, service RSS, query time).
Enter opens the file (`ShellExecuteW open`), Ctrl+Enter opens containing folder, Ctrl+C copies
path, Esc hides to tray. Tray icon with Show/Quit; global hotkey `Ctrl+Alt+Space` toggles.
Debounce 30 ms; sends `Search` with a stable `client_id`. If the pipe is not available, shows a
"Start indexer" button that launches `flokid run` elevated (`ShellExecuteW` verb `runas`).
Dark theme default, follows system if easy.

## 10. Gates (every change passes these)

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --release
cargo test --workspace
```
Plus, once the service exists, the product gate measured from an elevated
shell: `flk bench` must report p95 < 50 ms and RSS ≤ 60 MB per 1 M entries.

## 11. Milestones

- M1 skeleton: workspace builds, empty crates, gates green.
- M2 core + ntfs + proto.
- M3 service + cli (parallel) → first end-to-end `flk` search on this machine.
- M4 ui.
- M5 hardening: security review, benchmark loop until §0 numbers hold, persistence round-trip.
- Later (not v1): Everything `WM_COPYDATA` IPC compatibility (window class
  `EVERYTHING_IPC_WNDCLASS`) so Flow Launcher / PowerToys plugins work; `floki-mcp` stdio server via
  `rmcp`; fuzzy mode via `nucleo`; folded-arena SIMD kernel.
