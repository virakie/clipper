//! The trimmer's engine: probe a clip, grab preview frames, plan an export
//! that fits a target file size, and run it.
//!
//! Planning: the size budget is split into audio (kept good: AAC 128-160k,
//! 96k only when the budget is tiny) and video. The video gets the largest
//! resolution, at the clip's own frame rate, whose bits-per-pixel-per-frame
//! still looks clean for game footage; 30 fps is only used when even small
//! resolutions would turn blocky at the full rate.
//!
//! Codecs: AV1 (SVT-AV1, default) or H.264 (x264 two-pass, plays everywhere).
//! Measured 2026-10-30 on the user's own game footage at 2.5 Mbps: AV1 VMAF
//! 72.2 vs x264 54.5, and AV1 at 1.5 Mbps (61.2) still beat x264 at 2.5, at
//! the same encode speed (preset 8). So AV1 needs ~0.6x the bits per pixel.
//! The result is checked against the size limit and redone smaller if over.

use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Below this bits/pixel/frame x264 game footage starts to look blocky.
const BPP_OK: f64 = 0.065;
/// Above this extra bits are hard to see; the rest of the budget is left unused.
const BPP_CAP: f64 = 0.22;
/// Aim a bit under the target: container overhead and rate-control wobble.
const SAFETY: f64 = 0.96;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Codec {
    Av1,
    H264,
}

