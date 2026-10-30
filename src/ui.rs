//! The window, FramePacer-style: a small native Win32 window drawn with
//! Direct2D/DirectWrite (no browser engine). Windows 11 look, dark or light to
//! match the system. One toolbar row (menu, compact, pin, mode, clip length,
//! save), two status lines and a live sound/mic level graph. Everything else
//! lives in the menu. Compact view = the toolbar row only.
//!
//! Clicks become the same text commands the command line uses ("mode game",
//! "mic off", ...), queued in QUEUE and handled by the main window.

mod player;
mod trimpanel;

use crate::util::{log, wide};
use std::sync::Arc;
use trimpanel::{Drag, ExportState, FrameWorker, TIMER_PLAY, Trim, WM_TRIM_EXPORT, WM_TRIM_FRAME};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::Mutex;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::*;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_core::{PCSTR, PCWSTR, w};
use windows_numerics::Vector2;

/// Commands from the window, drained by the main window on WM_UI_CMD.
pub static QUEUE: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// What the window shows. Built by the app, pushed with `Ui::update`.
#[derive(Clone, Default)]
pub struct UiState {
    pub capturing: bool,
    pub mode: String,
    pub what: String,
    pub buffer: f64,
    pub seconds: u32,
    pub mb: f64,
    pub audio: String,
    pub device: String,
    pub devices: Vec<String>,
    pub default_device: String,
    pub level: f32,
    pub level_text: String,
    pub mic_on: bool,
    pub mic: String,
    pub mics: Vec<String>,
    pub default_mic: String,
    pub mic_level: f32,
    pub mic_level_text: String,
    pub mic_volume: u32,
    pub volume: u32,
    pub out: String,
    pub pinned: bool,
    pub last: String,
    pub last_ok: bool,
    pub startup: bool,
    pub startup_mode: String,
    pub compact: bool,
    /// chosen output lines, chosen fps, fps the capture actually runs at
    pub height: u32,
    pub fps: u32,
    pub cur_fps: u32,
    /// source (or largest display) width, height, refresh rate
    pub src: (u32, u32, u32),
    /// trimmer: remembered target size (MB) and codec ("av1" / "h264")
    pub trim_mb: f64,
    pub trim_codec: String,
    /// connected displays in mode order (main, second, ...), as "WxH"
    pub monitors: Vec<String>,
    /// global shortcuts ("Shift+F3" / "off") and a registration problem, if any
    pub hotkey_save: String,
    pub hotkey_window: String,
    pub hotkey_error: String,
}

/// not exported by the modules imported here
const WM_MOUSELEAVE: u32 = 0x02A3;

const W: f32 = 500.0; // client width, DIPs
const PAD: f32 = 20.0;
const ROW_H: f32 = 36.0;
const FULL_H: f32 = 262.0;
const COMPACT_H: f32 = ROW_H + 2.0 * PAD;
const HISTORY: usize = 60; // graph samples (250 ms each = 15 s)

fn class() -> PCWSTR {
    if std::env::var_os("CLIPPER_TEST").is_some() { w!("ClipperWindowTest") } else { w!("ClipperWindow") }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    None,
    Menu,
    Compact,
    Sound,
    Mic,
    Mode,
    Save,
    Scissors,
    Last,
    // trimmer
    Drop,
    Timeline,
    TPlayPause,
    TPreview,
    Codec,
    Export,
    Plan,
    TStatus,
}

#[derive(Clone, Copy)]
struct Palette {
    bg: D2D1_COLOR_F,
    button: D2D1_COLOR_F,
    hover: D2D1_COLOR_F,
    lit: D2D1_COLOR_F,
    border: D2D1_COLOR_F,
    text: D2D1_COLOR_F,
    sub: D2D1_COLOR_F,
    panel: D2D1_COLOR_F,
    grid: D2D1_COLOR_F,
    line: D2D1_COLOR_F,
    line2: D2D1_COLOR_F,
    rec: D2D1_COLOR_F,
}

const fn rgb(r: u8, g: u8, b: u8) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
}

const DARK: Palette = Palette {
    bg: rgb(0x20, 0x20, 0x20),
    button: rgb(0x2d, 0x2d, 0x2d),
    hover: rgb(0x37, 0x37, 0x37),
    lit: rgb(0x45, 0x45, 0x45),
    border: rgb(0x3a, 0x3a, 0x3a),
    text: rgb(0xff, 0xff, 0xff),
    sub: rgb(0x9d, 0x9d, 0x9d),
    panel: rgb(0x26, 0x26, 0x26),
    grid: rgb(0x34, 0x34, 0x34),
    line: rgb(0xe6, 0xe6, 0xe6),
    line2: rgb(0x8a, 0x8a, 0x8a),
    rec: rgb(0xC9, 0x36, 0x1B),
};

const LIGHT: Palette = Palette {
    bg: rgb(0xf3, 0xf3, 0xf3),
    button: rgb(0xfb, 0xfb, 0xfb),
    hover: rgb(0xf0, 0xf0, 0xf0),
    lit: rgb(0xe0, 0xe0, 0xe0),
    border: rgb(0xe0, 0xe0, 0xe0),
    text: rgb(0x1b, 0x1b, 0x1b),
    sub: rgb(0x60, 0x60, 0x60),
    panel: rgb(0xfb, 0xfb, 0xfb),
    grid: rgb(0xe8, 0xe8, 0xe8),
    line: rgb(0x2b, 0x2b, 0x2b),
    line2: rgb(0x9a, 0x9a, 0x9a),
    rec: rgb(0xC9, 0x36, 0x1B),
};

fn system_is_dark() -> bool {
    unsafe {
        let mut v = 1u32;
        let mut size = 4u32;
        let ok = RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut _ as *mut _),
            Some(&mut size),
        );
        ok.is_ok() && v == 0
    }
}

/// Let native popup menus follow the system dark mode (uxtheme ordinals
/// 135 SetPreferredAppMode / 136 FlushMenuThemes, what Explorer & co use).
fn allow_dark_menus() {
    unsafe {
        let Ok(ux) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        if let Some(f) = GetProcAddress(ux, PCSTR(135 as *const u8)) {
            let set: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set(1); // AllowDark: follow the system setting
        }
        if let Some(f) = GetProcAddress(ux, PCSTR(136 as *const u8)) {
            let flush: extern "system" fn() = std::mem::transmute(f);
            flush();
        }
    }
}

struct Gfx {
    rt: ID2D1HwndRenderTarget,
    brush: ID2D1SolidColorBrush,
}

