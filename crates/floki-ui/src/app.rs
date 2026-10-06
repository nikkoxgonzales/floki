//! eframe/egui front end: query line, virtualized results list, status line.
//!
//! The UI thread never does pipe I/O; it talks to [`crate::client::Worker`]
//! over channels, and reads row sizes/dates through [`crate::meta`].
//! [`eframe::App::logic`] (which also runs while the window is hidden) pumps
//! worker/tray/hotkey events and fires timers; `ui` only renders.
//!
//! Layout: one oversized query line with the match count and a `⋯` menu at
//! its right edge; a quiet list (no stripes, a raised band plus an amber edge
//! for the selection); a thin status line. Rows are painted directly rather
//! than built from a table widget so every pixel of the row is ours.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, CentralPanel, FontId, Key, Modifiers, Panel, RichText, TextEdit, ViewportCommand,
};
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use floki_proto::{pipe_name, HitRow, IndexState, Sort, VolumeStatus, PROTOCOL_VERSION};
use global_hotkey::hotkey::HotKey;

use crate::client::{
    FromWorker, StatusOutcome, ToWorker, Worker, MAX_LOADED_ROWS, PAGE_ROWS, TIME_SORT_ROWS,
};
use crate::meta::{MetaRequest, MetaWorker};
use crate::model::{self, RECONNECT_POLL, STATUS_POLL};
use crate::theme::{self, Palette};
use crate::{actions, menu, startup, tray};
use floki_mcp::{McpConfig, McpServer, PipeBackend};

/// Result row height.
const ROW_HEIGHT: f32 = 28.0;
/// Column header height.
const HEADER_HEIGHT: f32 = 26.0;
/// Left/right padding of the list, header, and query line.
const PAD: f32 = 14.0;
/// Keyboard page step until the list has been laid out once.
const PAGE_STEP: isize = 20;
/// Ask for the next page when the view is this many rows from the end.
const PAGE_AHEAD: usize = 200;
/// A pending search shows "Searching…" only after this long (no flicker on
/// the usual few-millisecond answer).
const SEARCHING_AFTER: Duration = Duration::from_millis(150);
/// eframe storage key for the theme choice.
const THEME_KEY: &str = "floki-theme";

/// [`FlokiApp::autostart`] values: sign-in task not queried yet / absent / present.
const AUTOSTART_UNKNOWN: u8 = 0;
const AUTOSTART_OFF: u8 = 1;
const AUTOSTART_ON: u8 = 2;

/// The global show/hide hotkey, as shown in menus and Settings (registered
/// in `main`).
const HOTKEY_TEXT: &str = "Ctrl+Alt+Space";

/// Menu → Sort by entries.
const SORTS: [(Sort, &str); 8] = [
    (Sort::NameAsc, "Name, A to Z"),
    (Sort::NameDesc, "Name, Z to A"),
    (Sort::PathAsc, "Folder, A to Z"),
    (Sort::PathDesc, "Folder, Z to A"),
    (Sort::ModifiedDesc, "Modified, newest first"),
    (Sort::ModifiedAsc, "Modified, oldest first"),
    (Sort::CreatedDesc, "Created, newest first"),
    (Sort::CreatedAsc, "Created, oldest first"),
];

/// Theme choices in menu order.
const THEMES: [theme::ThemeMode; 3] = [
    theme::ThemeMode::System,
    theme::ThemeMode::Light,
    theme::ThemeMode::Dark,
];

/// Clickable example queries on the empty screen.
const EXAMPLES: [(&str, &str); 5] = [
    ("report pdf", "names with both words, in any order"),
    ("*.pdf", "every PDF"),
    ("ext:jpg;png", "pictures, by extension"),
    ("folder: projects", "only folders"),
    (
        "path:downloads setup",
        "anything named setup under Downloads",
    ),
];

/// Help window: query syntax (see `floki_core::query`).
const SYNTAX_HELP: [(&str, &str); 11] = [
    ("report pdf", "both words, in any order"),
    ("report | invoice", "either word"),
    ("!draft", "leave out names containing draft"),
    ("\"annual report\"", "the exact phrase"),
    ("*.rs   report?.pdf", "wildcards match the whole name"),
    ("ext:jpg;png", "by extension"),
    ("path:projects\\floki", "anywhere in the full path"),
    ("folder:   file:", "only folders / only files"),
    ("wfn:readme.md", "the whole file name, exactly"),
    ("case:Readme", "case-sensitive"),
    ("regex:^v\\d+\\.log$", "a regular expression"),
];

/// Help window: keyboard shortcuts.
const SHORTCUT_HELP: [(&str, &str); 11] = [
    (HOTKEY_TEXT, "show or hide Floki from anywhere"),
    ("Enter", "open the selected result"),
    ("Ctrl+Enter", "show it in Explorer"),
    ("Ctrl+C", "copy its full path"),
    ("Del", "move it to the Recycle Bin"),
    ("Shift+F10", "more actions for it"),
    ("Up, Down, PgUp, PgDn, Home, End", "move the selection"),
    ("F2 or Ctrl+L", "jump to the search box"),
    ("Esc", "clear the search, then hide the window"),
    ("Ctrl+,", "open Settings"),
    ("F1", "this window"),
];

/// Settings window tab. `General` holds startup, theme, and keyboard;
/// `Drives` manages indexed drives and the new-drive policy; `Mcp` the
/// server that lets AI assistants search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    General,
    Drives,
    Mcp,
}

/// Pending in-app delete confirmation (Recycle Bin only, never permanent).
#[derive(Debug, Clone)]
struct DeleteTarget {
    path: String,
    name: String,
    idx: usize,
}

/// Which timestamp the date column shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DateColumn {
    Modified,
    Created,
}

impl DateColumn {
    /// Created only while sorting by it; Modified otherwise.
    fn for_sort(sort: Sort) -> Self {
        if matches!(sort, Sort::CreatedAsc | Sort::CreatedDesc) {
            DateColumn::Created
        } else {
            DateColumn::Modified
        }
    }

    fn title(self) -> &'static str {
        match self {
            DateColumn::Modified => "Modified",
            DateColumn::Created => "Created",
        }
    }

    fn of(self, hit: &HitRow) -> Option<i64> {
        match self {
            DateColumn::Modified => hit.modified_ms,
            DateColumn::Created => hit.created_ms,
        }
    }

    /// (ascending, descending) sorts behind this column's header.
    fn sorts(self) -> (Sort, Sort) {
        match self {
            DateColumn::Modified => (Sort::ModifiedAsc, Sort::ModifiedDesc),
            DateColumn::Created => (Sort::CreatedAsc, Sort::CreatedDesc),
        }
    }
}

/// Sorts that need every match's timestamps (fetched in one request, never
/// paged; see [`TIME_SORT_ROWS`]).
fn is_time_sort(sort: Sort) -> bool {
    matches!(
        sort,
        Sort::ModifiedAsc | Sort::ModifiedDesc | Sort::CreatedAsc | Sort::CreatedDesc
    )
}

/// Header click: same column flips direction; a new column starts
/// A-to-Z for names and newest-first for dates.
fn header_click(cur: Sort, asc: Sort, desc: Sort) -> Sort {
    if cur == asc {
        desc
    } else if cur == desc {
        asc
    } else if is_time_sort(asc) {
        desc
    } else {
        asc
    }
}

/// Split a row into the Name, Folder, Size, and date column rects.
fn column_rects(row: egui::Rect) -> [egui::Rect; 4] {
    const GAP: f32 = 16.0;
    const SIZE_W: f32 = 76.0;
    const DATE_W: f32 = 124.0;
    let inner = row.shrink2(egui::vec2(PAD, 0.0));
    let flexible = (inner.width() - SIZE_W - DATE_W - 3.0 * GAP).max(0.0);
    let name_w = (flexible * 0.45).clamp(flexible.min(200.0), 560.0);
    let folder_w = flexible - name_w;
    let mut x = inner.left();
    let mut take = |w: f32| {
        let r = egui::Rect::from_x_y_ranges(x..=x + w, row.y_range());
        x += w + GAP;
        r
    };
    [take(name_w), take(folder_w), take(SIZE_W), take(DATE_W)]
}

/// One line of text elided with `…` past `max_w`.
fn one_line(text: &str, size: f32, color: egui::Color32, max_w: f32) -> LayoutJob {
    let mut job = LayoutJob::single_section(
        text.to_owned(),
        TextFormat::simple(FontId::proportional(size), color),
    );
    job.wrap = TextWrapping::truncate_at_width(max_w.max(0.0));
    job
}

/// Paint `job` vertically centred in `cell`, left- or right-aligned.
fn paint_text(painter: &egui::Painter, cell: egui::Rect, job: LayoutJob, right: bool) {
    let galley = painter.layout_job(job);
    let x = if right {
        cell.right() - galley.size().x
    } else {
        cell.left()
    };
    let pos = egui::pos2(x, cell.center().y - galley.size().y / 2.0);
    painter.galley(pos, galley, egui::Color32::PLACEHOLDER);
}

/// Selection / hover background: a raised band, plus a 2 px amber edge on
/// the selected item.
fn paint_band(
    painter: &egui::Painter,
    rect: egui::Rect,
    p: Palette,
    selected: bool,
    hovered: bool,
) {
    if selected {
        painter.rect_filled(rect, 0.0, p.band);
        let edge = egui::Rect::from_min_size(rect.min, egui::vec2(2.0, rect.height()));
        painter.rect_filled(edge, 0.0, p.lantern);
    } else if hovered {
        painter.rect_filled(rect, 0.0, p.raised);
    }
}

/// Whether a row still needs its size/dates read (the service sent none).
fn needs_meta(hit: &HitRow) -> bool {
    hit.size.is_none() && hit.modified_ms.is_none() && hit.created_ms.is_none()
}

pub struct FlokiApp {
    worker: Worker,
    meta: MetaWorker,
    client_id: u64,
    hotkey_id: u32,
    tray: Option<tray::TrayHandles>,

    query: String,
    last_edit: Option<Instant>,
    search_seq: u64,
    /// The newest search request and when it was sent, until it answers.
    pending_search: Option<Instant>,
    /// A further page is on its way.
    page_pending: bool,

    hits: Vec<HitRow>,
    /// Per row: metadata already requested from [`MetaWorker`].
    meta_asked: Vec<bool>,
    /// Search sequence the rows in `hits` came from.
    hits_seq: u64,
    total: u64,
    elapsed_us: u64,
    search_error: Option<String>,
    selected: Option<usize>,
    /// Requested order (header arrow, menu radio).
    sort: Sort,
    /// Order of the rows on screen; a failed date sort reverts `sort` to it.
    shown_sort: Sort,

    connected: bool,
    status: Option<StatusOutcome>,
    status_seq: u64,
    last_status_poll: Instant,
    /// Last `Status` failure text (pipe error); shown in the down panel so
    /// `ERROR_ACCESS_DENIED`/missing-pipe stops being a mystery.
    status_error: Option<String>,

