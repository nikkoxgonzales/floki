//! `flk`: es.exe-style command-line client for the Floki indexer service.
//!
//! Talks to `flokid` over the named pipe from [`floki_proto::pipe_name`]
//! (override with `--pipe` / `FLOKI_PIPE`) and prints results to stdout.
//!
//! Exit codes: 0 ok (search: at least one hit), 1 search with zero hits,
//! 2 service not running, 3 error.

use clap::{Parser, Subcommand};
use floki_proto::{Client, IndexState, Request, Response, Sort};
use std::time::Instant;

/// Canned bench workload: 20 queries, each run `--iters` times.
const CANNED_QUERIES: [&str; 20] = [
    "a",
    "e",
    "win",
    "dll",
    "ext:exe",
    "ext:rs;toml",
    "readme",
    "path:program",
    "*.log",
    "folder:temp",
    "!ext:dll sys",
    "config|settings",
    "regex:^[a-c].*\\.txt$",
    "case:Windows",
    "wfn:notepad.exe",
    "x",
    "node_modules",
    "ext:png;jpg;gif",
    "\"program files\"",
    "cache",
];

/// Bench gate: p95 wall-clock round-trip must stay below 50 ms.
const P95_WALL_GATE_US: u64 = 50_000;
/// Bench gate: service RSS per 1 M entries must stay at or below 70 MiB. The gate
/// guards against unbounded creep and hog-scale regressions; measured reality
/// is ~55-64 MB/1M structural (bench_1m fixture) plus up to ~60 MB total of
/// deliberate reusable search scratch that stays resident for latency (flat
/// over an hour, not a leak). 70 MiB leaves headroom over both while still
/// failing on any real leak or regression.
const RSS_PER_MILLION_GATE_BYTES: f64 = 70.0 * 1024.0 * 1024.0;

#[derive(Parser, Debug)]
#[command(
    name = "flk",
    version,
    about = "Floki command-line search client (talks to flokid over a named pipe)",
    arg_required_else_help = true
)]
struct Cli {
    /// Named-pipe name override (also honored via the FLOKI_PIPE env var).
    #[arg(long, global = true, value_name = "NAME")]
    pipe: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,

    /// Maximum number of hits to print.
    #[arg(short = 'n', long = "max", default_value_t = 100)]
    max: u32,

    /// Number of leading hits to skip.
    #[arg(short = 'o', long = "offset", default_value_t = 0)]
    offset: u32,

