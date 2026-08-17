//! 1-second sampling of system + per-process metrics via native APIs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, MAX_PATH};
use windows_sys::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows_sys::Win32::NetworkManagement::Ndis::IF_OPER_STATUS;
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{
    GetSystemTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::etw::EtwShared;

pub const WM_APP_SNAPSHOT: u32 = 0x8002; // WM_APP + 2
pub const WM_APP_NOTIFY: u32 = 0x8003; // WM_APP + 3 — MCP notification pending

const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const DRIVE_FIXED: u32 = 3;
const IF_OPER_STATUS_UP: IF_OPER_STATUS = 1;

// ---------------------------------------------------------------- snapshot

#[derive(Clone, Default)]
pub struct DriveInfo {
    pub letter: char,
    pub total: u64,
    pub free: u64,
}

#[derive(Clone, Default)]
pub struct ProcStat {
    pub pid: u32,
    pub name: String,
    pub cpu_pct: f32,
    /// Total working set (private + shared pages).
    pub ws_bytes: u64,
    /// Private working set — what Task Manager's memory column shows.
    pub ws_private: u64,
    /// Read + write + other transfers. Stays the figure everything sorts and
    /// alerts on, so splitting the two directions out below changes no ranking.
    pub io_bps: u64,
    /// The two halves of `io_bps`, for the views that show a direction. `other`
    /// — named pipes, device control — belongs to neither, so these two do not
    /// have to add up to `io_bps` and are not presented as if they do.
    pub io_read_bps: u64,
    pub io_write_bps: u64,
    pub net_bps: u64,
    /// The two halves of `net_bps`. Kernel-Network reports sent and received as
    /// separate events, so this costs nothing to carry.
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
    pub gpu_pct: f32,
    /// Playback level 0..1 for this process (0 = not playing).
    pub audio: f32,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub cpu_pct: f32,
    pub mem_total: u64,
    pub mem_used: u64,
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
    pub drives: Vec<DriveInfo>,
    /// (pid, process name, frames per second) of the top presenter, if any.
    pub fps: Option<(u32, String, u32)>,
    /// Every app currently presenting frames, sorted by FPS descending.
    pub fps_list: Vec<(u32, String, u32)>,
    pub etw_ok: bool,
    /// Kernel-Network provider enabled — per-app network data is flowing.
    pub net_ok: bool,
    /// Overall GPU % (busiest engine type); sampled only while panel is open.
    pub gpu_pct: f32,
    pub gpu_ok: bool,
    /// Loudest active playback session, 0..1 (0 = silence).
    pub audio_peak: f32,
    pub audio_ok: bool,
    /// Per-logical-core busy %; sampled only while the panel is open.
    pub core_pcts: Vec<f32>,
    /// Per-process stats; populated only while the panel is open.
    pub procs: Vec<ProcStat>,
}

/// One summary sample kept for MCP/remote history queries.
#[derive(Clone, Copy)]
pub struct HistEntry {
    pub unix_ms: u64,
    pub cpu: f32,
    pub mem_pct: f32,
    pub gpu: f32,
    pub disk_bps: u64,
    pub net_bps: u64,
    pub fps: u32,
}

pub struct Shared {
    pub snap: Mutex<Snapshot>,
    pub panel_open: AtomicBool,
    pub interval_ms: AtomicU32,
    /// Incremented every published snapshot; lets readers wait for fresh data.
    pub seq: AtomicU64,
    /// Recent summary samples (~6 min at 1 s cadence).
    pub history: Mutex<VecDeque<HistEntry>>,
    /// Unix ms deadline until which per-process sampling is forced on
    /// (MCP queries need process data while the panel is closed).
    pub procs_wanted_until: AtomicU64,
    /// Last connection sweep. Separate from `snap` on purpose: hundreds of
    /// rows that only the connection views and MCP ever read.
    pub conns: Mutex<crate::conns::ConnTable>,
    /// Remote address -> hostname, filled by DNS-Client ETW events and by
    /// reverse lookups. Shared with the resolver thread, hence its own Arc.
    pub names: Arc<Mutex<crate::conns::NameMap>>,
    /// Unix ms deadline until which the connection sweep is forced on, the
    /// same keep-alive `procs_wanted_until` provides for process data.
    pub conns_wanted_until: AtomicU64,
    /// A connection view is on screen, so keep sweeping every tick.
    pub conns_view_open: AtomicBool,
    /// Pending (title, message) notifications from MCP clients; the UI
    /// thread drains these into tray balloons.
    /// (title, message, is_mcp) — is_mcp entries also land in the Messages list.
    pub notifications: Mutex<Vec<(String, String, bool)>>,
    /// MCP server enabled — gates pipe queries and notifications.
    pub mcp_enabled: AtomicBool,
    /// Live rule set for the alert engine (editable in UI).
    pub rules: Mutex<Vec<crate::rules::Rule>>,
    /// What connected AI tools have reported they are working on, one entry
    /// per AI session. Each `report_agents` call replaces only its own
    /// session's list, so two sessions cannot overwrite each other.
    pub agents: Mutex<Vec<AgentSession>>,
    /// Agents that have finished, newest first, across all sessions. Capped;
    /// see `AGENT_HISTORY_MAX`.
    pub agent_history: Mutex<std::collections::VecDeque<FinishedAgent>>,
    /// Instructions to hand AI clients, kept in sync with settings so the
    /// pipe thread can serve them without touching the UI.
    pub ai_instructions: Mutex<String>,
    /// Where to append finished agents, mirrored from settings so the pipe
    /// thread can write it without touching the UI. Empty means off.
    pub agent_log_file: Mutex<String>,
    pub etw: EtwShared,
}

// Agent types and rules live in `agents`, which is platform-independent so the
// archiving logic can be unit-tested. Re-exported here because the UI and pipe
// layers already reach for them through `sampler`.
pub use crate::agents::{AgentEntry, AgentSession, FinishedAgent, AGENT_STALE_MS};

impl Shared {
    pub fn new(interval_ms: u32, mcp_enabled: bool, rules: Vec<crate::rules::Rule>) -> Self {
        Shared {
            snap: Mutex::new(Snapshot::default()),
            panel_open: AtomicBool::new(false),
            interval_ms: AtomicU32::new(interval_ms),
            seq: AtomicU64::new(0),
            history: Mutex::new(VecDeque::with_capacity(360)),
            procs_wanted_until: AtomicU64::new(0),
            conns: Mutex::new(crate::conns::ConnTable::default()),
            names: Arc::new(Mutex::new(crate::conns::NameMap::default())),
            conns_wanted_until: AtomicU64::new(0),
            conns_view_open: AtomicBool::new(false),
            notifications: Mutex::new(Vec::new()),
            mcp_enabled: AtomicBool::new(mcp_enabled),
            rules: Mutex::new(rules),
            agents: Mutex::new(Vec::new()),
            agent_history: Mutex::new(std::collections::VecDeque::new()),
            ai_instructions: Mutex::new(String::new()),
            agent_log_file: Mutex::new(String::new()),
            etw: EtwShared::new(),
        }
    }
}

// ------------------------------------------------- NtQuerySystemInformation

#[link(name = "ntdll")]
extern "system" {
    fn NtQuerySystemInformation(
        class: u32,
        info: *mut c_void,
        len: u32,
        ret_len: *mut u32,
    ) -> i32;
}

const SYSTEM_PERFORMANCE_INFORMATION: u32 = 2;
const SYSTEM_PROCESS_INFORMATION_CLASS: u32 = 5;
const SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION_CLASS: u32 = 8;

/// Per-core times; KernelTime includes IdleTime.
#[repr(C)]
struct ProcessorPerfInfo {
    idle_time: i64,
    kernel_time: i64,
    user_time: i64,
    dpc_time: i64,
    interrupt_time: i64,
    interrupt_count: u32,
}

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

/// x64 layout of SYSTEM_PROCESS_INFORMATION (winternl.h + observed fields).
#[repr(C)]
struct SystemProcessInfo {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name: UnicodeString,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual_size: usize,
    virtual_size: usize,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_page_count: usize,
    read_operation_count: i64,
    write_operation_count: i64,
    other_operation_count: i64,
    read_transfer_count: i64,
    write_transfer_count: i64,
    other_transfer_count: i64,
}

/// Head of SYSTEM_PERFORMANCE_INFORMATION — we only need the IO transfer counters.
#[repr(C)]
struct SystemPerfInfoHead {
    idle_process_time: i64,
    io_read_transfer_count: i64,
    io_write_transfer_count: i64,
    io_other_transfer_count: i64,
}

// ---------------------------------------------------------------- sampler

struct PidTimes {
    cpu_100ns: u64,
    io_bytes: u64,
    read_bytes: u64,
    write_bytes: u64,
}

pub struct Sampler {
    hwnd: usize,
    shared: Arc<Shared>,
    ncpus: f64,
    last: Instant,
    prev_idle: u64,
    prev_busy_total: u64,
    prev_net_rx: u64,
    prev_net_tx: u64,
    prev_disk_read: u64,
    prev_disk_write: u64,
    prev_pids: HashMap<u32, PidTimes>,
    /// pid -> (received bytes/s, sent bytes/s) for the last tick.
    pid_net_rates: HashMap<u32, (u64, u64)>,
    pid_gpu: HashMap<u32, f32>,
    name_cache: HashMap<u32, String>,
    proc_buf: Vec<u8>,
    prev_cores: Vec<(u64, u64)>,
    gpu: Option<crate::gpu::GpuSampler>,
    gpu_tried: bool,
    audio: Option<crate::audio::AudioSampler>,
    audio_tried: bool,
    pid_audio: HashMap<u32, f32>,
    /// Last-fired time per rule, keyed by raw rule line (survives edits).
    rule_last: HashMap<String, Instant>,
    /// (pid, remote address, remote port) seen on the previous tick, so a
    /// connection rule can tell a new connection from one it already reported.
    prev_conns: HashSet<(u32, std::net::IpAddr, u16)>,
    logged_cores: bool,
}

fn filetime_u64(ft: &FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

impl Sampler {
    pub fn new(hwnd: usize, shared: Arc<Shared>) -> Self {
        let ncpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1) as f64;
        Sampler {
            rule_last: HashMap::new(),
            prev_conns: HashSet::new(),
            logged_cores: false,
            hwnd,
            shared,
            ncpus,
            last: Instant::now(),
            prev_idle: 0,
            prev_busy_total: 0,
            prev_net_rx: 0,
            prev_net_tx: 0,
            prev_disk_read: 0,
            prev_disk_write: 0,
            prev_pids: HashMap::new(),
            pid_net_rates: HashMap::new(),
            pid_gpu: HashMap::new(),
            name_cache: HashMap::new(),
            proc_buf: Vec::with_capacity(512 * 1024),
            prev_cores: Vec::new(),
            gpu: None,
            gpu_tried: false,
            audio: None,
            audio_tried: false,
            pid_audio: HashMap::new(),
        }
    }

    pub fn run(mut self) {
        // Prime the counters so the first visible tick has sane deltas.
        self.tick(true);
        loop {
            let ms = self.shared.interval_ms.load(Ordering::Relaxed).clamp(250, 10_000);
            std::thread::sleep(Duration::from_millis(ms as u64));
            self.tick(false);
            unsafe {
                PostMessageW(self.hwnd as _, WM_APP_SNAPSHOT, 0, 0);
            }
        }
    }

    fn tick(&mut self, priming: bool) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64().max(0.001);
        self.last = now;

        let mut snap = Snapshot::default();

        self.sample_cpu(&mut snap);
        sample_memory(&mut snap);
        self.sample_network(&mut snap, elapsed);
        self.sample_disk_io(&mut snap, elapsed);
        snap.drives = sample_drives();
        self.sample_etw(&mut snap, elapsed);


        let rules: Vec<crate::rules::Rule> =
            self.shared.rules.lock().unwrap().iter().filter(|r| r.enabled).cloned().collect();
        let need_procs = rules.iter().any(|r| r.needs_procs());
        let need_gpu = rules.iter().any(|r| r.needs_gpu());
        let need_conns = rules.iter().any(|r| r.needs_conns());

        let panel_open = self.shared.panel_open.load(Ordering::Relaxed)
            || unix_ms() < self.shared.procs_wanted_until.load(Ordering::Relaxed);
        // Per-process, per-core and GPU sampling: needed when the panel is
        // open (and always when a log rule references them); kept warm one
        // tick after priming so drill-downs have deltas immediately.
        if panel_open || priming || need_gpu {
            self.sample_gpu(&mut snap);
        }
        if panel_open || priming {
            self.sample_audio(&mut snap);
        }
        if panel_open || priming {
            self.sample_cores(&mut snap);
        } else {
            self.prev_cores.clear();
        }
        if panel_open || priming || need_procs {
            self.sample_processes(&mut snap, elapsed);
        } else {
            self.prev_pids.clear();
        }
        self.sample_conns(need_conns);

        if !priming {
            crate::mcp_pipe::expire_agents(&self.shared, self.hwnd);
            self.eval_rules(&rules, &snap);
            self.eval_conn_rules(&rules, &snap);
            let mem_pct = if snap.mem_total > 0 {
                snap.mem_used as f32 / snap.mem_total as f32 * 100.0
            } else {
                0.0
            };
            let entry = HistEntry {
                unix_ms: unix_ms(),
                cpu: snap.cpu_pct,
                mem_pct,
                gpu: snap.gpu_pct,
                disk_bps: snap.disk_read_bps + snap.disk_write_bps,
                net_bps: snap.net_rx_bps + snap.net_tx_bps,
                fps: snap.fps.as_ref().map(|(_, _, f)| *f).unwrap_or(0),
            };
            {
                let mut h = self.shared.history.lock().unwrap();
                if h.len() >= 360 {
                    h.pop_front();
                }
                h.push_back(entry);
            }
            *self.shared.snap.lock().unwrap() = snap;
            self.shared.seq.fetch_add(1, Ordering::Release);
        }
    }

    fn sample_audio(&mut self, snap: &mut Snapshot) {
        if self.audio.is_none() && !self.audio_tried {
            self.audio_tried = true;
            self.audio = crate::audio::AudioSampler::new();
        }
        if let Some(a) = self.audio.as_mut() {
            let (peak, per_pid) = a.sample();
            snap.audio_peak = peak;
            snap.audio_ok = true;
            self.pid_audio = per_pid;
        }
    }

    fn eval_rules(&mut self, rules: &[crate::rules::Rule], snap: &Snapshot) {
        use crate::rules::{ProcSub, RMetric};
        for rule in rules {
            let Some(metric) = rule.metric() else { continue };
            let value = match metric {
                RMetric::Cpu => snap.cpu_pct as f64,
                RMetric::RamPct => {
                    if snap.mem_total == 0 {
                        continue;
                    }
                    snap.mem_used as f64 / snap.mem_total as f64 * 100.0
                }
                RMetric::Gpu => {
                    if !snap.gpu_ok {
                        continue;
                    }
                    snap.gpu_pct as f64
                }
                RMetric::DiskMbs => (snap.disk_read_bps + snap.disk_write_bps) as f64 / 1048576.0,
                RMetric::NetMbs => (snap.net_rx_bps + snap.net_tx_bps) as f64 / 1048576.0,
                RMetric::Fps => snap.fps.as_ref().map(|(_, _, f)| *f as f64).unwrap_or(0.0),
                RMetric::SoundPct => snap.audio_peak as f64 * 100.0,
                RMetric::Proc { name, sub } => {
                    let mut v = 0.0f64;
                    for p in snap.procs.iter().filter(|p| p.name.eq_ignore_ascii_case(name)) {
                        v += match sub {
                            ProcSub::Cpu => p.cpu_pct as f64,
                            ProcSub::RamMb => p.ws_private as f64 / 1048576.0,
                            ProcSub::DiskMbs => p.io_bps as f64 / 1048576.0,
                            ProcSub::NetMbs => p.net_bps as f64 / 1048576.0,
                            ProcSub::SoundPct => p.audio as f64 * 100.0,
                        };
                    }
                    v
                }
            };
            if !rule.triggered(value) {
                continue;
            }
            let (title, body) = alert_text(rule, value, snap);
            self.fire_rule(rule, &format!("value={:.1}", value), title, body, snap);
        }
    }

    /// Deliver one triggered rule: cooldown, log file, desktop notification.
    /// Shared by threshold and connection rules so both obey the same
    /// per-rule cooldown and the same delivery choices.
    fn fire_rule(
        &mut self,
        rule: &crate::rules::Rule,
        detail: &str,
        title: String,
        body: String,
        snap: &Snapshot,
    ) {
        if let Some(last) = self.rule_last.get(&rule.raw) {
            if last.elapsed().as_secs() < rule.cooldown_s {
                return;
            }
        }
        if self.rule_last.len() > 64 {
            self.rule_last.clear();
        }
        self.rule_last.insert(rule.raw.clone(), Instant::now());
        write_rule_log(rule, detail, snap);
        if rule.notify {
            self.shared.notifications.lock().unwrap().push((title, body, false));
            unsafe { PostMessageW(self.hwnd as _, WM_APP_NOTIFY, 0, 0); }
        }
    }

    /// Connection rules, which fire on either of two signals: a connection
    /// that was not open on the previous tick, or a DNS lookup seen since it.
    ///
    /// Polling alone misses anything that opens and closes inside one
    /// interval — a presence heartbeat looks exactly like that — while DNS
    /// alone is blind to hardcoded addresses and to clients that resolve over
    /// HTTPS. Together they cover both, and the per-rule cooldown keeps a
    /// chatty endpoint from flooding.
    fn eval_conn_rules(&mut self, rules: &[crate::rules::Rule], snap: &Snapshot) {
        // Drained every tick regardless: the ETW callback keeps filling this
        // whether or not a rule is armed to read it.
        let events = self.shared.etw.take_dns_events();
        let armed: Vec<&crate::rules::Rule> =
            rules.iter().filter(|r| r.needs_conns()).collect();
        if armed.is_empty() {
            self.prev_conns.clear();
            return;
        }
        let table = self.shared.conns.lock().unwrap().clone();
        let process_of: HashMap<u32, String> =
            snap.procs.iter().map(|p| (p.pid, p.name.clone())).collect();
        let rows = {
            let names = self.shared.names.lock().unwrap();
            let (rows, _) = crate::conns::build_rows(
                &table.rows,
                &process_of,
                &names,
                &crate::conns::Filter {
                    // A rule asks about a specific endpoint, so nothing is
                    // filtered out ahead of it — including LAN and loopback,
                    // which is the whole point of a "port 445" rule.
                    scope: crate::conns::ScopeFilter::All,
                    state: crate::conns::StateFilter::All,
                    ..Default::default()
                },
                usize::MAX,
            );
            rows
        };

        // Identity of a connection for "is this new": the same app reaching
        // the same endpoint again after it closed is worth reporting again,
        // but the same socket seen on ten consecutive ticks is not.
        let mut current = HashSet::with_capacity(rows.len());
        for r in &rows {
            if let Some((ip, port)) = r.conn.remote {
                current.insert((r.conn.pid, ip, port));
            }
        }
        let first_tick = self.prev_conns.is_empty();

        for rule in &armed {
            let Some((field, pattern)) = rule.conn() else { continue };
            let mut hit: Option<(String, String)> = None;
            // A lookup is the earlier signal, so it is preferred when both
            // fire on the same tick.
            for ev in &events {
                let process = process_of.get(&ev.pid).cloned().unwrap_or_default();
                if crate::rules::dns_matches(field, pattern, &ev.host, &ev.addrs, &process) {
                    hit = Some(conn_alert_text(rule, &describe_lookup(&process, ev)));
                    break;
                }
            }
            if hit.is_none() {
                for r in &rows {
                    let Some((ip, port)) = r.conn.remote else { continue };
                    // On the first tick after arming, everything already open
                    // would look new; reporting a hundred existing
                    // connections at once is noise, not an alert.
                    if first_tick || self.prev_conns.contains(&(r.conn.pid, ip, port)) {
                        continue;
                    }
                    if crate::rules::conn_matches(field, pattern, r) {
                        hit = Some(conn_alert_text(rule, &describe_conn(r)));
                        break;
                    }
                }
            }
            if let Some((title, body)) = hit {
                let detail = body.clone();
                self.fire_rule(rule, &detail, title, body, snap);
            }
        }
        self.prev_conns = current;
    }

    fn sample_cpu(&mut self, snap: &mut Snapshot) {
        let mut idle = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut kernel = idle;
        let mut user = idle;
        if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
            return;
        }
        let idle = filetime_u64(&idle);
        // kernel time includes idle time
        let total = filetime_u64(&kernel) + filetime_u64(&user);
        let idle_d = idle.saturating_sub(self.prev_idle);
        let total_d = total.saturating_sub(self.prev_busy_total);
        if self.prev_busy_total != 0 && total_d > 0 {
            snap.cpu_pct = crate::util::cpu_pct(total_d.saturating_sub(idle_d), total_d);
        }
        self.prev_idle = idle;
        self.prev_busy_total = total;
    }

    fn sample_network(&mut self, snap: &mut Snapshot, elapsed: f64) {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if unsafe { GetIfTable2(&mut table) } != 0 || table.is_null() {
            return;
        }
        let (mut rx, mut tx) = (0u64, 0u64);
        unsafe {
            let t = &*table;
            let rows = std::slice::from_raw_parts(t.Table.as_ptr(), t.NumEntries as usize);
            for r in rows {
                if r.Type != IF_TYPE_SOFTWARE_LOOPBACK && r.OperStatus == IF_OPER_STATUS_UP {
                    rx += r.InOctets;
                    tx += r.OutOctets;
                }
            }
            FreeMibTable(table as *const c_void);
        }
        if self.prev_net_rx != 0 || self.prev_net_tx != 0 {
            snap.net_rx_bps = crate::util::rate(self.prev_net_rx, rx, elapsed);
            snap.net_tx_bps = crate::util::rate(self.prev_net_tx, tx, elapsed);
        }
        self.prev_net_rx = rx;
        self.prev_net_tx = tx;
    }

    fn sample_disk_io(&mut self, snap: &mut Snapshot, elapsed: f64) {
        let mut buf = [0u8; 1024];
        let mut ret = 0u32;
        let st = unsafe {
            NtQuerySystemInformation(
                SYSTEM_PERFORMANCE_INFORMATION,
                buf.as_mut_ptr() as _,
                buf.len() as u32,
                &mut ret,
            )
        };
        if st < 0 || (ret as usize) < std::mem::size_of::<SystemPerfInfoHead>() {
            return;
        }
        let head = unsafe { &*(buf.as_ptr() as *const SystemPerfInfoHead) };
        let read = head.io_read_transfer_count as u64;
        let write = head.io_write_transfer_count as u64;
        if self.prev_disk_read != 0 || self.prev_disk_write != 0 {
            snap.disk_read_bps = crate::util::rate(self.prev_disk_read, read, elapsed);
            snap.disk_write_bps = crate::util::rate(self.prev_disk_write, write, elapsed);
        }
        self.prev_disk_read = read;
        self.prev_disk_write = write;
    }

    fn sample_etw(&mut self, snap: &mut Snapshot, elapsed: f64) {
        snap.etw_ok = self.shared.etw.ok.load(Ordering::Relaxed);
        snap.net_ok = self.shared.etw.knet_ok.load(Ordering::Relaxed);

        let presents = std::mem::take(&mut *self.shared.etw.presents.lock().unwrap());
        let mut list: Vec<(u32, String, u32)> = Vec::new();
        for (pid, count) in presents {
            let fps = (count as f64 / elapsed) as u32;
            if fps < 2 {
                continue; // ordinary window churn, not continuous rendering
            }
            let name = self.process_name(pid);
            if name.eq_ignore_ascii_case("dwm.exe") {
                continue; // the compositor presents constantly; not a game
            }
            list.push((pid, name, fps));
        }
        list.sort_by(|a, b| b.2.cmp(&a.2));
        // Headline FPS keeps a higher bar so a video thumbnail at 4 fps
        // doesn't read as "the game".
        snap.fps = list.iter().find(|e| e.2 >= 5).cloned();
        snap.fps_list = list;

        // Per-pid network bytes since last tick -> rates, folded into procs
        // later. Kept as (received, sent) rather than summed on the way in: the
        // provider already separates them, and a view that wants to name a
        // direction cannot recover it from a total.
        let net = std::mem::take(&mut *self.shared.etw.net.lock().unwrap());
        self.pid_net_rates = net
            .into_iter()
            .map(|(pid, (t, r))| {
                (pid, ((r as f64 / elapsed) as u64, (t as f64 / elapsed) as u64))
            })
            .collect();
    }

    fn sample_gpu(&mut self, snap: &mut Snapshot) {
        if self.gpu.is_none() && !self.gpu_tried {
            self.gpu_tried = true;
            self.gpu = crate::gpu::GpuSampler::new();
            if self.gpu.is_none() {
                crate::log("GPU PDH counters unavailable");
            }
        }
        if let Some(g) = self.gpu.as_mut() {
            let (overall, per_pid) = g.sample();
            snap.gpu_pct = overall;
            snap.gpu_ok = true;
            self.pid_gpu = per_pid;
        }
    }

    fn sample_cores(&mut self, snap: &mut Snapshot) {
        let mut buf = vec![0u8; 256 * std::mem::size_of::<ProcessorPerfInfo>()];
        let mut ret = 0u32;
        let st = unsafe {
            NtQuerySystemInformation(
                SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION_CLASS,
                buf.as_mut_ptr() as _,
                buf.len() as u32,
                &mut ret,
            )
        };
        if st < 0 {
            if !self.logged_cores {
                self.logged_cores = true;
                crate::log(&format!("core query failed: status {:#x}", st));
            }
            return;
        }
        let n = ret as usize / std::mem::size_of::<ProcessorPerfInfo>();
        if !self.logged_cores {
            self.logged_cores = true;
            crate::log(&format!(
                "cores reported: {} ({} bytes, parallelism {})",
                n, ret, self.ncpus
            ));
        }
        let cores = unsafe {
            std::slice::from_raw_parts(buf.as_ptr() as *const ProcessorPerfInfo, n)
        };
        let mut pcts = Vec::with_capacity(n);
        let mut cur = Vec::with_capacity(n);
        for (i, c) in cores.iter().enumerate() {
            let idle = c.idle_time as u64;
            let total = (c.kernel_time + c.user_time) as u64;
            if let Some((pidle, ptotal)) = self.prev_cores.get(i) {
                let total_d = total.saturating_sub(*ptotal);
                let idle_d = idle.saturating_sub(*pidle);
                pcts.push(crate::util::cpu_pct(total_d.saturating_sub(idle_d), total_d));
            } else {
                pcts.push(0.0);
            }
            cur.push((idle, total));
        }
        self.prev_cores = cur;
        snap.core_pcts = pcts;
    }

    /// Enumerate live connections, but only while something is looking: a
    /// connection view is open, or an MCP request has asked within the last
    /// few seconds. Idle with the panel closed, this does nothing at all.
    ///
    /// The stale table is cleared rather than left behind, so a view opening
    /// later never shows connections from minutes ago as if they were live.
    fn sample_conns(&mut self, need_conns: bool) {
        let wanted = need_conns
            || self.shared.conns_view_open.load(Ordering::Relaxed)
            || unix_ms() < self.shared.conns_wanted_until.load(Ordering::Relaxed);
        if !wanted {
            let mut table = self.shared.conns.lock().unwrap();
            if !table.rows.is_empty() {
                *table = crate::conns::ConnTable::default();
            }
            return;
        }
        let rows = crate::conns::sweep();
        // Anything public we have no name for yet gets queued for a PTR
        // lookup; the worker answers off-thread and the next sweep picks it up.
        crate::conns::queue_reverse_lookups(&rows, &self.shared.names);
        *self.shared.conns.lock().unwrap() =
            crate::conns::ConnTable { rows, swept_ms: unix_ms() };
    }

    fn sample_processes(&mut self, snap: &mut Snapshot, elapsed: f64) {
        if !self.query_process_buffer() {
            return;
        }
        let mut procs = Vec::with_capacity(128);
        let mut new_pids = HashMap::new();
        let mut offset = 0usize;
        loop {
            if offset + std::mem::size_of::<SystemProcessInfo>() > self.proc_buf.len() {
                break;
            }
            let info = unsafe { &*(self.proc_buf.as_ptr().add(offset) as *const SystemProcessInfo) };
            let pid = info.unique_process_id as u32;
            if pid != 0 {
                let name = unicode_to_string(&info.image_name);
                let cpu_100ns = (info.kernel_time + info.user_time) as u64;
                let read_bytes = info.read_transfer_count as u64;
                let write_bytes = info.write_transfer_count as u64;
                let io_bytes =
                    read_bytes + write_bytes + info.other_transfer_count as u64;
                let (rx, tx) = *self.pid_net_rates.get(&pid).unwrap_or(&(0, 0));
                let mut stat = ProcStat {
                    pid,
                    name,
                    cpu_pct: 0.0,
                    ws_bytes: info.working_set_size as u64,
                    ws_private: info.working_set_private_size.max(0) as u64,
                    io_bps: 0,
                    io_read_bps: 0,
                    io_write_bps: 0,
                    net_bps: rx + tx,
                    net_rx_bps: rx,
                    net_tx_bps: tx,
                    gpu_pct: *self.pid_gpu.get(&pid).unwrap_or(&0.0),
                    audio: *self.pid_audio.get(&pid).unwrap_or(&0.0),
                };
                if let Some(prev) = self.prev_pids.get(&pid) {
                    let cpu_d = cpu_100ns.saturating_sub(prev.cpu_100ns);
                    // 100ns units -> fraction of wall time across all cores
                    stat.cpu_pct =
                        ((cpu_d as f64 / (elapsed * 1e7)) / self.ncpus * 100.0).min(100.0) as f32;
                    stat.io_bps = crate::util::rate(prev.io_bytes, io_bytes, elapsed);
                    stat.io_read_bps = crate::util::rate(prev.read_bytes, read_bytes, elapsed);
                    stat.io_write_bps = crate::util::rate(prev.write_bytes, write_bytes, elapsed);
                }
                new_pids.insert(pid, PidTimes { cpu_100ns, io_bytes, read_bytes, write_bytes });
                procs.push(stat);
            }
            if info.next_entry_offset == 0 {
                break;
            }
            offset += info.next_entry_offset as usize;
        }
        self.prev_pids = new_pids;
        snap.procs = procs;
    }

    fn query_process_buffer(&mut self) -> bool {
        if self.proc_buf.capacity() < 512 * 1024 {
            self.proc_buf.reserve(512 * 1024);
        }
        for _ in 0..4 {
            let cap = self.proc_buf.capacity();
            self.proc_buf.resize(cap, 0);
            let mut ret = 0u32;
            let st = unsafe {
                NtQuerySystemInformation(
                    SYSTEM_PROCESS_INFORMATION_CLASS,
                    self.proc_buf.as_mut_ptr() as _,
                    cap as u32,
                    &mut ret,
                )
            };
            if st >= 0 {
                self.proc_buf.truncate(ret as usize);
                return true;
            }
            // STATUS_INFO_LENGTH_MISMATCH: grow and retry
            self.proc_buf.reserve(cap); // double
        }
        false
    }

    fn process_name(&mut self, pid: u32) -> String {
        if let Some(n) = self.name_cache.get(&pid) {
            return n.clone();
        }
        let name = query_image_basename(pid).unwrap_or_else(|| format!("pid {}", pid));
        if self.name_cache.len() > 256 {
            self.name_cache.clear();
        }
        self.name_cache.insert(pid, name.clone());
        name
    }
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn local_timestamp() -> String {
    let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut st) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

