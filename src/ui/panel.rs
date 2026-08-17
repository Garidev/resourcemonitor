//! Panel window: overview with sparklines and app finder, per-app drill-downs
//! with filter + scrolling, per-process watch view, settings with a GUI log-
//! rule editor, pin-as-app-window mode (resizable), tray interaction.
//! All UI state lives here on the UI thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, IntersectClipRect, InvalidateRect, RestoreDC, SaveDC,
    ScreenToClient, SetBkColor, SetTextColor, HBRUSH, HDC, HFONT, PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::NIN_BALLOONUSERCLICK;
use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::gdi::{self, BackBuffer, Fonts};
use super::tray::{Tray, WM_APP_TRAY};
use crate::config::{self, Settings};
use crate::conns;
use crate::rules;
use crate::sampler::{ProcStat, Shared, Snapshot, WM_APP_NOTIFY, WM_APP_SNAPSHOT};
use crate::util::{
    affinity_label, format_bytes, format_pct, format_rate, Ceiling, NavTrail, Ring, Scale,
    CARD_METRIC,
    HEADER_STRIDE, RADIUS, ROW_LIST, ROW_METRIC, SPARK_W, ROW_NAV, ROW_NAV_STRIDE, SP1, SP2, SP3, SP4,
    SP5, SP6,
};

const IDM_EXIT: u32 = 100;
const IDM_AUTOSTART: u32 = 101;
const IDM_SETTINGS: u32 = 102;
const PANEL_W: i32 = 336;
const EDIT_FILTER: u32 = 1;
const EDIT_VALUE: u32 = 2;
const EDIT_PATH: u32 = 3;
const EDIT_NOTIFY: u32 = 4;
const EN_CHANGE_N: u32 = 0x0300;

// WS_CLIPCHILDREN is essential: without it the panel's double-buffered
// painting draws over the EDIT children every frame, making typed text and
// the caret invisible (the inputs look dead).
const FLYOUT_STYLE: u32 = WS_POPUP | WS_BORDER | WS_CLIPCHILDREN;
const PINNED_STYLE: u32 =
    WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_THICKFRAME | WS_CLIPCHILDREN;

/// Rule-builder metric choices: (key, label, unit).
const R_METRICS: [(&str, &str, &str); 9] = [
    ("cpu", "CPU", "%"),
    ("ram", "RAM", "%"),
    ("gpu", "GPU", "%"),
    ("disk", "Disk", "MB/s"),
    ("net", "Net", "MB/s"),
    ("fps", "FPS", "fps"),
    ("sound", "Sound", "%"),
    ("proc", "One app…", ""),
    ("conn", "Connection…", ""),
];
/// Which part of a connection a `conn` rule matches on.
const R_CONN_FIELDS: [(rules::ConnField, &str); 4] = [
    (rules::ConnField::Host, "hostname"),
    (rules::ConnField::Ip, "remote IP"),
    (rules::ConnField::Port, "port"),
    (rules::ConnField::Process, "app"),
];
const R_PROC_SUBS: [(&str, &str); 5] = [("cpu", "CPU"), ("ram", "RAM"), ("disk", "disk"), ("net", "net"), ("sound", "sound")];
const R_COOLDOWNS: [(u64, &str); 3] = [(30, "30 seconds"), (60, "60 seconds"), (300, "5 minutes")];

#[derive(Clone, Copy, PartialEq)]
pub enum Metric {
    Cpu,
    Ram,
    Gpu,
    Disk,
    Net,
    Audio,
}

/// What a hero plot is charting. FPS is deliberately not a `Metric` — it has no
/// per-app drill-down of the same shape, its own list view, and no place in the
/// metric ordering — but it has a history ring like the rest, so the plot takes
/// this instead of a `Metric` and FPS gets the same graph everything else has.
#[derive(Clone, Copy, PartialEq)]
enum Hero {
    M(Metric),
    Fps,
}

/// Settings categories, grouped by what they affect.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsPage {
    General,
    Ai,
    MainPanel,
    Desktop,
    Alerts,
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Main,
    Drill(Metric),
    Settings,
    /// One category of settings, opened from the settings menu.
    SettingsPage(SettingsPage),
    RuleEdit,
    /// Watching one app (name in `Ui::watch`) across all metrics.
    Process,
    /// All apps currently presenting frames, with their FPS.
    FpsApps,
    /// Live network connections: which app is talking to which endpoint.
    /// Machine-wide, or narrowed to one app when `conns_for` is set.
    Connections,
    /// Full list of messages received from MCP clients.
    McpMessages,
    /// What connected AI tools have reported they are working on.
    Activity,
}

#[derive(Clone, Copy)]
enum Action {
    Drill(Metric),
    Back,
    /// Hover-only: a core cell in the CPU grid. Exists so the cell participates
    /// in hit testing — which is what drives a repaint on mouse move — without
    /// doing anything when clicked.
    HoverCore,
    /// Pin or unpin the sample under the cursor on the hero plot.
    PinHero,
    OpenSettings,
    TogglePin,
    /// Dismiss the flyout (the × in the header strip).
    ClosePanel,
    /// Toggle one AI notification preset (a NOTIFY_* bit).
    NotifyPreset(u32),
    /// Add whatever is in the box to the notification instruction list.
    NotifyCustomAdd,
    NotifyCustomToggle(usize),
    NotifyCustomDelete(usize),
    ToggleTop,
    ToggleTray(usize),
    SetInterval(u32),
    Kill(usize),
    Watch(usize),
    KillWatched,
    /// Expand/collapse the subprocess list in the watch view.
    ToggleSubs,
    /// Which metric the subprocess list sorts by and shows.
    SubMetric(Metric),
    /// End one specific subprocess by pid.
    KillPid(u32),
    /// Expand/collapse one MCP message to its full text.
    ToggleMsg(usize),
    /// Expand/collapse one agent row to its full reported detail.
    ToggleAgent(u64),
    /// Pick a text size preset (index into config::TEXT_SIZES).
    SetTextSize(usize),
    ToggleRule(usize),
    DeleteRule(usize),
    AddRule,
    /// Show or hide one main-panel metric row.
    MetricVisible(usize),
    /// Press on a metric's drag handle; the drop is handled on button-up.
    MetricDragStart(usize),
    ToggleFpsOverlay,
    FpsColor(usize),
    FpsOpacity(usize),
    DraftMetric(usize),
    DraftDir(bool),
    DraftProcSub(usize),
    /// Which part of a connection a drafted connection rule matches on.
    DraftConnField(usize),
    DraftTop,
    /// Where a drafted alert is delivered: desktop, file, or both.
    DraftDeliver(usize),
    DraftCooldown,
    DraftSave,
    DraftCancel,
    PickApp,
    ShowFpsApps,
    /// Open the machine-wide connection list.
    ShowConns,
    /// Open the connection list narrowed to the app being watched.
    ShowAppConns,
    ClearFilter,
    ToggleAutostart,
    ToggleMcp,
    ShowMcp,
    /// Open the AI activity view.
    ShowActivity,
    /// Expand/collapse the finished list in the activity view.
    ToggleFinished,
    /// Turn the agent history log file on or off.
    ToggleAgentLog,
    /// Open one settings category.
    OpenSettingsPage(SettingsPage),
    /// Forget finished agents.
    ClearHistory,
    /// Forget every reported agent (they are only ever assistant claims).
    ClearAgents,
    ClearMcp,
    CopyMcpCmd,
    TogglePause,
    SetTheme(usize),
    ToggleWidget,
    SetWidgetTheme(usize),
    WidgetMetric(u32),
    SnapWidget,
    /// Put the widget back to its default size, for when a drag has left it
    /// unreadably small or absurdly large.
    ResetWidgetSize,
}

#[derive(Clone, Copy)]
struct RuleDraft {
    metric: usize,
    gt: bool,
    proc_sub: usize,
    /// Index into R_CONN_FIELDS, for a connection rule.
    conn_field: usize,
    /// Where the alert goes: DELIVER_DESKTOP, DELIVER_FILE or DELIVER_BOTH.
    deliver: usize,
    top: bool,
    cooldown: usize,
}

pub const DELIVER_DESKTOP: usize = 0;
pub const DELIVER_FILE: usize = 1;

impl Default for RuleDraft {
    fn default() -> Self {
        RuleDraft {
            metric: 0,
            gt: true,
            proc_sub: 0,
            conn_field: 0,
            deliver: DELIVER_DESKTOP,
            top: true,
            cooldown: 1,
        }
    }
}

pub struct Ui {
    shared: Arc<Shared>,
    cfg: Settings,
    /// The latest sample.
    ///
    /// Behind an `Rc` because the paint needs a snapshot it can read while it
    /// also holds `&mut self` for hit registration, and the way it got one was
    /// a deep clone — of a `Vec<ProcStat>` with a `String` per process, two or
    /// three times per paint. That was tolerable at one paint per second and is
    /// not at sixty, which is what the hover cross-fade made possible. Cloning
    /// an `Rc` is a refcount bump, so the deep copy now happens once per
    /// *sample* instead of several times per *frame*.
    snap: Rc<Snapshot>,
    hist_cpu: Ring,
    hist_mem: Ring,
    hist_gpu: Ring,
    /// Disk and network are two-way, so each keeps its directions apart rather
    /// than summing them into one trace. Summed, a chart cannot show that a
    /// machine is writing hard while reading nothing — which is most of what
    /// makes a disk graph worth looking at.
    hist_disk: Ring,
    hist_disk_w: Ring,
    hist_net: Ring,
    hist_net_tx: Ring,
    /// The current paint's pixel surface, valid only between `BackBuffer::new`
    /// and `present`. Charts reach for it to write antialiased coverage; when
    /// it is `None` they fall back to an aliased `Polyline`, so a DIB that
    /// failed to allocate degrades instead of disappearing.
    surf: Option<gdi::Surface>,
    /// True between button-down and button-up, so a card can paint its press
    /// state. Press is new: the panel previously gave no feedback at all
    /// between hover and whatever the click did.
    pressed: bool,
    /// The cached off-screen buffer, rebuilt only on a size change.
    bb: Option<BackBuffer>,
    /// Measured height of the drill-down hero plate, including the gap below
    /// it. Zero until the first paint.
    hero_h: i32,
    /// The live hover cross-fade: the rect being left, the rect being entered,
    /// and progress from 0 to 1. Either rect may be empty, meaning the cursor
    /// came from or went to nothing.
    ///
    /// The only motion in the product the user actually feels. Snapping between
    /// `card` and `card_hover` across a 15 % lightness step is what made the
    /// panel read as a form rather than an instrument.
    fade: Option<(RECT, RECT, f32)>,
    /// Whether the fade timer is currently live. A timer that outlives its
    /// transition would repaint forever, which in a resource monitor is the
    /// worst bug available — so it is created in exactly one place, killed the
    /// moment progress completes, and killed again when the mouse leaves.
    fade_timer: bool,
    /// One 60-sample ring per logical core. A core cell used to be a bare
    /// snapshot bar: 32 cores of history is a few KB and it turns each cell
    /// into a window, which is what makes a peak tick meaningful.
    core_hist: Vec<Ring>,
    /// Sticky y-ceilings for the four rate charts. One `f32` each; see
    /// [`util::Ceiling`]. Only the rate metrics need them — percentages pin to
    /// 100 and FPS quantises off a 60 floor.
    ceil_disk: Ceiling,
    ceil_net: Ceiling,
    /// The second direction gets its own ceiling. Shared, the smaller of the
    /// two was drawing one or two pixels above the midline on a row chart.
    ceil_disk_w: Ceiling,
    ceil_net_tx: Ceiling,
    ceil_watch_ram: Ceiling,
    ceil_watch_disk: Ceiling,
    ceil_watch_net: Ceiling,
    ceil_watch_disk_w: Ceiling,
    ceil_watch_net_tx: Ceiling,
    hist_audio: Ring,
    hist_fps: Ring,
    view: View,
    /// Views passed through to reach the current one. A single trail rather
    /// than a "came from" field per view: two views each remembering the other
    /// is a loop with no way out, which is exactly what a watched app and its
    /// connection list used to do.
    nav: NavTrail<View>,
    hits: Vec<(RECT, Action)>,
    /// (image name, pids) for rows currently shown in a drill/find view.
    drawn_rows: Vec<(String, Vec<u32>)>,
    /// Connections as of the last snapshot, already joined with process names
    /// and resolved hostnames. Rebuilt on the tick rather than per paint,
    /// which also happens on every mouse move.
    conn_rows: Vec<crate::conns::Row>,
    /// How many connections the sweep found before the view narrowed them.
    conn_total: usize,
    /// When that sweep ran; 0 means we have not swept since opening the view.
    conns_swept: u64,
    /// Narrows the connection view to one app, set when it is opened from a
    /// watched app rather than from the network drill-down.
    conns_for: Option<String>,
    fonts: Fonts,
    /// Everything is laid out in units of `scale`, so the text-size preference
    /// rides on this rather than on font heights alone — the panel grows with
    /// its text instead of the text outgrowing the panel.
    scale: f32,
    /// What the display alone asks for, before the user's text-size choice.
    /// Kept apart so changing that choice cannot compound.
    dpi_scale: f32,
    tray: Tray,
    taskbar_created_msg: u32,
    visible: bool,
    /// Drive count the current window height was computed for.
    layout_drives: usize,
    /// Footer button count the current window height was computed for.
    layout_footer: usize,
    /// Visible metric-row count the current window height was computed for.
    layout_metrics: usize,
    /// A pinned sample on the hero plot, counted back from the newest so it
    /// survives the window scrolling: 0 is the newest sample, 1 the one before
    /// it. Stored this way because an absolute index would slide one sample to
    /// the left on every tick and point at the wrong reading a second later.
    pin_back: Option<usize>,
    /// The hero plot's rect from the last paint, so a click can be turned back
    /// into a sample index.
    hero_plot: RECT,
    /// Where a click on the balloon currently on screen should land.
    balloon_target: Option<View>,
    /// Notifications waiting to be shown. `Shell_NotifyIcon` gives an icon one
    /// balloon at a time and raising a second replaces the first, so a batch
    /// arriving together has to be paced or only the last of it is ever seen.
    balloon_queue: std::collections::VecDeque<(String, String, bool)>,
    /// Whether the pacing timer is running, so it is started once per burst.
    balloon_timer: bool,
    edit: HWND,
    edit_val: HWND,
    edit_path: HWND,
    /// Multi-line box for the free-text AI notification instructions.
    edit_notify: HWND,
    /// Y the notify box was painted at this frame, or -1 when not shown.
    /// The settings view scrolls, so the child window is repositioned from
    /// this after each paint rather than guessed ahead of one.
    notify_text_y: i32,
    /// Right edge of the notify input frame as painted this frame, so the
    /// EDIT child is sized from the frame rather than recomputing it.
    notify_frame_right: i32,
    /// Index of the metric row being dragged in settings, if any.
    metric_drag: Option<usize>,
    /// Top and row height of the metric list as painted, so a drop position
    /// can be turned back into an index. Set every paint of the settings view.
    metric_list: (i32, i32),
    edit_brush: HBRUSH,
    filter: String,
    /// What was in the filter box before the watch view took it over, put
    /// back on the way out — the drill filter persists until cleared.
    watch_saved_filter: String,
    scroll: i32,
    max_scroll: i32,
    /// Image name being watched in the Process view, plus where to go back to.
    watch: Option<String>,

    /// cpu %, ram bytes, gpu %, disk B/s, net B/s history for the watched app.
    watch_rings: Vec<Ring>,
    draft: RuleDraft,
    overlay: HWND,
    widget: HWND,
    /// (view code, rule-editor shape, subs expanded, watch has fps,
    /// client w, client h) the EDIT children were last positioned for.
    /// Prevents hide/show churn every paint, which would steal keyboard
    /// focus from the inputs. The rule-editor shape covers both the chosen
    /// metric and the connection field, because either one changes which
    /// boxes exist and what they are asking for.
    edit_sig: (u32, u32, bool, bool, i32, i32, i32),
    /// Cursor position in client coords while inside the window; drives
    /// hover highlighting of clickable rows.
    hover_pos: Option<(i32, i32)>,
    mouse_tracking: bool,
    /// Cached "Start with Windows" task state (queried on entering settings).
    autostart_on: bool,
    autostart_err: bool,
    /// Recent MCP messages (HH:MM, title, message), newest first.
    mcp_messages: std::collections::VecDeque<(String, String, String)>,
    /// Freeze the drill-down list so fast-moving rows can be clicked.
    paused: bool,
    frozen: Option<Rc<Snapshot>>,
    /// True briefly after the MCP connect command is copied, so the button
    /// can read "Copied" as confirmation. Reset on any view change.
    mcp_copied: bool,
    /// Cached per-process role labels (pid -> "GPU process", "Renderer", ...),
    /// so we read each process's command line at most once.
    proc_roles: HashMap<u32, String>,
    /// Whether the subprocess breakdown in the watch view is expanded. Starts
    /// collapsed so opening a browser doesn't dump 30+ rows at once.
    subs_expanded: bool,
    /// Whether the Finished list in the activity view is open. Starts closed
    /// so the view shows live work first.
    finished_expanded: bool,
    /// Which metric the subprocess list sorts by and shows a column for.
    sub_metric: Metric,
    /// Indices of MCP messages expanded to their full wrapped text. Messages
    /// can be long, so the list shows one line until you open one.
    msg_expanded: std::collections::HashSet<usize>,
    /// Agent rows opened to their full wrapped detail. Keyed by a hash of the
    /// agent rather than by index: the activity list reorders as agents start
    /// and finish, and an index would move the open row out from under you.
    agent_expanded: std::collections::HashSet<u64>,
}


fn hhmm() -> String {
    let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut st) };
    format!("{:02}:{:02}", st.wHour, st.wMinute)
}
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Network rate that reads "0 KB/s" for an idle direction instead of "0 B/s".
fn net_rate(bps: u64) -> String {
    if bps == 0 {
        "0 KB/s".to_string()
    } else {
        format_rate(bps)
    }
}

/// The one-time command that registers this app as an MCP server in Claude
/// Code. The shim (`resmon-mcp.exe`) sits next to the running executable, so
/// the path is correct wherever the app is installed.
fn mcp_connect_cmd() -> String {
    let shim = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("resmon-mcp.exe")))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "resmon-mcp.exe".to_string());
    format!("claude mcp add resourcemonitor \"{}\"", shim)
}

/// Put UTF-16 text on the Windows clipboard.
fn copy_to_clipboard(hwnd: HWND, text: &str) {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
        if !h.is_null() {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                GlobalUnlock(h);
                SetClipboardData(CF_UNICODETEXT, h as _);
            }
        }
        CloseClipboard();
    }
}

// Placeholder text per EDIT handle, and the original window procedure we chain
// to. Keyed on the handle because the hint changes with the view.
thread_local! {
    static HINTS: RefCell<HashMap<isize, String>> = RefCell::new(HashMap::new());
    static EDIT_PROC: RefCell<WNDPROC> = const { RefCell::new(None) };
}

/// Set the placeholder a field shows while it is empty.
///
/// This deliberately does **not** use `EM_SETCUEBANNER`. The control draws a cue
/// banner in `GetSysColor(COLOR_GRAYTEXT)`, which is a fixed mid grey we cannot
/// restyle: measured against this app's dark field it reaches 3.7:1, and making
/// the field lighter moves it *toward* the grey and makes it worse — the best
/// any background can do is about 4:1, still under the 4.5:1 floor for text
/// this size. So the hint is ours to draw, in `mute`, which clears the floor on
/// every theme.
///
/// Drawn in a subclass rather than by putting placeholder text *in* the control:
/// the control's text is never touched, so a hint can never be mistaken for a
/// filter value.
fn set_cue(edit: HWND, text: &str) {
    HINTS.with(|h| h.borrow_mut().insert(edit as isize, text.to_string()));
    unsafe {
        let prev = SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_hint_proc as isize);
        if prev != 0 && prev != edit_hint_proc as isize {
            EDIT_PROC.with(|p| {
                let mut p = p.borrow_mut();
                if p.is_none() {
                    *p = std::mem::transmute::<isize, WNDPROC>(prev);
                }
            });
        }
        InvalidateRect(edit, std::ptr::null(), 1);
    }
}

/// Chains to the EDIT's own procedure, then paints the hint over the empty
/// field. `user32`'s `SetWindowLongPtrW` rather than comctl32's
/// `SetWindowSubclass`, so this costs no new DLL import.
unsafe extern "system" fn edit_hint_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let orig = EDIT_PROC.with(|p| *p.borrow());
    let call = |h, m, w, l| match orig {
        Some(_) => CallWindowProcW(orig, h, m, w, l),
        None => DefWindowProcW(h, m, w, l),
    };
    if msg != WM_PAINT {
        return call(hwnd, msg, wparam, lparam);
    }
    let ret = call(hwnd, msg, wparam, lparam);
    if GetWindowTextLengthW(hwnd) != 0 {
        return ret;
    }
    let hint = HINTS.with(|h| h.borrow().get(&(hwnd as isize)).cloned());
    let Some(hint) = hint else { return ret };
    if hint.is_empty() {
        return ret;
    }
    // The font the control was given, so the hint sits on the same baseline and
    // at the same size as whatever the user is about to type.
    let font = SendMessageW(hwnd, WM_GETFONT, 0, 0) as windows_sys::Win32::Graphics::Gdi::HFONT;
    let dc = windows_sys::Win32::Graphics::Gdi::GetDC(hwnd);
    if !dc.is_null() {
        let mut r: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut r);
        if !font.is_null() {
            let (asc, desc, _) = gdi::text_metrics(dc, font);
            let y = ((r.bottom - r.top) - (asc + desc)) / 2;
            gdi::text(dc, 1, y.max(0), font, gdi::t().mute, &hint);
        }
        windows_sys::Win32::Graphics::Gdi::ReleaseDC(hwnd, dc);
    }
    ret
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn make_edit(hwnd: HWND, id: u32, font: windows_sys::Win32::Graphics::Gdi::HFONT) -> HWND {
    make_edit_styled(hwnd, id, font, ES_AUTOHSCROLL as u32)
}

fn make_edit_styled(
    hwnd: HWND,
    id: u32,
    font: windows_sys::Win32::Graphics::Gdi::HFONT,
    extra: u32,
) -> HWND {
    unsafe {
        let e = CreateWindowExW(
            0,
            gdi::wide("EDIT").as_ptr(),
            gdi::wide("").as_ptr(),
            WS_CHILD | extra,
            0,
            0,
            0,
            0,
            hwnd,
            id as usize as _,
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        );
        SendMessageW(e, WM_SETFONT, font as usize, 1);
        e
    }
}

