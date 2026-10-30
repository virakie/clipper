//! In-window playback for the trimmer's preview box.
//!
//! Two ffmpeg children decode from `start`: one scaled BGRA frames for the box,
//! one 48 kHz stereo PCM played through waveOut. The audio position is the
//! clock (falls back to wall time when the clip has no sound); the UI timer
//! shows the newest frame whose time has come. Pausing stops everything;
//! playing again restarts at the playhead (ffmpeg seeks fast on these clips,
//! keyframe every second).

use std::collections::VecDeque;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::{MMTIME, TIME_SAMPLES};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const RATE: u32 = 48_000;
const QUEUE_MAX: usize = 24;

pub struct Frame {
    pub t: f64,
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

pub struct Player {
    pub start: f64,
    pub end: f64,
    stop: Arc<AtomicBool>,
    frames: Arc<Mutex<VecDeque<Frame>>>,
    video_done: Arc<AtomicBool>,
    wave: Arc<AtomicIsize>,
    audio: bool,
    began: Instant,
    children: Arc<Mutex<Vec<Child>>>,
}

fn cmd(exe: &Path) -> Command {
    let mut c = Command::new(exe);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

impl Player {
    /// Play `file` from `start` to `end` (seconds) at `w` x `h` and `fps`.
    pub fn start(ffmpeg: &Path, file: &Path, start: f64, end: f64, w: u32, h: u32, fps: f64, audio: bool) -> Player {
        let stop = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(Mutex::new(VecDeque::new()));
        let video_done = Arc::new(AtomicBool::new(false));
        let wave = Arc::new(AtomicIsize::new(0));
        let children = Arc::new(Mutex::new(Vec::new()));
        let len = (end - start).max(0.05);
        let fps = fps.clamp(1.0, 60.0);

        // video
        let vf = format!("scale={w}:{h}:flags=bilinear,fps={fps}");
        if let Ok(mut c) = cmd(ffmpeg)
            .args(["-v", "error", "-nostdin", "-ss", &format!("{start:.3}"), "-t", &format!("{len:.3}"), "-i"])
            .arg(file)
            .args(["-an", "-vf", &vf, "-f", "rawvideo", "-pix_fmt", "bgra", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            let mut out = c.stdout.take().unwrap();
            children.lock().unwrap().push(c);
            let (stop2, frames2, done2) = (stop.clone(), frames.clone(), video_done.clone());
            std::thread::spawn(move || {
                let size = (w * h * 4) as usize;
                let mut i = 0u64;
                loop {
                    if stop2.load(Ordering::SeqCst) {
                        break;
                    }
                    if frames2.lock().unwrap().len() >= QUEUE_MAX {
                        std::thread::sleep(std::time::Duration::from_millis(4));
                        continue;
                    }
                    let mut px = vec![0u8; size];
                    if out.read_exact(&mut px).is_err() {
                        break;
                    }
                    frames2.lock().unwrap().push_back(Frame { t: start + i as f64 / fps, w, h, px });
                    i += 1;
                }
                done2.store(true, Ordering::SeqCst);
            });
        } else {
            video_done.store(true, Ordering::SeqCst);
        }

        // audio
        let mut has_audio = false;
        if audio
            && let Ok(mut c) = cmd(ffmpeg)
                .args(["-v", "error", "-nostdin", "-ss", &format!("{start:.3}"), "-t", &format!("{len:.3}"), "-i"])
                .arg(file)
                .args(["-vn", "-f", "s16le", "-ar", "48000", "-ac", "2", "-"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
        {
            let out = c.stdout.take().unwrap();
            children.lock().unwrap().push(c);
            has_audio = true;
            let (stop2, wave2) = (stop.clone(), wave.clone());
            std::thread::spawn(move || play_pcm(out, stop2, wave2));
        }

        Player { start, end, stop, frames, video_done, wave, audio: has_audio, began: Instant::now(), children }
    }

    /// Playback position in clip seconds.
    pub fn clock(&self) -> f64 {
        if self.audio {
            let h = self.wave.load(Ordering::SeqCst);
            if h != 0 {
                let mut mt = MMTIME { wType: TIME_SAMPLES, ..Default::default() };
                unsafe {
                    if waveOutGetPosition(HWAVEOUT(h as *mut _), &mut mt, std::mem::size_of::<MMTIME>() as u32) == 0 && mt.wType == TIME_SAMPLES {
                        return self.start + mt.u.sample as f64 / RATE as f64;
                    }
                }
            }
            return self.start; // audio not started yet
        }
        self.start + self.began.elapsed().as_secs_f64()
    }

    /// The newest frame that is due, dropping older ones.
    pub fn take_due(&self) -> Option<Frame> {
        let now = self.clock();
        let mut q = self.frames.lock().unwrap();
        let mut best = None;
        while q.front().is_some_and(|f| f.t <= now) {
            best = q.pop_front();
        }
        best
    }

    pub fn finished(&self) -> bool {
        self.clock() >= self.end - 0.01 || (self.video_done.load(Ordering::SeqCst) && self.frames.lock().unwrap().is_empty() && !self.audio)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        for c in self.children.lock().unwrap().iter_mut() {
            let _ = c.kill();
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Stream PCM from `out` into waveOut until EOF or `stop`.
fn play_pcm(mut out: impl Read, stop: Arc<AtomicBool>, wave: Arc<AtomicIsize>) {
    unsafe {
        let fmt = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: 2,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let mut hwo = HWAVEOUT::default();
        if waveOutOpen(Some(&mut hwo), WAVE_MAPPER, &fmt, None, None, CALLBACK_NULL) != 0 {
            return;
        }
        wave.store(hwo.0 as isize, Ordering::SeqCst);
        const N: usize = 8;
        const BYTES: usize = 4800 * 4; // 100 ms per buffer
        let mut bufs: Vec<Vec<u8>> = (0..N).map(|_| vec![0u8; BYTES]).collect();
        let mut hdrs: Vec<WAVEHDR> = (0..N).map(|_| WAVEHDR::default()).collect();
        let hsize = std::mem::size_of::<WAVEHDR>() as u32;
        let mut next = 0usize;
        let mut eof = false;
        while !stop.load(Ordering::SeqCst) && !eof {
            let h = &mut hdrs[next];
            let busy = h.dwFlags & WHDR_PREPARED != 0 && h.dwFlags & WHDR_DONE == 0;
            if busy {
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            if h.dwFlags & WHDR_PREPARED != 0 {
                waveOutUnprepareHeader(hwo, h, hsize);
            }
            let buf = &mut bufs[next];
            let mut n = 0;
            while n < BYTES {
                match out.read(&mut buf[n..]) {
                    Ok(0) | Err(_) => {
                        eof = true;
                        break;
                    }
                    Ok(k) => n += k,
                }
            }
            if n == 0 {
                break;
            }
            *h = WAVEHDR { lpData: windows_core::PSTR(buf.as_mut_ptr()), dwBufferLength: (n - n % 4) as u32, ..Default::default() };
            waveOutPrepareHeader(hwo, h, hsize);
            waveOutWrite(hwo, h, hsize);
            next = (next + 1) % N;
        }
        // let the queued sound finish unless stopped
        while !stop.load(Ordering::SeqCst) && hdrs.iter().any(|h| h.dwFlags & WHDR_PREPARED != 0 && h.dwFlags & WHDR_DONE == 0) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        wave.store(0, Ordering::SeqCst);
        waveOutReset(hwo);
        for h in hdrs.iter_mut() {
            if h.dwFlags & WHDR_PREPARED != 0 {
                waveOutUnprepareHeader(hwo, h, hsize);
            }
        }
        waveOutClose(hwo);
    }
}