    /// Result order: name | -name | path | -path | modified | -modified | created |
    /// -created (also newest / oldest). A leading `-` means descending.
    #[arg(
        short = 's',
        long = "sort",
        default_value = "name",
        value_parser = parse_sort,
        allow_hyphen_values = true
    )]
    sort: Sort,

    /// Print the raw Response JSON line instead of formatted paths.
    #[arg(long = "json")]
    json: bool,

    /// Long listing: modified time and size before each path.
    #[arg(short = 'l', long = "long")]
    long: bool,

    /// After the results, print the match count and timings to stderr
    /// (service time inside flokid, and the full round trip).
    #[arg(long = "stats")]
    stats: bool,

    /// Query words; joined with spaces into one query string.
    #[arg(value_name = "QUERY")]
    query: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Show indexer status (entry count, volumes, RSS, uptime, state).
    Status {
        /// Print the raw Response JSON line instead of a table.
        #[arg(long = "json")]
        json: bool,
    },
    /// Ask the service to rescan, optionally one volume (e.g. `flk rescan C`).
    Rescan {
        /// Single volume letter, e.g. `C`.
        #[arg(value_name = "VOLUME")]
        volume: Option<String>,
    },
    /// List indexed volumes with their live/enabled/monitor flags.
    Volumes,
    /// Change one indexed volume: enable/disable, monitor on/off, or remove it.
    Volume {
        /// Volume letter, e.g. `C`.
        #[arg(value_name = "VOLUME")]
        volume: String,
        /// Include the volume's entries in search results.
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        /// Hide the volume's entries from search results (kept indexed and tailed).
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
        /// Tail the volume's journal for live updates.
        #[arg(long, conflicts_with = "no_monitor")]
        monitor: bool,
        /// Scan-only: index once, run no live tail.
        #[arg(long = "no-monitor", conflicts_with = "monitor")]
        no_monitor: bool,
        /// Drop the volume record and all its entries from the index.
        #[arg(long, conflicts_with_all = ["enable", "disable", "monitor", "no_monitor"])]
        remove: bool,
    },
    /// Show or replace the NTFS-targets policy (which volumes the daemon auto-indexes).
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Ask a running `flokid` to save its index and stop.
    Shutdown,
    /// Serve MCP over stdin/stdout for an AI assistant on this PC (no port,
    /// no token). Register it with e.g. `claude mcp add floki -- flk mcp`.
    Mcp,
    /// Run the 20 canned queries and check the latency/RAM gate.
    Bench {
        /// How many times to run each canned query.
        #[arg(long = "iters", default_value_t = 5, value_name = "N")]
        iters: u32,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Replace the NTFS-targets policy; unspecified flags keep their current values.
    Set {
        /// Auto-index newly arrived fixed NTFS volumes.
        #[arg(long = "auto-fixed", conflicts_with = "no_auto_fixed")]
        auto_fixed: bool,
        /// Do not auto-index newly arrived fixed NTFS volumes.
        #[arg(long = "no-auto-fixed", conflicts_with = "auto_fixed")]
        no_auto_fixed: bool,
        /// Auto-index newly arrived removable NTFS volumes.
        #[arg(long = "auto-removable", conflicts_with = "no_auto_removable")]
        auto_removable: bool,
        /// Do not auto-index newly arrived removable NTFS volumes.
        #[arg(long = "no-auto-removable", conflicts_with = "auto_removable")]
        no_auto_removable: bool,
        /// Drop indexed volumes that stay unopenable (offline media).
        #[arg(
            long = "auto-remove-offline",
            conflicts_with = "no_auto_remove_offline"
        )]
        auto_remove_offline: bool,
        /// Keep indexed volumes that stay unopenable.
        #[arg(
            long = "no-auto-remove-offline",
            conflicts_with = "auto_remove_offline"
        )]
        no_auto_remove_offline: bool,
    },
}

/// Parse `-s/--sort name|-name|path|-path|modified|-modified|created|-created`.
fn parse_sort(s: &str) -> Result<Sort, String> {
    match s {
        "name" => Ok(Sort::NameAsc),
        "-name" => Ok(Sort::NameDesc),
        "path" => Ok(Sort::PathAsc),
        "-path" => Ok(Sort::PathDesc),
        "modified" => Ok(Sort::ModifiedAsc),
        "-modified" => Ok(Sort::ModifiedDesc),
        "created" => Ok(Sort::CreatedAsc),
        "-created" => Ok(Sort::CreatedDesc),
        "newest" => Ok(Sort::ModifiedDesc),
        "oldest" => Ok(Sort::ModifiedAsc),
        other => Err(format!(
            "invalid sort {other:?}; expected one of: name, -name, path, -path, \
             modified, -modified, created, -created, newest, oldest"
        )),
    }
}

/// Join a parent directory and a file name with Windows separators.
///
/// `path` may already end with `\` (drive roots like `C:\` do); only add a
/// separator when it is missing.
pub fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('\\') || dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

