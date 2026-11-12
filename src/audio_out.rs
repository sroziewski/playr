//! Audio output and the playback clock.
//!
//! The decoder side pushes interleaved samples into a shared buffer; the device callback
//! drains it. The number of frames the callback has actually consumed is the master clock
//! that video is synchronised to, so audio and picture cannot drift apart.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub struct AudioShared {
    buf: Mutex<VecDeque<f32>>,
    played_frames: AtomicU64,
    channels: AtomicU32,
    rate: AtomicU32,
    paused: AtomicBool,
    muted: AtomicBool,
    /// When set, an empty buffer no longer stalls the clock: the callback keeps counting
    /// frames while emitting silence, so video can run past the end of (or a hole in) audio.
    free_run: AtomicBool,
    /// The device reported an error (unplugged, default device changed); the stream is dead.
    device_error: AtomicBool,
    volume_bits: AtomicU32,
}

impl Default for AudioShared {
    fn default() -> Self {
        Self {
            buf: Mutex::new(VecDeque::new()),
            played_frames: AtomicU64::new(0),
            channels: AtomicU32::new(2),
            rate: AtomicU32::new(48000),
            paused: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            free_run: AtomicBool::new(false),
            device_error: AtomicBool::new(false),
            volume_bits: AtomicU32::new(1.0f32.to_bits()),
        }
    }
}

impl AudioShared {
    pub fn set_format(&self, rate: u32, channels: u16) {
        self.rate.store(rate.max(1), Ordering::Relaxed);
        self.channels.store(channels.max(1) as u32, Ordering::Relaxed);
    }

    pub fn rate(&self) -> u32 {
        self.rate.load(Ordering::Relaxed)
    }

    pub fn channels(&self) -> usize {
        self.channels.load(Ordering::Relaxed) as usize
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn set_free_run(&self, on: bool) {
        self.free_run.store(on, Ordering::Relaxed);
    }

    pub fn is_free_run(&self) -> bool {
        self.free_run.load(Ordering::Relaxed)
    }

    pub fn flag_device_error(&self) {
        self.device_error.store(true, Ordering::Relaxed);
    }

    /// Whether the device has failed since this was last asked.
    pub fn take_device_error(&self) -> bool {
        self.device_error.swap(false, Ordering::Relaxed)
    }

    /// Linear gain, clamped to 0..=1.
    pub fn set_volume(&self, volume: f32) {
        let v = if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 0.0 };
        self.volume_bits.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }

    /// Queues interleaved samples in the device's channel layout.
    pub fn push(&self, samples: &[f32]) {
        self.buf.lock().unwrap().extend(samples.iter().copied());
    }

    pub fn buffered_frames(&self) -> usize {
        self.buf.lock().unwrap().len() / self.channels().max(1)
    }

    pub fn buffered_secs(&self) -> f64 {
        self.buffered_frames() as f64 / self.rate() as f64
    }

    /// Seconds of audio the device has consumed since the last [`reset`](Self::reset).
    pub fn played_secs(&self) -> f64 {
        self.played_frames.load(Ordering::Relaxed) as f64 / self.rate() as f64
    }

    /// Drops everything queued and restarts the frame count (used on open and on seek).
    pub fn reset(&self) {
        let mut buf = self.buf.lock().unwrap();
        buf.clear();
        self.played_frames.store(0, Ordering::Relaxed);
    }

    /// The device callback: fills `out` (interleaved, [`channels`](Self::channels) wide) and
    /// advances the clock by what was really played.
    pub fn render(&self, out: &mut [f32]) {
        let channels = self.channels().max(1);
        if self.is_paused() {
            out.fill(0.0);
            return;
        }
        let gain = if self.is_muted() { 0.0 } else { self.volume() };
        let mut buf = self.buf.lock().unwrap();
        // Only whole frames, so a channel can never slip into its neighbour's slot.
        let take = out.len().min(buf.len()) / channels * channels;
        for (dst, src) in out.iter_mut().zip(buf.drain(..take)) {
            *dst = src * gain;
        }
        drop(buf);
        out[take..].fill(0.0);
        let mut frames = take / channels;
        if self.is_free_run() {
            frames = out.len() / channels;
        }
        self.played_frames.fetch_add(frames as u64, Ordering::Relaxed);
    }
}

