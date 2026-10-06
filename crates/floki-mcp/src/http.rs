//! MCP Streamable HTTP transport: `POST /mcp` carries one JSON-RPC message
//! and gets one JSON answer (or `202` for a notification). No SSE stream,
//! no sessions: every Floki tool answers in one shot.
//!
//! Security, in the order checks run:
//! 1. `Origin`, when a browser sends one, must be this PC (blocks DNS
//!    rebinding from web pages).
//! 2. `Authorization: Bearer <token>`, compared in constant time, before the
//!    body is read, so a client without the token cannot hold the thread.
//! 3. Bodies over [`MAX_BODY`] are refused.
//!
//! One thread serves requests one at a time, so MCP never has more than one
//! search in flight against the indexer.

use std::io::{self, Read};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;
use tiny_http::{Header, Method, Response, Server};

use crate::backend::Backend;
use crate::config::McpConfig;
use crate::protocol;

/// Largest request body accepted (a tool call is a few hundred bytes).
pub const MAX_BODY: usize = 256 * 1024;

/// A running MCP endpoint. Dropping it stops it.
pub struct McpServer {
    server: Option<Arc<Server>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    addr: SocketAddr,
}

impl McpServer {
    /// Bind `config.bind_addr()` and serve on a background thread.
    ///
    /// # Errors
    /// The port is taken or the address cannot be bound (the message says
    /// which, for the Settings window).
    pub fn start(config: &McpConfig, backend: Arc<dyn Backend>) -> io::Result<Self> {
        let server = Arc::new(bind_with_retry(&config.bind_addr())?);
        let addr = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| io::Error::other("not an IP listener"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let token = config.token.clone();
        let thread = std::thread::Builder::new()
            .name("floki-mcp".to_owned())
            .spawn({
                let server = Arc::clone(&server);
                let stop = Arc::clone(&stop);
                move || serve(&server, &stop, &token, &*backend)
            })?;
        tracing::info!("MCP server listening on {addr}");
        Ok(Self {
            server: Some(server),
            stop,
            thread: Some(thread),
            addr,
        })
    }

    /// The bound address (the real port when started on port 0).
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(server) = &self.server {
            server.unblock();
        }
        if let Some(t) = self.thread.take() {
            // Returns at once when idle; at worst waits out one tool call.
            let _ = t.join();
        }
        // tiny_http wakes its accept thread by connecting to the listen
        // address, which fails for 0.0.0.0 on Windows: knock on loopback
        // too, so the port is released for a restart.
        let port = self.addr.port();
        drop(self.server.take());
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(200),
        );
        tracing::info!("MCP server on port {port} stopped");
    }
}

/// Bind, retrying briefly while a just-stopped server releases the port.
fn bind_with_retry(addr: &str) -> io::Result<Server> {
    let mut last = String::new();
    for _ in 0..10 {
        match Server::http(addr) {
            Ok(s) => return Ok(s),
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(Duration::from_millis(60));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        format!("couldn't listen on {addr}: {last}"),
    ))
}

fn serve(server: &Server, stop: &AtomicBool, token: &str, backend: &dyn Backend) {
    loop {
        match server.recv() {
            Ok(request) => {
                if stop.load(Ordering::SeqCst) {
                    let _ = request.respond(Response::empty(503));
                    return;
                }
                handle_request(request, token, backend);
            }
            Err(_) if stop.load(Ordering::SeqCst) => return,
            // A broken client connection; keep serving the others.
            Err(_) => {}
        }
    }
}

fn header<'a>(request: &'a tiny_http::Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("static header")
}

fn plain(request: tiny_http::Request, code: u16, text: &str) {
    let _ = request.respond(Response::from_string(text).with_status_code(code));
}

