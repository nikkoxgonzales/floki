//! floki-service library: the `flokid` indexer daemon's building blocks.
//!
//! The binary (`src/main.rs`) is a thin CLI wrapper; everything testable lives
//! here: NTFS attribute/event mapping, `schtasks` autostart vectors, data-dir
//! paths, shared daemon state, the pipe server, and the `run` startup sequence.

pub mod daemon;
pub mod install;
pub mod mapping;
pub mod paths;
pub mod server;
pub mod state;