/// Nearest-rank percentile over ascending-sorted `u64` samples.
///
/// `pct` is in 0..=100 (clamped). Returns 0 for empty input.
pub fn percentile(sorted: &[u64], pct: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let pct = pct.clamp(0.0, 100.0);
    let rank = (pct / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Mirror of flokid's CPU policy (same box, same core count, fixed policy):
/// below-normal process priority, half the cores (at least 2) as search
/// threads, max 2 searches in flight. The pipe schema carries no free-form
/// field, so the daemon cannot send this line or its per-component RAM
/// breakdown over `Status` — those are in the daemon log instead.
fn cpu_policy_line() -> String {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let threads = (cores / 2).max(2);
    format!("below-normal priority, {threads} search threads, max 2 in flight")
}

fn state_label(state: &IndexState) -> String {
    match state {
        IndexState::Loading => "loading".to_owned(),
        IndexState::Ready => "ready".to_owned(),
        IndexState::Scanning { volume, done } => {
            format!("scanning (volume {volume}, done {done})")
        }
    }
}

/// Connect to `flokid`, or print the standard "not running" message and exit 2.
fn connect_or_exit() -> Client {
    match Client::connect() {
        Ok(client) => client,
        Err(_) => {
            eprintln!(
                "flokid is not running (pipe {}); start it with an elevated shell: flokid run",
                floki_proto::pipe_name()
            );
            std::process::exit(2);
        }
    }
}

fn call_or_exit_3(client: &mut Client, request: &Request) -> Response {
    match client.call(request) {
        Ok(response) => response,
        Err(err) => {
            eprintln!("Error: {err}");
            std::process::exit(3);
        }
    }
}

/// How `cmd_search` prints results.
#[derive(Debug, Clone, Copy, Default)]
struct SearchOutput {
    json: bool,
    long: bool,
    stats: bool,
}

/// One `-l` line: `YYYY-MM-DD HH:MM  <size>  <path>` (UTC; `-` when unknown).
fn long_line(hit: &floki_proto::HitRow) -> String {
    let when = hit
        .modified_ms
        .map_or_else(|| "-".to_owned(), format_utc_minute);
    let size = match (hit.is_dir, hit.size) {
        (true, _) => "<dir>".to_owned(),
        (false, Some(bytes)) => bytes.to_string(),
        (false, None) => "-".to_owned(),
    };
    format!(
        "{when:<16}  {size:>12}  {}",
        join_path(&hit.path, &hit.name)
    )
}

/// Unix ms as `YYYY-MM-DD HH:MM` UTC (civil-from-days, no time crate).
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

/// Write lines to stdout, stopping quietly when the reader goes away
/// (`flk ... | head` used to panic on the closed pipe).
fn write_lines(lines: impl Iterator<Item = String>) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for line in lines {
        if writeln!(out, "{line}").is_err() {
            return;
        }
    }
    let _ = out.flush();
}

fn cmd_search(
    query_words: &[String],
    max: u32,
    offset: u32,
    sort: Sort,
    show: SearchOutput,
) -> i32 {
    let query = query_words.join(" ");
    let mut client = connect_or_exit();
    let request = Request::Search {
        query,
        max_results: max,
        offset,
        sort,
        client_id: u64::from(std::process::id()),
        // Plain paths need no metadata; skip the per-row disk reads.
        meta: show.long || show.json,
    };
    let started = std::time::Instant::now();
    let response = call_or_exit_3(&mut client, &request);
    let round_trip_ms = started.elapsed().as_secs_f64() * 1000.0;
    if show.json {
        match serde_json::to_string(&response) {
            Ok(line) => write_lines(std::iter::once(line)),
            Err(err) => {
                eprintln!("Error: {err}");
                return 3;
            }
        }
    } else if let Response::Results { hits, .. } = &response {
        if show.long {
            write_lines(hits.iter().map(long_line));
        } else {
            write_lines(hits.iter().map(|h| join_path(&h.path, &h.name)));
        }
    }
    if show.stats {
        if let Response::Results {
            hits,
            total,
            elapsed_us,
        } = &response
        {
            eprintln!(
                "{} of {total} matches · service {:.1} ms · round trip {round_trip_ms:.1} ms",
                hits.len(),
                *elapsed_us as f64 / 1000.0
            );
        }
    }
    match response {
        Response::Results { hits, .. } => {
            if hits.is_empty() {
                1
            } else {
                0
            }
        }
        Response::Error { message } => {
            eprintln!("Error: {message}");
            3
        }
        _ => {
            eprintln!("Error: unexpected response from flokid");
            3
        }
    }
}

fn cmd_status(json: bool) -> i32 {
    let mut client = connect_or_exit();
    let response = call_or_exit_3(&mut client, &Request::Status {});
    if json {
        match serde_json::to_string(&response) {
            Ok(line) => println!("{line}"),
            Err(err) => {
                eprintln!("Error: {err}");
                return 3;
            }
        }
    }
    match response {
        Response::Status {
            entries,
            volumes,
            rss_bytes,
            uptime_s,
            state,
        } => {
            if !json {
                let rss_mb = rss_bytes as f64 / 1024.0 / 1024.0;
                println!("entries: {entries}");
                println!("rss: {rss_mb:.2} MB ({rss_bytes} bytes)");
                println!("uptime: {uptime_s}s");
                println!("state: {}", state_label(&state));
                println!("cpu: {}", cpu_policy_line());
                println!("volumes:");
                println!("  {:<4} {:>10} {:>12}  live", "vol", "entries", "next_usn");
                for vol in &volumes {
                    let live = if vol.live { "yes" } else { "no" };
                    println!(
                        "  {:<4} {:>10} {:>12}  {live}",
                        vol.letter, vol.entries, vol.next_usn
                    );
                }
            }
            0
        }
        Response::Error { message } => {
            eprintln!("Error: {message}");
            3
        }
        _ => {
            eprintln!("Error: unexpected response from flokid");
            3
        }
    }
}

fn parse_volume(raw: Option<&str>) -> Result<Option<char>, String> {
    match raw {
        None => Ok(None),
        Some(s) => {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphabetic() => Ok(Some(c.to_ascii_uppercase())),
                _ => Err(format!(
                    "invalid volume {s:?}; expected a single letter, e.g. `flk rescan C`"
                )),
            }
        }
    }
}

