//! GPU utilization via PDH "GPU Engine" counters — the same per-engine data
//! Task Manager's GPU graphs use. Instance names look like
//! `pid_1234_luid_0x..._phys_0_engtype_3D`, which also gives per-process GPU.

use std::collections::HashMap;

use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_MORE_DATA,
};

const COUNTER_PATH: &str = "\\GPU Engine(*)\\Utilization Percentage";

pub struct GpuSampler {
    query: isize,
    counter: isize,
    buf: Vec<u8>,
}

impl GpuSampler {
    pub fn new() -> Option<Self> {
        let mut query: isize = 0;
        if unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) } != 0 {
            return None;
        }
        let path: Vec<u16> = COUNTER_PATH.encode_utf16().chain([0]).collect();
        let mut counter: isize = 0;
        if unsafe { PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) } != 0 {
            unsafe { PdhCloseQuery(query) };
            return None;
        }
        // Prime: rate counters need two collections before producing values.
        unsafe { PdhCollectQueryData(query) };
        Some(GpuSampler { query, counter, buf: Vec::new() })
    }

    /// Returns (overall GPU %, per-pid GPU %). Overall follows Task Manager:
    /// the busiest engine type across all processes.
    pub fn sample(&mut self) -> (f32, HashMap<u32, f32>) {
        let mut per_pid: HashMap<u32, f32> = HashMap::new();
        let mut per_type: HashMap<String, f64> = HashMap::new();
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return (0.0, per_pid);
            }
            let mut size = self.buf.len() as u32;
            let mut count = 0u32;
            let mut rc = PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                self.buf.as_mut_ptr() as _,
            );
            if rc == PDH_MORE_DATA {
                self.buf.resize(size as usize, 0);
                rc = PdhGetFormattedCounterArrayW(
                    self.counter,
                    PDH_FMT_DOUBLE,
                    &mut size,
                    &mut count,
                    self.buf.as_mut_ptr() as _,
                );
            }
            if rc != 0 {
                return (0.0, per_pid);
            }
            let items = std::slice::from_raw_parts(
                self.buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W,
                count as usize,
            );
            for item in items {
                let val = item.FmtValue.Anonymous.doubleValue;
                if val <= 0.0 || item.szName.is_null() {
                    continue;
                }
                let name = pwstr_to_string(item.szName);
                if let Some(pid) = parse_field(&name, "pid_") {
                    let e = per_pid.entry(pid).or_insert(0.0);
                    *e = (*e + val as f32).min(100.0);
                }
                let engtype = name.rsplit("engtype_").next().unwrap_or("?").to_string();
                *per_type.entry(engtype).or_insert(0.0) += val;
            }
        }
        let overall = per_type.values().cloned().fold(0.0f64, f64::max).min(100.0) as f32;
        (overall, per_pid)
    }
}

impl Drop for GpuSampler {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

unsafe fn pwstr_to_string(p: *const u16) -> String {
    let mut len = 0;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
}

fn parse_field(name: &str, prefix: &str) -> Option<u32> {
    let rest = &name[name.find(prefix)? + prefix.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}