/// One line describing a connection, for the alert and the log: who, where,
/// and the address behind the name so the name is never the only evidence.
fn describe_conn(row: &crate::conns::Row) -> String {
    let app = if row.process.is_empty() { "an app".to_string() } else { row.process.clone() };
    let endpoint = match (&row.host, row.conn.remote) {
        (Some(h), Some((ip, port))) => format!("{} ({}:{})", h, ip, port),
        (None, Some((ip, port))) => format!("{}:{}", ip, port),
        _ => "an endpoint".to_string(),
    };
    format!("{} connected to {}", app, endpoint)
}

/// The same, for a rule that fired on a name lookup rather than a socket.
fn describe_lookup(process: &str, ev: &crate::etw::DnsEvent) -> String {
    let app = if process.is_empty() { "an app".to_string() } else { process.to_string() };
    let addrs: Vec<String> = ev.addrs.iter().take(2).map(|a| a.to_string()).collect();
    if addrs.is_empty() {
        format!("{} looked up {}", app, ev.host)
    } else {
        format!("{} looked up {} ({})", app, ev.host, addrs.join(", "))
    }
}

fn conn_alert_text(rule: &crate::rules::Rule, what: &str) -> (String, String) {
    let title = match rule.conn() {
        Some((field, pattern)) => format!("Alert: {} matches {}", field.label(), pattern),
        None => "Alert: connection".to_string(),
    };
    (title, format!("{}.", what))
}

