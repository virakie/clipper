//! Self-update from GitHub releases (github.com/virakie/clipper).
//!
//! Check: ask the GitHub API for the latest release; newer than this exe ->
//! the menus offer "Update to vX.Y.Z". Install: download the small zip (just
//! the exe, no ffmpeg), check its SHA-256 against the one GitHub lists, rename
//! the running clipper.exe to clipper.exe.old (Windows allows renaming a
//! running exe, not overwriting it), put the new exe in its place, restart.
//! The .old file is deleted by the next start.
//!
//! Uses Windows' own curl.exe / tar.exe / certutil.exe, so no HTTP code here.
//! Test hooks: env CLIPPER_FAKE_VERSION=0.1.0 pretends to be older,
//! CLIPPER_UPDATE_API=<url or file:///...> replaces the API address.

use crate::util::{exe_dir, log, temp_dir};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

const API: &str = "https://api.github.com/repos/virakie/clipper/releases/latest";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// posted to the hidden window: wparam = one of the EV_ values
pub const WM_UPDATE: u32 = WM_APP + 4;
pub const EV_CHECKED: usize = 0;
pub const EV_READY: usize = 1;
pub const EV_FAILED: usize = 2;

#[derive(Clone, Default)]
pub struct Release {
    /// "0.6.5" (tag without the v)
    pub version: String,
    pub url: String,
    /// the same file through api.github.com (works when github.com downloads 504)
    pub api_url: String,
    /// lowercase hex SHA-256 from GitHub, empty if it didn't list one
    pub sha256: String,
}

#[derive(Default)]
pub struct State {
    /// newer release found by the last check
    pub available: Option<Release>,
    /// "Checking..." / "Downloading v0.6.5..." while a thread runs
    pub busy: String,
    /// last check result or error, for the menu / balloon
    pub message: String,
    /// the user asked (menu), so say "up to date" too
    pub manual: bool,
}

pub static STATE: Mutex<State> = Mutex::new(State { available: None, busy: String::new(), message: String::new(), manual: false });
/// set when the new exe is in place: main() starts it after the old one exits
pub static RESTART: Mutex<Option<Vec<String>>> = Mutex::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);

pub fn current() -> String {
    std::env::var("CLIPPER_FAKE_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

fn parse_ver(s: &str) -> Vec<u32> {
    s.trim_start_matches(['v', 'V']).split(['.', '-']).map(|p| p.parse().unwrap_or(0)).collect()
}

pub fn is_newer(latest: &str, cur: &str) -> bool {
    parse_ver(latest) > parse_ver(cur)
}

fn hidden(cmd: &str) -> Command {
    let mut c = Command::new(cmd);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

fn post(hwnd: isize, ev: usize) {
    unsafe {
        let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_UPDATE, WPARAM(ev), LPARAM(0));
    }
}

/// Value of the first `"key":"..."` at or after `from`.
fn json_str(text: &str, key: &str, from: usize) -> Option<(String, usize)> {
    let pat = format!("\"{key}\":\"");
    let start = text[from..].find(&pat)? + from + pat.len();
    let end = text[start..].find('"')? + start;
    Some((text[start..end].to_string(), end))
}

/// The release's slim zip ("...-win64.zip", not "-portable") and its digest.
fn parse_release(json: &str) -> Option<Release> {
    let (tag, _) = json_str(json, "tag_name", 0)?;
    let mut pos = 0;
    let mut asset_start = 0;
    while let Some((url, end)) = json_str(json, "browser_download_url", pos) {
        if url.ends_with("-win64.zip") {
            // the digest sits in the same asset object, before its URL
            let obj = &json[asset_start..end];
            let sha = obj
                .rfind("\"digest\":\"sha256:")
                .and_then(|i| json_str(obj, "digest", i))
                .map(|(d, _)| d.trim_start_matches("sha256:").to_lowercase())
                .unwrap_or_default();
            let mut api_url = String::new();
            let mut p = 0;
            while let Some((u, e)) = json_str(obj, "url", p) {
                if u.contains("/releases/assets/") {
                    api_url = u;
                }
                p = e;
            }
            return Some(Release { version: tag.trim_start_matches('v').to_string(), url, api_url, sha256: sha });
        }
        asset_start = end;
        pos = end;
    }
    None
}

fn set_busy(text: &str) {
    STATE.lock().unwrap().busy = text.to_string();
}

/// Ask GitHub in the background; posts EV_CHECKED (or EV_FAILED).
pub fn check(hwnd: isize, manual: bool) {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    {
        let mut s = STATE.lock().unwrap();
        s.manual = manual;
        s.busy = "Checking for updates...".into();
    }
    std::thread::spawn(move || {
        let api = std::env::var("CLIPPER_UPDATE_API").unwrap_or_else(|_| API.to_string());
        let out = hidden("curl.exe")
            .args(["-sSfL", "--max-time", "20", "-H", "User-Agent: Clipper-updater", "-H", "Accept: application/vnd.github+json", &api])
            .output();
        let result = match out {
            Ok(o) if o.status.success() => {
                parse_release(&String::from_utf8_lossy(&o.stdout)).ok_or_else(|| "no Clipper zip in the latest release".to_string())
            }
            Ok(o) => Err(format!("couldn't reach GitHub ({})", String::from_utf8_lossy(&o.stderr).trim())),
            Err(e) => Err(format!("curl.exe failed: {e}")),
        };
        let ev = {
            let mut s = STATE.lock().unwrap();
            s.busy.clear();
            match result {
                Ok(r) => {
                    let cur = current();
                    if is_newer(&r.version, &cur) {
                        log(format!("update: v{} available (this is v{cur})", r.version));
                        s.message = format!("Clipper v{} is available", r.version);
                        s.available = Some(r);
                    } else {
                        log(format!("update: up to date (v{cur}, latest v{})", r.version));
                        s.message = format!("Clipper v{cur} is the latest version");
                        s.available = None;
                    }
                    EV_CHECKED
                }
                Err(e) => {
                    log(format!("update check: {e}"));
                    s.message = format!("Update check failed: {e}");
                    EV_FAILED
                }
            }
        };
        RUNNING.store(false, Ordering::SeqCst);
        post(hwnd, ev);
    });
}

fn sha256(path: &Path) -> Option<String> {
    let o = hidden("certutil.exe").arg("-hashfile").arg(path).arg("SHA256").output().ok()?;
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| l.replace(' ', "").to_lowercase())
        .find(|l| l.len() == 64 && l.chars().all(|c| c.is_ascii_hexdigit()))
}

fn find_exe(dir: &Path) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_exe(&p) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("clipper.exe")) {
            return Some(p);
        }
    }
    None
}

