//! Video half: a hidden ffmpeg child captures on the GPU (gfxcapture =
//! Windows Graphics Capture), scales, and NVENC-encodes. We read raw H.264
//! from its stdout and cut it into frames on the access-unit delimiters.

use crate::buffer::{Frame, VideoRing};
use crate::config::Config;
use crate::util::{exe_dir, log, now_100ns};
use std::io::Read;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::*;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub enum Source {
    Window(u64),
    Monitor(u64),
}

pub struct VideoPipeline {
    child: Child,
    reader: Option<JoinHandle<()>>,
    pub alive: Arc<AtomicBool>,
}

/// Every ffmpeg we start joins this job, so it dies with Clipper even if
/// Clipper crashes.
fn job() -> HANDLE {
    static JOB: OnceLock<isize> = OnceLock::new();
    let h = *JOB.get_or_init(|| unsafe {
        let h = CreateJobObjectW(None, None).unwrap_or_default();
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let _ = SetInformationJobObject(
            h,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        h.0 as isize
    });
    HANDLE(h as *mut _)
}

pub fn ffmpeg_command(cfg: &Config) -> Command {
    ffmpeg_command_at(&cfg.ffmpeg)
}

pub fn ffmpeg_command_at(exe: &std::path::Path) -> Command {
    let mut c = Command::new(exe);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

pub fn adopt(child: &Child) {
    unsafe {
        let _ = AssignProcessToJobObject(job(), HANDLE(child.as_raw_handle()));
    }
}

impl VideoPipeline {
    pub fn start(
        cfg: &Config,
        src: Source,
        width: u32,
        height: u32,
        fps: u32,
        ring: Arc<Mutex<VideoRing>>,
    ) -> std::io::Result<VideoPipeline> {
        let target = match src {
            Source::Window(h) => format!("hwnd={h}"),
            Source::Monitor(h) => format!("hmonitor={h}"),
        };
        let filter = format!(
            "gfxcapture={target}:width={width}:height={height}:resize_mode=scale_aspect:max_framerate={fps}"
        );
        let gop = fps.to_string(); // keyframe every second
        let fps = fps.to_string();
        let qp = cfg.qp.to_string();
        let args: Vec<&str> = vec![
            "-hide_banner", "-loglevel", "warning", "-nostdin",
            "-f", "lavfi", "-i", &filter,
            "-fps_mode", "cfr", "-r", &fps,
            "-c:v", "h264_nvenc", "-preset", "p5", "-tune", "hq",
            "-rc", "constqp", "-qp", &qp, "-bf", "0", "-g", &gop,
            "-aud", "1", "-zerolatency", "1", "-delay", "0",
            "-flush_packets", "1", "-f", "h264", "pipe:1",
        ];
        log(format!("ffmpeg {}", args.join(" ")));
        let errlog = std::fs::File::create(exe_dir().join("ffmpeg.log"))?;
        let mut child = ffmpeg_command(cfg)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(errlog))
            .spawn()?;
        adopt(&child);
        let mut out = child.stdout.take().unwrap();
        let alive = Arc::new(AtomicBool::new(true));
        let alive2 = alive.clone();
        let reader = std::thread::spawn(move || {
            read_frames(&mut out, ring);
            alive2.store(false, Ordering::SeqCst);
            log("ffmpeg output ended");
        });
        Ok(VideoPipeline { child, reader: Some(reader), alive })
    }

    pub fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

/// Position of the next access unit delimiter (start code + NAL type 9).
fn find_aud(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 4 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 && (buf[i + 3] & 0x1f) == 9 {
            // include the leading zero of a 4-byte start code
            return Some(if i > from && buf[i - 1] == 0 { i - 1 } else { i });
        }
        i += 1;
    }
    None
}

fn is_keyframe(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 && (au[i + 3] & 0x1f) == 5 {
            return true;
        }
        i += 1;
    }
    false
}

fn read_frames(out: &mut impl Read, ring: Arc<Mutex<VideoRing>>) {
    let mut buf: Vec<u8> = Vec::with_capacity(4 << 20);
    let mut chunk = vec![0u8; 1 << 20];
    let mut idx = 0u64;
    loop {
        let n = match out.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let scan_from = buf.len().saturating_sub(4);
        buf.extend_from_slice(&chunk[..n]);
        // a frame is complete once the next delimiter shows up
        let Some(mut start) = find_aud(&buf, 0) else { continue };
        let mut search = scan_from.max(start + 4);
        while let Some(next) = find_aud(&buf, search) {
            let au = buf[start..next].to_vec();
            let key = is_keyframe(&au);
            let now = now_100ns();
            ring.lock().unwrap().push(Frame { idx, key, data: au }, now);
            idx += 1;
            start = next;
            search = next + 4;
        }
        buf.drain(..start);
    }
}
