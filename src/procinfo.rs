//! Best-effort per-process role labels, read from each process's command line.
//!
//! Modern apps (Edge, Chrome, Electron apps like Discord, Slack, VS Code) run
//! as a swarm of identical `*.exe` processes. Task Manager groups them under
//! the app and labels each one: Browser, GPU process, Renderer, Network
//! Service, Crashpad handler, Extension. Those labels come from the Chromium
//! `--type=` / `--utility-sub-type=` command-line flags, which is exactly what
//! this reads. Non-Chromium apps have no such flags, so they get no label.
//!
//! We can only see the *kind* of each subprocess, not the specific tab title or
//! extension name — those live inside the browser and are not exposed to other
//! programs.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationProcess(
        handle: HANDLE,
        class: u32,
        info: *mut c_void,
        len: u32,
        ret_len: *mut u32,
    ) -> i32;
}

/// ProcessCommandLineInformation (Windows 8.1+).
const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;

fn read_command_line(pid: u32) -> String {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return String::new();
        }
        // First call discovers the buffer size the OS wants to hand back.
        let mut needed = 0u32;
        NtQueryInformationProcess(
            h,
            PROCESS_COMMAND_LINE_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
        if needed == 0 || needed > 1 << 20 {
            CloseHandle(h);
            return String::new();
        }
        let mut buf = vec![0u8; needed as usize];
        let status = NtQueryInformationProcess(
            h,
            PROCESS_COMMAND_LINE_INFORMATION,
            buf.as_mut_ptr() as *mut c_void,
            needed,
            &mut needed,
        );
        CloseHandle(h);
        if status != 0 {
            return String::new();
        }
        // The buffer is a UNICODE_STRING header followed by its own text.
        let us = &*(buf.as_ptr() as *const UnicodeString);
        if us.buffer.is_null() || us.length == 0 {
            return String::new();
        }
        let slice = std::slice::from_raw_parts(us.buffer, (us.length / 2) as usize);
        String::from_utf16_lossy(slice)
    }
}

/// A human role for one process, derived from its command line. Empty when the
/// command line can't be read or the process is a plain single-process app.
pub fn process_role(pid: u32) -> String {
    role_from_cmdline(&read_command_line(pid))
}

/// Value of a `--flag=value` token, unquoted. Flags never contain spaces, so
/// splitting on whitespace is safe even when the exe path does.
fn flag<'a>(cmd: &'a str, prefix: &str) -> Option<&'a str> {
    cmd.split_whitespace()
        .find_map(|t| t.strip_prefix(prefix))
        .map(|v| v.trim_matches('"'))
}

fn role_from_cmdline(cmd: &str) -> String {
    let ty = match flag(cmd, "--type=") {
        Some(t) => t,
        None => {
            // No process type: the main process, or a normal single-exe app.
            return if cmd.is_empty() {
                String::new()
            } else {
                "Main process".to_string()
            };
        }
    };
    match ty {
        "gpu-process" => "GPU process".to_string(),
        "renderer" => {
            if cmd.contains("--extension-process") {
                "Extension".to_string()
            } else {
                "Renderer".to_string()
            }
        }
        "utility" => match flag(cmd, "--utility-sub-type=") {
            // e.g. "network.mojom.NetworkService" -> "Network Service"
            Some(sub) => spaced(sub.rsplit('.').next().unwrap_or(sub)),
            None => "Utility".to_string(),
        },
        "crashpad-handler" => "Crashpad handler".to_string(),
        "broker" => "Broker".to_string(),
        "zygote" => "Zygote".to_string(),
        other => spaced(other),
    }
}

/// "NetworkService" -> "Network Service"; already-spaced words pass through.
fn spaced(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && c.is_ascii_uppercase() {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_chromium_types() {
        assert_eq!(role_from_cmdline("edge.exe --type=gpu-process --x"), "GPU process");
        assert_eq!(role_from_cmdline("edge.exe --type=renderer"), "Renderer");
        assert_eq!(
            role_from_cmdline("edge.exe --type=renderer --extension-process"),
            "Extension"
        );
        assert_eq!(
            role_from_cmdline("edge.exe --type=utility --utility-sub-type=network.mojom.NetworkService"),
            "Network Service"
        );
        assert_eq!(role_from_cmdline("edge.exe --type=crashpad-handler"), "Crashpad handler");
    }

    #[test]
    fn main_and_unknown() {
        assert_eq!(role_from_cmdline("\"C:\\Program Files\\App\\app.exe\""), "Main process");
        assert_eq!(role_from_cmdline(""), "");
    }
}
