# Floki research: product landscape, GUI stack, modern features

*Date: 2026-09-04. Research only; no code. Every non-trivial claim cites a URL.
"Inference" = my judgement from cited facts. "UNVERIFIED" = could not confirm.*

## TL;DR

- **GUI stack: eframe/egui (primary), Slint (fallback).** egui has the smallest
  native binaries (~3–5 MB), ~280 ms startup, proven million-row virtualized
  tables (`egui_table`, `ScrollArea::show_rows`), and works with the Tauri
  ecosystem's `tray-icon` + `global-hotkey` crates. Slint is the fallback if a
  native look, UI-preview tooling, or API stability matters more than raw
  lightness. Reject Tauri-webview (50–100 MB+ idle, slower startup) and GPUI
  (pre-1.0, weak tray/hotkey story on Windows) for v1.
- **Architecture: single binary, two modes — privileged indexer/service +
  unprivileged UI (the "Everything Service" pattern).** Service reads MFT/USN
  journals (~1 MB RAM in Everything's case) and serves results over IPC;
  UI + CLI are thin clients. v1 can ship single-process with the split designed
  in (service behind a `--svc` flag and a pipe protocol).
- **Must-have v1 features (the 5 users can't live without):** (1) instant
  as-you-type filename results over the whole drive; (2) Everything-compatible
  filter syntax (`ext:`, `path:`, `size:`, `dm:`, booleans, regex);
  (3) global hotkey + tray + autostart-at-login; (4) Everything IPC/`es.exe`
  drop-in compatibility so Flow/PowerToys/Wox plugins work;
  (5) frecency/run-history ranking + fast sorting.
- **v2 list:** fuzzy/typo-tolerant mode (nucleo), content search (opt-in,
  off by default), MCP server via `rmcp` (cheap), local semantic filename
  search via fastembed/ONNX (opt-in, ~50–60 MB). **Never for v1:** local LLM
  NL-to-filter translation (GBs of RAM, weak payoff).

## 1. Competitor landscape (2025–2026)

### 1.1 voidtools Everything 1.4 (stable) — the benchmark

- **How it works (fact):** reads the NTFS Master File Table once for initial
  index, then tails the per-volume USN change journal (~every second) for
  real-time updates; index lives fully in RAM, DB file is only save/load.
  (https://www.voidtools.com/forum/viewtopic.php?t=12779,
  https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/fsutil-usn)
- **RAM (fact):** author states ~100 MB per 1M indexed files; a fresh Windows
  install uses ~14 MB RAM / <9 MB disk; 1M files ≈ 75 MB RAM / 45 MB disk.
  (https://www.voidtools.com/forum/viewtopic.php?t=9024,
  https://everything-tool.com/)
- **Interfaces (fact):** SDK DLL (`Everything_SetSearch`, `Everything_Query`,
  `Everything_GetResult*`, run-count APIs); raw IPC over `WM_COPYDATA`;
  `es.exe` CLI (v1.1.0.37); HTTP/ETP/Server plugins; `es:` URL protocol.
  Lite build disables servers + IPC.
  (https://www.voidtools.com/support/everything/sdk,
  https://www.voidtools.com/support/everything/command_line_interface/,
  https://www.voidtools.com/downloads)
- **Likes:** instant results, near-zero idle CPU, tiny base footprint, regex +
  filters, portable ZIP, NAS/ETP support.
  (https://www.reddit.com/r/windows/comments/1bj9ac4/if_you_use_windows_and_you_have_difficulty/,
  https://windowsforum.com/windows-news.4/six-fast-windows-11-search-alternatives-everything-fluent-flow-more.381211/)
- **Complaints:** single developer + perpetual alpha (see 1.2); RAM explodes
  with content indexing or 10M+ files (multi-GB reports; author advises
  ≤1 GB indexed content); service doubles RAM during rescan; paging to disk
  when minimized causes freezes; dated UI.
  (https://www.voidtools.com/forum/viewtopic.php?t=16351,
  https://www.voidtools.com/forum/viewtopic.php?t=15931,
  https://www.voidtools.com/forum/viewtopic.php?t=9024)

### 1.2 Everything 1.5 alpha — still alpha in 2025, beta announced 2026

- **Status (fact):** 1.5 stayed alpha through 2025 (build 1.5.0.1404a,
  Dec 2025); author said Sep 2024 he was "working on the final changes to
  alpha" with no beta date; forum upgrade guide notes a Beta line starting
  May 2026 that drops the `alpha_instance` requirement.
  (https://voidtools.com/forum/viewtopic.php?t=9787,
  https://www.voidtools.com/forum/viewtopic.php?t=15609,
  https://www.voidtools.com/forum/viewtopic.php?t=17663)
- **New vs 1.4 (fact):** property indexing/search/sort, dark mode, background
  index updates, faster search, natural sort, index journal, Everything Server,
  duplicate finding, virtual folders, undo, mixed files+folders.
  (https://www.voidtools.com/everything-1.5a/)
- **1.5 architecture note (fact):** adds "run indexing process as
  administrator" as an alternative to the service; installer recommends the
  service, portable recommends the admin indexing process.
  (https://ftp.voidtools.com/en-us/support/everything/options/)
- **Friction for Floki (fact):** 1.5a runs under instance name "1.5a" by
  default, which breaks Flow Launcher integration until users set
  `alpha_instance=false` — evidence the ecosystem is sensitive to instance
  naming, and a drop-in must answer the default instance.
  (https://www.voidtools.com/forum/viewtopic.php?t=16523)
- **Inference:** 1.5's scope creep (content index, properties, servers) is
  exactly what drives its RAM complaints — Floki's "lightweight" wedge is
  staying name-index-only in v1.

### 1.3 Launcher ecosystem (depends on Everything — Floki's free riders)

- **Flow Launcher (fact):** open-source MIT launcher (~135 MB RAM in normal
  use); old standalone Everything plugin merged into the Explorer plugin;
  requires Everything 1.4.1+ service running.
  (https://github.com/Flow-Launcher/Flow.Launcher.Plugin.Everything,
  https://flow-launcher.com/)
- **PowerToys Run / Command Palette (fact):** forked from Wox; Everything
  support comes from community plugin EverythingPowerToys (3.3k stars);
  2026 Command Palette added an official Everything extension.
  (https://github.com/microsoft/PowerToys/issues/20825,
  https://github.com/lin-ycv/EverythingPowerToys,
  https://www.xda-developers.com/powertoys-finally-has-launcher-thats-as-good-as-wox-or-flow/)
- **Wox (fact):** original launcher, v2 betas add JS/Python plugins; Files
  plugin can use Everything; needs Everything running.
  (https://windowsforum.com/windows-news.4/wox-2-fast-open-source-cross-platform-launcher-for-windows.395035)
- **The IPC contract Floki must clone (fact):** client finds window class
  `EVERYTHING_IPC_WNDCLASS`, sends `WM_COPYDATA` with `dwData =
  EVERYTHING_IPC_COPYDATAQUERY` and an `EVERYTHING_IPC_QUERY` struct
  (`max_results`, `offset`, `reply_copydata_message`, `search_flags`
  = REGEX|MATCHCASE|MATCHWHOLEWORD|MATCHPATH, `reply_hwnd`,
  `search_string`); results return via `WM_COPYDATA` as
  `EVERYTHING_IPC_LIST`; `EVERYTHING_IPC_CREATED` is broadcast when the
  server starts. SDK adds sort/request-flags/run-count APIs (`IPC2`).
  (https://www.voidtools.com/support/everything/sdk/ipc_c_example/,
  https://www.voidtools.com/support/everything/sdk,
  https://deepwiki.com/voidtools/everything_sdk3/2-architecture-and-ipc-protocol)
- **Inference:** implementing the `WM_COPYDATA` query path + default instance
  name + a compatible `es.exe`-style CLI buys Floki the whole launcher
  ecosystem on day one. ETP/HTTP server parity can wait.

### 1.4 Listary, Fluent Search — the UX-first competitors

- **Listary (fact):** hooks Explorer + open/save dialogs (type-to-find,
  Quick Switch), launcher mode with usage-frequency sorting; freemium
  (Pro one-time license), closed source; v6 rewritten, matches Everything on
  raw name speed per vendor.
  (https://www.listary.com/blog/best-file-search-tools-of-2024-everything-vs-listary,
  https://windowsforum.com/windows-news.4/six-fast-windows-11-search-alternatives-everything-fluent-flow-more.381211/)
- **Fluent Search (fact):** all-in-one launcher (files, apps, tabs, windows,
  clipboard); indexer is switchable: native / Windows Search / Everything;
  unique Screen Search (keyboard-driven UI-element + OCR search via UI
  Automation/image recognition); tags + plugins; single-developer project,
  free on Microsoft Store.
  (https://fluentsearch.net/, https://fluentsearch.net/docs/Getting%20started,
  https://fluentsearch.net/docs/Screen%20search/General)
- **Complaints pattern (fact):** Listary — Pro paywall, config complexity;
  Fluent — resource cost when deep features enabled, one-dev bus factor.
  (https://appmus.com/vs/listary-vs-everything,
  https://windowsforum.com/windows-news.4/six-fast-windows-11-search-alternatives-everything-fluent-flow-more.381211/)

### 1.5 Windows Search (the incumbent, improving fast in 2026)

- **2026 improvements (fact):** typo-tolerant app search ("utlook"→Outlook),
  local-file-first ranking, optional Bing-off toggle in testing, 2-character
  trigger shipped (KB5094126), substring matching in Insider.
  (https://www.windowslatest.com/2026/06/17/microsoft-confirms-windows-11-search-will-find-your-apps-not-bing-results-even-if-you-make-typos/)
- **Standing complaints (fact):** slow/non-indexed folders, cluttered panel,
  Bing-first results, inconsistency — the reason users install Everything +
  a launcher.
  (https://windowsforum.com/windows-news.4/six-fast-windows-11-search-alternatives-everything-fluent-flow-more.381211/)
- **Inference:** the bar rises yearly; Floki's durable edge is guaranteed
  local-only, instant, keyboard-first filename search — not content/AI.

### 1.6 Minor players: UltraSearch, Locate32, grepWin, fsearch, plocate

- **UltraSearch, JAM Software (fact):** free; reads the NTFS MFT directly per
  query — no background index/service; instant name+metadata search; does NOT
  search file contents; NTFS-only for full speed.
  (https://appmus.com/software/ultrasearch, https://www.jam-software.com/ultrasearch)
- **Related Rust prior art (fact):** `Dicklesworthstone/ultrasearch` (26
  stars) combines NTFS MFT enumeration via `usn-journal-rs` with a Tantivy
  content index in a multi-process Rust architecture — closest design
  reference for Floki's indexer. (https://github.com/Dicklesworthstone/ultrasearch)
- **Locate32 (fact):** open-source `updatedb`-style scheduled index; flagged
  Discontinued on AlternativeTo. Lesson: stale periodic indexes lose to
  USN-realtime. (https://alternativeto.net/software/ntfs-search/)
- **grepWin (UNVERIFIED details):** regex content search + replace tool, not
  an index — relevant only as the "content search" complement; do not treat
  as a direct competitor without further checking.
- **fsearch, Linux (fact):** C/GTK3, explicitly Everything-inspired; instant
  as-you-type, wildcards, regex, include/exclude folders, fast sort;
  praised as "as good as Everything" on Linux, with indexing speed as the
  main gripe. Design reference for filter UX on a non-Windows codebase.
  (https://github.com/cboxdoerfer/fsearch, https://itsfoss.com/fsearch/,
  https://alternativeto.net/software/fsearch/about)
- **plocate, Linux (fact):** trigram posting-list index; ms queries over
  27M paths; DB 466 MB vs mlocate 1.1 GB; default `locate` on Debian 12+ /
  Ubuntu 23.04+; name-only, stale until `updatedb`. Trigram-index idea is
  reusable for Floki's substring matching.
  (https://plocate.sesse.net/, https://unix.stackexchange.com/questions/727862/difference-between-mlocate-and-plocate)

### 1.7 The 5 features users cannot live without (inference from above)

1. **Instant as-you-type results across all drives** — the defining trait;
   every Reddit/roundup thread leads with it.
2. **Power filter syntax** (`ext:`, `path:`, `size:`, `dm:`, regex,
   booleans) — what keeps power users from going back to Start-menu search.
3. **Zero-friction access** — global hotkey, tray icon, start-with-Windows,
   no UAC prompts (i.e. the service pattern).
4. **Ecosystem compatibility** — IPC + `es.exe` so Total Commander, Opus,
   Flow/PowerToys/Wox keep working.
5. **Recency/frecency ranking + run history** — Listary's usage sorting and
   Everything's run-count APIs exist because plain alphabetical sort doesn't
   match how people re-find files.

## 2. GUI stack for a lightweight Rust Windows app

### 2.1 Measured numbers (facts; setups differ — compare within, not across rows)

| Stack | Binary | Startup | Idle/app RAM | Notes |
|---|---|---|---|---|
| egui/eframe | ~3–5 MB release desktop (2025 roundup); 18 MB hello-world in 2023 test | ~280 ms window (2023, Linux/X11) | Native/Rust group: 100–170 MB in a 2026 15-framework benchmark (methodology UNVERIFIED — treat as directional) | Immediate-mode; only repaints on interaction |
| iced | ~8–12 MB | ~230 ms window | same native group as above | Elm architecture, wgpu renderer |
| Slint | Rust template ~3.6 MB, ~2.6 MB w/ LTO+strip (femtovg); runtime claims <300 KiB (marketing) | fast (no hard number found — UNVERIFIED) | blank window ~18–20 MB w/ Qt or Skia backend at 1200×800 (maintainer-measured, VM) | ListView virtualizes; `.slint` markup + VS Code live preview |
| Tauri 2 | bundle 3–5 MB vs Electron 100–200 MB; app RAM typically 50–100 MB | ~380 ms, window at ~125 ms (2023 test) | WebView2 preinstalled on Win 10/11, so no runtime download | Webview floor: 450–530 MB group in the 2026 benchmark |
| GPUI | no stable numbers found (UNVERIFIED) | — | Zed-on-Windows uses DirectX 11 + DirectWrite | pre-1.0, frequent breaking changes |

Sources: (http://lukaskalbertodt.github.io/2023/02/03/tauri-iced-egui-performance-comparison.html,
https://an4t.com/rust-gui-libraries-compared,
https://zenn.dev/mizugeeks/articles/1019cf2353d343?locale=en,
https://github.com/slint-ui/slint/discussions/3376,
https://slint.dev/declarative-rust-gui,
https://lobehub.com/skills/macphobos-research-mind-toolchains-rust-desktop-applications,
https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md,
https://windowsforum.com/windows-news.4/zed-editor-arrives-on-windows-with-native-rust-gpu-ui-and-directx-11.384963)

### 2.2 Criterion-by-criterion (facts + inference)

- **Virtualized 1M+ rows:** egui has `ScrollArea::show_rows` built-in plus
  `egui_table` (explicitly "millions of rows", cell virtualization, sticky
  headers) and `egui_virtual_list`.
  (https://lib.rs/crates/egui_table, https://docs.rs/egui_table/latest/egui_table/struct.Table.html,
  https://lib.rs/crates/egui_virtual_list).
  Slint's `ListView` instantiates only visible elements and its Rust `Model`
  trait supports lazy `row_count`/`row_data` — suitable, but chat-scale
  reports show full-model resets flicker; must use incremental
  `set_row_data`/push. (https://docs.slint.dev/latest/docs/slint/reference/std-widgets/views/listview/,
  https://docs.rs/slint/latest/slint/, https://github.com/slint-ui/slint/issues/4097).
  iced/GPUI virtualization stories found no hard evidence (UNVERIFIED).
- **Tray + global hotkey + show/hide:** `tray-icon` (Tauri-owned, 24M+
  downloads) and `global-hotkey` (61k/month) crates support Windows via a
  win32 event loop — composable with winit/eframe apps.
  (https://lib.rs/crates/tray-icon, https://lib.rs/crates/global-hotkey,
  https://docs.rs/global-hotkey/latest/global_hotkey/).
  GPUI tray needs an immature third-party crate (`gpui-tray`, 2 stars,
  macOS stub). (https://github.com/Yamrc/gpui-tray)
- **DPI/dark mode/keyboard UX:** egui is non-native looking (its own FAQ:
  "if you want a GUI that looks native, egui is not for you"), DPI via
  winit, theming is manual; Slint has real theming/stability (1.x, no
  breaking changes) and keyboard focus support. eframe 0.32 needs Rust
  1.95+. (https://github.com/emilk/egui, https://docs.rs/egui/latest/egui/index.html,
  https://www.pistack.xyz/posts/2026-08-25-rust-gui-frameworks-egui-iced-slint-comparison/)
- **Dioxus native, windows-rs bare Win32:** no measurements gathered
  (UNVERIFIED) — Dioxus desktop is webview-based (same RAM objection as
  Tauri); raw Win32 maximizes leanness at maximal dev cost. A zero-dependency
  Win32 tray app in Rust exists as proof it's feasible
  (https://github.com/OlaProeis/TrayVault), but it is not a v1 strategy.
- **Recommendation (inference): eframe/egui primary; Slint fallback.**
  egui wins on binary size, startup, virtualized-table maturity, and crate
  ecosystem for tray/hotkey; its weakness (non-native look) matters least
  for a keyboard-driven search palette. Slint is the fallback if native
  aesthetics, designer tooling, or API stability outrank ounces of RAM.

## 3. Architecture patterns

- **The Everything Service pattern (fact):** a stateless ~1 MB service that
  only reads NTFS volumes/monitors USN journals so the GUI runs as standard
  user with no UAC prompts; managed via `Everything.exe -install-service /
  -start-service / -stop-service / -svc` (portable `-svc` runs it as a plain
  admin process); 1.5 adds a no-service "indexing process as admin" option.
  Service uses a named pipe (configurable `-svc-pipe-name`); any local user
  can query it — a documented security tradeoff.
  (https://www.voidtools.com/support/everything/everything_service/,
  https://www.voidtools.com/forum/viewtopic.php?t=4311,
  https://voidtools.com/forum/viewtopic.php?t=14335)
- **Why split (fact):** reading MFT/USN needs elevation; running the whole
  GUI elevated breaks drag-drop/SUBST drives/mapped drives and spawns
  elevated children. Best practice per forum: same-credentials UI + service.
  (https://www.voidtools.com/forum/viewtopic.php?t=12779,
  https://voidtools.com/forum/viewtopic.php?t=11426)
- **Named-pipe IPC in Rust (fact):** `tokio::net::windows::named_pipe`
  (ServerOptions, connect loop, per-client tasks) or the `interprocess`
  crate (sync + tokio async named pipes/local sockets, MIT/Apache).
  Cross-privilege pipes need an explicit permissive SECURITY_ATTRIBUTES
  (allow-all read/write), a fiddly but solved problem.
  (https://fluxsec.red/communicating-from-hooked-syscall-rust,
  https://docs.rs/interprocess,
  https://docs.rs/interprocess/latest/x86_64-pc-windows-msvc/interprocess/os/windows/named_pipe/index.html)
- **CLI client (fact):** `es.exe [options] <search>` uses full Everything
  syntax plus sort/display/export flags (`-sort size`, `-n 10`, `-export-efu`,
  `-save-db`, `-reindex`), return codes 0–8; source on GitHub.
  (https://www.voidtools.com/support/everything/command_line_interface/,
  https://github.com/voidtools/ES)
- **Auto-start (fact):** Everything offers "start on system startup" (tray +
  preload DB, no window); installer + Run-key/Startup-folder patterns are
  standard — e.g. TrayVault registers the per-user Run key and launches
  `--minimized` to tray. (https://www.voidtools.com/support/everything/installing_everything/,
  https://github.com/OlaProeis/TrayVault)
- **Recommendation (inference):** one binary, `floki.exe` (UI) +
  `floki.exe --svc` (indexer, runs elevated/service) + `flk.exe` (CLI) or
  one CLI with subcommands; JSON-over-named-pipe v1 internally, plus the
  `WM_COPYDATA` Everything-compatible surface for third parties; per-user
  Run-key autostart, minimized to tray.

## 4. Query language users expect

- **Baseline syntax (fact):** `term := [!][modifier:][function:]<text>`;
  space=AND, `|`=OR, `!`=NOT, `<>`=grouping, `"…"`=exact phrase; wildcards
  `*` (no backslash), `**` (any), `?`; macros (`audio:`, `zip:`, `doc:`…).
  (https://www.voidtools.com/en-us/support/everything/search_syntax/,
  https://gist.github.com/BrianPurgert/80326c3c2aa1ef26322d93836118d3e0)
- **Filters (fact):** `path:`/`ext:`/`size:`/`dm:` (+ `dc:`/`da:`/attributes/
  `run-count:`/`date-run:` in ES display/sort flags); `es.exe` documents
  DIR-style `/a` attributes and sorts (name/path/size/extension/dates,
  run-count). (https://www.voidtools.com/support/everything/command_line_interface/,
  https://github.com/voidtools/ES)
- **Fuzzy/typo tolerance (fact):** Rust's best option is `nucleo` /
  `nucleo-matcher` (helix-editor, fzf-identical scoring, Smith-Waterman with
  affine gaps): ~2–2.6 ms vs skim's ~17–18 ms on Linux-kernel-file benches
  (~6–8× faster), 3M-item demo at ~1 frame vs fzf's ~1 s; correct Unicode
  grapheme handling; MPL-2.0 (copyleft — check compatibility with Floki's
  planned license). (https://github.com/helix-editor/nucleo,
  https://lib.rs/crates/nucleo, https://lib.rs/crates/nucleo-matcher)
- **Ranking (fact → inference):** Everything already tracks run-count/run-dates
  (SDK `Get/Set/IncRunCountFromFileName`) and Listary sorts by frequency +
  recency — frecency is table stakes, implementable with zero ML.
  (https://www.voidtools.com/support/everything/sdk,
  https://www.listary.com/blog/best-file-search-tools-of-2024-everything-vs-listary)
- **Recommendation (inference):** v1 = Everything-compatible core (substring
  default, wildcards, regex flag, `path:/ext:/size:/dm:`, AND/OR/NOT,
  quotes, sort incl. run-count) + frecency re-rank; v1.x adds opt-in
  `nucleo`-fuzzy mode (gated: fuzzy over 1M+ rows per keystroke is the
  perf risk, so cap candidate set / debounce / prefix-first).

## 5. Post-AI-revolution features (verdicts with RAM costs)

- **Local semantic search over names/content — v2, opt-in, NEVER default.**
  `fastembed` (Rust, ONNX via `ort`, no tokio needed) ships MiniLM
  (~22 MB model) to BGE-M3; a real deployment reports ~50 MB loaded,
  ~60 MB during inference, ~28 ms/single embed, 1.8–3.2 s cold load.
  (https://docs.rs/fastembed, https://crates.io/crates/fastembed,
  https://github.com/Anush008/fastembed-rs,
  https://github.com/iflow-mcp/zircote-subcog/blob/main/docs/adrs/adr_0007.md).
  Inference: embeddings for 1M filenames ≈ 1.5 GB at 384-dim f32
  (arithmetic, UNVERIFIED at scale) — so ship filename-vector search capped
  to a working set, content vectors never in v1.
- **NL queries → filter syntax via small local model — v2 at best, likely
  never.** Even "small" local LLMs cost GBs of RAM/VRAM and seconds of
  latency for a translation task a cheat-sheet + autocomplete solves;
  Everything's own roadmap instead adds `:function:` autocomplete popups.
  (https://www.voidtools.com/forum/viewtopic.php?t=11267 for the autocomplete
  direction; model-size costs UNVERIFIED — re-verify before committing).
- **MCP server exposing search to AI agents — v1.x, cheap and high-leverage.**
  MCP spec is date-versioned (2025-11-25 / 2026-07-28); official Rust SDK is
  `rmcp` (tokio, stdio + Streamable HTTP transports, tools/resources/prompts
  macros). A `floki-mcp` stdio server reusing the pipe client is ~hundreds
  of LOC and ~0 idle RAM when not spawned.
  (https://github.com/modelcontextprotocol/rust-sdk,
  https://github.com/warpdotdev/rmcp, https://docs.rs/rmcp)

## 6. Naming: does "Floki" collide?

- **No Windows file-search product named Floki found.** A targeted web
  search for a "Floki" Windows search app returned no relevant product —
  weak evidence only, not clearance.
  (search "Floki Windows file search app name collision existing software",
  2026-09-04 — no hits)
- **Known adjacent collisions (UNVERIFIED — from memory, re-check):**
  Grafana **Loki** (log aggregation, different name but phonetically close);
  `Motaswitch/floki` (Docker-based build-runner CLI on GitHub); the FLOKI
  memecoin; crates.io may already have a `floki` crate.
- **Inference:** "Floki" is brandable and likely free in the desktop-search
  niche, but before locking it: check crates.io, winget/MS Store, USPTO +
  EUIPO/TMview, and de-confuse vs Grafana Loki in the README ("not
  affiliated"). Consider `floki-search` / `flokisearch.exe` as the binary
  name hedge.

## Open questions

1. MFT/USN access from Rust: validate `usn-journal-rs` (+ privilege split)
   throughput vs Everything's ~1 M files/sec-class scan before v1 scope lock.
2. `nucleo` is MPL-2.0 — confirm license compatibility, else `fuzzy-matcher`
   (MIT) or own subsequence scorer.
3. Everything IPC2 (sort/request-flags/run-count over IPC) + named instances:
   implement in v1 or v1.x? Flow/PowerToys need at least IPC1 + default
   instance.
4. Should the indexer be a real Windows Service (SYSTEM) or a
   per-user elevated `--svc` process? Service = no UAC ever, harder install;
   `--svc` = simpler, UAC at login. Everything offers both — decide for v1.
5. Content search: Tantivy sidecar (cf. ultrasearch-Rust) vs Everything-style
   unindexed grep — both deferred, but the choice shapes the pipe protocol.
6. Trademark/filed-name clearance for "Floki" (USPTO/EUIPO, MS Store).
