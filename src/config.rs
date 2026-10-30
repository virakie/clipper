//! clipper.ini next to the exe (portable: nothing is stored anywhere else).
//! Created with defaults on first run; the window writes changes straight back.

use crate::util::{exe_dir, log};
use std::path::PathBuf;

pub struct Config {
    /// "auto" looks next to clipper.exe, then scoop, then PATH
    pub ffmpeg: PathBuf,
    pub output_dir: PathBuf,
    pub seconds: u32,
    pub fps: u32,
    /// output lines (1080 = 1080p); never above the source
    pub height: u32,
    pub qp: u32,
    pub audio_kbps: u32,
    /// Shifts audio relative to video, in ms (+ = audio later). Calibrated
    /// with tests/synctest.pyw.
    pub av_offset_ms: i64,
    pub save_sound: String,
    pub error_sound: String,
    /// Fullscreen windows from these exes are never treated as games.
    pub exclude: Vec<String>,
    /// "game" (game's own sound in Game mode), "device" or "off"
    pub audio: String,
    /// playback device recorded in "device" mode (and outside Game mode)
    pub audio_device: String,
    /// percent applied to the recorded sound, 0-200
    pub audio_volume: u32,
    pub mic: bool,
    pub mic_device: String,
    /// percent, 0-200
    pub mic_volume: u32,
    pub pinned: bool,
    /// last window position (physical pixels); None = bottom-right corner
    pub window_pos: Option<(i32, i32)>,
    /// window settings (stored as ui_<key>): view = full | compact
    pub prefs: std::collections::BTreeMap<String, String>,
    /// mode Clipper starts in when launched with Windows
    pub startup_mode: String,
    /// global shortcuts ("Shift+F3"; "off" = none)
    pub hotkey_save: String,
    pub hotkey_window: String,
}

/// Look settings the window may store.
pub const PREF_KEYS: [&str; 3] = ["view", "trim_mb", "trim_codec"];

const DEFAULT_EXCLUDE: &str = "explorer.exe, chrome.exe, msedge.exe, firefox.exe, helium.exe, \
brave.exe, opera.exe, mpv.exe, vlc.exe, mpc-hc64.exe, potplayermini64.exe, obs64.exe, resolve.exe, \
cinema 4d.exe, houdinifx.exe, blender.exe, applicationframehost.exe, steam.exe, steamwebhelper.exe, \
discord.exe, code.exe, claude.exe, jellyfin.exe, kodi.exe, textinputhost.exe, lockapp.exe";

/// Videos\Clipper in the user's profile.
fn default_out() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Videos").join("Clipper")
}
const DEFAULT_SAVE_SOUND: &str = r"C:\Windows\Media\Speech On.wav";
const DEFAULT_ERROR_SOUND: &str = r"C:\Windows\Media\Speech Misrecognition.wav";
/// "default" = whatever Windows plays through
const DEFAULT_DEVICE: &str = "default";
/// longest clip (15 min); the buffer lives in RAM, ~0.3-0.6 GB per 2 min at 1080p60
pub const MAX_SECONDS: u32 = 900;

/// ffmpeg next to clipper.exe wins (portable), then scoop, then PATH.
fn find_ffmpeg() -> PathBuf {
    let local = exe_dir().join("ffmpeg.exe");
    if local.exists() {
        return local;
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        let p = PathBuf::from(home).join(r"scoop\apps\ffmpeg\current\bin\ffmpeg.exe");
        if p.exists() {
            return p;
        }
    }
    PathBuf::from("ffmpeg.exe")
}

fn default_text() -> String {
    format!(
        "# Clipper settings. The Clipper window changes most of these for you.\n\
         # Edits made here need a Clipper restart (Quit, then start it again).\n\
         \n\
         # ffmpeg 8.0+ (needs gfxcapture). auto = ffmpeg.exe next to clipper.exe,\n\
         # then scoop, then PATH. Or a full path to ffmpeg.exe.\n\
         ffmpeg = auto\n\
         output_dir = {out}\n\
         # Clip length in seconds\n\
         seconds = 120\n\
         fps = 60\n\
         # output lines: 1080 = 1080p (never above the screen or game window)\n\
         height = 1080\n\
         # NVENC constant QP: lower = better + bigger (18-26 sensible)\n\
         qp = 21\n\
         audio_kbps = 192\n\
         # + delays audio, - advances it (ms)\n\
         av_offset_ms = 0\n\
         save_sound = {snd}\n\
         error_sound = {err}\n\
         # Sound: game = only the game's own sound in Game mode (other modes use\n\
         # audio_device), device = everything on audio_device, off = none\n\
         audio = game\n\
         audio_device = {dev}\n\
         audio_volume = 100\n\
         # Microphone, mixed into the clip: on/off, device name or default, volume %\n\
         mic = on\n\
         mic_device = default\n\
         mic_volume = 100\n\
         # Shortcuts (work in any app): e.g. Shift+F3, Ctrl+Alt+C, F9; off = none\n\
         hotkey_save = Shift+F3\n\
         hotkey_window = Ctrl+Alt+C\n\
         # Fullscreen apps that are never 'games' (comma separated exe names)\n\
         exclude = {ex}\n",
        out = default_out().display(),
        snd = DEFAULT_SAVE_SOUND,
        err = DEFAULT_ERROR_SOUND,
        dev = DEFAULT_DEVICE,
        ex = DEFAULT_EXCLUDE,
    )
}