/// Remixes interleaved audio from `src_ch` to `dst_ch` channels.
///
/// Mono is copied to both front channels; 5.1 is folded down with the centre and surrounds at
/// -3 dB; any other surplus channels are dropped and missing ones left silent.
pub fn mix_channels(src: &[f32], src_ch: usize, dst_ch: usize, out: &mut Vec<f32>) {
    out.clear();
    if src_ch == 0 || dst_ch == 0 {
        return;
    }
    if src_ch == dst_ch {
        out.extend_from_slice(src);
        return;
    }
    out.reserve(src.len() / src_ch * dst_ch);
    for frame in src.chunks_exact(src_ch) {
        let (l, r) = match src_ch {
            1 => (frame[0], frame[0]),
            6 => (
                0.5 * (frame[0] + 0.707 * frame[2] + 0.707 * frame[4]),
                0.5 * (frame[1] + 0.707 * frame[2] + 0.707 * frame[5]),
            ),
            _ => (frame[0], frame[1]),
        };
        if dst_ch == 1 {
            out.push(0.5 * (l + r));
        } else {
            out.push(l);
            out.push(r);
            out.extend(std::iter::repeat_n(0.0, dst_ch - 2));
        }
    }
}

/// Linear-interpolation resampler, used only when the device refuses the source sample rate
/// (on Windows the system mixer converts for us, so this is the fallback path).
pub struct LinearResampler {
    channels: usize,
    step: f64,
    pos: f64,
    prev: Vec<f32>,
}

impl LinearResampler {
    pub fn new(src_rate: u32, dst_rate: u32, channels: usize) -> Self {
        let channels = channels.max(1);
        Self { channels, step: src_rate.max(1) as f64 / dst_rate.max(1) as f64, pos: 0.0, prev: vec![0.0; channels] }
    }

    pub fn process(&mut self, src: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        let frames = src.len() / ch;
        // `pos` is measured from the frame before `src[0]` (the tail of the previous call).
        while self.pos < frames as f64 {
            let i = self.pos.floor() as usize;
            let t = (self.pos - i as f64) as f32;
            for c in 0..ch {
                let a = if i == 0 { self.prev[c] } else { src[(i - 1) * ch + c] };
                let b = src[i * ch + c];
                out.push(a + (b - a) * t);
            }
            self.pos += self.step;
        }
        self.pos -= frames as f64;
        if frames > 0 {
            self.prev.copy_from_slice(&src[(frames - 1) * ch..frames * ch]);
        }
    }

    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.prev.fill(0.0);
    }
}

/// An open output device plus the conversion from the source format to the device format.
pub struct AudioOut {
    _stream: cpal::Stream,
    shared: Arc<AudioShared>,
    src_rate: u32,
    src_channels: usize,
    resampler: Option<LinearResampler>,
    mixed: Vec<f32>,
    resampled: Vec<f32>,
}

impl AudioOut {
    pub fn open(shared: Arc<AudioShared>, src_rate: u32, src_channels: usize) -> Result<Self, String> {
        let device = cpal::default_host().default_output_device().ok_or("no audio output device")?;
        let build = |rate: u32, channels: u16| {
            let cb_shared = shared.clone();
            let err_shared = shared.clone();
            let config = cpal::StreamConfig { channels, sample_rate: rate, buffer_size: cpal::BufferSize::Default };
            device.build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| cb_shared.render(data),
                // After an error the callback may never run again, which would freeze the
                // clock; the pipeline reopens the device when it sees this flag.
                move |_err| err_shared.flag_device_error(),
                None,
            )
        };
        // First choice: stereo at the source rate, so nothing is resampled in this process.
        let (stream, rate, channels) = match build(src_rate, 2) {
            Ok(s) => (s, src_rate, 2u16),
            Err(first) => {
                let def = device.default_output_config().map_err(|e| format!("audio device: {e} ({first})"))?;
                let (rate, channels) = (def.sample_rate(), def.channels());
                let s = build(rate, channels).map_err(|e| format!("audio device: {e}"))?;
                (s, rate, channels)
            }
        };
        shared.set_format(rate, channels);
        stream.play().map_err(|e| format!("audio device: {e}"))?;
        let resampler = (rate != src_rate).then(|| LinearResampler::new(src_rate, rate, channels as usize));
        Ok(Self {
            _stream: stream,
            shared,
            src_rate,
            src_channels,
            resampler,
            mixed: Vec::new(),
            resampled: Vec::new(),
        })
    }

    pub fn source_format(&self) -> (u32, usize) {
        (self.src_rate, self.src_channels)
    }

    /// Queues interleaved source-format samples for playback.
    pub fn write(&mut self, samples: &[f32]) {
        let dst_ch = self.shared.channels();
        mix_channels(samples, self.src_channels, dst_ch, &mut self.mixed);
        match self.resampler.as_mut() {
            Some(r) => {
                self.resampled.clear();
                r.process(&self.mixed, &mut self.resampled);
                self.shared.push(&self.resampled);
            }
            None => self.shared.push(&self.mixed),
        }
    }

    pub fn reset(&mut self) {
        if let Some(r) = self.resampler.as_mut() {
            r.reset();
        }
    }
}