fn cmd_rescan(volume_raw: Option<&str>) -> i32 {
    let volume = match parse_volume(volume_raw) {
        Ok(volume) => volume,
        Err(err) => {
            eprintln!("Error: {err}");
            return 3;
        }
    };
    let mut client = connect_or_exit();
    let response = call_or_exit_3(&mut client, &Request::Rescan { volume });
    match response {
        Response::Ok {} => {
            println!("Ok");
            0
        }
        Response::Error { message } => {
            eprintln!("Error: {message}");
            3
        }
        _ => {
            eprintln!("Error: unexpected response from flokid");
            3
        }
    }
}

/// Print one acknowledgement (`Ok`) or one service error; anything else is a
/// protocol surprise. Shared by the volume/config mutating commands.
fn ok_or_error(response: Response, what: &str) -> i32 {
    match response {
        Response::Ok {} => {
            println!("Ok");
            0
        }
        Response::Error { message } => {
            eprintln!("Error: {message}");
            3
        }
        _ => {
            eprintln!("Error: unexpected {what} response from flokid");
            3
        }
    }
}

fn cmd_volumes() -> i32 {
    let mut client = connect_or_exit();
    let response = call_or_exit_3(&mut client, &Request::Status {});
    match response {
        Response::Status { volumes, .. } => {
            println!(
                "  {:<4} {:>10} {:>12}  {:<4} {:<7} {:<7}",
                "vol", "entries", "next_usn", "live", "enabled", "monitor"
            );
            for vol in &volumes {
                let live = if vol.live { "yes" } else { "no" };
                let enabled = if vol.enabled { "yes" } else { "no" };
                let monitor = if vol.monitor { "yes" } else { "no" };
                println!(
                    "  {:<4} {:>10} {:>12}  {live:<4} {enabled:<7} {monitor:<7}",
                    vol.letter, vol.entries, vol.next_usn
                );
            }
            0
        }
        Response::Error { message } => {
            eprintln!("Error: {message}");
            3
        }
        _ => {
            eprintln!("Error: unexpected response from flokid");
            3
        }
    }
}

