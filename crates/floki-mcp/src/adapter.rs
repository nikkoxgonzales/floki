//! Indexer answers → compact text for a language model.
//!
//! Every byte here is paid for in the model's context, so the shapes are
//! chosen for tokens, not looks:
//!
//! - The first line is the decision: how many matched, how many are shown,
//!   and the exact `offset` for the next page (a silent cut makes the model
//!   reason as if the list were complete).
//! - One result per line, as the exact path the model can hand to another
//!   tool. Consecutive results in the same folder share one folder line
//!   (`C:\dir\` then indented names): free for name order, a large saving
//!   for folder order, and the order itself never changes.
//! - Folders end with `\`, so no "type" column is needed.
//! - Size and date only when asked for (`details`).

use floki_proto::{HitRow, IndexState, Sort, VolumeStatus};

/// Results per call unless the caller asks for more.
pub const DEFAULT_LIMIT: u32 = 50;
/// Hard ceiling on results per call (also declared in the tool schema).
pub const MAX_LIMIT: u32 = 500;

/// `sort` values the tool accepts, in schema order.
pub const SORT_NAMES: [&str; 8] = [
    "name",
    "-name",
    "path",
    "-path",
    "newest",
    "oldest",
    "newest_created",
    "oldest_created",
];

#[must_use]
pub fn parse_sort(s: &str) -> Option<Sort> {
    Some(match s {
        "name" => Sort::NameAsc,
        "-name" => Sort::NameDesc,
        "path" => Sort::PathAsc,
        "-path" => Sort::PathDesc,
        "newest" => Sort::ModifiedDesc,
        "oldest" => Sort::ModifiedAsc,
        "newest_created" => Sort::CreatedDesc,
        "oldest_created" => Sort::CreatedAsc,
        _ => return None,
    })
}

fn sort_label(sort: Sort) -> &'static str {
    match sort {
        Sort::NameAsc => "by name",
        Sort::NameDesc => "by name, Z to A",
        Sort::PathAsc => "by folder",
        Sort::PathDesc => "by folder, Z to A",
        Sort::ModifiedDesc => "newest modified first",
        Sort::ModifiedAsc => "oldest modified first",
        Sort::CreatedDesc => "newest created first",
        Sort::CreatedAsc => "oldest created first",
    }
}

/// One page of search results, as the tool returns it.
#[derive(Debug)]
pub struct Page<'a> {
    pub query: &'a str,
    pub sort: Sort,
    pub offset: u32,
    pub total: u64,
    pub hits: &'a [HitRow],
    /// Append size and modified time to each line.
    pub details: bool,
    /// Index state, for the "may be incomplete" note.
    pub state: Option<&'a IndexState>,
}

/// The whole tool answer for one page.
#[must_use]
pub fn results_text(p: &Page<'_>) -> String {
    let mut out = String::new();
    let shown = p.hits.len() as u64;
    let first = u64::from(p.offset) + 1;
    let last = u64::from(p.offset) + shown;
    if p.total == 0 {
        out.push_str(&format!("No matches for \"{}\".", p.query));
        out.push_str(
            " Every word must appear in the name; to match a folder anywhere in the \
             path, use path:word.",
        );
    } else if shown == 0 {
        out.push_str(&format!(
            "{} matches for \"{}\", but offset={} is past the end.",
            count(p.total),
            p.query,
            p.offset
        ));
    } else {
        if p.offset == 0 && shown == p.total {
            let noun = if p.total == 1 { "match" } else { "matches" };
            out.push_str(&format!("{} {noun}", count(p.total)));
        } else if p.offset == 0 {
            out.push_str(&format!("{} of {} matches", count(shown), count(p.total)));
        } else {
            out.push_str(&format!(
                "Matches {}-{} of {}",
                count(first),
                count(last),
                count(p.total)
            ));
        }
        out.push_str(&format!(" for \"{}\", {}.", p.query, sort_label(p.sort)));
        if last < p.total {
            out.push_str(&format!(
                " Next page: offset={last}, or narrow the query (ext:, path:, folder:)."
            ));
        }
        if p.details {
            out.push_str(" Each line: path, size, modified (UTC).");
        }
    }
    if let Some(note) = p.state.and_then(index_notice) {
        out.push('\n');
        out.push_str(&note);
    }
    let mut i = 0;
    while i < p.hits.len() {
        let dir = &p.hits[i].path;
        let run = p.hits[i..].iter().take_while(|h| &h.path == dir).count();
        if run == 1 {
            out.push('\n');
            out.push_str(&join_path(dir, &p.hits[i].name));
            push_entry_tail(&mut out, &p.hits[i], p.details);
        } else {
            out.push('\n');
            out.push_str(&with_trailing_sep(dir));
            for hit in &p.hits[i..i + run] {
                out.push_str("\n  ");
                out.push_str(&hit.name);
                push_entry_tail(&mut out, hit, p.details);
            }
        }
        i += run;
    }
    out
}