fn install_blocking(r: &Release) -> Result<(), String> {
    let tmp = temp_dir();
    let zip = tmp.join("update.zip");
    let dir = tmp.join("update");
    let _ = std::fs::remove_file(&zip);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("temp folder: {e}"))?;

    // API route first, the plain download link if that fails
    let mut err = String::new();
    for url in [&r.api_url, &r.url] {
        if url.is_empty() {
            continue;
        }
        let o = hidden("curl.exe")
            .args(["-sSfL", "--retry", "2", "--max-time", "300", "-H", "User-Agent: Clipper-updater", "-H", "Accept: application/octet-stream", "-o"])
            .arg(&zip)
            .arg(url)
            .output()
            .map_err(|e| format!("curl.exe failed: {e}"))?;
        if o.status.success() {
            err.clear();
            break;
        }
        err = format!("download failed ({})", String::from_utf8_lossy(&o.stderr).trim());
        log(format!("update: {url}: {err}"));
    }
    if !err.is_empty() {
        return Err(err);
    }
    if !r.sha256.is_empty() {
        let got = sha256(&zip).ok_or("couldn't hash the download")?;
        if got != r.sha256 {
            return Err(format!("download is damaged (SHA-256 {got}, expected {})", r.sha256));
        }
    }
    let o = hidden("tar.exe").arg("-xf").arg(&zip).arg("-C").arg(&dir).output().map_err(|e| format!("tar.exe failed: {e}"))?;
    if !o.status.success() {
        return Err(format!("unzip failed ({})", String::from_utf8_lossy(&o.stderr).trim()));
    }
    let new = find_exe(&dir).ok_or("no clipper.exe in the download")?;

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let old = exe.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("couldn't move the old exe aside: {e}"))?;
    if let Err(e) = std::fs::copy(&new, &exe) {
        let _ = std::fs::rename(&old, &exe); // put the old one back
        return Err(format!("couldn't write the new exe: {e}"));
    }
    let _ = std::fs::remove_file(&zip);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Download + swap the exe in the background; posts EV_READY or EV_FAILED.
pub fn install(hwnd: isize) {
    let Some(r) = STATE.lock().unwrap().available.clone() else { return };
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    set_busy(&format!("Downloading v{}...", r.version));
    log(format!("update: installing v{} from {}", r.version, r.url));
    std::thread::spawn(move || {
        let res = install_blocking(&r);
        let ev = {
            let mut s = STATE.lock().unwrap();
            s.busy.clear();
            match res {
                Ok(()) => {
                    log(format!("update: v{} in place, restarting", r.version));
                    EV_READY
                }
                Err(e) => {
                    log(format!("update failed: {e}"));
                    s.message = format!("Update failed: {e}");
                    EV_FAILED
                }
            }
        };
        RUNNING.store(false, Ordering::SeqCst);
        post(hwnd, ev);
    });
}

/// Start the (new) exe after this one has exited. Called at the end of main().
pub fn restart_if_asked() {
    let Some(args) = RESTART.lock().unwrap().take() else { return };
    if let Ok(exe) = std::env::current_exe() {
        // current_exe still names clipper.exe (the path, not the renamed file)
        let exe = if exe.extension().is_some_and(|e| e == "old") { exe.with_extension("") } else { exe };
        log(format!("update: starting {} {}", exe.display(), args.join(" ")));
        let _ = Command::new(exe).args(args).current_dir(exe_dir()).spawn();
    }
}

/// Remove what an update left behind (clipper.exe.old). The old process may
/// still be exiting, so retry for a few seconds in the background.
pub fn cleanup() {
    let Ok(exe) = std::env::current_exe() else { return };
    let old = exe.with_extension("exe.old");
    if !old.exists() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..20 {
            if std::fs::remove_file(&old).is_ok() || !old.exists() {
                log("update: removed the old exe");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("v0.6.5", "0.6.4"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("v0.6.4", "0.6.4"));
        assert!(!is_newer("0.6.3", "0.6.4"));
    }

    #[test]
    fn picks_slim_zip() {
        let j = r#"{"tag_name":"v0.6.3","assets":[{"name":"Clipper-0.6.3-win64-portable.zip","digest":"sha256:AA","browser_download_url":"https://x/Clipper-0.6.3-win64-portable.zip"},{"url":"https://api.github.com/repos/v/c/releases/assets/7","name":"Clipper-0.6.3-win64.zip","digest":"sha256:D1","browser_download_url":"https://x/Clipper-0.6.3-win64.zip"}]}"#;
        let r = parse_release(j).unwrap();
        assert_eq!(r.version, "0.6.3");
        assert_eq!(r.url, "https://x/Clipper-0.6.3-win64.zip");
        assert_eq!(r.sha256, "d1");
        assert_eq!(r.api_url, "https://api.github.com/repos/v/c/releases/assets/7");
    }
}
