//! Autostart registration via a per-user elevated Scheduled Task.
//!
//! `flokid install` creates a task named [`TASK_NAME`] that runs
//! `<exe> run --hidden` at logon with highest privileges, so the indexer starts
//! elevated without a UAC prompt, and starts it right away. Anyone in the
//! user's session can then start it again with `schtasks /Run /TN Floki`
//! (still no prompt) — the UI's Start button does exactly that.
//! `flokid uninstall` deletes the task.
//!
//! The task is registered from XML, not `schtasks /SC ONLOGON`: tasks made
//! that way get a 72-hour execution limit (Windows would kill a long-lived
//! indexer after three days) and refuse to start or keep running on battery.
//! Commands are executed with an argument array (never a shell string).

use std::process::Command;

use floki_ntfs::is_elevated;

use crate::daemon::NOT_ELEVATED_MSG;

/// Name of the Scheduled Task (also the `/TN` value).
pub const TASK_NAME: &str = "Floki";

/// Task Scheduler definition for `<exe> run --hidden` at `user`'s logon
/// (any user's logon when `None`), elevated, with no execution time limit,
/// no battery conditions and one instance at a time.
#[must_use]
pub fn task_xml(exe: &str, user: Option<&str>) -> String {
    let user_id = user
        .map(|u| format!("<UserId>{}</UserId>", xml_escape(u)))
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Floki indexer: keeps the file-name index live.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      {user_id}
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      {user_id}
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>run --hidden</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        exe = xml_escape(exe),
    )
}

/// Escape the five XML special characters (`&` is legal in Windows paths).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Exact `schtasks` argument vector for `install`: create from the XML at
/// `xml_path`, overwriting an existing task (`/F`; without it a re-install
/// stops at an interactive Y/N prompt, which hangs a hidden launch).
#[must_use]
pub fn install_argv(xml_path: &str) -> Vec<String> {
    vec![
        "/Create".to_owned(),
        "/TN".to_owned(),
        TASK_NAME.to_owned(),
        "/XML".to_owned(),
        xml_path.to_owned(),
        "/F".to_owned(),
    ]
}

/// Exact `schtasks` argument vector that starts the installed task now.
#[must_use]
pub fn run_argv() -> Vec<String> {
    vec!["/Run".to_owned(), "/TN".to_owned(), TASK_NAME.to_owned()]
}

/// Exact `schtasks` argument vector for `uninstall`.
#[must_use]
pub fn uninstall_argv() -> Vec<String> {
    vec![
        "/Delete".to_owned(),
        "/TN".to_owned(),
        TASK_NAME.to_owned(),
        "/F".to_owned(),
    ]
}

/// `DOMAIN\user` of the current session, when the environment names it.
fn current_user() -> Option<String> {
    let user = std::env::var("USERNAME").ok().filter(|u| !u.is_empty())?;
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => Some(format!(r"{domain}\{user}")),
        _ => Some(user),
    }
}

/// Register the logon task for the current executable, then start it.
pub fn install() -> anyhow::Result<()> {
    if !is_elevated() {
        eprintln!("{NOT_ELEVATED_MSG}");
        std::process::exit(1);
    }
    let exe = std::env::current_exe()?;
    let xml = task_xml(&exe.to_string_lossy(), current_user().as_deref());
    // schtasks reads the file as UTF-16 (BOM + LE units), matching the
    // encoding the XML declares.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    let xml_path = crate::paths::ensure_data_dir()?.join("floki-task.xml");
    std::fs::write(&xml_path, bytes)?;
    tracing::info!(target: "flokid", task = TASK_NAME, "creating scheduled task");
    let status = Command::new("schtasks")
        .args(install_argv(&xml_path.to_string_lossy()))
        .status();
    let _ = std::fs::remove_file(&xml_path);
    let status = status?;
    if !status.success() {
        return Err(anyhow::anyhow!(
            "schtasks /Create failed with status {status}"
        ));
    }
    // Start it now too; a running indexer makes this exit 3 harmlessly.
    let started = Command::new("schtasks").args(run_argv()).status()?;
    if !started.success() {
        tracing::warn!(target: "flokid", %started, "task created but /Run failed");
    }
    Ok(())
}

/// Remove the logon task.
pub fn uninstall() -> anyhow::Result<()> {
    if !is_elevated() {
        eprintln!("{NOT_ELEVATED_MSG}");
        std::process::exit(1);
    }
    let args = uninstall_argv();
    tracing::info!(target: "flokid", task = TASK_NAME, "deleting scheduled task");
    let status = Command::new("schtasks").args(&args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "schtasks /Delete failed with status {status}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_argv_is_exact() {
        assert_eq!(
            install_argv(r"C:\Data\floki-task.xml"),
            vec![
                "/Create",
                "/TN",
                "Floki",
                "/XML",
                r"C:\Data\floki-task.xml",
                "/F"
            ]
        );
    }

    #[test]
    fn run_and_uninstall_argv_are_exact() {
        assert_eq!(run_argv(), vec!["/Run", "/TN", "Floki"]);
        assert_eq!(uninstall_argv(), vec!["/Delete", "/TN", "Floki", "/F"]);
    }

    /// The defaults `schtasks /SC ONLOGON` would apply (72 h kill, battery
    /// stop) must be overridden explicitly.
    #[test]
    fn task_xml_never_times_out_or_stops_on_battery() {
        let xml = task_xml(r"C:\Tools\flokid.exe", Some(r"PC\me"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains(r"<Command>C:\Tools\flokid.exe</Command>"));
        assert!(xml.contains("<Arguments>run --hidden</Arguments>"));
        assert_eq!(xml.matches(r"<UserId>PC\me</UserId>").count(), 2);
    }

    #[test]
    fn task_xml_escapes_paths_and_omits_unknown_user() {
        let xml = task_xml(r"C:\R&D <x>\flokid.exe", None);
        assert!(xml.contains(r"<Command>C:\R&amp;D &lt;x&gt;\flokid.exe</Command>"));
        assert!(!xml.contains("<UserId>"));
    }
}