impl Codec {
    pub fn parse(s: &str) -> Codec {
        if s.eq_ignore_ascii_case("h264") { Codec::H264 } else { Codec::Av1 }
    }
    pub fn key(self) -> &'static str {
        match self {
            Codec::Av1 => "av1",
            Codec::H264 => "h264",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Codec::Av1 => "AV1",
            Codec::H264 => "H.264",
        }
    }
    /// bits AV1 needs for the same look, relative to H.264 (measured above)
    fn efficiency(self) -> f64 {
        match self {
            Codec::Av1 => 0.6,
            Codec::H264 => 1.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Probe {
    pub dur: f64,
    pub w: u32,
    pub h: u32,
    pub fps: f64,
    pub audio: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Quality {
    Great,
    Good,
    Okay,
    Low,
}

impl Quality {
    pub fn label(self) -> &'static str {
        match self {
            Quality::Great => "Great",
            Quality::Good => "Good",
            Quality::Okay => "Okay",
            Quality::Low => "Low",
        }
    }
    /// `bpp` already normalised to H.264-equivalent bits.
    fn of(bpp: f64) -> Quality {
        if bpp >= 0.12 {
            Quality::Great
        } else if bpp >= 0.085 {
            Quality::Good
        } else if bpp >= BPP_OK {
            Quality::Okay
        } else {
            Quality::Low
        }
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub w: u32,
    pub h: u32,
    pub fps: f64,
    pub vkbps: u32,
    pub akbps: u32,
    pub est_mb: f64,
    pub quality: Quality,
    pub codec: Codec,
}

impl Plan {
    pub fn describe(&self) -> String {
        format!(
            "{} {}p  ·  {} fps  ·  {:.1} Mbps  ·  {}k audio  ·  ~{:.1} MB  ·  {}",
            self.codec.label(),
            self.h,
            fmt_fps(self.fps),
            self.vkbps as f64 / 1000.0,
            self.akbps,
            self.est_mb,
            self.quality.label()
        )
    }
}

pub fn fmt_fps(f: f64) -> String {
    if (f - f.round()).abs() < 0.01 { format!("{}", f.round() as u32) } else { format!("{f:.2}") }
}

fn even(v: f64) -> u32 {
    ((v.round() as u32) & !1).max(2)
}

/// Total kbps the target size allows for `dur` seconds (MB = 1,000,000 bytes,
/// the stricter reading, so Discord's limit is never crossed).
fn budget_kbps(target_mb: f64, dur: f64) -> f64 {
    target_mb * 1_000_000.0 * 8.0 / 1000.0 * SAFETY / dur.max(0.1)
}

fn audio_kbps(p: &Probe, budget: f64) -> u32 {
    if !p.audio {
        0
    } else if budget >= 4000.0 {
        160
    } else if budget >= 1200.0 {
        128
    } else {
        96
    }
}

/// Plan for an exact height / fps (used by the menu of alternatives).
pub fn plan_at(p: &Probe, dur: f64, target_mb: f64, h: u32, fps: f64, codec: Codec) -> Plan {
    let budget = budget_kbps(target_mb, dur);
    let akbps = audio_kbps(p, budget);
    let h = h.min(p.h).max(2) & !1;
    let w = even(p.w as f64 * h as f64 / p.h.max(1) as f64);
    let pixels = w as f64 * h as f64 * fps;
    let vmax = (budget - akbps as f64).max(100.0);
    let eff = codec.efficiency();
    let vkbps = vmax.min(BPP_CAP * eff * pixels / 1000.0).max(100.0);
    let bpp = vkbps * 1000.0 / pixels / eff; // H.264-equivalent
    let est_mb = (vkbps + akbps as f64) * 1000.0 / 8.0 * dur / 1_000_000.0 * 1.01;
    Plan { w, h, fps, vkbps: vkbps as u32, akbps, est_mb, quality: Quality::of(bpp), codec }
}

/// Heights worth offering: the clip's own, then standard steps below it.
pub fn heights(p: &Probe) -> Vec<u32> {
    let mut v = vec![p.h & !1];
    v.extend([2160u32, 1440, 1080, 900, 720, 540, 480, 360].into_iter().filter(|h| *h < p.h));
    v
}

/// Frame rates worth offering: the clip's own, and 30 if it is higher.
pub fn rates(p: &Probe) -> Vec<f64> {
    if p.fps > 31.0 { vec![p.fps, 30.0] } else { vec![p.fps] }
}

/// The suggestion: largest clean resolution at the clip's own frame rate;
/// 30 fps only if nothing at the full rate is clean; else the smallest.
pub fn suggest(p: &Probe, dur: f64, target_mb: f64, codec: Codec) -> Plan {
    for fps in rates(p) {
        for h in heights(p) {
            let plan = plan_at(p, dur, target_mb, h, fps, codec);
            if plan.quality != Quality::Low {
                return plan;
            }
        }
    }
    let last_fps = *rates(p).last().unwrap();
    plan_at(p, dur, target_mb, *heights(p).last().unwrap(), last_fps, codec)
}

fn cmd(exe: &Path) -> Command {
    let mut c = Command::new(exe);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

pub fn ffprobe_for(ffmpeg: &Path) -> PathBuf {
    let p = ffmpeg.with_file_name("ffprobe.exe");
    if p.exists() { p } else { PathBuf::from("ffprobe.exe") }
}

pub fn probe(ffmpeg: &Path, file: &Path) -> Result<Probe, String> {
    let out = cmd(&ffprobe_for(ffmpeg))
        .args(["-v", "error", "-show_entries", "stream=codec_type,width,height,avg_frame_rate,r_frame_rate:format=duration", "-of", "default=nw=1"])
        .arg(file)
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (mut dur, mut w, mut h, mut fps, mut audio, mut in_video) = (0.0, 0, 0, 0.0, false, false);
    let rate = |v: &str| -> f64 {
        match v.split_once('/') {
            Some((a, b)) => a.parse::<f64>().unwrap_or(0.0) / b.parse::<f64>().unwrap_or(1.0).max(1.0),
            None => v.parse().unwrap_or(0.0),
        }
    };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "codec_type" => {
                in_video = v == "video" && w == 0;
                audio |= v == "audio";
            }
            "width" if in_video => w = v.parse().unwrap_or(0),
            "height" if in_video => h = v.parse().unwrap_or(0),
            "avg_frame_rate" if in_video && rate(v) > 0.0 => fps = rate(v),
            "r_frame_rate" if in_video && fps == 0.0 => fps = rate(v),
            "duration" => dur = v.parse().unwrap_or(0.0),
            _ => {}
        }
    }
    if w == 0 || h == 0 || dur <= 0.0 {
        return Err("not a video ffmpeg can read".into());
    }
    Ok(Probe { dur, w, h, fps: if fps > 0.0 { fps.min(240.0) } else { 30.0 }, audio })
}

/// One frame at `t` seconds, scaled to fit `max_w` x `max_h`, as BGRA.
pub fn frame(ffmpeg: &Path, file: &Path, t: f64, max_w: u32, max_h: u32) -> Result<(u32, u32, Vec<u8>), String> {
    let vf = format!("scale=w={max_w}:h={max_h}:force_original_aspect_ratio=decrease:flags=bilinear,scale=trunc(iw/2)*2:trunc(ih/2)*2");
    let mut child = cmd(ffmpeg)
        .args(["-v", "error", "-nostdin", "-ss", &format!("{:.3}", t.max(0.0)), "-i"])
        .arg(file)
        .args(["-frames:v", "1", "-vf", &vf, "-f", "image2pipe", "-vcodec", "bmp", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let _ = child.wait();
    bmp_to_bgra(&buf).ok_or_else(|| "no frame".into())
}

/// Minimal BMP reader for ffmpeg's 24-bit bottom-up output.
fn bmp_to_bgra(b: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if b.len() < 54 || &b[0..2] != b"BM" {
        return None;
    }
    let rd = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let off = rd(10) as usize;
    let w = rd(18);
    let hraw = rd(22) as i32;
    let bits = u16::from_le_bytes([b[28], b[29]]);
    let h = hraw.unsigned_abs();
    let bpp = (bits / 8) as usize;
    if bpp < 3 {
        return None;
    }
    let stride = (w as usize * bpp).div_ceil(4) * 4;
    let mut out = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h as usize {
        let src_row = if hraw > 0 { h as usize - 1 - y } else { y };
        let row = off + src_row * stride;
        for x in 0..w as usize {
            let s = row + x * bpp;
            let d = (y * w as usize + x) * 4;
            if s + 2 >= b.len() {
                return None;
            }
            out[d] = b[s];
            out[d + 1] = b[s + 1];
            out[d + 2] = b[s + 2];
            out[d + 3] = 255;
        }
    }
    Some((w, h, out))
}

/// Two-pass x264 export of [start, end) to `dst` (always .mp4). `progress`
/// gets 0..1. If the result overshoots the target it is redone smaller.
pub fn export(
    ffmpeg: &Path,
    src: &Path,
    start: f64,
    end: f64,
    plan: &Plan,
    src_fps: f64,
    target_mb: f64,
    dst: &Path,
    progress: &dyn Fn(f32),
) -> Result<u64, String> {
    let dur = (end - start).max(0.1);
    let limit = (target_mb * 1_000_000.0) as u64;
    let mut vkbps = plan.vkbps as f64;
    let log = crate::util::temp_dir().join(format!("clipper_trim_{}", std::process::id()));
    let passes = if plan.codec == Codec::H264 { 2 } else { 1 };
    for attempt in 0..3 {
        for pass in 1..=passes {
            let mut vf = format!("scale={}:{}:flags=lanczos", plan.w, plan.h);
            if plan.fps < src_fps - 0.5 {
                vf.push_str(&format!(",fps={}", plan.fps));
            }
            vf.push_str(",format=yuv420p");
            let v = format!("{}k", vkbps as u32);
            let maxr = format!("{}k", (vkbps * 1.6) as u32);
            let buf = format!("{}k", (vkbps * 2.0) as u32);
            let mut c = cmd(ffmpeg);
            c.args(["-hide_banner", "-v", "error", "-nostdin", "-y", "-progress", "pipe:1", "-nostats"])
                .args(["-ss", &format!("{start:.3}"), "-t", &format!("{dur:.3}"), "-i"])
                .arg(src)
                .args(["-vf", &vf]);
            if plan.codec == Codec::H264 {
                c.args(["-c:v", "libx264", "-preset", "medium", "-profile:v", "high"])
                    .args(["-b:v", &v, "-maxrate", &maxr, "-bufsize", &buf])
                    .args(["-pass", &pass.to_string(), "-passlogfile"])
                    .arg(&log);
            } else {
                // SVT-AV1 VBR; preset 8 = as fast as x264 two-pass here
                let gop = (plan.fps * 5.0).round().to_string();
                c.args(["-c:v", "libsvtav1", "-preset", "8", "-b:v", &v, "-g", &gop]);
            }
            if pass < passes {
                c.args(["-an", "-f", "null", "NUL"]);
            } else {
                if plan.akbps > 0 {
                    c.args(["-c:a", "aac", "-b:a", &format!("{}k", plan.akbps), "-ac", "2"]);
                } else {
                    c.arg("-an");
                }
                c.args(["-movflags", "+faststart", "-f", "mp4"]).arg(dst);
            }
            let mut child = c.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("ffmpeg: {e}"))?;
            let out = child.stdout.take().unwrap();
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(us) = line.strip_prefix("out_time_us=")
                    && let Ok(us) = us.trim().parse::<f64>()
                {
                    let f = (us / 1e6 / dur).clamp(0.0, 1.0);
                    progress((((pass - 1) as f64 + f) / passes as f64) as f32);
                }
            }
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            let st = child.wait().map_err(|e| e.to_string())?;
            if !st.success() {
                cleanup(&log);
                return Err(format!("ffmpeg failed: {}", err.lines().last().unwrap_or("").trim()));
            }
        }
        let size = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
        if size <= limit || attempt == 2 {
            cleanup(&log);
            progress(1.0);
            return Ok(size);
        }
        // overshot: shrink the video bitrate by the overshoot and redo
        vkbps *= limit as f64 / size as f64 * 0.97;
        crate::util::log(format!("trim: {size} bytes > {limit}, retrying at {} kbps", vkbps as u32));
    }
    unreachable!()
}

fn cleanup(log: &Path) {
    for ext in ["-0.log", "-0.log.mbtree", "-0.log.temp", "-0.log.mbtree.temp"] {
        let _ = std::fs::remove_file(format!("{}{}", log.display(), ext));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ultrawide() -> Probe {
        Probe { dur: 120.0, w: 2580, h: 1080, fps: 60.0, audio: true }
    }

    #[test]
    fn keeps_full_frame_rate_and_good_audio_for_a_typical_clip() {
        // 30 s at 20 MB on an ultrawide 1080p60 recording
        let p = suggest(&ultrawide(), 30.0, 20.0, Codec::H264);
        assert_eq!(p.fps, 60.0);
        assert!(p.akbps >= 128, "{p:?}");
        assert!(p.quality != Quality::Low, "{p:?}");
        assert!(p.h <= 900 && p.h >= 540, "{p:?}");
        assert!(p.est_mb <= 20.0, "{p:?}");
    }

    #[test]
    fn short_clip_stays_full_resolution_and_does_not_waste_bytes() {
        let p = suggest(&ultrawide(), 5.0, 20.0, Codec::H264);
        assert_eq!(p.h, 1080);
        assert_eq!(p.fps, 60.0);
        assert!(p.est_mb < 20.0, "{p:?}");
    }

    #[test]
    fn long_clip_drops_to_30fps_before_going_blocky() {
        let p = suggest(&ultrawide(), 120.0, 20.0, Codec::H264);
        assert!(p.est_mb <= 20.0, "{p:?}");
        assert!(p.akbps >= 96, "{p:?}");
    }

    #[test]
    fn never_upscales() {
        let small = Probe { dur: 10.0, w: 1280, h: 720, fps: 30.0, audio: false };
        let p = suggest(&small, 10.0, 50.0, Codec::H264);
        assert!(p.h <= 720 && p.w <= 1280, "{p:?}");
        assert_eq!(p.akbps, 0);
    }

    #[test]
    fn av1_affords_more_resolution_at_the_same_size() {
        let h264 = suggest(&ultrawide(), 30.0, 20.0, Codec::H264);
        let av1 = suggest(&ultrawide(), 30.0, 20.0, Codec::Av1);
        assert!(av1.h > h264.h || av1.quality as u8 <= h264.quality as u8, "{h264:?} {av1:?}");
        assert!(av1.est_mb <= 20.0, "{av1:?}");
    }

    /// Real export: CLIPPER_TRIM_SAMPLE=<clip> cargo test --release real_export -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_export() {
        let src = PathBuf::from(std::env::var("CLIPPER_TRIM_SAMPLE").expect("set CLIPPER_TRIM_SAMPLE"));
        let ffmpeg = PathBuf::from(std::env::var("USERPROFILE").unwrap()).join(r"scoop\apps\ffmpeg\current\bin\ffmpeg.exe");
        let p = probe(&ffmpeg, &src).unwrap();
        println!("probe: {p:?}");
        let (start, end) = (30.0, 60.0);
        for codec in [Codec::Av1, Codec::H264] {
            let plan = suggest(&p, end - start, 20.0, codec);
            println!("plan: {}", plan.describe());
            let dst = std::env::temp_dir().join(format!("clipper_trim_test_{}.mp4", codec.key()));
            let t = std::time::Instant::now();
            let size = export(&ffmpeg, &src, start, end, &plan, p.fps, 20.0, &dst, &|_| {}).unwrap();
            println!("  -> {:.2} MB in {:.1} s: {}", size as f64 / 1e6, t.elapsed().as_secs_f64(), dst.display());
            assert!(size <= 20_000_000);
        }
        let (w, h, px) = frame(&ffmpeg, &src, 40.0, 432, 200).unwrap();
        println!("frame {w}x{h}, {} bytes", px.len());
    }

    #[test]
    fn reads_bmp() {
        // 2x1 24-bit bottom-up BMP: blue, red
        let mut b = vec![0u8; 54];
        b[0] = b'B';
        b[1] = b'M';
        b[10] = 54;
        b[18] = 2;
        b[22] = 1;
        b[28] = 24;
        b.extend([255, 0, 0, 0, 0, 255, 0, 0]);
        let (w, h, px) = bmp_to_bgra(&b).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(&px[0..4], &[255, 0, 0, 255]);
        assert_eq!(&px[4..8], &[0, 0, 255, 255]);
    }
}
