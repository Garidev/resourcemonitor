//! ResMon — lightweight Windows tray resource monitor.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod agents;
mod config;
mod conns;
mod util;

#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod autostart;
#[cfg(windows)]
mod etw;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod mcp_pipe;
#[cfg(windows)]
mod procinfo;
mod rules;
#[cfg(windows)]
mod sampler;
#[cfg(windows)]
mod ui;

#[cfg(not(windows))]
fn main() {
    eprintln!("resmon targets Windows; build with --target x86_64-pc-windows-gnu");
}

#[cfg(windows)]
pub fn log(msg: &str) {
    use std::io::Write;
    if let Ok(base) = std::env::var("LOCALAPPDATA") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{}\\resmon.log", base))
        {
            let _ = writeln!(f, "{}", msg);
        }
    }
}

#[cfg(windows)]
fn main() {
    use std::sync::Arc;

    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    use ui::gdi::wide;

    std::panic::set_hook(Box::new(|info| {
        log(&format!("panic: {}", info));
    }));

    // --install / --uninstall CLI (also reachable from the tray menu).
    if let Some(arg) = std::env::args().nth(1) {
        let (ok, what) = match arg.as_str() {
            "--install" => (autostart::install(), "set up starting with Windows"),
            "--uninstall" => (autostart::uninstall(), "stop starting with Windows"),
            _ => (false, "understand that option (use --install or --uninstall)"),
        };
        let text = if ok {
            format!("Resource Monitor: {} succeeded.", what)
        } else {
            format!("Resource Monitor: could not {}.", what)
        };
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                wide(&text).as_ptr(),
                wide("resourcemonitor.app").as_ptr(),
                MB_OK,
            );
        }
        return;
    }

    unsafe {
        // Single instance.
        CreateMutexW(std::ptr::null(), 0, wide("Local\\ResMonSingleInstance").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }

        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let class = wide("ResMonWnd");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(ui::panel::wndproc);
        wc.hInstance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
        wc.lpszClassName = class.as_ptr();
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        wc.hIcon = LoadIconW(wc.hInstance, 1 as *const u16); // embedded app icon
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class.as_ptr(),
            wide("resourcemonitor.app").as_ptr(),
            WS_POPUP | WS_BORDER | WS_CLIPCHILDREN,
            0,
            0,
            10,
            10,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            wc.hInstance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            log("CreateWindowExW failed");
            return;
        }
        // Taskbar/alt-tab icon: WNDCLASS hIcon alone is not always honoured
        // for layered/tool windows, so set both icon sizes explicitly.
        let icon = LoadImageW(wc.hInstance, 1 as *const u16, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE | LR_SHARED);
        if !icon.is_null() {
            SendMessageW(hwnd, WM_SETICON, 0, icon as isize); // ICON_SMALL
            SendMessageW(hwnd, WM_SETICON, 1, icon as isize); // ICON_BIG
        }

        let cfg = config::load();
        let rule_list = rules::parse_all(&cfg.rule_lines);
        let shared = Arc::new(sampler::Shared::new(
            cfg.interval_ms,
            cfg.mcp_enabled,
            rule_list,
        ));
        *shared.ai_instructions.lock().unwrap() = cfg.ai_instructions();
        *shared.agent_log_file.lock().unwrap() = cfg.agent_log_file.clone();
        ui::panel::init(hwnd, shared.clone(), cfg);

        let etw_shared = shared.clone();
        std::thread::spawn(move || etw::run(etw_shared));

        // Reverse-DNS worker: idle until a connection sweep queues an address
        // it has no name for. Started here because PTR lookups can block for
        // seconds and must never sit on the sampler or pipe thread.
        conns::start_reverse_resolver(shared.names.clone());

        let hwnd_num = hwnd as usize;
        let sampler_shared = shared.clone();
        std::thread::spawn(move || sampler::Sampler::new(hwnd_num, sampler_shared).run());

        let pipe_shared = shared.clone();
        std::thread::spawn(move || mcp_pipe::run(pipe_shared, hwnd_num));

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
