//! Configurable set of live tray icons, drawn into 32-bit DIBs on change.

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_LARGE_ICON, NIIF_USER,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, GetSystemMetrics, LoadImageW, HICON, ICONINFO, IMAGE_ICON,
    LR_DEFAULTSIZE, LR_SHARED, SM_CXSMICON,
};

use super::gdi::{self, rgb};
use crate::config::Settings;
use crate::sampler::Snapshot;
use crate::util::compact_rate;

pub const WM_APP_TRAY: u32 = 0x8001; // WM_APP + 1

const KINDS: usize = 6;
const K_STATIC: usize = 0;
const K_CPU: usize = 1;
const K_RAM: usize = 2;
const K_DISK: usize = 3;
const K_NET: usize = 4;
const K_FPS: usize = 5;

fn accent(kind: usize) -> u32 {
    match kind {
        K_CPU => gdi::acc().cpu,
        K_RAM => gdi::acc().ram,
        K_DISK => gdi::acc().disk,
        K_NET => gdi::acc().net,
        K_FPS => gdi::acc().fps,
        _ => rgb(90, 130, 200),
    }
}

pub struct Tray {
    hwnd: HWND,
    present: [bool; KINDS],
    last_text: [String; KINDS],
    /// Shared (never destroyed) app icons: default size for balloons, small
    /// size for the tray itself.
    app_icon: HICON,
    app_icon_small: HICON,
}

/// The embedded application icon (resource id 1). `size` 0 = default size.
fn load_app_icon(size: i32) -> HICON {
    unsafe {
        let flags = if size == 0 { LR_DEFAULTSIZE | LR_SHARED } else { LR_SHARED };
        LoadImageW(
            GetModuleHandleW(std::ptr::null()),
            1 as *const u16,
            IMAGE_ICON,
            size,
            size,
            flags,
        ) as HICON
    }
}

fn base_nid(hwnd: HWND, id: u32) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = id;
    nid
}

fn set_tip(nid: &mut NOTIFYICONDATAW, tip: &str) {
    let w: Vec<u16> = tip.encode_utf16().take(127).collect();
    nid.szTip[..w.len()].copy_from_slice(&w);
    nid.szTip[w.len()] = 0;
}

