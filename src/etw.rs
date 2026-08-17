//! One real-time ETW session feeding three features:
//!  - Microsoft-Windows-DXGI Present events  -> per-process presents/sec (FPS)
//!  - Microsoft-Windows-Kernel-Network       -> per-process sent/recv bytes
//!  - Microsoft-Windows-DNS-Client           -> address -> hostname, with the
//!    pid that asked (the connection views join on this)
//!
//! Requires elevation (or membership in Performance Log Users). On failure we
//! set `ok = false` and the UI degrades gracefully.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows_sys::core::GUID;
use windows_sys::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, StartTraceW,
    CONTROLTRACE_HANDLE, EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_RECORD, EVENT_TRACE_CONTROL_STOP,
    EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE,
    PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME, WNODE_FLAG_TRACED_GUID,
};

const SESSION_NAME: &str = "ResMonTrace";
const ERROR_ALREADY_EXISTS: u32 = 183;
const TRACE_LEVEL_VERBOSE: u8 = 5;
const TRACE_LEVEL_INFORMATION: u8 = 4;

// Microsoft-Windows-DXGI {CA11C036-0102-4A2D-A6AD-F03CFED5D3C9}
const DXGI_PROVIDER: GUID = GUID {
    data1: 0xCA11C036,
    data2: 0x0102,
    data3: 0x4A2D,
    data4: [0xA6, 0xAD, 0xF0, 0x3C, 0xFE, 0xD5, 0xD3, 0xC9],
};
// DXGI manifest: IDXGISwapChain::Present start
const DXGI_EVENT_PRESENT_START: u16 = 42;

// Microsoft-Windows-Kernel-Network {7DD42A49-5329-4832-8DFD-43D979153A88}
const KNET_PROVIDER: GUID = GUID {
    data1: 0x7DD42A49,
    data2: 0x5329,
    data3: 0x4832,
    data4: [0x8D, 0xFD, 0x43, 0xD9, 0x79, 0x15, 0x3A, 0x88],
};
// Kernel-Network manifest event ids: data sent/received for TCPv4/v6, UDPv4/v6.
const KNET_SEND: [u16; 3] = [10, 26, 42];
const KNET_RECV: [u16; 3] = [11, 27, 43];
const KNET_SEND6: u16 = 58;
const KNET_RECV6: u16 = 59;

// Microsoft-Windows-DNS-Client {1C95126E-7EEA-49A9-A3FE-A378B03DDB4D}.
// Emitted inside the process that called the resolver, so the event header
// carries the pid that wanted the name — the attribution a machine-wide DNS
// cache dump cannot give.
const DNS_PROVIDER: GUID = GUID {
    data1: 0x1C95126E,
    data2: 0x7EEA,
    data3: 0x49A9,
    data4: [0xA3, 0xFE, 0xA3, 0x78, 0xB0, 0x3D, 0xDB, 0x4D],
};
// DNS_QUERY_COMPLETED: name, type, options, status, results.
const DNS_EVENT_QUERY_COMPLETED: u16 = 3008;

/// One observed name lookup: what was asked for, what it resolved to, and
/// which process asked. Connection alert rules read these.
#[derive(Clone, Debug)]
pub struct DnsEvent {
    pub pid: u32,
    pub host: String,
    pub addrs: Vec<std::net::IpAddr>,
}

/// Lookups are kept only until the next sampler tick reads them. The cap is
/// a backstop for the case where nothing is draining — a burst of lookups
/// must not grow this without bound.
const DNS_EVENT_CAP: usize = 256;

pub struct EtwShared {
    pub ok: AtomicBool,
    /// True only when the Kernel-Network provider actually enabled — the
    /// per-app network hint shows only when this is false.
    pub knet_ok: AtomicBool,
    /// True when the DNS-Client provider enabled. False means connections can
    /// still be listed, but only reverse lookups can name them.
    pub dns_ok: AtomicBool,
    /// pid -> Present-event count since last drain.
    pub presents: Mutex<HashMap<u32, u32>>,
    /// pid -> (sent bytes, received bytes) since last drain.
    pub net: Mutex<HashMap<u32, (u64, u64)>>,
    /// Name lookups seen since the last drain, oldest first.
    pub dns_events: Mutex<Vec<DnsEvent>>,
}

