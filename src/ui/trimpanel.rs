//! The trimmer panel under the main window (the scissors button).
//!
//! Drop a clip on the window (or click the drop zone to browse). The preview
//! box plays the clip itself (Space), scrubbing shows single frames, I / O set
//! in and out at the playhead, P plays just the selection. One compact row
//! holds the size (default 20 MB), codec, a short plan summary and Export;
//! the summary's dropdown has the details and lets you override the plan.
//! Export asks where to save, always .mp4.

use super::player::Player;
use super::*;
use crate::trim::{self, Codec, Plan, Probe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Condvar};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::*;

pub(super) const WM_TRIM_FRAME: u32 = WM_APP + 20;
pub(super) const WM_TRIM_EXPORT: u32 = WM_APP + 21;
pub(super) const TIMER_PLAY: usize = 7;

const PREVIEW_H: f32 = 210.0;
pub(super) const TRIM_EMPTY_H: f32 = 12.0 + 120.0 + PAD;
pub(super) const TRIM_H: f32 = 360.0 + PAD;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Drag {
    None,
    Head,
    In,
    Out,
}

#[derive(Default)]
pub(super) struct Trim {
    pub open: bool,
    pub file: Option<PathBuf>,
    pub probe: Option<Probe>,
    pub in_t: f64,
    pub out_t: f64,
    pub head: f64,
    pub target_mb: f64,
    pub codec: Option<Codec>,
    /// user override of (height, fps); None = the planner's suggestion
    pub force: Option<(u32, f64)>,
    pub plan: Option<Plan>,
    pub frame: Option<(u32, u32, Vec<u8>)>,
    pub bitmap: Option<ID2D1Bitmap>,
    /// shown only after an export (or when something went wrong)
    pub status: String,
    pub saved: Option<PathBuf>,
    pub exporting: bool,
    pub progress: f32,
    pub prefs_loaded: bool,
}

/// Latest-wins frame grabber: scrubbing fires many requests, only the newest
/// is decoded, so the preview never lags behind the mouse.
pub(super) struct FrameWorker {
    req: Mutex<Option<(PathBuf, f64, u32, u32)>>,
    cv: Condvar,
    out: Mutex<Option<(u32, u32, Vec<u8>)>>,
    hwnd: AtomicIsize,
}

impl FrameWorker {
    pub fn start(ffmpeg: PathBuf) -> Arc<FrameWorker> {
        let fw = Arc::new(FrameWorker { req: Mutex::new(None), cv: Condvar::new(), out: Mutex::new(None), hwnd: AtomicIsize::new(0) });
        let w = fw.clone();
        std::thread::spawn(move || loop {
            let job = {
                let mut r = w.req.lock().unwrap();
                while r.is_none() {
                    r = w.cv.wait(r).unwrap();
                }
                r.take().unwrap()
            };
            if let Ok(f) = trim::frame(&ffmpeg, &job.0, job.1, job.2, job.3) {
                *w.out.lock().unwrap() = Some(f);
                unsafe {
                    let _ = PostMessageW(Some(HWND(w.hwnd.load(Ordering::SeqCst) as *mut _)), WM_TRIM_FRAME, WPARAM(0), LPARAM(0));
                }
            }
        });
        fw
    }
    pub fn set_hwnd(&self, h: HWND) {
        self.hwnd.store(h.0 as isize, Ordering::SeqCst);
    }
    fn request(&self, file: &Path, t: f64, w: u32, h: u32) {
        *self.req.lock().unwrap() = Some((file.to_path_buf(), t, w, h));
        self.cv.notify_one();
    }
    pub fn take(&self) -> Option<(u32, u32, Vec<u8>)> {
        self.out.lock().unwrap().take()
    }
}

/// Export progress / result shared with the export thread.
#[derive(Default)]
pub(super) struct ExportState {
    pub progress: f32,
    pub done: Option<Result<(PathBuf, u64), String>>,
}

