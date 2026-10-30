//! Small shared helpers: a common clock, wide strings, logging.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::SystemInformation::GetLocalTime;

/// 100-nanosecond ticks on the QPC clock. WASAPI reports packet times in the
/// same unit and base, so audio and video share one timeline.
pub fn now_100ns() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        unsafe { QueryPerformanceFrequency(&mut f).ok() };
        f.max(1)
    });
    let mut c = 0i64;
    unsafe { QueryPerformanceCounter(&mut c).ok() };
    ((c as i128) * 10_000_000 / freq as i128) as i64
}

pub const TICKS_PER_SEC: i64 = 10_000_000;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// "261019-172603" (two-digit year) in local time; clips are "<name>_<stamp>.mp4".
pub fn stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:02}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear % 100, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

pub fn log(msg: impl AsRef<str>) {
    let path = exe_dir().join("clipper.log");
    if std::fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(false) {
        let _ = std::fs::rename(&path, exe_dir().join("clipper.old.log"));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let t = unsafe { GetLocalTime() };
        let _ = writeln!(
            f,
            "{:02}:{:02}:{:02}.{:03} {}",
            t.wHour, t.wMinute, t.wSecond, t.wMilliseconds,
            msg.as_ref()
        );
    }
}

/// Milliseconds on the same QPC clock (for short UI timers).
pub fn now_ms() -> u64 {
    (now_100ns() / 10_000) as u64
}
