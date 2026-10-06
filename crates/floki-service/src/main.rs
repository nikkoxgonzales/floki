//! `flokid`: the Floki indexer daemon (elevated) + pipe server.

use clap::{Parser, Subcommand};
use floki_proto::{Request, Response};

/// Floki indexer daemon: builds the file-name index from NTFS volumes and
/// serves search requests over the named pipe.
#[derive(Debug, Parser)]
#[command(
    name = "flokid",
    version,
    about = "Floki indexer daemon",
    after_help = concat!("Floki by Nikko Gonzales: ", env!("CARGO_PKG_REPOSITORY"))
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the indexer in the foreground (requires elevation).
    Run {
        /// Restrict to these volumes, e.g. `--volumes C,D` (default: all NTFS).
        #[arg(long, value_name = "LETS")]
        volumes: Option<String>,
        /// Skip loading the saved index even when present.
        #[arg(long)]
        no_load: bool,
        /// Override the pipe name (default: `FLOKI_PIPE` env or `\\.\pipe\floki`).
        #[arg(long, value_name = "NAME")]
        pipe: Option<String>,
        /// Hide the console window (UI-owned launch).
        #[arg(long)]
        hidden: bool,
    },
    /// Print daemon status from the pipe (exit 2 when unreachable).
    Status,
    /// Register autostart at logon (elevated scheduled task, no UAC prompt).
    Install,
    /// Remove the autostart task.
    Uninstall,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            volumes,
            no_load,
            pipe,
            hidden,
        } => {
            let volumes = volumes.map(parse_volumes).transpose()?;
            floki_service::daemon::run(floki_service::daemon::RunOptions {
                volumes,
                no_load,
                pipe,
                hidden,
            })
        }
        Command::Status => status(),
        Command::Install => floki_service::install::install(),
        Command::Uninstall => floki_service::install::uninstall(),
    }
}

/// Parse `--volumes C,D` into uppercase drive letters.
fn parse_volumes(raw: String) -> anyhow::Result<Vec<char>> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let letter = part.trim().to_ascii_uppercase();
        let mut chars = letter.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_alphabetic() => {
                if !out.contains(&c) {
                    out.push(c);
                }
            }
            _ => anyhow::bail!("invalid --volumes entry {part:?}; expected letters like C,D"),
        }
    }
    if out.is_empty() {
        anyhow::bail!("--volumes needs at least one letter, e.g. C,D");
    }
    Ok(out)
}

/// Query the daemon over the pipe and pretty-print its status.
fn status() -> anyhow::Result<()> {
    let mut client = match floki_proto::Client::connect() {
        Ok(client) => client,
        Err(e) => {
            eprintln!("flokid is not running ({e})");
            std::process::exit(2);
        }
    };
    let response = match client.call(&Request::Status {}) {
        Ok(response) => response,
        Err(e) => {
            eprintln!("flokid did not answer ({e})");
            std::process::exit(2);
        }
    };
    match response {
        Response::Status {
            entries,
            volumes,
            rss_bytes,
            uptime_s,
            state,
        } => {
            println!("entries: {entries}");
            println!("state: {state:?}");
            println!("uptime: {uptime_s}s");
            println!("rss: {} MB", rss_bytes / (1024 * 1024));
            for volume in &volumes {
                println!(
                    "volume {}: entries={} next_usn={} live={}",
                    volume.letter, volume.entries, volume.next_usn, volume.live
                );
            }
        }
        Response::Error { message } => {
            eprintln!("flokid error: {message}");
            std::process::exit(2);
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            std::process::exit(2);
        }
    }
    Ok(())
}
