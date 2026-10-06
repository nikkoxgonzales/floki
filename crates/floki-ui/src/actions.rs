//! Windows shell actions: open file, reveal in folder, elevate the indexer.
//!
//! `#[cfg(windows)]` uses `ShellExecuteW` / `ShellExecuteExW` /
//! `SHFileOperationW` from `windows-sys`; other platforms get stubs so the
//! crate still type-checks. Every function takes a plain path string so the
//! egui layer stays thin. Deletion is always to the Recycle Bin
//! (`FOF_ALLOWUNDO`), never permanent.

use std::io;

#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Double-NUL-terminated wide path for `SHFILEOPSTRUCTW::pFrom`.
#[cfg(windows)]
fn to_wide_double_nul(s: &str) -> Vec<u16> {
    s.encode_utf16()
        .chain(std::iter::once(0))
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn shell_execute(verb: &str, file: &str, params: Option<&str>) -> io::Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let op = to_wide(verb);
    let file = to_wide(file);
    let params_wide;
    let params_ptr = match params {
        Some(p) => {
            params_wide = to_wide(p);
            params_wide.as_ptr()
        }
        None => std::ptr::null(),
    };
    // SAFETY: ShellExecuteW only reads the NUL-terminated inputs for the call.
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            params_ptr,
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if rc as usize > 32 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "ShellExecuteW {verb} failed with code {}",
            rc as usize
        )))
    }
}

/// `ShellExecuteExW` with an explicit show command and mask: the hidden
/// elevated launch (`runas` + `SW_HIDE` + no-console/no-UI flags) goes
/// through here so the indexer never flashes a console window.
#[cfg(windows)]
fn shell_execute_ex(
    verb: &str,
    file: &str,
    params: Option<&str>,
    n_show: i32,
    mask: u32,
) -> io::Result<()> {
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SHELLEXECUTEINFOW};

    let verb_w = to_wide(verb);
    let file_w = to_wide(file);
    let params_w;
    let params_ptr = match params {
        Some(p) => {
            params_w = to_wide(p);
            params_w.as_ptr()
        }
        None => std::ptr::null(),
    };
    // SAFETY: zeroed struct with NUL-terminated inputs that outlive the
    // synchronous call; only `ShellExecuteExW` reads them.
    let ok = unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of_val(&info) as u32;
        info.fMask = mask;
        info.lpVerb = verb_w.as_ptr();
        info.lpFile = file_w.as_ptr();
        info.lpParameters = params_ptr;
        info.nShow = n_show;
        ShellExecuteExW(&mut info)
    };
    if ok == 0 {
        // Raw OS error, so callers can tell a declined UAC prompt (1223)
        // from a real failure.
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Open a file with its default handler (`ShellExecuteW "open"`).
#[cfg(windows)]
pub fn open_file(path: &str) -> io::Result<()> {
    shell_execute("open", path, None)
}

/// Show the "Open with" dialog: `ShellExecuteW` verb `openas`, falling back
/// to `rundll32.exe shell32.dll,OpenAs_RunDLL <path>`.
#[cfg(windows)]
pub fn open_with(path: &str) -> io::Result<()> {
    if shell_execute("openas", path, None).is_ok() {
        return Ok(());
    }
    let status = std::process::Command::new("rundll32.exe")
        .arg("shell32.dll,OpenAs_RunDLL")
        .arg(path)
        .status()
        .map_err(|e| io::Error::other(format!("failed to launch OpenAs dialog: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "OpenAs dialog exited with {status}"
        )))
    }
}

/// Reveal a path in Explorer (`explorer.exe /select,<path>`).
#[cfg(windows)]
pub fn open_containing_folder(path: &str) -> io::Result<()> {
    use std::process::Command;

    let status = Command::new("explorer.exe")
        .arg(format!("/select,{path}"))
        .status()
        .map_err(|e| io::Error::other(format!("failed to launch explorer: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("explorer exited with {status}")))
    }
}

/// Relaunch a program elevated (`ShellExecuteW` verb `runas` triggers UAC).
#[cfg(windows)]
pub fn run_as_admin(path: &str) -> io::Result<()> {
    shell_execute("runas", path, None)
}

/// Show the Explorer Properties dialog (`ShellExecuteExW` verb
/// `properties` with `SEE_MASK_INVOKEIDLIST`). Requires COM on the calling
/// thread; call [`ensure_com_initialized`] once at startup.
#[cfg(windows)]
pub fn show_properties(path: &str) -> io::Result<()> {
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_INVOKEIDLIST};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let verb = to_wide("properties");
    let file = to_wide(path);
    // SAFETY: zeroed struct with valid NUL-terminated inputs; the call only
    // reads them. `SHELLEXECUTEINFOW` needs the `Win32_System_Registry`
    // windows-sys feature (the `hkeyClass` field).
    unsafe {
        let mut info: windows_sys::Win32::UI::Shell::SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of_val(&info) as u32;
        info.fMask = SEE_MASK_INVOKEIDLIST;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut info) == 0 {
            return Err(io::Error::other("ShellExecuteExW properties failed"));
        }
    }
    Ok(())
}