fn cmd_volume(
    volume_raw: &str,
    enable: bool,
    disable: bool,
    monitor: bool,
    no_monitor: bool,
    remove: bool,
) -> i32 {
    let volume = match parse_volume(Some(volume_raw)) {
        Ok(Some(volume)) => volume,
        Ok(None) => {
            eprintln!("Error: expected a volume letter, e.g. `flk volume C`");
            return 3;
        }
        Err(err) => {
            eprintln!("Error: {err}");
            return 3;
        }
    };
    if remove {
        let mut client = connect_or_exit();
        let response = call_or_exit_3(&mut client, &Request::VolumesRemove { volume });
        return ok_or_error(response, "volumes_remove");
    }
    if enable == disable && monitor == no_monitor {
        eprintln!(
            "Error: nothing to do; pass --enable|--disable, --monitor|--no-monitor, or --remove"
        );
        return 3;
    }
    // `--enable` and `--disable` conflict at the clap level (as do
    // `--monitor`/`--no-monitor`), so at most one of each pair is set here.
    let mut client = connect_or_exit();
    if enable || disable {
        let response = call_or_exit_3(
            &mut client,
            &Request::VolumesSetEnabled {
                volume,
                enabled: enable,
            },
        );
        let code = ok_or_error(response, "volumes_set_enabled");
        if code != 0 {
            return code;
        }
    }
    if monitor || no_monitor {
        let response = call_or_exit_3(&mut client, &Request::VolumesSetMonitor { volume, monitor });
        let code = ok_or_error(response, "volumes_set_monitor");
        if code != 0 {
            return code;
        }
    }
    0
}

fn print_targets(
    auto_include_fixed: bool,
    auto_include_removable: bool,
    auto_remove_offline: bool,
) {
    println!("auto_include_fixed: {}", yes_no(auto_include_fixed));
    println!("auto_include_removable: {}", yes_no(auto_include_removable));
    println!("auto_remove_offline: {}", yes_no(auto_remove_offline));
}

fn yes_no(flag: bool) -> &'static str {
    if flag {
        "yes"
    } else {
        "no"
    }
}

fn cmd_config(action: Option<ConfigAction>) -> i32 {
    match action {
        None => {
            let mut client = connect_or_exit();
            let response = call_or_exit_3(&mut client, &Request::TargetsConfigGet {});
            match response {
                Response::TargetsConfig {
                    auto_include_fixed,
                    auto_include_removable,
                    auto_remove_offline,
                } => {
                    print_targets(
                        auto_include_fixed,
                        auto_include_removable,
                        auto_remove_offline,
                    );
                    0
                }
                Response::Error { message } => {
                    eprintln!("Error: {message}");
                    3
                }
                _ => {
                    eprintln!("Error: unexpected response from flokid");
                    3
                }
            }
        }
        Some(ConfigAction::Set {
            auto_fixed,
            no_auto_fixed,
            auto_removable,
            no_auto_removable,
            auto_remove_offline,
            no_auto_remove_offline,
        }) => {
            // Patch semantics: read the current policy first so unspecified
            // flags keep their values.
            let mut client = connect_or_exit();
            let current = match call_or_exit_3(&mut client, &Request::TargetsConfigGet {}) {
                Response::TargetsConfig {
                    auto_include_fixed,
                    auto_include_removable,
                    auto_remove_offline,
                } => (
                    auto_include_fixed,
                    auto_include_removable,
                    auto_remove_offline,
                ),
                Response::Error { message } => {
                    eprintln!("Error: {message}");
                    return 3;
                }
                _ => {
                    eprintln!("Error: unexpected response from flokid");
                    return 3;
                }
            };
            let (mut fixed, mut removable, mut offline) = current;
            if auto_fixed {
                fixed = true;
            }
            if no_auto_fixed {
                fixed = false;
            }
            if auto_removable {
                removable = true;
            }
            if no_auto_removable {
                removable = false;
            }
            if auto_remove_offline {
                offline = true;
            }
            if no_auto_remove_offline {
                offline = false;
            }
            let response = call_or_exit_3(
                &mut client,
                &Request::TargetsConfigSet {
                    auto_include_fixed: fixed,
                    auto_include_removable: removable,
                    auto_remove_offline: offline,
                },
            );
            match response {
                Response::TargetsConfig {
                    auto_include_fixed,
                    auto_include_removable,
                    auto_remove_offline,
                } => {
                    print_targets(
                        auto_include_fixed,
                        auto_include_removable,
                        auto_remove_offline,
                    );
                    0
                }
                Response::Ok {} => {
                    println!("Ok");
                    0
                }
                Response::Error { message } => {
                    eprintln!("Error: {message}");
                    3
                }
                _ => {
                    eprintln!("Error: unexpected response from flokid");
                    3
                }
            }
        }
    }
}

