//! Dev harness: serve MCP over HTTP against the running indexer, outside
//! the window, so the endpoint can be checked with curl.
//!
//! `cargo run -p floki-mcp --example serve -- 7458 <token> [seconds]`
//! listens on 127.0.0.1:7458 with that token, stops after `seconds`
//! (default 60).

use std::sync::Arc;
use std::time::Duration;

use floki_mcp::{McpConfig, McpServer, PipeBackend};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port = args.get(1).and_then(|p| p.parse().ok()).unwrap_or(7458);
    let token = args
        .get(2)
        .cloned()
        .unwrap_or_else(floki_mcp::config::new_token);
    let secs = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(60);
    let cfg = McpConfig {
        enabled: true,
        lan: false,
        port,
        token,
    };
    let server = McpServer::start(&cfg, Arc::new(PipeBackend)).expect("bind");
    eprintln!("MCP at {} for {secs}s", cfg.local_url());
    std::thread::sleep(Duration::from_secs(secs));
    drop(server);
}