/// Trim panel layout (DIPs), relative to its top edge `y0`.
pub(super) fn trim_layout(hit: Hit, y0: f32) -> D2D_RECT_F {
    let ty = y0 + 12.0 + PREVIEW_H + 10.0; // timeline row
    let ay = ty + 30.0 + 32.0; // action row
    match hit {
        Hit::Drop => rect(PAD, y0 + 12.0, W - 2.0 * PAD, 120.0),
        Hit::Timeline => rect(PAD, ty, W - 2.0 * PAD - 92.0, 30.0),
        Hit::TPlayPause => rect(W - PAD - 86.0, ty, 40.0, 30.0),
        Hit::TPreview => rect(W - PAD - 40.0, ty, 40.0, 30.0),
        Hit::Codec => rect(PAD + 82.0, ay, 60.0, 34.0),
        Hit::Plan => rect(PAD + 148.0, ay, W - 2.0 * PAD - 148.0 - 87.0, 34.0),
        Hit::Export => rect(W - PAD - 81.0, ay, 81.0, 34.0),
        Hit::TStatus => rect(PAD, ay + 42.0, W - 2.0 * PAD, 20.0),
        _ => rect(0.0, 0.0, 0.0, 0.0),
    }
}
fn preview_rect(y0: f32) -> D2D_RECT_F {
    rect(PAD, y0 + 12.0, W - 2.0 * PAD, PREVIEW_H)
}
pub(super) fn mb_rect(y0: f32) -> D2D_RECT_F {
    let c = trim_layout(Hit::Codec, y0);
    rect(PAD, c.top, 48.0, 34.0)
}

fn tclock(t: f64) -> String {
    let t = t.max(0.0);
    format!("{}:{:05.2}", (t / 60.0) as u64, t % 60.0)
}

impl Inner {
    /// Top of the trim panel: under the full window or the compact toolbar.
    pub(super) fn trim_y0(&self) -> f32 {
        if self.state.borrow().compact { COMPACT_H } else { FULL_H }
    }

    pub(super) fn trim_height(&self) -> f32 {
        let t = self.trim.borrow();
        if !t.open {
            0.0
        } else if t.probe.is_some() {
            TRIM_H
        } else {
            TRIM_EMPTY_H
        }
    }

    pub(super) fn trim_hit(&self, x: f32, y: f32) -> Hit {
        let t = self.trim.borrow();
        if !t.open {
            return Hit::None;
        }
        let y0 = self.trim_y0();
        let hits: &[Hit] = if t.probe.is_some() {
            &[Hit::Timeline, Hit::TPlayPause, Hit::TPreview, Hit::Codec, Hit::Plan, Hit::Export, Hit::TStatus]
        } else {
            &[Hit::Drop]
        };
        for &h in hits {
            if inside(&trim_layout(h, y0), x, y) {
                if h == Hit::TStatus && t.saved.is_none() {
                    continue;
                }
                return h;
            }
        }
        Hit::None
    }

    /// Prefs (target size, codec) arrive with the first state push.
    pub(super) fn trim_load_prefs(&self, st: &UiState) {
        let mut t = self.trim.borrow_mut();
        if !t.prefs_loaded {
            t.prefs_loaded = true;
            t.target_mb = if st.trim_mb > 0.0 { st.trim_mb } else { 20.0 };
            t.codec = Some(Codec::parse(&st.trim_codec));
        }
    }

    pub(super) fn trim_toggle(&self) {
        self.stop_play();
        {
            let mut t = self.trim.borrow_mut();
            t.open = !t.open;
        }
        self.trim_place_mb();
        self.resize_for_view();
    }