fn alert_text(rule: &crate::rules::Rule, value: f64, snap: &Snapshot) -> (String, String) {
    use crate::rules::{ProcSub, RMetric};
    let Some(metric) = rule.metric() else {
        return conn_alert_text(rule, "a matching connection appeared");
    };
    // Friendly name + unit + a nicely formatted value for this metric.
    let (name, unit, shown) = match metric {
        RMetric::Cpu => ("CPU".to_string(), "%", format!("{:.0}%", value)),
        RMetric::RamPct => ("RAM".to_string(), "%", format!("{:.0}%", value)),
        RMetric::Gpu => ("GPU".to_string(), "%", format!("{:.0}%", value)),
        RMetric::DiskMbs => ("Disk".to_string(), " MB/s", format!("{:.1} MB/s", value)),
        RMetric::NetMbs => ("Network".to_string(), " MB/s", format!("{:.1} MB/s", value)),
        RMetric::Fps => ("Frame rate".to_string(), " fps", format!("{:.0} fps", value)),
        RMetric::SoundPct => ("Sound".to_string(), "%", format!("{:.0}%", value)),
        RMetric::Proc { name, sub } => {
            let (label, shown) = match sub {
                ProcSub::Cpu => ("CPU", format!("{:.0}%", value)),
                ProcSub::RamMb => ("RAM", crate::util::format_bytes((value * 1048576.0) as u64)),
                ProcSub::DiskMbs => ("disk", format!("{:.1} MB/s", value)),
                ProcSub::NetMbs => ("network", format!("{:.1} MB/s", value)),
                ProcSub::SoundPct => ("sound", format!("{:.0}%", value)),
            };
            (format!("{} {}", name, label), "", shown)
        }
    };
    let (gt, threshold_value) = match &rule.cond {
        crate::rules::Cond::Threshold { gt, threshold, .. } => (*gt, *threshold),
        crate::rules::Cond::Conn { .. } => (true, 0.0),
    };
    let dir = if gt { "above" } else { "below" };
    let threshold = format!("{}{}", (threshold_value as i64), unit);
    let title = format!("Alert: {} {} {}", name, dir, threshold);
    let mut body = format!("{} reached {}.", name, shown);
    if !snap.procs.is_empty() {
        let mut by_cpu: Vec<&ProcStat> = snap.procs.iter().collect();
        by_cpu.sort_by(|a, b| b.cpu_pct.partial_cmp(&a.cpu_pct).unwrap_or(std::cmp::Ordering::Equal));
        if let Some(top) = by_cpu.first() {
            body = format!("{} Top app: {} at {:.0}% CPU.", body, top.name, top.cpu_pct);
        }
    }
    (title, body)
}

