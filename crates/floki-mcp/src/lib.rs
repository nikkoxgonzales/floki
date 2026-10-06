//! MCP (Model Context Protocol) server for Floki: lets AI assistants search
//! file names through the running indexer.
//!
//! - [`adapter`] turns indexer answers into compact text (the token budget
//!   lives there).
//! - [`protocol`] is the JSON-RPC layer: `initialize`, `tools/list`,
//!   `tools/call`, independent of transport.
//! - [`http`] serves it over Streamable HTTP with a bearer token (hosted by
//!   the tray window, configured in Settings); `flk mcp` serves it over
//!   stdio.
//! - [`backend`] reaches the indexer over its named pipe.
//!
//! Every answer is written to cost an assistant as few tokens as possible: a
//! one-line summary first, capped results that say how to get the rest,
//! errors that say what to do next.

pub mod adapter;
pub mod backend;
pub mod config;
pub mod http;
pub mod protocol;

pub use backend::{Backend, PipeBackend};
pub use config::McpConfig;
pub use http::McpServer;
