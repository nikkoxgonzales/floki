//! Run-on-startup via `HKCU\...\Run\Floki` (`"<exe>" --minimized`, or
//! `"<exe>"` alone when the window opens at sign-in instead of the tray).
//!
//! Windows-only; other platforms get stubs so the crate type-checks.

use std::io;

#[cfg(windows)]
const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const VALUE_NAME: &str = "Floki";

#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `"<exe>" --minimized` in the tray, else `"<exe>"` (quoted; no shell
/// involved on write).
fn command_for(exe: &str, in_tray: bool) -> String {
    if in_tray {
        format!("\"{exe}\" --minimized")
    } else {
        format!("\"{exe}\"")
    }
}

/// Whether a Run command starts Floki in the tray.
fn is_tray_command(cmd: &str) -> bool {
    cmd.split_whitespace().any(|a| a == "--minimized")
}

/// The Run value: `None` when Floki does not open at sign-in, else
/// whether it starts in the tray.
#[cfg(windows)]
pub fn state() -> Option<bool> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
    };

    let subkey = to_wide(RUN_SUBKEY);
    let name = to_wide(VALUE_NAME);
    let mut key = std::ptr::null_mut();
    // SAFETY: NUL-terminated inputs; handle closed below on success.
    let open = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        )
    };
    if open != ERROR_SUCCESS || key.is_null() {
        return None;
    }
    let value = query_value(key, &name);
    // SAFETY: key came from a successful open.
    unsafe {
        RegCloseKey(key);
    }
    let wide: Vec<u16> = value?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes(*b))
        .collect();
    // REG_SZ data keeps its NUL terminator.
    let cmd = String::from_utf16_lossy(&wide);
    Some(is_tray_command(cmd.trim_end_matches('\0')))
}

#[cfg(windows)]
fn query_value(key: windows_sys::Win32::System::Registry::HKEY, name: &[u16]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::RegQueryValueExW;

    let mut kind = 0u32;
    let mut bytes = 0u32;
    // SAFETY: querying size first; buffers sized from the returned length.
    let rc = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if rc != ERROR_SUCCESS || bytes == 0 || bytes > 32 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; bytes as usize];
    let mut len = bytes;
    let rc = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            buf.as_mut_ptr(),
            &mut len,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    buf.truncate(len as usize);
    Some(buf)
}

/// Create (`Some(in_tray)`) or delete (`None`) the Run value.
#[cfg(windows)]
pub fn set_state(state: Option<bool>) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    let subkey = to_wide(RUN_SUBKEY);
    let name = to_wide(VALUE_NAME);
    let mut key = std::ptr::null_mut();
    // SAFETY: NUL-terminated subkey; handle closed below on success.
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS || key.is_null() {
        return Err(io::Error::other(format!(
            "cannot open HKCU\\{RUN_SUBKEY} (code {rc})"
        )));
    }
    // SAFETY: key is open for the whole scope; closed exactly once below.
    let result = unsafe {
        if let Some(in_tray) = state {
            let exe = std::env::current_exe()?;
            let cmd = to_wide(&command_for(&exe.to_string_lossy(), in_tray));
            let bytes = std::slice::from_raw_parts(
                cmd.as_ptr().cast::<u8>(),
                cmd.len() * std::mem::size_of::<u16>(),
            );
            let rc = RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                bytes.as_ptr(),
                bytes.len() as u32,
            );
            if rc != ERROR_SUCCESS {
                Err(io::Error::other(format!(
                    "cannot write Run\\{VALUE_NAME} (code {rc})"
                )))
            } else {
                Ok(())
            }
        } else {
            let rc = RegDeleteValueW(key, name.as_ptr());
            // Deleting a missing value is already-disabled, not an error.
            if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
                Err(io::Error::other(format!(
                    "cannot delete Run\\{VALUE_NAME} (code {rc})"
                )))
            } else {
                Ok(())
            }
        }
    };
    // SAFETY: key came from a successful open above.
    unsafe {
        RegCloseKey(key);
    }
    result
}

#[cfg(not(windows))]
pub fn state() -> Option<bool> {
    None
}

#[cfg(not(windows))]
pub fn set_state(_state: Option<bool>) -> io::Result<()> {
    Err(io::Error::other("floki is Windows-only"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_flag_round_trips_through_the_command() {
        let exe = r"C:\Program Files\Floki\floki.exe";
        assert!(is_tray_command(&command_for(exe, true)));
        assert!(!is_tray_command(&command_for(exe, false)));
    }

    #[test]
    fn a_folder_named_like_the_flag_is_not_the_flag() {
        assert!(!is_tray_command(r#""C:\--minimized\floki.exe""#));
    }

    /// Reads the real HKCU Run value; run with `--ignored` on a machine
    /// where Floki opens at sign-in.
    #[test]
    #[cfg(windows)]
    #[ignore = "reads the user's registry"]
    fn reads_the_installed_run_value() {
        eprintln!("startup state: {:?}", state());
        assert!(state().is_some());
    }

    #[test]
    #[cfg(not(windows))]
    fn stubs_report_disabled_off_windows() {
        assert!(state().is_none());
        assert!(set_state(Some(true)).is_err());
    }
}