fn handle_request(mut request: tiny_http::Request, token: &str, backend: &dyn Backend) {
    let path = request.url().split('?').next().unwrap_or("");
    if path != "/mcp" {
        return plain(request, 404, "Not found. The MCP endpoint is /mcp.");
    }
    if let Some(origin) = header(&request, "Origin") {
        if !is_local_origin(origin) {
            return plain(request, 403, "Cross-origin requests are not allowed.");
        }
    }
    let authorized = header(&request, "Authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|given| constant_time_eq(given.trim().as_bytes(), token.as_bytes()));
    if !authorized {
        let challenge =
            Header::from_bytes(&b"WWW-Authenticate"[..], &b"Bearer"[..]).expect("static header");
        let _ = request.respond(
            Response::from_string("Missing or wrong bearer token.")
                .with_status_code(401)
                .with_header(challenge),
        );
        return;
    }
    if *request.method() != Method::Post {
        let allow = Header::from_bytes(&b"Allow"[..], &b"POST"[..]).expect("static header");
        let _ = request.respond(
            Response::from_string("Use POST.")
                .with_status_code(405)
                .with_header(allow),
        );
        return;
    }
    if request.body_length().is_some_and(|n| n > MAX_BODY) {
        return plain(request, 413, "Request body too large.");
    }
    let mut body = Vec::new();
    let read = request
        .as_reader()
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut body);
    if read.is_err() || body.len() > MAX_BODY {
        return plain(request, 413, "Request body too large.");
    }
    let reply = match serde_json::from_slice::<Value>(&body) {
        Ok(msg) => protocol::handle(&msg, backend),
        Err(e) => Some(serde_json::json!({
            "jsonrpc": "2.0", "id": null,
            "error": { "code": -32700, "message": format!("Parse error: {e}") }
        })),
    };
    let _ = match reply {
        Some(v) => request.respond(Response::from_string(v.to_string()).with_header(json_header())),
        None => request.respond(Response::empty(202)),
    };
}

/// `http(s)://localhost`, `127.0.0.1`, or `[::1]`, any port.
fn is_local_origin(origin: &str) -> bool {
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .unwrap_or("");
    let host = if let Some(v6) = rest.strip_prefix('[') {
        v6.split(']').next().map(|h| format!("[{h}]"))
    } else {
        rest.split([':', '/']).next().map(str::to_owned)
    };
    matches!(host.as_deref(), Some("localhost" | "127.0.0.1" | "[::1]"))
}

/// Equal-length inputs are compared without an early exit.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::tests::FakeBackend;
    use std::io::Write;

    fn start() -> (McpServer, String) {
        let cfg = McpConfig {
            enabled: true,
            lan: false,
            port: 0,
            token: "t".repeat(64),
        };
        let server = McpServer::start(&cfg, Arc::new(FakeBackend::default())).unwrap();
        (server, cfg.token)
    }

    /// Raw HTTP/1.1 exchange; returns (status code, body).
    fn send(addr: SocketAddr, method: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut req = format!("{method} /mcp HTTP/1.1\r\nHost: x\r\nConnection: close\r\n");
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        let code = out[9..12].parse().unwrap();
        let body = out.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
        (code, body)
    }

    const LIST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

    #[test]
    fn requests_without_the_token_are_refused() {
        let (server, _) = start();
        let (code, _) = send(server.addr(), "POST", &[], LIST);
        assert_eq!(code, 401);
        let (code, _) = send(
            server.addr(),
            "POST",
            &[("Authorization", "Bearer nope")],
            LIST,
        );
        assert_eq!(code, 401);
    }

    #[test]
    fn an_authorized_post_gets_the_json_rpc_answer() {
        let (server, token) = start();
        let auth = format!("Bearer {token}");
        let (code, body) = send(server.addr(), "POST", &[("Authorization", &auth)], LIST);
        assert_eq!(code, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["result"]["tools"][0]["name"], "search_files");
        let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let (code, _) = send(server.addr(), "POST", &[("Authorization", &auth)], note);
        assert_eq!(code, 202);
        let (code, _) = send(server.addr(), "GET", &[("Authorization", &auth)], "");
        assert_eq!(code, 405);
    }

    #[test]
    fn foreign_browser_origins_are_refused() {
        let (server, token) = start();
        let auth = format!("Bearer {token}");
        let evil = [
            ("Authorization", auth.as_str()),
            ("Origin", "https://evil.example"),
        ];
        assert_eq!(send(server.addr(), "POST", &evil, LIST).0, 403);
        let local = [
            ("Authorization", auth.as_str()),
            ("Origin", "http://localhost:3000"),
        ];
        assert_eq!(send(server.addr(), "POST", &local, LIST).0, 200);
    }

    #[test]
    fn a_stopped_server_releases_its_port() {
        let (server, token) = start();
        let port = server.addr().port();
        drop(server);
        let cfg = McpConfig {
            enabled: true,
            lan: false,
            port,
            token,
        };
        let again = McpServer::start(&cfg, Arc::new(FakeBackend::default()));
        assert!(again.is_ok(), "{:?}", again.err());
    }

    #[test]
    fn local_origins_are_recognised() {
        assert!(is_local_origin("http://localhost"));
        assert!(is_local_origin("http://127.0.0.1:8080"));
        assert!(is_local_origin("http://[::1]:1"));
        assert!(!is_local_origin("http://localhost.evil.com"));
        assert!(!is_local_origin("null"));
    }
}