/// `\` after a folder, then size/date when `details`.
fn push_entry_tail(out: &mut String, hit: &HitRow, details: bool) {
    if hit.is_dir {
        out.push('\\');
    }
    if !details {
        return;
    }
    if let (false, Some(bytes)) = (hit.is_dir, hit.size) {
        out.push_str("  ");
        out.push_str(&format_size(bytes));
    }
    if let Some(ms) = hit.modified_ms {
        out.push_str("  ");
        out.push_str(&format_utc_minute(ms));
    }
}

/// Note for an incomplete index; `None` once ready.
#[must_use]
pub fn index_notice(state: &IndexState) -> Option<String> {
    match state {
        IndexState::Ready => None,
        IndexState::Loading => {
            Some("Note: the index is still loading, so results may be incomplete.".to_owned())
        }
        IndexState::Scanning { volume, .. } => Some(format!(
            "Note: drive {volume}: is still being indexed, so results may be incomplete."
        )),
    }
}

/// `index_status` answer: totals and state on the first line, then one
/// line per drive.
#[must_use]
pub fn status_text(
    entries: u64,
    volumes: &[VolumeStatus],
    rss_bytes: u64,
    state: &IndexState,
) -> String {
    let drives: Vec<String> = volumes.iter().map(|v| format!("{}:", v.letter)).collect();
    let mut out = format!(
        "{} files and folders indexed on {}. ",
        count(entries),
        if drives.is_empty() {
            "no drives".to_owned()
        } else {
            drives.join(" ")
        }
    );
    out.push_str(&match state {
        IndexState::Ready => "Ready.".to_owned(),
        IndexState::Loading => "Loading the saved index; results may be incomplete.".to_owned(),
        IndexState::Scanning { volume, done } => format!(
            "Indexing {volume}: ({} read so far); results may be incomplete.",
            count(*done)
        ),
    });
    out.push_str(&format!(
        " Indexer memory: {} MB.",
        count(rss_bytes / 1_048_576)
    ));
    for v in volumes {
        let mode = if !v.enabled {
            "hidden from search"
        } else if !v.monitor {
            "scanned once, not kept up to date"
        } else if v.live {
            "live"
        } else {
            "offline"
        };
        out.push_str(&format!("\n{}:  {}  {mode}", v.letter, count(v.entries)));
    }
    out
}

