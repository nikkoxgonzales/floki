//! `floki-bench`: fast offline performance measurement over a saved index.
//!
//! Usage: `floki-bench <index.bin> [--threads N] [--iters K] [--queries file] [--json]`

use clap::Parser;
use floki_bench::{bench_file, default_threads, BenchReport, CANNED_QUERIES};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "floki-bench",
    version,
    about = "Offline bench over a saved floki index (counts and timings only — never prints names or paths)"
)]
struct Cli {
    /// Saved index file (written by flokid save).
    index: PathBuf,
    /// Search threads (default: max(2, cores/2)).
    #[arg(long)]
    threads: Option<usize>,
    /// Times to run each query.
    #[arg(long, default_value_t = 5)]
    iters: usize,
    /// File with one query per line (default: 20 canned queries).
    #[arg(long)]
    queries: Option<PathBuf>,
    /// Emit one JSON object instead of human-readable text.
    #[arg(long)]
    json: bool,
}

fn load_queries(path: Option<&PathBuf>) -> anyhow::Result<Vec<String>> {
    match path {
        None => Ok(CANNED_QUERIES.iter().map(|s| (*s).to_owned()).collect()),
        Some(file) => {
            let text = std::fs::read_to_string(file)?;
            let queries: Vec<String> = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            if queries.is_empty() {
                anyhow::bail!("no queries in {}", file.display());
            }
            Ok(queries)
        }
    }
}

fn print_text(report: &BenchReport) {
    let m = &report.memory;
    println!("entries: {}", report.entries);
    println!("volumes: {}", report.volumes);
    println!("load_ms: {:.3}", report.load_ms);
    println!(
        "memory_bytes: entries={} arena={} by_name={} frn_index={} entry_vol={} pending={} arena_aux={} total={} tombstone_info={}",
        m.entries_bytes,
        m.arena_bytes,
        m.by_name_bytes,
        m.frn_index_bytes,
        m.entry_vol_bytes,
        m.pending_bytes,
        m.arena_aux_bytes,
        m.total_bytes,
        m.tombstone_bytes,
    );
    println!("pending: {}", report.pending_len);
    println!("tombstones: {}", report.tombstones);
    println!("rss_before_bytes: {}", report.rss_before_bytes);
    for (i, q) in report.queries.iter().enumerate() {
        println!(
            "q{i:02} hits={} min_ms={:.3} median_ms={:.3} query={:?}",
            q.hits, q.min_ms, q.median_ms, q.query
        );
    }
    println!(
        "overall: runs={} p50_ms={:.3} p95_ms={:.3}",
        report.runs, report.p50_ms, report.p95_ms
    );
    println!("rss_after_bytes: {}", report.rss_after_bytes);
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let threads = cli.threads.unwrap_or_else(default_threads);
    let queries = load_queries(cli.queries.as_ref())?;
    let report = bench_file(&cli.index, threads, cli.iters, &queries)?;
    if cli.json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        print_text(&report);
    }
    Ok(())
}
