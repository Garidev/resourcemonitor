//! Raising another program's window.
//!
//! A pid is not a window. Most of the processes this app lists own no window at
//! all — service hosts, Chrome's renderers and its audio service, anything
//! headless — and several of the ones that do own more than one. So the caller
//! hands over an ordered list of candidate pids (the row's own process first,
//! then its siblings sharing an executable) and takes the first real window
//! found among them. That fallback is the whole reason double-clicking Chrome's
//! audio process lands you in Chrome: the process holding the sound is a
//! windowless utility process, and its browser-process sibling owns the window.
//!
//! What this deliberately cannot do is reach a *tab*. A Chromium tab is a
//! sandboxed renderer with no HWND, so no amount of window enumeration will find
//! it; that needs UI Automation over the browser's own tab strip, which is
//! per-browser, breaks on localisation, and forces the browser into full
//! accessibility mode for as long as it runs. The browser's own tab search
//! already does that job properly.

use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindow, GetWindowRect,
    GetWindowTextLengthW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    SetForegroundWindow, ShowWindow, GW_OWNER, SW_RESTORE,
};

struct Search {
    /// Candidate pids in priority order. Position in this list beats window
    /// size: the row you double-clicked is the one you meant.
    pids: Vec<u32>,
    best: HWND,
    best_rank: usize,
    best_area: i64,
}

/// Whether a window is one a person could switch to. `IsWindowVisible` alone is
/// not enough on Windows 10 and later: a suspended UWP app keeps a visible
/// top-level window that DWM has *cloaked*, and raising one flashes the taskbar
/// and does nothing. Owned windows are skipped so a modal or a tooltip does not
/// win over the frame it belongs to, and untitled ones because every process
/// with a message loop has an invisible message-only or tray window.
unsafe fn is_switchable(hwnd: HWND) -> bool {
    if IsWindowVisible(hwnd) == 0 || !GetWindow(hwnd, GW_OWNER).is_null() {
        return false;
    }
    if GetWindowTextLengthW(hwnd) == 0 {
        return false;
    }
    let mut cloaked: u32 = 0;
    let ok = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED as u32,
        &mut cloaked as *mut u32 as *mut _,
        std::mem::size_of::<u32>() as u32,
    );
    !(ok == 0 && cloaked != 0)
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let s = &mut *(lparam as *mut Search);
    if !is_switchable(hwnd) {
        return 1;
    }
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    let Some(rank) = s.pids.iter().position(|p| *p == pid) else {
        return 1;
    };
    // Among windows of the same process, the largest — a program with a main
    // frame and a floating palette should raise the frame.
    let mut r: RECT = std::mem::zeroed();
    let area = if GetWindowRect(hwnd, &mut r) != 0 {
        (r.right - r.left) as i64 * (r.bottom - r.top) as i64
    } else {
        0
    };
    if s.best.is_null() || rank < s.best_rank || (rank == s.best_rank && area > s.best_area) {
        s.best = hwnd;
        s.best_rank = rank;
        s.best_area = area;
    }
    1
}

/// Raise the first of `pids` that owns a switchable window. Returns false when
/// none of them do, which the caller has to be able to say out loud — silently
/// doing nothing reads as a broken double-click.
pub fn raise(pids: &[u32]) -> bool {
    if pids.is_empty() {
        return false;
    }
    let mut s = Search {
        pids: pids.to_vec(),
        best: null_mut(),
        best_rank: usize::MAX,
        best_area: -1,
    };
    unsafe {
        EnumWindows(Some(enum_cb), &mut s as *mut Search as LPARAM);
        if s.best.is_null() {
            return false;
        }
        let target = s.best;
        if IsIconic(target) != 0 {
            ShowWindow(target, SW_RESTORE);
        }
        // We are the foreground process — the user just double-clicked us — so
        // `SetForegroundWindow` is permitted outright. The thread-input attach
        // is for the case where the click arrives while focus has already moved
        // on: without it Windows refuses the change and flashes the taskbar
        // button instead, which looks exactly like a bug in this app.
        let fg = GetForegroundWindow();
        let ours = GetCurrentThreadId();
        let theirs = if fg.is_null() { 0 } else { GetWindowThreadProcessId(fg, null_mut()) };
        let attached = theirs != 0 && theirs != ours && AttachThreadInput(ours, theirs, 1) != 0;
        BringWindowToTop(target);
        let ok = SetForegroundWindow(target) != 0;
        if attached {
            AttachThreadInput(ours, theirs, 0);
        }
        ok
    }
}