    visible: bool,
    focus_search: bool,
    /// `show_window` arms this; the next `logic()` frame sends `Focus`
    /// (sending it in the same batch as `Visible(true)` is a no-op while
    /// the window is still invisible).
    focus_pending: bool,
    /// Last observed viewport-focus state; a false→true flip re-arms
    /// `focus_search` so keystrokes land in the box after Show/Foreground.
    was_focused: bool,
    /// Service handshake result (`Request::Hello` after each connect).
    service_protocol: Option<u32>,
    service_version: Option<String>,
    elevate_error: Option<String>,
    /// True when this UI process launched the indexer (`--hidden`) and the
    /// pipe has answered since: Quit sends a graceful `Shutdown` so no
    /// console window is left behind. Set only once the worker's status
    /// poll connects after the launch (not on launch-accepted), so a child
    /// that exits 3 (AlreadyRunning) or dies on a stale flag can never
    /// make Quit shut down a foreign indexer.
    indexer_owned: bool,
    /// A launch was accepted but the pipe has not answered yet. While set,
    /// `indexer_owned` stays false; `logic()` promotes once connected.
    launch_pending: bool,
    /// When the pending launch was accepted; bounds the wait so a dead
    /// child surfaces as `elevate_error` instead of silent owned-dead state.
    launched_at: Option<Instant>,
    /// The pending launch went through the sign-in task: the indexer then
    /// belongs to the system and Quit never shuts it down.
    launch_via_task: bool,
    /// The last start or sign-in-task change was a declined UAC prompt:
    /// shown as a neutral note, never as an error.
    launch_declined: bool,
    /// Whether the sign-in task exists ([`AUTOSTART_UNKNOWN`] /
    /// [`AUTOSTART_OFF`] / [`AUTOSTART_ON`]), written by a background
    /// `schtasks` query so the window never waits on it.
    autostart: Arc<AtomicU8>,
    /// Re-query the sign-in task at this time (an elevated install or
    /// uninstall finishes a moment after its prompt is accepted).
    autostart_recheck_at: Option<Instant>,
    /// "Details" toggle on the indexer-down panel.
    show_down_details: bool,
    /// True once Quit/Exit was chosen; lets `logic()` tell a real quit
    /// apart from an X press (which hides to tray).
    quit_requested: bool,
    /// HKCU Run-key state (read from the registry at launch, the only
    /// source of truth), toggled from Settings.
    run_on_startup: bool,
    /// Settings window open state (session-only, never persisted).
    settings_open: bool,
    /// Active Settings window tab (session-only, never persisted).
    settings_tab: SettingsTab,
    /// Global NTFS-targets policy, fetched when Settings opens.
    targets_config: Option<floki_proto::TargetsConfig>,
    /// Volume awaiting the inline "Remove from the index?" confirmation.
    confirm_remove: Option<char>,
    /// Local NTFS/ReFS drives (refreshed each time Settings opens); the
    /// Drives panel offers the ones that are not indexed for adding.
    local_drives: Vec<actions::LocalDrive>,
    /// "Search syntax and shortcuts" window.
    help_open: bool,
    /// "About Floki" window.
    about_open: bool,
    /// Letters the user asked to add; shown as "Adding…" until the
    /// indexer reports them.
    adding: HashSet<char>,
    /// One-shot auto-start guard (first seconds after launch only).
    auto_launch_tried: bool,
    created_at: Instant,

    theme: theme::ThemeMode,
    /// Store `theme` on save (false while a `--theme=` override is untouched).
    save_theme: bool,
    /// Set when Shift+F10 fires; consumed by the selected row's popup.
    open_menu_key: bool,
    /// Open state of the keyboard-opened context menu.
    context_open: bool,
    /// Bring this row into view on the next frame.
    scroll_target: Option<usize>,
    /// Jump the list back to the top on the next frame (new result set).
    scroll_to_top: bool,
    /// List scroll offset and viewport height from the last frame.
    list_offset: f32,
    list_view_h: f32,
    delete_confirm: Option<DeleteTarget>,
    action_error: Option<String>,

    /// MCP server settings (`%LOCALAPPDATA%\Floki\mcp.json`, loaded by
    /// `main`; tests never touch the file).
    mcp: McpConfig,
    /// The running endpoint while `mcp.enabled` and the port bound.
    mcp_server: Option<McpServer>,
    /// Why the endpoint is not running (port taken, save failed).
    mcp_error: Option<String>,
    /// Show the start of the token in clear text.
    mcp_reveal: bool,
}

impl FlokiApp {
    pub fn new(
        hotkey: HotKey,
        tray: Option<tray::TrayHandles>,
        theme: theme::ThemeMode,
        visible: bool,
        run_on_startup: bool,
    ) -> Self {
        let worker = Worker::spawn();
        // Kick off a status probe immediately so a missing service shows the
        // "Indexer not running" panel on the first frame.
        let _ = worker.tx.send(ToWorker::Status { seq: 0 });
        Self {
            worker,
            meta: MetaWorker::spawn(),
            client_id: crate::client::make_client_id(),
            hotkey_id: hotkey.id(),
            tray,
            query: String::new(),
            last_edit: None,
            search_seq: 0,
            pending_search: None,
            page_pending: false,
            hits: Vec::new(),
            meta_asked: Vec::new(),
            hits_seq: 0,
            total: 0,
            elapsed_us: 0,
            search_error: None,
            selected: None,
            sort: Sort::NameAsc,
            shown_sort: Sort::NameAsc,
            connected: false,
            status: None,
            status_seq: 0,
            last_status_poll: Instant::now(),
            status_error: None,
            visible,
            focus_search: true,
            focus_pending: false,
            was_focused: false,
            service_protocol: None,
            service_version: None,
            elevate_error: None,
            indexer_owned: false,
            launch_pending: false,
            launched_at: None,
            launch_via_task: false,
            launch_declined: false,
            autostart: Arc::new(AtomicU8::new(AUTOSTART_UNKNOWN)),
            // First frame queries whether the sign-in task exists.
            autostart_recheck_at: Some(Instant::now()),
            show_down_details: false,
            quit_requested: false,
            run_on_startup,
            settings_open: false,
            settings_tab: SettingsTab::General,
            targets_config: None,
            confirm_remove: None,
            local_drives: Vec::new(),
            help_open: false,
            about_open: false,
            adding: HashSet::new(),
            auto_launch_tried: false,
            created_at: Instant::now(),
            theme,
            save_theme: true,
            open_menu_key: false,
            context_open: false,
            scroll_target: None,
            scroll_to_top: false,
            list_offset: 0.0,
            list_view_h: 0.0,
            delete_confirm: None,
            action_error: None,
            mcp: McpConfig::default(),
            mcp_server: None,
            mcp_error: None,
            mcp_reveal: false,
        }
    }

    /// Adopt saved MCP settings at launch (starts the server when enabled).
    pub fn set_mcp_config(&mut self, config: McpConfig) {
        self.mcp = config;
        if self.mcp.enabled {
            self.restart_mcp();
        }
    }

    /// Save the MCP settings and (re)start or stop the endpoint to match.
    fn apply_mcp(&mut self) {
        self.mcp_error = self
            .mcp
            .save()
            .err()
            .map(|e| format!("Couldn't save the MCP settings: {e}"));
        self.restart_mcp();
    }

