//! Non-egui UI state and pure helpers (unit-tested).
//!
//! Everything in here is free of `eframe`/`egui` so `cargo test` exercises the
//! real logic: debounce timing, stale-response filtering by sequence number,
//! human size/time formatting, path joining, selection movement, and the
//! match-highlight term splitter.

use std::time::{Duration, Instant};

use floki_proto::IndexState;

/// How long to wait after the last keystroke before sending `Search`.
pub const DEBOUNCE: Duration = Duration::from_millis(30);

/// How often the UI polls `Status` while the service is up.
pub const STATUS_POLL: Duration = Duration::from_secs(2);

/// How often the UI retries `connect` while the service is down.
pub const RECONNECT_POLL: Duration = Duration::from_secs(1);

/// Returns `true` when a search typed at `last_change` should be sent `now`.
#[must_use]
pub fn debounce_due(last_change: Instant, now: Instant) -> bool {
    now.duration_since(last_change) >= DEBOUNCE
}

/// Returns `true` when a response carrying `resp_seq` is stale given that the
/// newest outstanding request is `current_seq`. Stale responses are dropped so
/// a slow earlier query can never overwrite newer results.
#[must_use]
pub fn is_stale(resp_seq: u64, current_seq: u64) -> bool {
    resp_seq != current_seq
}

/// Format a byte count as `B`/`KB`/`MB`/`GB` with one fractional digit above bytes.
#[must_use]
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.1} GB", b / GB)
    }
}

