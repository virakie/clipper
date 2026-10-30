//! Clipper: a background replay buffer. Tray icon + a small window;
//! Shift+F3 saves the last N seconds. Portable: everything lives next to the exe.
//!
//!   clipper.exe [--mode main|second|game|off] [--window TITLE]   start / switch capture
//!   clipper.exe --show | --toggle                                 open / toggle the window
//!   clipper.exe --audio game|device|off  --device "NAME"          sound source
//!   clipper.exe --mic on|off  --mic-device "NAME"                 microphone
//!   clipper.exe --save | --quit
//!
//! A second launch forwards its command to the running instance.

#![windows_subsystem = "windows"]

mod audio;
mod buffer;
mod config;
mod detect;
mod hotkey;
mod save;
mod trim;
mod ui;
mod util;
mod video;

use audio::{AudioCapture, AudioSource, DeviceInfo};
use buffer::{AudioRing, VideoRing};
use config::Config;
use detect::Target;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use ui::Ui;
use util::{log, wide};
use video::{Source, VideoPipeline};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_core::{PCWSTR, PWSTR, w};

/// The single-instance key. Test runs (env CLIPPER_TEST) get their own, so a
/// test copy can never forward commands to (or quit) the live Clipper.
fn class() -> PCWSTR {
    static NAME: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();
    let n = NAME.get_or_init(|| {
        let suffix = if std::env::var_os("CLIPPER_TEST").is_some() { "Test" } else { "" };
        wide(&format!("ClipperHiddenWindow{suffix}"))
    });
    PCWSTR(n.as_ptr())
}
const WM_TRAY: u32 = WM_APP + 1;
/// the page posted commands into ui::QUEUE
pub const WM_UI_CMD: u32 = WM_APP + 2;
/// a save finished
const WM_SAVED: u32 = WM_APP + 3;
const TIMER_TICK: usize = 1;
const TIMER_UI: usize = 2;
const HOTKEY_SAVE: i32 = 1;
const HOTKEY_WINDOW: i32 = 2;
const ID_SHOW: u32 = 5;
/// tray menu monitor modes: ID_MONITOR + index
const ID_MONITOR: u32 = 40;
const ID_GAME: u32 = 12;
const ID_OFF: u32 = 13;
const ID_SAVE: u32 = 20;
const ID_FOLDER: u32 = 21;
const ID_QUIT: u32 = 30;
const RED: (u8, u8, u8) = (0xC9, 0x36, 0x1B);
const GREY: (u8, u8, u8) = (0x6e, 0x6e, 0x74);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    /// 0 = main (primary) display, 1 = second, ... (detect::monitors order)
    Monitor(usize),
    Game,
    Off,
}

impl Mode {
    fn parse(s: &str) -> Option<Mode> {
        let s = s.to_lowercase();
        match s.as_str() {
            "ultrawide" => Some(Mode::Monitor(0)),
            "game" => Some(Mode::Game),
            "off" => Some(Mode::Off),
            _ => detect::MONITOR_NAMES.iter().position(|(k, _)| *k == s).map(Mode::Monitor),
        }
    }
    fn key(self) -> &'static str {
        match self {
            Mode::Monitor(i) => detect::MONITOR_NAMES[i].0,
            Mode::Game => "game",
            Mode::Off => "off",
        }
    }
    fn label(self) -> String {
        match self {
            Mode::Monitor(i) => format!("{} monitor", detect::MONITOR_NAMES[i].1),
            Mode::Game => "Game".into(),
            Mode::Off => "Off".into(),
        }
    }
}

struct App {
    cfg: Arc<Config>,
    hwnd: HWND,
    mode: Mode,
    video: Arc<Mutex<VideoRing>>,
    audio: Arc<Mutex<AudioRing>>,
    mic: Arc<Mutex<AudioRing>>,
    pipe: Option<VideoPipeline>,
    acap: Option<AudioCapture>,
    mcap: Option<AudioCapture>,
    /// game mode: the window being captured
    target: Option<Target>,
    source_name: String,
    /// file-safe name of the game being (or last) captured; kept after the
    /// game closes so its buffer still saves under its name
    last_game: Option<String>,
    /// test hook: capture this window instead of detecting a game
    force_title: Option<String>,
    saving: Arc<AtomicBool>,
    ticks: u32,
    retry_at: u32,
    icon_on: HICON,
    icon_off: HICON,
    // settings the window changes (written back to clipper.ini)
    audio_mode: String,
    /// chosen frame rate / output lines, and what the current capture uses
    fps: u32,
    cur_fps: u32,
    res_height: u32,
    /// source width, height, refresh (0 = nothing captured)
    src: (u32, u32, u32),
    /// sound mode to return to when sound is switched back on
    last_audio_on: String,
    audio_device: String,
    audio_volume: u32,
    mic_on: bool,
    mic_device: String,
    mic_volume: u32,
    seconds: u32,
    out_dir: PathBuf,
    pinned: bool,
    window_pos: Option<(i32, i32)>,
    ui: Option<Ui>,
    devices: Vec<DeviceInfo>,
    mics: Vec<DeviceInfo>,
    prefs: std::collections::BTreeMap<String, String>,
    startup_mode: String,
    /// global shortcuts as text ("Shift+F3", "off"), and why one didn't register
    hotkey_save: String,
    hotkey_window: String,
    hotkey_error: String,
}

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

