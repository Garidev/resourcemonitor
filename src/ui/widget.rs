//! Taskbar widget: an always-visible horizontal strip of live metrics that
//! can float anywhere or snap next to the tray clock. Layered window with
//! slight translucency; draggable; its own theme setting rather than the main
//! panel's — it sits on the taskbar, so it does not have to match the panel.

use std::cell::RefCell;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::gdi;
use crate::util::compact_rate;

pub const M_CPU: u32 = 1;
pub const M_RAM: u32 = 2;
pub const M_NET: u32 = 4;
pub const M_DISK: u32 = 8;
pub const M_FPS: u32 = 16;
pub const M_GPU: u32 = 32;
pub const M_AI: u32 = 64;

#[derive(Clone, Copy, Default)]
pub struct WidgetData {
    pub cpu: f32,
    pub ram: f32,
    pub gpu: f32,
    pub gpu_ok: bool,
    pub rx: u64,
    pub tx: u64,
    pub dr: u64,
    pub dw: u64,
    pub fps: Option<u32>,
    /// Agents reported running right now, across every connected session.
    pub agents: u32,
    /// Messages waiting in the panel that the user has not opened yet.
    pub msgs: u32,
    pub mask: u32,
    /// Index into `gdi::THEMES`.
    pub theme: usize,
}

struct State {
    data: WidgetData,
    /// Everything is drawn in units of this: `dpi * widget_scale`.
    scale: f32,
    /// Kept separately so a dragged width can have the display's scaling
    /// divided back out before the user's own size is stored.
    dpi: f32,
    moved_to: Option<(i32, i32)>,
    /// A size the user just finished dragging, with DPI removed.
    resized_to: Option<f32>,
    label_font: HFONT,
    value_font: HFONT,
    /// Whether the pointer is currently over the strip, so the resize grip
    /// only paints when it's actually reachable.
    hover: bool,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State {
        data: WidgetData::default(),
        scale: 1.0,
        dpi: 1.0,
        moved_to: None,
        resized_to: None,
        label_font: std::ptr::null_mut(),
        value_font: std::ptr::null_mut(),
        hover: false,
    });
}

/// What the user did to the widget since the last tick, for the panel to save.
#[derive(Default)]
pub struct Changes {
    pub moved_to: Option<(i32, i32)>,
    pub resized_to: Option<f32>,
}

/// Height of the strip at a given scale. The one place that ratio lives.
fn strip_height(scale: f32) -> i32 {
    (26.0 * scale) as i32
}

/// Point the drawing state at a new scale, rebuilding the two fonts to match.
/// Cheap to call repeatedly: a drag only changes the quantised scale a few
/// dozen times, and an unchanged scale returns without touching GDI.
fn apply_scale(scale: f32) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if (s.scale - scale).abs() < f32::EPSILON && !s.label_font.is_null() {
            return;
        }
        s.scale = scale;
        // Sized once per scale, so the old pair has to be released rather than
        // leaked — this used to be create-once, which is why changing the text
        // size never resized the widget.
        for f in [s.label_font, s.value_font] {
            if !f.is_null() {
                unsafe { DeleteObject(f as HGDIOBJ) };
            }
        }
        s.label_font = gdi::make_font((9.0 * scale) as i32, 700);
        s.value_font = gdi::make_font((12.0 * scale) as i32, 700);
    });
}

/// Resize the widget to a scale chosen outside a drag: the settings reset, or a
/// move to a display with different scaling.
pub fn set_scale(hwnd: HWND, dpi: f32, widget_scale: f32) {
    if hwnd.is_null() {
        return;
    }
    let scale = dpi * widget_scale;
    STATE.with(|s| s.borrow_mut().dpi = dpi);
    apply_scale(scale);
    let mask = STATE.with(|s| s.borrow().data.mask);
    unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            total_width(mask, scale),
            strip_height(scale),
            SWP_NOMOVE | SWP_NOZORDER,
        );
        InvalidateRect(hwnd, std::ptr::null(), 1);
    }
}

