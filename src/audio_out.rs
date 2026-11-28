//! Audio output and the playback clock.
//!
//! The decoder side pushes interleaved samples into a shared buffer; the device callback
//! drains it. The number of frames the callback has actually consumed is the master clock
//! that video is synchronised to, so audio and picture cannot drift apart.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::stretch::Stretcher;

/// A buffer the device callback filled: where its sound sits in the media and when the
/// device starts playing it.
#[derive(Clone, Copy, Debug)]
struct InFlight {
    /// Media frames played before this buffer.
    start: u64,
    /// Media frames in it (fewer than its length when audio ran short, scaled by the speed).
    media: u64,
    /// Output frames at its front that carry that media; the rest is silence.
    sounding: u64,
    /// When its first sample reaches the speaker.
    at: Instant,
}

/// Output latency above this is taken for a bogus device report and ignored.
const MAX_LATENCY: Duration = Duration::from_secs(2);
/// Buffers remembered while they are on their way to the speaker: latency / buffer length of
/// them, usually a handful; enough for [`MAX_LATENCY`] at 2 ms buffers. Allocated up front,
/// so the device callback never allocates.
const IN_FLIGHT: usize = 1024;

pub struct AudioShared {
    buf: Mutex<(VecDeque<f32>, Stretcher)>,
    played_frames: AtomicU64,
    /// Buffers handed to the device, oldest first, back to the one now being heard.
    in_flight: Mutex<VecDeque<InFlight>>,
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
    speed_bits: AtomicU64,
}

