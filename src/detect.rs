//! Finding what to capture: the fullscreen game in front, or a monitor.

use crate::config::Config;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_core::{BOOL, PCWSTR, PWSTR};

#[derive(Clone, Debug)]
pub struct Target {
    pub hwnd: HWND,
    pub pid: u32,
    pub exe: String,
    pub title: String,
}

fn text(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn class(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn exe_of(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        if ok.is_err() {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase()
    }
}

fn target(hwnd: HWND) -> Target {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    Target { hwnd, pid, exe: exe_of(pid), title: text(hwnd) }
}

fn cloaked(hwnd: HWND) -> bool {
    let mut v = 0u32;
    unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut v as *mut _ as *mut _, 4).is_ok() && v != 0
    }
}

/// The foreground window if it covers its whole monitor (fullscreen or
/// borderless) and isn't on the exclude list.
pub fn foreground_game(cfg: &Config) -> Option<Target> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() || cloaked(hwnd) {
            return None;
        }
        if matches!(class(hwnd).as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "ClipperHiddenWindow") {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(hwnd, &mut r).ok()?;
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return None;
        }
        let m = mi.rcMonitor;
        if !(r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom) {
            return None;
        }
        let t = target(hwnd);
        if t.exe.is_empty() || t.pid == std::process::id() || cfg.exclude.iter().any(|e| *e == t.exe) {
            return None;
        }
        Some(t)
    }
}

/// Test hook: first visible top-level window whose title contains `needle`.
pub fn window_by_title(needle: &str) -> Option<Target> {
    struct Search<'a> {
        needle: &'a str,
        found: Option<HWND>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        let s = unsafe { &mut *(lp.0 as *mut Search) };
        if unsafe { IsWindowVisible(hwnd) }.as_bool() && text(hwnd).contains(s.needle) {
            s.found = Some(hwnd);
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut s = Search { needle, found: None };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut s as *mut _ as isize));
    }
    s.found.map(target)
}

pub struct Monitor {
    pub handle: HMONITOR,
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
    /// refresh rate in Hz (0 if unknown)
    pub refresh: u32,
}

fn describe(h: HMONITOR) -> Option<Monitor> {
    unsafe {
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(h, &mut mi.monitorInfo).as_bool() {
            return None;
        }
        let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        let refresh = if EnumDisplaySettingsW(PCWSTR(mi.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm).as_bool() {
            dm.dmDisplayFrequency
        } else {
            0
        };
        let r = mi.monitorInfo.rcMonitor;
        Some(Monitor {
            handle: h,
            left: r.left,
            top: r.top,
            width: r.right - r.left,
            height: r.bottom - r.top,
            primary: mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            refresh,
        })
    }
}

/// The monitor a window is on.
pub fn monitor_of(hwnd: HWND) -> Option<Monitor> {
    describe(unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) })
}

/// Monitor modes, in the order `monitors()` returns the displays:
/// (mode key, short name). More displays than this aren't offered.
pub const MONITOR_NAMES: [(&str, &str); 6] =
    [("main", "Main"), ("second", "Second"), ("third", "Third"), ("fourth", "Fourth"), ("fifth", "Fifth"), ("sixth", "Sixth")];

/// All displays: the primary one first ("Main"), the rest left to right.
pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let v = unsafe { &mut *(lp.0 as *mut Vec<Monitor>) };
        if let Some(m) = describe(h) {
            v.push(m);
        }
        BOOL(1)
    }
    let mut v: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v.sort_by_key(|m| (!m.primary, m.left, m.top));
    v
}
