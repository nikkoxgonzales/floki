//! floki-bench: fast offline performance measurement over a saved index.
//!
//! PRIVACY: this tool reports aggregate counts and timings only. It never
//! prints entry names, paths, or hit contents. The search loop below reads
//! `result.total` and drops the page without touching any hit field; no code
//! in this crate formats a [`floki_core::Hit`], an entry name, or a path.
//! Per-query rows are labeled by the workload's query string (caller input,
//! not index content) plus counts and timings.

use floki_core::{Index, SearchOptions, Sort};
use serde::Serialize;
use std::path::Path;
use std::time::Instant;

/// Canned bench workload: 20 queries, copied from `floki-cli` (do not import
/// that crate — this one must stay a pure offline tool over `floki-core`).
pub const CANNED_QUERIES: [&str; 20] = [
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

/// Page size per query, mirroring the daemon's bench path.
pub const PAGE_SIZE: u32 = 100;

/// Default search threads: half the cores, at least 2 (same policy as the
/// daemon's `search_thread_budget`).
#[must_use]
pub fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .checked_div(2)
        .unwrap_or(2)
        .max(2)
}

/// Nearest-rank percentile over ascending-sorted `u64` samples.
///
/// `pct` is in 0..=100 (clamped). Returns 0 for empty input.
#[must_use]
pub fn percentile(sorted: &[u64], pct: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let pct = pct.clamp(0.0, 100.0);
    let rank = (pct / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Median (p50) over ascending-sorted samples. Returns 0 for empty input.
#[must_use]
pub fn median_sorted(sorted: &[u64]) -> u64 {
    percentile(sorted, 50.0)
}

/// Current process resident set size in bytes (0 on failure / non-Windows).
#[must_use]
pub fn current_rss_bytes() -> u64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: pseudo-handle needs no cleanup; `counters` is a live struct
        // of the documented size; the call is synchronous.
        unsafe {
            let process = GetCurrentProcess();
            let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let ok = GetProcessMemoryInfo(
                process,
                &mut counters,
                size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            );
            if ok == 0 {
                return 0;
            }
            counters.WorkingSetSize as u64
        }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Byte-level memory counters for one loaded index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MemoryStat {
    pub entries_bytes: u64,
    pub arena_bytes: u64,
    pub by_name_bytes: u64,
    pub frn_index_bytes: u64,
    pub entry_vol_bytes: u64,
    pub pending_bytes: u64,
    pub arena_aux_bytes: u64,
    pub total_bytes: u64,
    /// Informational subset of `entries_bytes`, not an additional term.
    pub tombstone_bytes: u64,
}

/// Timing summary for one query over all its runs (counts + timings only).
#[derive(Debug, Clone, Serialize)]
pub struct QueryStat {
    /// Workload label (caller input), never index content.
    pub query: String,
    /// Total matches (`SearchResult::total`), first run.
    pub hits: u64,
    pub runs: usize,
    pub min_ms: f64,
    pub median_ms: f64,
}

/// Full offline bench report: structural stats plus latency numbers.
#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    pub threads: usize,
    pub iters: usize,
    pub entries: usize,
    pub volumes: usize,
    pub pending_len: usize,
    pub tombstones: usize,
    pub load_ms: f64,
    pub rss_before_bytes: u64,
    pub rss_after_bytes: u64,
    pub memory: MemoryStat,
    pub queries: Vec<QueryStat>,
    /// Runs across all queries (`queries.len() * iters`).
    pub runs: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
}

/// Run every query `iters` times against a loaded index, exactly as the
/// daemon does: [`floki_core::parse`] + [`floki_core::search_paged`] with
/// page 100, offset 0, [`Sort::NameAsc`], no `prev` narrowing.
///
/// Returns per-query stats plus every run's wall time in micros (unsorted).
fn run_queries(index: &Index, queries: &[String], iters: usize) -> (Vec<QueryStat>, Vec<u64>) {
    let opts = SearchOptions::new(PAGE_SIZE, 0, Sort::NameAsc);
    let mut stats = Vec::with_capacity(queries.len());
    let mut all_us = Vec::with_capacity(queries.len() * iters);
    for query_text in queries {
        let mut samples_us = Vec::with_capacity(iters);
        let mut hits = 0u64;
        for _ in 0..iters {
            let started = Instant::now();
            let query = floki_core::parse(query_text);
            // PRIVACY: only `total` is read; the page is dropped without
            // touching any hit field, name, or path.
            let total = floki_core::search_paged(index, &query, &opts, None).total;
            let elapsed_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            if samples_us.is_empty() {
                hits = total;
            }
            samples_us.push(elapsed_us);
            all_us.push(elapsed_us);
        }
        samples_us.sort_unstable();
        let min_us = samples_us[0];
        stats.push(QueryStat {
            query: query_text.clone(),
            hits,
            runs: iters,
            min_ms: min_us as f64 / 1000.0,
            median_ms: median_sorted(&samples_us) as f64 / 1000.0,
        });
    }
    (stats, all_us)
}

/// Load the saved index at `path` and bench it. Installs the search thread
/// pool via [`floki_core::set_search_threads`] (first call per process wins;
/// later calls are ignored by the core pool).
///
/// # Errors
///
/// Returns an error when the index file cannot be loaded.
pub fn bench_file(
    path: &Path,
    threads: usize,
    iters: usize,
    queries: &[String],
) -> anyhow::Result<BenchReport> {
    let iters = iters.max(1);
    let threads = threads.max(1);
    let rss_before_bytes = current_rss_bytes();
    let started = Instant::now();
    let index = Index::load(path)?;
    let load_ms = started.elapsed().as_secs_f64() * 1000.0;
    let _ = floki_core::set_search_threads(threads);

    let (queries_stat, mut all_us) = run_queries(&index, queries, iters);
    all_us.sort_unstable();
    let runs = all_us.len();
    let report = BenchReport {
        threads,
        iters,
        entries: index.len(),
        volumes: index.volumes.len(),
        pending_len: index.pending_len(),
        tombstones: index.tombstone_count(),
        load_ms,
        rss_before_bytes,
        rss_after_bytes: current_rss_bytes(),
        memory: {
            let b = index.memory_breakdown();
            MemoryStat {
                entries_bytes: b.entries_bytes,
                arena_bytes: b.arena_bytes,
                by_name_bytes: b.by_name_bytes,
                frn_index_bytes: b.frn_index_bytes,
                entry_vol_bytes: b.entry_vol_bytes,
                pending_bytes: b.pending_bytes,
                arena_aux_bytes: b.arena_aux_bytes,
                total_bytes: b.total_bytes(),
                tombstone_bytes: b.tombstone_bytes,
            }
        },
        queries: queries_stat,
        runs,
        p50_ms: percentile(&all_us, 50.0) as f64 / 1000.0,
        p95_ms: percentile(&all_us, 95.0) as f64 / 1000.0,
    };
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn percentile_clamps_out_of_range() {
        let samples = vec![10, 20, 30, 40];
        assert_eq!(percentile(&samples, 200.0), 40);
        assert_eq!(percentile(&samples, -5.0), 10);
    }

    #[test]
    fn median_sorted_matches_p50() {
        let samples = vec![3, 1, 2];
        let mut sorted = samples;
        sorted.sort_unstable();
        assert_eq!(median_sorted(&sorted), 2);
        assert_eq!(median_sorted(&[]), 0);
    }

    #[test]
    fn default_threads_is_at_least_two() {
        assert!(default_threads() >= 2);
    }

    fn build_small_index() -> Index {
        let mut index = Index::new();
        let vol = index.add_volume(floki_core::Volume {
            letter: 'C',
            guid: [0u8; 16],
            journal_id: 0,
            next_usn: 0,
            root_frn: 5,
            enabled: true,
            monitor: true,
        });
        let names = [
            "readme.txt",
            "config.toml",
            "settings.json",
            "app.log",
            "notepad.exe",
            "driver.dll",
            "photo.png",
            "cache.dat",
            "node_modules",
            "program files",
        ];
        for (i, name) in names.iter().enumerate() {
            index.push(vol, 100 + i as u64, 5, name, 0);
        }
        // Pad with filler entries so counts/timings are over a wider scan.
        for i in 0..200 {
            let name = format!("filler_{i:04}.bin");
            index.push(vol, 1000 + i as u64, 5, &name, 0);
        }
        index.finalize();
        index.rebuild_by_name();
        index
    }

    #[test]
    fn benches_a_small_saved_index() {
        let index = build_small_index();
        let expected_entries = index.len();
        let path =
            std::env::temp_dir().join(format!("floki_bench_test_{}.bin", std::process::id()));
        index.save(&path).expect("save small test index");
        let canned: Vec<String> = CANNED_QUERIES.iter().map(|s| (*s).to_owned()).collect();
        let report = bench_file(&path, 2, 2, &canned).expect("bench small test index");
        let _ = std::fs::remove_file(&path);
        assert_eq!(report.entries, expected_entries);
        assert_eq!(report.volumes, 1);
        assert_eq!(report.queries.len(), CANNED_QUERIES.len());
        assert_eq!(report.runs, CANNED_QUERIES.len() * 2);
        assert_eq!(report.iters, 2);
        assert!(report.queries.iter().any(|q| q.hits > 0));
        assert!(report.memory.total_bytes > 0);
    }
}