/// Move a file or directory to the Recycle Bin (`SHFileOperationW`
/// `FO_DELETE` with `FOF_ALLOWUNDO | FOF_NOCONFIRMATION`). Never deletes
/// permanently; directories are handled by the shell.
#[cfg(windows)]
pub fn recycle_to_bin(path: &str) -> io::Result<()> {
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FO_DELETE,
    };

    let from = to_wide_double_nul(path);
    // SAFETY: zeroed struct; `pFrom` points at a valid double-NUL buffer
    // that outlives the synchronous call.
    let rc = unsafe {
        let mut op: windows_sys::Win32::UI::Shell::SHFILEOPSTRUCTW = std::mem::zeroed();
        op.wFunc = FO_DELETE;
        op.pFrom = from.as_ptr();
        op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION) as u16;
        SHFileOperationW(&mut op)
    };
    if rc != 0 {
        return Err(io::Error::other(format!(
            "move to Recycle Bin failed with code {rc}"
        )));
    }
    Ok(())
}

/// Initialize COM once on the calling (UI) thread for `show_properties`.
/// Extra calls are no-ops; safe to call more than once.
#[cfg(windows)]
pub fn ensure_com_initialized() {
    use std::sync::Once;
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };

    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: reserved param is NULL; apartment-threaded + no OLE1 DDE.
        let _ = unsafe {
            CoInitializeEx(
                std::ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
    });
}

/// A local fixed/removable drive the indexer can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalDrive {
    pub letter: char,
    /// ReFS (e.g. a Dev Drive) rather than NTFS.
    pub refs: bool,
}

/// Scheduled Task `flokid install` registers; must match
/// `floki_service::install::TASK_NAME`.
pub const INDEXER_TASK: &str = "Floki";

/// How [`start_indexer_hidden`] started the indexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexerLaunch {
    /// Through the sign-in task (`schtasks /Run`): no prompt; the indexer
    /// belongs to the system, not to this window.
    Task,
    /// One-off elevated launch: the user approved a UAC prompt.
    Prompted,
}

/// True when the user declined the UAC prompt (`ERROR_CANCELLED`).
#[must_use]
pub fn uac_declined(e: &io::Error) -> bool {
    e.raw_os_error() == Some(1223)
}

/// `schtasks` with a hidden console; true on exit status 0.
#[cfg(windows)]
fn schtasks(args: &[&str]) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("schtasks")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// True when the sign-in task exists (`flokid install` ran). Spawns
/// `schtasks` (~50 ms): call off the UI thread.
#[cfg(windows)]
#[must_use]
pub fn indexer_autostart_installed() -> bool {
    schtasks(&["/Query", "/TN", INDEXER_TASK])
}

/// Start the indexer with no console window. Through the sign-in task when
/// it exists (no UAC prompt), otherwise an elevated `flokid.exe run
/// --hidden` (one UAC prompt). Check [`uac_declined`] on the error.
#[cfg(windows)]
pub fn start_indexer_hidden() -> io::Result<IndexerLaunch> {
    use windows_sys::Win32::UI::Shell::{SEE_MASK_FLAG_NO_UI, SEE_MASK_NO_CONSOLE};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    if indexer_autostart_installed() && schtasks(&["/Run", "/TN", INDEXER_TASK]) {
        return Ok(IndexerLaunch::Task);
    }
    let flokid = indexer_exe()?;
    shell_execute_ex(
        "runas",
        &flokid.to_string_lossy(),
        Some("run --hidden"),
        SW_HIDE,
        SEE_MASK_NO_CONSOLE | SEE_MASK_FLAG_NO_UI,
    )?;
    Ok(IndexerLaunch::Prompted)
}

/// Register (`true`) or remove (`false`) the sign-in task: an elevated,
/// hidden `flokid.exe install` / `uninstall` (one UAC prompt). Install also
/// starts the indexer. Returns once the elevated process is launched; check
/// [`uac_declined`] on the error.
#[cfg(windows)]
pub fn set_indexer_autostart(on: bool) -> io::Result<()> {
    use windows_sys::Win32::UI::Shell::{SEE_MASK_FLAG_NO_UI, SEE_MASK_NO_CONSOLE};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let flokid = indexer_exe()?;
    shell_execute_ex(
        "runas",
        &flokid.to_string_lossy(),
        Some(if on { "install" } else { "uninstall" }),
        SW_HIDE,
        SEE_MASK_NO_CONSOLE | SEE_MASK_FLAG_NO_UI,
    )
}

