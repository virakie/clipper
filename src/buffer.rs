//! The rolling replay buffers. Video holds encoded H.264 frames, audio holds
//! raw 48 kHz stereo PCM; both are placed on the shared QPC timeline.

use crate::util::TICKS_PER_SEC;
use std::collections::VecDeque;

pub struct Frame {
    pub idx: u64,
    pub key: bool,
    pub data: Vec<u8>,
}

pub struct VideoRing {
    pub fps: u32,
    keep: usize,
    frames: VecDeque<Frame>,
    bytes: usize,
    /// Earliest estimate of when frame 0 was captured: min(arrival - idx/fps).
    /// Pipe/encoder jitter only ever makes frames arrive late, so the minimum
    /// is the cleanest estimate.
    t0: i64,
}

impl VideoRing {
    pub fn new(fps: u32, seconds: u32) -> Self {
        VideoRing {
            fps,
            keep: ((seconds + 3) * fps) as usize,
            frames: VecDeque::new(),
            bytes: 0,
            t0: i64::MAX,
        }
    }

    pub fn push(&mut self, f: Frame, arrival: i64) {
        let t = arrival - f.idx as i64 * TICKS_PER_SEC / self.fps as i64;
        self.t0 = self.t0.min(t);
        self.bytes += f.data.len();
        self.frames.push_back(f);
        while self.frames.len() > self.keep {
            self.pop();
        }
        // a clip must start on a keyframe
        while self.frames.front().is_some_and(|f| !f.key) {
            self.pop();
        }
    }

    fn pop(&mut self) {
        if let Some(f) = self.frames.pop_front() {
            self.bytes -= f.data.len();
        }
    }

    pub fn frame_time(&self, idx: u64) -> i64 {
        self.t0 + idx as i64 * TICKS_PER_SEC / self.fps as i64
    }

    pub fn seconds_buffered(&self) -> f64 {
        match (self.frames.front(), self.frames.back()) {
            (Some(a), Some(b)) => (b.idx - a.idx + 1) as f64 / self.fps as f64,
            _ => 0.0,
        }
    }

    pub fn megabytes(&self) -> f64 {
        self.bytes as f64 / 1_048_576.0
    }

    /// The last `seconds` of video starting on a keyframe:
    /// (annex-B bytes, frame count, capture time of the first frame).
    pub fn take_clip(&self, seconds: u32) -> Option<(Vec<u8>, u64, i64)> {
        let last = self.frames.back()?.idx;
        let want = last.saturating_sub((seconds * self.fps) as u64 - 1);
        let start = self
            .frames
            .iter()
            .find(|f| f.key && f.idx >= want)
            .or_else(|| self.frames.iter().find(|f| f.key))?
            .idx;
        let mut out = Vec::new();
        let mut n = 0u64;
        for f in self.frames.iter().filter(|f| f.idx >= start) {
            out.extend_from_slice(&f.data);
            n += 1;
        }
        Some((out, n, self.frame_time(start)))
    }
}

pub const RATE: i64 = 48_000;

pub struct AudioRing {
    keep_frames: usize,
    /// interleaved stereo i16
    samples: VecDeque<i16>,
    /// QPC time of the first sample frame
    start: i64,
    /// when the capture thread last delivered anything (0 = never)
    pub last_push: i64,
}

impl AudioRing {
    pub fn new(seconds: u32) -> Self {
        AudioRing {
            keep_frames: ((seconds + 5) as i64 * RATE) as usize,
            samples: VecDeque::new(),
            start: 0,
            last_push: 0,
        }
    }

    fn frames(&self) -> i64 {
        (self.samples.len() / 2) as i64
    }

    fn end(&self) -> i64 {
        self.start + self.frames() * TICKS_PER_SEC / RATE
    }

    /// Append a packet whose first sample was captured at time `t`.
    /// Loopback capture sends nothing during silence, so gaps are filled
    /// with zeros to keep the timeline honest.
    pub fn push(&mut self, data: &[i16], t: i64) {
        self.last_push = crate::util::now_100ns();
        if self.samples.is_empty() {
            self.start = t;
        } else {
            let gap = t - self.end();
            if gap > TICKS_PER_SEC / 50 {
                let n = (gap * RATE / TICKS_PER_SEC) as usize;
                self.samples.extend(std::iter::repeat_n(0i16, n * 2));
            }
        }
        self.samples.extend(data.iter().copied());
        let excess = self.frames() - self.keep_frames as i64;
        if excess > 0 {
            self.samples.drain(..(excess as usize) * 2);
            self.start += excess * TICKS_PER_SEC / RATE;
        }
    }

    /// Peak level of the newest `secs` of audio, in dBFS (-inf for silence).
    pub fn level_db(&self, secs: f64) -> f32 {
        let n = ((secs * RATE as f64) as usize * 2).min(self.samples.len());
        let peak = self.samples.iter().rev().take(n).map(|s| s.unsigned_abs()).max().unwrap_or(0);
        if peak == 0 { f32::NEG_INFINITY } else { 20.0 * (peak as f32 / 32768.0).log10() }
    }

    /// PCM covering [from, from + frames/RATE), zero-padded where missing.
    pub fn extract(&self, from: i64, frames: usize) -> Vec<i16> {
        let mut out = vec![0i16; frames * 2];
        if self.samples.is_empty() {
            return out;
        }
        let offset = ((from - self.start) as i128 * RATE as i128 / TICKS_PER_SEC as i128) as i64;
        let (a, b) = self.samples.as_slices();
        let get = |i: usize| if i < a.len() { a[i] } else { b[i - a.len()] };
        for (i, o) in out.chunks_exact_mut(2).enumerate() {
            let src = offset + i as i64;
            if src >= 0 && src < self.frames() {
                o[0] = get((src * 2) as usize);
                o[1] = get((src * 2 + 1) as usize);
            }
        }
        out
    }
}
