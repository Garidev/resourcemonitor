//! Named-pipe query interface for the MCP shim (resmon-mcp.exe) and other
//! local tooling. One request per connection, plain-text command in, JSON
//! out. Read-only: nothing here can mutate the system.
//!
//! Commands:  snapshot | top <cpu|ram|gpu|disk|net> <limit> | app <name>
//!            | history | fps | notify <title>\t<message>
//!            | rules | agents <compact-json-array>
//!
//! Each request is one pipe message — a single client write of any length.
//! Responses are one newline-terminated line, read line-wise by clients.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, ReadFile, WriteFile};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_WAIT,
};

use crate::sampler::{unix_ms, AgentEntry, ProcStat, Shared, Snapshot};

pub const PIPE_NAME: &str = r"\\.\pipe\resmon-mcp";
const PIPE_ACCESS_DUPLEX: u32 = 3;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// JSON string escaping (control chars, quotes, backslash). Shared with the
/// connection payload builder, which is unit-tested off-Windows.
use crate::util::json_escape as esc;

use crate::util::{json_objects, json_str_field};

/// SID of the account this process runs as, as an SDDL string ("S-1-5-21-...").
fn current_user_sid() -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
        let mut buf = vec![0u8; len.max(64) as usize];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as _, len, &mut len) != 0;
        CloseHandle(token);
        if !ok {
            return None;
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut pstr: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut pstr) == 0 || pstr.is_null() {
            return None;
        }
        let mut n = 0;
        while *pstr.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(pstr, n));
        LocalFree(pstr as _);
        Some(s)
    }
}

pub fn run(shared: Arc<Shared>, hwnd: usize) {
    unsafe {
        // The app runs elevated, so a default DACL would lock out the
        // unelevated MCP client. Rather than opening the pipe to Everyone,
        // grant only this user, SYSTEM and Administrators, and explicitly
        // deny network logons so the pipe cannot be reached over SMB.
        // Deny ACE first: order matters in SDDL.
        let rule = match current_user_sid() {
            Some(sid) => format!("D:(D;;GA;;;NU)(A;;GA;;;{})(A;;GA;;;SY)(A;;GA;;;BA)", sid),
            // Fall back to interactive users: still far narrower than Everyone.
            None => "D:(D;;GA;;;NU)(A;;GA;;;IU)(A;;GA;;;SY)(A;;GA;;;BA)".to_string(),
        };
        crate::log(&format!("mcp pipe acl: {}", rule));
        let sddl = wide(&rule);
        let mut sd = std::ptr::null_mut();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1, // SDDL_REVISION_1
            &mut sd,
            std::ptr::null_mut(),
        );
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let name = wide(PIPE_NAME);
        loop {
            // Message mode frames each client WriteFile as one request, so a
            // report larger than any single read buffer arrives whole: reads
            // that drain a long message return ERROR_MORE_DATA until done. A
            // byte-mode pipe cannot know where a request ends without a
            // delimiter the original clients never sent.
            let pipe = CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                4,
                64 * 1024,
                64 * 1024,
                0,
                &sa,
            );
            if pipe == INVALID_HANDLE_VALUE {
                crate::log("mcp pipe: CreateNamedPipeW failed");
                return;
            }
            // A client may connect in the gap before ConnectNamedPipe runs;
            // that surfaces as "failure" with ERROR_PIPE_CONNECTED (535) and
            // must be treated as a successful connection.
            let connected =
                ConnectNamedPipe(pipe, std::ptr::null_mut()) != 0 || GetLastError() == 535;
            if connected {
                // Drain the whole request message; a single fixed-size read
                // would silently truncate a long agent report, and a replace-
                // all report cut short archives still-running agents as done.
                const ERROR_MORE_DATA: u32 = 234;
                const MAX_REQUEST: usize = 1024 * 1024;
                let mut buf = [0u8; 4096];
                let mut msg: Vec<u8> = Vec::new();
                loop {
                    let mut read = 0u32;
                    let ok = ReadFile(pipe, buf.as_mut_ptr() as _, buf.len() as u32, &mut read, std::ptr::null_mut());
                    msg.extend_from_slice(&buf[..read as usize]);
                    if ok != 0 {
                        break;
                    }
                    if GetLastError() != ERROR_MORE_DATA || msg.len() > MAX_REQUEST {
                        msg.clear();
                        break;
                    }
                }
                if !msg.is_empty() {
                    let req = String::from_utf8_lossy(&msg).trim().to_string();
                    // Newline-terminate so the client reads exactly one line
                    // and never blocks waiting for EOF — DisconnectNamedPipe
                    // delivers an abrupt error, not a clean end-of-stream.
                    let mut resp = handle(&shared, hwnd, &req);
                    resp.push('\n');
                    let bytes = resp.as_bytes();
                    let mut written = 0u32;
                    WriteFile(pipe, bytes.as_ptr() as _, bytes.len() as u32, &mut written, std::ptr::null_mut());
                    FlushFileBuffers(pipe);
                }
            }
            DisconnectNamedPipe(pipe);
            CloseHandle(pipe);
        }
    }
}