struct Inner {
    hwnd: Cell<HWND>,
    owner: HWND,
    state: RefCell<UiState>,
    history: RefCell<VecDeque<(f32, f32)>>,
    hover: Cell<Hit>,
    pressed: Cell<Hit>,
    tracking: Cell<bool>,
    dark: Cell<bool>,
    factory: ID2D1Factory,
    text: IDWriteTextFormat,
    text_c: IDWriteTextFormat,
    small: IDWriteTextFormat,
    icons: IDWriteTextFormat,
    gfx: RefCell<Option<Gfx>>,
    edit: Cell<HWND>,
    edit_brush: Cell<HBRUSH>,
    edit_proc: Cell<isize>,
    flash_until: Cell<u64>,
    last_seen: RefCell<String>,
    small_c: IDWriteTextFormat,
    small_r: IDWriteTextFormat,
    // trimmer
    player: RefCell<Option<player::Player>>,
    ffmpeg: std::path::PathBuf,
    trim: RefCell<Trim>,
    frames: Arc<FrameWorker>,
    export_state: Arc<Mutex<ExportState>>,
    mb_edit: Cell<HWND>,
    drag: Cell<Drag>,
    /// waiting for a new shortcut: "save" / "window", plus a hint when the
    /// pressed combo can't be used
    bind: Cell<Option<&'static str>>,
    bind_msg: RefCell<String>,
}

pub struct Ui {
    pub hwnd: HWND,
    inner: Box<Inner>,
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h }
}

/// Toolbar layout (DIPs): menu, compact, pin | mode, length box, save.
fn layout(hit: Hit) -> D2D_RECT_F {
    let y = PAD;
    match hit {
        Hit::Menu => rect(PAD, y, 40.0, ROW_H),
        Hit::Compact => rect(PAD + 46.0, y, 40.0, ROW_H),
        Hit::Sound => rect(PAD + 92.0, y, 40.0, ROW_H),
        Hit::Mic => rect(PAD + 138.0, y, 40.0, ROW_H),
        Hit::Mode => rect(PAD + 191.0, y, 93.0, ROW_H),
        Hit::Save => rect(W - PAD - 46.0 - 70.0, y, 70.0, ROW_H),
        Hit::Scissors => rect(W - PAD - 40.0, y, 40.0, ROW_H),
        Hit::Last => rect(PAD, PAD + ROW_H + 34.0, W - 2.0 * PAD, 20.0),
        _ => rect(0.0, 0.0, 0.0, 0.0),
    }
}
/// The clip-length edit box sits between Mode and Save.
fn length_rect() -> D2D_RECT_F {
    rect(PAD + 290.0, PAD, 48.0, ROW_H)
}