    pub(super) fn trim_load(&self, file: PathBuf) {
        self.stop_play();
        match trim::probe(&self.ffmpeg, &file) {
            Ok(p) => {
                let mut t = self.trim.borrow_mut();
                t.open = true;
                t.in_t = 0.0;
                t.out_t = p.dur;
                t.head = 0.0;
                t.force = None;
                t.frame = None;
                t.bitmap = None;
                t.saved = None;
                t.status.clear();
                t.probe = Some(p);
                t.file = Some(file);
                drop(t);
                self.replan();
                self.request_frame();
            }
            Err(e) => {
                let mut t = self.trim.borrow_mut();
                t.open = true;
                t.status = format!("Can't open that file: {e}");
            }
        }
        self.trim_place_mb();
        self.resize_for_view();
        self.invalidate();
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd.get()), None, false);
        }
    }

    pub(super) fn replan(&self) {
        let mut t = self.trim.borrow_mut();
        let Some(p) = t.probe.clone() else { return };
        let dur = (t.out_t - t.in_t).max(0.1);
        let codec = t.codec.unwrap_or(Codec::Av1);
        t.plan = Some(match t.force {
            Some((h, fps)) => trim::plan_at(&p, dur, t.target_mb, h, fps, codec),
            None => trim::suggest(&p, dur, t.target_mb, codec),
        });
    }

    /// Pixel size of the preview box, and the clip fitted inside it.
    fn preview_size(&self) -> (u32, u32) {
        let s = self.scale();
        let r = preview_rect(self.trim_y0());
        let (bw, bh) = ((r.right - r.left) * s, (r.bottom - r.top) * s);
        let t = self.trim.borrow();
        let Some(p) = &t.probe else { return (bw as u32, bh as u32) };
        let k = (bw / p.w as f32).min(bh / p.h as f32).min(1.0);
        (((p.w as f32 * k) as u32 & !1).max(2), ((p.h as f32 * k) as u32 & !1).max(2))
    }

    pub(super) fn request_frame(&self) {
        let (head, file) = {
            let t = self.trim.borrow();
            (t.head, t.file.clone())
        };
        let Some(file) = file else { return };
        let (w, h) = self.preview_size();
        self.frames.request(&file, head, w, h);
    }

    pub(super) fn frame_arrived(&self) {
        if self.player.borrow().is_some() {
            return; // playback owns the picture
        }
        if let Some(f) = self.frames.take() {
            let mut t = self.trim.borrow_mut();
            t.frame = Some(f);
            t.bitmap = None; // rebuilt on the next paint
            drop(t);
            self.invalidate();
        }
    }

    // ---------- playback ----------

    fn playing(&self) -> bool {
        self.player.borrow().is_some()
    }

    fn start_play(&self, from: f64, to: f64) {
        self.stop_play();
        let (file, fps, audio) = {
            let t = self.trim.borrow();
            let Some(p) = &t.probe else { return };
            let Some(f) = t.file.clone() else { return };
            (f, p.fps, p.audio)
        };
        let (w, h) = self.preview_size();
        let pl = Player::start(&self.ffmpeg, &file, from, to, w, h, fps, audio);
        *self.player.borrow_mut() = Some(pl);
        unsafe {
            SetTimer(Some(self.hwnd.get()), TIMER_PLAY, 15, None);
        }
        self.invalidate();
    }

    pub(super) fn stop_play(&self) {
        if let Some(pl) = self.player.borrow_mut().take() {
            crate::util::log(format!("player: stopped at {:.3} s (started {:.3})", pl.clock(), pl.start));
            pl.stop();
            unsafe {
                let _ = KillTimer(Some(self.hwnd.get()), TIMER_PLAY);
            }
            self.invalidate();
        }
    }

    /// Space / the play button: from the playhead to the end of the clip.
    fn play_pause(&self) {
        if self.playing() {
            self.stop_play();
            return;
        }
        let (head, dur) = {
            let t = self.trim.borrow();
            (t.head, t.probe.as_ref().map(|p| p.dur).unwrap_or(0.0))
        };
        let from = if head >= dur - 0.05 { 0.0 } else { head };
        self.start_play(from, dur);
    }

    /// P / the selection button: just in -> out.
    fn play_selection(&self) {
        let (a, b) = {
            let t = self.trim.borrow();
            (t.in_t, t.out_t)
        };
        self.start_play(a, b);
    }

    /// Timer while playing: show the due frame, move the playhead.
    pub(super) fn tick_play(&self) {
        let (frame, clock, done) = {
            let p = self.player.borrow();
            let Some(pl) = p.as_ref() else { return };
            (pl.take_due(), pl.clock(), pl.finished())
        };
        {
            let mut t = self.trim.borrow_mut();
            t.head = clock.min(t.probe.as_ref().map(|p| p.dur).unwrap_or(clock));
            if let Some(f) = frame {
                t.frame = Some((f.w, f.h, f.px));
                t.bitmap = None;
            }
        }
        if done {
            self.stop_play();
        }
        self.invalidate();
    }

    // ---------- timeline ----------

    fn time_at(&self, x: f32) -> f64 {
        let t = self.trim.borrow();
        let dur = t.probe.as_ref().map(|p| p.dur).unwrap_or(0.0);
        let r = trim_layout(Hit::Timeline, self.trim_y0());
        (((x - r.left) / (r.right - r.left)) as f64 * dur).clamp(0.0, dur)
    }

    fn x_at(&self, time: f64) -> f32 {
        let t = self.trim.borrow();
        let dur = t.probe.as_ref().map(|p| p.dur).unwrap_or(1.0).max(0.001);
        let r = trim_layout(Hit::Timeline, self.trim_y0());
        r.left + (time / dur) as f32 * (r.right - r.left)
    }

    /// Mouse down on the timeline: grab the nearest handle, else scrub.
    pub(super) fn timeline_down(&self, x: f32) {
        self.stop_play();
        let (xi, xo) = {
            let t = self.trim.borrow();
            (self.x_at(t.in_t), self.x_at(t.out_t))
        };
        let d = if (x - xi).abs() <= 8.0 && (x - xi).abs() <= (x - xo).abs() {
            Drag::In
        } else if (x - xo).abs() <= 8.0 {
            Drag::Out
        } else {
            Drag::Head
        };
        self.drag.set(d);
        self.timeline_move(x);
    }

    pub(super) fn timeline_move(&self, x: f32) {
        let time = self.time_at(x);
        {
            let mut t = self.trim.borrow_mut();
            match self.drag.get() {
                Drag::In => t.in_t = time.min(t.out_t - 0.2).max(0.0),
                Drag::Out => t.out_t = time.max(t.in_t + 0.2),
                Drag::Head | Drag::None => {}
            }
            t.head = match self.drag.get() {
                Drag::In => t.in_t,
                Drag::Out => t.out_t,
                _ => time,
            };
        }
        self.request_frame();
        self.invalidate();
    }

    pub(super) fn timeline_up(&self) {
        if matches!(self.drag.get(), Drag::In | Drag::Out) {
            self.replan();
        }
        self.drag.set(Drag::None);
        self.invalidate();
    }

    fn set_in_out(&self, out: bool) {
        {
            let mut t = self.trim.borrow_mut();
            let h = t.head;
            if out {
                t.out_t = h.max(t.in_t + 0.2);
            } else {
                t.in_t = h.min(t.out_t - 0.2).max(0.0);
            }
        }
        self.replan();
        self.invalidate();
    }

    /// Space play/pause, P play selection, I / O set in / out at the
    /// playhead, arrows step one frame (Shift = one second) for exact cuts.
    pub(super) fn trim_key(&self, vk: u16) -> bool {
        if self.trim.borrow().probe.is_none() || !self.trim.borrow().open {
            return false;
        }
        let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
        match vk {
            0x20 => self.play_pause(),
            0x50 => self.play_selection(),  // P
            0x49 => self.set_in_out(false), // I
            0x4F => self.set_in_out(true),  // O
            v if v == VK_LEFT.0 || v == VK_RIGHT.0 => {
                self.stop_play();
                {
                    let mut t = self.trim.borrow_mut();
                    let fps = t.probe.as_ref().map(|p| p.fps).unwrap_or(60.0);
                    let step = if shift { 1.0 } else { 1.0 / fps };
                    let dur = t.probe.as_ref().map(|p| p.dur).unwrap_or(0.0);
                    t.head = (t.head + if v == VK_LEFT.0 { -step } else { step }).clamp(0.0, dur);
                }
                self.request_frame();
                self.invalidate();
            }
            _ => return false,
        }
        true
    }

    pub(super) fn trim_click(&self, h: Hit) {
        match h {
            Hit::Drop => {
                let start = self.state.borrow().out.clone();
                if let Some(f) = open_dialog(self.hwnd.get(), &start) {
                    self.trim_load(f);
                }
            }
            Hit::TPlayPause => self.play_pause(),
            Hit::TPreview => self.play_selection(),
            Hit::Codec => {
                let next = if self.trim.borrow().codec == Some(Codec::H264) { Codec::Av1 } else { Codec::H264 };
                self.trim.borrow_mut().codec = Some(next);
                send(self.owner, format!("pref trim_codec {}", next.key()));
                self.replan();
                self.invalidate();
            }
            Hit::Plan => self.plan_menu(),
            Hit::Export => self.export(),
            Hit::TStatus => {
                if let Some(p) = self.trim.borrow().saved.clone() {
                    let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{}", p.display())).spawn();
                }
            }
            _ => {}
        }
    }

    /// The details dropdown: what will be exported, the source, and every
    /// resolution x frame rate the clip allows (with the look of each).
    fn plan_menu(&self) {
        let items = {
            let t = self.trim.borrow();
            let Some(p) = t.probe.clone() else { return };
            let Some(cur) = t.plan.clone() else { return };
            let dur = (t.out_t - t.in_t).max(0.1);
            let codec = t.codec.unwrap_or(Codec::Av1);
            let auto = trim::suggest(&p, dur, t.target_mb, codec);
            let name = t.file.as_ref().and_then(|f| f.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let mut opts = vec![(
                "auto".to_string(),
                format!("Recommended: {}p{}  ({})", auto.h, trim::fmt_fps(auto.fps), auto.quality.label()),
                t.force.is_none(),
            )];
            for fps in trim::rates(&p) {
                for h in trim::heights(&p) {
                    let pl = trim::plan_at(&p, dur, t.target_mb, h, fps, codec);
                    opts.push((
                        format!("{h} {fps}"),
                        format!("{}p{}   {:.1} Mbps   {}", pl.h, trim::fmt_fps(fps), pl.vkbps as f64 / 1000.0, pl.quality.label()),
                        t.force == Some((h, fps)),
                    ));
                }
            }
            vec![
                Menu::Disabled(format!(
                    "Export: {} {}x{} · {} fps · {:.1} Mbps video · {}k audio · ~{:.1} MB",
                    cur.codec.label(),
                    cur.w,
                    cur.h,
                    trim::fmt_fps(cur.fps),
                    cur.vkbps as f64 / 1000.0,
                    cur.akbps,
                    cur.est_mb
                )),
                Menu::Disabled(format!("Source: {name} · {}x{} · {} fps · {}", p.w, p.h, trim::fmt_fps(p.fps), tclock(p.dur))),
                Menu::Sep,
                Menu::List(opts),
            ]
        };
        let anchor = trim_layout(Hit::Plan, self.trim_y0());
        if let Some(cmd) = self.popup(&items, anchor) {
            {
                let mut t = self.trim.borrow_mut();
                t.force = if cmd == "auto" {
                    None
                } else {
                    let mut it = cmd.split(' ');
                    it.next().and_then(|h| h.parse().ok()).zip(it.next().and_then(|f| f.parse().ok()))
                };
            }
            self.replan();
            self.invalidate();
        }
    }

    fn export(&self) {
        self.stop_play();
        let (file, plan, probe, a, b, target) = {
            let t = self.trim.borrow();
            if t.exporting {
                return;
            }
            let (Some(f), Some(pl), Some(p)) = (t.file.clone(), t.plan.clone(), t.probe.clone()) else { return };
            (f, pl, p, t.in_t, t.out_t, t.target_mb)
        };
        let stem = file.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "clip".into());
        let dir = file.parent().map(|d| d.to_path_buf()).unwrap_or_default();
        // mpv's trim naming: TRIMMED_<name> (not TRIMMED_TRIMMED_ on a re-trim)
        let stem = stem.strip_prefix("TRIMMED_").unwrap_or(&stem);
        let Some(dst) = save_dialog(self.hwnd.get(), &dir, &format!("TRIMMED_{stem}.mp4")) else { return };
        {
            let mut t = self.trim.borrow_mut();
            t.exporting = true;
            t.progress = 0.0;
            t.saved = None;
            t.status = format!("Exporting {}...", dst.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
        }
        crate::util::log(format!("trim: exporting {:.2}-{:.2} s as {}", a, b, plan.describe()));
        let shared = self.export_state.clone();
        *shared.lock().unwrap() = ExportState::default();
        let hwnd = self.hwnd.get().0 as isize;
        let ffmpeg = self.ffmpeg.clone();
        std::thread::spawn(move || {
            let post = || unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_TRIM_EXPORT, WPARAM(0), LPARAM(0));
            };
            let progress = |f: f32| {
                shared.lock().unwrap().progress = f;
                post();
            };
            let r = trim::export(&ffmpeg, &file, a, b, &plan, probe.fps, target, &dst, &progress);
            crate::util::log(format!("trim export {} -> {:?}", dst.display(), r.as_ref().map(|s| *s)));
            shared.lock().unwrap().done = Some(r.map(|s| (dst, s)));
            post();
        });
        self.invalidate();
    }

    pub(super) fn export_update(&self) {
        let (progress, done) = {
            let mut s = self.export_state.lock().unwrap();
            (s.progress, s.done.take())
        };
        let mut t = self.trim.borrow_mut();
        t.progress = progress;
        if let Some(r) = done {
            t.exporting = false;
            match r {
                Ok((path, size)) => {
                    t.status = format!("Saved {}  ({:.1} MB)  ·  click to show", path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), size as f64 / 1e6);
                    t.saved = Some(path);
                    crate::save::play(r"C:\Windows\Media\Speech On.wav");
                }
                Err(e) => t.status = format!("Export failed: {e}"),
            }
        }
        drop(t);
        self.invalidate();
    }

    pub(super) fn trim_place_mb(&self) {
        let (show, mb) = {
            let t = self.trim.borrow();
            (t.open && t.probe.is_some(), t.target_mb)
        };
        let e = self.mb_edit.get();
        let s = self.scale();
        let r = mb_rect(self.trim_y0());
        unsafe {
            let _ = SetWindowPos(
                e,
                None,
                ((r.left + 5.0) * s) as i32,
                ((r.top + (34.0 - 20.0) / 2.0) * s) as i32,
                ((r.right - r.left - 10.0) * s) as i32,
                (20.0 * s) as i32,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            let _ = ShowWindow(e, if show { SW_SHOWNA } else { SW_HIDE });
            if GetFocus() != e {
                let t = wide(&trim::fmt_fps(mb));
                let _ = SetWindowTextW(e, PCWSTR(t.as_ptr()));
            }
        }
    }

    pub(super) fn apply_mb(&self) {
        let mut buf = [0u16; 32];
        let n = unsafe { GetWindowTextW(self.mb_edit.get(), &mut buf) };
        let s = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
        if let Ok(v) = s.trim().trim_end_matches(|c: char| c.is_alphabetic()).trim().parse::<f64>()
            && v >= 1.0
        {
            self.trim.borrow_mut().target_mb = v;
            send(self.owner, format!("pref trim_mb {}", trim::fmt_fps(v)));
            self.replan();
        }
        self.trim_place_mb();
        self.invalidate();
    }

    /// Draw the panel. Called inside BeginDraw / EndDraw.
    pub(super) unsafe fn paint_trim(&self, g: &Gfx) {
        let p = self.pal();
        let y0 = self.trim_y0();
        let playing = self.playing();
        let mut t = self.trim.borrow_mut();
        if !t.open {
            return;
        }
        unsafe {
            g.brush.SetColor(&p.border);
            g.rt.DrawLine(Vector2 { X: PAD, Y: y0 }, Vector2 { X: W - PAD, Y: y0 }, &g.brush, 1.0, None);

            if t.probe.is_none() {
                let r = trim_layout(Hit::Drop, y0);
                g.brush.SetColor(&if self.hover.get() == Hit::Drop { p.hover } else { p.panel });
                g.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 8.0, radiusY: 8.0 }, &g.brush);
                g.brush.SetColor(&p.border);
                g.rt.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 8.0, radiusY: 8.0 }, &g.brush, 1.0, None);
                let msg = if t.status.is_empty() { "Drop a clip here, or click to choose one".to_string() } else { t.status.clone() };
                self.text_at(g, "\u{E8C6}", &self.icons, rect(r.left, r.top + 26.0, r.right - r.left, 30.0), p.sub);
                self.text_at(g, &msg, &self.text_c, rect(r.left, r.top + 58.0, r.right - r.left, 30.0), p.sub);
                return;
            }

            // preview: playback or the scrubbed frame, letterboxed
            let pr = preview_rect(y0);
            g.brush.SetColor(&rgb(0, 0, 0));
            g.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: pr, radiusX: 6.0, radiusY: 6.0 }, &g.brush);
            if t.bitmap.is_none()
                && let Some((w, h, px)) = &t.frame
            {
                let props = D2D1_BITMAP_PROPERTIES {
                    pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
                    dpiX: 96.0,
                    dpiY: 96.0,
                };
                t.bitmap = g.rt.CreateBitmap(D2D_SIZE_U { width: *w, height: *h }, Some(px.as_ptr() as *const _), w * 4, &props).ok();
            }
            if let (Some(bmp), Some((w, h, _))) = (&t.bitmap, &t.frame) {
                let s = self.scale();
                let (bw, bh) = (*w as f32 / s, *h as f32 / s);
                let k = ((pr.right - pr.left) / bw).min((pr.bottom - pr.top) / bh);
                let (dw, dh) = (bw * k, bh * k);
                let dx = pr.left + (pr.right - pr.left - dw) / 2.0;
                let dy = pr.top + (pr.bottom - pr.top - dh) / 2.0;
                g.rt.DrawBitmap(bmp, Some(&rect(dx, dy, dw, dh)), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
            }

            // timeline: track, kept range, handles, playhead
            let dur = t.probe.as_ref().map(|p| p.dur).unwrap_or(1.0).max(0.001);
            let tl = trim_layout(Hit::Timeline, y0);
            let xf = |time: f64| tl.left + (time / dur) as f32 * (tl.right - tl.left);
            g.brush.SetColor(&p.panel);
            g.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: tl, radiusX: 5.0, radiusY: 5.0 }, &g.brush);
            g.brush.SetColor(&p.border);
            g.rt.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: tl, radiusX: 5.0, radiusY: 5.0 }, &g.brush, 1.0, None);
            let (xi, xo) = (xf(t.in_t), xf(t.out_t));
            g.brush.SetColor(&D2D1_COLOR_F { a: 0.35, ..p.rec });
            g.rt.FillRectangle(&rect(xi, tl.top + 1.0, xo - xi, tl.bottom - tl.top - 2.0), &g.brush);
            g.brush.SetColor(&p.rec);
            for x in [xi, xo] {
                g.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: rect(x - 3.0, tl.top - 3.0, 6.0, tl.bottom - tl.top + 6.0), radiusX: 2.0, radiusY: 2.0 }, &g.brush);
            }
            let xh = xf(t.head);
            g.brush.SetColor(&p.text);
            g.rt.DrawLine(Vector2 { X: xh, Y: tl.top - 4.0 }, Vector2 { X: xh, Y: tl.bottom + 4.0 }, &g.brush, 2.0, None);

            // play / pause and play-selection, at the end of the timeline
            let pp = trim_layout(Hit::TPlayPause, y0);
            self.button_at(g, Hit::TPlayPause, pp);
            self.text_at(g, if playing { "\u{E769}" } else { "\u{E768}" }, &self.icons, pp, p.text);
            let ps = trim_layout(Hit::TPreview, y0);
            self.button_at(g, Hit::TPreview, ps);
            g.brush.SetColor(&p.rec);
            let (cx, cy) = ((ps.left + ps.right) / 2.0, (ps.top + ps.bottom) / 2.0);
            for x in [cx - 11.0, cx + 9.0] {
                g.rt.FillRectangle(&rect(x, cy - 8.0, 2.0, 16.0), &g.brush);
            }
            self.text_at(g, "\u{E768}", &self.icons, ps, p.text);

            // one quiet line: times on the left, keys on the right
            let info_y = tl.bottom + 7.0;
            let times = format!("{} \u{2192} {}   ({})", clock(t.in_t), clock(t.out_t), clock(t.out_t - t.in_t));
            self.text_at(g, &times, &self.small, rect(PAD, info_y, 200.0, 18.0), p.sub);
            // right side: "Mark in [I]   Mark out [O]", keys drawn as keycaps
            let mut x = W - PAD;
            for (label, key, lw) in [("Mark out", "O", 56.0f32), ("Mark in", "I", 48.0f32)] {
                x -= 18.0;
                self.keycap(g, key, rect(x, info_y, 18.0, 18.0));
                x -= lw + 5.0;
                self.text_at(g, label, &self.small_r, rect(x, info_y, lw, 18.0), p.sub);
                x -= 16.0;
            }

            // size box + MB, codec, plan summary, export
            let mr = mb_rect(y0);
            g.brush.SetColor(&p.button);
            g.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: mr, radiusX: 5.0, radiusY: 5.0 }, &g.brush);
            g.brush.SetColor(&p.border);
            g.rt.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: mr, radiusX: 5.0, radiusY: 5.0 }, &g.brush, 1.0, None);
            self.text_at(g, "MB", &self.small, rect(mr.right + 5.0, mr.top, 26.0, 34.0), p.sub);
            let codec = t.codec.unwrap_or(Codec::Av1);
            self.button_at(g, Hit::Codec, trim_layout(Hit::Codec, y0));
            self.text_at(g, codec.label(), &self.small_c, trim_layout(Hit::Codec, y0), p.text);
            if let Some(plan) = &t.plan {
                let r = trim_layout(Hit::Plan, y0);
                self.button_at(g, Hit::Plan, r);
                let summary = format!("{}p{}  ·  {}", plan.h, trim::fmt_fps(plan.fps), plan.quality.label());
                self.text_at(g, &summary, &self.small, D2D_RECT_F { left: r.left + 12.0, ..r }, p.text);
                self.text_at(g, "\u{E70D}", &self.icons, D2D_RECT_F { left: r.right - 28.0, ..r }, p.sub);
            }
            let er = trim_layout(Hit::Export, y0);
            let fill = if t.exporting {
                p.button
            } else if self.pressed.get() == Hit::Export {
                rgb(0xA8, 0x2C, 0x15)
            } else if self.hover.get() == Hit::Export {
                rgb(0xD9, 0x45, 0x29)
            } else {
                p.rec
            };
            g.brush.SetColor(&fill);
            g.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: er, radiusX: 5.0, radiusY: 5.0 }, &g.brush);
            if t.exporting {
                g.brush.SetColor(&p.rec);
                g.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: rect(er.left, er.top, (er.right - er.left) * t.progress, er.bottom - er.top), radiusX: 5.0, radiusY: 5.0 }, &g.brush);
            }
            let label = if t.exporting { format!("{:.0}%", t.progress * 100.0) } else { "Export".into() };
            self.text_at(g, &label, &self.text_c, er, rgb(255, 255, 255));

            // status: only after an export, or on errors
            if !t.status.is_empty() {
                let sc = if t.saved.is_some() && self.hover.get() == Hit::TStatus { p.text } else { p.sub };
                self.text_at(g, &t.status, &self.small, trim_layout(Hit::TStatus, y0), sc);
            }

            // hover tooltips (drawn last so they sit on top)
            match self.hover.get() {
                Hit::TPlayPause => {
                    self.tooltip(g, pp, if playing { "Pause" } else { "Play" }, &["Space"]);
                }
                Hit::TPreview => {
                    self.tooltip(g, ps, "Play selection", &["P"]);
                }
                Hit::Timeline if self.drag.get() == Drag::None => {
                    self.tooltip(g, rect(tl.right - 1.0, tl.top, 1.0, 1.0), "Step one frame", &["\u{2190}", "\u{2192}"]);
                }
                _ => {}
            }
        }
    }
}

