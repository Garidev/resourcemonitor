//! Start-with-Windows via an elevated Scheduled Task (no UAC prompt at logon).

use std::os::windows::process::CommandExt;
use std::process::Command;

const TASK_NAME: &str = "ResourceMonitor";
/// Pre-branding task name; cleaned up on install/uninstall.
const LEGACY_TASK_NAME: &str = "ResMon";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn schtasks(args: &[&str]) -> bool {
    Command::new("schtasks")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn is_installed() -> bool {
    schtasks(&["/Query", "/TN", TASK_NAME])
}

pub fn install() -> bool {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return false,
    };
    schtasks(&["/Delete", "/F", "/TN", LEGACY_TASK_NAME]);
    let tr = format!("\"{}\"", exe.display());
    schtasks(&[
        "/Create", "/F", "/TN", TASK_NAME, "/SC", "ONLOGON", "/RL", "HIGHEST", "/TR", &tr,
    ])
}

pub fn uninstall() -> bool {
    schtasks(&["/Delete", "/F", "/TN", LEGACY_TASK_NAME]);
    schtasks(&["/Delete", "/F", "/TN", TASK_NAME])
}