impl EtwShared {
    pub fn new() -> Self {
        EtwShared {
            ok: AtomicBool::new(false),
            knet_ok: AtomicBool::new(false),
            dns_ok: AtomicBool::new(false),
            presents: Mutex::new(HashMap::new()),
            net: Mutex::new(HashMap::new()),
            dns_events: Mutex::new(Vec::new()),
        }
    }

    /// Take everything seen since the last call. Callers drain every tick
    /// whether or not they have a use for the events, so the queue never
    /// accumulates.
    pub fn take_dns_events(&self) -> Vec<DnsEvent> {
        std::mem::take(&mut *self.dns_events.lock().unwrap())
    }
}

static ETW_STATE: OnceLock<Arc<crate::sampler::Shared>> = OnceLock::new();

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

#[repr(C)]
struct TraceProps {
    props: EVENT_TRACE_PROPERTIES,
    logger_name: [u16; 128],
}

fn new_props() -> TraceProps {
    let mut tp: TraceProps = unsafe { std::mem::zeroed() };
    tp.props.Wnode.BufferSize = std::mem::size_of::<TraceProps>() as u32;
    tp.props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
    tp.props.Wnode.ClientContext = 1; // QPC timestamps
    tp.props.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
    tp.props.LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
    tp
}

unsafe extern "system" fn on_event(rec: *mut EVENT_RECORD) {
    let Some(shared) = ETW_STATE.get() else { return };
    let r = &*rec;
    let provider = &r.EventHeader.ProviderId;
    let id = r.EventHeader.EventDescriptor.Id;

    if guid_eq(provider, &DXGI_PROVIDER) {
        if id == DXGI_EVENT_PRESENT_START {
            let pid = r.EventHeader.ProcessId;
            *shared.etw.presents.lock().unwrap().entry(pid).or_insert(0) += 1;
        }
    } else if guid_eq(provider, &KNET_PROVIDER) {
        let sent = KNET_SEND.contains(&id) || id == KNET_SEND6;
        let recv = KNET_RECV.contains(&id) || id == KNET_RECV6;
        if !sent && !recv {
            return;
        }
        // Payload starts with: PID (u32), size (u32).
        if (r.UserDataLength as usize) < 8 || r.UserData.is_null() {
            return;
        }
        let p = r.UserData as *const u32;
        let pid = p.read_unaligned();
        let size = p.add(1).read_unaligned() as u64;
        let mut net = shared.etw.net.lock().unwrap();
        let e = net.entry(pid).or_insert((0, 0));
        if sent {
            e.0 += size;
        } else {
            e.1 += size;
        }
    } else if guid_eq(provider, &DNS_PROVIDER) {
        if id != DNS_EVENT_QUERY_COMPLETED || r.UserData.is_null() {
            return;
        }
        let payload = std::slice::from_raw_parts(r.UserData as *const u8, r.UserDataLength as usize);
        // A payload we cannot decode costs one dropped name, never a crash:
        // every read in the parser is bounds-checked.
        let Some((host, ips)) = crate::conns::parse_dns_query_event(payload) else { return };
        let pid = r.EventHeader.ProcessId;
        let now = crate::sampler::unix_ms();
        {
            let mut names = shared.names.lock().unwrap();
            for ip in &ips {
                names.insert(*ip, &host, crate::conns::NameSource::DnsEvent, Some(pid), now);
            }
        }
        let mut queue = shared.etw.dns_events.lock().unwrap();
        if queue.len() < DNS_EVENT_CAP {
            queue.push(DnsEvent { pid, host, addrs: ips });
        }
    }
}

fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

/// Kernel-mode providers (Microsoft-Windows-Kernel-Network) refuse to enable
/// unless SeSystemProfilePrivilege is actively ENABLED in the token —
/// elevation only makes it available, still disabled. Same trick PresentMon
/// uses.
fn enable_profile_privilege() {
    use windows_sys::Win32::Foundation::LUID;
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES,
        SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut token) == 0 {
            return;
        }
        let mut luid = LUID { LowPart: 0, HighPart: 0 };
        let name = wide("SeSystemProfilePrivilege");
        if LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) != 0 {
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            AdjustTokenPrivileges(token, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut());
        }
        windows_sys::Win32::Foundation::CloseHandle(token);
    }
}