impl Tray {
    pub fn new(hwnd: HWND, cfg: &Settings) -> Self {
        let mut t = Tray {
            hwnd,
            present: [false; KINDS],
            last_text: Default::default(),
            app_icon: load_app_icon(0),
            app_icon_small: load_app_icon(unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16)),
        };
        t.sync(cfg);
        t
    }

    fn wanted(cfg: &Settings) -> [bool; KINDS] {
        let mut w = [
            cfg.tray_static,
            cfg.tray_cpu,
            cfg.tray_ram,
            cfg.tray_disk,
            cfg.tray_net,
            cfg.tray_fps,
        ];
        if !w.iter().any(|&b| b) {
            w[K_STATIC] = true; // always keep the app reachable
        }
        w
    }

    /// Add/remove icons to match the settings.
    pub fn sync(&mut self, cfg: &Settings) {
        let want = Self::wanted(cfg);
        for kind in 0..KINDS {
            if want[kind] == self.present[kind] {
                continue;
            }
            let mut nid = base_nid(self.hwnd, kind as u32 + 1);
            if want[kind] {
                let shared = kind == K_STATIC;
                let icon = if shared { self.app_icon_small } else { draw_icon("…", None, accent(kind)) };
                nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
                nid.uCallbackMessage = WM_APP_TRAY;
                nid.hIcon = icon;
                set_tip(&mut nid, "Resource Monitor");
                unsafe {
                    Shell_NotifyIconW(NIM_ADD, &mut nid);
                    // Opt into version 3 behaviour. Without this the icon stays
                    // at version 0, where the shell never sends the NIN_ balloon
                    // messages — so `balloon_click` could not fire and clicking
                    // a notification did nothing. The handler was wired at both
                    // ends and dead in the middle.
                    //
                    // Version 3 and not 4 on purpose: version 4 repacks the
                    // callback's wParam and lParam (coordinates in wParam, event
                    // in the low word of lParam), and `WM_APP_TRAY` reads the
                    // event straight out of lParam. Asking for 4 would silently
                    // break every tray click to fix the balloon click.
                    nid.Anonymous.uVersion = NOTIFYICON_VERSION;
                    Shell_NotifyIconW(NIM_SETVERSION, &mut nid);
                    if !shared {
                        DestroyIcon(icon);
                    }
                }
            } else {
                unsafe {
                    Shell_NotifyIconW(NIM_DELETE, &mut nid);
                }
            }
            self.present[kind] = want[kind];
            self.last_text[kind].clear();
        }
    }

    /// Re-add all present icons (after an explorer restart).
    pub fn readd(&mut self) {
        let present = self.present;
        for (kind, &p) in present.iter().enumerate() {
            if p {
                let mut nid = base_nid(self.hwnd, kind as u32 + 1);
                let shared = kind == K_STATIC;
                let icon = if shared { self.app_icon_small } else { draw_icon("…", None, accent(kind)) };
                nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
                nid.uCallbackMessage = WM_APP_TRAY;
                nid.hIcon = icon;
                set_tip(&mut nid, "Resource Monitor");
                unsafe {
                    Shell_NotifyIconW(NIM_ADD, &mut nid);
                    // Opt into version 3 behaviour. Without this the icon stays
                    // at version 0, where the shell never sends the NIN_ balloon
                    // messages — so `balloon_click` could not fire and clicking
                    // a notification did nothing. The handler was wired at both
                    // ends and dead in the middle.
                    //
                    // Version 3 and not 4 on purpose: version 4 repacks the
                    // callback's wParam and lParam (coordinates in wParam, event
                    // in the low word of lParam), and `WM_APP_TRAY` reads the
                    // event straight out of lParam. Asking for 4 would silently
                    // break every tray click to fix the balloon click.
                    nid.Anonymous.uVersion = NOTIFYICON_VERSION;
                    Shell_NotifyIconW(NIM_SETVERSION, &mut nid);
                    if !shared {
                        DestroyIcon(icon);
                    }
                }
                self.last_text[kind].clear();
            }
        }
    }

    pub fn update(&mut self, snap: &Snapshot, mem_pct: f32, tooltip: &str) {
        for kind in 0..KINDS {
            if !self.present[kind] {
                continue;
            }
            // (text, bar fill 0..=100)
            if kind == K_STATIC {
                // Fixed app icon; only the tooltip changes.
                let mut nid = base_nid(self.hwnd, kind as u32 + 1);
                nid.uFlags = NIF_TIP;
                set_tip(&mut nid, tooltip);
                unsafe {
                    Shell_NotifyIconW(NIM_MODIFY, &mut nid);
                }
                continue;
            }
            let (text, fill): (String, Option<i32>) = match kind {
                K_CPU => (format!("{}", (snap.cpu_pct.round() as i32).clamp(0, 99)), Some(snap.cpu_pct as i32)),
                K_RAM => (format!("{}", (mem_pct.round() as i32).clamp(0, 99)), Some(mem_pct as i32)),
                K_DISK => (compact_rate(snap.disk_read_bps + snap.disk_write_bps), None),
                K_NET => (compact_rate(snap.net_rx_bps + snap.net_tx_bps), None),
                K_FPS => (
                    snap.fps.as_ref().map(|(_, _, f)| f.to_string()).unwrap_or_else(|| "-".into()),
                    None,
                ),
                _ => continue,
            };
            let mut nid = base_nid(self.hwnd, kind as u32 + 1);
            nid.uFlags = NIF_TIP;
            set_tip(&mut nid, tooltip);
            if text != self.last_text[kind] {
                self.last_text[kind] = text.clone();
                nid.uFlags |= NIF_ICON;
                nid.hIcon = draw_icon(&text, fill, accent(kind));
            }
            unsafe {
                Shell_NotifyIconW(NIM_MODIFY, &mut nid);
                if !nid.hIcon.is_null() {
                    DestroyIcon(nid.hIcon);
                }
            }
        }
    }

    /// Native balloon notification anchored to the first tray icon.
    pub fn balloon(&mut self, title: &str, message: &str) {
        let Some(kind) = self.present.iter().position(|&p| p) else { return };
        let mut nid = base_nid(self.hwnd, kind as u32 + 1);
        nid.uFlags = NIF_INFO;
        // Show the app icon in the notification instead of the generic
        // system info glyph.
        nid.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON;
        nid.hBalloonIcon = self.app_icon;
        let t: Vec<u16> = title.encode_utf16().take(63).collect();
        nid.szInfoTitle[..t.len()].copy_from_slice(&t);
        nid.szInfoTitle[t.len()] = 0;
        let m: Vec<u16> = message.encode_utf16().take(255).collect();
        nid.szInfo[..m.len()].copy_from_slice(&m);
        nid.szInfo[m.len()] = 0;
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &mut nid);
        }
    }

    pub fn remove(&mut self) {
        for kind in 0..KINDS {
            if self.present[kind] {
                let mut nid = base_nid(self.hwnd, kind as u32 + 1);
                unsafe {
                    Shell_NotifyIconW(NIM_DELETE, &mut nid);
                }
                self.present[kind] = false;
            }
        }
    }
}