/// Force per-process sampling on and wait for a snapshot that contains it.
fn fresh_procs(shared: &Arc<Shared>) -> Snapshot {
    shared
        .procs_wanted_until
        .store(unix_ms() + 5000, Ordering::Relaxed);
    let start_seq = shared.seq.load(Ordering::Acquire);
    for _ in 0..40 {
        {
            let snap = shared.snap.lock().unwrap();
            if !snap.procs.is_empty()
                && (shared.seq.load(Ordering::Acquire) > start_seq || shared.panel_open.load(Ordering::Relaxed))
            {
                return snap.clone();
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    shared.snap.lock().unwrap().clone()
}

/// Force the connection sweep on and wait for one that started after this
/// request. Unlike `fresh_procs` it cannot accept whatever is already there:
/// a table swept before the caller asked may describe connections that have
/// since closed, and "who is this talking to right now" has no useful answer
/// built from stale rows.
fn fresh_conns(shared: &Arc<Shared>) -> crate::conns::ConnTable {
    let asked_at = unix_ms();
    shared
        .conns_wanted_until
        .store(asked_at + 5000, Ordering::Relaxed);
    for _ in 0..40 {
        {
            let table = shared.conns.lock().unwrap();
            if table.swept_ms >= asked_at {
                return table.clone();
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    shared.conns.lock().unwrap().clone()
}

fn conns_json(shared: &Arc<Shared>, args: &str) -> String {
    let (filter, limit) = crate::conns::parse_filter(args);
    // Order matters: ask for the sweep before waiting on process data, so
    // both land in the same tick and the pid -> name join has no gaps.
    shared
        .conns_wanted_until
        .store(unix_ms() + 5000, Ordering::Relaxed);
    let snap = fresh_procs(shared);
    let table = fresh_conns(shared);
    let process_of: HashMap<u32, String> =
        snap.procs.iter().map(|p| (p.pid, p.name.clone())).collect();
    let names = shared.names.lock().unwrap();
    let (rows, total) =
        crate::conns::build_rows(&table.rows, &process_of, &names, &filter, limit);
    crate::conns::to_json(
        &rows,
        total,
        shared.etw.dns_ok.load(Ordering::Relaxed),
        table.swept_ms,
        &filter,
    )
}

fn handle(shared: &Arc<Shared>, hwnd: usize, req: &str) -> String {
    if !shared.mcp_enabled.load(Ordering::Relaxed) {
        return "{\"error\":\"MCP is disabled in Resource Monitor settings\"}".to_string();
    }
    let parts: Vec<&str> = req.splitn(3, ' ').collect();
    match parts.first().copied().unwrap_or("") {
        "snapshot" => snapshot_json(&shared.snap.lock().unwrap().clone()),
        "notify" => {
            let rest = req.strip_prefix("notify ").unwrap_or("");
            let (title, message) = rest.split_once('\t').unwrap_or(("Claude Code", rest));
            shared
                .notifications
                .lock()
                .unwrap()
                .push((title.trim().to_string(), message.trim().to_string(), true));
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                    hwnd as _,
                    crate::sampler::WM_APP_NOTIFY,
                    0,
                    0,
                );
            }
            "{\"ok\":true}".to_string()
        }
        "rules" => {
            let text = shared.ai_instructions.lock().unwrap().clone();
            format!("{{\"instructions\":\"{}\"}}", esc(&text))
        }
        "agents" => {
            let rest = req.strip_prefix("agents").unwrap_or("");
            // `agents <key>\t<label>\t<json>`. A payload with no tabs comes
            // from a server built before sessions existed: give it one shared
            // identity rather than dropping its agents on the floor.
            let (key, label, payload) = match rest.splitn(3, '\t').collect::<Vec<_>>()[..] {
                [k, l, p] => (k.trim().to_string(), l.trim().to_string(), p.trim().to_string()),
                _ => ("legacy".to_string(), "AI assistant".to_string(), rest.trim().to_string()),
            };
            let (key, label) = (
                if key.is_empty() { "legacy".to_string() } else { key },
                if label.is_empty() { "AI assistant".to_string() } else { label },
            );
            let now = unix_ms();
            let reported: Vec<AgentEntry> = json_objects(&payload)
                .iter()
                .map(|obj| {
                    use crate::agents::clean_text;
                    let title = clean_text(&json_str_field(obj, "title"));
                    AgentEntry {
                        id: crate::agents::effective_id(&json_str_field(obj, "id"), &title),
                        title,
                        status: clean_text(&json_str_field(obj, "status")),
                        detail: clean_text(&json_str_field(obj, "detail")),
                        seen_ms: now,
                        started_ms: now,
                    }
                })
                .filter(|a| !a.title.is_empty() || !a.detail.is_empty())
                .collect();
            // An assistant repeating an id in one call is a sender bug; keep
            // the first mention rather than showing the agent twice.
            let mut seen: Vec<String> = Vec::new();
            let reported: Vec<AgentEntry> = reported
                .into_iter()
                .filter(|a| {
                    if seen.contains(&a.id) {
                        false
                    } else {
                        seen.push(a.id.clone());
                        true
                    }
                })
                .collect();
            let n = apply_agent_report(shared, &key, &label, reported, now);
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                    hwnd as _,
                    crate::sampler::WM_APP_NOTIFY,
                    0,
                    0,
                );
            }
            format!("{{\"ok\":true,\"agents\":{}}}", n)
        }
        "top" => {
            let metric = parts.get(1).copied().unwrap_or("cpu").to_string();
            let limit: usize = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
            let snap = fresh_procs(shared);
            top_json(&snap, &metric, limit.clamp(1, 50))
        }
        "app" => {
            let name = parts[1..].join(" ");
            let snap = fresh_procs(shared);
            app_json(&snap, name.trim())
        }
        "conns" => {
            let args = req.strip_prefix("conns").unwrap_or("").trim();
            conns_json(shared, args)
        }
        "history" => {
            let h = shared.history.lock().unwrap();
            let items: Vec<String> = h
                .iter()
                .map(|e| {
                    format!(
                        "{{\"unix_ms\":{},\"cpu_pct\":{:.1},\"mem_pct\":{:.1},\"gpu_pct\":{:.1},\"disk_bps\":{},\"net_bps\":{},\"fps\":{}}}",
                        e.unix_ms, e.cpu, e.mem_pct, e.gpu, e.disk_bps, e.net_bps, e.fps
                    )
                })
                .collect();
            format!("{{\"samples\":[{}]}}", items.join(","))
        }
        "fps" => {
            let snap = shared.snap.lock().unwrap().clone();
            let items: Vec<String> = snap
                .fps_list
                .iter()
                .map(|(pid, name, fps)| {
                    format!("{{\"pid\":{},\"name\":\"{}\",\"fps\":{}}}", pid, esc(name), fps)
                })
                .collect();
            format!(
                "{{\"tracking_available\":{},\"presenting_apps\":[{}]}}",
                snap.etw_ok,
                items.join(",")
            )
        }
        _ => "{\"error\":\"unknown command\"}".to_string(),
    }
}

/// Apply one report and, if the user set a log file, append whatever finished.
/// Returns the live count for that session.
fn apply_agent_report(
    shared: &Arc<Shared>,
    key: &str,
    label: &str,
    reported: Vec<AgentEntry>,
    now: u64,
) -> usize {
    let mut sessions = shared.agents.lock().unwrap();
    let mut history = shared.agent_history.lock().unwrap();
    let report = crate::agents::apply_report(&mut sessions, &mut history, key, label, reported, now);
    // Also sweep clients that died without ever saying so.
    let expired = crate::agents::expire(&mut sessions, &mut history, now);
    drop(history);
    drop(sessions);
    log_finished_agents(shared, report.archived.iter().chain(expired.iter()));
    report.live
}

/// Sweep for clients that died without saying so. Reports trigger the same
/// sweep, but a crashed sole client sends no further reports, so the sampler
/// calls this every tick — otherwise its agents would sit live forever and
/// the promised "abandoned" log lines would never be written.
pub fn expire_agents(shared: &Arc<Shared>, hwnd: usize) {
    let expired = {
        let mut sessions = shared.agents.lock().unwrap();
        let mut history = shared.agent_history.lock().unwrap();
        crate::agents::expire(&mut sessions, &mut history, unix_ms())
    };
    if expired.is_empty() {
        return;
    }
    log_finished_agents(shared, expired.iter());
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
            hwnd as _,
            crate::sampler::WM_APP_NOTIFY,
            0,
            0,
        );
    }
}

