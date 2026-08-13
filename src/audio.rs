//! Per-application audio levels via the Core Audio session API — the same
//! source the Windows volume mixer uses. Needs no elevation.
//!
//! Hand-rolled COM: `windows-sys` exposes no vtable wrappers, so each
//! interface is declared as its function table and called by index. Every
//! call is null- and HRESULT-checked; any failure degrades to "no audio data"
//! rather than propagating.

use std::collections::HashMap;
use std::ffi::c_void;

use windows_sys::core::GUID;
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

// {BCDE0395-E52F-467C-8E3D-C4579291692E}
const CLSID_MM_DEVICE_ENUMERATOR: GUID = GUID {
    data1: 0xBCDE0395,
    data2: 0xE52F,
    data3: 0x467C,
    data4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
};
// {A95664D2-9614-4F35-A746-DE8DB63617E6}
const IID_IMM_DEVICE_ENUMERATOR: GUID = GUID {
    data1: 0xA95664D2,
    data2: 0x9614,
    data3: 0x4F35,
    data4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
};
// {77AA99A0-1BD6-484F-8BC7-2C654C9A9B6F}
const IID_IAUDIO_SESSION_MANAGER2: GUID = GUID {
    data1: 0x77AA99A0,
    data2: 0x1BD6,
    data3: 0x484F,
    data4: [0x8B, 0xC7, 0x2C, 0x65, 0x4C, 0x9A, 0x9B, 0x6F],
};
// {BFB7FF88-7239-4FC9-8FA2-07C950BE9C6D}
const IID_IAUDIO_SESSION_CONTROL2: GUID = GUID {
    data1: 0xBFB7FF88,
    data2: 0x7239,
    data3: 0x4FC9,
    data4: [0x8F, 0xA2, 0x07, 0xC9, 0x50, 0xBE, 0x9C, 0x6D],
};
// {C02216F6-8C67-4B5B-9D00-D008E73E0064}
const IID_IAUDIO_METER_INFORMATION: GUID = GUID {
    data1: 0xC02216F6,
    data2: 0x8C67,
    data3: 0x4B5B,
    data4: [0x9D, 0x00, 0xD0, 0x08, 0xE7, 0x3E, 0x00, 0x64],
};

const E_RENDER: i32 = 0;
const E_MULTIMEDIA: i32 = 1;
const SESSION_ACTIVE: i32 = 1;

type Ptr = *mut c_void;

/// Interface pointer: **vtable -> [fn; ...]
unsafe fn vt(obj: Ptr, index: usize) -> *const c_void {
    let vtable = *(obj as *const *const *const c_void);
    *vtable.add(index)
}

unsafe fn release(obj: Ptr) {
    if obj.is_null() {
        return;
    }
    let f: unsafe extern "system" fn(Ptr) -> u32 = std::mem::transmute(vt(obj, 2));
    f(obj);
}

unsafe fn query_interface(obj: Ptr, iid: &GUID) -> Option<Ptr> {
    if obj.is_null() {
        return None;
    }
    let f: unsafe extern "system" fn(Ptr, *const GUID, *mut Ptr) -> i32 =
        std::mem::transmute(vt(obj, 0));
    let mut out: Ptr = std::ptr::null_mut();
    if f(obj, iid, &mut out) == 0 && !out.is_null() {
        Some(out)
    } else {
        None
    }
}

pub struct AudioSampler {
    manager: Ptr,
}

// The COM objects are only ever touched from the sampler thread.
unsafe impl Send for AudioSampler {}

