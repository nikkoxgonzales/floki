//! On-disk layout: everything lives under `%LOCALAPPDATA%\Floki`.

use std::path::PathBuf;

/// `%LOCALAPPDATA%\Floki` (created on demand by [`ensure_data_dir`]).
pub fn data_dir() -> anyhow::Result<PathBuf> {
    let base =
        dirs::data_local_dir().ok_or_else(|| anyhow::anyhow!("%LOCALAPPDATA% is not available"))?;
    Ok(base.join("Floki"))
}

/// `%LOCALAPPDATA%\Floki\index.bin`.
pub fn index_path() -> anyhow::Result<PathBuf> {
    Ok(data_dir()?.join("index.bin"))
}

/// `%LOCALAPPDATA%\Floki\flokid.log`.
pub fn log_path() -> anyhow::Result<PathBuf> {
    Ok(data_dir()?.join("flokid.log"))
}

/// Create the data directory (and parents) if missing.
pub fn ensure_data_dir() -> anyhow::Result<PathBuf> {
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
