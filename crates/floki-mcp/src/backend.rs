//! How the MCP layer reaches the indexer. A trait so the protocol and HTTP
//! layers are testable without a running `flokid`.

use floki_proto::{Request, Response};

/// One request, one answer. `Err` is a transport failure, already phrased
/// for the model (what happened, what to do).
pub trait Backend: Send + Sync {
    /// # Errors
    /// The indexer could not be reached or the pipe broke.
    fn call(&self, request: &Request) -> Result<Response, String>;
}

/// The running indexer, over its named pipe (one connection per call; a
/// pipe connect costs microseconds).
#[derive(Debug, Default, Clone, Copy)]
pub struct PipeBackend;

impl Backend for PipeBackend {
    #[cfg(windows)]
    fn call(&self, request: &Request) -> Result<Response, String> {
        let mut client = floki_proto::Client::connect().map_err(|_| {
            "The Floki indexer is not running on this PC. Start it from the Floki \
             window, then retry."
                .to_owned()
        })?;
        client
            .call(request)
            .map_err(|e| format!("The Floki indexer stopped answering ({e}). Retry shortly."))
    }

    #[cfg(not(windows))]
    fn call(&self, _request: &Request) -> Result<Response, String> {
        Err("Floki runs on Windows only.".to_owned())
    }
}