    fn restart_mcp(&mut self) {
        // Dropping the old server releases its port before the new bind.
        self.mcp_server = None;
        if !self.mcp.enabled {
            return;
        }
        match McpServer::start(&self.mcp, Arc::new(PipeBackend)) {
            Ok(server) => self.mcp_server = Some(server),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                self.mcp_error = Some(format!(
                    "Port {} is in use by another program. Pick another port.",
                    self.mcp.port
                ));
            }
            Err(e) => self.mcp_error = Some(format!("Couldn't start the MCP server: {e}")),
        }
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        // `Focus` while invisible is a no-op (egui `ViewportCommand::Focus`
        // docs), so only unhide here and let the next `logic()` frame send
        // `Focus` via `focus_pending`.
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        self.visible = true;
        self.focus_search = true;
        self.focus_pending = true;
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        self.visible = false;
        self.focus_pending = false;
    }

    fn toggle_window(&mut self, ctx: &egui::Context) {
        if self.visible {
            self.hide_window(ctx);
        } else {
            self.show_window(ctx);
        }
    }

    /// Start a new search for the current query and sort (first page).
    fn request_search(&mut self) {
        self.search_seq += 1;
        self.last_edit = None;
        self.page_pending = false;
        if self.query.trim().is_empty() {
            // No query: show the start screen, not the last result set.
            self.hits.clear();
            self.meta_asked.clear();
            self.total = 0;
            self.elapsed_us = 0;
            self.search_error = None;
            self.selected = None;
            self.pending_search = None;
            self.shown_sort = self.sort;
            return;
        }
        let max_results = if is_time_sort(self.sort) {
            TIME_SORT_ROWS
        } else {
            PAGE_ROWS
        };
        self.pending_search = Some(Instant::now());
        let _ = self.worker.tx.send(ToWorker::Search {
            seq: self.search_seq,
            query: self.query.clone(),
            sort: self.sort,
            client_id: self.client_id,
            offset: 0,
            max_results,
        });
    }

    /// Whether more rows can be fetched by paging the shown result set.
    fn can_page(&self) -> bool {
        !is_time_sort(self.shown_sort)
            && self.hits_seq == self.search_seq
            && (self.hits.len() as u64) < self.total
            && self.hits.len() < MAX_LOADED_ROWS
    }

    /// Fetch the page after the loaded rows (same search, same order).
    fn request_page(&mut self) {
        if self.page_pending || !self.can_page() {
            return;
        }
        self.page_pending = true;
        let _ = self.worker.tx.send(ToWorker::Search {
            seq: self.search_seq,
            query: self.query.clone(),
            sort: self.shown_sort,
            client_id: self.client_id,
            offset: u32::try_from(self.hits.len()).unwrap_or(u32::MAX),
            max_results: PAGE_ROWS,
        });
    }

    fn set_sort(&mut self, sort: Sort) {
        if sort != self.sort {
            self.sort = sort;
            self.request_search();
        }
    }

    /// A search is typed (debouncing) or sent and not yet answered.
    fn search_outstanding(&self) -> bool {
        self.last_edit.is_some() || self.pending_search.is_some()
    }

    /// A search has been out long enough to say so (sooner would flash
    /// "Searching…" on every keystroke).
    fn searching(&self) -> bool {
        self.pending_search
            .is_some_and(|t| t.elapsed() >= SEARCHING_AFTER)
    }

    fn rescan_volume(&mut self, volume: Option<char>) {
        self.action_error = None;
        let _ = self.worker.tx.send(ToWorker::Rescan { volume });
    }

    /// Quit path shared by tray Quit and the menu's Exit: stop a UI-owned
    /// indexer first (the scheduled-task install is never touched), then
    /// let `logic()` allow the close instead of hiding to tray.
    fn request_quit(&mut self, ctx: &egui::Context) {
        if self.indexer_owned {
            let _ = self.worker.tx.send(ToWorker::Shutdown);
        }
        self.quit_requested = true;
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    /// Hidden elevated start (`--hidden`, no console); records a pending
    /// launch on accept so Quit stops it again only after `logic()`
    /// confirms the pipe answers. No-op while connected: launching again
    /// would UAC-prompt for a rival indexer (which `flokid` itself also
    /// refuses via its singleton mutex).
    fn start_owned_indexer(&mut self) {
        if self.connected {
            self.auto_launch_tried = true;
            self.elevate_error = None;
            return;
        }
        match actions::start_indexer_hidden() {
            Ok(kind) => {
                self.launch_pending = true;
                self.launched_at = Some(Instant::now());
                self.launch_via_task = kind == actions::IndexerLaunch::Task;
                self.launch_declined = false;
                self.elevate_error = None;
            }
            Err(e) if actions::uac_declined(&e) => {
                self.launch_declined = true;
                self.elevate_error = None;
            }
            Err(e) => {
                self.launch_declined = false;
                self.elevate_error = Some(format!("Couldn't start the indexer: {e}"));
            }
        }
    }

    /// Sign-in task state: `None` until the background query answers.
    fn autostart_installed(&self) -> Option<bool> {
        match self.autostart.load(Ordering::Relaxed) {
            AUTOSTART_ON => Some(true),
            AUTOSTART_OFF => Some(false),
            _ => None,
        }
    }

    /// Re-read whether the sign-in task exists, off the UI thread
    /// (`schtasks` takes ~50 ms).
    fn refresh_autostart(&self) {
        let slot = Arc::clone(&self.autostart);
        std::thread::spawn(move || {
            let on = actions::indexer_autostart_installed();
            slot.store(
                if on { AUTOSTART_ON } else { AUTOSTART_OFF },
                Ordering::Relaxed,
            );
        });
    }

    /// Register or remove the sign-in task (one UAC prompt). Turning it on
    /// also starts the indexer, which from then on belongs to the system:
    /// Quit no longer stops it.
    fn set_indexer_autostart(&mut self, on: bool) {
        match actions::set_indexer_autostart(on) {
            Ok(()) => {
                self.launch_declined = false;
                self.elevate_error = None;
                self.autostart.store(
                    if on { AUTOSTART_ON } else { AUTOSTART_OFF },
                    Ordering::Relaxed,
                );
                self.autostart_recheck_at = Some(Instant::now() + Duration::from_secs(4));
                if on {
                    self.indexer_owned = false;
                    if !self.connected {
                        self.launch_pending = true;
                        self.launched_at = Some(Instant::now());
                        self.launch_via_task = true;
                    }
                }
            }
            Err(e) if actions::uac_declined(&e) => self.launch_declined = true,
            Err(e) => {
                self.elevate_error = Some(format!("Couldn't change the sign-in task: {e}"));
            }
        }
    }
    /// Run-on-startup write-through (Settings window General tab). Writes
    /// the HKCU Run key immediately; on failure the shown state is
    /// unchanged and `action_error` surfaces it.
    fn set_run_on_startup(&mut self, on: bool) {
        match startup::set_enabled(on) {
            Ok(()) => self.run_on_startup = on,
            Err(e) => {
                self.action_error = Some(format!("Startup toggle failed: {e}"));
            }
        }
    }
    /// Open Settings on `tab`, fetching the NTFS policy snapshot and the
    /// local drive list so the Drives panel is current the first frame.
    fn open_settings(&mut self, tab: SettingsTab) {
        self.settings_tab = tab;
        self.settings_open = true;
        self.confirm_remove = None;
        self.refresh_autostart();
        self.local_drives = actions::local_indexable_drives();
        let _ = self.worker.tx.send(ToWorker::TargetsConfigGet);
        self.refresh_status_now();
    }
    /// Make `logic()` poll status on its next frame (after a volume op).
    fn refresh_status_now(&mut self) {
        self.last_status_poll = Instant::now() - model::STATUS_POLL;
    }
    /// Apply `edit` to the cached status row of `letter` so a toggled
    /// checkbox shows its new state at once instead of snapping back until
    /// the next status poll confirms it.
    fn patch_volume(&mut self, letter: char, edit: impl FnOnce(&mut VolumeStatus)) {
        if let Some(v) = self
            .status
            .as_mut()
            .and_then(|s| s.volumes.iter_mut().find(|v| v.letter == letter))
        {
            edit(v);
        }
    }

    /// The `⋯` menu: actions on the selection, view options, the indexer,
    /// help, and leaving.
    fn main_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(230.0);
        let ctx = ui.ctx().clone();
        let has_selection = self.selected_hit().is_some();
        for item in [menu::Item::Open, menu::Item::Reveal, menu::Item::CopyPath] {
            let (label, shortcut) = item.label();
            if ui
                .add_enabled(
                    has_selection,
                    egui::Button::new(label).shortcut_text(shortcut),
                )
                .clicked()
            {
                self.apply_menu_item(item, &ctx);
                ui.close();
            }
        }
        ui.separator();
        ui.menu_button("Sort by", |ui| {
            for (sort, label) in SORTS {
                if ui.radio(self.sort == sort, label).clicked() {
                    self.set_sort(sort);
                    ui.close();
                }
            }
        });
        ui.menu_button("Theme", |ui| {
            for mode in THEMES {
                if ui.radio(self.theme == mode, mode.label()).clicked() {
                    self.set_theme(ui.ctx(), mode);
                    ui.close();
                }
            }
        });
        ui.separator();
        ui.label(RichText::new(self.indexer_summary()).small().weak());
        if !self.connected
            && ui
                .add_enabled(!self.launch_pending, egui::Button::new("Start the indexer"))
                .clicked()
        {
            self.start_owned_indexer();
            ui.close();
        }
        if ui
            .add_enabled(self.connected, egui::Button::new("Rescan all drives"))
            .on_hover_text("Re-read every indexed drive from scratch (takes a while)")
            .clicked()
        {
            self.rescan_volume(None);
            ui.close();
        }
        if ui.button("Drives…").clicked() {
            self.open_settings(SettingsTab::Drives);
            ui.close();
        }
        if ui
            .add(egui::Button::new("Settings…").shortcut_text("Ctrl+,"))
            .clicked()
        {
            self.open_settings(SettingsTab::General);
            ui.close();
        }
        ui.separator();
        if ui
            .add(egui::Button::new("Search syntax and shortcuts").shortcut_text("F1"))
            .clicked()
        {
            self.help_open = true;
            ui.close();
        }
        if ui.button("About Floki").clicked() {
            self.about_open = true;
            ui.close();
        }
        ui.separator();
        if ui
            .add(egui::Button::new("Hide to tray").shortcut_text(HOTKEY_TEXT))
            .clicked()
        {
            self.hide_window(&ctx);
            ui.close();
        }
        if ui
            .button("Exit")
            .on_hover_text("Close Floki. An indexer started by the sign-in task keeps running")
            .clicked()
        {
            self.request_quit(&ctx);
            ui.close();
        }
    }

    /// One-line indexer state for the menu.
    fn indexer_summary(&self) -> String {
        if !self.connected {
            return if self.launch_pending {
                "Indexer starting…"
            } else {
                "Indexer not running"
            }
            .to_owned();
        }
        match self.status.as_ref().map(|s| (&s.state, s.entries)) {
            None => "Connecting to the indexer…".to_owned(),
            Some((IndexState::Ready, entries)) => {
                format!("Indexer running, {} files", model::format_count(entries))
            }
            Some((IndexState::Loading, _)) => "Loading the index…".to_owned(),
            Some((IndexState::Scanning { volume, done }, _)) => format!(
                "Indexing {volume}: {} items so far",
                model::format_count(*done)
            ),
        }
    }

    /// `--theme=` chose this run's theme: don't store it over the user's.
    pub fn keep_stored_theme(&mut self) {
        self.save_theme = false;
    }

    fn set_theme(&mut self, ctx: &egui::Context, mode: theme::ThemeMode) {
        self.save_theme = true;
        self.theme = mode;
        theme::apply(ctx, mode);
    }

    /// Command-line extras from `main`: `--settings[=drives]` opens
    /// Settings, `--search=<text>` starts with that query.
    pub fn launch(&mut self, settings: Option<SettingsTab>, query: Option<String>) {
        if let Some(tab) = settings {
            self.open_settings(tab);
        }
        if let Some(q) = query {
            self.query = q;
            self.last_edit = Some(Instant::now());
        }
    }

    /// Help windows: search syntax + shortcuts, and About.
    fn help_windows(&mut self, ctx: &egui::Context) {
        egui::Window::new("Search syntax and shortcuts")
            .open(&mut self.help_open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                section(ui, "Search");
                caption(ui, "Plain words match anywhere in a name, ignoring case.");
                ui.add_space(4.0);
                help_grid(ui, "help-syntax", &SYNTAX_HELP);
                section(ui, "Keyboard");
                help_grid(ui, "help-keys", &SHORTCUT_HELP);
            });
        let service = match (&self.service_version, self.connected) {
            (Some(v), true) => format!("Indexer {v}"),
            _ => "Indexer not running".to_owned(),
        };
        egui::Window::new("About Floki")
            .open(&mut self.about_open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!("Floki {}", env!("CARGO_PKG_VERSION")))
                        .family(theme::semibold())
                        .size(18.0),
                );
                ui.label("Instant file-name search for NTFS and ReFS drives.");
                ui.add_space(6.0);
                caption(ui, &service);
            });
    }

    /// Settings window: tab list on the left, the tab's panel on the
    /// right, Close at the bottom. Changes apply immediately (no OK/Cancel).
    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.settings_open;
        // Fixed body height (fits the 400 px minimum window; scrolls past
        // it): the vertical tab separator would otherwise stretch the window
        // to the screen and push Close out of view, and tabs would resize it.
        let body_h = (ctx.content_rect().height() - 150.0).clamp(200.0, 420.0);
        let p = theme::palette(ctx);
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    ui.set_height(body_h);
                    ui.vertical(|ui| {
                        ui.set_width(104.0);
                        ui.spacing_mut().item_spacing.y = 2.0;
                        for (tab, label) in [
                            (SettingsTab::General, "General"),
                            (SettingsTab::Drives, "Drives"),
                            (SettingsTab::Mcp, "MCP"),
                        ] {
                            if nav_item(ui, label, self.settings_tab == tab, p) {
                                self.settings_tab = tab;
                            }
                        }
                    });
                    ui.add(egui::Separator::default().vertical().spacing(16.0));
                    egui::ScrollArea::vertical()
                        .id_salt("settings-body")
                        .max_height(body_h)
                        .auto_shrink([true, false])
                        .show(ui, |ui| {
                            // The row's horizontal layout would carry into
                            // the scroll area otherwise.
                            ui.vertical(|ui| {
                                ui.set_width(500.0);
                                match self.settings_tab {
                                    SettingsTab::General => self.general_panel(ui),
                                    SettingsTab::Drives => self.drives_panel(ui),
                                    SettingsTab::Mcp => self.mcp_panel(ui),
                                }
                            });
                        });
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if let Some(e) = &self.action_error {
                        ui.add(egui::Label::new(RichText::new(e).color(p.error)).truncate());
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            self.settings_open = false;
                        }
                    });
                });
            });
        self.settings_open = open && self.settings_open;
    }

    /// General tab: startup, appearance, keyboard.
    fn general_panel(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui.ctx());
        section(ui, "Startup");
        let mut on = self.run_on_startup;
        if ui.checkbox(&mut on, "Open Floki at sign-in").changed() {
            self.set_run_on_startup(on);
        }
        caption(
            ui,
            &format!("Starts in the tray, out of the way. {HOTKEY_TEXT} shows it."),
        );
        ui.add_space(6.0);
        let task = self.autostart_installed();
        let mut task_on = task.unwrap_or(false);
        if ui
            .add_enabled(
                task.is_some(),
                egui::Checkbox::new(&mut task_on, "Start the indexer at sign-in"),
            )
            .changed()
        {
            self.set_indexer_autostart(task_on);
        }
        caption(
            ui,
            "Keeps search ready from sign-in, with no administrator prompt each time. \
             Turning this on or off asks for administrator permission once.",
        );
        if self.launch_declined {
            caption(
                ui,
                "Administrator permission was declined; nothing changed.",
            );
        }
        if let Some(e) = &self.elevate_error {
            ui.label(RichText::new(e).color(p.error));
        }

        section(ui, "Appearance");
        ui.horizontal(|ui| {
            for mode in THEMES {
                if ui.radio(self.theme == mode, mode.label()).clicked() {
                    self.set_theme(ui.ctx(), mode);
                }
            }
        });

        section(ui, "Keyboard");
        egui::Grid::new("settings-keys")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.label("Show or hide Floki");
                ui.label(RichText::new(HOTKEY_TEXT).color(p.muted));
                ui.end_row();
                ui.label("Settings");
                ui.label(RichText::new("Ctrl+,").color(p.muted));
                ui.end_row();
            });
        ui.add_space(4.0);
        if ui.link("All shortcuts and search syntax").clicked() {
            self.help_open = true;
        }
    }

    /// Drives tab: indexed drives as a table (state, live updates, search
    /// visibility, Rescan / Remove), local drives that are not indexed (Add),
    /// and the new-drive policy. Toggles show their new state immediately;
    /// the next status poll confirms it.
    fn drives_panel(&mut self, ui: &mut egui::Ui) {
        let refs: Vec<char> = self
            .local_drives
            .iter()
            .filter(|d| d.refs)
            .map(|d| d.letter)
            .collect();
        let is_refs = |letter: char| refs.contains(&letter);
        let scanning = match self.status.as_ref().map(|s| &s.state) {
            Some(IndexState::Scanning { volume, done }) => Some((*volume, *done)),
            _ => None,
        };
        let volumes = self
            .status
            .as_ref()
            .map(|s| s.volumes.clone())
            .unwrap_or_default();
        self.adding
            .retain(|l| !volumes.iter().any(|v| v.letter == *l));

        section(ui, "Indexed drives");
        if self.status.is_none() {
            caption(ui, "Waiting for the indexer…");
        } else if volumes.is_empty() {
            caption(ui, "No drives are indexed yet. Add one below.");
        } else {
            // The Settings body scrolls, so the grid needs no scroll area.
            egui::Grid::new("drives-grid")
                .num_columns(6)
                .spacing([14.0, 8.0])
                .show(ui, |ui| {
                    for title in ["Drive", "Files", "State", "Live", "In results", ""] {
                        ui.label(RichText::new(title).small().weak());
                    }
                    ui.end_row();
                    for v in &volumes {
                        self.drive_row(ui, v, is_refs(v.letter), scanning);
                        ui.end_row();
                    }
                });
        }
        if let Some(letter) = self.confirm_remove {
            ui.add_space(6.0);
            ui.label(format!(
                "Remove {letter}: from the index? Its files leave search results and it \
                 is not added back automatically (Add below brings it back)."
            ));
            ui.horizontal(|ui| {
                if ui.button(format!("Remove {letter}:")).clicked() {
                    self.action_error = None;
                    let _ = self
                        .worker
                        .tx
                        .send(ToWorker::RemoveVolume { volume: letter });
                    if let Some(s) = self.status.as_mut() {
                        s.volumes.retain(|v| v.letter != letter);
                    }
                    self.confirm_remove = None;
                }
                if ui.button("Cancel").clicked() {
                    self.confirm_remove = None;
                }
            });
        }
        if volumes.len() > 1 {
            ui.add_space(6.0);
            if ui
                .button("Rescan all")
                .on_hover_text("Re-read every indexed drive from scratch (takes a while)")
                .clicked()
            {
                self.rescan_volume(None);
            }
        }

        let not_indexed: Vec<char> = self
            .local_drives
            .iter()
            .map(|d| d.letter)
            .filter(|l| !volumes.iter().any(|v| v.letter == *l))
            .collect();
        if self.status.is_some() && !not_indexed.is_empty() {
            section(ui, "Other drives on this PC");
            for letter in not_indexed {
                ui.horizontal(|ui| {
                    ui.label(format!("{letter}:"));
                    if is_refs(letter) {
                        ui.label(RichText::new("ReFS").small().weak());
                    }
                    let busy = self.adding.contains(&letter);
                    let text = if busy { "Adding…" } else { "Add" };
                    if ui
                        .add_enabled(!busy && self.connected, egui::Button::new(text))
                        .on_hover_text(if is_refs(letter) {
                            "Index this drive and keep it up to date. ReFS: turns on its \
                             change journal, and the first scan walks every folder, so it \
                             takes longer than on NTFS"
                        } else {
                            "Index this drive and keep it up to date"
                        })
                        .clicked()
                    {
                        self.adding.insert(letter);
                        self.rescan_volume(Some(letter));
                    }
                });
            }
        }

        section(ui, "New drives");
        if let Some(mut cfg) = self.targets_config {
            let mut changed = false;
            changed |= ui
                .checkbox(
                    &mut cfg.auto_include_fixed,
                    "Index new internal drives automatically",
                )
                .changed();
            changed |= ui
                .checkbox(
                    &mut cfg.auto_include_removable,
                    "Index USB sticks and SD cards automatically",
                )
                .changed();
            changed |= ui
                .checkbox(
                    &mut cfg.auto_remove_offline,
                    "Drop drives that stay disconnected (after 30 s)",
                )
                .changed();
            if changed {
                self.targets_config = Some(cfg);
                let _ = self.worker.tx.send(ToWorker::TargetsConfigSet {
                    auto_include_fixed: cfg.auto_include_fixed,
                    auto_include_removable: cfg.auto_include_removable,
                    auto_remove_offline: cfg.auto_remove_offline,
                });
            }
        } else {
            caption(ui, "Loading…");
        }
    }

    /// MCP tab: serve on/off, who can connect, port, token, and copyable
    /// client setups. Every change applies (and saves) at once.
    fn mcp_panel(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui.ctx());
        section(ui, "MCP server");
        caption(
            ui,
            "Lets AI assistants that speak MCP (Claude, Cursor, and others) search your file \
             names through Floki. They see names, folders, sizes, and dates, never file \
             contents.",
        );
        ui.add_space(6.0);
        let mut changed = ui
            .checkbox(&mut self.mcp.enabled, "Serve MCP while Floki is open")
            .changed();

        section(ui, "Who can connect");
        changed |= ui
            .radio_value(&mut self.mcp.lan, false, "This PC only")
            .changed();
        changed |= ui
            .radio_value(&mut self.mcp.lan, true, "Other devices on my network too")
            .changed();
        if self.mcp.lan {
            ui.label(
                RichText::new(
                    "Anyone on your network who has the token can search your file names, \
                     and the connection is not encrypted. Use this on networks you trust. \
                     Windows may ask to let Floki through the firewall.",
                )
                .small()
                .color(p.lantern),
            );
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Port");
            let port = ui.add(egui::DragValue::new(&mut self.mcp.port).range(1024..=65535));
            if port.drag_stopped() || port.lost_focus() {
                changed = true;
            }
        });

        section(ui, "Token");
        caption(
            ui,
            "Clients send it as \"Authorization: Bearer <token>\". Treat it like a password.",
        );
        ui.horizontal(|ui| {
            let shown = if self.mcp_reveal {
                format!("{}…", self.mcp.token.get(..16).unwrap_or(""))
            } else {
                "•".repeat(16)
            };
            ui.label(RichText::new(shown).color(p.muted))
                .on_hover_text("The first 16 of 64 characters");
            let reveal = if self.mcp_reveal { "Hide" } else { "Show" };
            if ui.small_button(reveal).clicked() {
                self.mcp_reveal = !self.mcp_reveal;
            }
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(self.mcp.token.clone());
            }
            if ui
                .small_button("New token")
                .on_hover_text("Clients using the old token stop working")
                .clicked()
            {
                self.mcp.token = floki_mcp::config::new_token();
                changed = true;
            }
        });

        if changed {
            self.apply_mcp();
        }

        ui.add_space(8.0);
        if let Some(e) = &self.mcp_error {
            ui.label(RichText::new(e).color(p.error));
        } else if let Some(server) = &self.mcp_server {
            let url = self.mcp.local_url();
            let text = if self.mcp.lan {
                format!(
                    "Listening on port {} on every network. From this PC: {url}",
                    server.addr().port()
                )
            } else {
                format!("Listening at {url}")
            };
            ui.label(RichText::new(text).color(p.lantern));
        } else {
            caption(ui, "Off.");
        }

        section(ui, "Connect a client");
        let url = self.mcp.local_url();
        let token = self.mcp.token.clone();
        let flk = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("flk.exe")))
            .map_or_else(|| "flk.exe".to_owned(), |p| p.display().to_string());
        let json = serde_json::json!({
            "mcpServers": { "floki": {
                "type": "http",
                "url": url,
                "headers": { "Authorization": format!("Bearer {token}") }
            } }
        });
        let setups = [
            (
                "Copy Claude Code command",
                format!(
                    "claude mcp add --transport http floki {url} \
                     --header \"Authorization: Bearer {token}\""
                ),
            ),
            ("Copy JSON config", json.to_string()),
            (
                "Copy local command (no port, no token)",
                format!("claude mcp add floki -- \"{flk}\" mcp"),
            ),
        ];
        for (label, text) in setups {
            if ui.button(label).clicked() {
                ui.ctx().copy_text(text);
            }
        }
        ui.add_space(2.0);
        caption(
            ui,
            "The local command runs Floki's MCP over stdin/stdout for assistants on this PC; \
             it works even when the server above is off. Either way, the indexer must be running.",
        );
    }

    /// One indexed-drive row of the Drives grid (six cells).
    fn drive_row(
        &mut self,
        ui: &mut egui::Ui,
        v: &VolumeStatus,
        refs: bool,
        scanning: Option<(char, u64)>,
    ) {
        let p = theme::palette(ui.ctx());
        let letter = v.letter;
        ui.horizontal(|ui| {
            ui.label(format!("{letter}:"));
            if refs {
                ui.label(RichText::new("ReFS").small().weak());
            }
        });
        ui.label(model::format_count(v.entries));
        match scanning {
            Some((l, done)) if l == letter => {
                ui.label(RichText::new("indexing").color(p.lantern))
                    .on_hover_text(format!("{} items read so far", model::format_count(done)));
            }
            _ => {
                ui.label(volume_state(v));
            }
        }
        let mut monitor = v.monitor;
        if ui
            .checkbox(&mut monitor, "")
            .on_hover_text("Keep this drive up to date as files change. Off: scanned once")
            .changed()
        {
            self.patch_volume(letter, |v| v.monitor = monitor);
            let _ = self.worker.tx.send(ToWorker::SetVolumeMonitor {
                volume: letter,
                monitor,
            });
        }
        let mut enabled = v.enabled;
        if ui
            .checkbox(&mut enabled, "")
            .on_hover_text("Show this drive's files in search results")
            .changed()
        {
            self.patch_volume(letter, |v| v.enabled = enabled);
            let _ = self.worker.tx.send(ToWorker::SetVolumeEnabled {
                volume: letter,
                enabled,
            });
        }
        ui.horizontal(|ui| {
            if ui
                .small_button("Rescan")
                .on_hover_text("Re-read this drive from scratch")
                .clicked()
            {
                self.rescan_volume(Some(letter));
            }
            let remove = ui.add_enabled(scanning.is_none(), egui::Button::new("Remove…").small());
            let remove = if let Some((l, _)) = scanning {
                remove.on_disabled_hover_text(format!("Available once the scan of {l}: finishes"))
            } else {
                remove.on_hover_text("Drop this drive from the index")
            };
            if remove.clicked() {
                self.confirm_remove = Some(letter);
            }
        });
    }
    fn selected_hit(&self) -> Option<(usize, HitRow, String)> {
        let idx = self.selected?;
        let hit = self.hits.get(idx)?.clone();
        let full = model::join_path(&hit.path, &hit.name);
        Some((idx, hit, full))
    }

    fn open_selected(&self) {
        if let Some((_, _, path)) = self.selected_hit() {
            if let Err(e) = actions::open_file(&path) {
                tracing::warn!("open failed for {path}: {e}");
            }
        }
    }

    fn move_sel(&mut self, delta: isize) {
        self.selected = model::move_selection(self.selected, delta, self.hits.len());
        self.scroll_target = self.selected;
    }

    fn move_sel_to(&mut self, idx: usize) {
        if self.hits.is_empty() {
            self.selected = None;
        } else {
            self.selected = Some(idx.min(self.hits.len() - 1));
        }
        self.scroll_target = self.selected;
    }

    /// Rows that fit in the list (keyboard page step).
    fn page_step(&self) -> isize {
        let rows = (self.list_view_h / ROW_HEIGHT).floor() as isize;
        if rows > 1 {
            rows - 1
        } else {
            PAGE_STEP
        }
    }

    /// Apply a context-menu (or shortcut) action to the selected row.
    /// `copy` needs the egui context; everything else is plain Win32.
    /// The item→effect mapping itself is the pure [`menu::dispatch`].
    fn apply_menu_item(&mut self, item: menu::Item, ctx: &egui::Context) {
        let Some((idx, hit, path)) = self.selected_hit() else {
            return;
        };
        let action = menu::dispatch(item, &path, &hit.name, idx);
        self.action_error = None;
        match action {
            menu::Action::OpenFile(p) => {
                if let Err(e) = actions::open_file(&p) {
                    tracing::warn!("open failed for {p}: {e}");
                }
            }
            menu::Action::OpenWith(p) => {
                if let Err(e) = actions::open_with(&p) {
                    self.action_error = Some(format!("Open with failed: {e}"));
                }
            }
            menu::Action::Reveal(p) => {
                if let Err(e) = actions::open_containing_folder(&p) {
                    self.action_error = Some(format!("Reveal failed: {e}"));
                }
            }
            menu::Action::CopyText(text) => ctx.copy_text(text),
            menu::Action::RunAsAdmin(p) => {
                if let Err(e) = actions::run_as_admin(&p) {
                    self.action_error = Some(format!("Run as admin failed: {e}"));
                }
            }
            menu::Action::ShowProperties(p) => {
                if let Err(e) = actions::show_properties(&p) {
                    self.action_error = Some(format!("Properties failed: {e}"));
                }
            }
            menu::Action::RequestDelete { path, name, idx } => {
                self.delete_confirm = Some(DeleteTarget { path, name, idx });
            }
        }
    }

    /// Remove a recycled row locally (the indexer confirms via the journal).
    fn drop_row(&mut self, idx: usize, path: &str) {
        let at_idx = self
            .hits
            .get(idx)
            .is_some_and(|h| model::join_path(&h.path, &h.name) == path);
        let pos = if at_idx {
            idx
        } else if let Some(pos) = self
            .hits
            .iter()
            .position(|h| model::join_path(&h.path, &h.name) == path)
        {
            pos
        } else {
            return;
        };
        self.hits.remove(pos);
        if pos < self.meta_asked.len() {
            self.meta_asked.remove(pos);
        }
        self.total = self.total.saturating_sub(1);
        self.selected = if self.hits.is_empty() {
            None
        } else {
            Some(idx.min(self.hits.len() - 1))
        };
    }

    /// Install a search answer: offset 0 replaces the list, a later page
    /// appends when it lines up with the rows already loaded.
    fn apply_results(&mut self, seq: u64, o: crate::client::SearchOutcome) {
        self.total = o.total;
        self.elapsed_us = o.elapsed_us;
        self.search_error = None;
        if o.offset == 0 {
            self.pending_search = None;
            self.page_pending = false;
            self.hits = o.hits;
            self.meta_asked = vec![false; self.hits.len()];
            self.hits_seq = seq;
            self.meta.set_current(seq);
            self.shown_sort = self.sort;
            self.selected = if self.hits.is_empty() { None } else { Some(0) };
            self.scroll_target = None;
            self.scroll_to_top = true;
        } else {
            self.page_pending = false;
            if o.offset as usize == self.hits.len() && seq == self.hits_seq {
                self.meta_asked
                    .resize(self.hits.len() + o.hits.len(), false);
                self.hits.extend(o.hits);
            }
        }
    }

    /// A search or page failed. The rows on screen stay; a refused date
    /// sort puts the header arrow back on their order.
    fn search_failed(&mut self, e: String) {
        self.pending_search = None;
        self.page_pending = false;
        self.sort = self.shown_sort;
        self.search_error = Some(e);
    }

    fn drain_worker(&mut self) {
        while let Ok(ev) = self.worker.rx.try_recv() {
            match ev {
                FromWorker::ServiceUp => self.connected = true,
                FromWorker::ServiceDown => {
                    self.connected = false;
                    self.last_status_poll = Instant::now();
                }
                FromWorker::SearchDone { seq, result } => {
                    if model::is_stale(seq, self.search_seq) {
                        continue;
                    }
                    match result {
                        Ok(o) => self.apply_results(seq, o),
                        Err(e) => self.search_failed(e),
                    }
                }
                FromWorker::HelloDone {
                    protocol,
                    service_version,
                } => {
                    self.service_protocol = Some(protocol);
                    self.service_version = Some(service_version);
                }
                FromWorker::StatusDone { seq, result } => {
                    if seq != self.status_seq {
                        continue;
                    }
                    match result {
                        Ok(s) => {
                            self.connected = true;
                            let refresh = model::should_refresh_search(
                                self.status.as_ref().map(|p| &p.state),
                                &s.state,
                                !self.query.trim().is_empty(),
                                self.hits.is_empty(),
                            );
                            self.status = Some(s);
                            self.status_error = None;
                            if refresh {
                                self.request_search();
                            }
                        }
                        Err(e) => {
                            self.connected = false;
                            self.status_error = Some(e);
                        }
                    }
                }
                FromWorker::RescanDone(result) => {
                    if result.is_err() {
                        self.adding.clear();
                    }
                    self.action_error = result.err().map(|e| format!("Rescan failed: {e}"));
                    self.refresh_status_now();
                }
                FromWorker::VolumeOpDone(result) => {
                    self.action_error = result.err().map(|e| format!("Volume update failed: {e}"));
                    self.refresh_status_now();
                }
                FromWorker::TargetsConfigDone(result) => match result {
                    Ok(cfg) => self.targets_config = Some(cfg),
                    Err(e) => {
                        self.action_error = Some(format!("Targets policy failed: {e}"));
                    }
                },
                FromWorker::ShutdownDone(result) => match result {
                    Ok(()) => self.indexer_owned = false,
                    Err(e) => {
                        self.action_error = Some(format!("Indexer shutdown failed: {e}"));
                    }
                },
            }
        }
    }

    /// Fill in sizes and dates read by the meta thread (rows are matched by
    /// path, so a row dropped in the meantime cannot get a neighbour's data).
    fn drain_meta(&mut self) {
        while let Ok(reply) = self.meta.rx.try_recv() {
            if reply.seq != self.hits_seq {
                continue;
            }
            let Some(meta) = reply.meta else { continue };
            if let Some(hit) = self.hits.get_mut(reply.idx) {
                if model::join_path(&hit.path, &hit.name) == reply.path {
                    hit.size = if hit.is_dir { None } else { meta.size };
                    hit.modified_ms = meta.modified_ms;
                    hit.created_ms = meta.created_ms;
                }
            }
        }
    }

    fn drain_external(&mut self, ctx: &egui::Context) {
        for action in tray::pump_tray(self.tray.as_ref()) {
            match action {
                tray::TrayAction::Show => self.show_window(ctx),
                tray::TrayAction::Settings => {
                    self.show_window(ctx);
                    self.open_settings(SettingsTab::General);
                }
                tray::TrayAction::Quit => self.request_quit(ctx),
            }
        }
        while let Ok(ev) = global_hotkey::GlobalHotKeyEvent::receiver().try_recv() {
            if ev.id() == self.hotkey_id && ev.state() == global_hotkey::HotKeyState::Pressed {
                self.toggle_window(ctx);
            }
        }
    }

    /// Indexed file count and service memory for the status line's right
    /// edge; `None` until the first status answer.
    fn index_text(&self) -> Option<(String, String)> {
        let s = self.status.as_ref()?;
        Some((
            format!("{} files indexed", model::format_count(s.entries)),
            model::format_rss_mb(s.rss_bytes),
        ))
    }

    /// "Indexing G: …" while the index is incomplete, `None` once ready.
    fn indexing_notice(&self) -> Option<String> {
        model::indexing_notice(&self.status.as_ref()?.state)
    }

    /// Note when the list holds fewer rows than matched and scrolling will
    /// not fetch the rest.
    fn truncation_note(&self) -> Option<String> {
        let loaded = self.hits.len() as u64;
        if self.hits.is_empty() || loaded >= self.total || self.can_page() {
            return None;
        }
        let by = if is_time_sort(self.shown_sort) {
            " by date"
        } else {
            ""
        };
        Some(format!(
            "Showing the first {}{by}. Narrow the search to reach the other {}.",
            model::format_count(loaded),
            model::format_count(self.total - loaded)
        ))
    }

    fn version_mismatch(&self) -> Option<String> {
        if let Some(p) = self.service_protocol {
            if p != PROTOCOL_VERSION {
                let svc = self.service_version.as_deref().unwrap_or("unknown");
                return Some(format!(
                    "The indexer speaks protocol {p} ({svc}) but this window expects \
                     {PROTOCOL_VERSION}. Restart the indexer after updating Floki."
                ));
            }
        }
        None
    }

    /// Shown instead of results while no indexer answers.
    fn down_panel(&mut self, ui: &mut egui::Ui, p: Palette) {
        let top = (ui.available_height() * 0.2).clamp(24.0, 140.0);
        ui.add_space(top);
        centered_column(ui, 440.0, |ui| {
            ui.label(
                RichText::new("The indexer isn't running")
                    .family(theme::semibold())
                    .size(theme::QUERY_SIZE),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Floki searches through a background indexer. It reads each drive's \
                     change journal, which needs administrator rights.",
                )
                .color(p.muted),
            );
            ui.add_space(18.0);
            let task = self.autostart_installed();
            if self.launch_pending {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Starting the indexer…");
                });
            } else {
                let start = ui
                    .add(
                        egui::Button::new(RichText::new("Start the indexer").color(p.base))
                            .fill(p.lantern)
                            .min_size(egui::vec2(0.0, 32.0)),
                    )
                    .on_hover_text(if task == Some(true) {
                        "Starts through the sign-in task, with no prompt"
                    } else {
                        "Asks for administrator permission"
                    });
                if start.clicked() {
                    self.start_owned_indexer();
                }
                if task == Some(false) {
                    ui.add_space(14.0);
                    if ui
                        .add(
                            egui::Button::new("Start it at every sign-in")
                                .min_size(egui::vec2(0.0, 30.0)),
                        )
                        .clicked()
                    {
                        self.set_indexer_autostart(true);
                    }
                    ui.add_space(2.0);
                    caption(
                        ui,
                        "Asks for administrator permission once. After that the indexer \
                         starts with Windows and never prompts again.",
                    );
                }
            }
            if self.launch_declined {
                ui.add_space(10.0);
                ui.label(
                    RichText::new(
                        "Administrator permission was declined, so the indexer didn't start.",
                    )
                    .color(p.muted),
                );
            }
            if let Some(e) = &self.elevate_error {
                ui.add_space(10.0);
                ui.label(RichText::new(e).color(p.error));
            }
            ui.add_space(20.0);
            let toggle = if self.show_down_details {
                "Hide details"
            } else {
                "Details"
            };
            if ui
                .link(RichText::new(toggle).small().color(p.lantern))
                .clicked()
            {
                self.show_down_details = !self.show_down_details;
            }
            if self.show_down_details {
                let detail = match &self.status_error {
                    Some(e) => format!("{}: {e}", pipe_name()),
                    None => format!("{}: did not answer", pipe_name()),
                };
                caption(ui, &detail);
                caption(
                    ui,
                    &format!("Checking again every {}s.", RECONNECT_POLL.as_secs()),
                );
            }
        });
    }

    /// Query line: the oversized search field, then the match count, a
    /// clear button, and the `⋯` menu at the right edge. Returns the
    /// field's id.
    fn query_line(&mut self, ui: &mut egui::Ui, p: Palette) -> egui::Id {
        let frame = egui::Frame::new()
            .fill(p.base)
            .inner_margin(egui::Margin::symmetric(PAD as i8, 10));
        Panel::top("query")
            .frame(frame)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    const RIGHT_W: f32 = 200.0;
                    let edit_w = (ui.available_width() - RIGHT_W).max(160.0);
                    let edit = TextEdit::singleline(&mut self.query)
                        .frame(egui::Frame::NONE)
                        .font(FontId::new(theme::QUERY_SIZE, theme::semibold()))
                        .hint_text(
                            RichText::new("Search files and folders")
                                .size(theme::QUERY_SIZE)
                                .color(p.muted),
                        )
                        .margin(egui::Margin::ZERO)
                        .desired_width(edit_w);
                    let resp = ui.add_sized(egui::vec2(edit_w, 34.0), edit);
                    if self.focus_search {
                        resp.request_focus();
                        self.focus_search = false;
                    }
                    if resp.changed() {
                        self.last_edit = Some(Instant::now());
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let dots = menu_dots(ui, p);
                        egui::Popup::menu(&dots).show(|ui| self.main_menu(ui));
                        if !self.query.is_empty()
                            && ui
                                .add(
                                    egui::Button::new(RichText::new("×").size(18.0).color(p.muted))
                                        .frame_when_inactive(false),
                                )
                                .on_hover_text("Clear (Esc)")
                                .clicked()
                        {
                            self.query.clear();
                            self.request_search();
                            self.focus_search = true;
                        }
                        let count = if self.query.trim().is_empty() {
                            String::new()
                        } else if self.searching() {
                            "Searching…".to_owned()
                        } else {
                            model::match_count_text(self.total)
                        };
                        ui.label(RichText::new(count).color(p.muted));
                    });
                    resp.id
                })
                .inner
            })
            .inner
    }

    /// Start screen: what is searchable, and example queries to click.
    fn start_screen(&mut self, ui: &mut egui::Ui, p: Palette) {
        ui.add_space(28.0);
        let scope = match &self.status {
            Some(s) => {
                let drives: Vec<String> = s
                    .volumes
                    .iter()
                    .filter(|v| v.enabled)
                    .map(|v| format!("{}:", v.letter))
                    .collect();
                format!(
                    "Search {} files on {}",
                    model::format_count(s.entries),
                    drives.join(" ")
                )
            }
            None => "Connecting to the indexer…".to_owned(),
        };
        let mut picked = None;
        ui.horizontal(|ui| {
            ui.add_space(PAD);
            ui.vertical(|ui| {
                ui.label(RichText::new(scope).size(18.0));
                ui.add_space(14.0);
                egui::Grid::new("examples")
                    .num_columns(2)
                    .spacing([20.0, 8.0])
                    .show(ui, |ui| {
                        for (q, what) in EXAMPLES {
                            if ui
                                .add(
                                    egui::Label::new(RichText::new(q).color(p.lantern))
                                        .sense(egui::Sense::click()),
                                )
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .clicked()
                            {
                                picked = Some(q);
                            }
                            ui.label(RichText::new(what).color(p.muted));
                            ui.end_row();
                        }
                    });
                ui.add_space(14.0);
                if ui.link("All search syntax and shortcuts").clicked() {
                    self.help_open = true;
                }
            });
        });
        if let Some(q) = picked {
            q.clone_into(&mut self.query);
            self.request_search();
            self.focus_search = true;
        }
    }

    /// Nothing to list: searching, an error, or no matches.
    fn empty_results(&self, ui: &mut egui::Ui, p: Palette) {
        ui.add_space(28.0);
        ui.horizontal(|ui| {
            ui.add_space(PAD);
            ui.vertical(|ui| {
                ui.set_max_width(560.0);
                if self.searching() {
                    ui.label(RichText::new("Searching…").size(18.0).color(p.muted));
                } else if self.search_outstanding() {
                    // The answer is moments away: show nothing rather than
                    // a "no matches" that is about to be wrong.
                } else if let Some(e) = &self.search_error {
                    ui.label(RichText::new(e).color(p.error));
                } else if self.indexing_notice().is_some() {
                    ui.label(RichText::new("No matches yet").size(18.0));
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Floki is still indexing, so this search runs again by itself as \
                             files are found.",
                        )
                        .color(p.muted),
                    );
                } else {
                    ui.label(
                        RichText::new(format!(
                            "Nothing matches \u{201c}{}\u{201d}",
                            self.query.trim()
                        ))
                        .size(18.0),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Check the spelling, or search with fewer words.")
                            .color(p.muted),
                    );
                }
            });
        });
    }

    /// Column titles; sortable ones flip order on click.
    fn header_row(&mut self, ui: &mut egui::Ui, p: Palette) {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), HEADER_HEIGHT),
            egui::Sense::hover(),
        );
        let painter = ui.painter_at(rect);
        painter.hline(
            rect.x_range(),
            rect.bottom() - 0.5,
            egui::Stroke::new(1.0, p.rule),
        );
        let cols = column_rects(rect);
        let date = DateColumn::for_sort(self.sort);
        let (date_asc, date_desc) = date.sorts();
        let headers = [
            ("Name", Some((Sort::NameAsc, Sort::NameDesc)), false),
            ("Folder", Some((Sort::PathAsc, Sort::PathDesc)), false),
            ("Size", None, true),
            (date.title(), Some((date_asc, date_desc)), false),
        ];
        for (i, (title, sorts, right)) in headers.into_iter().enumerate() {
            let cell = cols[i];
            let active = sorts.is_some_and(|(a, d)| self.sort == a || self.sort == d);
            let mut color = if active { p.text } else { p.muted };
            if let Some((asc, desc)) = sorts {
                let resp = ui
                    .interact(cell, ui.id().with(("header", i)), egui::Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.hovered() {
                    color = p.text;
                }
                if resp.clicked() {
                    self.set_sort(header_click(self.sort, asc, desc));
                }
            }
            let job = one_line(title, theme::SMALL_SIZE, color, cell.width() - 12.0);
            let galley = painter.layout_job(job);
            let w = galley.size().x;
            let x = if right { cell.right() - w } else { cell.left() };
            let y = cell.center().y - galley.size().y / 2.0;
            painter.galley(egui::pos2(x, y), galley, color);
            if active {
                let desc = sorts.is_some_and(|(_, d)| self.sort == d);
                paint_sort_arrow(
                    &painter,
                    egui::pos2(x + w + 7.0, cell.center().y),
                    desc,
                    p.lantern,
                );
            }
        }
    }

    /// The virtualized result list. Requests metadata for the rows it
    /// draws and the next page when the view nears the end.
    fn result_list(&mut self, ui: &mut egui::Ui, p: Palette) {
        let terms = model::plain_terms(&self.query).unwrap_or_default();
        let date = DateColumn::for_sort(self.shown_sort);
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt("results")
            .auto_shrink([false, false]);
        if std::mem::take(&mut self.scroll_to_top) {
            scroll = scroll.vertical_scroll_offset(0.0);
        } else if let Some(target) = self.scroll_target.take() {
            if let Some(off) =
                model::scroll_offset_for(target, self.list_offset, self.list_view_h, ROW_HEIGHT)
            {
                scroll = scroll.vertical_scroll_offset(off);
            }
        }
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut clicked: Option<(usize, bool)> = None;
        let mut pending_item: Option<menu::Item> = None;
        let mut want_meta: Vec<usize> = Vec::new();
        let mut near_end = false;
        let out = scroll.show_rows(ui, ROW_HEIGHT, self.hits.len(), |ui, range| {
            near_end = range.end + PAGE_AHEAD >= self.hits.len();
            for idx in range {
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), ROW_HEIGHT),
                    egui::Sense::click(),
                );
                let hit = &self.hits[idx];
                if needs_meta(hit) && !self.meta_asked.get(idx).copied().unwrap_or(true) {
                    want_meta.push(idx);
                }
                let sel = Some(idx) == self.selected;
                let painter = ui.painter_at(rect);
                paint_band(&painter, rect, p, sel, resp.hovered());
                let cols = column_rects(rect);
                let icon = egui::Rect::from_center_size(
                    egui::pos2(cols[0].left() + 7.0, rect.center().y),
                    egui::vec2(14.0, 14.0),
                );
                theme::paint_icon(
                    &painter,
                    icon,
                    model::icon_kind(hit.is_dir, &hit.name),
                    p.muted,
                );
                let name_cell = cols[0].with_min_x(cols[0].left() + 24.0);
                let mut name_job = if terms.is_empty() {
                    one_line(&hit.name, theme::BODY_SIZE, p.text, 0.0)
                } else {
                    let ranges = model::find_match_ranges(&hit.name, &terms);
                    theme::highlight_job(&hit.name, &ranges, p, theme::BODY_SIZE)
                };
                name_job.wrap = TextWrapping::truncate_at_width(name_cell.width());
                paint_text(&painter, name_cell, name_job, false);
                paint_text(
                    &painter,
                    cols[1],
                    one_line(&hit.path, theme::BODY_SIZE, p.muted, cols[1].width()),
                    false,
                );
                let size = if hit.is_dir {
                    String::new()
                } else {
                    hit.size.map(model::format_size).unwrap_or_default()
                };
                paint_text(
                    &painter,
                    cols[2],
                    one_line(&size, theme::SMALL_SIZE + 1.0, p.muted, cols[2].width()),
                    true,
                );
                let when = date
                    .of(hit)
                    .map(|ms| model::format_time_ms(Some(ms)))
                    .unwrap_or_default();
                paint_text(
                    &painter,
                    cols[3],
                    one_line(&when, theme::SMALL_SIZE + 1.0, p.muted, cols[3].width()),
                    false,
                );

                if resp.double_clicked() {
                    clicked = Some((idx, true));
                } else if resp.clicked() || resp.secondary_clicked() {
                    // Right-click selects first, then the menu opens.
                    clicked = Some((idx, false));
                }
                let full = model::join_path(&hit.path, &hit.name);
                let items = menu::menu_items(hit.is_dir, &full);
                if self.open_menu_key && sel {
                    let mut open_state = self.context_open;
                    let out = egui::Popup::menu(&resp)
                        .open_bool(&mut open_state)
                        .show(|ui| menu::show_menu(ui, &items));
                    self.context_open = open_state;
                    if let Some(picked) = out.and_then(|r| r.inner) {
                        pending_item = Some(picked);
                        self.context_open = false;
                    }
                } else {
                    // `Response::context_menu` takes a `()` closure, so
                    // capture the pick in a local.
                    let mut picked_here = None;
                    resp.context_menu(|ui| {
                        picked_here = menu::show_menu(ui, &items);
                    });
                    if picked_here.is_some() {
                        pending_item = picked_here;
                    }
                }
            }
        });
        self.list_offset = out.state.offset.y;
        self.list_view_h = out.inner_rect.height();
        self.open_menu_key = false;
        for idx in want_meta {
            if let (Some(asked), Some(hit)) = (self.meta_asked.get_mut(idx), self.hits.get(idx)) {
                *asked = true;
                self.meta.request(MetaRequest {
                    seq: self.hits_seq,
                    idx,
                    path: model::join_path(&hit.path, &hit.name),
                });
            }
        }
        if near_end {
            self.request_page();
        }
        if let Some((idx, open)) = clicked {
            self.selected = Some(idx);
            if open {
                self.open_selected();
            }
        }
        if let Some(item) = pending_item {
            let ctx = ui.ctx().clone();
            self.apply_menu_item(item, &ctx);
        }
    }

    /// Thin status line: one message on the left (errors first, then the
    /// indexing notice, then timing), index size on the right.
    fn status_line(&mut self, ui: &mut egui::Ui, p: Palette) {
        let frame = egui::Frame::new()
            .fill(p.base)
            .inner_margin(egui::Margin::symmetric(PAD as i8, 5));
        Panel::bottom("status").frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                let (text, color, hover) = if let Some(m) = self.version_mismatch() {
                    (m, p.error, None)
                } else if let Some(e) = &self.search_error {
                    (e.clone(), p.error, None)
                } else if let Some(e) = &self.action_error {
                    (e.clone(), p.error, None)
                } else if let Some(n) = self.indexing_notice() {
                    (
                        n,
                        p.lantern,
                        Some(
                            "Floki is still reading this drive. Files it hasn't reached yet \
                             are missing from results; the results refresh when it finishes.",
                        ),
                    )
                } else if let Some(n) = self.truncation_note() {
                    (n, p.muted, None)
                } else if !self.hits.is_empty() || self.total > 0 {
                    (
                        format!("Found in {}", model::format_ms(self.elapsed_us)),
                        p.muted,
                        None,
                    )
                } else {
                    (String::new(), p.muted, None)
                };
                let right_w = 230.0;
                ui.allocate_ui(
                    egui::vec2((ui.available_width() - right_w).max(80.0), 18.0),
                    |ui| {
                        let label = ui.add(
                            egui::Label::new(
                                RichText::new(text).size(theme::SMALL_SIZE).color(color),
                            )
                            .truncate(),
                        );
                        if let Some(h) = hover {
                            label.on_hover_text(h);
                        }
                    },
                );
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if let Some((files, mem)) = self.index_text() {
                        ui.label(RichText::new(mem).size(theme::SMALL_SIZE).color(p.muted))
                            .on_hover_text("Memory used by the indexer");
                        ui.add_space(10.0);
                        ui.label(RichText::new(files).size(theme::SMALL_SIZE).color(p.muted));
                    }
                });
            });
        });
    }
}