impl Inner {
    /// A key letter in a small rounded box.
    unsafe fn keycap(&self, g: &Gfx, key: &str, r: D2D_RECT_F) {
        let p = self.pal();
        unsafe {
            g.brush.SetColor(&p.button);
            g.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 3.0, radiusY: 3.0 }, &g.brush);
            g.brush.SetColor(&p.line2);
            let inner = D2D_RECT_F { left: r.left + 0.5, top: r.top + 0.5, right: r.right - 0.5, bottom: r.bottom - 0.5 };
            g.rt.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: inner, radiusX: 3.0, radiusY: 3.0 }, &g.brush, 1.0, None);
        }
        self.text_at(g, key, &self.small_c, r, p.text);
    }

    /// "Label [Key]" bubble above `anchor` (below it when there's no room,
    /// beside it in the one-row compact window), kept inside the window.
    pub(super) unsafe fn tooltip(&self, g: &Gfx, anchor: D2D_RECT_F, label: &str, keys: &[&str]) -> D2D_RECT_F {
        let p = self.pal();
        let caps: Vec<f32> = keys.iter().map(|k| (k.chars().count() as f32 * 8.0 + 10.0).max(18.0)).collect();
        let lw = label.chars().count() as f32 * 6.8 + 6.0;
        let w = 10.0 + lw + caps.iter().map(|c| c + 4.0).sum::<f32>() + 8.0;
        let h = 28.0;
        let mut x = (anchor.right - w).max(PAD).min(W - PAD - w);
        let mut y = anchor.top - h - 6.0;
        if y < 2.0 {
            y = anchor.bottom + 6.0;
        }
        if y + h > self.client_h() - 2.0 {
            // no room above or below: sit level with the anchor, to its side
            y = (anchor.top + anchor.bottom - h) / 2.0;
            x = if anchor.left - w - 6.0 >= 4.0 { anchor.left - w - 6.0 } else { anchor.right + 6.0 };
        }
        let r = rect(x, y, w, h);
        unsafe {
            g.brush.SetColor(&p.lit);
            g.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 5.0, radiusY: 5.0 }, &g.brush);
            g.brush.SetColor(&p.border);
            g.rt.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 5.0, radiusY: 5.0 }, &g.brush, 1.0, None);
        }
        self.text_at(g, label, &self.small, rect(x + 10.0, y, lw, h), p.text);
        let mut kx = x + 10.0 + lw;
        for (k, cw) in keys.iter().zip(caps) {
            unsafe { self.keycap(g, k, rect(kx, y + 5.0, cw, 18.0)) };
            kx += cw + 4.0;
        }
        r
    }
}

