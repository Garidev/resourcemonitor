//! Floating FPS overlay: a small always-on-top layered window with a
//! color-keyed transparent background and adjustable opacity. Draggable;
//! visible over borderless/windowed games (exclusive fullscreen bypasses
//! desktop composition and cannot show desktop overlays).

use std::cell::RefCell;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::gdi;

/// (label, COLORREF) presets; indices stored in config.
pub const COLORS: [(&str, u32); 5] = [
    ("white", gdi::rgb(240, 242, 245)),
    ("green", gdi::ACC_RAM),
    ("red", gdi::ACC_FPS),
    ("cyan", gdi::rgb(80, 220, 255)),
    ("yellow", gdi::ACC_NET),
];
/// (label, alpha) presets.
pub const OPACITIES: [(&str, u8); 3] = [("40%", 102), ("70%", 178), ("100%", 255)];

/// Near-black colorkey: everything painted this color becomes transparent.
const KEY: u32 = gdi::rgb(0, 0, 1);

struct State {
    fps: Option<u32>,
    color: u32,
    scale: f32,
    /// Last position the user dragged the overlay to (screen coords).
    moved_to: Option<(i32, i32)>,
    font: HFONT,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State {
        fps: None,
        color: COLORS[0].1,
        scale: 1.0,
        moved_to: None,
        font: std::ptr::null_mut(),
    });
}

pub fn create(x: i32, y: i32, scale: f32) -> HWND {
    unsafe {
        let class = gdi::wide("ResMonFpsOverlay");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(overlay_proc);
        wc.hInstance = GetModuleHandleW(std::ptr::null());
        wc.lpszClassName = class.as_ptr();
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_SIZEALL);
        RegisterClassW(&wc);

        let (w, h) = ((96.0 * scale) as i32, (44.0 * scale) as i32);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            gdi::wide("FPS").as_ptr(),
            WS_POPUP,
            if x >= 0 { x } else { (40.0 * scale) as i32 },
            if y >= 0 { y } else { (40.0 * scale) as i32 },
            w,
            h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            wc.hInstance,
            std::ptr::null(),
        );
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.scale = scale;
            if s.font.is_null() {
                s.font = gdi::make_font((30.0 * scale) as i32, 800);
            }
        });
        hwnd
    }
}

/// Push new data/appearance to the overlay; returns the position if the user
/// dragged it since the last call (so the caller can persist it).
pub fn update(hwnd: HWND, fps: Option<u32>, color_idx: u32, opacity_idx: u32) -> Option<(i32, i32)> {
    let color = COLORS.get(color_idx as usize).map(|c| c.1).unwrap_or(COLORS[0].1);
    let alpha = OPACITIES.get(opacity_idx as usize).map(|o| o.1).unwrap_or(178);
    let moved = STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.fps = fps;
        s.color = color;
        s.moved_to.take()
    });
    unsafe {
        SetLayeredWindowAttributes(hwnd, KEY, alpha, LWA_ALPHA | LWA_COLORKEY);
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }
    moved
}

pub fn show(hwnd: HWND, visible: bool) {
    unsafe {
        ShowWindow(hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
    }
}

unsafe extern "system" fn overlay_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            STATE.with(|s| {
                let s = s.borrow();
                gdi::fill(hdc, &rc, KEY);
                let label = s.fps.map(|f| f.to_string()).unwrap_or_else(|| "—".into());
                // Shadow for readability over bright scenes, then the number.
                let x = (6.0 * s.scale) as i32;
                let y = (4.0 * s.scale) as i32;
                gdi::text(hdc, x + 2, y + 2, s.font, gdi::rgb(10, 10, 12), &label);
                gdi::text(hdc, x, y, s.font, s.color, &label);
            });
            EndPaint(hwnd, &ps);
            0
        }
        WM_NCHITTEST => {
            // Whole window acts as a caption so it can be dragged anywhere.
            HTCAPTION as LRESULT
        }
        WM_EXITSIZEMOVE | WM_MOVE => {
            let mut r: RECT = std::mem::zeroed();
            GetWindowRect(hwnd, &mut r);
            STATE.with(|s| s.borrow_mut().moved_to = Some((r.left, r.top)));
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