fn inside(r: &D2D_RECT_F, x: f32, y: f32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn clock(s: f64) -> String {
    let s = s.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn mode_label(m: &str) -> &'static str {
    match m {
        "game" => "Game",
        _ => crate::detect::MONITOR_NAMES.iter().find(|(k, _)| *k == m).map(|(_, n)| *n).unwrap_or("Off"),
    }
}

/// Mode choices for a menu: one per connected display, then Game and Off.
fn mode_items(st: &UiState, verb: &str, cur: &str, game: &str, off: &str) -> Vec<(String, String, bool)> {
    let mut v: Vec<(String, String, bool)> = crate::detect::MONITOR_NAMES
        .iter()
        .zip(st.monitors.iter())
        .map(|((k, n), size)| (format!("{verb} {k}"), format!("{n} monitor\t{size}"), cur == *k))
        .collect();
    v.push((format!("{verb} game"), game.into(), cur == "game"));
    v.push((format!("{verb} off"), off.into(), cur == "off"));
    v
}

/// "Shift+F3" -> ["Shift", "F3"] for keycaps; nothing when off.
fn keys_of(combo: &str) -> Vec<&str> {
    if crate::hotkey::parse(combo).is_some() { combo.split('+').collect() } else { Vec::new() }
}

/// "SteelSeries Sonar - Gaming (SteelSeries Sonar Virtual Audio Device)" ->
/// the part before " (" when that is still unique in the list.
fn short_name(n: &str, all: &[String]) -> String {
    let s = n.split(" (").next().unwrap_or(n);
    if all.iter().filter(|m| m.split(" (").next() == Some(s)).count() == 1 { s.into() } else { n.into() }
}

fn send(owner: HWND, cmd: impl Into<String>) {
    QUEUE.lock().unwrap().push_back(cmd.into());
    unsafe {
        let _ = PostMessageW(Some(owner), crate::WM_UI_CMD, WPARAM(0), LPARAM(0));
    }
}

impl Inner {
    fn pal(&self) -> Palette {
        if self.dark.get() { DARK } else { LIGHT }
    }

    fn scale(&self) -> f32 {
        unsafe { GetDpiForWindow(self.hwnd.get()) as f32 / 96.0 }
    }

    fn ensure_gfx(&self) {
        if self.gfx.borrow().is_some() {
            return;
        }
        unsafe {
            let hwnd = self.hwnd.get();
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let dpi = GetDpiForWindow(hwnd) as f32;
            let props = D2D1_RENDER_TARGET_PROPERTIES { dpiX: dpi, dpiY: dpi, ..Default::default() };
            let hprops = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U { width: rc.right as u32, height: rc.bottom as u32 },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let Ok(rt) = self.factory.CreateHwndRenderTarget(&props, &hprops) else { return };
            rt.SetDpi(dpi, dpi);
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
            let Ok(brush) = rt.CreateSolidColorBrush(&self.pal().text, None) else { return };
            *self.gfx.borrow_mut() = Some(Gfx { rt, brush });
        }
    }

    fn text_at(&self, g: &Gfx, s: &str, fmt: &IDWriteTextFormat, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        let w: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            g.brush.SetColor(&c);
            g.rt.DrawText(&w, fmt, &r, &g.brush, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
        }
    }

    fn button(&self, g: &Gfx, hit: Hit, lit: bool) {
        self.button_lit(g, hit, layout(hit), lit);
    }

    fn button_at(&self, g: &Gfx, hit: Hit, r: D2D_RECT_F) {
        self.button_lit(g, hit, r, false);
    }

    fn button_lit(&self, g: &Gfx, hit: Hit, r: D2D_RECT_F, lit: bool) {
        let p = self.pal();
        let fill = if lit {
            p.lit
        } else if self.pressed.get() == hit {
            p.bg
        } else if self.hover.get() == hit {
            p.hover
        } else {
            p.button
        };
        let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: 5.0, radiusY: 5.0 };
        unsafe {
            g.brush.SetColor(&fill);
            g.rt.FillRoundedRectangle(&rr, &g.brush);
            g.brush.SetColor(&p.border);
            let inner = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F { left: r.left + 0.5, top: r.top + 0.5, right: r.right - 0.5, bottom: r.bottom - 0.5 },
                radiusX: 5.0,
                radiusY: 5.0,
            };
            g.rt.DrawRoundedRectangle(&inner, &g.brush, 1.0, None);
        }
    }

    fn paint(&self) {
        self.ensure_gfx();
        let gfx = self.gfx.borrow();
        let Some(g) = gfx.as_ref() else { return };
        let Ok(st) = self.state.try_borrow() else { return };
        let p = self.pal();
        unsafe {
            g.rt.BeginDraw();
            g.rt.Clear(Some(&p.bg));

            // menu, compact, pin (Segoe Fluent Icons glyphs, like FramePacer)
            self.button(g, Hit::Menu, false);
            self.text_at(g, "\u{E700}", &self.icons, layout(Hit::Menu), p.text);
            // lit while the full view (graph, status) is open
            self.button(g, Hit::Compact, !st.compact);
            self.text_at(g, "\u{ECA5}", &self.icons, layout(Hit::Compact), p.text);
            // desktop sound / mic on-off: a red slash through the icon when off
            let sound_on = st.audio != "off";
            self.button(g, Hit::Sound, false);
            self.text_at(g, "\u{E767}", &self.icons, layout(Hit::Sound), if sound_on { p.text } else { p.sub });
            self.button(g, Hit::Mic, false);
            self.text_at(g, "\u{E720}", &self.icons, layout(Hit::Mic), if st.mic_on { p.text } else { p.sub });
            for (hit, on) in [(Hit::Sound, sound_on), (Hit::Mic, st.mic_on)] {
                if !on {
                    let r = layout(hit);
                    let (cx, cy) = ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0);
                    g.brush.SetColor(&p.rec);
                    g.rt.DrawLine(Vector2 { X: cx - 9.0, Y: cy + 9.0 }, Vector2 { X: cx + 9.0, Y: cy - 9.0 }, &g.brush, 2.0, None);
                }
            }

            // divider
            g.brush.SetColor(&p.border);
            let x = PAD + 184.5;
            g.rt.DrawLine(Vector2 { X: x, Y: PAD + 6.0 }, Vector2 { X: x, Y: PAD + ROW_H - 6.0 }, &g.brush, 1.0, None);

            // mode picker with a recording dot
            self.button(g, Hit::Mode, false);
            let m = layout(Hit::Mode);
            let dot = D2D1_ELLIPSE { point: Vector2 { X: m.left + 16.0, Y: m.top + ROW_H / 2.0 }, radiusX: 5.0, radiusY: 5.0 };
            g.brush.SetColor(&if st.capturing { p.rec } else { p.line2 });
            g.rt.FillEllipse(&dot, &g.brush);
            self.text_at(g, mode_label(&st.mode), &self.text, D2D_RECT_F { left: m.left + 28.0, ..m }, p.text);
            self.text_at(g, "\u{E70D}", &self.small_icon(), D2D_RECT_F { left: m.right - 26.0, ..m }, p.sub);

            // length box frame (the edit control sits inside it)
            let l = length_rect();
            let rr = D2D1_ROUNDED_RECT { rect: l, radiusX: 5.0, radiusY: 5.0 };
            g.brush.SetColor(&p.button);
            g.rt.FillRoundedRectangle(&rr, &g.brush);
            g.brush.SetColor(&p.border);
            g.rt.DrawRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F { left: l.left + 0.5, top: l.top + 0.5, right: l.right - 0.5, bottom: l.bottom - 0.5 },
                    radiusX: 5.0,
                    radiusY: 5.0,
                },
                &g.brush,
                1.0,
                None,
            );

            // save (Clipper red while there is something to save)
            let s = layout(Hit::Save);
            let can = st.buffer >= 1.0;
            let flash = crate::util::now_ms() < self.flash_until.get();
            let fill = if flash {
                rgb(0x2f, 0x9e, 0x44)
            } else if !can {
                p.button
            } else if self.pressed.get() == Hit::Save {
                rgb(0xA8, 0x2C, 0x15)
            } else if self.hover.get() == Hit::Save {
                rgb(0xD9, 0x45, 0x29)
            } else {
                p.rec
            };
            g.brush.SetColor(&fill);
            g.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: s, radiusX: 5.0, radiusY: 5.0 }, &g.brush);
            let fg = if can || flash { rgb(255, 255, 255) } else { p.sub };
            self.text_at(g, if flash { "Saved" } else { "Save" }, &self.text_c, s, fg);
            let trim_open = self.trim.borrow().open;
            self.button(g, Hit::Scissors, trim_open);
            self.text_at(g, "\u{E8C6}", &self.icons, layout(Hit::Scissors), p.text);

            if !st.compact {
                // status lines
                let y1 = PAD + ROW_H + 10.0;
                let status = if st.mode == "off" {
                    "Off: not recording".to_string()
                } else if st.capturing {
                    format!("Recording: {}  ·  {}p{}", st.what, st.height.min(st.src.1.max(1)), st.cur_fps)
                } else {
                    st.what.clone()
                };
                // a shortcut another app owns is worth more than the status line
                let (status, sc) = if st.hotkey_error.is_empty() {
                    (status, p.sub)
                } else {
                    (format!("{} taken by another app (Menu > Shortcuts)", st.hotkey_error), p.rec)
                };
                self.text_at(g, &status, &self.small, rect(PAD, y1, W - 2.0 * PAD, 20.0), sc);
                let last = if st.last.is_empty() {
                    String::new()
                } else if st.last_ok {
                    format!("  |  Last: {}", st.last.split("  ").next().unwrap_or(""))
                } else {
                    format!("  |  {}", st.last)
                };
                let mb = if st.capturing { format!(" ({:.0} MB)", st.mb) } else { String::new() };
                let line2 = format!("Buffer: {} / {}{}{}", clock(st.buffer), clock(st.seconds as f64), mb, last);
                let lc = if self.hover.get() == Hit::Last && st.last_ok { p.text } else { p.sub };
                self.text_at(g, &line2, &self.small, layout(Hit::Last), lc);

                // level graph, FramePacer's frame-time panel turned into sound / mic
                let gr = rect(PAD, PAD + ROW_H + 62.0, W - 2.0 * PAD, FULL_H - PAD - (PAD + ROW_H + 62.0));
                g.brush.SetColor(&p.panel);
                g.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: gr, radiusX: 8.0, radiusY: 8.0 }, &g.brush);
                g.brush.SetColor(&p.grid);
                for i in 1..8 {
                    let x = gr.left + (gr.right - gr.left) * i as f32 / 8.0;
                    g.rt.DrawLine(Vector2 { X: x, Y: gr.top }, Vector2 { X: x, Y: gr.bottom }, &g.brush, 1.0, None);
                }
                for i in 1..4 {
                    let y = gr.top + (gr.bottom - gr.top) * i as f32 / 4.0;
                    g.rt.DrawLine(Vector2 { X: gr.left, Y: y }, Vector2 { X: gr.right, Y: y }, &g.brush, 1.0, None);
                }
                g.brush.SetColor(&p.border);
                g.rt.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: gr, radiusX: 8.0, radiusY: 8.0 }, &g.brush, 1.0, None);
                let hist = self.history.borrow();
                // Normalised: the loudest sample in view sits at 75% height and
                // everything is linear loudness relative to it. The floor
                // (-40 dB) stops near-silence being blown up into noise. The plot
                // starts under the legend so the line never runs through it.
                let amp = |db: f32| if db <= -99.0 { 0.0 } else { 10f32.powf(db / 20.0) };
                let peak = hist.iter().map(|s| amp(s.0).max(amp(s.1))).fold(0.01f32, f32::max);
                let (top, bottom) = (gr.top + 28.0, gr.bottom - 6.0);
                let to_y = |db: f32| {
                    let v = (amp(db) / peak * 0.75).clamp(0.0, 1.0);
                    bottom - v * (bottom - top)
                };
                let step = (gr.right - gr.left) / (HISTORY - 1) as f32;
                let start = HISTORY.saturating_sub(hist.len());
                for (which, color) in [(1usize, p.line2), (0usize, p.line)] {
                    g.brush.SetColor(&color);
                    let mut prev: Option<Vector2> = None;
                    for (i, s) in hist.iter().enumerate() {
                        let db = if which == 0 { s.0 } else { s.1 };
                        let pt = Vector2 { X: gr.left + (start + i) as f32 * step, Y: to_y(db) };
                        if let Some(p0) = prev {
                            g.rt.DrawLine(p0, pt, &g.brush, 1.5, None);
                        }
                        prev = Some(pt);
                    }
                }
                // legend
                let legend = format!("Sound {}   Mic {}", st.level_text, if st.mic_on { st.mic_level_text.as_str() } else { "off" });
                self.text_at(g, &legend, &self.small, rect(gr.left + 10.0, gr.top + 6.0, gr.right - gr.left - 20.0, 18.0), p.sub);
            }
            // toolbar tooltip, drawn after the trimmer so it sits on top
            let tip: Option<(Hit, String, String)> = match self.hover.get() {
                Hit::Menu => Some((Hit::Menu, "Settings".into(), String::new())),
                Hit::Compact => Some((Hit::Compact, if st.compact { "Full view" } else { "Compact view" }.into(), String::new())),
                Hit::Sound => Some((Hit::Sound, if sound_on { "Mute desktop sound" } else { "Record desktop sound" }.into(), String::new())),
                Hit::Mic => Some((Hit::Mic, if st.mic_on { "Mute mic" } else { "Record mic" }.into(), String::new())),
                Hit::Mode => Some((Hit::Mode, "What to record".into(), String::new())),
                Hit::Save => Some((Hit::Save, format!("Save the last {}", clock(st.seconds as f64)), st.hotkey_save.clone())),
                Hit::Scissors => Some((Hit::Scissors, if trim_open { "Close trimmer" } else { "Trim a clip" }.into(), String::new())),
                _ => None,
            };
            drop(st);
            self.paint_trim(g);
            let mut covers_edit = false;
            if self.bind.get().is_none()
                && let Some((hit, label, combo)) = tip
            {
                let r = self.tooltip(g, layout(hit), &label, &keys_of(&combo));
                let l = length_rect();
                covers_edit = r.left < l.right && r.right > l.left && r.top < l.bottom && r.bottom > l.top;
            }
            // the length box is a real child window and would paint over a bubble
            if self.bind.get().is_none() {
                let _ = ShowWindow(self.edit.get(), if covers_edit { SW_HIDE } else { SW_SHOWNA });
            }
            if let Some(which) = self.bind.get() {
                self.paint_bind(g, which);
            }
            let r = g.rt.EndDraw(None, None);
            if r.is_err() {
                drop(gfx);
                *self.gfx.borrow_mut() = None; // device lost: rebuild next paint
            }
        }
    }

    /// Client height in DIPs (main view + trimmer).
    fn client_h(&self) -> f32 {
        (if self.state.borrow().compact { COMPACT_H } else { FULL_H }) + self.trim_height()
    }

    /// Wait for the next key combo as the new `which` shortcut. The current
    /// shortcuts are switched off meanwhile so they can be pressed again.
    fn bind_start(&self, which: &'static str) {
        self.bind.set(Some(which));
        self.bind_msg.borrow_mut().clear();
        send(self.owner, "hotkeys pause");
        unsafe {
            // the text boxes are real child windows: they'd show through the card
            let _ = ShowWindow(self.edit.get(), SW_HIDE);
            let _ = ShowWindow(self.mb_edit.get(), SW_HIDE);
            let _ = SetFocus(Some(self.hwnd.get()));
            let _ = InvalidateRect(Some(self.hwnd.get()), None, false);
        }
    }

    fn bind_end(&self) {
        if self.bind.take().is_some() {
            send(self.owner, "hotkeys resume");
        }
        unsafe {
            let _ = ShowWindow(self.edit.get(), SW_SHOWNA);
            self.trim_place_mb(); // shows the size box again if the trimmer has a clip
            let _ = InvalidateRect(Some(self.hwnd.get()), None, false);
        }
    }

    /// Ctrl / Alt / Shift / Win held right now, as RegisterHotKey flags.
    fn held_mods() -> u32 {
        use crate::hotkey::{ALT, CTRL, SHIFT, WIN};
        let down = |vk: VIRTUAL_KEY| unsafe { GetKeyState(vk.0 as i32) } < 0;
        let mut m = 0;
        if down(VK_CONTROL) { m |= CTRL }
        if down(VK_MENU) { m |= ALT }
        if down(VK_SHIFT) { m |= SHIFT }
        if down(VK_LWIN) || down(VK_RWIN) { m |= WIN }
        m
    }

    /// A key while binding. Modifiers only redraw (the card shows them held).
    fn bind_key(&self, vk: u16) {
        let Some(which) = self.bind.get() else { return };
        let modifier = matches!(vk, 0x10..=0x12 | 0x5B | 0x5C | 0xA0..=0xA5);
        if vk == VK_ESCAPE.0 {
            self.bind_end();
            return;
        }
        if !modifier {
            let mods = Self::held_mods();
            match crate::hotkey::key_name(vk as u32) {
                Some(name) if mods == 0 && !crate::hotkey::bare_ok(vk as u32) => {
                    *self.bind_msg.borrow_mut() = format!("Add Ctrl, Alt or Shift to {name}");
                }
                Some(_) => {
                    let combo = crate::hotkey::format(mods, vk as u32).unwrap_or_default();
                    send(self.owner, format!("hotkey {which} {combo}"));
                    self.bind_end();
                    return;
                }
                None => *self.bind_msg.borrow_mut() = "That key can't be a shortcut".into(),
            }
        }
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd.get()), None, false);
        }
    }

    /// The "press a new shortcut" card over a dimmed window.
    fn paint_bind(&self, g: &Gfx, which: &str) {
        let p = self.pal();
        unsafe {
            g.brush.SetColor(&D2D1_COLOR_F { a: 0.88, ..p.bg });
            g.rt.FillRectangle(&rect(0.0, 0.0, W, self.client_h()), &g.brush);
            let card = rect(PAD, PAD - 10.0, W - 2.0 * PAD, ROW_H + 20.0);
            g.brush.SetColor(&p.panel);
            g.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: card, radiusX: 8.0, radiusY: 8.0 }, &g.brush);
            g.brush.SetColor(&p.rec);
            g.rt.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: card, radiusX: 8.0, radiusY: 8.0 }, &g.brush, 1.0, None);
        }
        let what = if which == "save" { "Save clip" } else { "Show / hide window" };
        self.text_at(g, &format!("Press the new shortcut for {what}"), &self.small, rect(PAD + 14.0, PAD - 4.0, 330.0, 20.0), p.text);
        let msg = self.bind_msg.borrow().clone();
        let mods = crate::hotkey::format(Self::held_mods(), 0x41).map(|s| s[..s.len() - 1].to_string()).unwrap_or_default();
        let sub = if !msg.is_empty() {
            msg
        } else if !mods.is_empty() {
            format!("{mods}...")
        } else {
            "e.g. Shift+F3, Ctrl+Alt+C or F9".into()
        };
        let sc = if self.bind_msg.borrow().is_empty() { p.sub } else { p.rec };
        self.text_at(g, &sub, &self.small, rect(PAD + 14.0, PAD + 16.0, 330.0, 20.0), sc);
        self.text_at(g, "Esc to cancel", &self.small_r, rect(W - PAD - 130.0, PAD + 6.0, 118.0, 20.0), p.sub);
    }

    fn small_icon(&self) -> IDWriteTextFormat {
        // chevron next to the mode name: the icon font at text size
        self.icons.clone()
    }

    fn hit_at(&self, x: f32, y: f32) -> Hit {
        let compact = self.state.borrow().compact;
        for h in [Hit::Menu, Hit::Compact, Hit::Sound, Hit::Mic, Hit::Mode, Hit::Save, Hit::Scissors] {
            if inside(&layout(h), x, y) {
                return h;
            }
        }
        if !compact && inside(&layout(Hit::Last), x, y) {
            return Hit::Last;
        }
        self.trim_hit(x, y)
    }

    fn click(&self, h: Hit) {
        let st = self.state.borrow().clone();
        match h {
            Hit::Compact => send(self.owner, format!("pref view {}", if st.compact { "full" } else { "compact" })),
            Hit::Sound => send(self.owner, format!("audio {}", if st.audio == "off" { "on" } else { "off" })),
            Hit::Mic => send(self.owner, format!("mic {}", if st.mic_on { "off" } else { "on" })),
            Hit::Save => send(self.owner, "save"),
            Hit::Scissors => self.trim_toggle(),
            Hit::Drop | Hit::TPlayPause | Hit::TPreview | Hit::Codec | Hit::Export | Hit::Plan | Hit::TStatus => self.trim_click(h),
            Hit::Last if st.last_ok => send(self.owner, "showlast"),
            Hit::Mode => {
                let items = mode_items(&st, "mode", &st.mode, "Game (auto-detect)", "Off");
                if let Some(cmd) = self.popup(&[Menu::List(items)], layout(Hit::Mode)) {
                    send(self.owner, cmd);
                }
            }
            Hit::Menu => match self.popup(&main_menu(&st), layout(Hit::Menu)) {
                Some(cmd) if cmd.starts_with("ui:bind ") => self.bind_start(if cmd.ends_with("save") { "save" } else { "window" }),
                Some(cmd) if cmd == "ui:resetkeys" => {
                    send(self.owner, format!("hotkey save {}", crate::hotkey::DEFAULT_SAVE));
                    send(self.owner, format!("hotkey window {}", crate::hotkey::DEFAULT_WINDOW));
                }
                Some(cmd) => send(self.owner, cmd),
                None => {}
            },
            _ => {}
        }
    }

    /// Native popup menu under `anchor`; returns the chosen command.
    fn popup(&self, items: &[Menu], anchor: D2D_RECT_F) -> Option<String> {
        unsafe {
            let mut cmds: Vec<String> = Vec::new();
            let m = build_menu(items, &mut cmds);
            let s = self.scale();
            let mut pt = POINT { x: (anchor.left * s) as i32, y: (anchor.bottom * s) as i32 + 2 };
            let _ = ClientToScreen(self.hwnd.get(), &mut pt);
            let id = TrackPopupMenu(m, TPM_RETURNCMD | TPM_NONOTIFY | TPM_LEFTALIGN | TPM_TOPALIGN, pt.x, pt.y, None, self.hwnd.get(), None);
            let _ = DestroyMenu(m);
            let id = id.0 as usize;
            (id > 0 && id <= cmds.len()).then(|| cmds[id - 1].clone())
        }
    }

    fn place_edit(&self) {
        let s = self.scale();
        let l = length_rect();
        unsafe {
            // vertically centre a one-line edit in the box
            let h = (20.0 * s) as i32;
            let _ = SetWindowPos(
                self.edit.get(),
                None,
                ((l.left + 6.0) * s) as i32,
                ((l.top + (ROW_H - 20.0) / 2.0) * s) as i32,
                ((l.right - l.left - 12.0) * s) as i32,
                h,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            let font = CreateFontW(
                -((15.0 * s) as i32), 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, w!("Segoe UI Variable Text"),
            );
            SendMessageW(self.edit.get(), WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        }
    }

    fn sync_edit(&self) {
        let edit = self.edit.get();
        unsafe {
            if GetFocus() == edit {
                return; // don't fight the user's typing
            }
            let secs = self.state.borrow().seconds;
            let t = wide(&clock(secs as f64));
            let _ = SetWindowTextW(edit, PCWSTR(t.as_ptr()));
        }
    }

    fn apply_edit(&self) {
        let mut buf = [0u16; 32];
        let n = unsafe { GetWindowTextW(self.edit.get(), &mut buf) };
        let s = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
        // "2", "2.5" (minutes) or "2:30"
        let secs = if let Some((m, sec)) = s.trim().split_once(':') {
            m.trim().parse::<f64>().ok().zip(sec.trim().parse::<f64>().ok()).map(|(m, s)| m * 60.0 + s)
        } else {
            s.trim().parse::<f64>().ok().map(|m| m * 60.0)
        };
        match secs {
            Some(v) if v >= 5.0 => send(self.owner, format!("seconds {}", v.round() as u32)),
            _ => self.sync_edit(),
        }
    }

    fn resize_for_view(&self) {
        let compact = self.state.borrow().compact;
        let s = self.scale();
        unsafe {
            let hwnd = self.hwnd.get();
            let main_h = if compact { COMPACT_H } else { FULL_H };
            let mut want = RECT { left: 0, top: 0, right: (W * s) as i32, bottom: ((main_h + self.trim_height()) * s) as i32 };
            let style = WINDOW_STYLE(GetWindowLongW(hwnd, GWL_STYLE) as u32);
            let _ = windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi(&mut want, style, false, WINDOW_EX_STYLE(0), GetDpiForWindow(hwnd));
            let mut cur = RECT::default();
            let _ = GetWindowRect(hwnd, &mut cur);
            let (w, h) = (want.right - want.left, want.bottom - want.top);
            if cur.right - cur.left != w || cur.bottom - cur.top != h {
                let _ = SetWindowPos(hwnd, None, 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
                keep_on_screen(hwnd);
            }
        }
    }
}

enum Menu {
    /// command, label, checked
    List(Vec<(String, String, bool)>),
    Sub(String, Vec<Menu>),
    Item(String, String, bool),
    Disabled(String),
    Sep,
}

unsafe fn build_menu(items: &[Menu], cmds: &mut Vec<String>) -> HMENU {
    unsafe {
        let m = CreatePopupMenu().unwrap_or_default();
        let add = |flags: MENU_ITEM_FLAGS, id: usize, text: &str| {
            let t = wide(text);
            let _ = AppendMenuW(m, flags, id, PCWSTR(t.as_ptr()));
        };
        for it in items {
            match it {
                Menu::List(v) => {
                    for (cmd, label, chk) in v {
                        cmds.push(cmd.clone());
                        add(MF_STRING | if *chk { MF_CHECKED } else { MF_UNCHECKED }, cmds.len(), label);
                    }
                }
                Menu::Item(cmd, label, chk) => {
                    cmds.push(cmd.clone());
                    add(MF_STRING | if *chk { MF_CHECKED } else { MF_UNCHECKED }, cmds.len(), label);
                }
                Menu::Sub(label, sub) => {
                    let s = build_menu(sub, cmds);
                    add(MF_POPUP, s.0 as usize, label);
                }
                Menu::Disabled(label) => add(MF_STRING | MF_GRAYED, 0, label),
                Menu::Sep => add(MF_SEPARATOR, 0, ""),
            }
        }
        m
    }
}

fn main_menu(st: &UiState) -> Vec<Menu> {
    let lens: Vec<(String, String, bool)> = [(30, "30 seconds"), (60, "1 minute"), (120, "2 minutes"), (180, "3 minutes"), (300, "5 minutes"), (600, "10 minutes")]
        .iter()
        .map(|(s, l)| (format!("seconds {s}"), l.to_string(), st.seconds == *s))
        .collect();
    let vols = |verb: &str, cur: u32| -> Vec<(String, String, bool)> {
        [25u32, 50, 75, 100, 125, 150, 200].iter().map(|v| (format!("{verb} {v}"), format!("{v}%"), cur == *v)).collect()
    };
    let devs = |verb: &str, list: &[String], cur: &str, def: &str| -> Vec<(String, String, bool)> {
        let mut v = vec![(format!("{verb} default"), format!("Windows default ({})", short_name(def, list)), cur == "default")];
        v.extend(list.iter().map(|d| (format!("{verb} {d}"), short_name(d, list), cur == d)));
        v
    };
    let starts = mode_items(st, "startupmode", &st.startup_mode, "Game", "Off (tray only)");
    let key_label = |k: &str| if crate::hotkey::parse(k).is_some() { k.to_string() } else { "none".into() };
    // resolution: standard heights up to the source's own (plus its native one)
    let (sw, sh, refresh) = st.src;
    let mut heights: Vec<u32> = [2160u32, 1440, 1080, 900, 720, 480].into_iter().filter(|h| *h <= sh).collect();
    if sh > 0 && !heights.contains(&sh) {
        heights.insert(0, sh);
    }
    let eff_h = st.height.min(sh.max(1));
    let res: Vec<(String, String, bool)> = heights
        .iter()
        .map(|&h| {
            let w = (sw as u64 * h as u64 / sh.max(1) as u64) as u32 & !1;
            let native = if h == sh { "  (native)" } else { "" };
            (format!("height {h}"), format!("{h}p   {w}x{h}{native}"), h == eff_h)
        })
        .collect();
    // frame rate: up to the display's refresh rate
    let max_fps = if refresh >= 10 { refresh } else { 240 };
    let mut rates: Vec<u32> = [24u32, 30, 48, 60, 90, 120, 144, 165, 240].into_iter().filter(|f| *f <= max_fps).collect();
    if refresh >= 10 && !rates.contains(&refresh) {
        rates.push(refresh);
    }
    let fps: Vec<(String, String, bool)> = rates
        .iter()
        .map(|&f| {
            let tag = if f == refresh { "  (display)" } else { "" };
            // the chosen rate, as capped by this display
            (format!("fps {f}"), format!("{f} fps{tag}"), f == st.fps.min(max_fps))
        })
        .collect();
    vec![
        Menu::Sub("Resolution".into(), vec![Menu::List(res)]),
        Menu::Sub("Frame rate".into(), vec![Menu::List(fps)]),
        Menu::Sub(
            "Clip length".into(),
            vec![Menu::List(lens), Menu::Sep, Menu::Disabled("Custom: type it in the box (e.g. 4:30)".into())],
        ),
        Menu::Sep,
        Menu::Sub(
            "Sound".into(),
            vec![
                Menu::List(vec![
                    ("audio game".into(), "Game's own sound only (Game mode)".into(), st.audio == "game"),
                    ("audio device".into(), "Everything on the device below".into(), st.audio == "device"),
                    ("audio off".into(), "No sound".into(), st.audio == "off"),
                ]),
                Menu::Sep,
                Menu::List(devs("device", &st.devices, &st.device, &st.default_device)),
            ],
        ),
        Menu::Sub("Sound volume".into(), vec![Menu::List(vols("volume", st.volume))]),
        Menu::Sub(
            "Mic".into(),
            vec![
                Menu::Item(format!("mic {}", if st.mic_on { "off" } else { "on" }), "Record the mic".into(), st.mic_on),
                Menu::Sep,
                Menu::List(devs("micdevice", &st.mics, &st.mic, &st.default_mic)),
            ],
        ),
        Menu::Sub("Mic volume".into(), vec![Menu::List(vols("micvolume", st.mic_volume))]),
        Menu::Sep,
        Menu::Sub(
            "Save folder".into(),
            vec![
                Menu::Disabled(st.out.clone()),
                Menu::Item("browse".into(), "Change...".into(), false),
                Menu::Item("openfolder".into(), "Open".into(), false),
            ],
        ),
        Menu::Sep,
        Menu::Sub(
            "Shortcuts".into(),
            vec![
                Menu::Item("ui:bind save".into(), format!("Save clip...\t{}", key_label(&st.hotkey_save)), false),
                Menu::Item("ui:bind window".into(), format!("Show / hide this window...\t{}", key_label(&st.hotkey_window)), false),
                Menu::Sep,
                Menu::Item("ui:resetkeys".into(), format!("Reset ({} / {})", crate::hotkey::DEFAULT_SAVE, crate::hotkey::DEFAULT_WINDOW), false),
            ],
        ),
        Menu::Item(format!("startup {}", if st.startup { "off" } else { "on" }), "Start with Windows".into(), st.startup),
        Menu::Sub("Start in".into(), vec![Menu::List(starts)]),
        Menu::Sep,
        Menu::Item(format!("pin {}", if st.pinned { "off" } else { "on" }), "Keep on top".into(), st.pinned),
        Menu::Item("hide".into(), "Hide to tray".into(), false),
        Menu::Item("quit".into(), "Quit Clipper".into(), false),
    ]
}

unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let parent = GetParent(hwnd).unwrap_or_default();
        let inner = GetWindowLongPtrW(parent, GWLP_USERDATA) as *const Inner;
        if inner.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let inner = &*inner;
        if msg == WM_KEYDOWN && (wp.0 as u16 == VK_RETURN.0 || wp.0 as u16 == VK_ESCAPE.0) {
            if wp.0 as u16 == VK_RETURN.0 {
                if hwnd == inner.mb_edit.get() { inner.apply_mb() } else { inner.apply_edit() }
            }
            let _ = SetFocus(Some(parent)); // leaving the box re-syncs it
            return LRESULT(0);
        }
        if msg == WM_CHAR && (wp.0 == 13 || wp.0 == 27) {
            return LRESULT(0); // no beep
        }
        let prev: WNDPROC = std::mem::transmute(inner.edit_proc.get());
        CallWindowProcW(prev, hwnd, msg, wp, lp)
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Inner;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let ui = &*ptr;
        let s = ui.scale();
        let (mx, my) = ((lp.0 & 0xffff) as i16 as f32 / s, ((lp.0 >> 16) & 0xffff) as i16 as f32 / s);
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                ui.paint();
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SIZE => {
                if let Some(g) = ui.gfx.borrow().as_ref() {
                    let _ = g.rt.Resize(&D2D_SIZE_U { width: (lp.0 & 0xffff) as u32, height: ((lp.0 >> 16) & 0xffff) as u32 });
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                // test copies get posted moves while the real cursor is elsewhere: no leave tracking there
                if !ui.tracking.get() && std::env::var_os("CLIPPER_TEST").is_none() {
                    let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    let _ = TrackMouseEvent(&mut t);
                    ui.tracking.set(true);
                }
                if ui.drag.get() != Drag::None {
                    ui.timeline_move(mx);
                    return LRESULT(0);
                }
                let h = ui.hit_at(mx, my);
                if h != ui.hover.get() {
                    ui.hover.set(h);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                ui.tracking.set(false);
                ui.hover.set(Hit::None);
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_LBUTTONDOWN if ui.bind.get().is_some() => {
                ui.bind_end(); // a click cancels
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let _ = SetFocus(Some(hwnd)); // leave the length box
                let h = ui.hit_at(mx, my);
                if h == Hit::Timeline {
                    SetCapture(hwnd);
                    ui.timeline_down(mx);
                    return LRESULT(0);
                }
                ui.pressed.set(h);
                if h != Hit::None {
                    SetCapture(hwnd);
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let _ = ReleaseCapture();
                if ui.drag.get() != Drag::None {
                    ui.timeline_up();
                    return LRESULT(0);
                }
                let was = ui.pressed.replace(Hit::None);
                let _ = InvalidateRect(Some(hwnd), None, false);
                if was != Hit::None && ui.hit_at(mx, my) == was {
                    ui.click(was);
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                let _ = ScreenToClient(hwnd, &mut p);
                let h = ui.hit_at(p.x as f32 / s, p.y as f32 / s);
                if (lp.0 & 0xffff) as u32 == HTCLIENT && h != Hit::None {
                    let _ = SetCursor(LoadCursorW(None, IDC_HAND).ok());
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_COMMAND => {
                // the length box lost focus: apply what was typed
                if (wp.0 >> 16) as u32 == EN_KILLFOCUS {
                    if HWND(lp.0 as *mut _) == ui.mb_edit.get() { ui.apply_mb() } else { ui.apply_edit() }
                }
                LRESULT(0)
            }
            WM_CTLCOLOREDIT => {
                let p = ui.pal();
                let c = |f: f32| (f * 255.0) as u32;
                let bg = COLORREF(c(p.button.r) | c(p.button.g) << 8 | c(p.button.b) << 16);
                let fg = COLORREF(c(p.text.r) | c(p.text.g) << 8 | c(p.text.b) << 16);
                let dc = HDC(wp.0 as *mut _);
                SetTextColor(dc, fg);
                SetBkColor(dc, bg);
                if ui.edit_brush.get().is_invalid() {
                    ui.edit_brush.set(CreateSolidBrush(bg));
                }
                LRESULT(ui.edit_brush.get().0 as isize)
            }
            // waiting for a new shortcut: every key (Alt combos arrive as SYS*)
            // is ours, and nothing reaches the system menu or the trimmer
            WM_KEYDOWN | WM_SYSKEYDOWN if ui.bind.get().is_some() => {
                ui.bind_key(wp.0 as u16);
                LRESULT(0)
            }
            WM_KEYUP | WM_SYSKEYUP | WM_SYSCHAR | WM_CHAR if ui.bind.get().is_some() => {
                let _ = InvalidateRect(Some(hwnd), None, false); // held modifiers changed
                LRESULT(0)
            }
            // switching away cancels (test copies are driven while in the background)
            WM_ACTIVATE if (wp.0 & 0xffff) as u32 == WA_INACTIVE && ui.bind.get().is_some() && std::env::var_os("CLIPPER_TEST").is_none() => {
                ui.bind_end();
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_KEYDOWN => {
                if ui.trim_key(wp.0 as u16) {
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_DROPFILES => {
                let drop = windows::Win32::UI::Shell::HDROP(wp.0 as *mut _);
                let mut buf = [0u16; 1024];
                let n = windows::Win32::UI::Shell::DragQueryFileW(drop, 0, Some(&mut buf));
                windows::Win32::UI::Shell::DragFinish(drop);
                if n > 0 {
                    ui.trim_load(std::path::PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])));
                }
                LRESULT(0)
            }
            WM_TIMER if wp.0 == TIMER_PLAY => {
                ui.tick_play();
                LRESULT(0)
            }
            WM_TRIM_FRAME => {
                ui.frame_arrived();
                LRESULT(0)
            }
            WM_TRIM_EXPORT => {
                ui.export_update();
                LRESULT(0)
            }
            WM_SYSCOMMAND if (wp.0 & 0xfff0) as u32 == SC_MINIMIZE => {
                send(ui.owner, "hide"); // minimise to tray, like FramePacer
                LRESULT(0)
            }
            WM_CLOSE => {
                send(ui.owner, "hide");
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let r = &*(lp.0 as *const RECT);
                *ui.gfx.borrow_mut() = None;
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                ui.place_edit();
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                let dark = system_is_dark();
                if dark != ui.dark.get() {
                    ui.dark.set(dark);
                    apply_frame_theme(hwnd, dark);
                    let old = ui.edit_brush.replace(HBRUSH::default());
                    if !old.is_invalid() {
                        let _ = DeleteObject(old.into());
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    let _ = InvalidateRect(Some(ui.edit.get()), None, true);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Slide the window fully inside its monitor's work area (above the taskbar).
/// An old saved position, a resolution change or growing out of compact view
/// can otherwise leave part of it off screen.
fn keep_on_screen(hwnd: HWND) {
    unsafe {
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_err() {
            return;
        }
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return;
        }
        let wa = mi.rcWork;
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let x = r.left.min(wa.right - w).max(wa.left);
        let y = r.top.min(wa.bottom - h).max(wa.top);
        if x != r.left || y != r.top {
            let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
}

fn apply_frame_theme(hwnd: HWND, dark: bool) {
    unsafe {
        let v: i32 = dark as i32;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &v as *const _ as *const _, 4);
    }
}

impl Ui {
    /// Creates the window (hidden). `owner` is Clipper's hidden main window.
    pub fn create(owner: HWND, pos: Option<(i32, i32)>, pinned: bool, ffmpeg: std::path::PathBuf) -> Option<Ui> {
        unsafe {
            allow_dark_menus();
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).ok()?;
            let fmt = |family: PCWSTR, size: f32, center: bool| -> Option<IDWriteTextFormat> {
                let f = dw
                    .CreateTextFormat(family, None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!("en-us"))
                    .ok()?;
                let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
                let _ = f.SetTextAlignment(if center { DWRITE_TEXT_ALIGNMENT_CENTER } else { DWRITE_TEXT_ALIGNMENT_LEADING });
                let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
                Some(f)
            };
            let text = fmt(w!("Segoe UI Variable Text"), 15.0, false)?;
            let text_c = fmt(w!("Segoe UI Variable Text"), 15.0, true)?;
            let small = fmt(w!("Segoe UI Variable Text"), 13.5, false)?;
            let small_c = fmt(w!("Segoe UI Variable Text"), 13.5, true)?;
            let small_r = fmt(w!("Segoe UI Variable Text"), 13.5, false)?;
            let _ = small_r.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING);
            let icons = fmt(w!("Segoe Fluent Icons"), 15.0, true)?;

            static REGISTERED: std::sync::Once = std::sync::Once::new();
            let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
            REGISTERED.call_once(|| {
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(proc),
                    hInstance: hinst,
                    lpszClassName: class(),
                    hIcon: LoadIconW(Some(hinst), PCWSTR(1 as _)).unwrap_or_default(),
                    hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                    ..Default::default()
                };
                RegisterClassW(&wc);
            });
            let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
            let ex = if pinned { WS_EX_TOPMOST } else { WINDOW_EX_STYLE(0) };
            let (x, y) = pos
                .filter(|&(x, y)| !MonitorFromPoint(POINT { x: x + 40, y: y + 20 }, MONITOR_DEFAULTTONULL).is_invalid())
                .unwrap_or((CW_USEDEFAULT, CW_USEDEFAULT));
            let hwnd = CreateWindowExW(ex, class(), w!("Clipper"), style, x, y, 480, 320, None, None, Some(hinst), None).ok()?;
            let dark = system_is_dark();
            apply_frame_theme(hwnd, dark);
            let inner = Box::new(Inner {
                hwnd: Cell::new(hwnd),
                owner,
                state: RefCell::new(UiState { compact: false, ..Default::default() }),
                history: RefCell::new(VecDeque::new()),
                hover: Cell::new(Hit::None),
                pressed: Cell::new(Hit::None),
                tracking: Cell::new(false),
                dark: Cell::new(dark),
                factory,
                text,
                text_c,
                small,
                icons,
                gfx: RefCell::new(None),
                edit: Cell::new(HWND::default()),
                edit_brush: Cell::new(HBRUSH::default()),
                edit_proc: Cell::new(0),
                flash_until: Cell::new(0),
                last_seen: RefCell::new(String::new()),
                small_c,
                small_r,
                player: RefCell::new(None),
                frames: FrameWorker::start(ffmpeg.clone()),
                ffmpeg,
                trim: RefCell::new(Trim::default()),
                export_state: Arc::new(Mutex::new(ExportState::default())),
                mb_edit: Cell::new(HWND::default()),
                drag: Cell::new(Drag::None),
                bind: Cell::new(None),
                bind_msg: RefCell::new(String::new()),
            });
            inner.frames.set_hwnd(hwnd);
            windows::Win32::UI::Shell::DragAcceptFiles(hwnd, true);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*inner as *const Inner as isize);
            let edit = CreateWindowExW(
                WINDOW_EX_STYLE(0), w!("EDIT"), w!(""), WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_CENTER as u32 | ES_AUTOHSCROLL as u32),
                0, 0, 10, 10, Some(hwnd), None, Some(hinst), None,
            )
            .ok()?;
            inner.edit.set(edit);
            inner.edit_proc.set(SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as usize as isize));
            inner.place_edit();
            // trimmer target-size box (shown only with a clip loaded)
            let mb = CreateWindowExW(
                WINDOW_EX_STYLE(0), w!("EDIT"), w!("20"), WS_CHILD | WINDOW_STYLE(ES_CENTER as u32 | ES_AUTOHSCROLL as u32),
                0, 0, 10, 10, Some(hwnd), None, Some(hinst), None,
            )
            .ok()?;
            SetWindowLongPtrW(mb, GWLP_WNDPROC, edit_proc as *const () as usize as isize);
            SendMessageW(mb, WM_SETFONT, Some(WPARAM(SendMessageW(edit, WM_GETFONT, None, None).0 as usize)), Some(LPARAM(1)));
            inner.mb_edit.set(mb);
            inner.trim_place_mb();
            inner.resize_for_view();
            log("ui: native window created");
            Some(Ui { hwnd, inner })
        }
    }

    pub fn update(&self, s: UiState) {
        let i = &self.inner;
        // saved a new clip: flash the button green for a moment
        if s.last_ok && !s.last.is_empty() && *i.last_seen.borrow() != s.last {
            if !i.last_seen.borrow().is_empty() {
                i.flash_until.set(crate::util::now_ms() + 1200);
            }
            *i.last_seen.borrow_mut() = s.last.clone();
        }
        {
            let mut h = i.history.borrow_mut();
            let lv = |live: bool, db: f32| if live { db } else { -100.0 };
            h.push_back((lv(s.level > -99.0, s.level), lv(s.mic_on && s.mic_level > -99.0, s.mic_level)));
            while h.len() > HISTORY {
                h.pop_front();
            }
        }
        i.trim_load_prefs(&s);
        let compact_changed = i.state.borrow().compact != s.compact;
        *i.state.borrow_mut() = s;
        if compact_changed {
            i.resize_for_view();
            i.trim_place_mb();
        }
        i.sync_edit();
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// Open the trimmer with a clip (command line `--trim FILE`).
    pub fn trim_file(&self, f: std::path::PathBuf) {
        self.inner.trim_load(f);
    }

    /// Wait for a new "save" / "window" shortcut (the menu's Shortcuts items).
    pub fn bind(&self, which: &str) {
        self.inner.bind_start(if which == "window" { "window" } else { "save" });
    }

    pub fn show(&self) {
        unsafe {
            keep_on_screen(self.hwnd);
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    pub fn visible(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd).as_bool() }
    }

    pub fn set_pinned(&self, on: bool) {
        unsafe {
            let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
            let _ = SetWindowPos(self.hwnd, Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    /// None if the window is already gone.
    pub fn position(&self) -> Option<(i32, i32)> {
        let mut r = RECT::default();
        unsafe { GetWindowRect(self.hwnd, &mut r) }.ok()?;
        Some((r.left, r.top))
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
            let b = self.inner.edit_brush.get();
            if !b.is_invalid() {
                let _ = DeleteObject(b.into());
            }
        }
    }
}