/// `detail` is what the rule observed: "value=93.2" for a threshold, or a
/// sentence for a connection rule. Threshold lines keep the exact shape they
/// have always had, so anything parsing existing logs still works.
fn write_rule_log(rule: &crate::rules::Rule, detail: &str, snap: &Snapshot) {
    use std::io::Write;
    if rule.file.trim().is_empty() {
        return; // notify-only rule
    }
    let mut line = format!("[{}] {} (rule: {})", local_timestamp(), detail, rule.raw);
    if rule.top && !snap.procs.is_empty() {
        let mut by_cpu: Vec<&ProcStat> = snap.procs.iter().collect();
        by_cpu.sort_by(|a, b| b.cpu_pct.partial_cmp(&a.cpu_pct).unwrap_or(std::cmp::Ordering::Equal));
        let top: Vec<String> = by_cpu
            .iter()
            .take(5)
            .map(|p| {
                format!(
                    "{} cpu={:.1}% ram={}",
                    p.name,
                    p.cpu_pct,
                    crate::util::format_bytes(p.ws_private)
                )
            })
            .collect();
        line = format!("{} | top: {}", line, top.join(", "));
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(&rule.file) {
        Ok(mut f) => {
            let _ = writeln!(f, "{}", line);
        }
        Err(e) => crate::log(&format!("rule log open failed ({}): {}", rule.file, e)),
    }
}

fn sample_memory(snap: &mut Snapshot) {
    let mut ms = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    if unsafe { GlobalMemoryStatusEx(&mut ms) } != 0 {
        snap.mem_total = ms.ullTotalPhys;
        snap.mem_used = ms.ullTotalPhys - ms.ullAvailPhys;
    }
}

fn sample_drives() -> Vec<DriveInfo> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root: Vec<u16> = format!("{}:\\", letter).encode_utf16().chain([0]).collect();
        if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
            continue;
        }
        let mut free = 0u64;
        let mut total = 0u64;
        if unsafe {
            GetDiskFreeSpaceExW(root.as_ptr(), std::ptr::null_mut(), &mut total, &mut free)
        } != 0
        {
            out.push(DriveInfo { letter, total, free });
        }
    }
    out
}

fn unicode_to_string(u: &UnicodeString) -> String {
    if u.buffer.is_null() || u.length == 0 {
        return "System".to_string();
    }
    let slice = unsafe { std::slice::from_raw_parts(u.buffer, (u.length / 2) as usize) };
    String::from_utf16_lossy(slice)
}

fn query_image_basename(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; MAX_PATH as usize];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().map(|s| s.to_string())
    }
}