fn segments(mask: u32) -> Vec<u32> {
    [M_CPU, M_RAM, M_GPU, M_FPS, M_DISK, M_NET, M_AI]
        .into_iter()
        .filter(|m| mask & m != 0)
        .collect()
}

fn seg_width(m: u32, scale: f32) -> i32 {
    let base = match m {
        M_NET | M_DISK => 96.0,
        M_AI => 68.0,
        _ => 58.0,
    };
    (base * scale) as i32
}

fn total_width(mask: u32, scale: f32) -> i32 {
    let segs = segments(mask);
    let pad = (8.0 * scale) as i32;
    2 * pad + segs.iter().map(|&m| seg_width(m, scale)).sum::<i32>()
}

/// Side length of the resize grip's hit area and its painted triangle, in
/// sync so the drawn indicator always matches where dragging actually works.
fn grip_size(scale: f32) -> i32 {
    ((10.0 * scale) as i32).max(6)
}

/// `dpi` is the display's scaling and `widget_scale` the user's own size for
/// the strip, kept apart so a drag can store one without baking in the other.
pub fn create(x: i32, y: i32, dpi: f32, widget_scale: f32) -> HWND {
    let scale = dpi * widget_scale;
    unsafe {
        let class = gdi::wide("ResMonWidget");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(widget_proc);
        wc.hInstance = GetModuleHandleW(std::ptr::null());
        wc.lpszClassName = class.as_ptr();
        // Real cursor choice happens in WM_SETCURSOR; this is only the
        // fallback Windows would use if that were ever skipped.
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            gdi::wide("Resource Monitor widget").as_ptr(),
            // WS_THICKFRAME is what lets Windows run the resize loop when the
            // corner grip reports HTBOTTOMRIGHT. The frame it would normally
            // draw is taken back in WM_NCCALCSIZE, so the strip stays
            // borderless.
            WS_POPUP | WS_THICKFRAME,
            if x >= 0 { x } else { (200.0 * scale) as i32 },
            if y >= 0 { y } else { (200.0 * scale) as i32 },
            (200.0 * scale) as i32,
            strip_height(scale),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            wc.hInstance,
            std::ptr::null(),
        );
        SetLayeredWindowAttributes(hwnd, 0, 235, LWA_ALPHA);
        STATE.with(|s| s.borrow_mut().dpi = dpi);
        apply_scale(scale);
        hwnd
    }
}

/// Push fresh data; reports back anything the user did to the widget by hand.
pub fn update(hwnd: HWND, data: WidgetData) -> Changes {
    let (changes, scale) = STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.data = data;
        (
            Changes { moved_to: s.moved_to.take(), resized_to: s.resized_to.take() },
            s.scale,
        )
    });
    unsafe {
        let w = total_width(data.mask, scale);
        let h = strip_height(scale);
        let mut r: RECT = std::mem::zeroed();
        GetWindowRect(hwnd, &mut r);
        // Width follows whichever metrics are on, so toggling one corrects the
        // strip here rather than needing its own resize path.
        if r.right - r.left != w || r.bottom - r.top != h {
            SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER);
        }
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }
    changes
}