impl AudioSampler {
    pub fn new() -> Option<Self> {
        unsafe {
            CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32);
            let mut enumerator: Ptr = std::ptr::null_mut();
            let hr = CoCreateInstance(
                &CLSID_MM_DEVICE_ENUMERATOR,
                std::ptr::null_mut(),
                CLSCTX_ALL,
                &IID_IMM_DEVICE_ENUMERATOR,
                &mut enumerator,
            );
            if hr != 0 || enumerator.is_null() {
                crate::log("audio: device enumerator unavailable");
                return None;
            }
            // IMMDeviceEnumerator::GetDefaultAudioEndpoint (vtable slot 4)
            let get_default: unsafe extern "system" fn(Ptr, i32, i32, *mut Ptr) -> i32 =
                std::mem::transmute(vt(enumerator, 4));
            let mut device: Ptr = std::ptr::null_mut();
            let hr = get_default(enumerator, E_RENDER, E_MULTIMEDIA, &mut device);
            release(enumerator);
            if hr != 0 || device.is_null() {
                crate::log("audio: no default playback device");
                return None;
            }
            // IMMDevice::Activate (slot 3)
            let activate: unsafe extern "system" fn(Ptr, *const GUID, u32, Ptr, *mut Ptr) -> i32 =
                std::mem::transmute(vt(device, 3));
            let mut manager: Ptr = std::ptr::null_mut();
            let hr = activate(
                device,
                &IID_IAUDIO_SESSION_MANAGER2,
                CLSCTX_ALL,
                std::ptr::null_mut(),
                &mut manager,
            );
            release(device);
            if hr != 0 || manager.is_null() {
                crate::log("audio: session manager unavailable");
                return None;
            }
            crate::log("audio: session manager ready");
            Some(AudioSampler { manager })
        }
    }

    /// (overall peak 0..1, per-pid peak) across active playback sessions.
    pub fn sample(&mut self) -> (f32, HashMap<u32, f32>) {
        let mut per_pid: HashMap<u32, f32> = HashMap::new();
        let mut overall = 0.0f32;
        unsafe {
            // IAudioSessionManager2::GetSessionEnumerator (slot 5)
            let get_enum: unsafe extern "system" fn(Ptr, *mut Ptr) -> i32 =
                std::mem::transmute(vt(self.manager, 5));
            let mut sessions: Ptr = std::ptr::null_mut();
            if get_enum(self.manager, &mut sessions) != 0 || sessions.is_null() {
                return (0.0, per_pid);
            }
            // IAudioSessionEnumerator::GetCount (3), GetSession (4)
            let get_count: unsafe extern "system" fn(Ptr, *mut i32) -> i32 =
                std::mem::transmute(vt(sessions, 3));
            let get_session: unsafe extern "system" fn(Ptr, i32, *mut Ptr) -> i32 =
                std::mem::transmute(vt(sessions, 4));
            let mut count = 0i32;
            if get_count(sessions, &mut count) != 0 {
                release(sessions);
                return (0.0, per_pid);
            }
            for i in 0..count.clamp(0, 256) {
                let mut ctrl: Ptr = std::ptr::null_mut();
                if get_session(sessions, i, &mut ctrl) != 0 || ctrl.is_null() {
                    continue;
                }
                // IAudioSessionControl::GetState (slot 3)
                let get_state: unsafe extern "system" fn(Ptr, *mut i32) -> i32 =
                    std::mem::transmute(vt(ctrl, 3));
                let mut state = 0i32;
                let active = get_state(ctrl, &mut state) == 0 && state == SESSION_ACTIVE;
                if active {
                    let pid = query_interface(ctrl, &IID_IAUDIO_SESSION_CONTROL2)
                        .and_then(|c2| {
                            // IAudioSessionControl2::GetProcessId (slot 14)
                            let f: unsafe extern "system" fn(Ptr, *mut u32) -> i32 =
                                std::mem::transmute(vt(c2, 14));
                            let mut pid = 0u32;
                            let ok = f(c2, &mut pid) == 0;
                            release(c2);
                            if ok {
                                Some(pid)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    if let Some(meter) = query_interface(ctrl, &IID_IAUDIO_METER_INFORMATION) {
                        // IAudioMeterInformation::GetPeakValue (slot 3)
                        let f: unsafe extern "system" fn(Ptr, *mut f32) -> i32 =
                            std::mem::transmute(vt(meter, 3));
                        let mut peak = 0.0f32;
                        if f(meter, &mut peak) == 0 && peak.is_finite() {
                            let peak = peak.clamp(0.0, 1.0);
                            overall = overall.max(peak);
                            if pid != 0 {
                                let e = per_pid.entry(pid).or_insert(0.0);
                                *e = e.max(peak);
                            }
                        }
                        release(meter);
                    }
                }
                release(ctrl);
            }
            release(sessions);
        }
        (overall, per_pid)
    }
}

impl Drop for AudioSampler {
    fn drop(&mut self) {
        unsafe { release(self.manager) };
    }
}
