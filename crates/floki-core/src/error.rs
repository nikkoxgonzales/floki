//! `CoreError`: floki-core failure modes.

/// Errors from [`save`](crate::Index::save) / [`load`](crate::Index::load).
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// File does not start with `b"FLOKIDX1"` or `b"FLOKIDX2"`.
    #[error("not a Floki index (bad magic)")]
    BadMagic,
    /// Structurally invalid file (truncated, bad counts, bad UTF-8, ...).
    #[error("corrupt index: {0}")]
    Corrupt(String),
    /// Underlying I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Header JSON failure.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