/// Is Clipper in the user's Run key (starts with Windows)?
fn startup_enabled() -> bool {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, Some(0), KEY_READ, &mut key).is_err() {
            return false;
        }
        let ok = RegQueryValueExW(key, w!("Clipper"), None, None, None, None).is_ok();
        let _ = RegCloseKey(key);
        ok
    }
}

/// Add or remove the Run entry. It points at this exe, wherever the portable
/// folder currently is, and starts in `mode`.
fn set_startup(on: bool, mode: &str) {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, Some(0), KEY_SET_VALUE, &mut key).is_err() {
            log("startup: cannot open the Run key");
            return;
        }
        if on {
            let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
            let cmd = format!("\"{exe}\" --mode {mode}");
            let w: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = std::slice::from_raw_parts(w.as_ptr() as *const u8, w.len() * 2);
            let r = RegSetValueExW(key, w!("Clipper"), Some(0), REG_SZ, Some(bytes));
            log(format!("startup on ({cmd}): {r:?}"));
        } else {
            let _ = RegDeleteValueW(key, w!("Clipper"));
            log("startup off");
        }
        let _ = RegCloseKey(key);
    }
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Win32 calls like SetWindowTextW, TrackPopupMenu or a dialog re-enter the
/// window procedure while the app is borrowed; those nested calls are skipped
/// instead of panicking.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| match a.try_borrow_mut() {
        Ok(mut g) => g.as_mut().map(f),
        Err(_) => None,
    })
}

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

/// Device names look like "SteelSeries Sonar - Gaming (SteelSeries Sonar
/// Virtual Audio Device)"; accept the full name or just the part before " (".
fn device_matches(full: &str, wanted: &str) -> bool {
    let short = full.split(" (").next().unwrap_or(full);
    full.eq_ignore_ascii_case(wanted) || short.eq_ignore_ascii_case(wanted)
}

fn find_device(list: &[DeviceInfo], wanted: &str) -> Option<String> {
    if wanted.eq_ignore_ascii_case("default") {
        return None;
    }
    let found = list.iter().find(|d| device_matches(&d.name, wanted));
    if found.is_none() {
        log(format!("device '{wanted}' not found, using the Windows default"));
    }
    found.map(|d| d.id.clone())
}

/// "Phantom Blade Zero  " -> "PhantomBladeZero": the window title made safe
/// for a file name, no spaces (falls back to the exe name, then "Clip").
fn file_name_for(t: &Target) -> String {
    let clean = |s: &str| {
        let s: String = s
            .chars()
            .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { ' ' } else { c })
            .collect();
        // no spaces in file names: "Phantom Blade Zero" -> "PhantomBladeZero"
        let s: String = s
            .split_whitespace()
            .map(|w| {
                let mut c = w.chars();
                c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
            })
            .collect();
        s.trim_matches('.').chars().take(60).collect::<String>()
    };
    let title = clean(&t.title);
    if !title.is_empty() {
        return title;
    }
    let exe = clean(t.exe.trim_end_matches(".exe"));
    if exe.is_empty() { "Clip".into() } else { exe }
}

/// Output size for a source of `sw` x `sh`: its own shape at `lines` lines
/// (never above the source), so a 21:9 source stays 21:9.
fn fit_size(sw: u32, sh: u32, lines: u32) -> (u32, u32) {
    let h = sh.min(lines).max(2) as u64;
    let w = ((sw as u64 * h / sh.max(1) as u64) as u32) & !1;
    (w.max(2), (h as u32) & !1)
}

/// A game window's client size (falls back to 1920x1080).
fn game_client(hwnd: HWND) -> (u32, u32) {
    let mut r = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut r) }.is_err() || r.right <= 0 || r.bottom <= 0 {
        return (1920, 1080);
    }
    (r.right as u32, r.bottom as u32)
}

fn level_text(ring: &Mutex<AudioRing>, running: bool) -> (f32, String) {
    if !running {
        return (-100.0, "off".into());
    }
    let a = ring.lock().unwrap();
    if a.last_push == 0 || util::now_100ns() - a.last_push > 15_000_000 {
        return (-100.0, "nothing playing".into());
    }
    let db = a.level_db(0.4);
    if db < -60.0 { (-100.0, "silent".into()) } else { (db, format!("{db:.0} dB")) }
}

impl App {
    fn stop_capture(&mut self) {
        if let Some(p) = self.pipe.take() {
            p.stop();
        }
        if let Some(a) = self.acap.take() {
            a.stop();
        }
        if let Some(m) = self.mcap.take() {
            m.stop();
        }
    }

    /// What to record for sound. `game_pid` is set when a game is captured.
    fn audio_source(&self, game_pid: Option<u32>) -> Option<AudioSource> {
        match (self.audio_mode.as_str(), game_pid) {
            ("off", _) => None,
            ("game", Some(pid)) => Some(AudioSource::Process(pid)),
            _ => Some(AudioSource::Device(find_device(&audio::list_devices(), &self.audio_device))),
        }
    }