impl Config {
    pub fn load() -> Config {
        let path = exe_dir().join("clipper.ini");
        if !path.exists() {
            let _ = std::fs::write(&path, default_text());
        }
        let text = std::fs::read_to_string(&path).unwrap_or_else(|_| default_text());
        let mut c = Config {
            ffmpeg: find_ffmpeg(),
            output_dir: default_out(),
            seconds: 120,
            fps: 60,
            height: 1080,
            qp: 21,
            audio_kbps: 192,
            av_offset_ms: 0,
            save_sound: DEFAULT_SAVE_SOUND.into(),
            error_sound: DEFAULT_ERROR_SOUND.into(),
            exclude: Vec::new(),
            audio: "game".into(),
            audio_device: DEFAULT_DEVICE.into(),
            audio_volume: 100,
            mic: true,
            mic_device: "default".into(),
            mic_volume: 100,
            pinned: false,
            window_pos: None,
            prefs: Default::default(),
            startup_mode: "off".into(),
            hotkey_save: crate::hotkey::DEFAULT_SAVE.into(),
            hotkey_window: crate::hotkey::DEFAULT_WINDOW.into(),
        };
        let mut exclude = DEFAULT_EXCLUDE.to_string();
        let (mut wx, mut wy) = (None, None);
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') || !line.contains('=') {
                continue;
            }
            let (k, v) = line.split_once('=').unwrap();
            let (k, v) = (k.trim(), v.trim());
            let num = |d: u32| v.parse::<u32>().unwrap_or(d);
            let on = matches!(v.to_lowercase().as_str(), "on" | "true" | "1" | "yes");
            match k {
                "ffmpeg" => {
                    if !v.eq_ignore_ascii_case("auto") && !v.is_empty() {
                        c.ffmpeg = PathBuf::from(v)
                    }
                }
                "output_dir" => c.output_dir = PathBuf::from(v),
                "seconds" => c.seconds = num(120).clamp(5, MAX_SECONDS),
                "fps" => c.fps = num(60).clamp(10, 360),
                "height" => c.height = num(1080).clamp(240, 4320),
                "qp" => c.qp = num(21).clamp(1, 51),
                "audio_kbps" => c.audio_kbps = num(192).clamp(64, 320),
                "av_offset_ms" => c.av_offset_ms = v.parse().unwrap_or(0),
                "save_sound" => c.save_sound = v.into(),
                "error_sound" => c.error_sound = v.into(),
                "exclude" => exclude = v.to_string(),
                "audio" => c.audio = v.to_lowercase(),
                "audio_device" => c.audio_device = v.to_string(),
                "audio_volume" => c.audio_volume = num(100).min(200),
                "mic" => c.mic = on,
                "mic_device" => c.mic_device = v.to_string(),
                "mic_volume" => c.mic_volume = num(100).min(200),
                "pinned" => c.pinned = on,
                "window_x" => wx = v.parse().ok(),
                "window_y" => wy = v.parse().ok(),
                "startup_mode" => c.startup_mode = v.to_lowercase(),
                "hotkey_save" => c.hotkey_save = v.to_string(),
                "hotkey_window" => c.hotkey_window = v.to_string(),
                k if k.starts_with("ui_") && PREF_KEYS.contains(&&k[3..]) => {
                    c.prefs.insert(k[3..].to_string(), v.to_string());
                }
                _ => log(format!("config: unknown key '{k}'")),
            }
        }
        if let (Some(x), Some(y)) = (wx, wy) {
            c.window_pos = Some((x, y));
        }
        c.exclude = exclude
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        c
    }
}

/// Write one setting back to clipper.ini, replacing the line or appending it.
pub fn set_value(key: &str, value: &str) {
    let path = exe_dir().join("clipper.ini");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut found = false;
    let mut lines: Vec<String> = text
        .lines()
        .map(|l| {
            let t = l.trim_start();
            if !t.starts_with('#') && t.split_once('=').is_some_and(|(k, _)| k.trim() == key) {
                found = true;
                format!("{key} = {value}")
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        lines.push(format!("{key} = {value}"));
    }
    if let Err(e) = std::fs::write(&path, lines.join("\r\n") + "\r\n") {
        log(format!("could not save {key}: {e}"));
    }
}