/// Small square icon: dark tile, optional accent fill bar from the bottom,
/// text on top sized to fit.
fn draw_icon(text: &str, fill_pct: Option<i32>, accent: u32) -> HICON {
    unsafe {
        let sz = GetSystemMetrics(SM_CXSMICON).max(16);
        let dc = CreateCompatibleDC(std::ptr::null_mut());

        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = sz;
        bi.bmiHeader.biHeight = -sz; // top-down
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        let old = SelectObject(dc, dib as HGDIOBJ);

        let full = windows_sys::Win32::Foundation::RECT { left: 0, top: 0, right: sz, bottom: sz };
        gdi::fill(dc, &full, rgb(20, 22, 26));
        if let Some(pct) = fill_pct {
            let fill_h = (sz * pct.clamp(0, 100)) / 100;
            let bar = windows_sys::Win32::Foundation::RECT {
                left: 0,
                top: sz - fill_h,
                right: sz,
                bottom: sz,
            };
            let dim = rgb(
                (accent & 0xFF) as u8 / 2,
                ((accent >> 8) & 0xFF) as u8 / 2,
                ((accent >> 16) & 0xFF) as u8 / 2,
            );
            gdi::fill(dc, &bar, dim);
        }

        // Shrink font with text length so up to 4 chars fit.
        let fh = match text.chars().count() {
            0..=2 => sz * 11 / 16,
            3 => sz * 8 / 16,
            _ => sz * 6 / 16,
        };
        let font = gdi::make_font(fh.max(6), 700);
        let tw = gdi::text_width(dc, font, text);
        let color = if fill_pct.is_some() { rgb(240, 242, 245) } else { accent };
        gdi::text(dc, ((sz - tw) / 2).max(0), (sz - fh) / 2 - 1, font, color, text);
        DeleteObject(font as HGDIOBJ);

        // GDI text/fill leave alpha = 0; force the icon fully opaque.
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (sz * sz) as usize);
        for p in px.iter_mut() {
            *p |= 0xFF00_0000;
        }

        SelectObject(dc, old);
        let mask = CreateBitmap(sz, sz, 1, 1, std::ptr::null());
        let ii = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: dib,
        };
        let icon = CreateIconIndirect(&ii);
        DeleteObject(dib as HGDIOBJ);
        DeleteObject(mask as HGDIOBJ);
        DeleteDC(dc);
        icon
    }
}