fn cmd_shutdown() -> i32 {
    let mut client = connect_or_exit();
    let response = call_or_exit_3(&mut client, &Request::Shutdown {});
    match response {
        Response::Ok {} => {
            println!("flokid stopping");
            0
        }
        Response::Error { message } => {
            eprintln!("Error: {message}");
            3
        }
        _ => {
            eprintln!("Error: unexpected response from flokid");
            3
        }
    }
}
fn cmd_bench(iters: u32) -> i32 {
    let iters = iters.max(1);
    let mut client = connect_or_exit();
    let client_id = u64::from(std::process::id());

    // RSS gate uses the daemon RSS sampled at REST, before the query loop:
    // sustained searches leave ~62 MB of deliberate reusable per-query
    // scratch resident (bounded and stable, not a leak). The warm figure is
    // reported after the loop for transparency only.
    let rest_status = call_or_exit_3(&mut client, &Request::Status {});
    let (entries, rest_rss_bytes) = match rest_status {
        Response::Status {
            entries, rss_bytes, ..
        } => (entries, rss_bytes),
        Response::Error { message } => {
            eprintln!("Error: {message}");
            return 3;
        }
        _ => {
            eprintln!("Error: unexpected response from flokid");
            return 3;
        }
    };

    let mut service_us: Vec<u64> = Vec::with_capacity(CANNED_QUERIES.len() * iters as usize);
    let mut wall_us: Vec<u64> = Vec::with_capacity(CANNED_QUERIES.len() * iters as usize);
    for _ in 0..iters {
        for canned in &CANNED_QUERIES {
            let request = Request::Search {
                query: (*canned).to_owned(),
                max_results: 100,
                offset: 0,
                sort: Sort::NameAsc,
                client_id,
                meta: true,
            };
            let started = Instant::now();
            let response = call_or_exit_3(&mut client, &request);
            let wall = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            match response {
                Response::Results { elapsed_us, .. } => {
                    service_us.push(elapsed_us);
                    wall_us.push(wall);
                }
                Response::Error { message } => {
                    eprintln!("Error: {message}");
                    return 3;
                }
                _ => {
                    eprintln!("Error: unexpected response from flokid");
                    return 3;
                }
            }
        }
    }

    let warm_status = call_or_exit_3(&mut client, &Request::Status {});
    let warm_rss_bytes = match warm_status {
        Response::Status { rss_bytes, .. } => rss_bytes,
        Response::Error { message } => {
            eprintln!("Error: {message}");
            return 3;
        }
        _ => {
            eprintln!("Error: unexpected response from flokid");
            return 3;
        }
    };

    service_us.sort_unstable();
    wall_us.sort_unstable();
    let service_p50 = percentile(&service_us, 50.0);
    let service_p95 = percentile(&service_us, 95.0);
    let wall_p50 = percentile(&wall_us, 50.0);
    let wall_p95 = percentile(&wall_us, 95.0);

    let runs = service_us.len();
    println!(
        "bench: {} queries x {iters} iters = {runs} runs (one connection)",
        CANNED_QUERIES.len()
    );
    println!("metric    p50_us    p95_us");
    println!("service   {service_p50:<8}  {service_p95}");
    println!("wall      {wall_p50:<8}  {wall_p95}");
    let rest_mb = rest_rss_bytes as f64 / 1024.0 / 1024.0;
    let warm_mb = warm_rss_bytes as f64 / 1024.0 / 1024.0;
    println!("entries: {entries}");
    if entries == 0 {
        println!("rss at rest: {rest_mb:.2} MB (n/a: zero entries)");
    } else {
        let rest_per_million_mb =
            rest_rss_bytes as f64 / entries as f64 * 1_000_000.0 / 1024.0 / 1024.0;
        println!("rss at rest: {rest_mb:.2} MB ({rest_per_million_mb:.2} MB/1M entries)");
    }
    println!("rss warm (after queries): {warm_mb:.2} MB ({warm_rss_bytes} bytes)");

    let wall_ok = wall_p95 < P95_WALL_GATE_US;
    let wall_ms = wall_p95 as f64 / 1000.0;
    println!(
        "gate p95 wall < 50 ms: {} ({wall_ms:.3} ms)",
        if wall_ok { "ok" } else { "FAIL" }
    );

    let rss_ok = if entries == 0 {
        println!("gate rss/1M at rest <= 70 MB: n/a (zero entries)");
        true
    } else {
        let per_million = rest_rss_bytes as f64 / entries as f64 * 1_000_000.0;
        let per_million_mb = per_million / 1024.0 / 1024.0;
        let ok = per_million <= RSS_PER_MILLION_GATE_BYTES;
        println!(
            "gate rss/1M at rest <= 70 MB: {} ({per_million_mb:.2} MB)",
            if ok { "ok" } else { "FAIL" }
        );
        ok
    };

    if wall_ok && rss_ok {
        println!("PASS");
        0
    } else {
        println!("FAIL");
        3
    }
}