    fn start_capture(&mut self, src: Source, w: u32, h: u32, game_pid: Option<u32>, name: String) {
        self.stop_capture();
        *self.video.lock().unwrap() = VideoRing::new(self.cur_fps, self.seconds);
        *self.audio.lock().unwrap() = AudioRing::new(self.seconds);
        *self.mic.lock().unwrap() = AudioRing::new(self.seconds);
        match VideoPipeline::start(&self.cfg, src, w, h, self.cur_fps, self.video.clone()) {
            Ok(p) => self.pipe = Some(p),
            Err(e) => {
                log(format!("could not start ffmpeg ({}): {e}", self.cfg.ffmpeg.display()));
                return;
            }
        }
        if std::env::var_os("CLIPPER_NOAUDIO").is_none() {
            if let Some(a) = self.audio_source(game_pid) {
                self.acap = Some(AudioCapture::start(a, self.audio.clone()));
            }
            self.start_mic();
        }
        log(format!("capturing {name} at {w}x{h} {} fps", self.cur_fps));
        self.source_name = name;
    }

    fn start_mic(&mut self) {
        if let Some(m) = self.mcap.take() {
            m.stop();
        }
        if self.mic_on && self.capturing() {
            let id = find_device(&audio::list_mics(), &self.mic_device);
            self.mcap = Some(AudioCapture::start(AudioSource::Mic(id), self.mic.clone()));
        }
    }