impl Default for AudioShared {
    fn default() -> Self {
        Self {
            buf: Mutex::new((VecDeque::new(), Stretcher::default())),
            played_frames: AtomicU64::new(0),
            in_flight: Mutex::new(VecDeque::with_capacity(IN_FLIGHT)),
            channels: AtomicU32::new(2),
            rate: AtomicU32::new(48000),
            paused: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            free_run: AtomicBool::new(false),
            device_error: AtomicBool::new(false),
            volume_bits: AtomicU32::new(1.0f32.to_bits()),
            speed_bits: AtomicU64::new(1.0f64.to_bits()),
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

    pub fn set_speed(&self, speed: f64) {
        let s = if speed.is_finite() && speed > 0.0 { speed } else { 1.0 };
        self.speed_bits.store(s.to_bits(), Ordering::Relaxed);
    }

    pub fn speed(&self) -> f64 {
        f64::from_bits(self.speed_bits.load(Ordering::Relaxed))
    }

    /// Queues interleaved samples in the device's channel layout.
    pub fn push(&self, samples: &[f32]) {
        self.buf.lock().unwrap().0.extend(samples.iter().copied());
    }

    pub fn buffered_frames(&self) -> usize {
        let queue = self.buf.lock().unwrap();
        (queue.0.len() / self.channels().max(1)).saturating_sub(queue.1.played_frames_held())
    }

    pub fn buffered_secs(&self) -> f64 {
        self.buffered_frames() as f64 / self.rate() as f64
    }

    /// Seconds of audio the device has consumed since the last [`reset`](Self::reset).
    pub fn played_secs(&self) -> f64 {
        self.played_frames.load(Ordering::Relaxed) as f64 / self.rate() as f64
    }

    /// Seconds of the media heard from the speaker by now: what the device has consumed, less
    /// what is still on its way out (its output latency and the buffer it is playing). This is
    /// the clock pictures are shown by; [`played_secs`](Self::played_secs) would put them
    /// ahead of the sound by that latency.
    pub fn heard_secs(&self) -> f64 {
        self.heard_secs_at(Instant::now())
    }

    /// [`heard_secs`](Self::heard_secs) at `now`.
    pub fn heard_secs_at(&self, now: Instant) -> f64 {
        let in_flight = self.in_flight.lock().unwrap();
        let rate = self.rate() as f64;
        let Some(first) = in_flight.front() else { return self.played_secs() };
        // The buffer being heard is the newest one that has reached the speaker. Its sound
        // plays at its own pace from its front; after that (silence, or a pause) the clock
        // waits for the next buffer, which starts where this one ended.
        let Some(b) = in_flight.iter().rev().find(|b| b.at <= now) else { return first.start as f64 / rate };
        let into = (now - b.at).as_secs_f64() * rate;
        let within = if b.sounding == 0 { 0.0 } else { into.min(b.sounding as f64) * b.media as f64 / b.sounding as f64 };
        (b.start as f64 + within) / rate
    }

    /// Drops everything queued and restarts the frame count (used on open and on seek).
    pub fn reset(&self) {
        let mut queue = self.buf.lock().unwrap();
        queue.0.clear();
        queue.1.reset();
        self.played_frames.store(0, Ordering::Relaxed);
        self.in_flight.lock().unwrap().clear();
    }

    /// The device callback: fills `out` (interleaved, [`channels`](Self::channels) wide) and
    /// advances the clock by what was really played.
    pub fn render(&self, out: &mut [f32]) {
        self.render_at(out, Instant::now(), Duration::ZERO);
    }

    /// [`render`](Self::render) called at `now` for a buffer the device starts playing
    /// `latency` later.
    pub fn render_at(&self, out: &mut [f32], now: Instant, latency: Duration) {
        let channels = self.channels().max(1);
        if self.is_paused() {
            out.fill(0.0);
            return;
        }
        let gain = if self.is_muted() { 0.0 } else { self.volume() };
        let speed = self.speed();
        let mut queue = self.buf.lock().unwrap();
        let (buf, stretch) = &mut *queue;
        let mut filled = 0;
        let mut frames = 0u64;
        if speed != 1.0 || stretch.is_active() {
            let done = stretch.render(buf, out, speed, self.rate(), channels);
            filled = done.samples;
            frames = done.media_frames;
        }
        if !stretch.is_active() {
            // Only whole frames, so a channel can never slip into its neighbour's slot.
            let take = (out.len() - filled).min(buf.len()) / channels * channels;
            for (dst, src) in out[filled..filled + take].iter_mut().zip(buf.drain(..take)) {
                *dst = src;
            }
            filled += take;
            frames += (take / channels) as u64;
        }
        drop(queue);
        for sample in &mut out[..filled] {
            *sample *= gain;
        }
        out[filled..].fill(0.0);
        let mut sounding = (filled / channels) as u64;
        if self.is_free_run() {
            frames = ((out.len() / channels) as f64 * speed).round() as u64;
            // Free running is the clock going on without sound: over the whole buffer.
            sounding = (out.len() / channels) as u64;
        }
        let start = self.played_frames.fetch_add(frames, Ordering::Relaxed);
        let latency = if latency > MAX_LATENCY { Duration::ZERO } else { latency };
        let mut in_flight = self.in_flight.lock().unwrap();
        // Buffers wholly superseded by a later one already being heard are forgotten.
        while in_flight.get(1).is_some_and(|b| b.at <= now) || in_flight.len() >= IN_FLIGHT {
            in_flight.pop_front();
        }
        in_flight.push_back(InFlight { start, media: frames, sounding, at: now + latency });
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

/// How long the device may go without asking for sound, while reporting passing errors,
/// before it is taken for dead (Linux).
pub const STALL_BEFORE_LOST: Duration = Duration::from_millis(500);

/// Whether a device error, `since` the last time the device asked for sound, means the stream
/// is dead and must be reopened.
///
/// An underrun and a reroute to a new default device never do: the stream carries on (ALSA on
/// Linux reports every underrun). On Linux the ALSA stream also carries on after any other
/// error but a lost device; the PulseAudio/PipeWire bridge reports passing I/O errors when
/// streams come and go, and reopening for each would end playback with "audio device keeps
/// failing". There such an error counts only once the device has stopped asking for sound.
pub fn error_kills_stream(kind: cpal::ErrorKind, since: Duration) -> bool {
    use cpal::ErrorKind::*;
    match kind {
        Xrun | DeviceChanged => false,
        DeviceNotAvailable | StreamInvalidated | HostUnavailable => true,
        _ if cfg!(target_os = "linux") => since >= STALL_BEFORE_LOST,
        _ => true,
    }
}

impl AudioOut {
    pub fn open(shared: Arc<AudioShared>, src_rate: u32, src_channels: usize) -> Result<Self, String> {
        let device = cpal::default_host().default_output_device().ok_or("no audio output device")?;
        let build = |rate: u32, channels: u16| {
            let cb_shared = shared.clone();
            let err_shared = shared.clone();
            // When the data callback last ran, in milliseconds since the stream was built.
            let born = Instant::now();
            let last_render = Arc::new(AtomicU64::new(0));
            let cb_last_render = last_render.clone();
            let config = cpal::StreamConfig { channels, sample_rate: rate, buffer_size: cpal::BufferSize::Default };
            device.build_output_stream(
                config,
                move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    let t = info.timestamp();
                    let now = Instant::now();
                    cb_last_render.store((now - born).as_millis() as u64, Ordering::Relaxed);
                    cb_shared.render_at(data, now, t.playback.duration_since(t.callback));
                },
                // After an error the callback may never run again, which would freeze the
                // clock; the pipeline reopens the device when it sees this flag.
                move |err: cpal::Error| {
                    let since = (Instant::now() - born).saturating_sub(Duration::from_millis(last_render.load(Ordering::Relaxed)));
                    if error_kills_stream(err.kind(), since) {
                        err_shared.flag_device_error()
                    }
                },
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