/// Append finished agents to the user's chosen log file. Empty path means off,
/// as it does for alert rules. A bad path is reported once per write and never
/// allowed to break reporting.
fn log_finished_agents<'a>(
    shared: &Arc<Shared>,
    finished: impl Iterator<Item = &'a crate::agents::FinishedAgent>,
) {
    let path = shared.agent_log_file.lock().unwrap().clone();
    if path.trim().is_empty() {
        return;
    }
    let mut finished = finished.peekable();
    if finished.peek().is_none() {
        return;
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            use std::io::Write;
            for a in finished {
                let _ = writeln!(f, "{}", crate::agents::log_line(a, &crate::sampler::local_timestamp()));
            }
        }
        Err(e) => crate::log(&format!("agent log open failed ({}): {}", path, e)),
    }
}

fn snapshot_json(s: &Snapshot) -> String {
    let drives: Vec<String> = s
        .drives
        .iter()
        .map(|d| {
            format!(
                "{{\"letter\":\"{}\",\"total_bytes\":{},\"free_bytes\":{}}}",
                d.letter, d.total, d.free
            )
        })
        .collect();
    let fps = match &s.fps {
        Some((pid, name, f)) => format!("{{\"pid\":{},\"name\":\"{}\",\"fps\":{}}}", pid, esc(name), f),
        None => "null".to_string(),
    };
    let mem_pct = if s.mem_total > 0 {
        s.mem_used as f64 / s.mem_total as f64 * 100.0
    } else {
        0.0
    };
    format!(
        "{{\"cpu_pct\":{:.1},\"mem_used_bytes\":{},\"mem_total_bytes\":{},\"mem_pct\":{:.1},\
         \"gpu_pct\":{:.1},\"gpu_available\":{},\"disk_read_bps\":{},\"disk_write_bps\":{},\
         \"net_rx_bps\":{},\"net_tx_bps\":{},\"fps\":{},\"audio_peak\":{:.2},\
         \"per_app_network_available\":{},\"drives\":[{}]}}",
        s.cpu_pct,
        s.mem_used,
        s.mem_total,
        mem_pct,
        s.gpu_pct,
        s.gpu_ok,
        s.disk_read_bps,
        s.disk_write_bps,
        s.net_rx_bps,
        s.net_tx_bps,
        fps,
        s.audio_peak,
        s.net_ok,
        drives.join(",")
    )
}