    /// Swap only the sound source; the video buffer keeps running.
    fn restart_audio(&mut self) {
        if let Some(a) = self.acap.take() {
            a.stop();
        }
        if !self.capturing() {
            return;
        }
        let pid = if self.mode == Mode::Game { self.target.as_ref().map(|t| t.pid) } else { None };
        match self.audio_source(pid) {
            Some(src) => {
                log(format!("audio -> {src:?}"));
                self.acap = Some(AudioCapture::start(src, self.audio.clone()));
            }
            None => log("audio -> off"),
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        log(format!("mode -> {mode:?}"));
        self.mode = mode;
        self.stop_capture();
        self.target = None;
        self.last_game = None;
        self.source_name.clear();
        if matches!(mode, Mode::Monitor(_)) {
            self.start_monitor();
        }
        let t = wide(&format!("Clipper - {}", self.mode.label()));
        unsafe {
            let _ = SetWindowTextW(self.hwnd, PCWSTR(t.as_ptr()));
        }
        self.tick();
    }

    /// Remember what is being captured (for the size / fps menus) and pick the
    /// frame rate: the chosen one, but never above the display refresh rate.
    fn set_source(&mut self, w: u32, h: u32, refresh: u32) {
        self.src = (w, h, refresh);
        self.cur_fps = if refresh >= 10 { self.fps.min(refresh) } else { self.fps };
    }

    /// When nothing is captured, the menus offer what the biggest display allows.
    fn source_or_largest(&self) -> (u32, u32, u32) {
        if self.capturing() && self.src.1 > 0 {
            return self.src;
        }
        detect::monitors()
            .iter()
            .map(|m| (m.width as u32, m.height as u32, m.refresh))
            .max_by_key(|m| (m.1, m.2))
            .unwrap_or((1920, 1080, 60))
    }

    fn start_monitor(&mut self) {
        let mons = detect::monitors();
        let Mode::Monitor(i) = self.mode else { return };
        // a display that's gone (unplugged third screen) falls back to the main one
        let Some(m) = mons.get(i).or(mons.first()) else {
            log("no monitor found");
            return;
        };
        let (mw, mh, mr) = (m.width as u32, m.height as u32, m.refresh);
        let handle = m.handle.0 as u64;
        self.set_source(mw, mh, mr);
        let (w, h) = fit_size(mw, mh, self.res_height);
        let name = format!("{} ({}x{})", self.mode.label(), m.width, m.height);
        self.start_capture(Source::Monitor(handle), w, h, None, name);
    }

    fn tick(&mut self) {
        self.ticks += 1;
        // ffmpeg died (window closed mid-capture, driver hiccup, ...)
        if self.pipe.as_ref().is_some_and(|p| !p.alive.load(Ordering::SeqCst)) {
            log("capture ended");
            self.stop_capture();
            self.target = None;
            self.retry_at = self.ticks + 3;
        }
        match self.mode {
            Mode::Game => {
                if let Some(t) = &self.target
                    && !unsafe { IsWindow(Some(t.hwnd)) }.as_bool()
                {
                    log(format!("{} closed", t.exe));
                    self.stop_capture(); // keep the buffer: it can still be saved
                    self.target = None;
                }
                let found = match &self.force_title {
                    Some(title) => detect::window_by_title(title),
                    None => detect::foreground_game(&self.cfg),
                };
                if let Some(t) = found
                    && self.target.as_ref().map(|c| c.hwnd) != Some(t.hwnd)
                    && self.ticks >= self.retry_at
                {
                    let name = if t.title.is_empty() { t.exe.clone() } else { t.title.clone() };
                    log(format!("game: {} (pid {}) \"{}\"", t.exe, t.pid, t.title));
                    let (cw, ch) = game_client(t.hwnd);
                    self.set_source(cw, ch, detect::monitor_of(t.hwnd).map(|m| m.refresh).unwrap_or(0));
                    let (w, h) = fit_size(cw, ch, self.res_height);
                    self.start_capture(Source::Window(t.hwnd.0 as u64), w, h, Some(t.pid), name);
                    self.last_game = Some(file_name_for(&t));
                    self.target = Some(t);
                }
            }
            Mode::Monitor(_) => {
                if self.pipe.is_none() && self.ticks >= self.retry_at {
                    self.start_monitor();
                }
            }
            Mode::Off => {}
        }
        if self.ticks == 4 {
            promote_tray_icon();
        }
        self.update_tray(false);
    }

    fn capturing(&self) -> bool {
        self.pipe.is_some()
    }

    fn what(&self) -> String {
        match self.mode {
            Mode::Off => "off".into(),
            _ if self.capturing() => self.source_name.clone(),
            Mode::Game => "waiting for a fullscreen game".into(),
            _ => "starting...".into(),
        }
    }

    fn tooltip(&self) -> String {
        let secs = self.video.lock().unwrap().seconds_buffered() as u32;
        let (_, snd) = level_text(&self.audio, self.acap.is_some());
        let (_, mic) = level_text(&self.mic, self.mcap.is_some());
        format!(
            "Clipper: {} - {}\nbuffer {}:{:02}\nsound {snd}, mic {mic}",
            self.mode.label(),
            self.what(),
            secs / 60,
            secs % 60
        )
    }

    /// Everything the window shows.
    fn ui_state(&mut self) -> ui::UiState {
        // device lists are cheap but not free; refresh them every few pushes
        if self.devices.is_empty() || self.ticks % 5 == 0 {
            self.devices = audio::list_devices();
            self.mics = audio::list_mics();
        }
        let (buffer, mb) = {
            let v = self.video.lock().unwrap();
            (v.seconds_buffered(), v.megabytes())
        };
        let (level, snd_text) = level_text(&self.audio, self.acap.is_some());
        let (mic_level, mic_text) = level_text(&self.mic, self.mcap.is_some());
        // the full device name, even if the ini holds the short one
        let full = |list: &[DeviceInfo], want: &str| {
            list.iter()
                .find(|d| device_matches(&d.name, want))
                .map(|d| d.name.clone())
                .unwrap_or_else(|| "default".into())
        };
        let names = |l: &[DeviceInfo]| l.iter().map(|d| d.name.clone()).collect::<Vec<_>>();
        let (last, last_ok) = save::LAST
            .lock()
            .unwrap()
            .as_ref()
            .map(|(t, ok, _)| (t.clone(), *ok))
            .unwrap_or_default();
        ui::UiState {
            capturing: self.capturing(),
            mode: self.mode.key().into(),
            what: self.what(),
            buffer,
            seconds: self.seconds,
            mb,
            audio: self.audio_mode.clone(),
            device: full(&self.devices, &self.audio_device),
            devices: names(&self.devices),
            default_device: audio::default_device_name(),
            level,
            level_text: snd_text,
            mic_on: self.mic_on,
            mic: full(&self.mics, &self.mic_device),
            mics: names(&self.mics),
            default_mic: audio::default_mic_name(),
            mic_level,
            mic_level_text: mic_text,
            mic_volume: self.mic_volume,
            volume: self.audio_volume,
            out: self.out_dir.to_string_lossy().into(),
            pinned: self.pinned,
            last,
            last_ok,
            startup: startup_enabled(),
            startup_mode: self.startup_mode.clone(),
            compact: self.prefs.get("view").is_some_and(|v| v == "compact"),
            height: self.res_height,
            fps: self.fps,
            cur_fps: self.cur_fps,
            src: self.source_or_largest(),
            trim_mb: self.prefs.get("trim_mb").and_then(|v| v.parse().ok()).unwrap_or(20.0),
            trim_codec: self.prefs.get("trim_codec").cloned().unwrap_or_else(|| "av1".into()),
            monitors: detect::monitors().iter().take(detect::MONITOR_NAMES.len()).map(|m| format!("{}x{}", m.width, m.height)).collect(),
            hotkey_save: self.hotkey_save.clone(),
            hotkey_window: self.hotkey_window.clone(),
            hotkey_error: self.hotkey_error.clone(),
        }
    }

    /// (Re)register both global shortcuts; a combo another app already owns
    /// is reported in the window instead.
    fn register_hotkeys(&mut self) {
        let mut errors = Vec::new();
        for (id, text) in [(HOTKEY_SAVE, self.hotkey_save.clone()), (HOTKEY_WINDOW, self.hotkey_window.clone())] {
            unsafe {
                let _ = UnregisterHotKey(Some(self.hwnd), id);
            }
            let Some((mods, vk)) = hotkey::parse(&text) else { continue }; // "off"
            let ok = unsafe { RegisterHotKey(Some(self.hwnd), id, HOT_KEY_MODIFIERS(mods) | MOD_NOREPEAT, vk) }.is_ok();
            if !ok {
                errors.push(text);
            }
        }
        // just the combos ("Shift+F3 and Ctrl+Alt+C"); the window words it
        self.hotkey_error = errors.join(" and ");
        log(format!("hotkeys: save {}, window {} {}", self.hotkey_save, self.hotkey_window, self.hotkey_error));
    }

    fn unregister_hotkeys(&self) {
        unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_SAVE);
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_WINDOW);
        }
    }

    fn push_ui(&mut self) {
        if self.ui.as_ref().is_some_and(|u| u.visible()) {
            let s = self.ui_state();
            if let Some(ui) = &self.ui {
                ui.update(s);
            }
        }
    }

    fn ui_visible(&self) -> bool {
        self.ui.as_ref().is_some_and(|u| u.visible())
    }

    /// The window is created once and then only shown / hidden.
    fn show_ui(&mut self) {
        if self.ui.is_none() {
            self.ui = Ui::create(self.hwnd, self.window_pos, self.pinned, self.cfg.ffmpeg.clone());
        }
        let s = self.ui_state();
        if let Some(ui) = &self.ui {
            ui.update(s);
            ui.show();
            unsafe { SetTimer(Some(self.hwnd), TIMER_UI, 250, None) };
        }
    }

    fn hide_ui(&mut self) {
        if let Some(ui) = &self.ui
            && ui.visible()
        {
            if let Some((x, y)) = ui.position() {
                self.window_pos = Some((x, y));
                config::set_value("window_x", &x.to_string());
                config::set_value("window_y", &y.to_string());
            }
            ui.hide();
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_UI);
            }
        }
    }

    fn update_tray(&self, add: bool) {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: if self.capturing() { self.icon_on } else { self.icon_off },
            ..Default::default()
        };
        let tip: Vec<u16> = self.tooltip().encode_utf16().take(127).collect();
        nid.szTip[..tip.len()].copy_from_slice(&tip);
        unsafe {
            if add || !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
                let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            }
        }
    }

    fn remove_tray(&self) {
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }

    fn save(&self) {
        if self.video.lock().unwrap().seconds_buffered() < 1.0 {
            log("save requested with an empty buffer");
            save::play(&self.cfg.error_sound);
            return;
        }
        let job = save::SaveJob {
            ffmpeg: self.cfg.ffmpeg.clone(),
            out_dir: self.out_dir.clone(),
            seconds: self.seconds,
            fps: self.cur_fps,
            audio_kbps: self.cfg.audio_kbps,
            av_offset_ms: self.cfg.av_offset_ms,
            save_sound: self.cfg.save_sound.clone(),
            error_sound: self.cfg.error_sound.clone(),
            audio_gain: self.audio_volume as f32 / 100.0,
            mic_gain: self.mcap.as_ref().map(|_| self.mic_volume as f32 / 100.0),
            notify: self.hwnd.0 as isize,
            notify_msg: WM_SAVED,
            name: match (&self.mode, &self.last_game) {
                (Mode::Game, Some(g)) => g.clone(),
                _ => "Clip".into(),
            },
        };
        save::save_clip(job, self.video.clone(), self.audio.clone(), self.mic.clone(), self.saving.clone());
    }

    /// Shows the tray menu and returns the chosen command id (0 = none).
    fn menu(&self) -> u32 {
        unsafe {
            let Ok(m) = CreatePopupMenu() else { return 0 };
            let add = |flags: MENU_ITEM_FLAGS, id: u32, text: &str| {
                let t = wide(text);
                let _ = AppendMenuW(m, flags, id as usize, PCWSTR(t.as_ptr()));
            };
            add(MF_STRING | MF_DEFAULT, ID_SHOW, "Open Clipper");
            add(MF_SEPARATOR, 0, "");
            let count = detect::monitors().len().clamp(1, detect::MONITOR_NAMES.len());
            let mut modes: Vec<(u32, Mode, String)> = (0..count).map(|i| (ID_MONITOR + i as u32, Mode::Monitor(i), Mode::Monitor(i).label())).collect();
            modes.push((ID_GAME, Mode::Game, "Game (auto-detect)".into()));
            modes.push((ID_OFF, Mode::Off, "Off".into()));
            for (id, mode, text) in modes {
                let chk = if self.mode == mode { MF_CHECKED } else { MF_UNCHECKED };
                add(MF_STRING | chk, id, &text);
            }
            add(MF_SEPARATOR, 0, "");
            add(MF_STRING, ID_SAVE, &format!("Save clip now\t{}", self.hotkey_save));
            add(MF_STRING, ID_FOLDER, "Open clips folder");
            add(MF_SEPARATOR, 0, "");
            add(MF_STRING, ID_QUIT, "Quit Clipper");
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            let _ = SetForegroundWindow(self.hwnd);
            let id = TrackPopupMenu(m, TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY, p.x, p.y, None, self.hwnd, None);
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(m);
            id.0 as u32
        }
    }
}

