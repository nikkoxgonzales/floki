# Floki research: NTFS MFT/USN indexing for a low-RAM Rust file search engine

> Status: research only, no code. Observed facts carry a URL; inferences are marked
> *(inference)* and things that could not be verified are marked **UNVERIFIED**.

## TL;DR recommendation

- **Enumeration API:** `FSCTL_ENUM_USN_DATA` (`MFT_ENUM_DATA_V0`, NTFS) for the full
  scan + `FSCTL_READ_USN_JOURNAL` (`READ_USN_JOURNAL_DATA_V0/V1`) for the live tail.
  Query state with `FSCTL_QUERY_USN_JOURNAL`; create/repair with
  `FSCTL_CREATE_USN_JOURNAL`. This is the Everything-style path: one sequential,
  OS-mediated MFT walk, no raw-sector `$MFT` parsing, no per-file `stat`.
- **Crate stack:** `windows-sys` (just `Win32_System_Ioctl` + `Win32_Storage_FileSystem`
  + `Win32_Foundation`, to keep build times down) for `DeviceIoControl`; own thin
  `#[repr(C)]` USN structs rather than depending on a journal crate; `memchr` for
  search; `rayon` for parallel scan/search; `memmap2`/`mmap` only for the persisted
  index file. Use `ntfs-reader` (0.4.5, Mar 2026) or `usn-journal-rs` (0.4.1, May 2026)
  as *reference implementations*, not dependencies — both are small single-author
  crates and Floki wants full control over record layout for RAM reasons.