/// MCP stdio transport: one JSON-RPC message per line in, one answer per
/// line out. Nothing else may touch stdout (it is the protocol channel).
fn cmd_mcp() -> i32 {
    use std::io::{BufRead, Write};
    let backend = floki_mcp::PipeBackend;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return 0 };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(msg) => floki_mcp::protocol::handle(&msg, &backend),
            Err(e) => Some(serde_json::json!({
                "jsonrpc": "2.0", "id": null,
                "error": { "code": -32700, "message": format!("Parse error: {e}") }
            })),
        };
        if let Some(reply) = reply {
            if writeln!(stdout, "{reply}")
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return 0;
            }
        }
    }
    0
}

fn run() -> i32 {
    let cli = Cli::parse();
    if let Some(pipe) = cli.pipe.as_deref() {
        std::env::set_var("FLOKI_PIPE", pipe);
    }
    match cli.command {
        Some(Commands::Status { json }) => cmd_status(json),
        Some(Commands::Rescan { volume }) => cmd_rescan(volume.as_deref()),
        Some(Commands::Volumes) => cmd_volumes(),
        Some(Commands::Volume {
            volume,
            enable,
            disable,
            monitor,
            no_monitor,
            remove,
        }) => cmd_volume(&volume, enable, disable, monitor, no_monitor, remove),
        Some(Commands::Config { action }) => cmd_config(action),
        Some(Commands::Shutdown) => cmd_shutdown(),
        Some(Commands::Bench { iters }) => cmd_bench(iters),
        Some(Commands::Mcp) => cmd_mcp(),
        None => cmd_search(
            &cli.query,
            cli.max,
            cli.offset,
            cli.sort,
            SearchOutput {
                json: cli.json,
                long: cli.long,
                stats: cli.stats,
            },
        ),
    }
}