/// Windows 11 hides new tray icons under the ^ overflow. Mark ours as
/// promoted so it sits in the visible tray (takes effect on next launch the
/// first time).
fn promote_tray_icon() {
    let Ok(exe) = std::env::current_exe() else { return };
    let exe = exe.to_string_lossy().to_lowercase();
    unsafe {
        let mut root = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, w!("Control Panel\\NotifyIconSettings"), Some(0), KEY_READ, &mut root).is_err() {
            return;
        }
        let mut i = 0u32;
        loop {
            let mut name = [0u16; 64];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(root, i, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None).is_err() {
                break;
            }
            i += 1;
            let mut key = HKEY::default();
            if RegOpenKeyExW(root, PCWSTR(name.as_ptr()), Some(0), KEY_READ | KEY_SET_VALUE, &mut key).is_err() {
                continue;
            }
            let mut buf = [0u16; 520];
            let mut size = (buf.len() * 2) as u32;
            let got = RegQueryValueExW(key, w!("ExecutablePath"), None, None, Some(buf.as_mut_ptr() as *mut u8), Some(&mut size));
            if got.is_ok() {
                let path = String::from_utf16_lossy(&buf[..(size as usize / 2)]).trim_end_matches('\0').to_lowercase();
                if path == exe {
                    let one = 1u32.to_le_bytes();
                    let _ = RegSetValueExW(key, w!("IsPromoted"), Some(0), REG_DWORD, Some(&one));
                }
            }
            let _ = RegCloseKey(key);
        }
        let _ = RegCloseKey(root);
    }
}