/// Join a parent directory and a file name with a single backslash.
///
/// `HitRow` splits hits into `name` + parent `path`; the full path is only
/// rebuilt for rows the user acts on (open/copy) or that are rendered.
#[must_use]
pub fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with(['\\', '/']) || dir.is_empty() {
        format!("{dir}{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

/// Add a path filter to a query without pretending it is a separate index scope.
/// Empty paths leave the query unchanged; existing query text is preserved.
#[must_use]
#[allow(dead_code)]
pub fn with_path_filter(path: &str, query: &str) -> String {
    let path = path.trim();
    let query = query.trim();
    if path.is_empty() {
        return query.to_owned();
    }
    if query.is_empty() {
        format!("path:{path}")
    } else {
        format!("path:{path} {query}")
    }
}

/// Whether the global Ctrl+C (copy selected result's path) may fire.
///
/// Returns `false` when the search box owns the key: focused with a non-empty
/// text selection, so the `TextEdit` copies the query text instead. In every
/// other case (box unfocused, or focused with just a cursor) the path copy
/// wins.
#[must_use]
pub fn should_copy_path(search_focused: bool, search_has_selection: bool) -> bool {
    !(search_focused && search_has_selection)
}

/// Move a list selection by `delta` rows, clamping to `[0, len)`.
/// `None` (no selection) moves to the first/last row depending on direction.
#[must_use]
pub fn move_selection(current: Option<usize>, delta: isize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let cur = match current {
        Some(i) => i.min(len - 1) as isize,
        None => {
            return Some(if delta < 0 { len - 1 } else { 0 });
        }
    };
    Some((cur + delta).clamp(0, len as isize - 1) as usize)
}

/// Format an integer with thousands separators: `4810187` -> `"4,810,187"`.
#[must_use]
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Match counter beside the query: `"1 match"`, `"4,810 matches"`.
#[must_use]
pub fn match_count_text(total: u64) -> String {
    if total == 1 {
        "1 match".to_owned()
    } else {
        format!("{} matches", format_count(total))
    }
}

/// Scroll offset that brings row `target` into a list showing `rows_in_view`
/// rows of `row_h` from offset `offset`; `None` when it is already visible.
/// Rows above snap to the top edge, rows below to the bottom edge.
#[must_use]
pub fn scroll_offset_for(target: usize, offset: f32, view_h: f32, row_h: f32) -> Option<f32> {
    let top = target as f32 * row_h;
    let bottom = top + row_h;
    if top < offset {
        Some(top)
    } else if bottom > offset + view_h {
        Some((bottom - view_h).max(0.0))
    } else {
        None
    }
}

/// Status-bar notice while the index is incomplete; `None` once ready.
#[must_use]
pub fn indexing_notice(state: &IndexState) -> Option<String> {
    match state {
        IndexState::Ready => None,
        IndexState::Loading => Some("Loading the index. Results may be incomplete.".to_owned()),
        IndexState::Scanning { volume, done } => Some(format!(
            "Indexing {volume}: {} items so far. Results may be incomplete.",
            format_count(*done)
        )),
    }
}

/// Whether a status poll should re-run the visible search: results computed
/// against a partial index went stale once indexing finished (or moved on
/// to another drive), and an empty result is retried on every poll while
/// indexing so files show up as they are found.
#[must_use]
pub fn should_refresh_search(
    prev: Option<&IndexState>,
    now: &IndexState,
    has_query: bool,
    no_hits: bool,
) -> bool {
    if !has_query {
        return false;
    }
    match (prev, now) {
        (Some(IndexState::Ready) | None, IndexState::Ready) => false,
        (Some(_), IndexState::Ready) => true,
        (Some(IndexState::Scanning { volume: a, .. }), IndexState::Scanning { volume: b, .. })
            if a != b =>
        {
            true
        }
        _ => no_hits,
    }
}

/// Format service RSS bytes as whole megabytes: `282 MB`, `1,024 MB`.
#[must_use]
pub fn format_rss_mb(bytes: u64) -> String {
    format!("{} MB", format_count(bytes / 1_048_576))
}

/// Format a query time in microseconds as milliseconds with one decimal:
/// `18500` -> `"18.5 ms"`.
#[must_use]
pub fn format_ms(us: u64) -> String {
    format!("{:.1} ms", us as f64 / 1000.0)
}

/// File-type classification for the result-row icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconKind {
    Folder,
    Executable,
    Archive,
    Image,
    Document,
    File,
}

/// Classify a hit for its row icon: directories first, then by extension
/// (case-insensitive, without the dot).
#[must_use]
pub fn icon_kind(is_dir: bool, name: &str) -> IconKind {
    if is_dir {
        return IconKind::Folder;
    }
    match extension(name).as_deref() {
        Some("exe" | "msi" | "bat" | "cmd" | "ps1" | "com" | "scr") => IconKind::Executable,
        Some("zip" | "7z" | "rar" | "tar" | "gz" | "bz2" | "xz" | "cab" | "iso") => {
            IconKind::Archive
        }
        Some("png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tiff" | "tif" | "svg" | "ico") => {
            IconKind::Image
        }
        Some(
            "txt" | "md" | "rst" | "log" | "doc" | "docx" | "pdf" | "rtf" | "odt" | "xls" | "xlsx"
            | "ppt" | "pptx" | "csv" | "json" | "xml" | "yaml" | "yml" | "toml" | "ini",
        ) => IconKind::Document,
        _ => IconKind::File,
    }
}

/// Lowercase extension without the dot; `None` when there is none.
/// A leading dot (`.gitignore`) is not an extension.
fn extension(name: &str) -> Option<String> {
    let base = name.rsplit(['\\', '/']).next().unwrap_or(name);
    let dot = base.rfind('.')?;
    if dot == 0 || dot + 1 >= base.len() {
        return None;
    }
    Some(base[dot + 1..].to_lowercase())
}

/// Whether the "Run as administrator" menu item applies: files (never
/// directories) with an executable-ish extension.
#[must_use]
pub fn should_show_runas(path: &str, is_dir: bool) -> bool {
    if is_dir {
        return false;
    }
    matches!(
        extension(path).as_deref(),
        Some("exe" | "msi" | "bat" | "cmd" | "ps1")
    )
}

/// Split a search string into plain highlight terms.
///
/// Returns `None` when the query is not plain-substring search (glob
/// `*`/`?`, quoted phrases, `|` alternation, `<>` groups, or a function
/// that changes how names match, such as `regex:`/`case:`), in which case
/// the UI skips match highlighting. Filters that leave the name match alone
/// (`path:`, `ext:`, bare `folder:`/`file:`) are skipped, so
/// `path:C:\Windows shell` still lights up `shell`. Otherwise returns the
/// whitespace-separated terms (empty vec for an empty query). A Windows
/// drive prefix (`C:\…`) is not treated as a function filter.
#[must_use]
pub fn plain_terms(query: &str) -> Option<Vec<String>> {
    if query.contains(['*', '?', '"', '|', '<', '>']) {
        return None;
    }
    let mut terms = Vec::new();
    for token in query.split_whitespace() {
        let t = token.strip_prefix('!').unwrap_or(token);
        if t.is_empty() {
            continue;
        }
        if is_function_token(t) {
            let lower = t.to_ascii_lowercase();
            if lower.starts_with("path:")
                || lower.starts_with("ext:")
                || lower == "folder:"
                || lower == "file:"
            {
                continue;
            }
            return None;
        }
        terms.push(t.to_owned());
    }
    Some(terms)
}

/// A token like `ext:rs` (but not a drive path like `C:\foo`).
fn is_function_token(token: &str) -> bool {
    let Some(colon) = token.find(':') else {
        return false;
    };
    let prefix = &token[..colon];
    if prefix.len() == 1 && token.as_bytes().get(1) == Some(&b':') {
        // Single letter + colon: a drive (`C:\`) unless followed by
        // something else entirely; drives are `X:\` or `X:/`.
        let after = &token[2..];
        if after.starts_with(['\\', '/']) || after.is_empty() {
            return false;
        }
    }
    prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Find all non-overlapping case-insensitive matches of `terms` in `name`.
///
/// Returns byte ranges `(start, end)` into `name`, sorted and merged.
/// Matching runs on lowercased copies; when lowercasing changes the byte
/// length (rare non-ASCII expansions) highlighting is skipped for safety.
#[must_use]
pub fn find_match_ranges(name: &str, terms: &[String]) -> Vec<(usize, usize)> {
    let lower = name.to_lowercase();
    if lower.len() != name.len() {
        return Vec::new();
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for term in terms {
        let needle = term.to_lowercase();
        if needle.is_empty() {
            continue;
        }
        let mut from = 0;
        while let Some(rel) = lower[from..].find(needle.as_str()) {
            let start = from + rel;
            let end = start + needle.len();
            // Only keep char-boundary ranges (ASCII fast path always is).
            if name.is_char_boundary(start) && name.is_char_boundary(end) {
                ranges.push((start, end));
            }
            from = end.max(from + 1);
            if from >= lower.len() {
                break;
            }
        }
    }
    ranges.sort_unstable();
    // Merge overlaps (e.g. terms "foo" + "ooba" in "foobar").
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        merged.push((s, e));
    }
    merged
}

/// Format an optional Unix-epoch-milliseconds timestamp for the
/// Modified/Created columns: `"--"` when unknown, else [`format_time`].
#[must_use]
pub fn format_time_ms(ms: Option<i64>) -> String {
    match ms.and_then(|m| {
        u64::try_from(m)
            .ok()
            .and_then(|u| std::time::UNIX_EPOCH.checked_add(Duration::from_millis(u)))
    }) {
        Some(t) => format_time(t),
        None => "--".to_owned(),
    }
}

/// Format a `SystemTime` as `YYYY-MM-DD HH:MM` in local time (best effort).
/// Uses the C runtime `localtime_s` via `chrono`-free arithmetic: converts to
/// a Unix timestamp and formats with the OS local offset. Falls back to the
/// raw debug representation when out of range.
#[must_use]
pub fn format_time(t: std::time::SystemTime) -> String {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => {
            let secs = d.as_secs() as i64;
            // Days since epoch -> civil date (Howard Hinnant's algorithm).
            let days = secs.div_euclid(86_400);
            let tod = secs.rem_euclid(86_400);
            let z = days + 719_468;
            let era = z.div_euclid(146_097);
            let doe = z.rem_euclid(146_097);
            let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
            let y = yoe + era * 400;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                y + i64::from(m <= 2),
                m,
                d,
                tod / 3600,
                (tod % 3600) / 60
            )
        }
        Err(_) => "—".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_fires_only_after_30ms() {
        let t0 = Instant::now();
        assert!(!debounce_due(t0, t0));
        assert!(!debounce_due(t0, t0 + Duration::from_millis(29)));
        assert!(debounce_due(t0, t0 + Duration::from_millis(30)));
        assert!(debounce_due(t0, t0 + Duration::from_secs(5)));
    }

    #[test]
    fn stale_responses_are_dropped_by_sequence() {
        // Newest outstanding request is #3: only its response applies.
        assert!(is_stale(1, 3));
        assert!(is_stale(2, 3));
        assert!(!is_stale(3, 3));
        // A response from the future can never happen; treat as stale.
        assert!(is_stale(4, 3));
    }

    #[test]
    fn size_formatting_pins_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1), "1 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(5 * 1024 * 1024 + 512 * 1024), "5.5 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0 GB");
    }

    #[test]
    fn path_joining_handles_root_backslash() {
        assert_eq!(join_path(r"C:\docs", "foo.txt"), r"C:\docs\foo.txt");
        assert_eq!(join_path(r"C:\", "foo.txt"), r"C:\foo.txt");
        assert_eq!(join_path("", "foo.txt"), "foo.txt");
        assert_eq!(join_path(r"C:\docs\", "foo.txt"), r"C:\docs\foo.txt");
    }

    #[test]
    fn ctrl_c_goes_to_search_box_only_with_a_selection() {
        assert!(should_copy_path(false, false));
        assert!(should_copy_path(false, true));
        assert!(should_copy_path(true, false));
        assert!(!should_copy_path(true, true));
    }

    #[test]
    fn selection_moves_and_clamps() {
        assert_eq!(move_selection(None, 1, 0), None);
        assert_eq!(move_selection(None, 1, 5), Some(0));
        assert_eq!(move_selection(None, -1, 5), Some(4));
        assert_eq!(move_selection(Some(2), 1, 5), Some(3));
        assert_eq!(move_selection(Some(2), -1, 5), Some(1));
        assert_eq!(move_selection(Some(4), 1, 5), Some(4));
        assert_eq!(move_selection(Some(0), -1, 5), Some(0));
        assert_eq!(move_selection(Some(99), 1, 5), Some(4));
    }

    #[test]
    fn optional_size_and_time_format_as_dashes() {
        assert_eq!(format_time_ms(None), "--");
        assert_eq!(format_time_ms(Some(-1)), "--");
        assert_eq!(format_time_ms(Some(1_700_000_000_000)).len(), 16);
    }

    #[test]
    fn time_formats_epoch_sanely() {
        let s = format_time(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        // 2023-11-14 22:13 UTC; local timezone may shift it, so only check shape.
        assert_eq!(s.len(), 16);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], " ");
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(7), "7");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(1_234), "1,234");
        assert_eq!(format_count(4_810_187), "4,810,187");
        assert_eq!(format_count(1_000_000_000), "1,000,000,000");
    }

    #[test]
    fn rss_formats_as_whole_megabytes() {
        assert_eq!(format_rss_mb(0), "0 MB");
        assert_eq!(format_rss_mb(41 * 1_048_576), "41 MB");
        assert_eq!(format_rss_mb(282 * 1_048_576), "282 MB");
    }

    #[test]
    fn ms_formats_with_one_decimal() {
        assert_eq!(format_ms(0), "0.0 ms");
        assert_eq!(format_ms(18_500), "18.5 ms");
        assert_eq!(format_ms(101_741), "101.7 ms");
    }

    #[test]
    fn icon_class_comes_from_dir_flag_then_extension() {
        assert_eq!(icon_kind(true, "setup.exe"), IconKind::Folder);
        assert_eq!(icon_kind(true, "noext"), IconKind::Folder);
        assert_eq!(icon_kind(false, "setup.exe"), IconKind::Executable);
        assert_eq!(icon_kind(false, "SETUP.EXE"), IconKind::Executable);
        assert_eq!(icon_kind(false, "patch.msi"), IconKind::Executable);
        assert_eq!(icon_kind(false, "run.bat"), IconKind::Executable);
        assert_eq!(icon_kind(false, "run.CMD"), IconKind::Executable);
        assert_eq!(icon_kind(false, "job.ps1"), IconKind::Executable);
        assert_eq!(icon_kind(false, "data.zip"), IconKind::Archive);
        assert_eq!(icon_kind(false, "data.7z"), IconKind::Archive);
        assert_eq!(icon_kind(false, "photo.png"), IconKind::Image);
        assert_eq!(icon_kind(false, "photo.JPG"), IconKind::Image);
        assert_eq!(icon_kind(false, "notes.txt"), IconKind::Document);
        assert_eq!(icon_kind(false, "deck.pdf"), IconKind::Document);
        assert_eq!(icon_kind(false, "main.rs"), IconKind::File);
        assert_eq!(icon_kind(false, "noext"), IconKind::File);
        assert_eq!(icon_kind(false, ".gitignore"), IconKind::File);
        assert_eq!(icon_kind(false, "trailing."), IconKind::File);
    }

    #[test]
    fn runas_only_for_executable_files() {
        assert!(should_show_runas(r"C:\a\setup.exe", false));
        assert!(should_show_runas(r"C:\a\SETUP.EXE", false));
        assert!(should_show_runas(r"C:\a\patch.msi", false));
        assert!(should_show_runas(r"C:\a\run.bat", false));
        assert!(should_show_runas(r"C:\a\run.cmd", false));
        assert!(should_show_runas(r"C:\a\job.ps1", false));
        assert!(!should_show_runas(r"C:\a\setup.exe", true));
        assert!(!should_show_runas(r"C:\a\notes.txt", false));
        assert!(!should_show_runas(r"C:\a\archive.zip", false));
        assert!(!should_show_runas(r"C:\a\noext", false));
        assert!(!should_show_runas(r"C:\a\folder", true));
    }

    #[test]
    fn plain_terms_split_whitespace_and_skip_patterns() {
        assert_eq!(plain_terms(""), Some(vec![]));
        assert_eq!(
            plain_terms("foo bar"),
            Some(vec!["foo".to_owned(), "bar".to_owned()])
        );
        assert_eq!(
            plain_terms("  Floki  Design "),
            Some(vec!["Floki".to_owned(), "Design".to_owned()])
        );
        // Glob / regex / structured queries: no highlighting.
        assert_eq!(plain_terms("*.rs"), None);
        assert_eq!(plain_terms("foo?"), None);
        assert_eq!(plain_terms("\"exact phrase\""), None);
        assert_eq!(plain_terms("a|b"), None);
        assert_eq!(plain_terms("<group>"), None);
        assert_eq!(plain_terms("regex:foo"), None);
        // Filters that leave the name match alone are skipped, not fatal.
        assert_eq!(plain_terms("ext:rs"), Some(vec![]));
        assert_eq!(plain_terms("path:docs"), Some(vec![]));
        assert_eq!(
            plain_terms(r"path:C:\Windows ext:dll;exe folder: shell"),
            Some(vec!["shell".to_owned()])
        );
        assert_eq!(plain_terms("folder:temp"), None);
        assert_eq!(plain_terms("case:Foo"), None);
        assert_eq!(plain_terms("!foo"), Some(vec!["foo".to_owned()]));
        // A drive path is not a function filter.
        assert_eq!(
            plain_terms(r"C:\docs foo"),
            Some(vec![r"C:\docs".to_owned(), "foo".to_owned()])
        );
    }

    #[test]
    fn match_ranges_are_case_insensitive_sorted_and_merged() {
        let terms = vec!["flo".to_owned(), "DESIGN".to_owned()];
        assert_eq!(
            find_match_ranges("FlokiDesign.md", &terms),
            vec![(0, 3), (5, 11)]
        );
        // Repeated and overlapping hits merge into one range.
        assert_eq!(
            find_match_ranges("foobar", &["foo".to_owned(), "ooba".to_owned()]),
            vec![(0, 5)]
        );
        assert_eq!(find_match_ranges("aaa", &["a".to_owned()]), vec![(0, 3)]);
        // No terms, empty needle, or no hit: nothing highlighted.
        assert_eq!(find_match_ranges("abc", &[]), Vec::<(usize, usize)>::new());
        assert_eq!(
            find_match_ranges("abc", &["zzz".to_owned()]),
            Vec::<(usize, usize)>::new()
        );
    }

    #[test]
    fn path_filter_preserves_query_text() {
        assert_eq!(
            with_path_filter(r"C:\Users\Ada", "report"),
            r"path:C:\Users\Ada report"
        );
        assert_eq!(with_path_filter(r"D:\Archive", ""), r"path:D:\Archive");
        assert_eq!(with_path_filter("", " report "), "report");
    }

    #[test]
    fn match_count_text_handles_one_and_many() {
        assert_eq!(match_count_text(0), "0 matches");
        assert_eq!(match_count_text(1), "1 match");
        assert_eq!(match_count_text(4810), "4,810 matches");
    }

    #[test]
    fn scroll_offset_for_snaps_only_when_off_screen() {
        // View of 10 rows at offset 280 (rows 10..20 visible).
        assert_eq!(scroll_offset_for(12, 280.0, 280.0, 28.0), None);
        assert_eq!(scroll_offset_for(3, 280.0, 280.0, 28.0), Some(84.0));
        assert_eq!(scroll_offset_for(20, 280.0, 280.0, 28.0), Some(308.0));
        // A view shorter than one row still shows the row's bottom edge.
        assert_eq!(scroll_offset_for(0, 0.0, 10.0, 28.0), Some(18.0));
    }

    #[test]
    fn indexing_notice_names_the_drive_and_count() {
        assert_eq!(indexing_notice(&IndexState::Ready), None);
        assert_eq!(
            indexing_notice(&IndexState::Scanning {
                volume: 'G',
                done: 1_234_567
            })
            .as_deref(),
            Some("Indexing G: 1,234,567 items so far. Results may be incomplete.")
        );
        assert!(indexing_notice(&IndexState::Loading).is_some());
    }

    #[test]
    fn search_refreshes_when_indexing_ends_or_finds_nothing() {
        let scan = |volume| IndexState::Scanning { volume, done: 10 };
        let ready = IndexState::Ready;
        // Indexing just finished: the shown results came from a partial index.
        assert!(should_refresh_search(Some(&scan('G')), &ready, true, false));
        assert!(should_refresh_search(
            Some(&IndexState::Loading),
            &ready,
            true,
            false
        ));
        // Moved on to another drive.
        assert!(should_refresh_search(
            Some(&scan('C')),
            &scan('G'),
            true,
            false
        ));
        // Still indexing: retry only an empty result.
        assert!(should_refresh_search(
            Some(&scan('G')),
            &scan('G'),
            true,
            true
        ));
        assert!(!should_refresh_search(
            Some(&scan('G')),
            &scan('G'),
            true,
            false
        ));
        // Ready all along, or no query: never.
        assert!(!should_refresh_search(Some(&ready), &ready, true, true));
        assert!(!should_refresh_search(None, &ready, true, true));
        assert!(!should_refresh_search(
            Some(&scan('G')),
            &ready,
            false,
            true
        ));
    }
}