/// Settings section title with breathing room above it.
fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(14.0);
    ui.label(RichText::new(title).family(theme::semibold()).size(15.0));
    ui.add_space(4.0);
}

/// Small muted explanation line under a setting.
fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).small().weak());
}

/// Two-column help table.
fn help_grid(ui: &mut egui::Ui, id: &str, rows: &[(&str, &str)]) {
    let lantern = theme::palette(ui.ctx()).lantern;
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([20.0, 6.0])
        .show(ui, |ui| {
            for (key, what) in rows {
                ui.label(RichText::new(*key).color(lantern));
                ui.label(RichText::new(*what).weak());
                ui.end_row();
            }
        });
}

/// Settings tab entry, drawn like a selected result row (band + amber
/// edge). Returns true when clicked.
fn nav_item(ui: &mut egui::Ui, label: &str, selected: bool, p: Palette) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::click());
    let painter = ui.painter_at(rect);
    paint_band(&painter, rect, p, selected, resp.hovered());
    let color = if selected { p.text } else { p.muted };
    paint_text(
        &painter,
        rect.shrink2(egui::vec2(12.0, 0.0)),
        one_line(label, theme::BODY_SIZE, color, rect.width() - 24.0),
        false,
    );
    resp.clicked()
}

/// The `⋯` menu button, painted (Segoe UI has no midline-ellipsis glyph).
fn menu_dots(ui: &mut egui::Ui, p: Palette) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    if resp.hovered() || open {
        ui.painter().rect_filled(rect, 4.0, p.band);
    }
    let color = if resp.hovered() || open {
        p.text
    } else {
        p.muted
    };
    for dx in [-6.0, 0.0, 6.0] {
        ui.painter()
            .circle_filled(rect.center() + egui::vec2(dx, 0.0), 1.8, color);
    }
    resp.on_hover_text("Menu")
}