/// GuideBook's icon shape: a flat disc (r = 120/256 of the canvas).
fn make_icon(rgb: (u8, u8, u8)) -> HICON {
    const S: i32 = 32;
    let r = 120.0 / 256.0 * S as f32;
    let mut px = vec![0u32; (S * S) as usize];
    for y in 0..S {
        for x in 0..S {
            let (dx, dy) = (x as f32 + 0.5 - S as f32 / 2.0, y as f32 + 0.5 - S as f32 / 2.0);
            let a = (r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0); // anti-aliased edge
            if a > 0.0 {
                let c = |v: u8| (v as f32 * a) as u32; // premultiplied
                px[(y * S + x) as usize] = ((a * 255.0) as u32) << 24 | c(rgb.0) << 16 | c(rgb.1) << 8 | c(rgb.2);
            }
        }
    }
    unsafe {
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: S,
                biHeight: -S,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len());
        let mask = CreateBitmap(S, S, 1, 1, None);
        let info = ICONINFO { fIcon: TRUE, hbmMask: mask, hbmColor: color, ..Default::default() };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

/// Windows' folder picker, starting at `start`.
fn pick_folder(owner: HWND, start: &std::path::Path) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let s = wide(&start.to_string_lossy());
        if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(s.as_ptr()), None) {
            let _ = dlg.SetFolder(&item);
        }
        let t = wide("Save clips to");
        let _ = dlg.SetTitle(PCWSTR(t.as_ptr()));
        dlg.Show(Some(owner)).ok()?;
        let item = dlg.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        path.map(PathBuf::from)
    }
}

fn open_path(p: &std::path::Path) {
    let d = wide(&p.to_string_lossy());
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(d.as_ptr()), None, None, SW_SHOWNORMAL);
    }
}

