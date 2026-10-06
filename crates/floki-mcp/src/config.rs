//! MCP server settings, kept in `%LOCALAPPDATA%\Floki\mcp.json`.
//!
//! Local (not roaming) app data: the file holds the bearer token, which must
//! not sync to other machines. The folder is per-user, so other accounts on
//! the PC cannot read it.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Port used until the user picks another.
pub const DEFAULT_PORT: u16 = 7457;

/// Tokens shorter than this (hand-edited file) are replaced on load.
const MIN_TOKEN_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpConfig {
    /// Serve MCP while the Floki window (tray) runs.
    pub enabled: bool,
    /// Listen on every network (`0.0.0.0`) instead of this PC only
    /// (`127.0.0.1`).
    pub lan: bool,
    pub port: u16,
    /// Bearer token every request must carry.
    pub token: String,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            lan: false,
            port: DEFAULT_PORT,
            token: new_token(),
        }
    }
}

/// 256 random bits as 64 hex characters.
///
/// # Panics
/// When the OS random source fails (it does not on supported Windows).
#[must_use]
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random source");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl McpConfig {
    /// Socket address to bind.
    #[must_use]
    pub fn bind_addr(&self) -> String {
        let host = if self.lan { "0.0.0.0" } else { "127.0.0.1" };
        format!("{host}:{}", self.port)
    }

    /// Endpoint URL as seen from this PC.
    #[must_use]
    pub fn local_url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// `%LOCALAPPDATA%\Floki\mcp.json`.
    #[must_use]
    pub fn path() -> Option<PathBuf> {
        Some(dirs::data_local_dir()?.join("Floki").join("mcp.json"))
    }

    /// Saved settings, or defaults (with a fresh token) when the file is
    /// missing or unreadable. A repaired or first-time config is written
    /// back at once, so the token a user copies stays valid across launches.
    #[must_use]
    pub fn load() -> Self {
        let saved = Self::path().and_then(|p| std::fs::read(p).ok());
        let cfg = saved.as_deref().map_or_else(Self::default, Self::from_json);
        let stored = saved
            .as_deref()
            .and_then(|b| serde_json::from_slice::<Self>(b).ok());
        if stored.as_ref() != Some(&cfg) {
            if let Err(e) = cfg.save() {
                tracing::warn!("couldn't save MCP settings: {e}");
            }
        }
        cfg
    }

    /// Parse a saved file; a broken file or a too-short token falls back to
    /// defaults / a new token.
    #[must_use]
    pub fn from_json(bytes: &[u8]) -> Self {
        let mut cfg: Self = serde_json::from_slice(bytes).unwrap_or_default();
        if cfg.token.len() < MIN_TOKEN_LEN {
            cfg.token = new_token();
        }
        if cfg.port == 0 {
            cfg.port = DEFAULT_PORT;
        }
        cfg
    }

    /// Write the settings (temp file + rename, so a crash never leaves half
    /// a file).
    ///
    /// # Errors
    /// No `%LOCALAPPDATA%`, or the file cannot be written.
    pub fn save(&self) -> io::Result<()> {
        let path =
            Self::path().ok_or_else(|| io::Error::other("%LOCALAPPDATA% is not available"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_hex_and_unique() {
        let a = new_token();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, new_token());
    }

    #[test]
    fn broken_or_weak_files_get_safe_defaults() {
        let cfg = McpConfig::from_json(b"not json");
        assert!(!cfg.enabled);
        assert_eq!(cfg.port, DEFAULT_PORT);
        let weak =
            McpConfig::from_json(br#"{"enabled":true,"lan":true,"port":9000,"token":"abc"}"#);
        assert!(weak.enabled && weak.lan);
        assert_eq!(weak.port, 9000);
        assert_eq!(weak.token.len(), 64, "a short token is replaced");
    }

    #[test]
    fn bind_addr_follows_lan() {
        let mut cfg = McpConfig::default();
        assert_eq!(cfg.bind_addr(), format!("127.0.0.1:{DEFAULT_PORT}"));
        cfg.lan = true;
        assert_eq!(cfg.bind_addr(), format!("0.0.0.0:{DEFAULT_PORT}"));
        assert_eq!(
            cfg.local_url(),
            format!("http://127.0.0.1:{DEFAULT_PORT}/mcp")
        );
    }
}