/// Local (fixed or removable) NTFS and ReFS drives, A-Z order. Unelevated
/// and cheap (`GetLogicalDrives` + `GetVolumeInformationW` per letter);
/// the Settings Drives panel uses it to offer volumes that are not indexed
/// (never scanned, or removed by the user) for adding back.
#[cfg(windows)]
pub fn local_indexable_drives() -> Vec<LocalDrive> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;

    // SAFETY: no arguments; returns a bitmask of present drive letters.
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for bit in 0..26u32 {
        if mask & (1 << bit) == 0 {
            continue;
        }
        let letter = char::from(b'A' + bit as u8);
        let root = to_wide(&format!("{letter}:\\"));
        // SAFETY: NUL-terminated root path; synchronous query.
        let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
        if kind != DRIVE_FIXED && kind != DRIVE_REMOVABLE {
            continue;
        }
        let mut fs_name = [0u16; 32];
        // SAFETY: valid root path; only the file-system name buffer is
        // requested (its length is passed); the rest are null/zero.
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
            )
        };
        let len = fs_name
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(fs_name.len());
        if ok == 0 {
            continue;
        }
        match String::from_utf16_lossy(&fs_name[..len]).as_str() {
            "NTFS" => out.push(LocalDrive {
                letter,
                refs: false,
            }),
            "ReFS" => out.push(LocalDrive { letter, refs: true }),
            _ => {}
        }
    }
    out
}

/// `flokid.exe` next to the running `floki.exe`.
#[cfg(windows)]
fn indexer_exe() -> io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| io::Error::other("current exe has no parent directory"))?;
    let flokid = dir.join("flokid.exe");
    if !flokid.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} not found next to floki.exe", flokid.display()),
        ));
    }
    Ok(flokid)
}

#[cfg(not(windows))]
pub fn open_file(_path: &str) -> io::Result<()> {
    Err(io::Error::other("floki is Windows-only"))
}

#[cfg(not(windows))]
pub fn open_with(_path: &str) -> io::Result<()> {
    Err(io::Error::other("floki is Windows-only"))
}

#[cfg(not(windows))]
pub fn open_containing_folder(_path: &str) -> io::Result<()> {
    Err(io::Error::other("floki is Windows-only"))
}

#[cfg(not(windows))]
pub fn run_as_admin(_path: &str) -> io::Result<()> {
    Err(io::Error::other("floki is Windows-only"))
}

#[cfg(not(windows))]
pub fn show_properties(_path: &str) -> io::Result<()> {
    Err(io::Error::other("floki is Windows-only"))
}

#[cfg(not(windows))]
pub fn recycle_to_bin(_path: &str) -> io::Result<()> {
    Err(io::Error::other("floki is Windows-only"))
}

#[cfg(not(windows))]
pub fn ensure_com_initialized() {}

#[cfg(not(windows))]
pub fn local_indexable_drives() -> Vec<LocalDrive> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn start_indexer_hidden() -> io::Result<IndexerLaunch> {
    Err(io::Error::other("floki is Windows-only"))
}

#[cfg(not(windows))]
#[must_use]
pub fn indexer_autostart_installed() -> bool {
    false
}

#[cfg(not(windows))]
pub fn set_indexer_autostart(_on: bool) -> io::Result<()> {
    Err(io::Error::other("floki is Windows-only"))
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::{to_wide, to_wide_double_nul};

    #[test]
    fn declined_uac_is_recognized() {
        assert!(super::uac_declined(&std::io::Error::from_raw_os_error(
            1223
        )));
        assert!(!super::uac_declined(&std::io::Error::from_raw_os_error(5)));
        assert!(!super::uac_declined(&std::io::Error::other("x")));
    }

    #[test]
    #[cfg(windows)]
    fn wide_strings_are_nul_terminated() {
        assert_eq!(to_wide("open"), vec![0x6F, 0x70, 0x65, 0x6E, 0]);
        let p = to_wide_double_nul(r"C:\a.txt");
        assert!(p.ends_with(&[0, 0]));
        // No interior NULs before the terminator pair.
        assert!(!p[..p.len() - 2].contains(&0));
    }

    #[test]
    #[cfg(not(windows))]
    fn stubs_error_off_windows() {
        assert!(super::open_file(r"C:\a").is_err());
        assert!(super::recycle_to_bin(r"C:\a").is_err());
    }
}