/// Lay `add` out in a column at most `width` wide, centred horizontally.
fn centered_column(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    let w = width.min(ui.available_width() - 2.0 * PAD);
    ui.horizontal(|ui| {
        ui.add_space(((ui.available_width() - w) / 2.0).max(PAD));
        ui.vertical(|ui| {
            ui.set_width(w);
            add(ui);
        });
    });
}

/// Short state for one volume row in Settings→Drives.
fn volume_state(v: &VolumeStatus) -> &'static str {
    if !v.enabled {
        "hidden from search"
    } else if !v.monitor {
        "scan only"
    } else if v.live {
        "live"
    } else {
        "offline"
    }
}

/// Whether the `TextEdit` with `id` currently holds a non-empty text
/// selection (egui 0.36: persisted `TextEditState`, `cursor.char_range()` is
/// `Some` with distinct ends only when text is selected).
fn text_edit_has_selection(ctx: &egui::Context, id: egui::Id) -> bool {
    egui::TextEdit::load_state(ctx, id)
        .and_then(|s| s.cursor.char_range())
        .is_some_and(|r| r.single().is_none())
}

/// Paint a 7 px sort-direction triangle centred on `c` (pointing up for
/// ascending).
fn paint_sort_arrow(
    painter: &egui::Painter,
    c: egui::Pos2,
    descending: bool,
    color: egui::Color32,
) {
    let (tip, base) = if descending { (3.0, -3.0) } else { (-3.0, 3.0) };
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(c.x - 3.5, c.y + base),
            egui::pos2(c.x + 3.5, c.y + base),
            egui::pos2(c.x, c.y + tip),
        ],
        color,
        egui::Stroke::NONE,
    ));
}