fn open_dialog(owner: HWND, start: &str) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let filters = [
            COMDLG_FILTERSPEC { pszName: w!("Videos"), pszSpec: w!("*.mp4;*.mkv;*.webm;*.mov;*.avi;*.m4v") },
            COMDLG_FILTERSPEC { pszName: w!("All files"), pszSpec: w!("*.*") },
        ];
        let _ = dlg.SetFileTypes(&filters);
        let _ = dlg.SetTitle(w!("Choose a clip to trim"));
        let s = wide(start);
        if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(s.as_ptr()), None) {
            let _ = dlg.SetFolder(&item);
        }
        dlg.Show(Some(owner)).ok()?;
        let p = dlg.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        path.map(PathBuf::from)
    }
}

/// Always .mp4: the filter and default extension force it.
fn save_dialog(owner: HWND, dir: &Path, name: &str) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let filters = [COMDLG_FILTERSPEC { pszName: w!("MP4 video"), pszSpec: w!("*.mp4") }];
        let _ = dlg.SetFileTypes(&filters);
        let _ = dlg.SetDefaultExtension(w!("mp4"));
        let _ = dlg.SetTitle(w!("Export trimmed clip"));
        let n = wide(name);
        let _ = dlg.SetFileName(PCWSTR(n.as_ptr()));
        let d = wide(&dir.to_string_lossy());
        if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(d.as_ptr()), None) {
            let _ = dlg.SetFolder(&item);
        }
        dlg.Show(Some(owner)).ok()?;
        let p = dlg.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        let mut path = PathBuf::from(path?);
        if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4")) {
            path.set_extension("mp4");
        }
        Some(path)
    }
}