fn metric_of(p: &ProcStat, metric: &str) -> f64 {
    match metric {
        "ram" => p.ws_private as f64,
        "gpu" => p.gpu_pct as f64,
        "disk" => p.io_bps as f64,
        "net" => p.net_bps as f64,
        _ => p.cpu_pct as f64,
    }
}

fn top_json(snap: &Snapshot, metric: &str, limit: usize) -> String {
    let mut agg: HashMap<&str, (f64, u32)> = HashMap::new();
    for p in &snap.procs {
        let e = agg.entry(p.name.as_str()).or_insert((0.0, 0));
        e.0 += metric_of(p, metric);
        e.1 += 1;
    }
    let mut rows: Vec<(&str, f64, u32)> =
        agg.into_iter().map(|(k, (v, n))| (k, v, n)).collect();
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    rows.retain(|(_, v, _)| *v > 0.0);
    rows.truncate(limit);
    let unit = match metric {
        "ram" => "bytes",
        "disk" | "net" => "bytes_per_sec",
        _ => "percent",
    };
    let items: Vec<String> = rows
        .iter()
        .map(|(name, v, n)| {
            format!(
                "{{\"name\":\"{}\",\"value\":{:.1},\"processes\":{}}}",
                esc(name),
                v,
                n
            )
        })
        .collect();
    format!(
        "{{\"metric\":\"{}\",\"unit\":\"{}\",\"apps\":[{}]}}",
        esc(metric),
        unit,
        items.join(",")
    )
}

fn app_json(snap: &Snapshot, name: &str) -> String {
    let procs: Vec<&ProcStat> = snap
        .procs
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case(name))
        .collect();
    if procs.is_empty() {
        return format!("{{\"name\":\"{}\",\"running\":false}}", esc(name));
    }
    let cpu: f32 = procs.iter().map(|p| p.cpu_pct).sum();
    let ram_priv: u64 = procs.iter().map(|p| p.ws_private).sum();
    let ram_total: u64 = procs.iter().map(|p| p.ws_bytes).sum();
    let gpu: f32 = procs.iter().map(|p| p.gpu_pct).sum();
    let disk: u64 = procs.iter().map(|p| p.io_bps).sum();
    let net: u64 = procs.iter().map(|p| p.net_bps).sum();
    let pids: Vec<String> = procs.iter().map(|p| p.pid.to_string()).collect();
    let fps = snap
        .fps_list
        .iter()
        .filter(|(_, n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, _, f)| *f)
        .max();
    format!(
        "{{\"name\":\"{}\",\"running\":true,\"process_count\":{},\"pids\":[{}],\
         \"cpu_pct\":{:.1},\"ram_bytes\":{},\"ram_with_shared_bytes\":{},\"gpu_pct\":{:.1},\
         \"disk_bps\":{},\"net_bps\":{},\"fps\":{}}}",
        esc(name),
        procs.len(),
        pids.join(","),
        cpu,
        ram_priv,
        ram_total,
        gpu,
        disk,
        net,
        fps.map(|f| f.to_string()).unwrap_or_else(|| "null".into()),
    )
}