/// Runs the ETW session on the current thread; blocks in ProcessTrace until
/// the session is externally stopped. Call from a dedicated thread.
pub fn run(shared: Arc<crate::sampler::Shared>) {
    let _ = ETW_STATE.set(shared.clone());
    enable_profile_privilege();
    let name = wide(SESSION_NAME);

    unsafe {
        let mut session = CONTROLTRACE_HANDLE { Value: 0 };
        let mut tp = new_props();
        let mut rc = StartTraceW(&mut session, name.as_ptr(), &mut tp.props);
        if rc == ERROR_ALREADY_EXISTS {
            // Stale session from a previous run — stop it and retry.
            let mut stop = new_props();
            ControlTraceW(
                CONTROLTRACE_HANDLE { Value: 0 },
                name.as_ptr(),
                &mut stop.props,
                EVENT_TRACE_CONTROL_STOP,
            );
            tp = new_props();
            rc = StartTraceW(&mut session, name.as_ptr(), &mut tp.props);
        }
        if rc != 0 {
            crate::log(&format!("ETW StartTrace failed: {} (not elevated?)", rc));
            return;
        }

        let mut enabled = 0;
        // DNS-Client is enabled at Information rather than Verbose: query
        // completions are Information-level, and Verbose would additionally
        // subscribe this session to the resolver's debug traffic for nothing.
        for (provider, pname, level) in [
            (&DXGI_PROVIDER, "DXGI", TRACE_LEVEL_VERBOSE),
            (&KNET_PROVIDER, "Kernel-Network", TRACE_LEVEL_VERBOSE),
            (&DNS_PROVIDER, "DNS-Client", TRACE_LEVEL_INFORMATION),
        ] {
            let rc = EnableTraceEx2(
                session,
                provider,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                level,
                u64::MAX, // match any keyword; keyword-less events always pass
                0,
                0,
                std::ptr::null(),
            );
            if rc != 0 {
                crate::log(&format!("ETW enable {} failed: {}", pname, rc));
            } else {
                enabled += 1;
                if pname == "Kernel-Network" {
                    shared.etw.knet_ok.store(true, Ordering::Relaxed);
                }
                if pname == "DNS-Client" {
                    shared.etw.dns_ok.store(true, Ordering::Relaxed);
                }
            }
        }
        if enabled == 0 {
            // Session exists but no provider could be enabled (not elevated):
            // tear it down so the UI reports "needs administrator".
            let mut stop = new_props();
            ControlTraceW(
                CONTROLTRACE_HANDLE { Value: 0 },
                name.as_ptr(),
                &mut stop.props,
                EVENT_TRACE_CONTROL_STOP,
            );
            return;
        }

        let mut logfile: EVENT_TRACE_LOGFILEW = std::mem::zeroed();
        logfile.LoggerName = name.as_ptr() as *mut u16;
        logfile.Anonymous1.ProcessTraceMode =
            PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        logfile.Anonymous2.EventRecordCallback = Some(on_event);

        let trace = OpenTraceW(&mut logfile);
        if trace.Value == u64::MAX {
            crate::log("ETW OpenTrace failed");
            let mut stop = new_props();
            ControlTraceW(
                CONTROLTRACE_HANDLE { Value: 0 },
                name.as_ptr(),
                &mut stop.props,
                EVENT_TRACE_CONTROL_STOP,
            );
            return;
        }

        shared.etw.ok.store(true, Ordering::Relaxed);
        ProcessTrace(&trace, 1, std::ptr::null(), std::ptr::null());
        // Session ended (stop requested or error).
        shared.etw.ok.store(false, Ordering::Relaxed);
        CloseTrace(trace);
    }
}

/// Stop the session so ProcessTrace unblocks; used on app exit.
pub fn stop() {
    let name = wide(SESSION_NAME);
    let mut stop = new_props();
    unsafe {
        ControlTraceW(
            CONTROLTRACE_HANDLE { Value: 0 },
            name.as_ptr(),
            &mut stop.props,
            EVENT_TRACE_CONTROL_STOP,
        );
    }
}