fn main() {
    std::process::exit(run());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_drive_root_does_not_double_separator() {
        assert_eq!(join_path("C:\\", "foo.txt"), "C:\\foo.txt");
    }

    #[test]
    fn join_nested_dir_adds_separator() {
        assert_eq!(join_path("C:\\docs", "foo.txt"), "C:\\docs\\foo.txt");
    }

    #[test]
    fn join_trailing_forward_slash_is_kept_as_is() {
        assert_eq!(join_path("C:/docs/", "foo.txt"), "C:/docs/foo.txt");
    }

    #[test]
    fn sort_flag_parses_all_eight_forms() {
        assert_eq!(parse_sort("name"), Ok(Sort::NameAsc));
        assert_eq!(parse_sort("-name"), Ok(Sort::NameDesc));
        assert_eq!(parse_sort("path"), Ok(Sort::PathAsc));
        assert_eq!(parse_sort("-path"), Ok(Sort::PathDesc));
        assert_eq!(parse_sort("modified"), Ok(Sort::ModifiedAsc));
        assert_eq!(parse_sort("-modified"), Ok(Sort::ModifiedDesc));
        assert_eq!(parse_sort("created"), Ok(Sort::CreatedAsc));
        assert_eq!(parse_sort("-created"), Ok(Sort::CreatedDesc));
        assert_eq!(parse_sort("newest"), Ok(Sort::ModifiedDesc));
        assert_eq!(parse_sort("oldest"), Ok(Sort::ModifiedAsc));
    }

    /// `-s -modified` (hyphen value) parses as the sort, not a flag.
    #[test]
    fn hyphen_sort_value_is_accepted() {
        let cli = Cli::try_parse_from(["flk", "-s", "-modified", "python"]).expect("parses");
        assert_eq!(cli.sort, Sort::ModifiedDesc);
        assert_eq!(cli.query, ["python"]);
    }

    #[test]
    fn long_line_shows_utc_minute_size_and_path() {
        let hit = floki_proto::HitRow {
            name: "a.txt".to_owned(),
            path: r"C:\d".to_owned(),
            is_dir: false,
            size: Some(42),
            modified_ms: Some(1_700_000_000_000),
            created_ms: None,
        };
        assert_eq!(
            long_line(&hit),
            format!("2023-11-14 22:13  {:>12}  C:\\d\\a.txt", 42)
        );
        assert_eq!(format_utc_minute(0), "1970-01-01 00:00");
    }

    #[test]
    fn sort_flag_rejects_anything_else() {
        assert!(parse_sort("Name").is_err());
        assert!(parse_sort("size").is_err());
        assert!(parse_sort("").is_err());
    }

    #[test]
    fn percentile_nearest_rank() {
        let samples = vec![10, 20, 30, 40];
        assert_eq!(percentile(&samples, 0.0), 10);
        assert_eq!(percentile(&samples, 25.0), 10);
        assert_eq!(percentile(&samples, 50.0), 20);
        assert_eq!(percentile(&samples, 95.0), 40);
        assert_eq!(percentile(&samples, 100.0), 40);
    }

    #[test]
    fn percentile_single_sample_and_empty() {
        assert_eq!(percentile(&[7], 50.0), 7);
        assert_eq!(percentile(&[], 50.0), 0);
    }

    #[test]
    fn rescan_volume_accepts_single_letter_case_insensitive() {
        assert_eq!(parse_volume(None), Ok(None));
        assert_eq!(parse_volume(Some("C")), Ok(Some('C')));
        assert_eq!(parse_volume(Some("d")), Ok(Some('D')));
        assert!(parse_volume(Some("CC")).is_err());
        assert!(parse_volume(Some("")).is_err());
        assert!(parse_volume(Some("1")).is_err());
    }
}