fn get_text(h: HWND) -> String {
    let mut buf = [0u16; 260];
    let n = unsafe { GetWindowTextW(h, buf.as_mut_ptr(), 260) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Like `get_text` but sized from the control's actual content, for the
/// free-text instructions box where 260 characters is not enough.
fn get_text_long(h: HWND) -> String {
    const WM_GETTEXTLENGTH: u32 = 0x000E;
    let len = unsafe { SendMessageW(h, WM_GETTEXTLENGTH, 0, 0) } as usize;
    if len == 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len + 1];
    let n = unsafe { GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn set_text(h: HWND, s: &str) {
    unsafe { SetWindowTextW(h, gdi::wide(s).as_ptr()) };
}

pub fn init(hwnd: HWND, shared: Arc<Shared>, cfg: Settings) {
    gdi::set_theme(cfg.theme as usize);
    let dpi = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
    let dpi_scale = dpi as f32 / 96.0;
    let scale = dpi_scale * config::text_scale(cfg.text_size);
    let taskbar_created_msg =
        unsafe { RegisterWindowMessageW(gdi::wide("TaskbarCreated").as_ptr()) };

    // Dark title bar for pinned mode (best-effort; ignored on old builds).
    unsafe {
        let on: i32 = 1;
        windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute(
            hwnd,
            20, // DWMWA_USE_IMMERSIVE_DARK_MODE
            &on as *const i32 as *const _,
            4,
        );
    }

    let fonts = Fonts::new(scale);
    let edit = make_edit(hwnd, EDIT_FILTER, fonts.body);
    let edit_val = make_edit(hwnd, EDIT_VALUE, fonts.body);
    let edit_path = make_edit(hwnd, EDIT_PATH, fonts.body);
    let edit_notify = make_edit(hwnd, EDIT_NOTIFY, fonts.body);
    let edit_brush = unsafe { CreateSolidBrush(gdi::t().input_bg) };

    let start_pinned = cfg.pinned;
    let ui = Ui {
        shared: shared.clone(),
        tray: Tray::new(hwnd, &cfg),
        cfg,
        snap: Rc::new(Snapshot::default()),
        hist_cpu: Ring::new(60),
        hist_mem: Ring::new(60),
        hist_gpu: Ring::new(60),
        hist_disk: Ring::new(60),
        hist_disk_w: Ring::new(60),
        hist_net: Ring::new(60),
        hist_net_tx: Ring::new(60),
        surf: None,
        pressed: false,
        bb: None,
        hero_h: 0,
        fade: None,
        fade_timer: false,
        core_hist: Vec::new(),
        ceil_disk: Ceiling::default(),
        ceil_net: Ceiling::default(),
        ceil_disk_w: Ceiling::default(),
        ceil_net_tx: Ceiling::default(),
        ceil_watch_ram: Ceiling::default(),
        ceil_watch_disk: Ceiling::default(),
        ceil_watch_net: Ceiling::default(),
        ceil_watch_disk_w: Ceiling::default(),
        ceil_watch_net_tx: Ceiling::default(),
        hist_audio: Ring::new(60),
        hist_fps: Ring::new(60),
        view: View::Main,
        hits: Vec::new(),
        drawn_rows: Vec::new(),
        conn_rows: Vec::new(),
        conn_total: 0,
        conns_swept: 0,
        conns_for: None,
        fonts,
        scale,
        dpi_scale,
        taskbar_created_msg,
        visible: false,
        layout_drives: 0,
        layout_footer: 0,
        layout_metrics: 0,
        pin_back: None,
        hero_plot: RECT { left: 0, top: 0, right: 0, bottom: 0 },
        balloon_target: None,
        balloon_queue: std::collections::VecDeque::new(),
        balloon_timer: false,
        edit,
        edit_val,
        edit_path,
        edit_notify,
        notify_text_y: -1,
        notify_frame_right: 0,
        metric_drag: None,
        metric_list: (0, 0),
        edit_brush,
        filter: String::new(),
        watch_saved_filter: String::new(),
        scroll: 0,
        max_scroll: 0,
        watch: None,
        nav: NavTrail::new(16),
        watch_rings: (0..9).map(|_| Ring::new(60)).collect(),
        draft: RuleDraft::default(),
        overlay: std::ptr::null_mut(),
        widget: std::ptr::null_mut(),
        edit_sig: (u32::MAX, u32::MAX, false, false, 0, 0, 0),
        hover_pos: None,
        mouse_tracking: false,
        autostart_on: false,
        autostart_err: false,
        mcp_messages: std::collections::VecDeque::with_capacity(16),
        paused: false,
        frozen: None,
        mcp_copied: false,
        proc_roles: HashMap::new(),
        subs_expanded: false,
        finished_expanded: false,
        sub_metric: Metric::Ram,
        msg_expanded: std::collections::HashSet::new(),
        agent_expanded: std::collections::HashSet::new(),
    };
    UI.with(|u| *u.borrow_mut() = Some(ui));
    if start_pinned {
        with_ui(|ui| ui.set_pinned(hwnd, true));
    }
}

fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|cell| {
        let Ok(mut borrow) = cell.try_borrow_mut() else { return None };
        borrow.as_mut().map(f)
    })
}

pub unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Win32 re-enters this proc synchronously (WM_ACTIVATE during
    // ShowWindow/SetForegroundWindow, WM_ERASEBKGND during BeginPaint,
    // menu/dialog loops). If the state is already borrowed we are inside such
    // a nested call — let DefWindowProc handle it.
    let handled = with_ui(|ui| ui.handle(hwnd, msg, wparam, lparam)).flatten();
    match handled {
        Some(r) => r,
        None => match msg {
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        },
    }
}

impl Ui {
    fn s(&self, v: i32) -> i32 {
        crate::util::scaled(v, self.scale)
    }

    /// Height of every interactive control: chips, input frames, buttons.
    /// One number so a chip never sits shorter than the input beside it.
    fn ctrl_h(&self) -> i32 {
        crate::util::ctrl_h(self.scale)
    }

    /// Vertical advance past a row of controls.
    fn ctrl_row(&self) -> i32 {
        crate::util::ctrl_row(self.scale)
    }

    /// Gap the "add" chip needs at the right of the notify input. Painted
    /// frame and EDIT child both size from this, so they cannot disagree.
    fn add_chip_gap(&self, dc: HDC) -> i32 {
        gdi::text_width(dc, self.fonts.micro, "add") + self.s(16) + self.s(6)
    }

    fn handle(&mut self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_APP_SNAPSHOT => {
                self.on_snapshot(hwnd);
                Some(0)
            }
            WM_APP_NOTIFY => {
                let pending: Vec<(String, String, bool)> =
                    std::mem::take(&mut *self.shared.notifications.lock().unwrap());
                for (title, message, is_mcp) in pending {
                    // Only MCP messages populate the Messages list and footer;
                    // alert notifications just show as a desktop balloon. The
                    // list is filled straight away even though the balloons are
                    // paced, so nothing is missing from it while a burst drains.
                    let from_mcp = is_mcp && self.cfg.mcp_enabled;
                    if from_mcp {
                        self.mcp_messages.push_front((hhmm(), title.clone(), message.clone()));
                        if self.mcp_messages.len() > 12 {
                            self.mcp_messages.pop_back();
                        }
                    }
                    self.balloon_queue.push_back((title, message, from_mcp));
                    // Drop from the front when the backlog is over the bound:
                    // a stale alert is the least useful thing in the queue.
                    while self.balloon_queue.len() > BALLOON_QUEUE_MAX {
                        self.balloon_queue.pop_front();
                    }
                }
                // Show one immediately and let the timer walk the remainder, so
                // a single notification is still instant.
                if !self.balloon_timer && !self.balloon_queue.is_empty() {
                    self.balloon_timer = true;
                    unsafe { SetTimer(hwnd, ID_BALLOON, BALLOON_MS, None) };
                    self.next_balloon(hwnd);
                }
                if self.visible {
                    unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
                }
                Some(0)
            }
            WM_APP_TRAY => {
                match lparam as u32 {
                    WM_LBUTTONUP => self.tray_click(hwnd),
                    WM_RBUTTONUP => self.tray_menu(hwnd),
                    NIN_BALLOONUSERCLICK => self.balloon_click(hwnd),
                    _ => {}
                }
                Some(0)
            }
            WM_PAINT => {
                self.paint(hwnd);
                Some(0)
            }
            WM_LBUTTONDOWN => {
                let x = (lparam & 0xFFFF) as i16 as i32;
                let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
                // Set before dispatching so the card the click lands on paints
                // its press state. The action still fires on button-down, so
                // nothing about the interaction model changes — the user simply
                // sees the press land, which the panel never showed before.
                self.pressed = true;
                self.hover_pos = Some((x, y));
                self.click(hwnd, x, y);
                Some(0)
            }
            WM_MOUSEMOVE => {
                let x = (lparam & 0xFFFF) as i16 as i32;
                let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
                if !self.mouse_tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    unsafe { TrackMouseEvent(&mut tme) };
                    self.mouse_tracking = true;
                }
                let before = self.hover_pos.and_then(|(ox, oy)| self.hit_at(ox, oy));
                let after = self.hit_at(x, y);
                self.hover_pos = Some((x, y));
                // While dragging a metric the insertion line follows the
                // cursor, so every move needs a repaint, not just those that
                // change which control is hovered.
                if before != after {
                    let rect_of = |i: Option<usize>| {
                        i.and_then(|i| self.hits.get(i)).map(|h| h.0).unwrap_or(RECT {
                            left: 0,
                            top: 0,
                            right: 0,
                            bottom: 0,
                        })
                    };
                    self.fade = Some((rect_of(before), rect_of(after), 0.0));
                    if !self.fade_timer {
                        unsafe { SetTimer(hwnd, ID_FADE, FADE_MS, None) };
                        self.fade_timer = true;
                    }
                }
                if before != after || self.metric_drag.is_some() {
                    unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
                }
                Some(0)
            }
            WM_LBUTTONUP if self.metric_drag.is_some() => {
                let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
                self.pressed = false;
                self.drop_metric(hwnd, y);
                Some(0)
            }
            WM_LBUTTONUP => {
                // Repaint to drop the press state, then fall through: swallowing
                // button-up here would break the default processing the window
                // still relies on.
                if std::mem::take(&mut self.pressed) {
                    unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
                }
                None
            }
            WM_TIMER if wparam == ID_BALLOON => {
                self.next_balloon(hwnd);
                Some(0)
            }
            WM_TIMER if wparam == ID_FADE => {
                let done = match &mut self.fade {
                    Some((_, _, t)) => {
                        *t += FADE_MS as f32 / FADE_TOTAL_MS;
                        *t >= 1.0
                    }
                    None => true,
                };
                if done {
                    self.fade = None;
                    unsafe { KillTimer(hwnd, ID_FADE) };
                    self.fade_timer = false;
                }
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
                Some(0)
            }
            WM_MOUSELEAVE => {
                self.mouse_tracking = false;
                self.pressed = false;
                self.fade = None;
                if self.fade_timer {
                    unsafe { KillTimer(hwnd, ID_FADE) };
                    self.fade_timer = false;
                }
                if self.hover_pos.take().is_some() {
                    unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
                }
                Some(0)
            }
            WM_SETCURSOR => {
                const HTCLIENT: u32 = 1;
                if (lparam & 0xFFFF) as u32 == HTCLIENT {
                    let mut p = POINT { x: 0, y: 0 };
                    unsafe {
                        GetCursorPos(&mut p);
                        ScreenToClient(hwnd, &mut p);
                    }
                    if self.hit_at(p.x, p.y).is_some() {
                        unsafe {
                            SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_HAND));
                        }
                        return Some(1);
                    }
                    // Empty space on the flyout moves it, so say so: the drag
                    // has no other affordance to announce itself.
                    if !self.cfg.pinned {
                        unsafe {
                            SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_SIZEALL));
                        }
                        return Some(1);
                    }
                }
                None
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam >> 16) & 0xFFFF) as i16 as i32;
                let step = self.s(52);
                self.scroll = (self.scroll - delta / 120 * step).clamp(0, self.max_scroll);
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
                Some(0)
            }
            WM_COMMAND => {
                let id = (wparam & 0xFFFF) as u32;
                let code = ((wparam >> 16) & 0xFFFF) as u32;
                if id == EDIT_FILTER && code == EN_CHANGE_N {
                    self.filter = get_text(self.edit).to_lowercase();
                    if matches!(self.view, View::Main | View::Drill(_) | View::Process) {
                        self.scroll = 0;
                        unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
                    }
                }
                Some(0)
            }
            WM_CTLCOLOREDIT => {
                unsafe {
                    SetBkColor(wparam as HDC, gdi::t().input_bg);
                    SetTextColor(wparam as HDC, gdi::t().text);
                }
                Some(self.edit_brush as LRESULT)
            }
            WM_GETMINMAXINFO => {
                let mmi = lparam as *mut MINMAXINFO;
                if !mmi.is_null() {
                    unsafe {
                        (*mmi).ptMinTrackSize.x = self.s(300);
                        (*mmi).ptMinTrackSize.y = self.s(340);
                    }
                }
                Some(0)
            }
            WM_SIZE => {
                if self.cfg.pinned && self.visible && wparam != 1 {
                    // SIZE_MINIMIZED == 1
                    let mut r: RECT = unsafe { std::mem::zeroed() };
                    unsafe { GetWindowRect(hwnd, &mut r) };
                    self.cfg.win_w = r.right - r.left;
                    self.cfg.win_h = r.bottom - r.top;
                }
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
                Some(0)
            }
            WM_ACTIVATE => {
                // The panel deliberately does NOT close on click-away. Dismiss
                // it with the × in the header, Esc, or the tray icon.
                Some(0)
            }
            WM_KEYDOWN => {
                const VK_ESCAPE: usize = 0x1B;
                if wparam == VK_ESCAPE {
                    match self.view {
                        // Escape steps back one level, so a settings page
                        // returns to the menu rather than leaving outright.
                        View::Process
                        | View::RuleEdit
                        | View::SettingsPage(_)
                        | View::Connections
                        | View::Drill(_)
                        | View::Settings
                        | View::FpsApps
                        | View::McpMessages
                        | View::Activity => self.go_back(hwnd),
                        View::Main => {
                            if !self.filter.is_empty() {
                                // Clear state directly — SetWindowText doesn't
                                // reliably raise EN_CHANGE.
                                self.filter.clear();
                                set_text(self.edit, "");
                                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
                            } else if !self.cfg.pinned {
                                // Esc is an explicit dismissal, same as the ×.
                                self.hide_panel(hwnd);
                            }
                        }
                    }
                }
                Some(0)
            }
            WM_MOVE | WM_EXITSIZEMOVE => {
                if self.cfg.pinned && self.visible {
                    let mut r: RECT = unsafe { std::mem::zeroed() };
                    unsafe { GetWindowRect(hwnd, &mut r) };
                    self.cfg.win_x = r.left;
                    self.cfg.win_y = r.top;
                    if msg == WM_EXITSIZEMOVE {
                        config::save(&self.cfg);
                    }
                } else if !self.cfg.pinned && self.visible && msg == WM_EXITSIZEMOVE {
                    // Only at the end of a drag, never on the WM_MOVE that
                    // showing the flyout itself sends: otherwise the first
                    // open would pin it there and it would stop following the
                    // cursor for anyone who never moves it.
                    let mut r: RECT = unsafe { std::mem::zeroed() };
                    unsafe { GetWindowRect(hwnd, &mut r) };
                    self.cfg.fly_x = r.left;
                    self.cfg.fly_y = r.top;
                    config::save(&self.cfg);
                }
                Some(0)
            }
            WM_CLOSE => {
                // X on the pinned window: back to tray-only flyout mode.
                self.hide_panel(hwnd);
                self.set_pinned(hwnd, false);
                Some(0)
            }
            WM_DESTROY => {
                config::save(&self.cfg);
                self.tray.remove();
                crate::etw::stop();
                unsafe { PostQuitMessage(0) };
                Some(0)
            }
            m if m == self.taskbar_created_msg => {
                self.tray.readd();
                Some(0)
            }
            _ => None,
        }
    }

    // ------------------------------------------------------------ events

    fn on_snapshot(&mut self, hwnd: HWND) {
        self.snap = Rc::new(self.shared.snap.lock().unwrap().clone());
        if matches!(self.view, View::Connections) {
            self.refresh_conns();
        }
        let s = &self.snap;
        let mem_pct = if s.mem_total > 0 {
            s.mem_used as f32 / s.mem_total as f32 * 100.0
        } else {
            0.0
        };
        // While the graph is frozen the display rings do not advance, so a peak
        // stays where it is instead of sliding out from under the cursor. Frozen
        // means either the pause button is on or the pointer is over the plot.
        //
        // Only the rings stop. Alerts, the MCP server, the tray icons and the
        // tooltip all read the snapshot directly, so nothing that matters is
        // paused — this holds the picture, not the monitoring.
        let over_plot = matches!(self.view, View::Drill(_) | View::FpsApps)
            && self.hover_pos.is_some_and(|(x, y)| {
                let p = self.hero_plot;
                x >= p.left && x < p.right && y >= p.top && y < p.bottom
            });
        let frozen = self.paused || over_plot;

        // Only the display rings and their ceilings are inside this guard. The
        // tray icons, overlay, widget and relayout below it keep running, so a
        // frozen graph never means a frozen tray.
        if !frozen {
        self.hist_cpu.push(s.cpu_pct);
        self.hist_mem.push(mem_pct);
        self.hist_gpu.push(s.gpu_pct);
        self.hist_disk.push(s.disk_read_bps as f32);
        self.hist_disk_w.push(s.disk_write_bps as f32);
        self.hist_net.push(s.net_rx_bps as f32);
        self.hist_net_tx.push(s.net_tx_bps as f32);
        self.hist_audio.push(s.audio_peak * 100.0);
        // Advance the sticky rate ceilings exactly once per sample. Doing this
        // in the paint would decay them at the repaint rate, which hover alone
        // can drive to dozens of times a second.
        if self.core_hist.len() != s.core_pcts.len() {
            self.core_hist = (0..s.core_pcts.len()).map(|_| Ring::new(60)).collect();
        }
        for (r, pct) in self.core_hist.iter_mut().zip(s.core_pcts.iter()) {
            r.push(*pct);
        }
        // One ceiling per direction. A shared one keeps the halves comparable,
        // which reads well in a specification and badly on a real machine: disk
        // read runs seven times its write and download twelve times its upload,
        // so the shared scale left the secondary flat on the midline. Both
        // ceilings are labelled on the hero plot, so the asymmetry is stated
        // rather than hidden.
        self.ceil_disk.update(self.hist_disk.max());
        self.ceil_disk_w.update(self.hist_disk_w.max());
        self.ceil_net.update(self.hist_net.max());
        self.ceil_net_tx.update(self.hist_net_tx.max());
        // 0 when nothing is presenting, so the graph flatlines rather than
        // holding the last frame rate of an app that has since closed.
        self.hist_fps.push(s.fps.as_ref().map(|(_, _, f)| *f as f32).unwrap_or(0.0));

        if let Some(name) = self.watch.clone() {
            if !self.snap.procs.is_empty() {
                let a = watch_sums(&self.snap.procs, &name);
                self.watch_rings[0].push(a.cpu);
                self.watch_rings[1].push(a.ram_private as f32);
                self.watch_rings[2].push(a.gpu);
                // 3 and 4 are the first direction, 6 and 7 the second, so the
                // watch charts mirror the same way the main panel's do.
                self.watch_rings[3].push(a.disk_read_bps as f32);
                self.watch_rings[4].push(a.net_rx_bps as f32);
                self.watch_rings[6].push(a.disk_write_bps as f32);
                self.watch_rings[7].push(a.net_tx_bps as f32);
                self.watch_rings[5].push(watch_fps(&self.snap, &name) as f32);
                self.watch_rings[8].push(a.audio * 100.0);
                self.ceil_watch_ram.update(self.watch_rings[1].max());
                self.ceil_watch_disk.update(self.watch_rings[3].max());
                self.ceil_watch_disk_w.update(self.watch_rings[6].max());
                self.ceil_watch_net_tx.update(self.watch_rings[7].max());
                self.ceil_watch_net.update(self.watch_rings[4].max());
            }
        }
        }

        let mut tip = format!(
            "CPU {}   RAM {}\nDownload {}   Upload {}",
            format_pct(s.cpu_pct),
            format_pct(mem_pct),
            format_rate(s.net_rx_bps),
            format_rate(s.net_tx_bps),
        );
        if let Some((_, name, fps)) = &s.fps {
            tip = format!("{}\n{} fps in {}", tip, fps, name);
        }
        let snap = self.snap.clone();
        self.tray.update(&snap, mem_pct, &tip);
        self.update_overlay();
        self.update_widget(mem_pct);

        if self.visible {
            self.relayout(hwnd);
            unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
        }
    }

    /// The window height depends on the drive count (empty until the first
    /// snapshot) and on how many footer buttons are showing, both of which
    /// change while the panel is open. Re-layout when either does — flyout
    /// only, since a pinned window keeps whatever size the user gave it.
    fn relayout(&mut self, hwnd: HWND) {
        if self.cfg.pinned {
            return;
        }
        let footer = self.footer_rows();
        let metrics = self.visible_metric_rows();
        if self.snap.drives.len() == self.layout_drives
            && footer == self.layout_footer
            && metrics == self.layout_metrics
        {
            return;
        }
        let (w, h) = self.outer_size(hwnd);
        unsafe {
            SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER);
        }
        self.layout_drives = self.snap.drives.len();
        self.layout_footer = footer;
        self.layout_metrics = metrics;
    }

    fn update_overlay(&mut self) {
        if !self.cfg.fps_overlay {
            if !self.overlay.is_null() {
                super::overlay::show(self.overlay, false);
            }
            return;
        }
        if self.overlay.is_null() {
            self.overlay = super::overlay::create(self.cfg.fps_x, self.cfg.fps_y, self.scale);
        }
        let fps = self.snap.fps.as_ref().map(|(_, _, f)| *f);
        if let Some((x, y)) =
            super::overlay::update(self.overlay, fps, self.cfg.fps_color, self.cfg.fps_opacity)
        {
            if (x, y) != (self.cfg.fps_x, self.cfg.fps_y) {
                self.cfg.fps_x = x;
                self.cfg.fps_y = y;
                config::save(&self.cfg);
            }
        }
        super::overlay::show(self.overlay, true);
    }

    fn update_widget(&mut self, mem_pct: f32) {
        if !self.cfg.widget_on {
            if !self.widget.is_null() {
                super::widget::show(self.widget, false);
            }
            return;
        }
        if self.widget.is_null() {
            // Sized from its own scale, not the panel's text size, so the strip
            // can be matched to the taskbar independently.
            self.widget = super::widget::create(
                self.cfg.widget_x,
                self.cfg.widget_y,
                self.dpi_scale,
                self.cfg.widget_scale,
            );
        }
        // The AI segment is dark unless the MCP server is on: with nothing
        // able to report, a permanent "—" would look like a fault.
        let (agents, msgs) = if self.cfg.mcp_enabled {
            let now = crate::sampler::unix_ms();
            let live = crate::agents::live_count(&self.shared.agents.lock().unwrap(), now);
            (live as u32, self.mcp_messages.len() as u32)
        } else {
            (0, 0)
        };
        let s = &self.snap;
        let data = super::widget::WidgetData {
            cpu: s.cpu_pct,
            ram: mem_pct,
            gpu: s.gpu_pct,
            gpu_ok: s.gpu_ok,
            rx: s.net_rx_bps,
            tx: s.net_tx_bps,
            dr: s.disk_read_bps,
            dw: s.disk_write_bps,
            fps: s.fps.as_ref().map(|(_, _, f)| *f),
            agents,
            msgs,
            mask: self.cfg.widget_mask,
            theme: self.cfg.widget_theme as usize,
        };
        let changes = super::widget::update(self.widget, data);
        let mut dirty = false;
        if let Some((x, y)) = changes.moved_to {
            if (x, y) != (self.cfg.widget_x, self.cfg.widget_y) {
                self.cfg.widget_x = x;
                self.cfg.widget_y = y;
                dirty = true;
            }
        }
        if let Some(scale) = changes.resized_to {
            let scale = config::clamp_widget_scale(scale);
            if scale != self.cfg.widget_scale {
                self.cfg.widget_scale = scale;
                dirty = true;
            }
        }
        if dirty {
            config::save(&self.cfg);
        }
        super::widget::show(self.widget, true);
    }

    fn tray_click(&mut self, hwnd: HWND) {
        if self.cfg.pinned {
            if self.visible {
                unsafe { SetForegroundWindow(hwnd) };
            } else {
                self.show_panel(hwnd);
            }
        } else if self.visible {
            self.hide_panel(hwnd);
        } else {
            // The panel no longer hides on deactivation, so there is no
            // hide-then-reopen race to debounce here any more.
            self.show_panel(hwnd);
        }
    }

    /// Clicking the desktop notification opens the panel on the message it was
    /// about, already expanded. The balloon truncates at 255 characters, so
    /// following it is only worth doing if it lands on the full text.
    fn balloon_click(&mut self, hwnd: HWND) {
        if self.visible {
            unsafe { SetForegroundWindow(hwnd) };
        } else {
            self.show_panel(hwnd);
        }
        // An alert has no entry in the messages list, so it only raises the
        // panel and leaves the user where they were.
        if let Some(view) = self.balloon_target.take() {
            self.navigate(hwnd, view);
            if matches!(view, View::McpMessages) {
                // The message it was about went to the front of the list.
                // Indices shift as messages arrive, so anything opened earlier
                // no longer means what it did.
                self.msg_expanded.clear();
                self.msg_expanded.insert(0);
            }
        }
    }

    /// How many metric rows the main view will actually draw: the ones the
    /// user left visible. Config guarantees every entry is a metric this
    /// build knows, so no visible entry can fail to draw.
    fn visible_metric_rows(&self) -> usize {
        self.cfg.main_metrics.iter().filter(|(_, on)| *on).count().max(1)
    }

    fn panel_height(&self) -> i32 {
        let drives = self.snap.drives.len().max(1) as i32;
        let temps = 0;
        // Bottom band reserved for the MCP footer buttons: one row each for
        // activity and messages. Always at least one row's worth while the
        // server is on, so the panel does not jump the first time either
        // appears.
        let mcp = if self.cfg.mcp_enabled {
            self.s(30) * self.footer_rows().max(1) as i32
        } else {
            0
        };
        // Metric rows are hideable, so the band is sized from the count the
        // main view will draw — a constant here left dead space per hidden
        // row. Drives are two lines each: 22 for the figures, 20 for the bar.
        let metrics = self.visible_metric_rows() as i32 * self.s(ROW_METRIC);
        self.s(12 + HEADER_STRIDE + 50 + 26) + metrics + drives * self.s(42) + temps + mcp + self.s(12)
    }

    fn outer_size(&self, hwnd: HWND) -> (i32, i32) {
        if self.cfg.pinned && self.cfg.win_w > 100 && self.cfg.win_h > 100 {
            return (self.cfg.win_w, self.cfg.win_h);
        }
        let (cw, ch) = (self.s(PANEL_W), self.panel_height());
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            let mut r = RECT { left: 0, top: 0, right: cw, bottom: ch };
            AdjustWindowRectEx(&mut r, style, 0, ex);
            (r.right - r.left, r.bottom - r.top)
        }
    }

    fn show_panel(&mut self, hwnd: HWND) {
        let (w, h) = self.outer_size(hwnd);
        let mut wa = RECT { left: 0, top: 0, right: 800, bottom: 600 };
        unsafe {
            SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut RECT as *mut _, 0);
        }
        let (x, y) = if self.cfg.pinned {
            if self.cfg.win_x >= 0 {
                (self.cfg.win_x, self.cfg.win_y.max(0))
            } else {
                ((wa.right - w) / 2, (wa.bottom - h) / 2)
            }
        } else if self.cfg.fly_x >= 0 && self.cfg.fly_y >= 0 {
            // Wherever the user last dragged it, held inside the work area in
            // case that shrank — or the display went away — since.
            (
                self.cfg.fly_x.clamp(wa.left, (wa.right - w).max(wa.left)),
                self.cfg.fly_y.clamp(wa.top, (wa.bottom - h).max(wa.top)),
            )
        } else {
            let mut cursor = POINT { x: 0, y: 0 };
            unsafe { GetCursorPos(&mut cursor) };
            (
                (cursor.x - w / 2).clamp(wa.left + 8, (wa.right - w - 8).max(wa.left + 8)),
                (wa.bottom - h - 8).max(wa.top + 8),
            )
        };
        self.visible = true;
        self.layout_drives = self.snap.drives.len();
        self.layout_footer = self.footer_rows();
        self.layout_metrics = self.visible_metric_rows();
        self.shared.panel_open.store(true, Ordering::Relaxed);
        unsafe {
            SetWindowPos(hwnd, self.insert_after(), x, y, w, h, 0);
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
            SetFocus(self.edit);
            InvalidateRect(hwnd, std::ptr::null(), 0);
        }
    }

    fn insert_after(&self) -> HWND {
        // Flyouts are always on top; pinned windows follow the toggle.
        if !self.cfg.pinned || self.cfg.on_top {
            HWND_TOPMOST
        } else {
            HWND_NOTOPMOST
        }
    }

    fn hide_panel(&mut self, hwnd: HWND) {
        if !self.visible {
            return;
        }
        self.visible = false;
        self.shared.panel_open.store(false, Ordering::Relaxed);
        config::save(&self.cfg);
        // Release the back buffer. At 336 px by the window's height this DIB is
        // about a megabyte, and the panel spends most of its life hidden in the
        // tray — holding a megabyte for a window nobody is looking at is the
        // opposite of what this app is for. Caching it across paints is still
        // right while it *is* open: rebuilding per paint meant allocating and
        // freeing that much on every hover change.
        self.bb = None;
        self.fade = None;
        if self.fade_timer {
            unsafe { KillTimer(hwnd, ID_FADE) };
            self.fade_timer = false;
        }
        unsafe { ShowWindow(hwnd, SW_HIDE) };
    }

    fn set_pinned(&mut self, hwnd: HWND, pinned: bool) {
        // Unpinning used to drop the panel down beside the taskbar, away from
        // wherever the user had it. Carrying the current corner across means it
        // unpins in place, and from there it can be dragged.
        if !pinned && self.cfg.pinned {
            let mut r: RECT = unsafe { std::mem::zeroed() };
            unsafe { GetWindowRect(hwnd, &mut r) };
            if r.right > r.left {
                self.cfg.fly_x = r.left.max(0);
                self.cfg.fly_y = r.top.max(0);
            }
        }
        self.cfg.pinned = pinned;
        config::save(&self.cfg);
        unsafe {
            let (style, ex) = if pinned {
                (PINNED_STYLE, WS_EX_APPWINDOW)
            } else {
                (FLYOUT_STYLE, WS_EX_TOOLWINDOW | WS_EX_TOPMOST)
            };
            SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex as isize);
            let (w, h) = self.outer_size(hwnd);
            SetWindowPos(
                hwnd,
                self.insert_after(),
                0,
                0,
                w,
                h,
                SWP_FRAMECHANGED | SWP_NOMOVE,
            );
        }
        if self.visible || pinned {
            self.show_panel(hwnd);
        }
    }

    /// Move forward to `view`, recording where we came from.
    ///
    /// Revisiting a view already on the trail collapses back to it rather than
    /// stacking a second copy. That is what keeps two views that can each open
    /// the other — a watched app and its connection list — from pointing at
    /// each other forever: the trail can only ever shrink back to a view it
    /// already holds, so Back always makes progress.
    fn navigate(&mut self, hwnd: HWND, view: View) {
        self.nav.advance(self.view, view);
        self.change_view(hwnd, view);
    }

    /// Step back one level. An empty trail means we are as far back as the
    /// panel goes, which is the main view.
    fn go_back(&mut self, hwnd: HWND) {
        let back = self.nav.back().unwrap_or(View::Main);
        self.change_view(hwnd, back);
    }

    fn change_view(&mut self, hwnd: HWND, view: View) {
        // Free-text instructions are only written to disk on the way out of
        // settings, rather than on every keystroke.
        // Free-text instructions are written on the way out of the settings
        // area, not on every keystroke. Moving between its pages stays inside.
        let in_settings = |v: &View| matches!(v, View::Settings | View::SettingsPage(_));
        if in_settings(&self.view) && !in_settings(&view) {
            config::save(&self.cfg);
        }
        let was_process = matches!(self.view, View::Process);
        // The connection sweep only runs while its view is on screen. Clear
        // the rows on the way out too, so re-opening shows "collecting"
        // rather than a table of connections that may have closed since.
        let entering_conns = matches!(view, View::Connections);
        if entering_conns != matches!(self.view, View::Connections) {
            self.shared
                .conns_view_open
                .store(entering_conns, Ordering::Relaxed);
            if !entering_conns {
                self.conn_rows.clear();
                self.conn_total = 0;
                self.conns_swept = 0;
                self.conns_for = None;
            }
        }
        self.view = view;
        self.scroll = 0;
        self.max_scroll = 0;
        self.paused = false;
        self.frozen = None;
        self.mcp_copied = false;
        self.subs_expanded = false;
        if matches!(view, View::Settings | View::SettingsPage(_)) {
            self.refresh_autostart();
            self.autostart_err = false;
        }
        // The watch view reuses the shared filter box, so it must not inherit
        // whatever was typed in a drill-down — but that text is only parked,
        // not lost: it comes back on the way out, because a drill filter
        // persists until the user clears it. Both the box and `self.filter`
        // are set explicitly: SetWindowText doesn't reliably raise EN_CHANGE,
        // and a stale `self.filter` would keep filtering behind an empty box.
        let entering_watch = matches!(view, View::Process) && !was_process;
        let leaving_watch = was_process && !matches!(view, View::Process);
        if entering_watch {
            self.watch_saved_filter = get_text(self.edit);
        }
        if matches!(view, View::Process) {
            self.filter.clear();
            set_text(self.edit, "");
        }
        if leaving_watch {
            self.filter = self.watch_saved_filter.to_lowercase();
            let saved = std::mem::take(&mut self.watch_saved_filter);
            set_text(self.edit, &saved);
        }
        if matches!(view, View::Main | View::Drill(_)) {
            unsafe { SetFocus(self.edit) };
        }
        unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
    }

    fn hit_at(&self, x: i32, y: i32) -> Option<usize> {
        self.hits
            .iter()
            .position(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
    }

    /// Whether the cursor is currently inside `r` (drives hover styling).
    fn hovered(&self, r: &RECT) -> bool {
        self.hover_pos
            .map_or(false, |(x, y)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
    }

    fn hover_fill(&self, dc: HDC, r: &RECT, base: u32) {
        gdi::fill(dc, r, if self.hovered(r) { gdi::t().card_hover } else { base });
    }

    /// A card at rest, hovered or pressed. Hover also lifts the border one mix
    /// step toward `text`, so the edge moves with the fill instead of staying
    /// put and reading as a seam.
    fn card(&self, dc: HDC, r: &RECT) {
        let hot = self.hovered(r);
        let down = hot && self.pressed;
        let (base, hover) = (gdi::t().card, gdi::t().card_hover);
        let fill = if down {
            gdi::t().card_press
        } else {
            match self.fade_for(r) {
                // ease_out, so the fill arrives quickly and settles.
                Some((from_hot, t)) => {
                    let e = 1.0 - (1.0 - t) * (1.0 - t);
                    if from_hot {
                        gdi::mix(hover, base, e)
                    } else {
                        gdi::mix(base, hover, e)
                    }
                }
                None if hot => hover,
                None => base,
            }
        };
        let line =
            if hot { gdi::mix(gdi::t().line, gdi::t().text, 0.14) } else { gdi::t().line };
        gdi::card(dc, r, fill, line, self.s(RADIUS));
    }

    /// If `r` is one end of the live cross-fade, whether it is the end being
    /// *left* and how far along the transition is.
    fn fade_for(&self, r: &RECT) -> Option<(bool, f32)> {
        let (from, to, t) = self.fade?;
        let same = |a: &RECT, b: &RECT| {
            a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
        };
        if same(r, &to) {
            Some((false, t))
        } else if same(r, &from) {
            Some((true, t))
        } else {
            None
        }
    }

    /// Metric glyph plus its name, both in the metric's accent, centred on the
    /// card's midline — the same line the value sits on. Returns the right edge
    /// of the ink, which is the budget `draw_figures` measures against.
    fn metric_name(
        &self,
        dc: HDC,
        card: &RECT,
        accent: u32,
        glyph: gdi::Glyph,
        label: &str,
    ) -> i32 {
        let mid = (card.top + card.bottom) / 2;
        let g = self.s(GLYPH);
        let x = card.left + self.s(SP4);
        gdi::metric_icon(dc, x + g / 2, mid, g, self.s(1).max(1), accent, glyph);
        let caps = label.to_uppercase();
        let track = self.s(gdi::TRACK_LABEL);
        let tx = x + g + self.s(SP2);
        gdi::text_t(
            dc,
            tx,
            gdi::centre_y_caps(dc, self.fonts.label, mid),
            self.fonts.label,
            track,
            accent,
            &caps,
        );
        tx + gdi::text_width_t(dc, self.fonts.label, track, &caps)
    }

    /// Width a unit occupies to the right of a figure, gap included.
    fn unit_w(&self, dc: HDC, u: Unit) -> i32 {
        match u {
            Unit::None => 0,
            Unit::Word(w) => {
                self.s(SP2) + gdi::text_width_t(dc, self.fonts.micro, self.s(gdi::TRACK_MICRO), w)
            }
            Unit::Down | Unit::Up => self.s(SP2) + self.s(MARKER),
        }
    }

    /// Draw a direction marker centred on `mid`, its left edge at `x`.
    fn draw_marker(&self, dc: HDC, x: i32, mid: i32, size: i32, u: Unit) {
        if u != Unit::Down && u != Unit::Up {
            return;
        }
        let th = (size / 8).max(1);
        gdi::arrow(dc, x + size / 2, mid, size, th, gdi::t().mute, u == Unit::Down);
    }

    /// A metric row's right-hand figures, per §2 of the UI foundation.
    ///
    /// Two rules, and the second is the one that matters: the primary value is
    /// baseline-centred on the card's midline **in every row**, and the
    /// secondary figure is taken out of the vertical flow and hung off the
    /// card's bottom edge. Stacking the pair and centring it — the obvious
    /// layout — puts the value ~5 px high in rows that have a secondary and
    /// exactly on the midline in rows that don't, so the value column zigzags
    /// down the panel and no value lines up with its own metric name.
    ///
    /// `name_right` is the right edge of the metric name's ink, and `right` the
    /// right edge of the figure column. Between them is the band the value has
    /// to fit in; when it will not, the value steps down its font ladder, and if
    /// even the smallest step will not clear, the name ellipsises. A value and a
    /// name must never be able to overlap.
    fn draw_figures(&self, dc: HDC, card: &RECT, name_right: i32, right: i32, f: &Figures) {
        let mid = (card.top + card.bottom) / 2;
        let uw = self.unit_w(dc, f.unit);
        let budget = right - uw - name_right - self.s(SP3);
        let stack = self.fonts.fit_stack();
        let mut font = *stack.last().expect("fit_stack is never empty");
        for &cand in stack.iter() {
            if gdi::text_width(dc, cand, &f.value) <= budget {
                font = cand;
                break;
            }
        }

        let vx = right - uw;
        let vy = gdi::centre_y(dc, font, mid);
        gdi::text_right(dc, vx, vy, font, gdi::t().text, &f.value);

        match f.unit {
            Unit::None => {}
            Unit::Word(w) => {
                // Share the value's baseline rather than its box: the two steps
                // differ by 4 px of cell, so box-aligning them sits the unit low.
                let (vasc, _, _) = gdi::text_metrics(dc, font);
                let (masc, _, _) = gdi::text_metrics(dc, self.fonts.micro);
                gdi::text_t(
                    dc,
                    vx + self.s(SP2),
                    vy + vasc - masc,
                    self.fonts.micro,
                    self.s(gdi::TRACK_MICRO),
                    gdi::t().mute,
                    w,
                );
            }
            u => self.draw_marker(dc, vx + self.s(SP2), mid, self.s(MARKER), u),
        }

        let Some(sub) = &f.sub else { return };
        let suw = self.unit_w(dc, f.sub_unit);
        let sy = gdi::bottom_y(dc, self.fonts.micro, card.bottom, self.s(SP1));
        // A number is a number: a secondary figure reads in `text`, the same ink
        // as the value above it, and is separated from it by size alone. `dim`
        // was the first attempt at this and was still reported as too faint —
        // which it was, because the row's own value sits beside it in `text` and
        // gives the eye an immediate comparison.
        let sub_ink = if f.sub_is_figure { gdi::t().text } else { gdi::t().mute };
        gdi::text_right_t(
            dc,
            right - suw,
            sy,
            self.fonts.micro,
            self.s(gdi::TRACK_MICRO),
            sub_ink,
            sub,
        );
        match f.sub_unit {
            Unit::None => {}
            // The noun after a secondary figure stays `mute` while the figure
            // itself is `text`: `290 KB/s` has to be readable, `write` does not.
            // This is the rule the whole row follows — figures in `text`, the
            // units and nouns naming them in `mute` — so the primary value's
            // `used`/`read` and the direction markers stay quiet too.
            Unit::Word(w) => {
                gdi::text_t(
                    dc,
                    right - suw + self.s(SP2),
                    sy,
                    self.fonts.micro,
                    self.s(gdi::TRACK_MICRO),
                    gdi::t().mute,
                    w,
                );
            }
            u => {
                let smid = gdi::centre_y(dc, self.fonts.micro, 0);
                // centre_y(…, 0) is the offset from a midline to the cell top,
                // so subtracting it from sy recovers this run's midline.
                self.draw_marker(dc, right - suw + self.s(SP2), sy - smid, self.s(MARKER_SM), u);
            }
        }
    }

    /// A unit beside a figure that is *not* in a metric card: a word in
    /// `micro`/`mute` sharing the figure's baseline, or a direction marker on
    /// the figure's midline. Same two rules `draw_figures` applies, factored out
    /// so the list rows cannot drift from the cards — a word and a marker have
    /// to sit differently, and getting that wrong is invisible until it is a
    /// unit floating a pixel above its number.
    ///
    /// `y` is the cell top of the run the unit follows, and `x` its left edge.
    fn draw_unit_after(&self, dc: HDC, x: i32, y: i32, font: HFONT, u: Unit) {
        match u {
            Unit::None => {}
            Unit::Word(w) => {
                // Share the figure's baseline, not its box: the two steps
                // differ by several pixels of cell, so box-aligning them sits
                // the unit low.
                let (vasc, _, _) = gdi::text_metrics(dc, font);
                let (masc, _, _) = gdi::text_metrics(dc, self.fonts.micro);
                gdi::text_t(
                    dc,
                    x,
                    y + vasc - masc,
                    self.fonts.micro,
                    self.s(gdi::TRACK_MICRO),
                    gdi::t().mute,
                    w,
                );
            }
            u => {
                // centre_y(…, 0) is the offset from a midline to the cell top,
                // so subtracting it from `y` recovers this run's midline.
                let mid = y - gdi::centre_y(dc, font, 0);
                self.draw_marker(dc, x, mid, self.s(MARKER_SM), u);
            }
        }
    }

    /// Show the next queued balloon, or stop the pacing timer once the queue
    /// is empty. The click target is set from the balloon actually going on
    /// screen rather than from whatever happened to be queued last, which is
    /// what made clicking a notification open the wrong view.
    fn next_balloon(&mut self, hwnd: HWND) {
        let Some((title, message, from_mcp)) = self.balloon_queue.pop_front() else {
            unsafe { KillTimer(hwnd, ID_BALLOON) };
            self.balloon_timer = false;
            return;
        };
        self.tray.balloon(&title, &message);
        self.balloon_target = if from_mcp { Some(View::McpMessages) } else { None };
    }

    fn refresh_autostart(&mut self) {
        self.autostart_on = crate::autostart::is_installed();
    }

    fn sync_rules(&mut self) {
        config::save(&self.cfg);
        *self.shared.rules.lock().unwrap() = rules::parse_all(&self.cfg.rule_lines);
    }

    /// Persist and republish the AI-facing instructions. Connected clients
    /// only see the new text when they reconnect or call `notify_rules`, so
    /// the settings screen says as much.
    fn sync_ai_instructions(&mut self) {
        config::save(&self.cfg);
        *self.shared.ai_instructions.lock().unwrap() = self.cfg.ai_instructions();
        *self.shared.agent_log_file.lock().unwrap() = self.cfg.agent_log_file.clone();
    }

    fn click(&mut self, hwnd: HWND, x: i32, y: i32) {
        let action = self
            .hits
            .iter()
            .find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
            .map(|(_, a)| *a);
        let Some(a) = action else {
            // Empty space on the unpinned flyout drags the window. It has no
            // title bar to grab, so without this it could only ever appear
            // where it chose to, never where the user wanted it.
            if !self.cfg.pinned {
                unsafe {
                    ReleaseCapture();
                    SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION as usize, 0);
                }
            }
            return;
        };
        match a {
            // Hover-only; clicking a core does nothing. Per-core *usage* per
            // process would need ETW context-switch tracing, so there is no
            // honest drill-down to offer here.
            Action::HoverCore => {}
            Action::Drill(m) => {
                // Keep any entered filter text — it persists until cleared.
                self.navigate(hwnd, View::Drill(m));
            }
            Action::Back => {
                self.go_back(hwnd);
            }
            Action::ShowConns => {
                self.conns_for = None;
                self.navigate(hwnd, View::Connections);
            }
            Action::ShowAppConns => {
                self.conns_for = self.watch.clone();
                self.navigate(hwnd, View::Connections);
            }
            Action::OpenSettings => self.navigate(hwnd, View::Settings),
            Action::OpenSettingsPage(p) => self.navigate(hwnd, View::SettingsPage(p)),
            Action::TogglePin => {
                self.set_pinned(hwnd, !self.cfg.pinned);
            }
            // These three only change what is drawn, so without an explicit
            // invalidate the panel kept the old frame until the next sampler
            // tick — up to `interval_ms` of nothing happening after a click.
            Action::SubMetric(m) => {
                self.sub_metric = m;
                self.scroll = 0;
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::ToggleMsg(i) => {
                if !self.msg_expanded.remove(&i) {
                    self.msg_expanded.insert(i);
                }
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::ToggleAgent(k) => {
                if !self.agent_expanded.remove(&k) {
                    self.agent_expanded.insert(k);
                }
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::ClosePanel => {
                self.hide_panel(hwnd);
            }
            Action::NotifyPreset(bit) => {
                self.cfg.notify_presets ^= bit;
                self.sync_ai_instructions();
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::NotifyCustomAdd => {
                let text = get_text_long(self.edit_notify).trim().to_string();
                if !text.is_empty() {
                    self.cfg.notify_custom.push((true, text));
                    set_text(self.edit_notify, "");
                    self.sync_ai_instructions();
                }
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::NotifyCustomToggle(i) => {
                if let Some(entry) = self.cfg.notify_custom.get_mut(i) {
                    entry.0 = !entry.0;
                    self.sync_ai_instructions();
                }
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::NotifyCustomDelete(i) => {
                if i < self.cfg.notify_custom.len() {
                    self.cfg.notify_custom.remove(i);
                    self.sync_ai_instructions();
                }
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::ToggleTop => {
                self.cfg.on_top = !self.cfg.on_top;
                config::save(&self.cfg);
                unsafe {
                    SetWindowPos(hwnd, self.insert_after(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
            }
            Action::ToggleTray(kind) => {
                let f = match kind {
                    0 => &mut self.cfg.tray_static,
                    1 => &mut self.cfg.tray_cpu,
                    2 => &mut self.cfg.tray_ram,
                    3 => &mut self.cfg.tray_disk,
                    4 => &mut self.cfg.tray_net,
                    _ => &mut self.cfg.tray_fps,
                };
                *f = !*f;
                if self.cfg.no_tray_icon() {
                    self.cfg.tray_static = true; // keep the app reachable
                }
                config::save(&self.cfg);
                let cfg = self.cfg.clone();
                self.tray.sync(&cfg);
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::SetInterval(ms) => {
                self.cfg.interval_ms = ms;
                config::save(&self.cfg);
                self.shared.interval_ms.store(ms, Ordering::Relaxed);
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::Kill(idx) => {
                let row = self.drawn_rows.get(idx).cloned();
                if let Some((name, pids)) = row {
                    self.confirm_kill(hwnd, &name, &pids);
                }
            }
            Action::Watch(idx) => {
                if let Some((name, _)) = self.drawn_rows.get(idx).cloned() {
                    self.watch = Some(name);
                    for r in &mut self.watch_rings {
                        *r = Ring::new(60);
                    }
                    self.navigate(hwnd, View::Process);
                }
            }
            Action::KillWatched => {
                if let Some(name) = self.watch.clone() {
                    let pids: Vec<u32> = self
                        .snap
                        .procs
                        .iter()
                        .filter(|p| p.name == name)
                        .map(|p| p.pid)
                        .collect();
                    self.confirm_kill(hwnd, &name, &pids);
                }
            }
            Action::ToggleSubs => {
                self.subs_expanded = !self.subs_expanded;
                self.scroll = 0;
                self.scroll = 0;
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::KillPid(pid) => {
                let name = self.watch.clone().unwrap_or_default();
                self.confirm_kill(hwnd, &name, &[pid]);
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::ToggleMcp => {
                self.cfg.mcp_enabled = !self.cfg.mcp_enabled;
                config::save(&self.cfg);
                self.shared.mcp_enabled.store(self.cfg.mcp_enabled, Ordering::Relaxed);
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::ToggleRule(idx) => {
                if let Some(line) = self.cfg.rule_lines.get(idx).cloned() {
                    let enabled = rules::parse_line(&line).map(|r| r.enabled).unwrap_or(true);
                    self.cfg.rule_lines[idx] = rules::set_enabled(&line, !enabled);
                    self.sync_rules();
                    unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
                }
            }
            Action::DeleteRule(idx) => {
                if idx < self.cfg.rule_lines.len() {
                    self.cfg.rule_lines.remove(idx);
                    self.sync_rules();
                    unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
                }
            }
            Action::MetricVisible(i) => {
                if let Some(entry) = self.cfg.main_metrics.get_mut(i) {
                    entry.1 = !entry.1;
                }
                // Never let the panel empty out completely: with nothing shown
                // it reads as a broken app rather than a configured one.
                if !self.cfg.main_metrics.iter().any(|(_, on)| *on) {
                    if let Some(entry) = self.cfg.main_metrics.get_mut(i) {
                        entry.1 = true;
                    }
                }
                config::save(&self.cfg);
                // The flyout is sized from the visible rows, so it must
                // follow the toggle rather than wait for the next snapshot.
                self.relayout(hwnd);
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::MetricDragStart(i) => {
                self.metric_drag = Some(i);
                unsafe {
                    SetCapture(hwnd);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
            }
            Action::AddRule => {
                self.draft = RuleDraft::default();
                set_text(self.edit, "");
                set_text(self.edit_val, "90");
                let default_path = std::env::var("LOCALAPPDATA")
                    .map(|p| format!("{}\\resmon-alerts.log", p))
                    .unwrap_or_else(|_| "C:\\resmon-alerts.log".to_string());
                set_text(self.edit_path, &default_path);
                self.navigate(hwnd, View::RuleEdit);
            }
            Action::DraftMetric(i) => {
                self.draft.metric = i;
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::DraftDir(gt) => {
                self.draft.gt = gt;
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::DraftProcSub(i) => {
                self.draft.proc_sub = i;
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::DraftConnField(i) => {
                self.draft.conn_field = i;
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::DraftDeliver(i) => {
                self.draft.deliver = i;
                // The path row appears or disappears, so the EDIT children
                // must be repositioned.
                self.edit_sig = (u32::MAX, u32::MAX, false, false, 0, 0, 0);
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::DraftTop => {
                self.draft.top = !self.draft.top;
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::DraftCooldown => {
                self.draft.cooldown = (self.draft.cooldown + 1) % R_COOLDOWNS.len();
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::DraftSave => {
                if self.save_draft() {
                    self.navigate(hwnd, View::SettingsPage(SettingsPage::Alerts));
                } else {
                    unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
                }
            }
            Action::DraftCancel => self.navigate(hwnd, View::SettingsPage(SettingsPage::Alerts)),
            Action::PickApp => self.pick_app_menu(hwnd),
            Action::ShowFpsApps => self.navigate(hwnd, View::FpsApps),
            Action::PinHero => {
                // Turn the click's x back into a sample. Clicking the sample
                // that is already pinned unpins it, so the same gesture undoes
                // itself rather than needing somewhere else to click.
                let hero = match self.view {
                    View::Drill(m) => Hero::M(m),
                    View::FpsApps => Hero::Fps,
                    _ => return,
                };
                let (cap, held) = {
                    let (ring, _, _, _) = self.hero_series(hero);
                    (ring.capacity(), ring.iter().count())
                };
                let plot = self.hero_plot;
                let idx = self
                    .hover_pos
                    .and_then(|(hx, _)| gdi::chart_hit(&plot, cap, held, hx));
                self.pin_back = match idx {
                    Some(i) => {
                        let back = held.saturating_sub(1).saturating_sub(i);
                        if self.pin_back == Some(back) { None } else { Some(back) }
                    }
                    None => None,
                };
            }
            Action::TogglePause => {
                self.paused = !self.paused;
                self.frozen = if self.paused { Some(self.snap.clone()) } else { None };
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::ShowMcp => self.navigate(hwnd, View::McpMessages),
            Action::ShowActivity => self.navigate(hwnd, View::Activity),
            Action::ClearAgents => {
                // Live only. History is cleared from its own chip, so wiping
                // the current list cannot take the record with it.
                for s in self.shared.agents.lock().unwrap().iter_mut() {
                    s.agents.clear();
                }
                self.agent_expanded.clear();
                if self.shared.agent_history.lock().unwrap().is_empty() {
                    self.navigate(hwnd, View::Main);
                } else {
                    unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
                }
            }
            Action::ClearHistory => {
                self.shared.agent_history.lock().unwrap().clear();
                self.agent_expanded.clear();
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::ToggleAgentLog => {
                // A default path rather than an editor: the value is a whole
                // file path, and the one place it sensibly lives is beside
                // the settings file.
                self.cfg.agent_log_file = if self.cfg.agent_log_file.trim().is_empty() {
                    std::env::var("LOCALAPPDATA")
                        .map(|p| format!("{}\\resmon-agents.log", p))
                        .unwrap_or_else(|_| "C:\\resmon-agents.log".to_string())
                } else {
                    String::new()
                };
                self.sync_ai_instructions();
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::ToggleFinished => {
                self.finished_expanded = !self.finished_expanded;
                self.scroll = 0;
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::ClearMcp => {
                self.mcp_messages.clear();
                self.navigate(hwnd, View::Main);
            }
            Action::CopyMcpCmd => {
                copy_to_clipboard(hwnd, &mcp_connect_cmd());
                self.mcp_copied = true;
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::ClearFilter => {
                // Clear state directly — SetWindowText doesn't reliably raise
                // EN_CHANGE, so don't depend on it to reset the filter.
                self.filter.clear();
                self.scroll = 0;
                set_text(self.edit, "");
                unsafe {
                    SetFocus(self.edit);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
            }
            Action::SetTheme(i) => {
                self.cfg.theme = i as u32;
                gdi::set_theme(i);
                config::save(&self.cfg);
                unsafe {
                    // The EDIT background brush must match the new theme.
                    windows_sys::Win32::Graphics::Gdi::DeleteObject(self.edit_brush as _);
                }
                self.edit_brush = unsafe { CreateSolidBrush(gdi::t().input_bg) };
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 1) };
            }
            Action::SetTextSize(i) => {
                self.cfg.text_size = i as u32;
                config::save(&self.cfg);
                self.scale = self.dpi_scale * config::text_scale(self.cfg.text_size);
                // Fonts are sized once, at creation, so a new scale means a
                // new set — and the old set has to go with it.
                self.fonts.destroy();
                self.fonts = Fonts::new(self.scale);
                for e in [self.edit, self.edit_val, self.edit_path, self.edit_notify] {
                    unsafe { SendMessageW(e, WM_SETFONT, self.fonts.body as usize, 1) };
                }
                // Every layout constant is in scaled units, so the window
                // itself has to resize or the new text has nowhere to go.
                if !self.cfg.pinned {
                    let (w, h) = self.outer_size(hwnd);
                    unsafe {
                        SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER);
                    }
                }
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 1) };
            }
            Action::ToggleWidget => {
                self.cfg.widget_on = !self.cfg.widget_on;
                config::save(&self.cfg);
                let mem_pct = if self.snap.mem_total > 0 {
                    self.snap.mem_used as f32 / self.snap.mem_total as f32 * 100.0
                } else {
                    0.0
                };
                self.update_widget(mem_pct);
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::SetWidgetTheme(i) => {
                self.cfg.widget_theme = i as u32;
                config::save(&self.cfg);
                // WidgetData carries the theme index on every tick, so this
                // just needs one early tick rather than touching the widget
                // window directly.
                let mem_pct = if self.snap.mem_total > 0 {
                    self.snap.mem_used as f32 / self.snap.mem_total as f32 * 100.0
                } else {
                    0.0
                };
                self.update_widget(mem_pct);
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::WidgetMetric(bit) => {
                self.cfg.widget_mask ^= bit;
                if self.cfg.widget_mask == 0 {
                    self.cfg.widget_mask = bit; // keep at least one metric
                }
                config::save(&self.cfg);
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::SnapWidget => {
                if !self.widget.is_null() {
                    if let Some((x, y)) =
                        super::widget::snap_position(self.widget, self.cfg.widget_mask)
                    {
                        self.cfg.widget_x = x;
                        self.cfg.widget_y = y;
                        config::save(&self.cfg);
                        unsafe {
                            SetWindowPos(
                                self.widget,
                                std::ptr::null_mut(),
                                x,
                                y,
                                0,
                                0,
                                SWP_NOSIZE | SWP_NOZORDER,
                            );
                        }
                    }
                }
            }
            Action::ResetWidgetSize => {
                self.cfg.widget_scale = 1.0;
                config::save(&self.cfg);
                super::widget::set_scale(self.widget, self.dpi_scale, self.cfg.widget_scale);
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::ToggleAutostart => {
                let want_on = !self.autostart_on;
                let ok = if want_on {
                    crate::autostart::install()
                } else {
                    crate::autostart::uninstall()
                };
                self.refresh_autostart();
                self.autostart_err = !ok || self.autostart_on != want_on;
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::ToggleFpsOverlay => {
                self.cfg.fps_overlay = !self.cfg.fps_overlay;
                config::save(&self.cfg);
                self.update_overlay();
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::FpsColor(i) => {
                self.cfg.fps_color = i as u32;
                config::save(&self.cfg);
                self.update_overlay();
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            Action::FpsOpacity(i) => {
                self.cfg.fps_opacity = i as u32;
                config::save(&self.cfg);
                self.update_overlay();
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
        }
    }

    fn save_draft(&mut self) -> bool {
        let (key, _, _) = R_METRICS[self.draft.metric];
        // Delivery is decided the same way for both kinds of rule.
        let wants_file = self.draft.deliver != DELIVER_DESKTOP;
        let path = if wants_file { get_text(self.edit_path).trim().to_string() } else { String::new() };
        let notify = self.draft.deliver != DELIVER_FILE;
        if wants_file && path.is_empty() {
            return false;
        }
        if key == "conn" {
            let pattern = get_text(self.edit).trim().to_string();
            if pattern.is_empty() {
                return false;
            }
            let line = rules::build_conn_line(
                R_CONN_FIELDS[self.draft.conn_field].0,
                &pattern,
                &path,
                notify,
                self.draft.top,
                R_COOLDOWNS[self.draft.cooldown].0,
            );
            // A port that is not a number, say, is rejected here rather than
            // saved as a rule that could never fire.
            if rules::parse_line(&line).is_none() {
                return false;
            }
            self.cfg.rule_lines.push(line);
            self.sync_rules();
            set_text(self.edit, "");
            return true;
        }
        let metric = if key == "proc" {
            let name = get_text(self.edit).trim().to_string();
            if name.is_empty() {
                return false;
            }
            format!("proc:{}:{}", name, R_PROC_SUBS[self.draft.proc_sub].0)
        } else {
            key.to_string()
        };
        let Ok(threshold) = get_text(self.edit_val).trim().parse::<f64>() else {
            return false;
        };
        let line = rules::build_line(
            &metric,
            self.draft.gt,
            threshold,
            &path,
            notify,
            self.draft.top,
            R_COOLDOWNS[self.draft.cooldown].0,
        );
        if rules::parse_line(&line).is_none() {
            return false;
        }
        self.cfg.rule_lines.push(line);
        self.sync_rules();
        set_text(self.edit, "");
        true
    }

    fn confirm_kill(&mut self, hwnd: HWND, name: &str, pids: &[u32]) {
        if pids.is_empty() {
            return;
        }
        let text = format!("End {} process(es) named \"{}\"?", pids.len(), name);
        let answer = unsafe {
            MessageBoxW(
                hwnd,
                gdi::wide(&text).as_ptr(),
                gdi::wide("Resource Monitor — Close app").as_ptr(),
                MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
            )
        };
        if answer != IDYES {
            return;
        }
        for &pid in pids {
            unsafe {
                let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if !h.is_null() {
                    TerminateProcess(h, 1);
                    windows_sys::Win32::Foundation::CloseHandle(h);
                }
            }
        }
    }

    /// Native dropdown of currently running apps; the choice fills the field.
    fn pick_app_menu(&mut self, hwnd: HWND) {
        // Unique app names by memory, most first, so common apps are near the top.
        let mut seen = std::collections::HashSet::new();
        let mut apps: Vec<(&str, u64)> = Vec::new();
        for p in &self.snap.procs {
            if seen.insert(p.name.as_str()) {
                let total: u64 = self.snap.procs.iter().filter(|q| q.name == p.name).map(|q| q.ws_private).sum();
                apps.push((p.name.as_str(), total));
            }
        }
        apps.sort_by(|a, b| b.1.cmp(&a.1));
        apps.truncate(40);
        if apps.is_empty() {
            return;
        }
        unsafe {
            let menu = CreatePopupMenu();
            for (i, (name, _)) in apps.iter().enumerate() {
                AppendMenuW(menu, MF_STRING, (i + 1) as usize, gdi::wide(name).as_ptr());
            }
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                pt.x,
                pt.y,
                0,
                hwnd,
                std::ptr::null(),
            ) as usize;
            DestroyMenu(menu);
            if cmd >= 1 && cmd <= apps.len() {
                set_text(self.edit, apps[cmd - 1].0);
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
        }
    }

    fn tray_menu(&mut self, hwnd: HWND) {
        unsafe {
            let menu = CreatePopupMenu();
            let installed = crate::autostart::is_installed();
            let check = if installed { MF_CHECKED } else { 0 };
            AppendMenuW(menu, MF_STRING, IDM_SETTINGS as usize, gdi::wide("Settings").as_ptr());
            AppendMenuW(
                menu,
                MF_STRING | check,
                IDM_AUTOSTART as usize,
                gdi::wide("Start with Windows").as_ptr(),
            );
            AppendMenuW(menu, MF_STRING, IDM_EXIT as usize, gdi::wide("Exit").as_ptr());
            let mut p = POINT { x: 0, y: 0 };
            GetCursorPos(&mut p);
            SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                p.x,
                p.y,
                0,
                hwnd,
                std::ptr::null(),
            ) as u32;
            DestroyMenu(menu);
            match cmd {
                IDM_EXIT => {
                    DestroyWindow(hwnd);
                }
                IDM_AUTOSTART => {
                    if installed {
                        crate::autostart::uninstall();
                    } else {
                        crate::autostart::install();
                    }
                }
                IDM_SETTINGS => {
                    self.refresh_autostart();
                    self.autostart_err = false;
                    self.view = View::Settings;
                    if !self.visible {
                        self.show_panel(hwnd);
                    } else {
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
                _ => {}
            }
        }
    }

    // ----------------------------------------------------------- painting

    /// Position/show the EDIT children to match the current view's layout.
    /// Idempotent: only acts when the view or client size changes — hiding
    /// and re-showing every paint would steal focus from the inputs.
    fn update_edit(&mut self, rc: &RECT) {
        // Both the metric and the connection field change which boxes the
        // rule editor shows, so both belong in the signature.
        let draft_shape = if matches!(self.view, View::RuleEdit) {
            (self.draft.metric as u32) * 16 + self.draft.conn_field as u32
        } else {
            0
        };
        let code = match self.view {
            View::Main => 0,
            View::Drill(_) => 1,
            View::Settings => 2,
            View::SettingsPage(p) => 100 + p as u32,
            View::RuleEdit => 3,
            View::Process => 4,
            View::FpsApps => 5,
            View::McpMessages => 6,
            View::Activity => 7,
            View::Connections => 8,
        };
        // subs_expanded matters because the watch view's filter box only
        // exists while the subprocess list is open; the FPS row because it
        // shifts everything below it by a row when it appears or decays away.
        // `code` cannot tell one drill-down from another, but the network one
        // carries a Connections row that pushes its filter box down. Keying on
        // the y we are about to position the child at catches that, and any
        // future layout change that moves the box, without another special case.
        let sig = (
            code,
            draft_shape,
            self.subs_expanded,
            self.watch_has_fps(),
            rc.right,
            rc.bottom,
            self.filter_input_y(),
        );
        if sig == self.edit_sig {
            return;
        }
        self.edit_sig = sig;
        match self.view {
            View::Main => set_cue(self.edit, "Search for an app…"),
            View::Drill(_) => set_cue(self.edit, "Filter this list…"),
            View::Connections => set_cue(self.edit, "Filter by app, host, IP or port…"),
            // The pattern box shares the app-name box, so the hint has to say
            // which of the two it is asking for right now.
            View::RuleEdit => {
                let cue = if R_METRICS[self.draft.metric].0 == "conn" {
                    match R_CONN_FIELDS[self.draft.conn_field].0 {
                        rules::ConnField::Host => "Host name, for example *.asus.com",
                        rules::ConnField::Ip => "Address or prefix, for example 204.79.",
                        rules::ConnField::Port => "Port number, for example 445",
                        rules::ConnField::Process => "App name, for example mscopilot.exe",
                    }
                } else {
                    "App name, for example chrome.exe"
                };
                set_cue(self.edit, cue);
            }
            View::Process => set_cue(self.edit, "Filter processes…"),
            _ => {}
        }
        let pad = self.s(12);
        unsafe {
            ShowWindow(self.edit, SW_HIDE);
            ShowWindow(self.edit_val, SW_HIDE);
            ShowWindow(self.edit_path, SW_HIDE);
            match self.view {
                View::Main => {
                    // Must agree with the merged header in `draw_main`: the
                    // field starts at the gutter and the text begins after the
                    // magnifier, and its right edge stops short of the clear ×.
                    let text_left = pad + self.s(SP3) + self.s(GLYPH) + self.s(SP2);
                    let controls = self.header_controls_w();
                    SetWindowPos(
                        self.edit,
                        std::ptr::null_mut(),
                        text_left,
                        pad + self.s(3),
                        rc.right - pad - controls - text_left - self.s(20),
                        self.ctrl_h() - 2 * self.s(3),
                        SWP_NOZORDER,
                    );
                    ShowWindow(self.edit, SW_SHOW);
                    SetFocus(self.edit);
                }
                View::Drill(_) | View::Connections => {
                    // Width leaves room for the clear × and the pause button.
                    SetWindowPos(
                        self.edit,
                        std::ptr::null_mut(),
                        pad + self.s(SP3) + self.s(GLYPH) + self.s(SP2),
                        self.filter_input_y(),
                        rc.right
                            - 2 * pad
                            - self.s(SP3)
                            - self.s(GLYPH)
                            - self.s(SP2)
                            - self.s(20)
                            - self.ctrl_h(),
                        self.s(18),
                        SWP_NOZORDER,
                    );
                    ShowWindow(self.edit, SW_SHOW);
                    SetFocus(self.edit);
                }
                View::RuleEdit => {
                    let l = self.rule_edit_layout();
                    if l.proc {
                        // Narrower than the frame to leave room for the "pick" button.
                        SetWindowPos(
                            self.edit,
                            std::ptr::null_mut(),
                            pad + self.s(70),
                            l.y_name,
                            rc.right - 2 * pad - self.s(74) - self.s(52),
                            self.s(18),
                            SWP_NOZORDER,
                        );
                        ShowWindow(self.edit, SW_SHOW);
                    }
                    if l.conn {
                        // The pattern box: full width, since a connection rule
                        // has no "pick" button beside it.
                        SetWindowPos(
                            self.edit,
                            std::ptr::null_mut(),
                            pad + self.s(70),
                            l.y_name,
                            rc.right - 2 * pad - self.s(74),
                            self.s(18),
                            SWP_NOZORDER,
                        );
                        ShowWindow(self.edit, SW_SHOW);
                    }
                    // The threshold box belongs to threshold rules only.
                    if !l.conn {
                        SetWindowPos(
                            self.edit_val,
                            std::ptr::null_mut(),
                            pad + self.s(70),
                            l.y_thresh,
                            self.s(70),
                            self.s(18),
                            SWP_NOZORDER,
                        );
                        ShowWindow(self.edit_val, SW_SHOW);
                    }
                    if l.file {
                        SetWindowPos(
                            self.edit_path,
                            std::ptr::null_mut(),
                            pad + self.s(70),
                            l.y_file,
                            rc.right - 2 * pad - self.s(74),
                            self.s(18),
                            SWP_NOZORDER,
                        );
                        ShowWindow(self.edit_path, SW_SHOW);
                    }
                }
                View::Process if self.subs_expanded => {
                    let l = self.proc_layout();
                    SetWindowPos(
                        self.edit,
                        std::ptr::null_mut(),
                        pad + self.s(6),
                        l.y_filter,
                        rc.right - 2 * pad - self.s(10) - self.s(20),
                        self.s(18),
                        SWP_NOZORDER,
                    );
                    ShowWindow(self.edit, SW_SHOW);
                    SetFocus(self.edit);
                }
                _ => {}
            }
        }
    }

    fn paint(&mut self, hwnd: HWND) {
        let rc_copy;
        unsafe {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            rc_copy = rc;
            self.notify_text_y = -1;
            self.update_edit(&rc);
            // Reuse the cached buffer unless the window changed size. Taken out
            // of `self` for the duration so the draw calls can hold `&mut self`.
            let need = (rc.right.max(1), rc.bottom.max(1));
            let bb = self
                .bb
                .take()
                .filter(|b| b.size() == need)
                .unwrap_or_else(|| BackBuffer::new(hdc, need.0, need.1));
            self.surf = bb.surface();
            gdi::fill(bb.dc, &rc, gdi::t().bg);
            self.hits.clear();
            match self.view {
                View::Main => self.draw_main(bb.dc, &rc),
                View::Drill(m) => self.draw_drill(bb.dc, &rc, m),
                View::Settings => self.draw_settings(bb.dc, &rc),
                View::RuleEdit => self.draw_rule_edit(bb.dc, &rc),
                View::Process => self.draw_process(bb.dc, &rc),
                View::FpsApps => self.draw_fps_apps(bb.dc, &rc),
                View::Connections => self.draw_conns(bb.dc, &rc),
                View::McpMessages => self.draw_mcp_messages(bb.dc, &rc),
                View::Activity => self.draw_activity(bb.dc, &rc),
                View::SettingsPage(p) => self.draw_settings_page(bb.dc, &rc, p),
            }
            // Dropped before the buffer is, so no chart can hold a pointer
            // into a DIB that has been deleted.
            self.surf = None;
            bb.present(hdc);
            self.bb = Some(bb);
            EndPaint(hwnd, &ps);
        }
        self.place_notify_edit(&rc_copy);
    }

    /// Move the free-text notify box to wherever this frame's paint put it.
    /// The settings list scrolls, so the position is only known after
    /// `draw_settings` has run — hence positioning after the paint rather
    /// than in `update_edit` like the fixed-position boxes.
    fn place_notify_edit(&mut self, rc: &RECT) {
        let pad = self.s(12);
        let top = self.notify_text_y;
        let content_top = pad + self.header_height();
        // Hidden when not in settings, or scrolled under the fixed header /
        // past the bottom edge — a child window would otherwise draw over
        // the header, which is painted into the back buffer beneath it.
        let visible = matches!(self.view, View::SettingsPage(SettingsPage::Ai))
            && self.cfg.mcp_enabled
            && top >= content_top
            && top + self.ctrl_h() <= rc.bottom;
        unsafe {
            if !visible {
                ShowWindow(self.edit_notify, SW_HIDE);
                return;
            }
            // The paint hands over the frame it actually drew, so the child
            // cannot disagree with it about where the "add" chip starts. This
            // was previously recomputed here from different constants and
            // agreed only by coincidence of font metrics.
            let inset = self.s(3);
            SetWindowPos(
                self.edit_notify,
                std::ptr::null_mut(),
                pad + self.s(6),
                top + inset,
                self.notify_frame_right - pad - self.s(10),
                self.ctrl_h() - 2 * inset,
                SWP_NOZORDER,
            );
            ShowWindow(self.edit_notify, SW_SHOW);
        }
    }

    /// Finish a metric drag: turn the drop position into an index, reorder and
    /// persist. A drop outside the list clamps to its ends rather than being
    /// discarded, so a slightly overshot gesture still does what was meant.
    fn drop_metric(&mut self, hwnd: HWND, y: i32) {
        let Some(from) = self.metric_drag.take() else { return };
        unsafe { ReleaseCapture() };
        let (top, row_h) = self.metric_list;
        let len = self.cfg.main_metrics.len();
        if row_h > 0 && len > 0 {
            let to = ((y - top) / row_h).clamp(0, len as i32 - 1) as usize;
            config::reorder_main_metrics(&mut self.cfg.main_metrics, from, to);
            config::save(&self.cfg);
        }
        unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
    }

    /// Small text button; returns the next button's right edge.
    /// An ALL-CAPS section heading.
    ///
    /// One function, because the divergence between this app and its own design
    /// spec came from letting 155 call sites each pick a font. The bulk rename
    /// from the old four-font set mapped every 11/400 `small` to the new 11/500
    /// `micro`, and only the places actively being rewritten were revisited — so
    /// every heading in the product stayed a whisper when the spec had said
    /// `label` (12/600, tracked) all along. Roles belong in functions, not in
    /// arguments repeated a hundred and fifty times.
    fn heading(&self, dc: HDC, x: i32, y: i32, text: &str) {
        gdi::text_t(
            dc,
            x,
            y,
            self.fonts.label,
            self.s(gdi::TRACK_LABEL),
            gdi::t().dim,
            text,
        );
    }

    /// A checkbox. Rounded frame, and when checked an accent fill carrying a
    /// tick — the old "checked" state was a smaller filled square inside a
    /// bigger one, which is a radio button's idiom, not a checkbox's.
    fn check_box(&self, dc: HDC, r: &RECT, checked: bool) {
        let fill_c = if checked { gdi::acc().cpu } else { gdi::t().input_bg };
        let line_c = if checked { fill_c } else { gdi::t().input_border };
        gdi::card(dc, r, fill_c, line_c, self.s(3));
        if checked {
            gdi::icon(
                dc,
                (r.left + r.right) / 2,
                (r.top + r.bottom) / 2,
                r.right - r.left,
                self.s(2).max(1),
                gdi::on(fill_c),
                fill_c,
                gdi::Icon::Check,
            );
        }
    }

    /// The destructive mark. A trash glyph in `danger`, never the `×` that also
    /// dismissed windows — the same mark for "close this" and "terminate this
    /// process" was the one genuinely dangerous thing in the old icon set.
    fn kill_glyph(&self, dc: HDC, x: i32, y: i32) {
        let g = self.s(GLYPH);
        gdi::icon(
            dc,
            x + g / 2,
            y + g / 2 + self.s(SP1),
            g,
            self.s(1).max(1),
            gdi::t().danger,
            gdi::t().card,
            gdi::Icon::Trash,
        );
    }

    /// The magnifier that sits inside a search or filter frame, in place of a
    /// word label beside it.
    fn search_glyph(&self, dc: HDC, frame: &RECT) {
        gdi::icon(
            dc,
            frame.left + self.s(SP3) + self.s(GLYPH) / 2,
            (frame.top + frame.bottom) / 2,
            self.s(GLYPH),
            self.s(1).max(1),
            gdi::t().mute,
            gdi::t().input_bg,
            gdi::Icon::Search,
        );
    }

    /// Total width the header's icon buttons occupy, including the gap between
    /// each. One function so `draw_main` and `update_edit` cannot disagree about
    /// where the search field has to stop — they are 1500 lines apart.
    fn header_controls_w(&self) -> i32 {
        // Always three: gear and pin, plus either close (flyout) or on-top
        // (pinned) — never both, since a pinned window has a real title bar.
        // Plus the separating rule and the air either side of it: without that,
        // the pin sits hard against the search field and reads as belonging to
        // it, as though it pinned the search.
        3 * self.ctrl_h() + 2 * self.s(SP2) + 2 * self.s(SP4) + 1
    }

    /// A square icon button, `CTRL_H` on a side, laid out right-to-left.
    /// Returns the right edge for the next button along.
    ///
    /// Replaces the word-buttons — `settings`, `pin`, `top`, `pause` — that used
    /// to sit in the header. A row of words reads as prose and has to be
    /// re-read; a row of glyphs is scanned once and learned.
    fn icon_button(
        &mut self,
        dc: HDC,
        right_edge: i32,
        y: i32,
        g: gdi::Icon,
        active: bool,
        action: Action,
    ) -> i32 {
        let h = self.ctrl_h();
        let r = RECT { left: right_edge - h, top: y, right: right_edge, bottom: y + h };
        let hot = self.hovered(&r);
        let down = hot && self.pressed;
        let ground = if down {
            gdi::t().card_press
        } else if hot {
            gdi::t().card_hover
        } else {
            gdi::t().bg
        };
        if hot || active {
            let fill_c = if active && !hot { gdi::t().card } else { ground };
            gdi::card(dc, &r, fill_c, gdi::t().line, self.s(RADIUS));
        }
        // Close is not destructive, so it never goes red; the trash glyph is the
        // only thing in the product that does.
        let color = if active {
            gdi::acc().cpu
        } else if hot {
            if g == gdi::Icon::Trash { gdi::t().danger } else { gdi::t().text }
        } else {
            gdi::t().dim
        };
        gdi::icon(
            dc,
            (r.left + r.right) / 2,
            (r.top + r.bottom) / 2,
            self.s(CHROME_GLYPH),
            self.s(1).max(1) + 1,
            color,
            ground,
            g,
        );
        self.hits.push((r, action));
        r.left - self.s(SP2)
    }



    /// Filled pill button (for save/cancel and choice chips).
    fn chip(&mut self, dc: HDC, x: i32, y: i32, label: &str, active: bool, action: Action) -> i32 {
        let w = gdi::text_width_t(dc, self.fonts.label, self.s(gdi::TRACK_LABEL), label)
            + self.s(16);
        let r = RECT { left: x, top: y, right: x + w, bottom: y + self.ctrl_h() };
        let fill_c = if active { gdi::acc().cpu } else { gdi::t().card };
        let line_c = if active { fill_c } else { gdi::t().line };
        gdi::card(dc, &r, fill_c, line_c, self.s(RADIUS));
        // Centred rather than offset by a constant, so the label stays put if
        // the control height or the font ever changes.
        let (asc, desc, _) = gdi::text_metrics(dc, self.fonts.label);
        gdi::text_t(
            dc,
            x + self.s(SP3),
            y + (self.ctrl_h() - (asc + desc)) / 2,
            self.fonts.label,
            self.s(gdi::TRACK_LABEL),
            if active { gdi::on(fill_c) } else { gdi::t().text },
            label,
        );
        self.hits.push((r, action));
        x + w + self.s(6)
    }

    fn scrollbar(&mut self, dc: HDC, rc: &RECT, list_top: i32, content_h: i32) {
        let viewport = rc.bottom - self.s(12) - list_top;
        self.max_scroll = (content_h - viewport).max(0);
        self.scroll = self.scroll.clamp(0, self.max_scroll);
        if self.max_scroll == 0 {
            return;
        }
        let track = RECT {
            left: rc.right - self.s(6),
            top: list_top,
            right: rc.right - self.s(3),
            bottom: rc.bottom - self.s(12),
        };
        gdi::fill(dc, &track, gdi::t().card);
        let th = ((viewport as f32 / content_h as f32) * (track.bottom - track.top) as f32) as i32;
        let ty = track.top
            + ((self.scroll as f32 / self.max_scroll as f32)
                * (track.bottom - track.top - th) as f32) as i32;
        let thumb = RECT { left: track.left, top: ty, right: track.right, bottom: ty + th.max(self.s(16)) };
        gdi::fill(dc, &thumb, gdi::t().dim);
    }

    fn draw_main(&mut self, dc: HDC, rc: &RECT) {
        let pad = self.s(12);
        let mut y = pad;
        let s = self.snap.clone();

        // --- header strip: search field and window controls on ONE row.
        //
        // These used to be two bands: a row of word-buttons, then a labelled
        // input below it. Merging them reclaims a whole band, and dropping the
        // "Find app" label gives its 60 px to the field — the magnifier inside
        // the frame says what the box is for.
        let pinned = self.cfg.pinned;
        let mut edge = rc.right - pad;
        // A pinned window has a real title-bar close button, so the in-panel
        // close is only drawn for the flyout.
        if !pinned {
            edge = self.icon_button(dc, edge, y, gdi::Icon::Close, false, Action::ClosePanel);
        }
        edge = self.icon_button(dc, edge, y, gdi::Icon::Gear, false, Action::OpenSettings);
        // A flyout is always topmost (see insert_after), so the toggle only
        // means anything once the window is pinned in place.
        if pinned {
            let on_top = self.cfg.on_top;
            edge = self.icon_button(dc, edge, y, gdi::Icon::OnTop, on_top, Action::ToggleTop);
        }
        let pin_glyph = if pinned { gdi::Icon::Unpin } else { gdi::Icon::Pin };
        edge = self.icon_button(dc, edge, y, pin_glyph, pinned, Action::TogglePin);

        // A rule between the controls and the field. Without it the pin sits
        // hard against the search box and proximity groups them, so it reads as
        // pinning the search rather than the window.
        let controls_left = edge + self.s(SP2);
        let div_x = controls_left - self.s(SP4);
        let div = RECT {
            left: div_x,
            top: y + self.s(SP2),
            right: div_x + 1,
            bottom: y + self.ctrl_h() - self.s(SP2),
        };
        gdi::fill(dc, &div, gdi::t().line);
        let frame =
            RECT { left: pad, top: y, right: div_x - self.s(SP4), bottom: y + self.ctrl_h() };
        gdi::input_frame(dc, &frame);
        self.search_glyph(dc, &frame);
        self.clear_button(dc, &frame);
        y += self.s(HEADER_STRIDE);

        // --- app finder results replace the overview while searching
        if !self.filter.is_empty() {
            if self.snap.procs.is_empty() {
                gdi::text(dc, pad, y, self.fonts.body, gdi::t().dim, "Collecting data…");
                return;
            }
            self.drawn_rows = top_by(&self.snap.procs, Metric::Cpu, 20, &self.filter);
            if self.drawn_rows.is_empty() {
                gdi::text(dc, pad, y, self.fonts.body, gdi::t().dim, "No apps match your search");
                return;
            }
            let rows = self.drawn_rows.clone();
            let name_fonts = [self.fonts.body, self.fonts.micro];
            for (idx, (name, _)) in rows.iter().enumerate() {
                let a = watch_sums(&self.snap.procs, name);
                let vs = format!(
                    "{}  ·  {}  ·  {}",
                    format_pct(a.cpu),
                    format_bytes(a.ram_private),
                    format_rate(a.net_bps)
                );
                let vw = gdi::text_width(dc, self.fonts.micro, &vs);
                let hit = RECT { left: pad, top: y - self.s(2), right: rc.right - pad, bottom: y + self.s(22) };
                if self.hovered(&hit) {
                    gdi::fill(dc, &hit, gdi::t().card_hover);
                }
                gdi::text_fit(dc, pad + self.s(4), y, rc.right - pad - vw - self.s(14), &name_fonts, gdi::t().text, name);
                gdi::text_right(dc, rc.right - pad - self.s(4), y + self.s(2), self.fonts.micro, gdi::t().dim, &vs);
                self.hits.push((hit, Action::Watch(idx)));
                y += self.s(ROW_LIST);
                if y > rc.bottom - self.s(24) {
                    break;
                }
            }
            return;
        }


        // --- metric rows
        let gpu_figs = if s.gpu_ok {
            Figures::new(format_pct(s.gpu_pct))
        } else {
            Figures::new("—".to_string())
        };
        // FPS reads as a dash when nothing is presenting, exactly as GPU does
        // when the card reports no usage. It used to be a special-cased row
        // above these with a prose empty state and no graph.
        let fps_figs = match (&s.fps, s.etw_ok) {
            (Some((_, name, fps)), _) => Figures::new(fps.to_string())
                .unit(Unit::Word("fps"))
                .note(format!("in {}", name)),
            (None, true) => Figures::new("—".to_string()),
            (None, false) => Figures::new("—".to_string())
                .note("run as administrator to see".to_string()),
        };
        let ram_figs = {
            let f = Figures::new(format_bytes(s.mem_used)).unit(Unit::Word("used"));
            if s.mem_total > 0 {
                let pct = s.mem_used as f32 / s.mem_total as f32 * 100.0;
                f.sub(
                    format!("of {} · {}", format_bytes(s.mem_total), format_pct(pct)),
                    Unit::None,
                )
            } else {
                f
            }
        };
        // name, action, figures, ring, scale — label, accent and glyph all come
        // from `metric_label`, the one map for a main_metrics name.
        let rows: [(&str, Action, Figures, &Ring, Scale); 7] = [
            (
                "cpu",
                Action::Drill(Metric::Cpu),
                Figures::new(format_pct(s.cpu_pct)),
                &self.hist_cpu,
                Scale::Percent,
            ),
            ("ram", Action::Drill(Metric::Ram), ram_figs, &self.hist_mem, Scale::Percent),
            ("gpu", Action::Drill(Metric::Gpu), gpu_figs, &self.hist_gpu, Scale::Percent),
            // Visually uniform with the rest, but keeps its own destination:
            // the FPS list, not a generic drill.
            ("fps", Action::ShowFpsApps, fps_figs, &self.hist_fps, Scale::Fps),
            (
                "disk",
                Action::Drill(Metric::Disk),
                Figures::new(format_rate(s.disk_read_bps))
                    .unit(Unit::Word("read"))
                    .sub(format_rate(s.disk_write_bps), Unit::Word("write")),
                &self.hist_disk,
                Scale::Rate,
            ),
            (
                "net",
                Action::Drill(Metric::Net),
                Figures::new(net_rate(s.net_rx_bps))
                    .unit(Unit::Down)
                    .sub(net_rate(s.net_tx_bps), Unit::Up),
                &self.hist_net,
                Scale::Rate,
            ),
            (
                "audio",
                Action::Drill(Metric::Audio),
                {
                    let mut names: Vec<&str> = Vec::new();
                    for p in &s.procs {
                        if p.audio > 0.01 && !names.iter().any(|n| *n == p.name.as_str()) {
                            names.push(p.name.as_str());
                        }
                    }
                    if names.is_empty() {
                        Figures::new("Silent".to_string())
                    } else {
                        Figures::new(format!("{} playing", names.len()))
                    }
                },
                &self.hist_audio,
                Scale::Percent,
            ),
        ];
        // Drawn in the user's chosen order, skipping the ones they hid. Any
        // row whose name is not in their list simply does not draw; config
        // guarantees every known metric is present, so that cannot silently
        // drop a metric this build knows about.
        let order = self.cfg.main_metrics.clone();
        for (name, visible) in &order {
            if !visible {
                continue;
            }
            let Some((_, action, figs, ring, scale)) = rows.iter().find(|r| r.0 == name.as_str())
            else {
                continue;
            };
            let (label, accent, glyph) = metric_label(name);
            let (action, ring, scale) = (*action, *ring, *scale);
            let sticky = match name.as_str() {
                "disk" => &self.ceil_disk,
                _ => &self.ceil_net,
            };
            let max = scale.ceiling(ring.max(), sticky);
            let row = RECT {
                left: pad,
                top: y,
                right: rc.right - pad,
                bottom: y + self.s(CARD_METRIC),
            };
            self.card(dc, &row);
            let name_right = self.metric_name(dc, &row, accent, glyph, label);
            let spark = RECT {
                left: row.right - self.s(SPARK_W),
                top: y + self.s(SP3),
                right: row.right - self.s(SP4),
                bottom: y + self.s(44),
            };
            self.draw_figures(dc, &row, name_right, spark.left - self.s(SP3), figs);
            // A two-way metric puts its second direction below the midline. The
            // labels are passed but only paint where a half is tall enough for
            // them; at row height the row's own two figures name the directions.
            let mirror = match name.as_str() {
                "disk" => Some(gdi::Mirror {
                    ring: &self.hist_disk_w,
                    label_hi: "Read",
                    label_lo: "Write",
                    color: gdi::acc().disk_w,
                    ceiling: self.ceil_disk_w.peek(),
                }),
                "net" => Some(gdi::Mirror {
                    ring: &self.hist_net_tx,
                    label_hi: "Down",
                    label_lo: "Up",
                    color: gdi::acc().net_tx,
                    ceiling: self.ceil_net_tx.peek(),
                }),
                _ => None,
            };
            gdi::chart(
                dc,
                self.surf.as_ref(),
                &spark,
                ring,
                max,
                accent,
                gdi::ChartSize::Row,
                self.scale,
                None,
                None,
                chart_units(scale),
                mirror,
            );
            self.hits.push((row, action));
            y += self.s(ROW_METRIC);
        }

        // --- drives
        self.heading(dc, pad, y + self.s(4), "DRIVES");
        y += self.s(26);
        if s.drives.is_empty() {
            gdi::text(dc, pad, y + self.s(4), self.fonts.body, gdi::t().dim, "No drives found");
            y += self.s(30);
        }
        // Two lines per drive: letter and figures on top, full-width bar
        // beneath. Sharing one line left the bar 156px and the figures 128px,
        // which crowded both and let a long figure overrun the bar.
        for d in &s.drives {
            let used = d.total.saturating_sub(d.free);
            let frac = if d.total > 0 { used as f32 / d.total as f32 } else { 0.0 };
            let (basc, _, _) = gdi::text_metrics(dc, self.fonts.value);
            let (sasc, _, _) = gdi::text_metrics(dc, self.fonts.micro);
            let baseline = y + basc;
            gdi::text(dc, pad, baseline - basc, self.fonts.value, gdi::t().text, &format!("{}:", d.letter));
            let label = format!("{} free of {}", format_bytes(d.free), format_bytes(d.total));
            let lw = gdi::text_width(dc, self.fonts.micro, &label);
            // Right-aligned, but never allowed to reach back into the letter.
            let lx = (rc.right - pad - lw).max(pad + self.s(30));
            gdi::text_fit(
                dc,
                lx,
                baseline - sasc,
                rc.right - pad,
                &[self.fonts.micro],
                gdi::t().dim,
                &label,
            );
            y += self.s(22);
            let bar = RECT { left: pad, top: y, right: rc.right - pad, bottom: y + self.s(SP3) };
            // Severity is separate from the metric's accent: a nearly full disk
            // is a state the user should read at a glance, not a shade of teal.
            let fill_c = if frac >= 0.95 {
                gdi::t().danger
            } else if frac >= 0.85 {
                gdi::t().warn
            } else {
                gdi::acc().disk
            };
            gdi::bar(dc, &bar, frac, fill_c);
            y += self.s(20);
        }

        // --- MCP footer buttons, pinned to the bottom.
        if self.footer_rows() > 0 {
            self.mcp_footer(dc, rc);
        }
    }

    /// Whether there is agent work worth a door into the activity view.
    /// History counts as much as live work: gating on live agents alone made
    /// the only way back to what had just finished close the moment it did.
    /// Stale agents count too — the view exists to show their "last heard"
    /// state, so going stale must not take away the only way to see it.
    fn any_agents(&self) -> bool {
        self.shared.agents.lock().unwrap().iter().any(|s| !s.agents.is_empty())
            || !self.shared.agent_history.lock().unwrap().is_empty()
    }

    /// How many footer buttons are showing (0, 1 or 2). The window height is
    /// computed from this outside of a paint, so it cannot live in the drawing
    /// code.
    fn footer_rows(&self) -> usize {
        if !self.cfg.mcp_enabled {
            return 0;
        }
        usize::from(!self.mcp_messages.is_empty()) + usize::from(self.any_agents())
    }

    /// Clickable buttons pinned to the bottom of the panel: one for AI
    /// activity, one for messages. Each gets its own full-width row and its
    /// own accent. Sharing a row made them read as a single bar with an
    /// invisible seam down it, left neither label room for its long form, and
    /// implied the two counts were about the same thing — they are not.
    fn mcp_footer(&mut self, dc: HDC, rc: &RECT) {
        let pad = self.s(12);
        let n = self.mcp_messages.len();
        let now = crate::sampler::unix_ms();
        let (live, present, sessions) = {
            let s = self.shared.agents.lock().unwrap();
            (
                crate::agents::live_count(&s, now),
                s.iter().map(|x| x.agents.len()).sum::<usize>(),
                s.iter().filter(|x| !x.agents.is_empty()).count(),
            )
        };
        let finished = self.shared.agent_history.lock().unwrap().len();

        // Built bottom-up, so the row nearest the edge stays where it is as
        // the other one comes and goes.
        let mut rows: Vec<(String, Action, u32)> = Vec::new();
        if n > 0 {
            rows.push((
                format!("◇  {} new message{}", n, if n == 1 { "" } else { "s" }),
                Action::ShowMcp,
                gdi::acc().net,
            ));
        }
        if present > 0 || finished > 0 {
            // Sessions are only worth naming once there is more than one; with
            // a single assistant connected the count would be noise.
            let label = if live == 0 && present > 0 {
                // Agents we stopped hearing from: the row must match what the
                // activity view will say about them.
                format!("◆  {} gone quiet", present)
            } else if live == 0 {
                // Nothing running, but there is a record to look back on.
                format!("◆  {} finished", finished)
            } else if sessions > 1 {
                format!("◆  {} agent{} running in {} sessions", live, if live == 1 { "" } else { "s" }, sessions)
            } else {
                format!("◆  {} agent{} running", live, if live == 1 { "" } else { "s" })
            };
            rows.push((label, Action::ShowActivity, gdi::acc().gpu));
        }

        let h = self.s(26);
        let gap = self.s(4);
        let (left, right) = (pad, rc.right - pad);
        let mut top = rc.bottom - pad - h;
        for (label, action, accent) in rows {
            self.footer_button(dc, left, top, right, h, &label, action, accent);
            top -= h + gap;
        }
    }

    /// One footer button: accent outline, filled on hover, label clipped to
    /// fit. The border and interior are shades of the accent, so agents (teal)
    /// and messages (amber) are told apart at a glance.
    fn footer_button(
        &mut self,
        dc: HDC,
        left: i32,
        top: i32,
        right: i32,
        h: i32,
        label: &str,
        action: Action,
        accent: u32,
    ) {
        let bar = RECT { left, top, right, bottom: top + h };
        let hot = self.hovered(&bar);
        if hot {
            gdi::fill(dc, &bar, accent);
        } else {
            // Accent border over a darker interior of the same hue, matching
            // the site's teal treatment for whatever accent is passed in.
            gdi::fill(dc, &bar, gdi::shade(accent, 0.39));
            let inner = RECT { left: bar.left + 1, top: bar.top + 1, right: bar.right - 1, bottom: bar.bottom - 1 };
            gdi::fill(dc, &inner, gdi::shade(accent, 0.12));
        }
        let color = if hot { gdi::on(accent) } else { accent };
        gdi::text_fit(
            dc,
            bar.left + self.s(11),
            bar.top + self.s(6),
            bar.right - self.s(8),
            &[self.fonts.value_sm, self.fonts.micro],
            color,
            label,
        );
        self.hits.push((bar, action));
    }

    /// Draw the "connect this to Claude Code" helper: a one-line instruction,
    /// a Copy button, and the exact command. Returns the y below it.
    fn draw_mcp_connect(&mut self, dc: HDC, rc: &RECT, x: i32, mut y: i32) -> i32 {
        let pad = self.s(12);
        let right = rc.right - pad;
        gdi::text(
            dc,
            x,
            y,
            self.fonts.micro,
            gdi::t().dim,
            "Connect it in Claude Code (run this once):",
        );
        let label = if self.mcp_copied { "Copied" } else { "Copy" };
        let cw = gdi::text_width(dc, self.fonts.micro, label) + self.s(16);
        self.chip(dc, right - cw, y - self.s(3), label, self.mcp_copied, Action::CopyMcpCmd);
        y += self.s(20);
        let box_r = RECT { left: x, top: y, right, bottom: y + self.s(22) };
        gdi::fill(dc, &box_r, gdi::t().card);
        gdi::text_fit(
            dc,
            x + self.s(8),
            y + self.s(4),
            right - self.s(8),
            &[self.fonts.body, self.fonts.micro],
            gdi::t().text,
            &mcp_connect_cmd(),
        );
        y + self.s(30)
    }

    /// What AI tools have reported they are doing. Everything here is a claim
    /// made by the assistant over MCP, not something the app measured, so
    /// entries that stop being refreshed are marked stale rather than left
    /// looking live.
    fn draw_activity(&mut self, dc: HDC, rc: &RECT) {
        let pad = self.s(12);
        let mut y = pad;
        y = self.header(dc, rc, y, "AI activity", gdi::acc().gpu);

        let now = crate::sampler::unix_ms();
        let sessions: Vec<crate::sampler::AgentSession> =
            self.shared.agents.lock().unwrap().clone();
        let history: Vec<crate::sampler::FinishedAgent> =
            self.shared.agent_history.lock().unwrap().iter().cloned().collect();
        let live_total: usize = sessions.iter().map(|s| s.agents.len()).sum();

        if live_total == 0 && history.is_empty() {
            gdi::text(dc, pad, y, self.fonts.body, gdi::t().dim, "Nothing reported");
            gdi::text(dc, pad, y + self.s(20), self.fonts.micro, gdi::t().dim, "When an AI tool tells this app what its");
            gdi::text(dc, pad, y + self.s(35), self.fonts.micro, gdi::t().dim, "agents are doing, it appears here.");
            return;
        }

        if live_total > 0 {
            self.chip(dc, rc.right - pad - self.s(52), y, "clear", false, Action::ClearAgents);
            if let Some(last) = self.hits.pop() {
                self.hits.insert(0, last);
            }
        }
        y += self.s(26);

        // Build the display list first so scrolling can walk variable heights.
        enum Item<'a> {
            Session(&'a str),
            /// The row plus its key in `agent_expanded`.
            Live(&'a crate::sampler::AgentEntry, u64),
            FinishedHeader(usize),
            Finished(&'a crate::sampler::FinishedAgent, u64),
        }
        let mut items: Vec<Item> = Vec::new();
        // A single session needs no header: naming it would be noise when
        // there is nothing to tell it apart from.
        let multi = sessions.iter().filter(|s| !s.agents.is_empty()).count() > 1;
        for sess in sessions.iter().filter(|s| !s.agents.is_empty()) {
            if multi {
                items.push(Item::Session(&sess.label));
            }
            for a in &sess.agents {
                items.push(Item::Live(a, agent_key(&sess.key, &a.id, a.started_ms)));
            }
        }
        if !history.is_empty() {
            items.push(Item::FinishedHeader(history.len()));
            if self.finished_expanded {
                for f in &history {
                    items.push(Item::Finished(f, agent_key("finished", &f.id, f.finished_ms)));
                }
            }
        }
        // Rows are variable height once opened, so measure every row up front
        // and use the same numbers for the walk and the scroll extent.
        let heights: Vec<i32> = items
            .iter()
            .map(|it| match it {
                Item::Session(_) => self.s(24),
                Item::Live(a, key) => self.s(40) + self.detail_extra(dc, rc, &a.detail, *key),
                Item::FinishedHeader(_) => self.s(30),
                Item::Finished(f, key) => self.s(50) + self.detail_extra(dc, rc, &f.detail, *key),
            })
            .collect();

        let list_top = y;
        let saved = unsafe { SaveDC(dc) };
        unsafe { IntersectClipRect(dc, 0, list_top, rc.right, rc.bottom) };
        let mut ry = list_top - self.scroll;
        for (item, &h) in items.iter().zip(heights.iter()) {
            if ry + h < list_top || ry > rc.bottom {
                ry += h;
                continue;
            }
            match item {
                Item::Session(label) => {
                    gdi::text_fit(
                        dc,
                        pad,
                        ry + self.s(5),
                        rc.right - pad,
                        &[self.fonts.micro],
                        gdi::acc().gpu,
                        label,
                    );
                }
                Item::Live(a, key) => {
                    self.agent_row_hit(dc, rc, ry, h, &a.detail, *key);
                    self.draw_live_agent(dc, rc, ry, a, now, *key);
                }
                Item::FinishedHeader(n) => {
                    let caret = if self.finished_expanded { "▾" } else { "▸" };
                    let label = format!("{}  Finished ({})", caret, n);
                    gdi::text(dc, pad, ry + self.s(8), self.fonts.value_sm, gdi::t().text, &label);
                    let hit = RECT { left: pad, top: ry, right: rc.right - pad - self.s(60), bottom: ry + self.s(28) };
                    self.hits.push((hit, Action::ToggleFinished));
                    // History gets its own clear, so the header chip cannot
                    // wipe it by reflex along with the live list.
                    if self.finished_expanded {
                        self.chip(dc, rc.right - pad - self.s(52), ry + self.s(3), "clear", false, Action::ClearHistory);
                    }
                }
                Item::Finished(f, key) => {
                    self.agent_row_hit(dc, rc, ry, h, &f.detail, *key);
                    self.draw_finished_agent(dc, rc, ry, f, *key);
                }
            }
            ry += h;
        }
        unsafe { RestoreDC(dc, saved) };
        self.scrollbar(dc, rc, list_top, heights.iter().sum());
    }

    /// How many lines an agent's reported detail wraps to at the activity
    /// view's width. 0 when there is no detail; 1 means nothing to open.
    fn detail_lines(&self, dc: HDC, rc: &RECT, detail: &str) -> i32 {
        if detail.is_empty() {
            return 0;
        }
        let pad = self.s(12);
        let body_x = pad + self.s(16);
        gdi::wrap_lines(dc, self.fonts.micro, rc.right - pad - body_x, detail)
            .len()
            .max(1) as i32
    }

    /// Extra height an opened detail adds to its row.
    fn detail_extra(&self, dc: HDC, rc: &RECT, detail: &str, key: u64) -> i32 {
        if !self.agent_expanded.contains(&key) {
            return 0;
        }
        (self.detail_lines(dc, rc, detail) - 1).max(0) * self.s(15)
    }

    /// Hover shading and the click target for an agent row, registered before
    /// the row paints so the fill sits under its text. A detail that already
    /// fits on one line gets neither: an affordance that does nothing is
    /// worse than none.
    fn agent_row_hit(&mut self, dc: HDC, rc: &RECT, ry: i32, h: i32, detail: &str, key: u64) {
        if self.detail_lines(dc, rc, detail) <= 1 {
            return;
        }
        let pad = self.s(12);
        let row = RECT { left: pad, top: ry, right: rc.right - pad, bottom: ry + h - self.s(6) };
        if self.hovered(&row) {
            gdi::fill(dc, &row, gdi::t().card_hover);
        }
        self.hits.push((row, Action::ToggleAgent(key)));
    }

    /// The detail line of an agent row: one clipped line, or the full wrapped
    /// text when open, with a caret in the gutter under the status dot so the
    /// text keeps its full width. Returns the y below what it drew.
    fn agent_detail(&self, dc: HDC, rc: &RECT, y: i32, detail: &str, key: u64) -> i32 {
        let lines = self.detail_lines(dc, rc, detail);
        if lines == 0 {
            return y;
        }
        let pad = self.s(12);
        let line_h = self.s(15);
        let body_x = pad + self.s(16);
        let open = lines > 1 && self.agent_expanded.contains(&key);
        if lines > 1 {
            gdi::text(dc, pad, y, self.fonts.micro, gdi::t().dim, if open { "▾" } else { "▸" });
        }
        if open {
            gdi::text_wrap(
                dc,
                body_x,
                y,
                rc.right - pad - body_x,
                line_h,
                self.fonts.micro,
                gdi::t().dim,
                detail,
            );
            y + lines * line_h
        } else {
            gdi::text_fit(dc, body_x, y, rc.right - pad, &[self.fonts.micro], gdi::t().dim, detail);
            y + line_h
        }
    }

    /// One live agent row: status dot, title, reported status, detail.
    fn draw_live_agent(&mut self, dc: HDC, rc: &RECT, ry: i32, a: &crate::sampler::AgentEntry, now: u64, key: u64) {
        let pad = self.s(12);
        let age = now.saturating_sub(a.seen_ms);
        let stale = age >= crate::sampler::AGENT_STALE_MS;
        let running = crate::agents::is_live(&a.status);
        let dot_color = if stale {
            gdi::t().dim
        } else {
            match a.status.as_str() {
                "failed" => gdi::rgb(220, 90, 90),
                "done" => gdi::t().dim,
                "waiting" => gdi::acc().fps,
                _ => gdi::acc().gpu,
            }
        };
        self.agent_dot(dc, ry, dot_color, running && !stale);
        let status_text = if stale {
            format!("last heard {}", short_age(age))
        } else if a.status.is_empty() {
            "running".to_string()
        } else {
            a.status.clone()
        };
        let sw = gdi::text_width(dc, self.fonts.micro, &status_text);
        let sx = rc.right - pad - sw;
        gdi::text(dc, sx, ry, self.fonts.micro, if stale { gdi::t().dim } else { dot_color }, &status_text);
        gdi::text_fit(
            dc,
            pad + self.s(16),
            ry,
            sx - self.s(8),
            &[self.fonts.value_sm, self.fonts.micro],
            if stale { gdi::t().dim } else { gdi::t().text },
            // Fall back to the id when an assistant sends detail but no title,
            // so the row still identifies itself.
            if !a.title.is_empty() { &a.title } else { &a.id },
        );
        self.agent_detail(dc, rc, ry + self.s(17), &a.detail, key);
    }

    /// One finished agent: hollow dot, title, when it ended and how long it
    /// ran, its detail, and which session it came from.
    fn draw_finished_agent(&mut self, dc: HDC, rc: &RECT, ry: i32, f: &crate::sampler::FinishedAgent, key: u64) {
        let pad = self.s(12);
        let color = if f.status == "failed" { gdi::rgb(220, 90, 90) } else { gdi::t().dim };
        self.agent_dot(dc, ry, color, false);
        let when = format!(
            "{} · {}",
            clock_of(f.finished_ms),
            crate::agents::format_duration(f.finished_ms.saturating_sub(f.started_ms))
        );
        let sw = gdi::text_width(dc, self.fonts.micro, &when);
        let sx = rc.right - pad - sw;
        gdi::text(dc, sx, ry, self.fonts.micro, gdi::t().dim, &when);
        gdi::text_fit(
            dc,
            pad + self.s(16),
            ry,
            sx - self.s(8),
            &[self.fonts.value_sm, self.fonts.micro],
            color,
            if !f.title.is_empty() { &f.title } else { &f.id },
        );
        // The session label follows the detail, so an opened row pushes it
        // down rather than letting the two overlap.
        let label_y = self.agent_detail(dc, rc, ry + self.s(16), &f.detail, key);
        gdi::text_fit(
            dc,
            pad + self.s(16),
            if f.detail.is_empty() { ry + self.s(31) } else { label_y },
            rc.right - pad,
            &[self.fonts.micro],
            gdi::acc().gpu,
            &f.session_label,
        );
    }

    /// Filled dot for live work, hollow for finished.
    fn agent_dot(&self, dc: HDC, ry: i32, color: u32, filled: bool) {
        let pad = self.s(12);
        gdi::icon(
            dc,
            pad + self.s(6),
            ry + self.s(SP3),
            self.s(GLYPH),
            self.s(2).max(1),
            color,
            gdi::t().bg,
            if filled { gdi::Icon::DotFilled } else { gdi::Icon::DotHollow },
        );
    }

    fn draw_mcp_messages(&mut self, dc: HDC, rc: &RECT) {
        let pad = self.s(12);
        let mut y = pad;
        y = self.header(dc, rc, y, "Messages", gdi::acc().gpu);

        if self.mcp_messages.is_empty() {
            gdi::text(dc, pad, y, self.fonts.body, gdi::t().dim, "No messages yet");
            gdi::text(dc, pad, y + self.s(20), self.fonts.micro, gdi::t().dim, "Messages from connected AI tools appear here.");
            return;
        }

        // Clear-all chip below the header, right-aligned; its hit is moved to
        // the front so it wins over any overlapping row.
        self.chip(dc, rc.right - pad - self.s(52), y, "clear", false, Action::ClearMcp);
        if let Some(last) = self.hits.pop() {
            self.hits.insert(0, last);
        }
        y += self.s(26);

        let msgs: Vec<(String, String, String)> = self.mcp_messages.iter().cloned().collect();
        let list_top = y;
        let line_h = self.s(15);
        let body_x = pad + self.s(14);
        let body_w = rc.right - pad - body_x;
        let saved = unsafe { windows_sys::Win32::Graphics::Gdi::SaveDC(dc) };
        unsafe { windows_sys::Win32::Graphics::Gdi::IntersectClipRect(dc, 0, list_top, rc.right, rc.bottom) };
        // Rows are variable height once expanded, so walk a running y rather
        // than indexing a fixed row pitch.
        let mut ry = list_top - self.scroll;
        for (i, (time, title, message)) in msgs.iter().enumerate() {
            // A message that fits on one line has nothing to expand, so it
            // gets no chevron and no click target — an affordance that does
            // nothing is worse than none.
            let lines = gdi::wrap_lines(dc, self.fonts.micro, body_w, message).len() as i32;
            let expandable = lines > 1;
            let open = expandable && self.msg_expanded.contains(&i);
            let body_h = if open { lines * line_h } else { line_h };
            let row_h = self.s(20) + body_h + self.s(8);
            // Skip rows scrolled entirely out of view, but keep advancing y.
            if ry + row_h >= list_top && ry <= rc.bottom {
                let row = RECT { left: pad, top: ry, right: rc.right - pad, bottom: ry + row_h - self.s(4) };
                if expandable && self.hovered(&row) {
                    gdi::fill(dc, &row, gdi::t().card_hover);
                }
                if expandable {
                    gdi::disclosure(
                        dc,
                        pad + self.s(5),
                        ry + self.s(7),
                        self.s(8),
                        self.s(2),
                        gdi::t().dim,
                        open,
                    );
                }
                gdi::text(dc, body_x, ry, self.fonts.micro, gdi::acc().gpu, time);
                let head = if title.is_empty() { "message" } else { title.as_str() };
                gdi::text(
                    dc,
                    body_x + self.s(40),
                    ry,
                    self.fonts.value_sm,
                    gdi::t().text,
                    head,
                );
                if open {
                    gdi::text_wrap(
                        dc,
                        body_x,
                        ry + self.s(20),
                        body_w,
                        line_h,
                        self.fonts.micro,
                        gdi::t().dim,
                        message,
                    );
                } else {
                    gdi::text_fit(
                        dc,
                        body_x,
                        ry + self.s(20),
                        rc.right - pad,
                        &[self.fonts.micro],
                        gdi::t().dim,
                        message,
                    );
                }
                if expandable {
                    self.hits.push((
                        RECT {
                            left: pad,
                            top: ry.max(list_top),
                            right: rc.right - pad,
                            bottom: ry + row_h - self.s(4),
                        },
                        Action::ToggleMsg(i),
                    ));
                }
            }
            ry += row_h;
        }
        unsafe { windows_sys::Win32::Graphics::Gdi::RestoreDC(dc, saved) };
        let content_h = ry + self.scroll - list_top;
        self.scrollbar(dc, rc, list_top, content_h);
    }

    /// Section header bar: accent left rule, back chevron, title, and an
    /// optional right-hand action. Returns the y where content should begin.
    /// Height must match the reserve in `header_height`.
    fn header(&mut self, dc: HDC, rc: &RECT, y: i32, title: &str, accent: u32) -> i32 {
        self.header_ex(dc, rc, y, title, accent, None, false)
    }

    /// The one header implementation. `right` adds a trailing label whose hit
    /// is registered *before* the bar's Back hit, so a click there is not
    /// swallowed. `fixed` skips hover shading for headers that are painted
    /// over scrolled content rather than scrolling with it.
    ///
    /// The chevron is drawn as a vector rather than a `‹` glyph: glyphs centre
    /// their font *cell*, not their ink, so a glyph run never lines up with
    /// adjacent text set in a different font. Here both are placed off one
    /// baseline computed from the title's own metrics.
    fn header_ex(
        &mut self,
        dc: HDC,
        rc: &RECT,
        y: i32,
        title: &str,
        accent: u32,
        right: Option<(&str, u32, Action)>,
        fixed: bool,
    ) -> i32 {
        let pad = self.s(12);
        let h = self.s(40);
        let bar = RECT { left: pad, top: y, right: rc.right - pad, bottom: y + h };
        if fixed {
            gdi::fill(dc, &bar, gdi::t().card);
        } else {
            self.hover_fill(dc, &bar, gdi::t().card);
        }
        let rule = RECT { left: pad, top: y + self.s(6), right: pad + self.s(3), bottom: y + h - self.s(6) };
        gdi::fill(dc, &rule, accent);

        // Centre the title's ascent-to-baseline block in the bar, then hang
        // everything else off that baseline.
        let (asc, desc, ilead) = gdi::text_metrics(dc, self.fonts.title);
        let baseline = y + (h + asc - desc) / 2;
        let title_y = baseline - asc;
        // Middle of the capitals, which is what the eye reads as "centre".
        let ink_cy = baseline - (asc - ilead) / 2;

        // 14 u at 1.7, which is the spec's chevron. It was 11 at a 2 px stroke:
        // shorter and heavier than the drawing, so it read as a chunky arrow.
        let csz = self.s(14);
        let ccx = pad + self.s(17);
        gdi::chevron(dc, ccx, ink_cy, csz, self.s(1).max(1) + 1, gdi::t().dim, true);
        let title_x = ccx + csz / 2 + self.s(10);

        // Right-hand action first: its hit must beat the bar's Back hit.
        let mut title_right = rc.right - pad - self.s(10);
        if let Some((label, color, action)) = right {
            let lw = gdi::text_width(dc, self.fonts.micro, label);
            let lx = rc.right - pad - self.s(12) - lw;
            // Same baseline as the title, so the two runs sit level.
            let (lasc, _, _) = gdi::text_metrics(dc, self.fonts.micro);
            gdi::text(dc, lx, baseline - lasc, self.fonts.micro, color, label);
            self.hits.push((
                RECT { left: lx - self.s(8), top: y, right: rc.right - pad, bottom: y + h },
                action,
            ));
            title_right = lx - self.s(10);
        }

        gdi::text_fit(
            dc,
            title_x,
            title_y,
            title_right,
            &[self.fonts.title, self.fonts.value, self.fonts.value_sm],
            gdi::t().text,
            title,
        );
        self.hits.push((bar, Action::Back));
        y + h + self.s(10)
    }

    fn header_height(&self) -> i32 {
        self.s(40 + 10)
    }

    /// Height of the drill-down's hero plate plus the gap below it.
    ///
    /// Measured during the paint and cached, because the callers that need it
    /// for layout — `filter_input_y`, and through it the EDIT child's position —
    /// have no device context to measure with. The fallback only applies to the
    /// very first paint.
    fn hero_h(&self) -> i32 {
        if self.hero_h > 0 { self.hero_h } else { self.s(HERO_H_GUESS + SP4) }
    }

    /// The ring, accent and scale behind a hero chart.
    fn hero_series(&self, h: Hero) -> (&Ring, u32, Scale, &'static str) {
        match h {
            Hero::Fps => (&self.hist_fps, gdi::acc().fps, Scale::Fps, "Frame rate"),
            Hero::M(Metric::Cpu) => (&self.hist_cpu, gdi::acc().cpu, Scale::Percent, "Processor"),
            Hero::M(Metric::Ram) => (&self.hist_mem, gdi::acc().ram, Scale::Percent, "Memory"),
            Hero::M(Metric::Gpu) => (&self.hist_gpu, gdi::acc().gpu, Scale::Percent, "Graphics"),
            Hero::M(Metric::Disk) => (&self.hist_disk, gdi::acc().disk, Scale::Rate, "Disk"),
            Hero::M(Metric::Net) => (&self.hist_net, gdi::acc().net, Scale::Rate, "Network"),
            Hero::M(Metric::Audio) => (&self.hist_audio, gdi::acc().audio, Scale::Percent, "Sound"),
        }
    }

    /// The hero chart: one plate carrying the metric's name, its current value
    /// at the display step, the window, the ceiling, a peak marker and — on
    /// hover — the sample under the cursor with its age.
    ///
    /// The age is relative on purpose. `23s ago` answers the question a monitor
    /// is asked; `14:22:07` does not.
    fn draw_hero(&mut self, dc: HDC, rc: &RECT, y: i32, h: Hero) -> i32 {
        let pad = self.s(SP4);
        // Everything below is laid out from measured text, so the plate is
        // exactly as tall as its contents need and the plot cannot escape it.
        let (d_asc, d_desc, _) = gdi::text_metrics(dc, self.fonts.display);
        let (l_asc, l_desc, _) = gdi::text_metrics(dc, self.fonts.label);
        let (m_asc, m_desc, _) = gdi::text_metrics(dc, self.fonts.micro);
        let head_h = self.s(SP3) + (l_asc + l_desc) + self.s(SP1) + (d_asc + d_desc);
        let plot_h = self.s(HERO_PLOT_H);
        // Disk and Network carry a second figure under the first, so the plate
        // is a line taller for them.
        let two_way = matches!(h, Hero::M(Metric::Disk) | Hero::M(Metric::Net));
        let second_line = if two_way { self.s(SP1) + (m_asc + m_desc) } else { 0 };
        let plate_h = head_h
            + second_line
            + self.s(SP4)
            + plot_h
            + self.s(SP1)
            + (m_asc + m_desc)
            + self.s(SP3);
        self.hero_h = plate_h + self.s(SP4);
        let plate = RECT { left: pad, top: y, right: rc.right - pad, bottom: y + plate_h };
        gdi::card(dc, &plate, gdi::t().card, gdi::t().line, self.s(RADIUS));

        let plot_top = plate.top + head_h + second_line + self.s(SP4);
        let plot = RECT {
            left: plate.left + self.s(SP4),
            top: plot_top,
            right: plate.right - self.s(SP4),
            bottom: plot_top + plot_h,
        };

        // Everything that mutates `self` happens before the ring is borrowed for
        // drawing: `held` comes from a scoped borrow so the pin can be dropped
        // and the hit registered without holding a reference across either.
        let held = {
            let (ring, _, _, _) = self.hero_series(h);
            ring.iter().count()
        };
        // A pin that has scrolled off the left of the window is dropped, which
        // is the one case where it should not survive a refresh.
        if self.pin_back.is_some_and(|b| b + 1 > held) {
            self.pin_back = None;
        }
        // The plot is clickable, so a peak can be pinned and read at leisure.
        self.hero_plot = plot;
        self.hits.push((plot, Action::PinHero));

        let (ring, accent, scale, kind) = self.hero_series(h);
        let cap = ring.capacity();
        // Only the rate scales consult a sticky ceiling; percent and FPS resolve
        // from the scale alone, so which one is passed for them is immaterial.
        let sticky = if h == Hero::M(Metric::Disk) { &self.ceil_disk } else { &self.ceil_net };
        let ceiling = scale.ceiling(ring.max(), sticky);
        let latest = ring.iter().last().unwrap_or(0.0);

        // Hovering wins while the cursor is over the plot; otherwise a pinned
        // sample holds, and only with neither does the readout follow the live
        // value. `hover_pos` is already tracked and already forces a repaint on
        // change, so this costs nothing new to drive.
        let hovered = self
            .hover_pos
            .filter(|(_, hy)| *hy >= plot.top && *hy < plot.bottom)
            .and_then(|(hx, _)| gdi::chart_hit(&plot, cap, held, hx));
        let pinned = self.pin_back.map(|b| held.saturating_sub(1).saturating_sub(b));
        let hit = hovered.or(pinned);
        let shown = hit.and_then(|i| ring.iter().nth(i)).unwrap_or(latest);

        // The second direction at the same sample, so a pinned peak reports both
        // halves of the chart rather than only the half that happens to be on
        // top. Read straight from the mirrored ring, so the two figures always
        // describe the same instant.
        let second: Option<(f32, &'static str, u32)> = match h {
            Hero::M(Metric::Disk) => Some((
                hit.and_then(|i| self.hist_disk_w.iter().nth(i))
                    .unwrap_or_else(|| self.hist_disk_w.iter().last().unwrap_or(0.0)),
                "write",
                gdi::acc().disk_w,
            )),
            Hero::M(Metric::Net) => Some((
                hit.and_then(|i| self.hist_net_tx.iter().nth(i))
                    .unwrap_or_else(|| self.hist_net_tx.iter().last().unwrap_or(0.0)),
                "up",
                gdi::acc().net_tx,
            )),
            _ => None,
        };
        let primary_unit = match h {
            Hero::M(Metric::Disk) => Some("read"),
            Hero::M(Metric::Net) => Some("down"),
            _ => None,
        };

        let interval = self.cfg.interval_ms.max(1);
        let age = match hit {
            Some(i) => {
                let back = held.saturating_sub(1).saturating_sub(i) as u32;
                if back == 0 {
                    "now".to_string()
                } else {
                    format!("\u{2212}{}s", back * interval / 1000)
                }
            }
            None => "now".to_string(),
        };

        gdi::text_t(
            dc,
            plate.left + self.s(SP4),
            plate.top + self.s(SP3),
            self.fonts.label,
            self.s(gdi::TRACK_LABEL),
            accent,
            &kind.to_uppercase(),
        );
        let vs = match scale {
            Scale::Percent => format_pct(shown),
            _ => format_rate(shown as u64),
        };
        let vy = plate.top + self.s(SP3) + (l_asc + l_desc) + self.s(SP1);
        gdi::text(dc, plate.left + self.s(SP4), vy, self.fonts.display, gdi::t().text, &vs);
        // The word naming the primary direction sits on the big figure's
        // baseline, the same way a metric row's unit does.
        let mut after = plate.left + self.s(SP4) + gdi::text_width(dc, self.fonts.display, &vs);
        if let Some(u) = primary_unit {
            gdi::text_t(
                dc,
                after + self.s(SP2),
                vy + d_asc - m_asc,
                self.fonts.micro,
                self.s(gdi::TRACK_MICRO),
                gdi::t().mute,
                u,
            );
            after += self.s(SP2) + gdi::text_width(dc, self.fonts.micro, u);
        }
        let _ = after;
        // The second direction, under the first and in its own trace's colour,
        // so the readout maps onto the chart without needing a legend.
        if let Some((v2, u2, c2)) = second {
            let s2 = format_rate(v2 as u64);
            let y2 = vy + (d_asc + d_desc) + self.s(SP1);
            gdi::text(dc, plate.left + self.s(SP4), y2, self.fonts.micro, c2, &s2);
            gdi::text_t(
                dc,
                plate.left + self.s(SP4) + gdi::text_width(dc, self.fonts.micro, &s2) + self.s(SP2),
                y2,
                self.fonts.micro,
                self.s(gdi::TRACK_MICRO),
                gdi::t().mute,
                u2,
            );
        }
        // "pinned" replaces the age when a sample is held, because the age of a
        // pinned sample keeps changing and the reading does not — saying "23s
        // ago" next to a figure frozen at 14s ago would be a lie that grows.
        let right_label = if hovered.is_none() && self.pin_back.is_some() {
            format!("{} · pinned", age)
        } else {
            age.clone()
        };
        gdi::text_right_t(
            dc,
            plate.right - self.s(SP4),
            plate.top + self.s(SP3),
            self.fonts.micro,
            self.s(gdi::TRACK_MICRO),
            if hovered.is_none() && self.pin_back.is_some() { gdi::t().dim } else { gdi::t().mute },
            &right_label,
        );

        gdi::chart(
            dc,
            self.surf.as_ref(),
            &plot,
            ring,
            ceiling,
            accent,
            gdi::ChartSize::Hero,
            self.scale,
            hit,
            Some(self.fonts.micro),
            chart_units(scale),
            // The drill-down plot is the one place with room for the permanent
            // direction labels, so this is where a two-way metric finally reads
            // as two directions rather than one summed line.
            match h {
                Hero::M(Metric::Disk) => Some(gdi::Mirror {
                    ring: &self.hist_disk_w,
                    label_hi: "Read",
                    label_lo: "Write",
                    color: gdi::acc().disk_w,
                    ceiling: self.ceil_disk_w.peek(),
                }),
                Hero::M(Metric::Net) => Some(gdi::Mirror {
                    ring: &self.hist_net_tx,
                    label_hi: "Down",
                    label_lo: "Up",
                    color: gdi::acc().net_tx,
                    ceiling: self.ceil_net_tx.peek(),
                }),
                _ => None,
            },
        );

        // Window and ceiling labels. The window is derived, not hardcoded —
        // this is the first time the panel says what its own graphs cover.
        let secs = cap as u32 * interval / 1000;
        gdi::text_t(
            dc,
            plot.left,
            plot.bottom + self.s(SP1),
            self.fonts.micro,
            self.s(gdi::TRACK_MICRO),
            gdi::t().mute,
            &format!("{}s", secs),
        );
        y + self.hero_h()
    }

    /// Height of a nav row, and the stride to the next thing below it.
    fn nav_row_h(&self) -> i32 {
        self.s(ROW_NAV)
    }
    fn nav_row_stride(&self) -> i32 {
        self.s(ROW_NAV_STRIDE)
    }

    /// A slim row that goes somewhere rather than reporting a number. Used
    /// for "Connections" in both the app detail and the network drill-down,
    /// so the two ways in look like the same control. Deliberately shorter
    /// than a metric row: it has no value and no sparkline, and matching
    /// their height would just read as a sparkline that failed to load.
    fn nav_row(&mut self, dc: HDC, rc: &RECT, y: i32, label: &str, action: Action) -> i32 {
        let pad = self.s(12);
        let row = RECT { left: pad, top: y, right: rc.right - pad, bottom: y + self.nav_row_h() };
        self.hover_fill(dc, &row, gdi::t().card);
        gdi::text(dc, row.left + self.s(10), y + self.s(7), self.fonts.micro, gdi::acc().net, label);
        let chev = "›";
        let cw = gdi::text_width(dc, self.fonts.micro, chev);
        gdi::text(
            dc,
            row.right - self.s(10) - cw,
            y + self.s(7),
            self.fonts.micro,
            gdi::t().dim,
            chev,
        );
        self.hits.push((row, action));
        y + self.nav_row_stride()
    }

    /// Y of the drill-down filter EDIT control (below the header). The
    /// network drill-down carries a Connections row above its filter, so the
    /// input — a real EDIT child positioned from this in `update_edit` —
    /// starts that much further down there and nowhere else.
    fn filter_input_y(&self) -> i32 {
        let nav = if matches!(self.view, View::Drill(Metric::Net)) {
            self.nav_row_stride()
        } else {
            0
        };
        let hero = if matches!(self.view, View::Drill(_)) { self.hero_h() } else { 0 };
        self.s(12) + self.header_height() + hero + self.s(3) + nav
    }

    fn draw_drill(&mut self, dc: HDC, rc: &RECT, metric: Metric) {
        let pad = self.s(12);
        let mut y = pad;
        let (title, accent) = match metric {
            Metric::Cpu => ("Top apps by CPU use", gdi::acc().cpu),
            Metric::Ram => ("Top apps by RAM use", gdi::acc().ram),
            Metric::Gpu => ("Top apps by GPU use", gdi::acc().gpu),
            Metric::Disk => ("Top apps by disk activity", gdi::acc().disk),
            Metric::Net => ("Top apps by network use", gdi::acc().net),
            Metric::Audio => ("Apps playing sound", gdi::acc().audio),
        };
        y = self.header(dc, rc, y, title, accent);
        y = self.draw_hero(dc, rc, y, Hero::M(metric));

        // The network list answers "how much"; the connections list answers
        // "to whom". It gets a row of its own rather than a word tucked into the
        // header, where it read as decoration and was easy to miss. Below the
        // graph and above the filter: the graph belongs to the header it
        // describes, and a nav row wedged between the two separated them.
        // `filter_input_y` already accounts for both, so the sum is unchanged.
        if metric == Metric::Net {
            y = self.nav_row(dc, rc, y, "Network connections", Action::ShowConns);
        }

        // Paused shows a frozen snapshot so fast-moving rows can be clicked.
        let snap = if self.paused {
            self.frozen.clone().unwrap_or_else(|| self.snap.clone())
        } else {
            self.snap.clone()
        };

        // Filter row: framed input + pause toggle. The EDIT child is positioned
        // in update_edit at (pad + SP3 + GLYPH + SP2, filter_input_y()) — inside
        // this frame, clear of the magnifier drawn at its left.
        // No "Filter" label: it was 10 px of low-contrast word doing a job the
        // magnifier inside the frame does without costing any field width.
        let top = self.filter_input_y() - self.s(3);
        let btn = self.s(26);
        let frame = RECT {
            left: pad,
            top,
            right: rc.right - pad - btn,
            bottom: top + self.ctrl_h(),
        };
        gdi::input_frame(dc, &frame);
        self.search_glyph(dc, &frame);
        self.clear_button(dc, &frame);
        // Pause / resume toggle at the far right of the filter row. Its hit is
        // inserted at the front so no other rect can swallow the click.
        // Paused shows the play glyph — the button says what it will do next,
        // not what state it is in. The accent ground carries the state.
        let pbtn = RECT {
            left: rc.right - pad - self.ctrl_h(),
            top,
            right: rc.right - pad,
            bottom: top + self.ctrl_h(),
        };
        let hot = self.hovered(&pbtn);
        let ground = if self.paused {
            gdi::acc().fps
        } else if hot {
            gdi::t().card_hover
        } else {
            gdi::t().card
        };
        gdi::card(dc, &pbtn, ground, gdi::t().line, self.s(RADIUS));
        let gcol = if self.paused { gdi::on(ground) } else { gdi::t().dim };
        gdi::icon(
            dc,
            (pbtn.left + pbtn.right) / 2,
            (pbtn.top + pbtn.bottom) / 2,
            self.s(GLYPH),
            self.s(1).max(1),
            gcol,
            ground,
            if self.paused { gdi::Icon::Play } else { gdi::Icon::Pause },
        );
        self.hits.insert(0, (pbtn, Action::TogglePause));
        y += self.s(32);
        if self.paused {
            gdi::text(dc, pad, y - self.s(2), self.fonts.micro, gdi::acc().fps, "Paused");
            y += self.s(16);
        }

        // Per-core grid at the top of the CPU drill-down.
        if metric == Metric::Cpu && !snap.core_pcts.is_empty() {
            let cores = snap.core_pcts.clone();
            self.heading(dc, pad, y, &format!("CPU CORES · {}", cores.len()));
            y += self.s(SP5);
            let cols: i32 = if cores.len() <= 8 { 2 } else if cores.len() <= 32 { 4 } else { 8 };
            let gap = self.s(SP3);
            let bar_w = (rc.right - rc.left - 2 * pad - (cols - 1) * gap) / cols;
            let mut readout: Option<String> = None;
            for (i, pct) in cores.iter().enumerate() {
                let col = (i as i32) % cols;
                let row_i = (i as i32) / cols;
                let bx = pad + col * (bar_w + gap);
                let by = y + row_i * self.s(SP4);
                let r = RECT { left: bx, top: by, right: bx + bar_w, bottom: by + self.s(SP3) };
                gdi::bar(dc, &r, pct / 100.0, gdi::acc().cpu);
                // One pixel per core that turns a snapshot into a window.
                // Only worth a mark when the peak is meaningfully above the
                // current fill, and in `dim` rather than `text`: at full ink on
                // a mostly-empty bar it read as a defect rather than a marker.
                let peak = self.core_hist.get(i).map(|h| h.max()).unwrap_or(0.0);
                if peak > pct + 4.0 {
                    let px = r.left + (((r.right - r.left) as f32) * (peak / 100.0).min(1.0)) as i32;
                    let tick = RECT {
                        left: px.min(r.right - 1),
                        top: r.top,
                        right: (px + 1).min(r.right),
                        bottom: r.bottom,
                    };
                    gdi::fill(dc, &tick, gdi::t().dim);
                }
                // Hover names the core. The grid carries no per-cell labels
                // otherwise: at four to eight columns there is no room for
                // sixteen numbers, and unlabelled rules would be decoration.
                let hit = RECT {
                    left: r.left,
                    top: r.top - self.s(SP1),
                    right: r.right,
                    bottom: r.bottom + self.s(SP1),
                };
                if self.hovered(&hit) {
                    readout =
                        Some(format!("core {} · {:.0}% now · {:.0}% peak in 60s", i, pct, peak));
                }
                // The cell has to be a hit for the readout to work at all:
                // WM_MOUSEMOVE only repaints when the hovered hit *index*
                // changes, so cells outside `hits` never triggered one and the
                // readout stayed on its placeholder however you moved.
                self.hits.push((hit, Action::HoverCore));
            }
            let rows_n = (cores.len() as i32 + cols - 1) / cols;
            y += rows_n * self.s(SP4) + self.s(SP1);
            gdi::text_t(
                dc,
                pad,
                y,
                self.fonts.micro,
                self.s(gdi::TRACK_MICRO),
                gdi::t().mute,
                readout.as_deref().unwrap_or("hover a core for its number and peak"),
            );
            y += self.s(SP5);
        }

        if metric == Metric::Net && !snap.etw_ok {
            gdi::text(dc, pad, y, self.fonts.body, gdi::t().dim, "Network usage per app needs administrator access.");
            gdi::text(dc, pad, y + self.s(20), self.fonts.micro, gdi::t().dim, "Restart Resource Monitor as administrator, or turn on");
            gdi::text(dc, pad, y + self.s(34), self.fonts.micro, gdi::t().dim, "\"Start with Windows\" in settings, which runs it elevated.");
            return;
        }
        if metric == Metric::Gpu && !snap.gpu_ok {
            gdi::text(dc, pad, y, self.fonts.body, gdi::t().dim, "Your GPU does not report usage data.");
            return;
        }
        if snap.procs.is_empty() {
            gdi::text(dc, pad, y, self.fonts.body, gdi::t().dim, "Collecting data…");
            return;
        }

        self.drawn_rows = top_by(&snap.procs, metric, 50, &self.filter);
        if self.drawn_rows.is_empty() {
            gdi::text(dc, pad, y, self.fonts.body, gdi::t().dim, "No apps are using this right now");
            if metric == Metric::Net && !snap.net_ok {
                gdi::text(dc, pad, y + self.s(20), self.fonts.micro, gdi::t().dim, "You may need to run as administrator to see");
                gdi::text(dc, pad, y + self.s(34), self.fonts.micro, gdi::t().dim, "which apps are using the network.");
            }
            return;
        }
        let rows = self.drawn_rows.clone();
        // Disk and Network show both directions, which needs a second line, so
        // their rows take the taller stride the connections list already uses.
        // Every other metric is one number and keeps the compact row.
        let two_way = matches!(metric, Metric::Disk | Metric::Net);
        let row_h = if two_way { self.s(38) } else { self.s(ROW_LIST) };
        let list_top = y;
        let name_fonts = [self.fonts.body, self.fonts.micro];
        // Clip the scrolling list to its own region so partially scrolled
        // rows never draw over the filter/header above it.
        let saved = unsafe { SaveDC(dc) };
        unsafe { IntersectClipRect(dc, 0, list_top, rc.right, rc.bottom) };
        for (idx, (name, _pids)) in rows.iter().enumerate() {
            let ry = list_top + idx as i32 * row_h - self.scroll;
            if ry + row_h < list_top || ry > rc.bottom - self.s(20) {
                continue;
            }
            let value = row_value(&snap.procs, name, metric);
            let pair = row_pair(&snap.procs, name, metric);
            // The unit that names the primary figure, and the one that names the
            // secondary. Disk takes words because read and write are not
            // directions — an arrow beside a read rate would imply the disk is
            // downloading — and the network takes the direction markers.
            let (u1, u2) = match (two_way, metric) {
                (true, Metric::Disk) => (Unit::Word("read"), Unit::Word("write")),
                (true, _) => (Unit::Down, Unit::Up),
                _ => (Unit::None, Unit::None),
            };
            let vs = match metric {
                Metric::Cpu | Metric::Gpu => format!("{:.1}%", value),
                Metric::Ram => format_bytes(value as u64),
                // Sound just lists which apps are playing; a level percentage adds noise.
                Metric::Audio => String::new(),
                // The direction, not the total: a lone rate that could be either
                // read or write tells you less than the pair does.
                Metric::Disk | Metric::Net => match pair {
                    Some((first, _)) => format_rate(first as u64),
                    None => format_rate(value as u64),
                },
            };
            let kx = rc.right - pad - self.s(18);
            let row_bg = RECT {
                left: pad,
                top: ry - self.s(2),
                right: rc.right - pad,
                bottom: ry + if two_way { self.s(32) } else { self.s(22) },
            };
            if self.hovered(&row_bg) {
                gdi::fill(dc, &row_bg, gdi::t().card_hover);
            }
            // The unit is reserved out of the figure column before the value is
            // placed, so a unit can never push the number under the kill glyph.
            let u1w = self.unit_w(dc, u1);
            let vw = gdi::text_width(dc, self.fonts.value_sm, &vs);
            let vx = kx - self.s(10) - u1w - vw;
            gdi::text_fit(dc, pad + self.s(4), ry, vx - self.s(8), &name_fonts, gdi::t().text, name);
            gdi::text(dc, vx, ry, self.fonts.value_sm, gdi::t().text, &vs);
            self.draw_unit_after(dc, vx + vw + self.s(SP2), ry, self.fonts.value_sm, u1);
            if let Some((_, second)) = pair {
                // Bottom line of the row, in `micro`. A figure, so it takes
                // `text` like the value above it and is told apart by size.
                let ss = format_rate(second as u64);
                let u2w = self.unit_w(dc, u2);
                let sw = gdi::text_width(dc, self.fonts.micro, &ss);
                let sx = kx - self.s(10) - u2w - sw;
                let sy = ry + self.s(17);
                gdi::text(dc, sx, sy, self.fonts.micro, gdi::t().text, &ss);
                self.draw_unit_after(dc, sx + sw + self.s(SP2), sy, self.fonts.micro, u2);
            }
            self.kill_glyph(dc, kx, ry);
            // Follows the row's own height, or a two-way row's second line would
            // sit outside the hit that opens the app it belongs to.
            let name_hit = RECT {
                left: pad,
                top: (ry - self.s(2)).max(list_top),
                right: kx - self.s(6),
                bottom: ry + if two_way { self.s(32) } else { self.s(22) },
            };
            self.hits.push((name_hit, Action::Watch(idx)));
            let kill_hit = RECT {
                left: kx - self.s(6),
                top: (ry - self.s(4)).max(list_top),
                right: rc.right - pad + self.s(4),
                bottom: ry + self.s(20),
            };
            self.hits.push((kill_hit, Action::Kill(idx)));
        }
        self.scrollbar(dc, rc, list_top, rows.len() as i32 * row_h);
        unsafe { RestoreDC(dc, saved) };
    }

    /// Join the last sweep with process names and resolved hostnames.
    ///
    /// Loopback is dropped: on a normal desktop it is dozens of rows of the
    /// machine talking to itself, and it buries the answer to the question
    /// this view exists for. The footer says so rather than leaving the count
    /// silently short.
    fn refresh_conns(&mut self) {
        let table = self.shared.conns.lock().unwrap().clone();
        self.conns_swept = table.swept_ms;
        let process_of: HashMap<u32, String> =
            self.snap.procs.iter().map(|p| (p.pid, p.name.clone())).collect();
        let filter = conns::Filter {
            process: self.conns_for.clone(),
            scope: conns::ScopeFilter::All,
            ..Default::default()
        };
        let rows: Vec<conns::Conn> = table
            .rows
            .into_iter()
            .filter(|c| {
                c.remote_ip()
                    .map_or(false, |ip| conns::scope_of(&ip) != conns::Scope::Loopback)
            })
            .collect();
        let names = self.shared.names.lock().unwrap();
        let (rows, total) = conns::build_rows(
            &rows,
            &process_of,
            &names,
            &filter,
            conns::MAX_LIMIT,
        );
        self.conn_rows = rows;
        self.conn_total = total;
    }

    /// Text typed in the filter box, matched across everything on the row so
    /// one box serves "edge", "asus.com", "204.79." and "443" alike.
    fn conn_matches_filter(row: &conns::Row, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }
        let c = &row.conn;
        if row.process.to_lowercase().contains(filter) {
            return true;
        }
        if let Some(h) = &row.host {
            if h.contains(filter) {
                return true;
            }
        }
        if let Some((ip, port)) = &c.remote {
            if ip.to_string().contains(filter) || port.to_string() == filter {
                return true;
            }
        }
        c.pid.to_string() == filter
    }

    fn draw_conns(&mut self, dc: HDC, rc: &RECT) {
        let pad = self.s(12);
        let title = match &self.conns_for {
            Some(app) => format!("{} network connections", app),
            None => "Live network connections".to_string(),
        };
        // The y the header hands back is deliberately dropped: the filter frame
        // below is placed from `filter_input_y()`, and everything under it is
        // anchored to that frame's own bottom edge, so the header and the field
        // cannot drift into each other the way they just did.
        self.header(dc, rc, pad, &title, gdi::acc().net);
        let mut y;

        // Filter row, same geometry as a drill-down's but with no pause
        // toggle: the list is already only as fast as the sweep.
        // No "Filter" label: it was 10 px of low-contrast word doing a job the
        // magnifier inside the frame does without costing any field width.
        let top = self.filter_input_y() - self.s(3);
        let frame = RECT {
            // `pad`, exactly as the drill-down's filter does it. This used to be
            // `pad + s(44)`, which was the inset the removed "Filter" label had
            // needed. The label went; the inset stayed. The EDIT child is
            // positioned at `pad + SP3 + GLYPH + SP2` by `update_edit`, so the
            // frame was starting 19 px to the *right* of its own text and the
            // placeholder hung out of the left-hand edge.
            left: pad,
            top,
            right: rc.right - pad,
            bottom: top + self.ctrl_h(),
        };
        gdi::input_frame(dc, &frame);
        // The magnifier is what earns the removal of the word "Filter", and it
        // was never drawn here — so this field had neither.
        self.search_glyph(dc, &frame);
        self.clear_button(dc, &frame);
        // Anchored to the field's own bottom rather than a stride guessed from
        // the header, so the summary cannot creep back up against it: the old
        // `s(32)` left 8 px, which read as the empty state touching the box.
        y = frame.bottom + self.s(SP5);

        if self.conns_swept == 0 {
            gdi::text(dc, pad, y, self.fonts.body, gdi::t().dim, "Collecting connections…");
            return;
        }

        let filter = self.filter.clone();
        // Taken rather than cloned: the list can be hundreds of rows and this
        // runs on every paint, including plain mouse movement.
        let all = std::mem::take(&mut self.conn_rows);
        let rows: Vec<&conns::Row> =
            all.iter().filter(|r| Self::conn_matches_filter(r, &filter)).collect();

        let named = rows.iter().filter(|r| r.host.is_some()).count();
        let summary = if self.conn_total == 0 {
            "Nothing is connected right now".to_string()
        } else {
            format!("{} shown · {} named · loopback hidden", rows.len(), named)
        };
        gdi::text(dc, pad, y, self.fonts.micro, gdi::t().dim, &summary);
        y += self.s(18);
        // Without the DNS provider only reverse lookups can name anything, so
        // say why the column is thin rather than letting it read as "nothing
        // to see here".
        if !self.shared.etw.dns_ok.load(Ordering::Relaxed) {
            gdi::text(
                dc,
                pad,
                y,
                self.fonts.micro,
                gdi::t().dim,
                "Host names need administrator access.",
            );
            y += self.s(16);
        }

        self.drawn_rows = rows
            .iter()
            .map(|r| (r.process.clone(), vec![r.conn.pid]))
            .collect();

        let row_h = self.s(38);
        let list_top = y;
        let name_fonts = [self.fonts.body, self.fonts.micro];
        let saved = unsafe { SaveDC(dc) };
        unsafe { IntersectClipRect(dc, 0, list_top, rc.right, rc.bottom) };
        for (idx, r) in rows.iter().enumerate() {
            let ry = list_top + idx as i32 * row_h - self.scroll;
            if ry + row_h < list_top || ry > rc.bottom - self.s(20) {
                continue;
            }
            let c = &r.conn;
            let row_bg = RECT {
                left: pad,
                top: ry - self.s(2),
                right: rc.right - pad,
                bottom: ry + row_h - self.s(6),
            };
            if self.hovered(&row_bg) {
                gdi::fill(dc, &row_bg, gdi::t().card_hover);
            }
            // Right of the first line: the port, and its protocol when the
            // port is one anybody would recognise.
            let port = c.remote_port().unwrap_or(c.local_port);
            let right = match conns::service_name(port) {
                Some(s) => format!("{}  {}", port, s),
                None => port.to_string(),
            };
            let rw = gdi::text_width(dc, self.fonts.value_sm, &right);
            let rx = rc.right - pad - self.s(6) - rw;
            gdi::text(dc, rx, ry, self.fonts.value_sm, gdi::t().text, &right);
            gdi::text_fit(
                dc,
                pad + self.s(4),
                ry,
                rx - self.s(8),
                &name_fonts,
                gdi::t().text,
                &r.process,
            );

            // Second line: what it is talking to. The name when we have one,
            // with the address still shown on the right so the row is never
            // only as trustworthy as the name.
            let addr = c.remote_ip().map(|i| i.to_string()).unwrap_or_default();
            let (left_text, right_text) = match &r.host {
                Some(h) => (h.clone(), addr),
                None => (addr, String::new()),
            };
            let sy = ry + self.s(17);
            let mut right_edge = rc.right - pad - self.s(6);
            if !right_text.is_empty() {
                let w = gdi::text_width(dc, self.fonts.micro, &right_text);
                gdi::text(dc, right_edge - w, sy, self.fonts.micro, gdi::t().dim, &right_text);
                right_edge -= w + self.s(10);
            }
            // A state worth mentioning is one that is not simply "connected".
            let state = conns::state_name(c.state);
            let label = if c.proto == conns::Proto::Udp {
                "udp".to_string()
            } else if state == "established" {
                String::new()
            } else {
                state.to_string()
            };
            if !label.is_empty() {
                let w = gdi::text_width(dc, self.fonts.micro, &label);
                gdi::text(dc, right_edge - w, sy, self.fonts.micro, gdi::t().dim, &label);
                right_edge -= w + self.s(10);
            }
            gdi::text_fit(
                dc,
                pad + self.s(4),
                sy,
                right_edge,
                &[self.fonts.micro],
                gdi::t().dim,
                &left_text,
            );

            let hit = RECT {
                left: pad,
                top: (ry - self.s(2)).max(list_top),
                right: rc.right - pad,
                bottom: ry + row_h - self.s(6),
            };
            self.hits.push((hit, Action::Watch(idx)));
        }
        self.scrollbar(dc, rc, list_top, rows.len() as i32 * row_h);
        unsafe { RestoreDC(dc, saved) };
        drop(rows);
        self.conn_rows = all;

        if self.conn_total > 0 && self.drawn_rows.is_empty() {
            gdi::text(dc, pad, list_top, self.fonts.body, gdi::t().dim, "Nothing matches that filter");
        }
    }

    fn draw_process(&mut self, dc: HDC, rc: &RECT) {
        let pad = self.s(12);
        let mut y = pad;
        let name = self.watch.clone().unwrap_or_default();

        y = self.header_ex(
            dc,
            rc,
            y,
            &name,
            gdi::acc().cpu,
            Some(("close app", gdi::rgb(230, 100, 100), Action::KillWatched)),
            false,
        );

        let count = self.snap.procs.iter().filter(|p| p.name == name).count();
        let mut status = if count == 0 {
            "Not running".to_string()
        } else {
            format!("{} running", count)
        };
        // Which cores this app is allowed on. Cheap and exact, and the useful
        // half of the per-core question — see `util::affinity_label` for why the
        // other half (which core it is *on*) is not answered here.
        if let Some(pid) = self.snap.procs.iter().find(|p| p.name == name).map(|p| p.pid) {
            if let Some((m, sys)) = crate::procinfo::affinity(pid) {
                if m != sys {
                    status.push_str(" · ");
                    status.push_str(&affinity_label(m, sys));
                }
            }
        }
        gdi::text_t(
            dc,
            pad,
            y,
            self.fonts.micro,
            self.s(gdi::TRACK_MICRO),
            gdi::t().mute,
            &status,
        );
        y += self.s(SP6);

        let a = watch_sums(&self.snap.procs, &name);
        let fps = watch_fps(&self.snap, &name);
        let mut rows: Vec<(&str, u32, gdi::Glyph, Figures, usize, Scale)> = Vec::with_capacity(6);
        // FPS leads, but only for apps that are actually presenting frames.
        if fps > 0 || self.watch_rings[5].max() > 0.0 {
            rows.push((
                "FPS",
                gdi::acc().fps,
                gdi::Glyph::Fps,
                if fps > 0 {
                    Figures::new(fps.to_string()).unit(Unit::Word("fps"))
                } else {
                    Figures::new("—".to_string())
                },
                5,
                Scale::Fps,
            ));
        }
        rows.push((
            "CPU",
            gdi::acc().cpu,
            gdi::Glyph::Cpu,
            Figures::new(format!("{:.1}%", a.cpu)),
            0,
            Scale::Percent,
        ));
        rows.push((
            "RAM",
            gdi::acc().ram,
            gdi::Glyph::Ram,
            Figures::new(format_bytes(a.ram_private)),
            1,
            // Private bytes have no ceiling of their own, so they take the same
            // sticky treatment as a rate rather than a window maximum.
            Scale::Rate,
        ));
        rows.push((
            "GPU",
            gdi::acc().gpu,
            gdi::Glyph::Gpu,
            Figures::new(format!("{:.1}%", a.gpu)),
            2,
            Scale::Percent,
        ));
        rows.push((
            "Disk",
            gdi::acc().disk,
            gdi::Glyph::Disk,
            Figures::new(format_rate(a.disk_read_bps))
                .unit(Unit::Word("read"))
                .sub(format_rate(a.disk_write_bps), Unit::Word("write")),
            3,
            Scale::Rate,
        ));
        // Sound, when the app is actually playing something. Reported as the
        // level rather than as a count: "2 playing" answers a question about the
        // machine, and this view is about one app.
        if a.audio > 0.01 || self.watch_rings[8].max() > 0.0 {
            rows.push((
                "Sound",
                gdi::acc().audio,
                gdi::Glyph::Audio,
                if a.audio > 0.01 {
                    Figures::new(format_pct(a.audio * 100.0))
                } else {
                    Figures::new("Silent".to_string())
                },
                8,
                Scale::Percent,
            ));
        }
        rows.push((
            "Network",
            gdi::acc().net,
            gdi::Glyph::Net,
            // Was the combined rate wearing a ↓ marker, which named a direction
            // the number did not have. Both halves are carried now, so each
            // marker sits on the rate it actually describes.
            Figures::new(format_rate(a.net_rx_bps))
                .unit(Unit::Down)
                .sub(format_rate(a.net_tx_bps), Unit::Up),
            4,
            Scale::Rate,
        ));
        for (label, accent, glyph, figs, ring_idx, scale) in rows {
            let row = RECT {
                left: pad,
                top: y,
                right: rc.right - pad,
                bottom: y + self.s(CARD_METRIC),
            };
            gdi::card(dc, &row, gdi::t().card, gdi::t().line, self.s(RADIUS));
            let name_right = self.metric_name(dc, &row, accent, glyph, label);
            let spark = RECT {
                left: row.right - self.s(SPARK_W),
                top: y + self.s(SP3),
                right: row.right - self.s(SP4),
                bottom: y + self.s(44),
            };
            let sticky = match ring_idx {
                1 => &self.ceil_watch_ram,
                3 => &self.ceil_watch_disk,
                _ => &self.ceil_watch_net,
            };
            let max = scale.ceiling(self.watch_rings[ring_idx].max(), sticky);
            self.draw_figures(dc, &row, name_right, spark.left - self.s(SP3), &figs);
            gdi::chart(
                dc,
                self.surf.as_ref(),
                &spark,
                &self.watch_rings[ring_idx],
                max,
                accent,
                gdi::ChartSize::Row,
                self.scale,
                None,
                None,
                chart_units(scale),
                // Disk is ring 3 paired with 6, network 4 paired with 7.
                match ring_idx {
                    3 => Some(gdi::Mirror {
                        ring: &self.watch_rings[6],
                        label_hi: "Read",
                        label_lo: "Write",
                        color: gdi::acc().disk_w,
                        ceiling: self.ceil_watch_disk_w.peek(),
                    }),
                    4 => Some(gdi::Mirror {
                        ring: &self.watch_rings[7],
                        label_hi: "Down",
                        label_lo: "Up",
                        color: gdi::acc().net_tx,
                        ceiling: self.ceil_watch_net_tx.peek(),
                    }),
                    _ => None,
                },
            );
            y += self.s(ROW_METRIC);
        }

        // Where this app is talking to, as its own item rather than a hint
        // buried in the Network row. It carries no number: connections are
        // only enumerated while the list is open, so a live count here would
        // mean sweeping forever behind a screen nobody is looking at.
        // The subprocess block below re-anchors from `proc_layout`, which
        // accounts for this row, so the returned y is deliberately dropped.
        self.nav_row(dc, rc, y, "Network connections", Action::ShowAppConns);

        // --- subprocesses: collapsed by default (a browser can be 70+ rows).
        // Expand for the individual processes, labelled by role where the
        // command line reveals one (Chromium: Browser, GPU process, Renderer,
        // Network Service, Crashpad handler, Extension...). Each row has an ×
        // to end just that process, like Task Manager.
        let subs: Vec<ProcStat> =
            self.snap.procs.iter().filter(|p| p.name == name).cloned().collect();
        if subs.len() > 1 {
            let l = self.proc_layout();
            y = l.y_subs_header;
            let hrow = RECT { left: pad, top: y, right: rc.right - pad, bottom: y + self.s(28) };
            self.hover_fill(dc, &hrow, gdi::t().card);
            gdi::disclosure(
                dc,
                pad + self.s(11),
                y + self.s(11),
                self.s(8),
                self.s(2),
                gdi::t().dim,
                self.subs_expanded,
            );
            gdi::text(
                dc,
                pad + self.s(24),
                y + self.s(4),
                self.fonts.body,
                gdi::t().text,
                &format!("Processes ({})", subs.len()),
            );
            self.hits.push((hrow, Action::ToggleSubs));

            if self.subs_expanded {
                // Filter box (the EDIT is positioned over it in update_edit).
                let frame = RECT {
                    left: pad,
                    top: l.y_filter - self.s(3),
                    right: rc.right - pad,
                    bottom: l.y_filter - self.s(3) + self.ctrl_h(),
                };
                gdi::input_frame(dc, &frame);
                self.clear_button(dc, &frame);

                // Metric chips: the active one sorts the list and supplies the
                // value column, so "which process is using sound" is one tap.
                let metrics: [(Metric, &str); 6] = [
                    (Metric::Cpu, "cpu"),
                    (Metric::Ram, "ram"),
                    (Metric::Gpu, "gpu"),
                    (Metric::Disk, "disk"),
                    (Metric::Net, "net"),
                    (Metric::Audio, "sound"),
                ];
                let mut cx = pad;
                for (m, label) in metrics {
                    cx = self.chip(dc, cx, l.y_chips, label, self.sub_metric == m, Action::SubMetric(m));
                }

                if self.proc_roles.len() > 4000 {
                    self.proc_roles.clear();
                }
                let metric = self.sub_metric;
                let filter = self.filter.to_lowercase();
                let mut labelled: Vec<(String, ProcStat)> = subs
                    .iter()
                    .map(|p| {
                        let role = if let Some(r) = self.proc_roles.get(&p.pid) {
                            r.clone()
                        } else {
                            let r = crate::procinfo::process_role(p.pid);
                            self.proc_roles.insert(p.pid, r.clone());
                            r
                        };
                        (role, p.clone())
                    })
                    .filter(|(role, p)| {
                        filter.is_empty()
                            || role.to_lowercase().contains(&filter)
                            || p.pid.to_string().contains(&filter)
                    })
                    .collect();
                labelled.sort_by(|a, b| {
                    metric_value(&b.1, metric)
                        .partial_cmp(&metric_value(&a.1, metric))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });

                let list_top = l.y_list;
                let row_h = self.s(30);
                let saved = unsafe { SaveDC(dc) };
                unsafe { IntersectClipRect(dc, 0, list_top, rc.right, rc.bottom) };
                if labelled.is_empty() {
                    gdi::text(dc, pad, list_top, self.fonts.micro, gdi::t().dim, "No processes match your filter");
                }
                for (i, (role, p)) in labelled.iter().enumerate() {
                    let ry = list_top + i as i32 * row_h - self.scroll;
                    if ry + row_h < list_top || ry > rc.bottom {
                        continue;
                    }
                    let kx = rc.right - pad - self.s(16);
                    let row_bg = RECT { left: pad, top: ry - self.s(2), right: rc.right - pad, bottom: ry + self.s(20) };
                    if self.hovered(&row_bg) {
                        gdi::fill(dc, &row_bg, gdi::t().card_hover);
                    }
                    let title = if role.is_empty() {
                        format!("PID {}", p.pid)
                    } else {
                        format!("{}  ·  PID {}", role, p.pid)
                    };
                    let vs = sub_value_text(p, metric);
                    let vw = gdi::text_width(dc, self.fonts.value_sm, &vs);
                    let vx = kx - self.s(10) - vw;
                    gdi::text_fit(
                        dc,
                        pad + self.s(6),
                        ry,
                        vx - self.s(6),
                        &[self.fonts.body, self.fonts.micro],
                        gdi::t().text,
                        &title,
                    );
                    gdi::text(dc, vx, ry, self.fonts.value_sm, accent_for(metric), &vs);
                    self.kill_glyph(dc, kx, ry);
                    let kill_hit = RECT {
                        left: kx - self.s(6),
                        top: (ry - self.s(2)).max(list_top),
                        right: rc.right - pad + self.s(4),
                        bottom: ry + self.s(20),
                    };
                    self.hits.push((kill_hit, Action::KillPid(p.pid)));
                }
                unsafe { RestoreDC(dc, saved) };
                self.scrollbar(dc, rc, list_top, labelled.len() as i32 * row_h);
            }
        }
    }

    /// × inside the right edge of a filter input; shown only while there is
    /// text to clear.
    fn clear_button(&mut self, dc: HDC, frame: &RECT) {
        if self.filter.is_empty() {
            return;
        }
        let cx = frame.right - self.s(14);
        let cy = (frame.top + frame.bottom) / 2 - self.s(8);
        gdi::icon(
            dc,
            cx + self.s(5),
            cy + self.s(7),
            self.s(11),
            self.s(1).max(1),
            gdi::t().dim,
            gdi::t().input_bg,
            gdi::Icon::Close,
        );
        let hit = RECT {
            left: frame.right - self.s(24),
            top: frame.top,
            right: frame.right,
            bottom: frame.bottom,
        };
        self.hits.push((hit, Action::ClearFilter));
    }

    fn draw_fps_apps(&mut self, dc: HDC, rc: &RECT) {
        let pad = self.s(12);
        let mut y = pad;
        y = self.header(dc, rc, y, "FPS", gdi::acc().fps);

        if !self.snap.etw_ok {
            gdi::text(dc, pad, y, self.fonts.body, gdi::t().dim, "Frame rate tracking needs administrator access.");
            return;
        }
        // The same plot every other metric gets, above the per-app list, which is
        // unchanged. Drawn after the ETW check, because without the provider
        // there is no frame data at all and an empty axis would be furniture
        // pretending to be a reading. Drawn *before* the empty-list check on
        // purpose: with nothing presenting, a flatline is the answer.
        y = self.draw_hero(dc, rc, y, Hero::Fps);
        if self.snap.fps_list.is_empty() {
            gdi::text(dc, pad, y, self.fonts.body, gdi::t().dim, "Nothing on screen");
            return;
        }

        self.drawn_rows = self
            .snap
            .fps_list
            .iter()
            .map(|(pid, name, _)| (name.clone(), vec![*pid]))
            .collect();
        let list = self.snap.fps_list.clone();
        let name_fonts = [self.fonts.body, self.fonts.micro];
        for (idx, (_pid, name, fps)) in list.iter().enumerate() {
            let vs = format!("{} fps", fps);
            let hit = RECT { left: pad, top: y - self.s(2), right: rc.right - pad, bottom: y + self.s(22) };
            if self.hovered(&hit) {
                gdi::fill(dc, &hit, gdi::t().card_hover);
            }
            let vw = gdi::text_width(dc, self.fonts.value_sm, &vs);
            gdi::text_fit(dc, pad + self.s(4), y, rc.right - pad - vw - self.s(16), &name_fonts, gdi::t().text, name);
            gdi::text_right(dc, rc.right - pad - self.s(4), y, self.fonts.value_sm, gdi::t().text, &vs);
            self.hits.push((hit, Action::Watch(idx)));
            y += self.s(26);
            if y > rc.bottom - self.s(24) {
                break;
            }
        }
    }

    /// Settings is a short menu; each row opens a focused page. Eleven
    /// sections on one scroll meant hunting for anything, so they are grouped
    /// by what they affect. Nothing is hidden — it is one step further in.
    fn draw_settings(&mut self, dc: HDC, rc: &RECT) {
        let pad = self.s(12);
        let content_top = pad + self.header_height();
        let mut y = content_top - self.scroll;

        let n = self.cfg.rule_lines.len();
        let alerts = if n == 1 { "1 rule".to_string() } else { format!("{} rules", n) };
        let pages: [(SettingsPage, &str, &str); 5] = [
            (SettingsPage::General, "General", "Start-up, update rate, theme, text size"),
            (SettingsPage::Ai, "AI tools", "Connection, notifications, history"),
            (SettingsPage::MainPanel, "Main panel", "Which metrics, what order"),
            (SettingsPage::Desktop, "Desktop extras", "Taskbar strip, FPS counter, tray"),
            (SettingsPage::Alerts, "Alerts", &alerts),
        ];
        for (page, title, sub) in pages {
            let row = RECT { left: pad, top: y, right: rc.right - pad, bottom: y + self.s(44) };
            self.hover_fill(dc, &row, gdi::t().card);
            gdi::text(dc, row.left + self.s(10), y + self.s(7), self.fonts.value_sm, gdi::t().text, title);
            gdi::text_fit(
                dc,
                row.left + self.s(10),
                y + self.s(24),
                rc.right - pad - self.s(26),
                &[self.fonts.micro],
                gdi::t().dim,
                sub,
            );
            gdi::chevron(dc, rc.right - pad - self.s(14), y + self.s(22), self.s(10), self.s(2), gdi::t().dim, false);
            self.hits.push((row, Action::OpenSettingsPage(page)));
            y += self.s(50);
        }

        let content_h = y + self.scroll - content_top;
        self.scrollbar(dc, rc, content_top, content_h);
        self.settings_header(dc, rc, "Settings");
    }

    /// Fixed header for settings and its pages, drawn last so scrolled content
    /// passes under it, with its hits moved to the front so they beat rows
    /// scrolled beneath the bar.
    fn settings_header(&mut self, dc: HDC, rc: &RECT, title: &str) {
        let pad = self.s(12);
        let bg_top = RECT { left: 0, top: 0, right: rc.right, bottom: pad };
        gdi::fill(dc, &bg_top, gdi::t().bg);
        let hits_before = self.hits.len();
        self.header_ex(dc, rc, pad, title, gdi::acc().cpu, None, true);
        let added: Vec<_> = self.hits.drain(hits_before..).collect();
        for hit in added.into_iter().rev() {
            self.hits.insert(0, hit);
        }
    }

    fn draw_settings_page(&mut self, dc: HDC, rc: &RECT, page: SettingsPage) {
        let content_top = self.s(12) + self.header_height();
        let y = content_top - self.scroll;
        let (title, y) = match page {
            SettingsPage::General => ("General", self.page_general(dc, rc, y)),
            SettingsPage::Ai => ("AI tools", self.page_ai(dc, rc, y)),
            SettingsPage::MainPanel => ("Main panel", self.page_main_panel(dc, rc, y)),
            SettingsPage::Desktop => ("Desktop extras", self.page_desktop(dc, rc, y)),
            SettingsPage::Alerts => ("Alerts", self.page_alerts(dc, rc, y)),
        };
        let content_h = y + self.scroll - content_top;
        self.scrollbar(dc, rc, content_top, content_h);
        self.settings_header(dc, rc, title);
    }

    fn page_general(&mut self, dc: HDC, rc: &RECT, mut y: i32) -> i32 {
        let pad = self.s(12);
        y = self.check_row(
            dc,
            rc,
            y,
            "Start with Windows (as administrator)",
            self.autostart_on,
            Action::ToggleAutostart,
        );
        if self.autostart_err {
            gdi::text(
                dc,
                pad + self.s(26),
                y,
                self.fonts.micro,
                gdi::rgb(220, 120, 90),
                "Needs administrator — run the app as admin once",
            );
            y += self.s(18);
        }

        y += self.s(10);
        self.heading(dc, pad, y, "HOW OFTEN TO UPDATE");
        y += self.s(22);
        for (ms, label) in [
            (500u32, "Every half second — most responsive"),
            (1000, "Every second — recommended"),
            (2000, "Every two seconds — lightest on your PC"),
        ] {
            y = self.check_row(dc, rc, y, label, self.cfg.interval_ms == ms, Action::SetInterval(ms));
        }

        y += self.s(10);
        self.heading(dc, pad, y, "THEME");
        y += self.s(22);
        let mut x = pad;
        for (i, th) in gdi::THEMES.iter().enumerate() {
            x = self.chip(dc, x, y, th.name, gdi::theme_idx() == i, Action::SetTheme(i));
        }
        y += self.ctrl_row();

        y += self.s(10);
        self.heading(dc, pad, y, "TEXT SIZE");
        y += self.s(22);
        let mut x = pad;
        for (i, (label, _)) in config::TEXT_SIZES.iter().enumerate() {
            x = self.chip(dc, x, y, label, self.cfg.text_size as usize == i, Action::SetTextSize(i));
        }
        gdi::text(
            dc,
            pad,
            y + self.ctrl_h() + self.s(4),
            self.fonts.micro,
            gdi::t().dim,
            "Scales the whole panel, on top of Windows' own scaling",
        );
        y + self.ctrl_row() + self.s(16)
    }

    fn page_ai(&mut self, dc: HDC, rc: &RECT, mut y: i32) -> i32 {
        let pad = self.s(12);
        y = self.check_row(dc, rc, y, "Allow AI tools to connect", self.cfg.mcp_enabled, Action::ToggleMcp);
        gdi::text(dc, pad + self.s(26), y, self.fonts.micro, gdi::t().dim, "Read these stats and send you messages");
        y += self.s(18);
        if !self.cfg.mcp_enabled {
            self.notify_text_y = -1;
            return y;
        }
        y += self.s(6);
        y = self.draw_mcp_connect(dc, rc, pad + self.s(26), y);

        y += self.s(10);
        // The heading carries the shared "when" stem so each row below can
        // drop its own leading "When" and read as a continuation of it.
        self.heading(dc, pad, y, "NOTIFY ME WHEN");
        y += self.s(22);
        let presets: [(u32, &str); 4] = [
            (config::NOTIFY_FINISHED, "A build or long task finishes"),
            (config::NOTIFY_ERRORS, "Something errors or fails"),
            (config::NOTIFY_INPUT, "It needs my input to continue"),
            (config::NOTIFY_VERBOSE, "Every step of note (verbose)"),
        ];
        for (bit, label) in presets {
            y = self.check_row(dc, rc, y, label, self.cfg.notify_presets & bit != 0, Action::NotifyPreset(bit));
        }

        // Custom entries are not notify triggers: config::ai_instructions
        // sends them as free-form instructions to follow. They get their own
        // heading because filing them under "notify me when" is what made the
        // input confusing in the first place.
        y += self.s(12);
        self.heading(dc, pad, y, "CUSTOM AI REQUESTS");
        y += self.s(22);
        // Each toggles like a preset, and carries an × because unlike a preset
        // it can be removed entirely.
        let custom = self.cfg.notify_custom.clone();
        for (i, (on, text)) in custom.iter().enumerate() {
            y = self.check_row_deletable(
                dc,
                rc,
                y,
                text,
                *on,
                Action::NotifyCustomToggle(i),
                Action::NotifyCustomDelete(i),
            );
        }
        y += self.s(6);
        // The EDIT child is positioned over this frame after the paint. Both
        // size from add_chip_gap so the frame and the child cannot disagree
        // about where the chip starts.
        let frame = RECT {
            left: pad,
            top: y,
            right: rc.right - pad - self.add_chip_gap(dc),
            bottom: y + self.ctrl_h(),
        };
        gdi::input_frame(dc, &frame);
        self.notify_text_y = y;
        self.notify_frame_right = frame.right;
        // Same top and same height as the frame: the chip is a control of the
        // same class, not a decoration beside one.
        self.chip(dc, frame.right + self.s(6), y, "add", true, Action::NotifyCustomAdd);
        y += self.ctrl_row();

        // Timing is not guessable from the controls, and getting it wrong
        // means waiting for a change that will never arrive in this session.
        gdi::text(dc, pad, y, self.fonts.micro, gdi::t().dim, "You may need to inform your AI tool such as");
        y += self.s(15);
        gdi::text(dc, pad, y, self.fonts.micro, gdi::t().dim, "Claude Code to recheck any new notifications");
        y += self.s(15);
        gdi::text(dc, pad, y, self.fonts.micro, gdi::t().dim, "that are added.");
        y += self.s(20);

        // Agent history is kept for the session by default; a file makes it
        // survive a restart. Empty path means off, as it does for an alert
        // rule's file.
        y += self.s(10);
        self.heading(dc, pad, y, "AGENT HISTORY");
        y += self.s(22);
        let logging = !self.cfg.agent_log_file.trim().is_empty();
        y = self.check_row(dc, rc, y, "Also save finished agents to a file", logging, Action::ToggleAgentLog);
        if logging {
            gdi::text_fit(
                dc,
                pad + self.s(26),
                y,
                rc.right - pad,
                &[self.fonts.micro],
                gdi::t().dim,
                &self.cfg.agent_log_file,
            );
            y += self.s(20);
        }
        y
    }

    fn page_main_panel(&mut self, dc: HDC, rc: &RECT, y: i32) -> i32 {
        self.draw_metric_order(dc, rc, y)
    }

    fn page_desktop(&mut self, dc: HDC, rc: &RECT, mut y: i32) -> i32 {
        let pad = self.s(12);
        self.heading(dc, pad, y, "TASKBAR WIDGET");
        y += self.s(22);
        y = self.check_row(
            dc,
            rc,
            y,
            "Show a strip of live stats on screen",
            self.cfg.widget_on,
            Action::ToggleWidget,
        );
        {
            let metrics: [(u32, &str); 7] = [
                (super::widget::M_CPU, "CPU"),
                (super::widget::M_RAM, "RAM"),
                (super::widget::M_GPU, "GPU"),
                (super::widget::M_FPS, "FPS"),
                (super::widget::M_DISK, "disk"),
                (super::widget::M_NET, "net"),
                (super::widget::M_AI, "AI"),
            ];
            let mut x = pad;
            for (bit, label) in metrics {
                x = self.chip(dc, x, y, label, self.cfg.widget_mask & bit != 0, Action::WidgetMetric(bit));
            }
            y += self.ctrl_row();
            x = self.chip(dc, pad, y, "move next to the clock", false, Action::SnapWidget);
            self.chip(dc, x, y, "reset size", false, Action::ResetWidgetSize);
            y += self.ctrl_row();

            y += self.s(6);
            self.heading(dc, pad, y, "WIDGET THEME");
            y += self.s(22);
            let mut x = pad;
            for (i, th) in gdi::THEMES.iter().enumerate() {
                x = self.chip(dc, x, y, th.name, self.cfg.widget_theme as usize == i, Action::SetWidgetTheme(i));
            }
            y += self.ctrl_row();
        }

        y += self.s(10);
        self.heading(dc, pad, y, "FRAME RATE COUNTER");
        y += self.s(22);
        y = self.check_row(
            dc,
            rc,
            y,
            "Show a floating frame rate counter",
            self.cfg.fps_overlay,
            Action::ToggleFpsOverlay,
        );
        {
            gdi::text(dc, pad, y + self.s(4), self.fonts.micro, gdi::t().dim, "color");
            let mut x = pad + self.s(50);
            for (i, (label, _)) in super::overlay::COLORS.iter().enumerate() {
                x = self.chip(dc, x, y, label, self.cfg.fps_color as usize == i, Action::FpsColor(i));
            }
            y += self.ctrl_row();
            gdi::text(dc, pad, y + self.s(4), self.fonts.micro, gdi::t().dim, "opacity");
            let mut x = pad + self.s(50);
            for (i, (label, _)) in super::overlay::OPACITIES.iter().enumerate() {
                x = self.chip(dc, x, y, label, self.cfg.fps_opacity as usize == i, Action::FpsOpacity(i));
            }
            y += self.ctrl_row();
        }

        y += self.s(10);
        self.heading(dc, pad, y, "TASKBAR TRAY ICONS");
        y += self.s(22);
        let items: [(usize, &str, bool); 6] = [
            (0, "App icon", self.cfg.tray_static),
            (1, "CPU %", self.cfg.tray_cpu),
            (2, "RAM %", self.cfg.tray_ram),
            (3, "Disk activity", self.cfg.tray_disk),
            (4, "Network speed", self.cfg.tray_net),
            (5, "FPS", self.cfg.tray_fps),
        ];
        for (kind, label, checked) in items {
            y = self.check_row(dc, rc, y, label, checked, Action::ToggleTray(kind));
        }
        y
    }

    fn page_alerts(&mut self, dc: HDC, rc: &RECT, mut y: i32) -> i32 {
        let pad = self.s(12);
        gdi::text(dc, pad, y, self.fonts.micro, gdi::t().dim, "Each alert chooses its own delivery.");
        y += self.s(22);
        let lines = self.cfg.rule_lines.clone();
        for (idx, line) in lines.iter().enumerate() {
            let rule = rules::parse_line(line);
            let enabled = rule.as_ref().map(|r| r.enabled).unwrap_or(false);
            let label = rule
                .as_ref()
                .map(rules::summary)
                .unwrap_or_else(|| format!("(invalid) {}", line));
            y = self.check_row_deletable(
                dc,
                rc,
                y,
                &label,
                enabled,
                Action::ToggleRule(idx),
                Action::DeleteRule(idx),
            );
        }
        self.chip(dc, pad, y, "+ new alert", false, Action::AddRule);
        y + self.ctrl_row()
    }

    /// Whether the watched app gets an FPS row: it shifts every row below it,
    /// so `update_edit`'s layout signature must change whenever this does.
    fn watch_has_fps(&self) -> bool {
        let name = self.watch.clone().unwrap_or_default();
        // The FPS row only appears for apps that are actually presenting.
        watch_fps(&self.snap, &name) > 0 || self.watch_rings[5].max() > 0.0
    }

    /// Shared layout for the watch view's subprocess section. `draw_process`
    /// and `update_edit` both derive from this so the filter EDIT can never
    /// drift from the frame painted under it.
    fn proc_layout(&self) -> ProcLayout {
        let rows = if self.watch_has_fps() { 6 } else { 5 };
        // The metric stack, then the Connections row, then the subprocesses.
        //
        // These must be the *same* constants the paint uses. This formula
        // carried a literal 52 for the metric stride; when `ROW_METRIC` became
        // 60 the subprocess header moved 40 px up and landed on top of the
        // Connections row, hiding it — and because its hit was registered
        // first, clicking "Processes" opened Connections instead. A parallel
        // layout formula with its own copies of the numbers is the bug, so
        // there are no numbers of its own left in it.
        let y_subs_header = self.s(12)
            + self.header_height()
            + self.s(SP6)
            + rows * self.s(ROW_METRIC)
            + self.nav_row_stride()
            + self.s(SP1);
        let y_filter = y_subs_header + self.s(36);
        let y_chips = y_filter + self.s(28);
        let y_list = y_chips + self.s(28);
        ProcLayout { y_subs_header, y_filter, y_chips, y_list }
    }

    /// Shared layout for the rule editor: y positions the EDIT children and
    /// the painted labels must agree on.
    fn rule_edit_layout(&self) -> RuleEditLayout {
        let pad = self.s(12);
        let proc = R_METRICS[self.draft.metric].0 == "proc";
        let conn = R_METRICS[self.draft.metric].0 == "conn";
        let y_metric = pad + self.header_height();
        let y_grid = y_metric + self.s(18);
        let grid_rows = (R_METRICS.len() as i32 + 1) / 2;
        // A connection rule has no direction and no threshold, so the row
        // that would say "goes above" carries its field chips instead and
        // everything below moves up by the row it does not need.
        let y_when = y_grid + grid_rows * self.s(24) + self.s(8);
        let y_name = y_when + self.s(30);
        let y_sub = y_name + self.s(26);
        let y_thresh = if conn {
            y_name
        } else if proc {
            y_sub + self.s(28)
        } else {
            y_when + self.s(30)
        };
        let y_deliver = y_thresh + self.s(30);
        // The path row only exists for "file" and "both"; when it is absent
        // everything below closes up.
        let file = self.draft.deliver != DELIVER_DESKTOP;
        let y_file = y_deliver + self.s(28);
        let y_opts = if file { y_file + self.s(28) } else { y_deliver + self.s(30) };
        let y_buttons = y_opts + self.s(30);
        RuleEditLayout { proc, conn, file, y_metric, y_grid, y_when, y_name, y_sub, y_thresh, y_deliver, y_file, y_opts, y_buttons }
    }

    fn draw_rule_edit(&mut self, dc: HDC, rc: &RECT) {
        let pad = self.s(12);
        let l = self.rule_edit_layout();

        let mut hy = pad;
        hy = self.header(dc, rc, hy, "New alert", gdi::acc().net);
        let _ = hy;

        self.heading(dc, pad, l.y_metric, "ALERT ME WHEN");
        let cols = 2;
        let cell_w = (rc.right - 2 * pad - self.s(6)) / cols;
        for (i, (_, label, _)) in R_METRICS.iter().enumerate() {
            let cx = pad + (i as i32 % cols) * (cell_w + self.s(6));
            let cy = l.y_grid + (i as i32 / cols) * self.s(24);
            let r = RECT { left: cx, top: cy, right: cx + cell_w, bottom: cy + self.s(20) };
            let active = self.draft.metric == i;
            gdi::fill(dc, &r, if active { gdi::acc().cpu } else { gdi::t().card });
            gdi::text(
                dc,
                cx + self.s(8),
                cy + self.s(3),
                self.fonts.label,
                if active { gdi::on(gdi::acc().cpu) } else { gdi::t().text },
                label,
            );
            self.hits.push((r, Action::DraftMetric(i)));
        }

        if l.conn {
            gdi::text(dc, pad, l.y_when + self.s(4), self.fonts.micro, gdi::t().dim, "match");
            let mut x = pad + self.s(48);
            for (i, (_, label)) in R_CONN_FIELDS.iter().enumerate() {
                x = self.chip(dc, x, l.y_when, label, self.draft.conn_field == i, Action::DraftConnField(i));
            }
            gdi::text(dc, pad, l.y_name + self.s(2), self.fonts.micro, gdi::t().dim, "is");
            gdi::input_frame(
                dc,
                &RECT {
                    left: pad + self.s(66),
                    top: l.y_name - self.s(3),
                    right: rc.right - pad,
                    bottom: l.y_name - self.s(3) + self.ctrl_h(),
                },
            );
        } else {
            gdi::text(dc, pad, l.y_when + self.s(4), self.fonts.micro, gdi::t().dim, "goes");
            let mut x = pad + self.s(40);
            x = self.chip(dc, x, l.y_when, "above", self.draft.gt, Action::DraftDir(true));
            self.chip(dc, x, l.y_when, "below", !self.draft.gt, Action::DraftDir(false));
        }

        if l.proc {
            gdi::text(dc, pad, l.y_name + self.s(2), self.fonts.micro, gdi::t().dim, "app");
            let field_right = rc.right - pad - self.s(52);
            gdi::input_frame(
                dc,
                &RECT {
                    left: pad + self.s(66),
                    top: l.y_name - self.s(3),
                    right: field_right,
                    bottom: l.y_name - self.s(3) + self.ctrl_h(),
                },
            );
            // "pick" opens a native dropdown of running apps.
            let pick = RECT {
                left: field_right + self.s(6),
                top: l.y_name - self.s(3),
                right: rc.right - pad,
                bottom: l.y_name + self.s(21),
            };
            gdi::fill(dc, &pick, gdi::t().card);
            gdi::text(dc, pick.left + self.s(7), l.y_name + self.s(2), self.fonts.micro, gdi::acc().cpu, "pick ▾");
            self.hits.push((pick, Action::PickApp));
            gdi::text(dc, pad, l.y_sub + self.s(4), self.fonts.micro, gdi::t().dim, "measuring");
            let mut x = pad + self.s(60);
            for (i, (_, label)) in R_PROC_SUBS.iter().enumerate() {
                x = self.chip(dc, x, l.y_sub, label, self.draft.proc_sub == i, Action::DraftProcSub(i));
            }
        }

        // A connection rule has nothing to compare, so the threshold row is
        // not drawn at all rather than drawn and ignored.
        if !l.conn {
            let unit = if l.proc { "" } else { R_METRICS[self.draft.metric].2 };
            gdi::text(dc, pad, l.y_thresh + self.s(2), self.fonts.micro, gdi::t().dim, "reaches");
            gdi::input_frame(
                dc,
                &RECT {
                    left: pad + self.s(66),
                    top: l.y_thresh - self.s(3),
                    right: pad + self.s(144),
                    bottom: l.y_thresh - self.s(3) + self.ctrl_h(),
                },
            );
            gdi::text(dc, pad + self.s(150), l.y_thresh + self.s(2), self.fonts.micro, gdi::t().dim, unit);
        }

        // Delivery is per alert. The three chips are exhaustive, which is why
        // there is no separate "optional" hint on the path row any more — that
        // label used to be drawn straight through the input frame.
        gdi::text(dc, pad, l.y_deliver + self.s(4), self.fonts.micro, gdi::t().dim, "deliver by");
        let mut x = pad + self.s(66);
        for (i, label) in ["desktop", "file", "both"].iter().enumerate() {
            x = self.chip(dc, x, l.y_deliver, label, self.draft.deliver == i, Action::DraftDeliver(i));
        }

        if l.file {
            gdi::text(dc, pad, l.y_file + self.s(2), self.fonts.micro, gdi::t().dim, "log file");
            gdi::input_frame(
                dc,
                &RECT {
                    left: pad + self.s(66),
                    top: l.y_file - self.s(3),
                    right: rc.right - pad,
                    bottom: l.y_file - self.s(3) + self.ctrl_h(),
                },
            );
        }

        let mut x = pad;
        x = self.chip(
            dc,
            x,
            l.y_opts,
            if self.draft.top { "include top apps" } else { "include top apps" },
            self.draft.top,
            Action::DraftTop,
        );
        self.chip(
            dc,
            x,
            l.y_opts,
            &format!("at most every {}", R_COOLDOWNS[self.draft.cooldown].1),
            false,
            Action::DraftCooldown,
        );

        let mut x = pad;
        x = self.chip(dc, x, l.y_buttons, "save alert", true, Action::DraftSave);
        self.chip(dc, x, l.y_buttons, "cancel", false, Action::DraftCancel);
    }

    /// A check row that can also be removed: checkbox, label, and an × at the
    /// right. Used by both the alert list and the user's own AI notification
    /// instructions, which behave identically.
    fn check_row_deletable(
        &mut self,
        dc: HDC,
        rc: &RECT,
        y: i32,
        label: &str,
        checked: bool,
        toggle: Action,
        delete: Action,
    ) -> i32 {
        let pad = self.s(12);
        let box_r = RECT {
            left: pad + self.s(4),
            top: y + self.s(2),
            right: pad + self.s(18),
            bottom: y + self.s(16),
        };
        self.check_box(dc, &box_r, checked);
        let del_x = rc.right - pad - self.s(18);
        // Delete hit is pushed first so the × is not swallowed by the row.
        let del_hit = RECT { left: del_x - self.s(6), top: y - self.s(2), right: rc.right - pad + self.s(4), bottom: y + self.s(20) };
        self.hits.push((del_hit, delete));
        let toggle_hit = RECT { left: pad, top: y - self.s(2), right: del_x - self.s(8), bottom: y + self.s(20) };
        self.hits.push((toggle_hit, toggle));
        let color = if checked { gdi::t().text } else { gdi::t().dim };
        gdi::text_fit(dc, pad + self.s(26), y + self.s(1), del_x - self.s(8), &[self.fonts.micro], color, label);
        self.kill_glyph(dc, del_x, y);
        y + self.s(24)
    }

    /// The main-panel metric list: a drag handle and a checkbox per metric.
    /// Returns the y below it.
    fn draw_metric_order(&mut self, dc: HDC, rc: &RECT, y: i32) -> i32 {
        let pad = self.s(12);
        let row_h = self.s(24);
        let list = self.cfg.main_metrics.clone();
        self.metric_list = (y, row_h);
        // Where the dragged row would land if released now.
        let drop_at = self.metric_drag.and_then(|_| self.hover_pos).map(|(_, my)| {
            ((my - y) / row_h).clamp(0, list.len() as i32 - 1) as usize
        });

        let mut ry = y;
        for (i, (name, visible)) in list.iter().enumerate() {
            let dragging = self.metric_drag == Some(i);
            let row_bg = RECT { left: pad, top: ry - self.s(2), right: rc.right - pad, bottom: ry + self.s(20) };
            if dragging {
                gdi::fill(dc, &row_bg, gdi::t().card);
            } else if self.hovered(&row_bg) {
                gdi::fill(dc, &row_bg, gdi::t().card_hover);
            }
            // Insertion line at the edge of the row the drop would target.
            if let Some(d) = drop_at {
                if d == i && !dragging {
                    let line_y = if d > self.metric_drag.unwrap_or(0) { ry + self.s(20) } else { ry - self.s(2) };
                    let line = RECT { left: pad, top: line_y, right: rc.right - pad, bottom: line_y + self.s(2) };
                    gdi::fill(dc, &line, gdi::acc().cpu);
                }
            }

            // Drag handle. Two bars rather than three: at this size the third
            // reads as noise, and two still say "grab me".
            let grip_x = pad + self.s(4);
            gdi::icon(
                dc,
                grip_x + self.s(GLYPH) / 2,
                ry + self.s(SP3) + self.s(SP1),
                self.s(GLYPH),
                self.s(1).max(1),
                if dragging { gdi::acc().cpu } else { gdi::t().dim },
                gdi::t().bg,
                gdi::Icon::Grip,
            );
            let grip_hit = RECT { left: pad, top: ry - self.s(2), right: grip_x + self.s(16), bottom: ry + self.s(20) };
            self.hits.push((grip_hit, Action::MetricDragStart(i)));

            let box_r = RECT {
                left: grip_x + self.s(22),
                top: ry + self.s(2),
                right: grip_x + self.s(36),
                bottom: ry + self.s(16),
            };
            self.check_box(dc, &box_r, *visible);
            // Mixed case here, not the row's uppercase: this is a settings list
            // of names, not the metric's identity band.
            let (label, accent, _) = metric_label(name);
            gdi::text(dc, box_r.right + self.s(SP3), ry, self.fonts.body, accent, label);
            let toggle_hit =
                RECT { left: box_r.left - self.s(4), top: ry - self.s(2), right: rc.right - pad, bottom: ry + self.s(20) };
            self.hits.push((toggle_hit, Action::MetricVisible(i)));
            ry += row_h;
        }
        ry += self.s(6);
        // Kept despite the copy rules: a drag handle is the only affordance
        // for this gesture, so the hint teaches rather than restates.
        gdi::text(dc, pad, ry, self.fonts.micro, gdi::t().dim, "Drag ≡ to reorder");
        ry + self.s(20)
    }

    fn check_row(&mut self, dc: HDC, rc: &RECT, y: i32, label: &str, checked: bool, action: Action) -> i32 {
        let pad = self.s(12);
        let row_bg = RECT { left: pad, top: y - self.s(2), right: rc.right - pad, bottom: y + self.s(20) };
        if self.hovered(&row_bg) {
            gdi::fill(dc, &row_bg, gdi::t().card_hover);
        }
        let box_r = RECT {
            left: pad + self.s(4),
            top: y + self.s(2),
            right: pad + self.s(18),
            bottom: y + self.s(16),
        };
        self.check_box(dc, &box_r, checked);
        gdi::text(dc, pad + self.s(26), y, self.fonts.body, gdi::t().text, label);
        let hit = RECT { left: pad, top: y - self.s(2), right: rc.right - pad, bottom: y + self.s(20) };
        self.hits.push((hit, action));
        y + self.s(24)
    }
}

/// Y positions in the watch view that the painter and `update_edit` share.
struct ProcLayout {
    y_subs_header: i32,
    y_filter: i32,
    y_chips: i32,
    y_list: i32,
}

/// Display label and accent for a `main_metrics` name — the single map both
/// `draw_main` and the settings list draw from.
fn metric_label(name: &str) -> (&'static str, u32, gdi::Glyph) {
    match name {
        "cpu" => ("CPU", gdi::acc().cpu, gdi::Glyph::Cpu),
        "ram" => ("RAM", gdi::acc().ram, gdi::Glyph::Ram),
        "gpu" => ("GPU", gdi::acc().gpu, gdi::Glyph::Gpu),
        "fps" => ("FPS", gdi::acc().fps, gdi::Glyph::Fps),
        "disk" => ("Disk", gdi::acc().disk, gdi::Glyph::Disk),
        "net" => ("Network", gdi::acc().net, gdi::Glyph::Net),
        "audio" => ("Sound", gdi::acc().audio, gdi::Glyph::Audio),
        _ => ("?", gdi::acc().cpu, gdi::Glyph::Cpu),
    }
}

/// How a chart should format its own peak and ceiling labels, from the scale
/// policy that produced the ceiling. Keeping the two derived from one value is
/// what stops a rate's peak label printing a raw byte count.
fn chart_units(scale: Scale) -> gdi::Units {
    match scale {
        Scale::Percent => gdi::Units::Percent,
        Scale::Rate => gdi::Units::Rate,
        Scale::Fps | Scale::Fixed(_) => gdi::Units::Count,
    }
}

/// Nominal size of a metric glyph in a row. Design px, before `scale`.
const GLYPH: i32 = 13;

/// Nominal size of a chrome glyph inside a `CTRL_H` button. Larger than
/// `GLYPH`: a row glyph sits beside 11 px caps and must not shout over them,
/// but a button glyph is alone in a 24 px square and was reported as too small
/// at 13.
const CHROME_GLYPH: i32 = 17;

/// The drill-down hero plate, and the plot inside it. Design px, before scale.
/// Timer id for the hover cross-fade, and its frame interval. 16 ms is one
/// display frame at 60 Hz; 90 ms is six of them.
const ID_FADE: usize = 1;
const ID_BALLOON: usize = 2;
/// Windows shows a tray balloon for around five seconds, so anything much
/// shorter than this would replace a notification the user is still reading.
const BALLOON_MS: u32 = 6000;
/// A bound on the backlog. At six seconds each, an unbounded queue could
/// spend minutes working through a burst, and by then the oldest entries are
/// describing something that has long since stopped being true.
const BALLOON_QUEUE_MAX: usize = 20;
const FADE_MS: u32 = 16;
const FADE_TOTAL_MS: f32 = 90.0;

/// The plot inside the drill-down hero plate. Design px, before scale. The
/// plate's own height is *derived* from this plus the measured header text — a
/// fixed plate height and a measured header disagree the moment the type scale
/// or the DPI changes, and the plot then overflows the card.
const HERO_PLOT_H: i32 = 72;
/// Fallback plate height for the first paint, before anything is measured.
const HERO_H_GUESS: i32 = 172;

/// Nominal size of a direction marker beside a value, and beside a secondary
/// figure. Design px, before `scale`.
const MARKER: i32 = 10;
const MARKER_SM: i32 = 9;

/// The unit that sits beside a figure.
#[derive(Copy, Clone, PartialEq)]
enum Unit {
    None,
    /// A word in `micro`/`mute`: `used`, `read`, `fps`.
    Word(&'static str),
    /// A direction marker glyph. Only the network rates take these. `read` and
    /// `write` stay words on purpose — they are not directions, and an arrow
    /// beside a disk read rate would imply the disk is downloading.
    Down,
    Up,
}

/// The right-hand side of a metric row: one value on the card's centre line,
/// its unit beside it, and at most one secondary figure hung off the bottom.
struct Figures {
    value: String,
    unit: Unit,
    sub: Option<String>,
    sub_unit: Unit,
    /// True when `sub` is a figure rather than prose. A figure is set in `text`,
    /// the same ink as the primary value, and is distinguished from it by size
    /// alone; prose — a process name, `of`, `in` — stays `mute`.
    ///
    /// Reported twice. The first report said the secondary numbers were too
    /// faint to read while the words beside them were fine grey, and `dim` was
    /// the answer. The second said `dim` was still too faint, and it is right:
    /// the row puts a `text` value directly above a `dim` one, so the eye has
    /// the brighter ink to compare against and reads the dimmer as disabled.
    /// Size already says which figure is secondary.
    sub_is_figure: bool,
}

impl Figures {
    fn new(value: String) -> Self {
        Figures {
            value,
            unit: Unit::None,
            sub: None,
            sub_unit: Unit::None,
            sub_is_figure: false,
        }
    }

    fn unit(mut self, u: Unit) -> Self {
        self.unit = u;
        self
    }

    /// A secondary figure: set in `text`, because it is a number the user reads.
    fn sub(mut self, s: String, u: Unit) -> Self {
        self.sub = Some(s);
        self.sub_unit = u;
        self.sub_is_figure = true;
        self
    }

    /// A secondary line that is prose, not a figure — a process name, or a
    /// sentence. Stays `mute`.
    fn note(mut self, s: String) -> Self {
        self.sub = Some(s);
        self.sub_unit = Unit::None;
        self.sub_is_figure = false;
        self
    }
}

/// Identity of one agent row, for remembering which rows the user opened.
/// Scope plus id plus start time: ids are only unique within a session, and a
/// session can run the same id twice.
fn agent_key(scope: &str, id: &str, at_ms: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    scope.hash(&mut h);
    id.hash(&mut h);
    at_ms.hash(&mut h);
    h.finish()
}

/// "14:22" in local time, for a finished agent's row.
fn clock_of(unix_ms: u64) -> String {
    let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut local: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    // Unix ms -> FILETIME (100ns ticks since 1601) -> local wall clock.
    let ticks = unix_ms as u64 * 10_000 + 116_444_736_000_000_000;
    let ft = windows_sys::Win32::Foundation::FILETIME {
        dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    unsafe {
        windows_sys::Win32::System::Time::FileTimeToSystemTime(&ft, &mut st);
        windows_sys::Win32::System::Time::SystemTimeToTzSpecificLocalTime(
            std::ptr::null(),
            &st,
            &mut local,
        );
    }
    format!("{:02}:{:02}", local.wHour, local.wMinute)
}

fn short_age(ms: u64) -> String {
    let mins = ms / 60_000;
    if mins < 60 {
        format!("{}m ago", mins.max(1))
    } else {
        format!("{}h ago", mins / 60)
    }
}

/// Value column text for one subprocess under the selected metric.
fn sub_value_text(p: &ProcStat, metric: Metric) -> String {
    match metric {
        Metric::Cpu => format!("{:.1}%", p.cpu_pct),
        Metric::Ram => format_bytes(p.ws_private),
        Metric::Gpu => format!("{:.1}%", p.gpu_pct),
        Metric::Disk => format_rate(p.io_bps),
        Metric::Net => format_rate(p.net_bps),
        Metric::Audio => {
            if p.audio > 0.0 {
                format!("{:.0}%", p.audio * 100.0)
            } else {
                "—".to_string()
            }
        }
    }
}

fn accent_for(metric: Metric) -> u32 {
    match metric {
        Metric::Cpu => gdi::acc().cpu,
        Metric::Ram => gdi::acc().ram,
        Metric::Gpu => gdi::acc().gpu,
        Metric::Disk => gdi::acc().disk,
        Metric::Net => gdi::acc().net,
        Metric::Audio => gdi::acc().audio,
    }
}

struct RuleEditLayout {
    proc: bool,
    /// Whether this is a connection rule: field chips and a pattern, in place
    /// of the direction and threshold rows.
    conn: bool,
    /// Whether the log-file row is shown (delivery is "file" or "both").
    file: bool,
    y_metric: i32,
    y_grid: i32,
    y_when: i32,
    y_name: i32,
    y_sub: i32,
    y_thresh: i32,
    y_deliver: i32,
    y_file: i32,
    y_opts: i32,
    y_buttons: i32,
}

/// Aggregate processes by image name, filter by needle, top `n` as (name, pids).
fn top_by(procs: &[ProcStat], metric: Metric, n: usize, filter: &str) -> Vec<(String, Vec<u32>)> {
    let mut agg: HashMap<&str, (f64, Vec<u32>)> = HashMap::new();
    for p in procs {
        if !filter.is_empty() && !p.name.to_lowercase().contains(filter) {
            continue;
        }
        let v = metric_value(p, metric);
        let e = agg.entry(p.name.as_str()).or_insert((0.0, Vec::new()));
        e.0 += v;
        e.1.push(p.pid);
    }
    let mut v: Vec<(String, f64, Vec<u32>)> = agg
        .into_iter()
        // With a filter active, show matches even when the value is 0 so the
        // user can find and open any app; otherwise hide idle rows.
        .filter(|(_, (v, _))| *v > 0.0 || !filter.is_empty())
        .map(|(k, (v, pids))| (k.to_string(), v, pids))
        .collect();
    // Value descending, then name ascending: HashMap iteration order varies
    // between paints, so ties must break deterministically or equal-valued
    // rows swap places every refresh.
    v.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    v.truncate(n);
    v.into_iter().map(|(name, _, pids)| (name, pids)).collect()
}

fn metric_value(p: &ProcStat, metric: Metric) -> f64 {
    match metric {
        Metric::Cpu => p.cpu_pct as f64,
        // Private working set: matches Task Manager's memory column.
        Metric::Ram => p.ws_private as f64,
        Metric::Gpu => p.gpu_pct as f64,
        Metric::Disk => p.io_bps as f64,
        Metric::Net => p.net_bps as f64,
        Metric::Audio => p.audio as f64,
    }
}

fn row_value(procs: &[ProcStat], name: &str, metric: Metric) -> f64 {
    procs
        .iter()
        .filter(|p| p.name == name)
        .map(|p| metric_value(p, metric))
        .sum()
}

/// The two directions of a metric that has two, summed over every process with
/// this image name: `(read, write)` for disk, `(down, up)` for network. `None`
/// for the metrics that are a single number.
///
/// The list is still ranked by the total from `row_value`, which is the right
/// ordering — an app writing hard is busy whether or not it reads — so the pair
/// shown is not always what the row was sorted on. For disk the two also exclude
/// `other` transfers, so they need not add up to that total and are never
/// presented as if they do.
fn row_pair(procs: &[ProcStat], name: &str, metric: Metric) -> Option<(f64, f64)> {
    let mine = || procs.iter().filter(|p| p.name == name);
    match metric {
        Metric::Disk => Some(mine().fold((0.0, 0.0), |(r, w), p| {
            (r + p.io_read_bps as f64, w + p.io_write_bps as f64)
        })),
        Metric::Net => Some(mine().fold((0.0, 0.0), |(d, u), p| {
            (d + p.net_rx_bps as f64, u + p.net_tx_bps as f64)
        })),
        _ => None,
    }
}

/// Per-app totals summed over all processes with this image name.
#[derive(Default, Clone, Copy)]
struct AppSums {
    cpu: f32,
    ram_private: u64,
    ram_total: u64,
    gpu: f32,
    disk_bps: u64,
    disk_read_bps: u64,
    disk_write_bps: u64,
    net_bps: u64,
    net_rx_bps: u64,
    net_tx_bps: u64,
    /// Loudest stream among this app's processes, 0..1. Summed would be wrong —
    /// six Chrome renderers at 0.4 are not 240 % of anything — so the app's level
    /// is the loudest thing it is playing.
    audio: f32,
}

/// Highest FPS among this app's processes (0 when it isn't presenting).
fn watch_fps(snap: &Snapshot, name: &str) -> u32 {
    snap.fps_list
        .iter()
        .filter(|(_, n, _)| n == name)
        .map(|(_, _, f)| *f)
        .max()
        .unwrap_or(0)
}

fn watch_sums(procs: &[ProcStat], name: &str) -> AppSums {
    let mut out = AppSums::default();
    for p in procs.iter().filter(|p| p.name == name) {
        out.cpu += p.cpu_pct;
        out.ram_private += p.ws_private;
        out.ram_total += p.ws_bytes;
        out.gpu += p.gpu_pct;
        out.disk_bps += p.io_bps;
        out.disk_read_bps += p.io_read_bps;
        out.disk_write_bps += p.io_write_bps;
        out.net_bps += p.net_bps;
        out.net_rx_bps += p.net_rx_bps;
        out.net_tx_bps += p.net_tx_bps;
        out.audio = out.audio.max(p.audio);
    }
    out
}