fn run_menu_command(hwnd: HWND, id: u32) {
    match id {
        ID_SHOW => handle_command(hwnd, "show"),
        id if (ID_MONITOR..ID_MONITOR + detect::MONITOR_NAMES.len() as u32).contains(&id) => {
            handle_command(hwnd, &format!("mode {}", detect::MONITOR_NAMES[(id - ID_MONITOR) as usize].0))
        }
        ID_GAME => handle_command(hwnd, "mode game"),
        ID_OFF => handle_command(hwnd, "mode off"),
        ID_SAVE => handle_command(hwnd, "save"),
        ID_FOLDER => handle_command(hwnd, "openfolder"),
        ID_QUIT => handle_command(hwnd, "quit"),
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wp.0 == TIMER_TICK => {
            with_app(|a| a.tick());
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_UI => {
            with_app(|a| a.push_ui());
            LRESULT(0)
        }
        WM_HOTKEY if wp.0 as i32 == HOTKEY_SAVE => {
            with_app(|a| a.save());
            LRESULT(0)
        }
        WM_HOTKEY if wp.0 as i32 == HOTKEY_WINDOW => {
            handle_command(hwnd, "toggle");
            LRESULT(0)
        }
        WM_TRAY => {
            match (lp.0 & 0xffff) as u32 {
                WM_LBUTTONUP => handle_command(hwnd, "toggle"),
                WM_RBUTTONUP => {
                    let id = with_app(|a| a.menu()).unwrap_or(0);
                    if id != 0 {
                        run_menu_command(hwnd, id);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_UI_CMD => {
            let cmds: Vec<String> = ui::QUEUE.lock().unwrap().drain(..).collect();
            for c in cmds {
                handle_command(hwnd, &c);
            }
            LRESULT(0)
        }
        WM_SAVED => {
            with_app(|a| a.push_ui());
            LRESULT(0)
        }
        WM_COPYDATA => {
            let cds = unsafe { &*(lp.0 as *const COPYDATASTRUCT) };
            let bytes = unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
            let cmd = String::from_utf8_lossy(bytes).to_string();
            log(format!("command: {cmd}"));
            // handled after SendMessage returns, so the sender isn't blocked
            for c in cmd.split('|') {
                ui::QUEUE.lock().unwrap().push_back(c.to_string());
            }
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_UI_CMD, WPARAM(0), LPARAM(0));
            }
            LRESULT(1)
        }
        WM_DESTROY => {
            with_app(|a| {
                a.hide_ui();
                a.stop_capture();
                a.remove_tray();
            });
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => {
            if msg != 0 && msg == TASKBAR_CREATED.load(Ordering::SeqCst) {
                with_app(|a| a.update_tray(true)); // Explorer restarted
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
    }
}

/// One command, from the command line, the tray menu or the window.
fn handle_command(hwnd: HWND, cmd: &str) {
    let (verb, arg) = cmd.split_once(' ').unwrap_or((cmd, ""));
    let arg = arg.trim();
    let on = matches!(arg, "on" | "1" | "true");
    match verb {
        "mode" => {
            if let Some(m) = Mode::parse(arg) {
                with_app(|a| {
                    a.force_title = None;
                    a.set_mode(m);
                    a.push_ui();
                });
            }
        }
        "window" => {
            with_app(|a| {
                a.force_title = Some(arg.to_string());
                a.set_mode(Mode::Game)
            });
        }
        "audio" if matches!(arg, "game" | "device" | "off" | "on") => {
            with_app(|a| {
                // "on" (the window's speaker toggle) brings back the last sound mode
                let m = if arg == "on" { a.last_audio_on.clone() } else { arg.to_string() };
                if m != "off" {
                    a.last_audio_on = m.clone();
                }
                a.audio_mode = m.clone();
                config::set_value("audio", &m);
                a.restart_audio();
                a.push_ui();
            });
        }
        "device" => {
            with_app(|a| {
                a.audio_device = arg.to_string();
                config::set_value("audio_device", arg);
                if a.audio_mode == "off" {
                    a.audio_mode = "device".into();
                    config::set_value("audio", "device");
                }
                a.restart_audio();
                a.push_ui();
            });
        }
        "mic" => {
            with_app(|a| {
                a.mic_on = on;
                config::set_value("mic", if on { "on" } else { "off" });
                a.start_mic();
                a.push_ui();
            });
        }
        "micdevice" => {
            with_app(|a| {
                a.mic_device = arg.to_string();
                config::set_value("mic_device", arg);
                a.start_mic();
                a.push_ui();
            });
        }
        "volume" => {
            if let Ok(v) = arg.parse::<u32>() {
                with_app(|a| {
                    a.audio_volume = v.min(200);
                    config::set_value("audio_volume", &a.audio_volume.to_string());
                });
            }
        }
        "micvolume" => {
            if let Ok(v) = arg.parse::<u32>() {
                with_app(|a| {
                    a.mic_volume = v.min(200);
                    config::set_value("mic_volume", &a.mic_volume.to_string());
                });
            }
        }
        "height" | "fps" => {
            if let Ok(v) = arg.parse::<u32>() {
                with_app(|a| {
                    if verb == "height" {
                        a.res_height = v.clamp(240, 4320);
                        config::set_value("height", &a.res_height.to_string());
                    } else {
                        a.fps = v.clamp(10, 360);
                        config::set_value("fps", &a.fps.to_string());
                    }
                    // the encoder and buffers are sized by these, so capture restarts
                    let m = a.mode;
                    a.set_mode(m);
                    a.push_ui();
                });
            }
        }
        "seconds" => {
            if let Ok(s) = arg.parse::<u32>() {
                with_app(|a| {
                    a.seconds = s.clamp(5, config::MAX_SECONDS);
                    config::set_value("seconds", &a.seconds.to_string());
                    // the buffers are sized by length, so capture restarts
                    let m = a.mode;
                    a.set_mode(m);
                    a.push_ui();
                });
            }
        }
        "out" if !arg.is_empty() => {
            with_app(|a| {
                a.out_dir = PathBuf::from(arg);
                config::set_value("output_dir", arg);
                a.push_ui();
            });
        }
        "browse" => {
            // the dialog runs its own message loop, so keep the app unborrowed
            let Some((owner, start)) = with_app(|a| (a.ui.as_ref().map(|u| u.hwnd).unwrap_or(a.hwnd), a.out_dir.clone())) else {
                return;
            };
            if let Some(p) = pick_folder(owner, &start) {
                handle_command(hwnd, &format!("out {}", p.display()));
            }
        }
        "openfolder" => {
            if let Some(d) = with_app(|a| a.out_dir.clone()) {
                let _ = std::fs::create_dir_all(&d);
                open_path(&d);
            }
        }
        "showlast" => {
            let last = save::LAST.lock().unwrap().as_ref().map(|(_, ok, p)| (*ok, p.clone()));
            if let Some((true, p)) = last {
                let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{}", p.display())).spawn();
            }
        }
        "pin" => {
            with_app(|a| {
                a.pinned = on;
                config::set_value("pinned", if on { "on" } else { "off" });
                if let Some(ui) = &a.ui {
                    ui.set_pinned(on);
                }
                a.push_ui();
            });
        }
        "show" => {
            with_app(|a| a.show_ui());
        }
        "hide" => {
            with_app(|a| a.hide_ui());
        }
        "toggle" => {
            with_app(|a| if a.ui_visible() { a.hide_ui() } else { a.show_ui() });
        }
        "ready" => {
            with_app(|a| a.push_ui());
        }
        "pref" => {
            // "pref view compact" / "pref view full"
            let (k, v) = arg.split_once(' ').unwrap_or((arg, ""));
            if config::PREF_KEYS.contains(&k) && !v.is_empty() {
                with_app(|a| {
                    a.prefs.insert(k.to_string(), v.to_string());
                    config::set_value(&format!("ui_{k}"), v);
                    a.push_ui();
                });
            }
        }
        "startup" => {
            with_app(|a| {
                set_startup(on, &a.startup_mode);
                a.push_ui();
            });
        }
        "startupmode" if Mode::parse(arg).is_some() => {
            with_app(|a| {
                a.startup_mode = arg.to_string();
                config::set_value("startup_mode", arg);
                if startup_enabled() {
                    set_startup(true, arg); // rewrite the entry with the new mode
                }
                a.push_ui();
            });
        }
        "hotkey" => {
            // "hotkey save Ctrl+F9" / "hotkey window off"
            let (which, combo) = arg.split_once(' ').unwrap_or((arg, ""));
            let combo = combo.trim();
            let text = if combo.eq_ignore_ascii_case("off") {
                Some("off".to_string())
            } else {
                hotkey::parse(combo).and_then(|(m, vk)| hotkey::format(m, vk))
            };
            if let Some(text) = text
                && matches!(which, "save" | "window")
            {
                with_app(|a| {
                    if which == "save" { a.hotkey_save = text.clone() } else { a.hotkey_window = text.clone() }
                    config::set_value(&format!("hotkey_{which}"), &text);
                    a.register_hotkeys();
                    a.push_ui();
                });
            }
        }
        "bind" => {
            with_app(|a| {
                a.show_ui();
                if let Some(ui) = &a.ui {
                    ui.bind(arg);
                }
            });
        }
        "hotkeys" => {
            // the window is waiting for a new combo: let the current ones through
            with_app(|a| if arg == "pause" { a.unregister_hotkeys() } else { a.register_hotkeys() });
        }
        "trim" if !arg.is_empty() => {
            // open the window with this clip in the trimmer
            with_app(|a| {
                a.show_ui();
                if let Some(ui) = &a.ui {
                    ui.trim_file(PathBuf::from(arg));
                }
            });
        }
        "save" => {
            with_app(|a| a.save());
        }
        "quit" => {
            // close the window first: Windows destroys owned windows before
            // our WM_DESTROY, which would lose the window position
            with_app(|a| a.hide_ui());
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        _ => {}
    }
}

fn send_to_running(existing: HWND, cmd: &str) {
    let cds = COPYDATASTRUCT {
        dwData: 0,
        cbData: cmd.len() as u32,
        lpData: cmd.as_ptr() as *mut _,
    };
    unsafe {
        SendMessageW(existing, WM_COPYDATA, Some(WPARAM(0)), Some(LPARAM(&cds as *const _ as isize)));
    }
}

fn main() {
    std::panic::set_hook(Box::new(|info| log(format!("PANIC: {info}"))));
    // command line -> one command string, e.g. "mode game|show"
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cmds: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let next = args.get(i + 1).cloned().unwrap_or_default();
        let (verb, takes) = match args[i].as_str() {
            "--mode" => ("mode", true),
            "--window" => ("window", true),
            "--audio" => ("audio", true),
            "--device" => ("device", true),
            "--mic" => ("mic", true),
            "--mic-device" => ("micdevice", true),
            "--height" => ("height", true),
            "--fps" => ("fps", true),
            "--trim" => ("trim", true),
            "--bind" => ("bind", true),
            "--hotkey" => ("hotkey", true),
            "--show" => ("show", false),
            "--toggle" => ("toggle", false),
            "--save" => ("save", false),
            "--quit" => ("quit", false),
            _ => ("", false),
        };
        if !verb.is_empty() {
            cmds.push(if takes { format!("{verb} {next}") } else { verb.into() });
        }
        i += if takes { 2 } else { 1 };
    }
    let cmd = cmds.join("|");

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if let Ok(existing) = FindWindowW(class(), None) {
            if !cmd.is_empty() {
                send_to_running(existing, &cmd);
            }
            return;
        }
        if cmds.iter().any(|c| c == "quit" || c == "save") {
            return; // nothing running to quit / save from
        }

        // the UI thread lists devices and hosts WebView2: single-threaded COM
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let cfg = Arc::new(Config::load());
        log(format!("--- Clipper start, ffmpeg = {}", cfg.ffmpeg.display()));
        log(format!(
            "audio = {} on '{}', mic = {} on '{}' (Windows defaults: '{}', '{}')",
            cfg.audio,
            cfg.audio_device,
            cfg.mic,
            cfg.mic_device,
            audio::default_device_name(),
            audio::default_mic_name()
        ));
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst, lpszClassName: class(), ..Default::default() };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0), class(), w!("Clipper - Off"), WS_OVERLAPPED,
            0, 0, 0, 0, None, None, Some(hinst), None,
        )
        .expect("window");

        let app = App {
            cfg: cfg.clone(),
            hwnd,
            mode: Mode::Off,
            video: Arc::new(Mutex::new(VideoRing::new(cfg.fps, cfg.seconds))),
            fps: cfg.fps,
            cur_fps: cfg.fps,
            res_height: cfg.height,
            src: (0, 0, 0),
            audio: Arc::new(Mutex::new(AudioRing::new(cfg.seconds))),
            mic: Arc::new(Mutex::new(AudioRing::new(cfg.seconds))),
            pipe: None,
            acap: None,
            mcap: None,
            target: None,
            source_name: String::new(),
            last_game: None,
            force_title: None,
            saving: Arc::new(AtomicBool::new(false)),
            ticks: 0,
            retry_at: 0,
            icon_on: make_icon(RED),
            icon_off: make_icon(GREY),
            audio_mode: cfg.audio.clone(),
            last_audio_on: if cfg.audio == "off" { "game".into() } else { cfg.audio.clone() },
            audio_device: cfg.audio_device.clone(),
            audio_volume: cfg.audio_volume,
            mic_on: cfg.mic,
            mic_device: cfg.mic_device.clone(),
            mic_volume: cfg.mic_volume,
            seconds: cfg.seconds,
            out_dir: cfg.output_dir.clone(),
            pinned: cfg.pinned,
            window_pos: cfg.window_pos,
            ui: None,
            devices: Vec::new(),
            mics: Vec::new(),
            prefs: cfg.prefs.clone(),
            startup_mode: cfg.startup_mode.clone(),
            hotkey_save: cfg.hotkey_save.clone(),
            hotkey_window: cfg.hotkey_window.clone(),
            hotkey_error: String::new(),
        };
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::SeqCst);
        APP.with(|a| *a.borrow_mut() = Some(app));
        with_app(|a| {
            a.update_tray(true);
            a.register_hotkeys();
        });
        SetTimer(Some(hwnd), TIMER_TICK, 1000, None);
        for c in &cmds {
            handle_command(hwnd, c);
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        APP.with(|a| *a.borrow_mut() = None);
        log("--- Clipper exit");
    }
}