pub fn show(hwnd: HWND, visible: bool) {
    unsafe {
        ShowWindow(hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
    }
}

/// Position just left of the tray notification area, vertically centered in
/// the taskbar. Returns None if the taskbar can't be located.
pub fn snap_position(hwnd: HWND, mask: u32) -> Option<(i32, i32)> {
    unsafe {
        let taskbar = FindWindowW(gdi::wide("Shell_TrayWnd").as_ptr(), std::ptr::null());
        if taskbar.is_null() {
            return None;
        }
        let mut tb: RECT = std::mem::zeroed();
        GetWindowRect(taskbar, &mut tb);
        let notify = FindWindowExW(
            taskbar,
            std::ptr::null_mut(),
            gdi::wide("TrayNotifyWnd").as_ptr(),
            std::ptr::null(),
        );
        let right_edge = if notify.is_null() {
            tb.right - 200
        } else {
            let mut nr: RECT = std::mem::zeroed();
            GetWindowRect(notify, &mut nr);
            nr.left
        };
        let scale = STATE.with(|s| s.borrow().scale);
        let w = total_width(mask, scale);
        let mut wr: RECT = std::mem::zeroed();
        GetWindowRect(hwnd, &mut wr);
        let h = wr.bottom - wr.top;
        Some((right_edge - w - (8.0 * scale) as i32, tb.top + (tb.bottom - tb.top - h) / 2))
    }
}

unsafe extern "system" fn widget_proc(
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
                let d = &s.data;
                let sc = s.scale;
                let th = &gdi::THEMES[d.theme.min(gdi::THEMES.len() - 1)];
                gdi::fill(hdc, &rc, th.bg);
                let pad = (8.0 * sc) as i32;
                let mut x = pad;
                let ly = (2.0 * sc) as i32;
                let vy = (10.0 * sc) as i32;
                let segs = segments(d.mask);
                let last = segs.len().saturating_sub(1);
                for (i, m) in segs.into_iter().enumerate() {
                    let (label, accent, value) = match m {
                        M_CPU => ("CPU", gdi::ACC_CPU, format!("{:.0}%", d.cpu)),
                        M_RAM => ("RAM", gdi::ACC_RAM, format!("{:.0}%", d.ram)),
                        M_GPU => (
                            "GPU",
                            gdi::ACC_GPU,
                            if d.gpu_ok { format!("{:.0}%", d.gpu) } else { "—".into() },
                        ),
                        M_FPS => (
                            "FPS",
                            gdi::ACC_FPS,
                            d.fps.map(|f| f.to_string()).unwrap_or_else(|| "—".into()),
                        ),
                        M_DISK => (
                            "DSK",
                            gdi::ACC_DISK,
                            format!("R{} W{}", compact_rate(d.dr), compact_rate(d.dw)),
                        ),
                        M_NET => (
                            "NET",
                            gdi::ACC_NET,
                            format!("↓{} ↑{}", compact_rate(d.rx), compact_rate(d.tx)),
                        ),
                        // Running agents and waiting messages are separate
                        // counts, so they keep separate glyphs rather than
                        // being added together into one meaningless number.
                        _ => (
                            "AI",
                            gdi::ACC_GPU,
                            match (d.agents, d.msgs) {
                                (0, 0) => "—".into(),
                                (a, 0) => format!("{a}◆"),
                                (0, m) => format!("{m}◇"),
                                (a, m) => format!("{a}◆ {m}◇"),
                            },
                        ),
                    };
                    let seg_w = seg_width(m, sc);
                    gdi::text(hdc, x, ly, s.label_font, accent, label);
                    if i == last {
                        // The last segment's value hugs the strip's own right
                        // edge, same as the first label hugs the left pad —
                        // left-aligning it here like the others would leave
                        // its box's unused width as a visibly bigger margin
                        // than the left side has.
                        gdi::text_right(hdc, x + seg_w, vy, s.value_font, th.text, &value);
                    } else {
                        gdi::text(hdc, x, vy, s.value_font, th.text, &value);
                    }
                    x += seg_w;
                }
                if s.hover {
                    let grip = grip_size(sc);
                    gdi::resize_grip(hdc, rc.right, rc.bottom, grip, th.dim);
                }
            });
            EndPaint(hwnd, &ps);
            0
        }
        // The whole strip drags to move, except one corner that resizes it.
        // The little triangle painted there is the only visual cue; the
        // cursor is set explicitly below rather than left to Windows, which
        // otherwise falls back to the class cursor for this hit-test code.
        WM_NCHITTEST => {
            let sx = (lparam & 0xFFFF) as i16 as i32;
            let sy = ((lparam >> 16) & 0xFFFF) as i16 as i32;
            let mut r: RECT = std::mem::zeroed();
            GetWindowRect(hwnd, &mut r);
            let grip = STATE.with(|s| grip_size(s.borrow().scale));
            if sx >= r.right - grip && sy >= r.bottom - grip {
                HTBOTTOMRIGHT as LRESULT
            } else {
                HTCAPTION as LRESULT
            }
        }
        // DefWindowProc's own WM_SETCURSOR handling only special-cases the
        // client area and the real resize-border hit codes; HTCAPTION (what
        // the rest of the strip reports, to get drag-move for free) falls
        // through to the window class's cursor — IDC_SIZEALL, a big 4-way
        // arrow cross that reads as oversized on a 26px-tall strip. Handling
        // it here keeps the body a plain arrow and the corner a matching
        // diagonal resize cursor, and stops any surprise elsewhere.
        WM_SETCURSOR => {
            let hit = (lparam & 0xFFFF) as u32;
            let id = if hit == HTBOTTOMRIGHT as u32 { IDC_SIZENWSE } else { IDC_ARROW };
            unsafe { SetCursor(LoadCursorW(std::ptr::null_mut(), id)) };
            1
        }
        // The whole window stays client area: WS_THICKFRAME is only there to
        // enable the resize loop, not to put a frame around the strip.
        WM_NCCALCSIZE if wparam != 0 => 0,
        // A sizable window is otherwise held to Windows' minimum track size,
        // which is wider and taller than the strip at its smallest.
        WM_GETMINMAXINFO => {
            let mmi = lparam as *mut MINMAXINFO;
            (*mmi).ptMinTrackSize.x = 1;
            (*mmi).ptMinTrackSize.y = 1;
            0
        }
        // Dragging the corner scales the strip as a whole. The dragged width
        // chooses the scale — the long axis gives far more precision than a
        // 26px height — and both dimensions are then recomputed from it, so the
        // preview stays proportional instead of stretching.
        WM_SIZING => {
            let r = lparam as *mut RECT;
            let (mask, dpi) = STATE.with(|s| {
                let s = s.borrow();
                (s.data.mask, s.dpi)
            });
            let want = crate::util::widget_scale_from_width(
                total_width(mask, 1.0),
                (*r).right - (*r).left,
                dpi,
            );
            let scale = dpi * want;
            apply_scale(scale);
            (*r).right = (*r).left + total_width(mask, scale);
            (*r).bottom = (*r).top + strip_height(scale);
            1
        }
        // WM_SIZING only proposes a rect; without this the strip's own
        // content is never invalidated as the drag resizes it, leaving the
        // previous frame's pixels behind as artifacts.
        WM_SIZE => {
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_EXITSIZEMOVE | WM_MOVE => {
            let mut r: RECT = std::mem::zeroed();
            GetWindowRect(hwnd, &mut r);
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                s.moved_to = Some((r.left, r.top));
                // Reported once the drag ends rather than on every step of it,
                // so a resize costs one config write instead of dozens.
                if msg == WM_EXITSIZEMOVE && s.dpi > 0.0 {
                    s.resized_to = Some(s.scale / s.dpi);
                }
            });
            0
        }
        // The grip only paints while hovered, so entry needs to flip the flag
        // and repaint; TrackMouseEvent is what makes WM_MOUSELEAVE arrive at
        // all, since Windows doesn't otherwise tell a window the pointer left.
        WM_MOUSEMOVE => {
            let was_hovering = STATE.with(|s| s.borrow().hover);
            if !was_hovering {
                STATE.with(|s| s.borrow_mut().hover = true);
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut tme);
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_MOUSELEAVE => {
            STATE.with(|s| s.borrow_mut().hover = false);
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