/// Thousands separators: `4810187` → `4,810,187`.
fn count(n: u64) -> String {
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

/// Folder + name with exactly one separator (`C:\` already ends in one).
fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else if dir.ends_with('\\') {
        format!("{dir}{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

fn with_trailing_sep(dir: &str) -> String {
    if dir.ends_with('\\') {
        dir.to_owned()
    } else {
        format!("{dir}\\")
    }
}

/// `512 B`, `12.3 KB`, `4.1 MB`, `2.0 GB`.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// Unix ms as `YYYY-MM-DD HH:MM` UTC (civil-from-days).
fn format_utc_minute(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        tod / 3600,
        tod % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, name: &str, is_dir: bool) -> HitRow {
        HitRow {
            name: name.to_owned(),
            path: path.to_owned(),
            is_dir,
            size: Some(2048),
            modified_ms: Some(1_700_000_000_000),
            created_ms: None,
        }
    }

    fn page<'a>(hits: &'a [HitRow], total: u64, offset: u32) -> Page<'a> {
        Page {
            query: "python",
            sort: Sort::NameAsc,
            offset,
            total,
            hits,
            details: false,
            state: None,
        }
    }

    #[test]
    fn every_sort_name_parses() {
        for name in SORT_NAMES {
            assert!(parse_sort(name).is_some(), "{name}");
        }
        assert_eq!(parse_sort("modified"), None);
    }

    #[test]
    fn first_line_says_shown_total_and_next_offset() {
        let hits = [hit(r"C:\a", "python.exe", false)];
        let text = results_text(&page(&hits, 1_092_658, 0));
        let first = text.lines().next().unwrap();
        assert_eq!(
            first,
            "1 of 1,092,658 matches for \"python\", by name. Next page: offset=1, \
             or narrow the query (ext:, path:, folder:)."
        );
        let later = results_text(&page(&hits, 1_092_658, 50));
        assert!(later.starts_with("Matches 51-51 of 1,092,658"), "{later}");
    }

    #[test]
    fn complete_pages_have_no_paging_hint() {
        let hits = [hit(r"C:\a", "python.exe", false)];
        let text = results_text(&page(&hits, 1, 0));
        assert_eq!(text, "1 match for \"python\", by name.\nC:\\a\\python.exe");
    }

    #[test]
    fn consecutive_results_share_one_folder_line() {
        let hits = [
            hit(r"C:\py", "python.exe", false),
            hit(r"C:\py", "Scripts", true),
            hit(r"D:\", "python.zip", false),
        ];
        let text = results_text(&page(&hits, 3, 0));
        let body: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(
            body,
            ["C:\\py\\", "  python.exe", "  Scripts\\", "D:\\python.zip"]
        );
    }

    #[test]
    fn details_add_size_and_utc_minute() {
        let hits = [hit(r"C:\a", "b.txt", false), hit(r"C:\", "dir", true)];
        let mut p = page(&hits, 2, 0);
        p.details = true;
        let text = results_text(&p);
        assert!(
            text.contains("C:\\a\\b.txt  2.0 KB  2023-11-14 22:13"),
            "{text}"
        );
        assert!(text.contains("C:\\dir\\  2023-11-14 22:13"), "{text}");
    }

    #[test]
    fn empty_and_indexing_answers_steer() {
        let mut p = page(&[], 0, 0);
        let scanning = IndexState::Scanning {
            volume: 'G',
            done: 5,
        };
        p.state = Some(&scanning);
        let text = results_text(&p);
        assert!(text.starts_with("No matches for \"python\"."));
        assert!(text.contains("path:word"));
        assert!(text.contains("drive G: is still being indexed"));
    }

    #[test]
    fn status_lists_each_drive_once() {
        let v = |letter, live| VolumeStatus {
            letter,
            entries: 1234,
            next_usn: 0,
            live,
            enabled: true,
            monitor: true,
        };
        let text = status_text(
            2468,
            &[v('C', true), v('D', false)],
            41 << 20,
            &IndexState::Ready,
        );
        assert_eq!(
            text,
            "2,468 files and folders indexed on C: D:. Ready. Indexer memory: 41 MB.\n\
             C:  1,234  live\nD:  1,234  offline"
        );
    }

    #[test]
    fn sizes_are_compact() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(12 * 1024 + 300), "12.3 KB");
        assert_eq!(format_size(3 << 30), "3.0 GB");
    }
}