- **Index layout:** `Vec<Entry{ frn: u64, parent: u32/u64, name_off: u32, name_len: u16,
  flags: u8 }>` + one contiguous lowercased-name arena + `HashMap<frn, idx>` only for
  the live-update path (or sort by FRN instead). Rebuild full paths on demand by
  walking parents. Target: **< 60 bytes/file** (vs Everything's ~100 bytes/file),
  i.e. < 60 MB per million files, by indexing names only and leaving size/dates
  unindexed (fetch from disk on display).

---

## 1. Enumerating the MFT fast from user mode

### 1.1 The USN ioctl family (primary source: Microsoft Learn)

| Ioctl | Purpose |
|---|---|
| `FSCTL_ENUM_USN_DATA` | Enumerate MFT records between two USNs (full-volume scan primitive). Takes `MFT_ENUM_DATA_V0` (NTFS) or `MFT_ENUM_DATA_V1` (ReFS) as input. https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_enum_usn_data |
| `FSCTL_READ_USN_JOURNAL` | Read journal records selectively (by USN + reason mask + `ReturnOnlyOnClose`). The live-tail primitive. https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_read_usn_journal |
| `FSCTL_QUERY_USN_JOURNAL` | Returns `USN_JOURNAL_DATA` (`UsnJournalID`, `FirstUsn`, `NextUsn`, `MaxUsn`, min/max supported major versions). https://ntdoc.m417z.com/fsctl_query_usn_journal |
| `FSCTL_CREATE_USN_JOURNAL` | Create or resize a journal (`CREATE_USN_JOURNAL_DATA{ MaximumSize, AllocationDelta }`). https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_create_usn_journal |
| `FSCTL_DELETE_USN_JOURNAL` | Delete journal (walks the whole MFT zeroing USNs; slow; persists across reboots; all other journal ioctls fail with `ERROR_JOURNAL_DELETE_IN_PROGRESS` meanwhile). https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_delete_usn_journal |

Walk-the-buffer sample (canonical `CreateFile("\\\\.\\c:")` + QUERY + READ loop):
https://learn.microsoft.com/en-us/windows/win32/fileio/walking-a-buffer-of-change-journal-records

### 1.2 Opening the volume, privileges

- Handle: `CreateFileW(L"\\\\.\\C:", GENERIC_READ|GENERIC_WRITE,
  FILE_SHARE_READ|FILE_SHARE_WRITE, OPEN_EXISTING)`. The `\\.\X:` form is documented
  under "Obtaining a Volume Handle for Change Journal Operations", and **all change-journal
  operations require membership in Administrators**
  (https://installsetupconfig.com/win32programming/windowsvolumeapis1_11.html — mirrors
  the classic MS "Change Journal" doc; privilege requirement also stated at
  https://learn.microsoft.com/en-us/windows/win32/fileio/using-the-change-journal-identifier
  and https://learn.microsoft.com/en-us/windows/win32/fileio/creating-modifying-and-deleting-a-change-journal).
- *Inference:* Floki therefore needs either "run elevated", an elevated service/daemon
  plus unelevated client (Everything-service model), or a one-time broker install
  (UFFS "Access Broker" model, see §2.3). There is no unprivileged USN path.
- Target volume must be NTFS 3.0+ or ReFS (`fsutil fsinfo ntfsinfo X:` to check).

### 1.3 Record versions and 128-bit FRNs

- `USN_RECORD_V2`: NTFS classic; 64-bit `FileReferenceNumber` (48-bit MFT index +
  16-bit sequence number). https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-usn_record_v2
- `USN_RECORD_V3`: identical layout except FRN and parent FRN become 16-byte
  (128-bit) IDs for ReFS (https://pinvoke.net/default.aspx/Structures/USN_RECORD.html;
  background https://digitalinvestigator.blogspot.com/2026/05/ntfs-forensics-usn-change-journal.html).
- `USN_RECORD_V4`: Windows 10+; extra granularity/security fields; **only returned if
  range tracking is enabled**, otherwise the journal still yields V2/V3
  (https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-usn_record_v4,
  https://www.hecfblog.com/2025/02/daily-blog-739-usn-versions-2-3-and-4.html).
- `MFT_ENUM_DATA_V0` = NTFS boundaries (`StartFileReferenceNumber=0`, `LowUsn=0`,
  `HighUsn=NextUsn` from QUERY);
  `MFT_ENUM_DATA_V1` = ReFS variant
  (https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-mft_enum_data_v0,
  https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-mft_enum_data_v1).
- `READ_USN_JOURNAL_DATA_V0` vs `V1`: V1 appends `MinMajorVersion/MaxMajorVersion`
  (Win8+). The old MSDN sample zero-fills a V0-shaped struct and breaks with
  `ERROR_INVALID_PARAMETER (87)` on Win10 when compiled against the V1 typedef —
  **always set both version fields from `USN_JOURNAL_DATA`** or explicitly use V0
  (https://stackoverflow.com/questions/46978678/walking-the-ntfs-change-journal-on-windows-10).
- Both `FSCTL_ENUM_USN_DATA` and `FSCTL_READ_USN_JOURNAL` return `USN + USN_RECORD*`
  in the output buffer; advance with `RecordLength`; next call continues from the
  leading `USN` (same walk-the-buffer doc as above).

### 1.4 Journal absent / wrapped / deleted

- **Integrity protocol:** store `(UsnJournalID, NextUsn)` per volume. On every poll,
  QUERY first; if `UsnJournalID` differs → journal was deleted/recreated → **full
  rescan** (https://learn.microsoft.com/en-us/windows/win32/fileio/using-the-change-journal-identifier).
- **Wrap:** the journal is a fixed-size ring; if `FirstUsn > stored NextUsn` the tail
  was overwritten → rescan that volume (backup-vendor writeup of the same condition:
  https://helpdesk.kaseya.com/hc/en-gb/articles/4407517757585).
- Journal sizing: `MaximumSize` target + `AllocationDelta` trim unit (cluster multiple);
  typical defaults ~32 MB / 4 KB delta; Everything author recommends 32–128 MB max
  (https://www.voidtools.com/forum/viewtopic.php?t=12403,
  https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/fsutil-usn).
- **Disabled journal:** NTFS usually has one; ReFS Change Journal is **off by default**
  and must be enabled with `fsutil` (forensic paper:
  https://dfrws.org/wp-content/uploads/2021/01/2021_APAC_paper-forensic_analysis_of_refs_journaling.pdf).
  If QUERY fails with "journal not active", Floki should offer to create it
  (`FSCTL_CREATE_USN_JOURNAL`) and fall back to rescan-on-start. **UNVERIFIED** for
  exact Win11 error code — test `ERROR_JOURNAL_NOT_ACTIVE` empirically.
- Deleting is expensive (full MFT walk, survives reboot); never delete silently —
  only create/resize (https://learn.microsoft.com/en-us/windows/win32/fileio/creating-modifying-and-deleting-a-change-journal).

### 1.5 `FSCTL_ENUM_USN_DATA` vs raw `$MFT` parsing

- Everything-style tools enumerate via `FSCTL_ENUM_USN_DATA`: one sequential kernel
  call stream, ~1 KB per MFT entry so ~0.1 GB of I/O per 100k files, seconds for a
  full disk (https://stackoverflow.com/questions/67554165/how-does-a-software-like-voidtoolss-everything-indexes-more-than-100k-files-in).
- Raw `$MFT` parsing (open volume, decode FILE records yourself) bypasses per-file
  ACLs and can grab sizes/dates in the same pass — WinSearch uses raw-parse as primary
  with `FSCTL_ENUM_USN_DATA` fallback (`WS_NO_MFT=1` forces fallback), plus a Win32
  backfill for attribute-list edge cases (https://github.com/STE-FalconSoftware/WinSearch).
  UFFS likewise reads the raw MFT with IOCP + bitmap skip of deleted records
  (https://github.com/skyllc-ai/UltraFastFileSearch/).
- *Inference / recommendation for Floki:* start with `FSCTL_ENUM_USN_DATA` only.
  Reasons: (a) no NTFS on-disk-format parser to maintain (attribute lists, resident vs
  non-resident, `$UpCase`); (b) the `ntfs`/`mft` crates are **offline parsers**, not
  live-volume enumerators (see §2.1); (c) raw access still needs admin anyway, so no
  privilege is saved. Add a raw path later only if profiling shows the ioctl is the
  bottleneck — WinSearch/UFFS evidence suggests it rarely is on SSDs.

---

## 2. Rust crates and existing Everything clones

### 2.1 Low-level building blocks

| Crate | Version / maintenance | Role for Floki |
|---|---|---|
| `windows` / `windows-sys` | Actively maintained by Microsoft; `windows-sys` is the raw-FFI, faster-build option. Use only needed features (`Win32_System_Ioctl`, `Win32_Storage_FileSystem`, `Win32_Foundation`). Docs: https://docs.rs/windows-sys (well-known; **UNVERIFIED** exact latest version number at time of writing — check crates.io before pinning). | `CreateFileW` + `DeviceIoControl` bindings. Lowest-level access; everything else sits above this. |
| `ntfs` (ColinFinck) | **0.4.0, 2023-06-13**; ~300k total downloads; `no_std`, zero-`unsafe`, NTFS 3.x RAII parser (`Ntfs::new` over `Read+Seek`, `NtfsFile`, `$FILE_NAME`, indexes, `$UpCase`). https://crates.io/crates/ntfs, https://docs.rs/ntfs/latest/ntfs/, https://github.com/ColinFinck/ntfs | Best **offline/forensic** NTFS reader. Wrong tool for live enumeration (needs a partition reader, no USN ioctls). Candidate only if Floki later adds a raw-`$MFT` fast path. |
| `mft` (omerbenamram) | 0.7.0-era forensic MFT-record parser, ~157 GitHub stars, safe Rust, JSON/CSV + resident-data extraction; geared at dumped MFT captures. https://github.com/omerbenamram/mft, https://crates.io/crates/mft, https://lib.rs/crates/mft | Same verdict as `ntfs`: offline analysis, not live indexing. Last-release date **UNVERIFIED** (crates.io metadata showed a 2019 timestamp in one mirror — re-check before citing). |
| `ntfs-reader` (kikijiki) | **0.4.5, 2026-03-28**; actively maintained (releases in 2024, 2025, Mar 2026); "fast in-memory scan of all records in the `$MFT`" + "USN journal reader"; `Volume::new("\\\\.\\C:")` + `Mft::new(volume)` API; documents HashMap-vs-Vec path-cache timings in its README. https://crates.io/crates/ntfs-reader, https://github.com/kikijiki/ntfs-reader | Closest to Floki's needs as a **reference**; small single-author crate ("undocumented and crappy" per its own 0.3.0 notes) — vendor the ideas, not the dependency. |
| `usn-journal-rs` (wangfu91) | **0.4.1, 2026-05-27**; active 2025–2026 release train; safe abstractions over USN journal + MFT enumerator + volume handles, NTFS **and ReFS**. https://crates.io/crates/usn-journal-rs, https://docs.rs/usn-journal-rs | Best live-journal API reference, especially for ReFS/V3 handling. Same vendoring caveat. CLI sibling `usn-parser-rs` demos search/monitor/read subcommands (https://github.com/wangfu91/usn-parser-rs). |
| `usnjrnl` (janstarke) | **0.4.5, 2023-05-08**; parses **offline** `$UsnJrnl:$J` captures (+ `mft2bodyfile` companion). https://crates.io/crates/usnjrnl | Forensics only; does not do live `DeviceIoControl`. Not a Floki dependency. |
| `usnrs` (simsor) | 0.2.1; **V2-records only**. https://crates.io/crates/usnrs | Too narrow; skip. |
| `everything-rs` / `everything-sys-bindgen` | Safe wrapper over the **Everything SDK (IPC to the Everything service)**. https://docs.rs/everything-rs/latest/everything_rs/ | Makes Floki a *client* of Everything — defeats the purpose. Skip except for interop tests. |
| `memchr` | De-facto std; SIMD (`SSE2` baseline, `AVX2` with `std` runtime detection; also `aarch64`/`wasm32`), `memmem` substring search ~5–6× std on standard benchmarks. https://docs.rs/memchr/latest/memchr/, https://github.com/BurntSushi/memchr | The search-scan kernel (§3). |
| `rayon` | De-facto std for data parallelism. (No separate citation gathered — **UNVERIFIED** here, but ubiquity is uncontroversial; confirm version at build time.) | Parallel MFT-decode + parallel query fan-out. |

### 2.2 What "lowest-level access" means here

Nothing on crates.io wraps `FSCTL_ENUM_USN_DATA` better than calling
`DeviceIoControl` yourself via `windows-sys` with hand-written `#[repr(C)]`
`MFT_ENUM_DATA_*` / `USN_RECORD_*` structs. That is ~200 lines, avoids inheriting
someone's record-layout bugs (the V0/V1 and 128-bit FRN pitfalls in §1.3), and keeps
the hot structs `Copy`-friendly for Floki's own arena layout.

### 2.3 Existing Rust Everything clones (all Windows-only, all admin-gated)

| Project | Approach | Notes / RAM |
|---|---|---|
| **goz** (mustafaahci) — daemon `gozd` (elevated) + CLI `goz` over named pipe; 4-crate workspace; raw MFT read + USN tail; explicit hard-link-rename handling "Everything 1.4 misses"; `docs/DESIGN.md`, `docs/SECURITY.md`. https://github.com/mustafaahci/goz | Raw MFT + USN tail | RAM numbers not published — **UNVERIFIED**. Closest architectural model for Floki. |
| **WinSearch** (STE-FalconSoftware) — M1–M5 milestones done: raw-`$MFT` one-pass names+sizes+dates, ENUM fallback, rayon + SIMD case-insensitive scan, 700 ms USN poll, `%LOCALAPPDATA%\WinSearch\index.bin` warm cache with journal catch-up, egui UI + tray. https://github.com/STE-FalconSoftware/WinSearch | Raw MFT primary, ENUM fallback | RAM **UNVERIFIED**; design (arena + flat entries + on-demand paths) is exactly the §3 recommendation. |
| **UltraFastFileSearch** (skyllc-ai) — raw MFT via IOCP sliding window, bitmap skip cuts 40–55% I/O, zero-copy parse into 224-byte `FileRecord`, extension+trigram accelerators, daemon+CLI+TUI+MCP, Access Broker for one-time elevation. https://github.com/skyllc-ai/UltraFastFileSearch/ | Raw MFT + daemon | 224 B/record is the *anti*-target for Floki (fast but fat) — useful upper bound. |
| **rayo** (waar19) — `FSCTL_ENUM_USN_DATA` enumerate + `FSCTL_READ_USN_JOURNAL` live updates, FRN-keyed index, parent-walk path rebuild, substring + `--trigram` mode with measured latencies (~6.6 ms plain vs ~0.5 ms trigram on one anecdotal query), named-pipe service + Slint GUI. https://github.com/waar19/rayo | ENUM + USN (Floki's planned path) | Only clone found publishing before/after trigram latencies; RAM **UNVERIFIED**. |
| **AllTheThings** (Swatto86) — Rust + Tauri UI clone, "reads the MFT directly, tails the USN journal". https://github.com/Swatto86/AllTheThings | MFT + USN | Early-stage; RAM **UNVERIFIED**. |
| **usn-parser-rs** (wangfu91) — search/monitor/read CLI over NTFS/ReFS. https://github.com/wangfu91/usn-parser-rs | ENUM + journal read | Utility, not a search engine; good ioctl reference. |
| **teamy-mft** — caches `.mft` + `.mft_search_index` files, substring query CLI. https://github.com/TeamDman/teamy-mft | Cached MFT dump + index | Persistence-side reference for §4. |

*Inference:* the ecosystem has converged on arena + parent-pointer + USN-tail; no
clone publishes rigorous bytes/file numbers, so Floki can differentiate with a
measured RAM benchmark from day one.

---

## 3. Compact index design (millions of files, low RAM)

### 3.1 The key decision: never store full paths

Everything "doesn't store path strings, instead it stores a parent pointer"
(author `void`, https://www.voidtools.com/forum/viewtopic.php?t=11234). Floki should
do the same: `Entry { frn, parent_frn, name }`, reconstructing display paths by
walking to the root only for the ~100 visible hits.

Rough per-file budgets *(estimates — arithmetic from struct sizes + published
Everything deltas, not measurements)*:

| Design | Bytes/file (approx) | 1 M files |
|---|---|---|
| Full UTF-16 path per row (naive) | 200–400+ | 200–400 MB — non-starter |
| `(u64 frn, u64 parent, String name)` idiomatic Rust | ~120–160 with allocator overhead | ~150 MB — loses to Everything |
| Packed `Entry{frn:u64,parent:u64,off:u32,len:u16,flags:u8}` (16 B) + single name arena (avg ~30 B/name UTF-8) | **~50–70** | **~60 MB** — beats Everything's ~100 |
| Above + lowercase-folded second arena (for caseless search) | +~30 | ~90 MB — parity; prefer on-the-fly fold or fold-once-at-index instead |
| Sorted-by-name `u32` index (prefix search, binary search) | +4 | +4 MB — cheap, take it |
| Trigram posting lists (`HashMap<[u8;3], Vec<u32>>`) | +30–100+ depending on cutoff | Doubles memory — optional feature flag only |
| Size + dates + attrs (Everything defaults) | +8 per field; +4–8 per fast-sort (https://www.voidtools.com/support/everything/options/) | +24–40 MB per M — **leave unindexed** in Floki |

### 3.2 Published Everything numbers (the target to beat)

- **~100 MB RAM per 1 M files** (x64, default settings) — stated repeatedly by the
  author: https://www.voidtools.com/forum/viewtopic.php?t=14053,
  https://www.voidtools.com/forum/viewtopic.php?t=8318; FAQ: fresh Win11 (~250k
  files) ≈ 35 MB RAM / 14 MB disk; 1 M files ≈ 100 MB RAM / 45 MB disk
  (https://www.voidtools.com/en-au/faq/).
- 1.4 vs 1.3: 1.4 indexed size+dates and fast sorts by default (extra ~8–16 B per
  field per file); disabling them returns to 1.3-level footprint
  (https://www.voidtools.com/forum/viewtopic.php?t=5503,
  https://www.voidtools.com/en-us/support/everything/indexes/).
- 1.5: author says it "should use less resources than 1.4" absent
  properties/content indexing (https://www.voidtools.com/forum/viewtopic.php?t=12413);
  content/property indexes dominate blowups (multi-GB cases in
  https://www.voidtools.com/forum/viewtopic.php?t=15931). x86 build uses ~half the
  RAM of x64 (pointer width) but caps at 2 GB
  (https://www.voidtools.com/forum/viewtopic.php?t=14053).
- **Floki target: ≤ 60 MB per 1 M files** for a name-only index — plausible per the
  table above, since Everything's 100 MB includes size+date-modified by default and
  Floki will not.

### 3.3 Search data structures

- **Concatenated names buffer + offsets** (one big `Vec<u8>` UTF-8 arena, entries hold
  `(offset, len)`): best cache locality for linear scan; append-only; deletion =
  tombstone entry + optional periodic compaction. Mirrors WinSearch's "one contiguous
  byte arena; flat entry array" (https://github.com/STE-FalconSoftware/WinSearch).
- **Case-insensitivity:** NTFS is caseless (volume `$UpCase` table; the `ntfs` crate
  exposes it via `read_upcase_table` + `UpcaseOrd`: https://docs.rs/ntfs/latest/ntfs/).
  Simplest correct-enough approach: lowercase-fold names once at index time (Unicode
  `to_lowercase`, ASCII-fast-path) and fold the query the same way; keeps the scan
  byte-exact. Full `$UpCase`-table fidelity is a later refinement — **UNVERIFIED**
  whether `$UpCase` differs observably from Unicode lowercase for search purposes.
- **SIMD substring:** `memchr::memmem::Finder` over the arena (prebuilt searcher
  reused across entries; byte-oriented so UTF-8/UTF-16-agnostic; AVX2 via runtime
  detection): https://docs.rs/memchr/latest/memchr/,
  https://github.com/BurntSushi/memchr. Parallelize with `rayon` across entry
  shards (WinSearch precedent). Per-entry scan pays searcher-construction once per
  query, not per entry — use the `Finder` API, not one-shot `find`.
- **Prefix search:** separate `Vec<u32>` of entry indices sorted by name at index
  time (4 B/file); binary-search the prefix range. Essentially free vs trigram.
- **Trigram index:** only as an opt-in accelerator for very large corpora; rayo's
  anecdotal numbers show the shape of the win (6.6 ms → 0.5 ms on a rare query)
  at a large RAM cost (https://github.com/waar19/rayo). Default off.
- **FRN map:** needed only to apply USN deltas (`frn → index`). A
  `HashMap<u64, u32>` costs ~16–24 B/file — acceptable at ~20 MB/M, or sort entries
  by FRN and binary-search to save it. Keep it out of the persisted file if sortable.

---

## 4. Live update loop and persistence

### 4.1 Delta polling

- Steady state: `QUERY` → `READ_USN_JOURNAL(StartUsn = stored NextUsn,
  UsnJournalID = stored ID, ReasonMask = all-or-filtered, Min/MaxMajorVersion set)`;
  apply records; store new `NextUsn`. Poll cadence ~0.5–1 s (WinSearch uses ~700 ms:
  https://github.com/STE-FalconSoftware/WinSearch); each poll is cheap when idle.
- Everything does exactly this: after the first MFT read it "doesn't look at the MFT
  anymore, but will keep reading the USN Journal" (~1 s cadence), and on startup
  replays the journal; if the journal no longer covers the downtime it reindexes that
  volume (https://www.voidtools.com/forum/viewtopic.php?t=12779,
  https://www.voidtools.com/forum/viewtopic.php?t=5601).

### 4.2 Rename / move / hard link / reparse handling

- **Rename/move = two records, same FRN:** `USN_REASON_RENAME_OLD_NAME (0x1000)` with
  old parent + `USN_REASON_RENAME_NEW_NAME (0x2000)` with new parent; the parent delta
  *is* the move
  (https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-usn_record_v3,
  https://www.usnparser.com/en/blog/usn-reason-codes-forensic-analysis).
  Update `parent_frn` + name arena entry; directory renames need no child updates
  (paths are rebuilt on demand — the payoff of §3.1).
- **Hard links:** `USN_REASON_HARD_LINK_CHANGE (0x10000)`; one FRN can have several
  `(parent, name)` pairs. goz explicitly handles "hard-link renames that Everything
  1.4 misses" (https://github.com/mustafaahci/goz) — Floki should store a small
  spillover list for multi-link FRNs rather than assuming 1:1. Subtle trap: with
  `ReturnOnlyOnClose = 1` ("summary mode") the OLD_NAME half can be lost
  (https://stackoverflow.com/questions/18058673/usn-journal-for-hard-links) — use
  `ReturnOnlyOnClose = 0` for the tail.
- **Reparse points / symlinks / junctions:** index the link name itself; never follow
  during enumeration (there is no directory walk, so mostly free); Everything 1.5
  added an explicit "not follow reparse points" option as precedent
  (from its changelog thread https://www.voidtools.com/forum/viewtopic.php?p=39850).
  Reason flag `USN_REASON_REPARSE_POINT_CHANGE (0x100000)` exists for updates.
- **Deletes:** tombstone by FRN; keep tombstones through one persistence cycle so a
  delete-then-recreate with a recycled MFT index + bumped sequence number can't
  resurrect stale names *(inference — validate against sequence-number behavior in
  testing)*.

### 4.3 "Journal wrapped / ID changed → rescan" (per volume, not global)

Conditions → full `FSCTL_ENUM_USN_DATA` re-enumeration of that volume only
(Everything 1.4+ reuses other volumes' indexes:
https://www.voidtools.com/support/everything/indexes/):
`UsnJournalID` change, `FirstUsn > stored NextUsn`, `ERROR_JOURNAL_DELETE_IN_PROGRESS`,
or `ERROR_JOURNAL_NOT_ACTIVE`. Recommend 64–128 MB journals to stretch coverage
(https://www.voidtools.com/forum/viewtopic.php?t=5601,
https://www.voidtools.com/forum/viewtopic.php?t=12403).

### 4.4 Persisting the index (what Everything does)

- Everything keeps the DB in RAM and writes `%LOCALAPPDATA%\Everything\Everything.db`
  **only on exit** (via `.db.tmp` + rename), reloads on start, then catches up from
  the USN journal — so shutdown kills before flush leave a `.tmp` behind
  (https://www.voidtools.com/en-us/support/everything/indexes/,
  https://www.voidtools.com/forum/viewtopic.php?t=17424).
- Floki plan *(inference)*: `index.bin` = header `(volume GUID, UsnJournalID, NextUsn,
  counts)` + flat entries + name arena + sorted-name `u32` array; `mmap` it read-only
  on startup (instant, OS-paged) and journal-forward from stored `NextUsn`; rewrite
  atomically (tmp + rename) on clean exit and every N minutes. Clones already doing
  this: WinSearch's `index.bin` + catch-up, teamy-mft's `.mft` + `.mft_search_index`
  pair (https://github.com/STE-FalconSoftware/WinSearch,
  https://github.com/TeamDman/teamy-mft).

---

## 5. What changed in 2024–2026

- **Dev Drive / ReFS:** Win11 (≥ 10.0.22621.2338) Dev Drives are ReFS volumes with
  block cloning, copy-on-write, and Defender performance mode
  (https://learn.microsoft.com/en-us/windows/dev-drive/,
  https://devblogs.microsoft.com/engineering-at-microsoft/dev-drive-and-copy-on-write-for-developer-performance/).
  Block cloning reached Win11 24H2 / Server 2025
  (https://learn.microsoft.com/en-us/windows/dev-drive/ — 2024 update note).
- **ReFS USN latency got worse, not better:** ReFS (Dev Drive and, per 2025 reports,
  regular ReFS too) **buffers changes before writing them to the USN journal**; the
  Everything author calls it a ReFS limitation with no tuning knob and suggests
  folder-indexing fallback for ReFS volumes
  (https://www.voidtools.com/forum/viewtopic.php?p=78795). *Implication for Floki:*
  NTFS-first; treat ReFS/Dev Drive as tier-2 (slower live updates) and use
  `MFT_ENUM_DATA_V1` + 128-bit FRNs there.
  **Probed 2026-10-06 (Dev Drive, Win11):** `FSCTL_ENUM_USN_DATA` is not
  supported on ReFS at all (`ERROR_INVALID_FUNCTION` for every input shape) —
  enumerate by directory walk instead; journal V3 reads delivered events
  within 1 ms.
- **USN_RECORD_V4 / range tracking:** V4 exists since Win10 but is inert unless range
  tracking is enabled; default journals still yield V2/V3 — no code changes required,
  just negotiate versions via `USN_JOURNAL_DATA` min/max
  (https://www.hecfblog.com/2025/02/daily-blog-739-usn-versions-2-3-and-4.html,
  https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-usn_record_v4).
- **Windows Search:** no new public MFT/USN API surfaced in 2024–2026 sources found;
  effort went into AI/Recall-style experiences, not the NTFS journal contract. The
  `FSCTL_*_USN_JOURNAL` surface is stable since Win8 — the plan above does not depend
  on anything new. (Absence of evidence only — **UNVERIFIED** that nothing changed;
  re-check `FSCTL_*` docs at implementation time.)
- **Crate ecosystem is fresh:** `ntfs-reader` 0.4.5 (Mar 2026) and `usn-journal-rs`
  0.4.1 (May 2026) both shipped in the last few months — the live-journal reference
  code is current (https://crates.io/crates/ntfs-reader,
  https://crates.io/crates/usn-journal-rs).

---

## Open questions (verify during implementation)

1. Does `FSCTL_ENUM_USN_DATA` on the test machines return V2 or V3 for NTFS, and does
   setting `MaxMajorVersion` affect throughput? Empirical test needed.
2. Exact error code when the journal is disabled on Win11 24H2 (`ERROR_JOURNAL_NOT_ACTIVE`?)
   and whether `FSCTL_CREATE_USN_JOURNAL` succeeds from an elevated daemon without reboot.
3. `$UpCase` vs Unicode-lowercase equivalence for Turkish-I et al. — measure false
   negatives of the fold-at-index approach on a multilingual corpus.
4. Tombstone vs immediate-compaction policy: measure arena growth on a churn-heavy
   volume (browser profile, build tree) over 24 h.
5. ReFS tier-2 behavior: quantify the USN buffering delay on a Dev Drive to decide
   between "documented lag" vs folder-watcher fallback.
6. 32-bit vs 64-bit Floki: Everything halves RAM with x86 at a 2 GB cap
   (https://www.voidtools.com/forum/viewtopic.php?t=14053) — probably not worth it
   for Floki, but the pointer-width audit (`u32` indices, never `usize`, in entries)
   achieves most of the saving on x64.
7. `mft` crate last-release date and `windows-sys` latest version — re-check crates.io
   at build time (marked UNVERIFIED above).
