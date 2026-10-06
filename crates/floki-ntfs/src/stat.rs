//! Per-file metadata without opening the file.
//!
//! `std::fs::metadata` opens a handle per file (`CreateFileW` +
//! `GetFileInformationByHandle`); `GetFileAttributesExW` answers from the
//! directory entry, several times cheaper. Used for result rows and time
//! sorts, always outside the index lock.

/// Size and times of one file or folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    pub is_dir: bool,
    /// Bytes; `None` for folders (a folder length is meaningless).
    pub size: Option<u64>,
    /// Last write, Unix epoch milliseconds.
    pub modified_ms: Option<i64>,
    /// Creation, Unix epoch milliseconds.
    pub created_ms: Option<i64>,
}

/// 100 ns ticks between 1601-01-01 and 1970-01-01.
const EPOCH_DIFF_TICKS: u64 = 116_444_736_000_000_000;

/// FILETIME halves as Unix epoch milliseconds; `None` for zero/pre-1970.
#[must_use]
pub fn filetime_ms(low: u32, high: u32) -> Option<i64> {
    let ticks = (u64::from(high) << 32) | u64::from(low);
    let since_unix = ticks.checked_sub(EPOCH_DIFF_TICKS)?;
    i64::try_from(since_unix / 10_000).ok()
}

/// Metadata for `path`, or `None` when it cannot be read (gone, no access).
/// Long paths get the `\\?\` prefix.
#[cfg(windows)]
#[must_use]
pub fn file_meta(path: &str) -> Option<FileMeta> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesExW, GetFileExInfoStandard, FILE_ATTRIBUTE_DIRECTORY,
        WIN32_FILE_ATTRIBUTE_DATA,
    };
    let long = path.len() >= 248 && !path.starts_with(r"\\");
    let mut wide: Vec<u16> = Vec::with_capacity(path.len() + 5);
    if long {
        wide.extend(r"\\?\".encode_utf16());
    }
    wide.extend(path.encode_utf16());
    wide.push(0);
    // SAFETY: all-zero is a valid WIN32_FILE_ATTRIBUTE_DATA (plain integers).
    let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
    // SAFETY: `wide` is NUL-terminated and outlives the call; `data` is the
    // struct `GetFileExInfoStandard` writes.
    let ok = unsafe {
        GetFileAttributesExW(
            wide.as_ptr(),
            GetFileExInfoStandard,
            std::ptr::from_mut(&mut data).cast(),
        )
    };
    if ok == 0 {
        return None;
    }
    let is_dir = data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let size = (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow);
    Some(FileMeta {
        is_dir,
        size: (!is_dir).then_some(size),
        modified_ms: filetime_ms(
            data.ftLastWriteTime.dwLowDateTime,
            data.ftLastWriteTime.dwHighDateTime,
        ),
        created_ms: filetime_ms(
            data.ftCreationTime.dwLowDateTime,
            data.ftCreationTime.dwHighDateTime,
        ),
    })
}

/// Non-Windows stub so dependents type-check anywhere.
#[cfg(not(windows))]
#[must_use]
pub fn file_meta(_path: &str) -> Option<FileMeta> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_converts_to_unix_ms() {
        assert_eq!(filetime_ms(0, 0), None);
        // 2023-11-14T22:13:20Z = 1_700_000_000_000 ms.
        let ticks = EPOCH_DIFF_TICKS + 1_700_000_000_000 * 10_000;
        assert_eq!(
            filetime_ms(ticks as u32, (ticks >> 32) as u32),
            Some(1_700_000_000_000)
        );
    }

    #[cfg(windows)]
    #[test]
    fn file_meta_reads_this_source_file_and_its_folder() {
        let file = concat!(env!("CARGO_MANIFEST_DIR"), r"\src\stat.rs");
        let meta = file_meta(file).expect("stat.rs exists");
        assert!(!meta.is_dir);
        assert!(meta.size.is_some_and(|s| s > 0));
        assert!(meta.modified_ms.is_some());
        let dir = file_meta(env!("CARGO_MANIFEST_DIR")).expect("crate dir exists");
        assert!(dir.is_dir);
        assert_eq!(dir.size, None);
        assert_eq!(file_meta(r"C:\definitely\not\here.floki"), None);
    }
}