impl eframe::App for FlokiApp {
    /// Non-rendering work; also runs while the window is hidden, so tray,
    /// hotkey, debounce, and polling keep working from the tray.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(100));

        // X hides to tray; only an explicit Quit/Exit sets `quit_requested`
        // and is allowed to close. eframe 0.36 has no `on_close_event`;
        // `logic()` runs even while hidden, so it sees the request.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit_requested {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.hide_window(ctx);
        }

        // Second half of `show_window`: the window is visible by now, so
        // `Focus` actually takes effect. Runs before `drain_external` so a
        // Show on this frame re-arms for the next one.
        if self.focus_pending {
            self.focus_pending = false;
            ctx.send_viewport_cmd(ViewportCommand::Focus);
            self.focus_search = true;
        }

        self.drain_worker();
        self.drain_meta();
        self.drain_external(ctx);

        let now = Instant::now();
        // One auto-start shortly after launch when the worker's status poll
        // has failed to reach an indexer AND no custom pipe is set: covers
        // startup/`--minimized` and the first-run case. Never probes the
        // pipe on this thread (a busy indexer would freeze the window); a
        // live pipe (even mid-scan) keeps `connected` true, and `flokid`'s
        // singleton mutex refuses a rival anyway.
        if !self.auto_launch_tried
            && !self.connected
            && self.status_error.is_some()
            && now.duration_since(self.created_at) > Duration::from_secs(3)
            && std::env::var_os("FLOKI_PIPE").is_none()
        {
            self.auto_launch_tried = true;
            self.start_owned_indexer();
        }
        // Confirm a pending launch: the child may have exited 3
        // (AlreadyRunning) or died on a stale flag, so `indexer_owned`
        // is set only once the pipe actually answers. After 30 s with
        // no answer, surface a visible error instead of silent owned-dead.
        if self.launch_pending {
            if self.connected {
                self.launch_pending = false;
                self.launched_at = None;
                self.indexer_owned = !self.launch_via_task;
                self.elevate_error = None;
            } else if self
                .launched_at
                .is_some_and(|t| now.duration_since(t) > Duration::from_secs(30))
            {
                self.launch_pending = false;
                self.launched_at = None;
                self.elevate_error = Some(
                    "The indexer didn't come up within 30 seconds. Its log is in \
                     %LOCALAPPDATA%\\Floki\\flokid.log."
                        .to_owned(),
                );
            }
        }
        if self.autostart_recheck_at.is_some_and(|t| now >= t) {
            self.autostart_recheck_at = None;
            self.refresh_autostart();
        }
        if self.last_edit.is_some_and(|t| model::debounce_due(t, now)) {
            self.request_search();
        }

        let poll_interval = if self.connected {
            STATUS_POLL
        } else {
            RECONNECT_POLL
        };
        if now.duration_since(self.last_status_poll) >= poll_interval {
            self.last_status_poll = now;
            self.status_seq += 1;
            let _ = self.worker.tx.send(ToWorker::Status {
                seq: self.status_seq,
            });
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if self.save_theme {
            storage.set_string(THEME_KEY, self.theme.as_str().to_owned());
        }
    }

    fn on_exit(&mut self) {
        #[cfg(windows)]
        {
            if self.indexer_owned {
                std::thread::spawn(|| {
                    if let Ok(mut c) = floki_proto::Client::connect() {
                        let _ = c.call(&floki_proto::Request::Shutdown {});
                    }
                });
            }
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let p = theme::palette(&ctx);
        if self.settings_open {
            self.settings_window(&ctx);
        }
        self.help_windows(&ctx);
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F1)) {
            self.help_open = !self.help_open;
        }
        if !self.connected {
            // Keep the menu reachable (Settings, Exit) while the indexer is down.
            Panel::top("down-bar")
                .frame(
                    egui::Frame::new()
                        .fill(p.base)
                        .inner_margin(egui::Margin::symmetric(PAD as i8, 8)),
                )
                .show_separator_line(false)
                .show(ui, |ui| {
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let dots = menu_dots(ui, p);
                        egui::Popup::menu(&dots).show(|ui| self.main_menu(ui));
                    });
                });
            CentralPanel::default()
                .frame(egui::Frame::new().fill(p.base))
                .show(ui, |ui| self.down_panel(ui, p));
            return;
        }

        // Window just became visible/focused (hotkey/tray-Show, or a
        // SetForegroundWindow from outside): arm the search-box focus so the
        // next keystrokes land in it. `show_window` also arms `focus_search`
        // directly; this flip catches the cases it cannot see.
        let focused_now = ctx.input(|i| i.focused);
        if focused_now && !self.was_focused {
            self.focus_search = true;
        }
        self.was_focused = focused_now;

        // Query line first: the TextEdit must see keys before the global
        // shortcuts below decide what is left over (egui 0.36 consumes handled
        // keys during `ui.add`, so a later `consume_key` only fires when the
        // box did not use the key itself).
        let search_id = self.query_line(ui, p);
        // ---- keyboard shortcuts that don't need the search box focused ----
        let mut reveal = false;
        let mut copy = false;
        let mut hide = false;
        let mut open = false;
        let mut menu_key = false;
        let mut focus_box = false;
        let mut esc = false;
        let mut settings = false;
        ui.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                self.move_sel(1);
            } else if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                self.move_sel(-1);
            } else if i.consume_key(Modifiers::CTRL, Key::Enter) {
                reveal = true;
            } else if i.consume_key(Modifiers::NONE, Key::Enter) {
                open = true;
            } else if i.consume_key(Modifiers::NONE, Key::Escape) {
                esc = true;
            } else if i.consume_key(Modifiers::NONE, Key::F2)
                || i.consume_key(Modifiers::CTRL, Key::L)
            {
                focus_box = true;
            } else if i.consume_key(Modifiers::SHIFT, Key::F10) {
                menu_key = true;
            } else if i.consume_key(Modifiers::CTRL, Key::Comma) {
                settings = true;
            }
        });
        if settings {
            self.open_settings(SettingsTab::General);
        }
        // Home/End/PageUp/PageDown/Del edit the query when the box is
        // focused, so they only move/recycle the selection otherwise.
        let search_focused = ctx.memory(|m| m.has_focus(search_id));
        if !search_focused {
            let step = self.page_step();
            ui.input_mut(|i| {
                if i.consume_key(Modifiers::NONE, Key::Delete) {
                    if let Some((_, hit, path)) = self.selected_hit() {
                        self.delete_confirm = Some(DeleteTarget {
                            path,
                            name: hit.name,
                            idx: self.selected.unwrap_or(0),
                        });
                    }
                } else if i.consume_key(Modifiers::NONE, Key::PageDown) {
                    self.move_sel(step);
                } else if i.consume_key(Modifiers::NONE, Key::PageUp) {
                    self.move_sel(-step);
                } else if i.consume_key(Modifiers::NONE, Key::Home) {
                    self.move_sel_to(0);
                } else if i.consume_key(Modifiers::NONE, Key::End) {
                    self.move_sel_to(usize::MAX);
                }
            });
        }
        // Ctrl+C copies the selected result's path ONLY when the search box
        // must not keep it: unfocused, or focused with no text selection
        // (in which case the TextEdit left the key unconsumed for us).
        if model::should_copy_path(search_focused, text_edit_has_selection(&ctx, search_id)) {
            ui.input_mut(|i| {
                if i.consume_key(Modifiers::CTRL, Key::C) {
                    copy = true;
                }
            });
        }
        if esc {
            if self.delete_confirm.is_some() {
                self.delete_confirm = None;
            } else if !self.query.is_empty() {
                // First Esc clears the query; the second hides to tray.
                self.query.clear();
                self.request_search();
            } else {
                hide = true;
            }
        }
        if focus_box {
            self.focus_search = true;
        }
        if menu_key && self.selected.is_some() {
            self.open_menu_key = true;
            self.context_open = true;
        }
        if reveal {
            self.apply_menu_item(menu::Item::Reveal, &ctx);
        }
        if open {
            self.open_selected();
        }
        if copy {
            if let Some((_, _, path)) = self.selected_hit() {
                ctx.copy_text(path);
            }
        }
        if hide {
            self.hide_window(&ctx);
        }

        self.status_line(ui, p);

        CentralPanel::default()
            .frame(egui::Frame::new().fill(p.base))
            .show(ui, |ui| {
                // Rows click-select anywhere: selectable labels would eat
                // clicks and show an I-beam cursor.
                ui.style_mut().interaction.selectable_labels = false;
                if self.query.trim().is_empty() {
                    self.start_screen(ui, p);
                    return;
                }
                self.header_row(ui, p);
                if self.hits.is_empty() {
                    self.empty_results(ui, p);
                    return;
                }
                self.result_list(ui, p);
            });

        // Recycle-Bin confirmation modal (in-app; the shell itself stays silent).
        if let Some(target) = self.delete_confirm.clone() {
            let mut confirm = false;
            let mut cancel = false;
            egui::Modal::new(egui::Id::new("delete-confirm")).show(&ctx, |ui| {
                ui.set_width(380.0);
                ui.label(
                    RichText::new("Move to the Recycle Bin?")
                        .family(theme::semibold())
                        .size(16.0),
                );
                ui.add_space(6.0);
                ui.label(RichText::new(&target.name).color(p.muted));
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("Move to Recycle Bin").color(p.base))
                                .fill(p.lantern),
                        )
                        .clicked()
                    {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
            if confirm {
                match actions::recycle_to_bin(&target.path) {
                    Ok(()) => {
                        self.drop_row(target.idx, &target.path);
                        self.delete_confirm = None;
                    }
                    Err(e) => {
                        self.action_error = Some(format!("Delete failed: {e}"));
                        self.delete_confirm = None;
                    }
                }
            } else if cancel {
                self.delete_confirm = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use global_hotkey::hotkey::{Code, Modifiers as HotkeyModifiers};

    fn test_app() -> FlokiApp {
        FlokiApp::new(
            HotKey::new(
                Some(HotkeyModifiers::CONTROL | HotkeyModifiers::ALT),
                Code::Space,
            ),
            None,
            theme::ThemeMode::Dark,
            true,
            false,
        )
    }

    fn row(name: &str) -> HitRow {
        HitRow {
            name: name.to_owned(),
            path: r"C:\d".to_owned(),
            is_dir: false,
            size: None,
            modified_ms: None,
            created_ms: None,
        }
    }

    fn outcome(offset: u32, total: u64, names: &[&str]) -> crate::client::SearchOutcome {
        crate::client::SearchOutcome {
            offset,
            total,
            hits: names.iter().map(|n| row(n)).collect(),
            elapsed_us: 10,
        }
    }

    #[test]
    fn index_text_waits_for_status() {
        let app = test_app();
        assert_eq!(app.index_text(), None);
    }

    #[test]
    fn drop_row_removes_by_position_then_path() {
        let mut app = test_app();
        app.hits = vec![row("a.txt"), row("b.txt")];
        app.meta_asked = vec![true, false];
        app.total = 2;
        app.selected = Some(0);
        app.drop_row(0, r"C:\d\a.txt");
        assert_eq!(app.hits.len(), 1);
        assert_eq!(app.meta_asked, [false]);
        assert_eq!(app.total, 1);
        assert_eq!(app.selected, Some(0));
        // Unknown path: no-op.
        app.drop_row(0, r"C:\d\missing.txt");
        assert_eq!(app.hits.len(), 1);
    }

    #[test]
    fn a_page_appends_only_when_it_lines_up() {
        let mut app = test_app();
        app.search_seq = 3;
        app.apply_results(3, outcome(0, 5, &["a", "b"]));
        assert_eq!(app.hits.len(), 2);
        assert!(app.can_page());
        app.apply_results(3, outcome(2, 5, &["c", "d"]));
        assert_eq!(app.hits.len(), 4);
        assert_eq!(app.meta_asked.len(), 4);
        // A duplicate of the same page (offset no longer the end) is ignored.
        app.apply_results(3, outcome(2, 5, &["c", "d"]));
        assert_eq!(app.hits.len(), 4);
    }

    #[test]
    fn date_sorts_never_page() {
        let mut app = test_app();
        app.search_seq = 1;
        app.sort = Sort::ModifiedDesc;
        app.apply_results(1, outcome(0, 50_000, &["a"]));
        assert!(!app.can_page());
        assert!(app.truncation_note().is_some_and(|n| n.contains("by date")));
    }

    #[test]
    fn header_click_flips_and_dates_start_newest() {
        assert_eq!(
            header_click(Sort::NameAsc, Sort::NameAsc, Sort::NameDesc),
            Sort::NameDesc
        );
        assert_eq!(
            header_click(Sort::NameDesc, Sort::NameAsc, Sort::NameDesc),
            Sort::NameAsc
        );
        assert_eq!(
            header_click(Sort::NameAsc, Sort::PathAsc, Sort::PathDesc),
            Sort::PathAsc
        );
        assert_eq!(
            header_click(Sort::NameAsc, Sort::ModifiedAsc, Sort::ModifiedDesc),
            Sort::ModifiedDesc
        );
        assert_eq!(
            header_click(Sort::ModifiedDesc, Sort::ModifiedAsc, Sort::ModifiedDesc),
            Sort::ModifiedAsc
        );
    }

    #[test]
    fn column_rects_stay_inside_the_row_without_overlap() {
        for width in [480.0, 960.0, 2400.0] {
            let row = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, ROW_HEIGHT));
            let cols = column_rects(row);
            for pair in cols.windows(2) {
                assert!(pair[0].right() <= pair[1].left(), "{width}: {cols:?}");
            }
            assert!(
                cols[3].right() <= row.right() - PAD + 0.01,
                "{width}: {cols:?}"
            );
            assert!(cols[0].left() >= row.left() + PAD);
        }
    }

    #[test]
    fn a_failed_date_sort_restores_the_shown_order() {
        let mut app = test_app();
        app.query = "x".to_owned();
        app.request_search();
        let seq = app.search_seq;
        app.apply_results(seq, outcome(0, 2, &["a", "b"]));
        app.set_sort(Sort::ModifiedDesc);
        assert!(app.searching() || app.pending_search.is_some());
        app.search_failed("Too many matches to sort by date".to_owned());
        assert_eq!(app.sort, Sort::NameAsc);
        assert_eq!(app.hits.len(), 2);
        assert!(app.pending_search.is_none());
    }

    #[test]
    fn volume_state_names_the_actual_mode() {
        let v = |live, enabled, monitor| VolumeStatus {
            letter: 'C',
            entries: 1,
            next_usn: 0,
            live,
            enabled,
            monitor,
        };
        assert_eq!(volume_state(&v(true, true, true)), "live");
        assert_eq!(volume_state(&v(false, true, true)), "offline");
        // A scan-only volume has no tail, but it is not "offline".
        assert_eq!(volume_state(&v(false, true, false)), "scan only");
        assert_eq!(volume_state(&v(true, false, true)), "hidden from search");
    }
}
