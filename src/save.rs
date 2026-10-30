//! Shift+F3: write the last N seconds to an MP4. The video is already
//! encoded, so this is a stream copy plus a quick AAC encode of the audio.
//! Mic audio is mixed into the same track (one track plays everywhere).

use crate::buffer::{AudioRing, RATE, VideoRing};
use crate::util::{log, stamp, wide};
use crate::video::{adopt, ffmpeg_command_at};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows_core::PCWSTR;

pub fn play(path: &str) {
    let w = wide(path);
    unsafe {
        let _ = PlaySoundW(PCWSTR(w.as_ptr()), None, SND_FILENAME | SND_ASYNC | SND_NODEFAULT);
    }
}

/// Everything a save needs, copied at the moment Shift+F3 was pressed.
pub struct SaveJob {
    pub ffmpeg: PathBuf,
    pub out_dir: PathBuf,
    pub seconds: u32,
    pub fps: u32,
    pub audio_kbps: u32,
    pub av_offset_ms: i64,
    pub save_sound: String,
    pub error_sound: String,
    /// multiplier for the recorded sound
    pub audio_gain: f32,
    /// None when the mic is off
    pub mic_gain: Option<f32>,
    /// file name before the date: the game's name in Game mode, else "Clip"
    pub name: String,
    /// told with WM_SAVED when the save finishes
    pub notify: isize,
    pub notify_msg: u32,
}

/// Result of the last save: (text for the window, success).
pub static LAST: Mutex<Option<(String, bool, PathBuf)>> = Mutex::new(None);

pub fn save_clip(
    job: SaveJob,
    video: Arc<Mutex<VideoRing>>,
    audio: Arc<Mutex<AudioRing>>,
    mic: Arc<Mutex<AudioRing>>,
    busy: Arc<AtomicBool>,
) {
    if busy.swap(true, Ordering::SeqCst) {
        log("save ignored: previous save still running");
        return;
    }
    std::thread::spawn(move || {
        let r = save(&job, &video, &audio, &mic);
        busy.store(false, Ordering::SeqCst);
        match r {
            Ok((path, info)) => {
                log(format!("saved {} {info}", path.display()));
                play(&job.save_sound);
                let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                *LAST.lock().unwrap() = Some((format!("{name}  {info}"), true, path));
            }
            Err(e) => {
                log(format!("save failed: {e}"));
                play(&job.error_sound);
                *LAST.lock().unwrap() = Some((format!("Save failed: {e}"), false, PathBuf::new()));
            }
        }
        unsafe {
            let _ = PostMessageW(Some(HWND(job.notify as *mut _)), job.notify_msg, WPARAM(0), LPARAM(0));
        }
    });
}

fn save(
    job: &SaveJob,
    video: &Mutex<VideoRing>,
    audio: &Mutex<AudioRing>,
    mic: &Mutex<AudioRing>,
) -> Result<(PathBuf, String), String> {
    let (h264, frames, t_start) = video
        .lock()
        .unwrap()
        .take_clip(job.seconds)
        .ok_or("nothing buffered yet")?;
    let audio_frames = (frames as i64 * RATE / job.fps as i64) as usize;
    // + offset = audio plays later, so read it from earlier in the buffer
    let from = t_start - job.av_offset_ms * 10_000;
    let mut pcm = audio.lock().unwrap().extract(from, audio_frames);
    if (job.audio_gain - 1.0).abs() > 0.001 {
        for o in pcm.iter_mut() {
            *o = (*o as f32 * job.audio_gain).clamp(-32768.0, 32767.0) as i16;
        }
    }
    if let Some(gain) = job.mic_gain {
        let m = mic.lock().unwrap().extract(from, audio_frames);
        for (o, s) in pcm.iter_mut().zip(m) {
            *o = (*o as f32 + s as f32 * gain).clamp(-32768.0, 32767.0) as i16;
        }
    }

    let id = format!("{}_{}", std::process::id(), stamp());
    let tmp = crate::util::temp_dir();
    let vpath = tmp.join(format!("clipper_{id}.h264"));
    let apath = tmp.join(format!("clipper_{id}.pcm"));
    std::fs::write(&vpath, &h264).map_err(|e| e.to_string())?;
    let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
    std::fs::write(&apath, &bytes).map_err(|e| e.to_string())?;

    std::fs::create_dir_all(&job.out_dir).map_err(|e| format!("{}: {e}", job.out_dir.display()))?;
    let base = format!("{}_{}", job.name, stamp());
    let mut out = job.out_dir.join(format!("{base}.mp4"));
    let mut n = 2;
    while out.exists() {
        out = job.out_dir.join(format!("{base}_{n}.mp4"));
        n += 1;
    }

    let fps = job.fps.to_string();
    let rate = RATE.to_string();
    let ab = format!("{}k", job.audio_kbps);
    let mut child = ffmpeg_command_at(&job.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(["-framerate", &fps, "-f", "h264", "-i"])
        .arg(&vpath)
        .args(["-f", "s16le", "-ar", &rate, "-ac", "2", "-i"])
        .arg(&apath)
        .args(["-map", "0:v:0", "-map", "1:a:0", "-c:v", "copy", "-c:a", "aac", "-b:a", &ab])
        .args(["-movflags", "+faststart"])
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    adopt(&child);
    let mut err = String::new();
    if let Some(mut s) = child.stderr.take() {
        use std::io::Read;
        let _ = s.read_to_string(&mut err);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&vpath);
    let _ = std::fs::remove_file(&apath);
    if !status.success() || !out.exists() {
        return Err(format!("mux failed: {}", err.trim()));
    }
    let info = format!(
        "({:.0} s, {:.0} MB)",
        frames as f64 / job.fps as f64,
        std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0
    );
    Ok((out, info))
}
