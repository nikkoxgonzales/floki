//! Volume discovery (`list_indexable_volumes`, `volume_fs`) and elevation
//! check (`is_elevated`).

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::{GetLogicalDrives, GetVolumeInformationW};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use std::ptr::null_mut;

use crate::FsKind;

/// All present drive letters on NTFS or ReFS, in `A..=Z` order.
///
/// Uses `GetLogicalDrives` for presence and [`volume_fs`] for the file
/// system; drives that cannot be queried (empty card readers, …) are
/// silently skipped.
pub fn list_indexable_volumes() -> Vec<char> {
    // SAFETY: no arguments, no buffers; always safe to call.
    let mask = unsafe { GetLogicalDrives() };
    ('A'..='Z')
        .enumerate()
        .filter(|&(i, letter)| mask & (1 << i) != 0 && volume_fs(letter).is_some())
        .map(|(_, letter)| letter)
        .collect()
}

/// File system of `letter`'s volume when Floki can index it (needs no
/// elevation).
#[must_use]
pub fn volume_fs(letter: char) -> Option<FsKind> {
    let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
    let mut fs_name = [0u16; 32];
    // SAFETY: `root` is NUL-terminated; `fs_name` is a live 32-wide buffer
    // with its length passed correctly; unused outputs are null.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            null_mut(),
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let end = fs_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(fs_name.len());
    FsKind::from_name(&String::from_utf16_lossy(&fs_name[..end]))
}

/// `true` when the current process token has elevation (Administrator).
///
/// `OpenProcessToken` + `GetTokenInformation(TokenElevation)`; any failure
/// (e.g. restricted token) conservatively yields `false` rather than an error.
#[must_use]
pub fn is_elevated() -> bool {
    // SAFETY: `GetCurrentProcess` needs no cleanup; `token` is checked for
    // null before use and closed exactly once on every success path.
    unsafe {
        let process = GetCurrentProcess();
        let mut token = null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 || token.is_null() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned = 0u32;
        // SAFETY: `elevation` is a live `TOKEN_ELEVATION` with its size passed
        // correctly; `token` is a valid open token handle here.
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut std::ffi::c_void,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        // SAFETY: `token` is still the open handle from above; closed once.
        CloseHandle(token);
        // Any failure simply means "not elevated"; no GetLastError read needed.
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevation_check_does_not_panic() {
        // Result depends on the environment; both outcomes are valid here.
        let _ = is_elevated();
    }

    #[test]
    #[ignore]
    fn admin_list_indexable_volumes_contains_c() {
        let vols = list_indexable_volumes();
        println!("indexable volumes: {vols:?}");
        assert!(vols.contains(&'C'), "expected C: in {vols:?}");
    }
}
