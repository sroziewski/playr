//! The playback pipeline: one thread that demuxes, decodes and keeps the audio buffer and the
//! video frame queue topped up, plus the state the UI reads.

use std::collections::VecDeque;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::registry::CodecRegistry;

use crate::audio_out::{AudioOut, AudioShared};
use crate::demux::{self, Demuxer};
use crate::media::{AudioPacket, AudioTrack, MediaInfo, Packet, VideoFrame, VideoTrack, trim_decoded};
use crate::net;
use crate::vdec::{self, VideoDecoder};

/// Decoded pictures held ready for display. Small on purpose: a 1080p frame is 3 MB.
pub const FRAME_QUEUE: usize = 6;
/// How far demuxing may run ahead of one stream while feeding the other.
const VIDEO_STASH: usize = 300;
const AUDIO_STASH: usize = 2000;
/// Seconds of decoded audio to keep queued: files need little, radio needs a jitter buffer.
const AUDIO_BUFFER_FILE: f64 = 0.6;
const AUDIO_BUFFER_LIVE: f64 = 3.0;
/// A seek decodes forward from the previous keyframe to the exact target only when that costs
/// at most this many seconds of video; farther than that it resumes at the keyframe.
const PRECISE_SEEK_WINDOW: f64 = 3.0;
/// Audio that starts later than the resume point is held back with silence, up to this much.
const MAX_AUDIO_PAD_SECS: f64 = 60.0;
/// Audio within this distance of the resume point is played as it is.
const ALIGN_TOLERANCE_SECS: f64 = 0.01;
/// Consecutive undecodable audio packets after which the user is told.
const AUDIO_FAILURES_BEFORE_NOTE: u32 = 50;
/// How many times a lost output device is reopened before audio is given up.
const MAX_DEVICE_REOPENS: u32 = 5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Idle,
    Loading,
    Playing,
    Ended,
    Failed,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub state: State,
    pub source: String,
    pub title: String,
    /// Stream title announced by a radio station.
    pub now_playing: Option<String>,
    pub duration: Option<f64>,
    pub seekable: bool,
    pub live: bool,
    pub video: Option<(u32, u32)>,
    pub has_audio: bool,
    pub info: String,
    pub error: Option<String>,
    pub notes: Vec<String>,
}

struct Clock {
    base: f64,
    use_audio: bool,
    wall_accum: f64,
    wall_anchor: Option<Instant>,
}

pub struct Shared {
    pub audio: Arc<AudioShared>,
    frames: Mutex<VecDeque<VideoFrame>>,
    status: Mutex<Status>,
    clock: Mutex<Clock>,
    /// Set after open and seek so the first decoded picture is shown even while paused.
    force_show: AtomicBool,
    /// Abort flag of the session being opened or played; lets the UI thread cut short a
    /// stalled network read that the pipeline thread is blocked in.
    abort: Mutex<Arc<AtomicBool>>,
    repaint: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn new(repaint: Box<dyn Fn() + Send + Sync>) -> Self {
        Self {
            audio: Arc::new(AudioShared::default()),
            frames: Mutex::new(VecDeque::new()),
            status: Mutex::new(Status::default()),
            clock: Mutex::new(Clock { base: 0.0, use_audio: false, wall_accum: 0.0, wall_anchor: None }),
            force_show: AtomicBool::new(false),
            abort: Mutex::new(Arc::new(AtomicBool::new(false))),
            repaint,
        }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    fn update_status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap());
        (self.repaint)();
    }

    fn note(&self, note: String) {
        self.update_status(|s| {
            if !s.notes.contains(&note) {
                s.notes.push(note);
            }
        });
    }

    fn fail(&self, error: String) {
        self.set_paused(true);
        self.update_status(|st| {
            st.state = State::Failed;
            st.error = Some(error);
        });
    }

    /// Current playback position in seconds.
    pub fn position(&self) -> f64 {
        let c = self.clock.lock().unwrap();
        if c.use_audio {
            c.base + self.audio.played_secs()
        } else {
            c.base + c.wall_accum + c.wall_anchor.map_or(0.0, |t| t.elapsed().as_secs_f64())
        }
    }

    pub fn is_paused(&self) -> bool {
        self.audio.is_paused()
    }

    pub fn set_paused(&self, paused: bool) {
        self.audio.set_paused(paused);
        let mut c = self.clock.lock().unwrap();
        if paused {
            if let Some(t) = c.wall_anchor.take() {
                c.wall_accum += t.elapsed().as_secs_f64();
            }
        } else if c.wall_anchor.is_none() {
            c.wall_anchor = Some(Instant::now());
        }
    }

    fn restart_clock(&self, base: f64, use_audio: Option<bool>) {
        let paused = self.is_paused();
        let mut c = self.clock.lock().unwrap();
        c.base = base;
        if let Some(use_audio) = use_audio {
            c.use_audio = use_audio;
        }
        c.wall_accum = 0.0;
        c.wall_anchor = (!paused).then(Instant::now);
    }

    fn set_clock_base(&self, base: f64) {
        self.clock.lock().unwrap().base = base;
    }

    fn frames_len(&self) -> usize {
        self.frames.lock().unwrap().len()
    }

    fn push_frame(&self, frame: VideoFrame) {
        let was_empty = {
            let mut q = self.frames.lock().unwrap();
            let was_empty = q.is_empty();
            q.push_back(frame);
            was_empty
        };
        // The UI sleeps when it has nothing scheduled, so the first frame has to wake it.
        if was_empty {
            (self.repaint)();
        }
    }

    fn clear_frames(&self) {
        self.frames.lock().unwrap().clear();
    }

    /// The picture that should be on screen now, if it changed, and the delay in seconds until
    /// the next one is due. Pictures that are already overdue are skipped, which is what keeps
    /// video in step with audio when decoding falls behind.
    pub fn take_due_frame(&self) -> (Option<VideoFrame>, Option<f64>) {
        let pos = self.position();
        let mut q = self.frames.lock().unwrap();
        let mut latest = None;
        if self.force_show.load(Ordering::Relaxed) && !q.is_empty() {
            self.force_show.store(false, Ordering::Relaxed);
            latest = q.pop_front();
        }
        while q.front().is_some_and(|f| f.pts <= pos + 0.002) {
            latest = q.pop_front();
        }
        let next = q.front().map(|f| (f.pts - pos).max(0.0));
        (latest, next)
    }

    fn abort_current(&self) {
        self.abort.lock().unwrap().store(true, Ordering::Relaxed);
    }
}

enum Cmd {
    Open(String),
    Seek(f64),
    Stop,
    Quit,
}

/// Handle owned by the UI. Dropping it stops the pipeline thread.
pub struct Player {
    tx: Sender<Cmd>,
    pub shared: Arc<Shared>,
}

impl Player {
    /// `repaint` is called from the pipeline thread whenever the UI has something new to show.
    pub fn new(repaint: Box<dyn Fn() + Send + Sync>) -> Self {
        let shared = Arc::new(Shared::new(repaint));
        let (tx, rx) = channel();
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("playr-pipeline".into())
            .spawn(move || run(thread_shared, rx))
            .expect("spawn pipeline thread");
        Self { tx, shared }
    }

    /// Opens a file path or an http(s) URL and starts playing it.
    pub fn open(&self, target: impl Into<String>) {
        // Whatever is playing is about to be replaced; do not let a stalled stream delay that.
        self.shared.abort_current();
        let _ = self.tx.send(Cmd::Open(target.into()));
    }

    pub fn seek(&self, secs: f64) {
        let _ = self.tx.send(Cmd::Seek(secs));
    }

    pub fn stop(&self) {
        self.shared.abort_current();
        let _ = self.tx.send(Cmd::Stop);
    }

    /// Play/pause; at the end of the media it restarts from the beginning.
    pub fn toggle_pause(&self) {
        if self.shared.status().state == State::Ended {
            self.seek(0.0);
        } else {
            self.shared.set_paused(!self.shared.is_paused());
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.abort_current();
        let _ = self.tx.send(Cmd::Quit);
    }
}

fn run(shared: Arc<Shared>, rx: Receiver<Cmd>) {
    let mut session: Option<Session> = None;
    let mut queued: Option<Cmd> = None;
    loop {
        let cmd = match queued.take() {
            Some(c) => Some(c),
            None => match session.as_mut() {
                None => match rx.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                },
                Some(s) => match rx.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Disconnected) => return,
                    // Demuxers and decoders parse untrusted data; a panic in any of them ends
                    // this media, not the pipeline thread (which would leave the UI dead).
                    Err(TryRecvError::Empty) => match catch_unwind(AssertUnwindSafe(|| s.step(&shared))) {
                        Ok(true) => None,
                        Ok(false) => match rx.recv_timeout(s.idle_wait(&shared)) {
                            Ok(c) => Some(c),
                            Err(RecvTimeoutError::Timeout) => None,
                            Err(RecvTimeoutError::Disconnected) => return,
                        },
                        Err(_) => {
                            session = None;
                            shared.fail("playback stopped: internal error in a decoder".into());
                            None
                        }
                    },
                },
            },
        };
        match cmd {
            Some(Cmd::Open(mut target)) => {
                // Opening can block on the network for many seconds; if the user has already
                // asked for something else, do not make them wait for this one first.
                // A seek belongs to whatever was opened before it, so only one that follows
                // the newest open survives.
                let mut seek_after = None;
                loop {
                    match rx.try_recv() {
                        Ok(Cmd::Open(newer)) => {
                            target = newer;
                            seek_after = None;
                        }
                        Ok(Cmd::Seek(to)) => seek_after = Some(to),
                        Ok(other) => {
                            queued = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                if queued.is_none() {
                    queued = seek_after.map(Cmd::Seek);
                }
                // Drop the old session first so its device and download are released.
                session = None;
                begin_loading(&shared, &target);
                match catch_unwind(AssertUnwindSafe(|| Session::open(&shared, &target))) {
                    Ok(Ok(s)) => session = Some(s),
                    Ok(Err(e)) => shared.fail(e),
                    Err(_) => shared.fail("cannot open: internal error while reading the file".into()),
                }
            }
            Some(Cmd::Seek(mut to)) => {
                // Dragging the seek bar queues many seeks; only the newest one matters.
                loop {
                    match rx.try_recv() {
                        Ok(Cmd::Seek(newer)) => to = newer,
                        Ok(other) => {
                            queued = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                if let Some(s) = session.as_mut()
                    && catch_unwind(AssertUnwindSafe(|| s.seek(&shared, to))).is_err()
                {
                    session = None;
                    shared.fail("playback stopped: internal error while seeking".into());
                }
            }
            Some(Cmd::Stop) => {
                session = None;
                begin_loading(&shared, "");
                shared.set_paused(true);
                shared.update_status(|st| *st = Status::default());
            }
            Some(Cmd::Quit) => return,
            None => {}
        }
    }
}

fn begin_loading(shared: &Shared, target: &str) {
    shared.clear_frames();
    shared.audio.reset();
    shared.audio.set_free_run(false);
    shared.audio.take_device_error();
    shared.set_paused(false);
    shared.restart_clock(0.0, Some(false));
    shared.update_status(|st| {
        *st = Status { state: State::Loading, source: target.to_string(), title: display_name(target), ..Default::default() };
    });
}

fn display_name(target: &str) -> String {
    if net::is_url(target) {
        return target.to_string();
    }
    Path::new(target).file_name().and_then(|n| n.to_str()).unwrap_or(target).to_string()
}

/// Symphonia's own decoders plus the pure-Rust Opus decoder it does not ship.
fn audio_codecs() -> &'static CodecRegistry {
    static REGISTRY: OnceLock<CodecRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut registry);
        registry.register_audio_decoder::<symphonia_adapter_opus_rs::OpusAdapter>();
        registry
    })
}

enum ADec {
    Sym(Box<dyn AudioDecoder>),
    Pcm { rate: u32, channels: u16, bits: u16 },
}

impl ADec {
    /// Decodes one packet into interleaved `f32`; returns (sample rate, channel count).
    fn decode(&mut self, packet: &AudioPacket, out: &mut Vec<f32>) -> Result<(u32, usize), String> {
        out.clear();
        match self {
            ADec::Sym(dec) => {
                let buf = dec.decode(packet).map_err(|e| e.to_string())?;
                let spec = buf.spec();
                let format = (spec.rate(), spec.channels().count());
                buf.copy_to_vec_interleaved::<f32>(out);
                Ok(format)
            }
            ADec::Pcm { rate, channels, bits } => {
                pcm_to_f32(&packet.data, *bits, out);
                Ok((*rate, *channels as usize))
            }
        }
    }
}

/// Little-endian PCM to `f32`: 8-bit is unsigned, 16/24/32-bit are signed.
pub fn pcm_to_f32(data: &[u8], bits: u16, out: &mut Vec<f32>) {
    match bits {
        8 => out.extend(data.iter().map(|&b| (b as f32 - 128.0) / 128.0)),
        16 => out.extend(data.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)),
        24 => out.extend(
            data.chunks_exact(3).map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f32 / 8_388_608.0),
        ),
        32 => out.extend(
            data.chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0),
        ),
        _ => {}
    }
}

/// What to do with the first decoded audio after an open or a seek.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioAlign {
    /// The whole packet lies before the resume point: discard it and look at the next one.
    Drop,
    /// Discard `skip` leading frames, or precede the packet with `pad` frames of silence.
    Play { skip: usize, pad: usize },
}

/// Lines up a packet of `frames` frames starting at `pts` with the point playback resumes
/// from, so that the audio clock (which counts frames played) agrees with picture timestamps.
pub fn align_audio(resume_at: f64, pts: f64, frames: usize, rate: u32) -> AudioAlign {
    let lead = resume_at - pts;
    if lead > 0.0 {
        let skip = (lead * rate as f64).round() as usize;
        if skip >= frames { AudioAlign::Drop } else { AudioAlign::Play { skip, pad: 0 } }
    } else if lead < -ALIGN_TOLERANCE_SECS {
        AudioAlign::Play { skip: 0, pad: ((-lead).min(MAX_AUDIO_PAD_SECS) * rate as f64) as usize }
    } else {
        AudioAlign::Play { skip: 0, pad: 0 }
    }
}

/// A demuxed audio packet waiting to be decoded (see [`Packet::Audio`] for the fields).
struct AudioIn {
    pts: f64,
    packet: AudioPacket,
    skip: u32,
    limit: Option<u32>,
}

struct Session {
    demux: Box<dyn Demuxer>,
    info: MediaInfo,
    vtrack: Option<VideoTrack>,
    vdec: Option<Box<dyn VideoDecoder>>,
    adec: Option<ADec>,
    out: Option<AudioOut>,
    pending_v: VecDeque<(f64, Box<[u8]>)>,
    pending_a: VecDeque<AudioIn>,
    /// Pictures flushed out of the decoder at end of media, waiting for room in the queue.
    tail: VecDeque<VideoFrame>,
    eof: bool,
    /// The end has been reported to the UI; nothing more to do until a seek.
    finished: bool,
    live: bool,
    /// A network source of known length: reaching its end is a normal end, not a lost stream.
    finite: bool,
    seekable: bool,
    /// Presentation time playback (re)starts from; earlier audio and video is discarded.
    resume_at: f64,
    /// Time the audio write position stands at when `align_pending` is set.
    audio_resume: f64,
    /// The next audio packet must be lined up with `audio_resume`: it is the first after an
    /// open, a seek or a device change, or its timestamp does not follow on from the last.
    align_pending: bool,
    /// Where the next audio packet starts if the track is continuous.
    next_audio_pts: Option<f64>,
    /// After a seek: drop video packets until a keyframe at or after this time.
    wait_key: Option<f64>,
    /// The clock runs on wall time because the audio track is not delivering sound.
    on_wall_clock: bool,
    scratch: Vec<f32>,
    frames_out: u64,
    decoder_panics: u32,
    audio_failures: u32,
    device_reopens: u32,
    last_device_loss: Option<Instant>,
    reported_video_failure: bool,
    audio_written: bool,
    /// The demuxer error that ended reading, kept to report a file that never played at all.
    read_error: Option<String>,
    abort: Arc<AtomicBool>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.abort.store(true, Ordering::Relaxed);
    }
}

impl Session {
    fn open(shared: &Arc<Shared>, target: &str) -> Result<Self, String> {
        let abort = Arc::new(AtomicBool::new(false));
        *shared.abort.lock().unwrap() = abort.clone();
        let mut target = target.trim().to_string();
        if !net::is_url(&target) && net::is_playlist_name(&target) {
            let mut text = Vec::new();
            std::fs::File::open(&target)
                .and_then(|f| f.take(256 * 1024).read_to_end(&mut text))
                .map_err(|e| format!("cannot read {target}: {e}"))?;
            target = net::parse_playlist(&String::from_utf8_lossy(&text))?
                .ok_or("playlist contains no stream address")?;
        }

        let mut title = display_name(&target);
        let live = net::is_url(&target);
        let mut finite = false;
        let demux: Box<dyn Demuxer> = if live {
            let cb_shared = shared.clone();
            let stream = net::open_url(
                &target,
                abort.clone(),
                Box::new(move |t| cb_shared.update_status(|st| st.now_playing = Some(t))),
            )?;
            if let Some(name) = stream.name {
                title = name;
            }
            finite = stream.finite;
            demux::open_stream(stream.source, stream.ext.as_deref(), stream.mime.as_deref())?
        } else {
            demux::open_path(Path::new(&target))?
        };

        let info = demux.info().clone();
        let mut notes = info.notes.clone();
        let mut vdec = None;
        if let Some(track) = &info.video {
            match vdec::make_decoder(track) {
                Ok(d) => vdec = Some(d),
                Err(e) => notes.push(e),
            }
        }
        let adec = match &info.audio {
            Some(AudioTrack::Symphonia(params)) => {
                match audio_codecs().make_audio_decoder(params, &AudioDecoderOptions::default()) {
                    Ok(d) => Some(ADec::Sym(d)),
                    Err(e) => {
                        notes.push(format!("audio codec is not supported ({e})"));
                        None
                    }
                }
            }
            Some(AudioTrack::Pcm { rate, channels, bits }) => {
                Some(ADec::Pcm { rate: *rate, channels: *channels, bits: *bits })
            }
            None => None,
        };
        if vdec.is_none() && adec.is_none() {
            let why = if notes.is_empty() { "no playable streams".to_string() } else { notes.join("; ") };
            return Err(why);
        }

        let seekable = info.seekable && info.duration.is_some();
        let vtrack = info.video.clone().filter(|_| vdec.is_some());
        let mut summary = info.container.clone();
        if let Some(t) = &vtrack {
            summary.push_str(&format!(" · {} {}×{}", t.codec.name(), t.width, t.height));
        }
        shared.restart_clock(0.0, Some(adec.is_some()));
        shared.force_show.store(true, Ordering::Relaxed);
        shared.update_status(|st| {
            st.state = State::Playing;
            st.title = title;
            st.duration = info.duration;
            st.seekable = seekable;
            st.live = live && !finite;
            st.video = vtrack.as_ref().map(|t| (t.width, t.height));
            st.has_audio = adec.is_some();
            st.info = summary;
            st.notes = notes;
        });

        Ok(Self {
            demux,
            info,
            vtrack,
            on_wall_clock: adec.is_none(),
            vdec,
            adec,
            out: None,
            pending_v: VecDeque::new(),
            pending_a: VecDeque::new(),
            tail: VecDeque::new(),
            eof: false,
            finished: false,
            live,
            finite,
            seekable,
            resume_at: 0.0,
            audio_resume: 0.0,
            align_pending: true,
            next_audio_pts: None,
            wait_key: None,
            scratch: Vec::new(),
            frames_out: 0,
            decoder_panics: 0,
            audio_failures: 0,
            device_reopens: 0,
            last_device_loss: None,
            reported_video_failure: false,
            audio_written: false,
            read_error: None,
            abort,
        })
    }

    /// How long the pipeline thread may sleep when a step found nothing to do. Commands wake
    /// it at once; the timeout only bounds how late it notices that the UI resumed playback
    /// or consumed pictures.
    fn idle_wait(&self, shared: &Shared) -> Duration {
        if self.finished {
            Duration::from_millis(500)
        } else if shared.is_paused() {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(4)
        }
    }

    /// Does one unit of work. Returns `false` when every buffer is full (or the media is
    /// finished), which tells the caller to sleep.
    fn step(&mut self, shared: &Arc<Shared>) -> bool {
        if self.out.is_some() && shared.audio.take_device_error() {
            self.on_device_lost(shared);
        }
        let high = if self.live { AUDIO_BUFFER_LIVE } else { AUDIO_BUFFER_FILE };
        let need_a = self.adec.is_some() && shared.audio.buffered_secs() < high;
        let need_v = (self.vdec.is_some() || !self.tail.is_empty()) && shared.frames_len() < FRAME_QUEUE;
        let can_read = !self.eof && self.pending_v.len() < VIDEO_STASH && self.pending_a.len() < AUDIO_STASH;

        if self.eof && self.pending_a.is_empty() {
            // No more audio will arrive: the clock must carry on by itself for the pictures
            // that remain.
            self.run_clock_without_audio(shared);
        }
        // Once the last video packet has been decoded, take the pictures the decoder is still
        // holding (a threaded decoder runs a second or so behind its input). This must not
        // wait for the audio that is still queued, or the end of the video would arrive late
        // — or, when paused, never.
        if self.eof
            && self.pending_v.is_empty()
            && let Some(mut dec) = self.vdec.take()
        {
            let tail = catch_unwind(AssertUnwindSafe(|| dec.flush())).unwrap_or_default();
            self.tail.extend(tail.into_iter().filter(|f| f.pts + 1e-3 >= self.resume_at));
            return true;
        }
        if need_v && let Some(frame) = self.tail.pop_front() {
            shared.push_frame(frame);
            return true;
        }

        // Audio first: it is cheap, and an audio underrun is far more noticeable than a late
        // video frame.
        if need_a {
            if let Some(audio) = self.pending_a.pop_front() {
                self.on_audio(shared, audio);
                return true;
            }
            if can_read {
                self.read_one(shared, need_a, need_v);
                return true;
            }
        }
        if need_v {
            if let Some((pts, data)) = self.pending_v.pop_front() {
                self.on_video(shared, pts, &data);
                return true;
            }
            if can_read {
                self.read_one(shared, need_a, need_v);
                return true;
            }
        }
        self.housekeeping(shared);
        false
    }

    /// Keeps time moving when the audio track cannot drive the clock: lets the device count
    /// silence, or, when no device was ever opened (nothing decodable arrived), switches to
    /// wall time — without a device there is no callback to do the counting.
    fn run_clock_without_audio(&mut self, shared: &Arc<Shared>) {
        if self.out.is_some() {
            shared.audio.set_free_run(true);
        } else if !self.on_wall_clock {
            self.on_wall_clock = true;
            shared.restart_clock(shared.position(), Some(false));
        }
    }

    fn on_device_lost(&mut self, shared: &Arc<Shared>) {
        // What was queued for the old device is gone with it; carry on from the current
        // position, and let the next packet open whatever is the default device now.
        let position = shared.position();
        self.out = None;
        shared.audio.reset();
        shared.restart_clock(position, Some(true));
        self.on_wall_clock = false;
        self.audio_resume = position;
        self.align_pending = true;
        self.next_audio_pts = None;
        // The cap is against a device that fails over and over, not against a user who
        // switches headphones a few times in an evening.
        if self.last_device_loss.is_some_and(|t| t.elapsed() > Duration::from_secs(60)) {
            self.device_reopens = 0;
        }
        self.last_device_loss = Some(Instant::now());
        self.device_reopens += 1;
    }

    fn read_one(&mut self, shared: &Arc<Shared>, need_a: bool, need_v: bool) {
        match self.demux.next_packet() {
            Ok(Some(Packet::Video { pts, data })) => {
                if self.vdec.is_none() {
                    return;
                }
                if need_v && self.pending_v.is_empty() {
                    self.on_video(shared, pts, &data);
                } else {
                    self.pending_v.push_back((pts, data));
                }
            }
            Ok(Some(Packet::Audio { pts, packet, skip, limit })) => {
                let audio = AudioIn { pts, packet, skip, limit };
                if self.adec.is_none() {
                    return;
                }
                if need_a && self.pending_a.is_empty() {
                    self.on_audio(shared, audio);
                } else {
                    self.pending_a.push_back(audio);
                }
            }
            Ok(None) => self.eof = true,
            Err(e) => {
                self.eof = true;
                if !self.abort.load(Ordering::Relaxed) {
                    shared.note(format!("stopped early: {e}"));
                    self.read_error = Some(e);
                }
            }
        }
    }

    fn housekeeping(&mut self, shared: &Arc<Shared>) {
        if !self.eof {
            // Audio has a hole (or ended before the video did, or cannot be decoded at all):
            // demuxing is as far ahead as it may go and still found none. Let the clock run
            // on so the picture keeps moving.
            if self.adec.is_some()
                && self.vdec.is_some()
                && self.pending_v.len() >= VIDEO_STASH
                && shared.audio.buffered_frames() == 0
            {
                self.run_clock_without_audio(shared);
            }
            return;
        }
        if !self.pending_v.is_empty() || !self.pending_a.is_empty() || !self.tail.is_empty() || self.finished {
            return;
        }
        let audio_done = self.out.is_none() || shared.audio.buffered_frames() == 0;
        if audio_done && shared.frames_len() == 0 {
            self.finished = true;
            shared.set_paused(true);
            let lost = self.live && !self.finite;
            // A file that broke before anything could be shown or heard did not play.
            let nothing_played = self.frames_out == 0 && !self.audio_written;
            let broken = self.read_error.take().filter(|_| nothing_played);
            shared.update_status(|st| {
                if let Some(e) = broken {
                    st.state = State::Failed;
                    st.error = Some(e);
                } else if lost {
                    st.state = State::Failed;
                    st.error = Some("the stream ended".into());
                } else {
                    st.state = State::Ended;
                }
            });
        }
    }

    fn on_video(&mut self, shared: &Arc<Shared>, pts: f64, data: &[u8]) {
        let Some(track) = self.vtrack.as_ref() else { return };
        if let Some(key_pts) = self.wait_key {
            if !(vdec::is_keyframe(track, data) && pts >= key_pts - 1e-3) {
                return;
            }
            self.wait_key = None;
        }
        let Some(dec) = self.vdec.as_mut() else { return };
        // The video decoders are young crates; a panic in one must not take the player down.
        match catch_unwind(AssertUnwindSafe(|| dec.decode(data, pts))) {
            Ok(frames) => {
                for f in frames {
                    if f.pts + 1e-3 < self.resume_at {
                        continue;
                    }
                    self.frames_out += 1;
                    shared.push_frame(f);
                }
                if self.frames_out == 0 && dec.errors() > 60 && !self.reported_video_failure {
                    self.reported_video_failure = true;
                    shared.note(format!("video cannot be decoded ({})", dec.last_error().unwrap_or("unknown error")));
                }
            }
            Err(_) => {
                self.decoder_panics += 1;
                self.vdec = if self.decoder_panics <= 5 { vdec::make_decoder(track).ok() } else { None };
                self.wait_key = Some(f64::NEG_INFINITY);
                if self.vdec.is_none() {
                    shared.note("video decoder failed; continuing without picture".into());
                    shared.update_status(|st| st.video = None);
                }
            }
        }
    }

    fn on_audio(&mut self, shared: &Arc<Shared>, audio: AudioIn) {
        let AudioIn { pts, packet, skip, limit } = audio;
        // After a seek the container may hand out audio from well before the resume point;
        // it would be decoded only to be thrown away.
        if self.align_pending && self.seekable && pts < self.audio_resume - 1.0 {
            return;
        }
        let Some(dec) = self.adec.as_mut() else { return };
        let scratch = &mut self.scratch;
        let (rate, channels) = match catch_unwind(AssertUnwindSafe(|| dec.decode(&packet, scratch))) {
            Ok(Ok(format)) => format,
            // A corrupt packet is skipped; the decoder resynchronises on the next one.
            Ok(Err(_)) | Err(_) => {
                self.audio_failures += 1;
                if self.audio_failures == AUDIO_FAILURES_BEFORE_NOTE {
                    shared.note("audio cannot be decoded".into());
                }
                return;
            }
        };
        self.audio_failures = 0;
        trim_decoded(&mut self.scratch, channels, skip, limit);
        if self.scratch.is_empty() || channels == 0 || rate == 0 {
            return;
        }
        if self.out.as_ref().is_none_or(|o| o.source_format() != (rate, channels)) {
            self.out = None;
            let opened = if self.device_reopens > MAX_DEVICE_REOPENS {
                Err("audio device keeps failing".to_string())
            } else {
                AudioOut::open(shared.audio.clone(), rate, channels)
            };
            match opened {
                Ok(out) => self.out = Some(out),
                Err(e) => {
                    self.adec = None;
                    self.pending_a.clear();
                    if self.vtrack.is_none() {
                        // Nothing would be left to play.
                        self.eof = true;
                        self.finished = true;
                        shared.fail(e);
                        return;
                    }
                    // Keep playing the picture against the wall clock.
                    self.on_wall_clock = true;
                    shared.restart_clock(shared.position(), Some(false));
                    shared.update_status(|st| st.has_audio = false);
                    shared.note(e);
                    return;
                }
            }
            if self.on_wall_clock {
                // Sound has appeared after the clock had to start without it: hand the clock
                // back to the device, continuing from where the wall clock stands.
                let position = shared.position();
                self.on_wall_clock = false;
                shared.audio.reset();
                shared.restart_clock(position, Some(true));
                self.audio_resume = position;
                self.align_pending = true;
            }
        }
        // A jump in the audio timestamps (a hole in the track, or sound resuming after the
        // clock ran on without it) must not be played back to back with what came before:
        // line the packet up with where the audio output currently stands.
        if self.seekable && !self.align_pending && self.next_audio_pts.is_some_and(|next| (pts - next).abs() > 0.05) {
            self.audio_resume = shared.position() + shared.audio.buffered_secs();
            self.align_pending = true;
        }
        self.next_audio_pts = Some(pts + (self.scratch.len() / channels) as f64 / rate as f64);
        let Some(out) = self.out.as_mut() else { return };
        let mut samples: &[f32] = &self.scratch;
        if self.align_pending {
            self.align_pending = false;
            if !self.seekable {
                // A stream starts wherever its timestamps happen to be.
                shared.set_clock_base(pts);
            } else {
                match align_audio(self.audio_resume, pts, samples.len() / channels, rate) {
                    AudioAlign::Drop => {
                        self.align_pending = true;
                        return;
                    }
                    AudioAlign::Play { skip, pad } => {
                        samples = &samples[skip * channels..];
                        if pad > 0 {
                            out.write(&vec![0.0; pad * channels]);
                        }
                    }
                }
            }
        }
        out.write(samples);
        self.audio_written = true;
        if !(self.eof && self.pending_a.is_empty()) {
            shared.audio.set_free_run(false);
        }
    }

    fn seek(&mut self, shared: &Arc<Shared>, target: f64) {
        if !self.seekable {
            return;
        }
        let end = self.info.duration.unwrap_or(f64::MAX);
        let target = if target.is_finite() { target.clamp(0.0, (end - 0.25).max(0.0)) } else { 0.0 };
        self.pending_v.clear();
        self.pending_a.clear();
        self.tail.clear();
        self.wait_key = None;
        let has_video = self.vtrack.is_some();
        let sought = if has_video { self.seek_video(target) } else { self.demux.seek(target).map(|_| target) };
        // The demuxer has moved even when the seek failed part-way, so playback state is
        // rebuilt either way; a failed seek then simply runs into the end of the media.
        let (resume, failed) = match sought {
            Ok(r) => (r, false),
            Err(e) => {
                shared.note(e);
                (target, true)
            }
        };
        // Decoder state refers to the pictures and samples before the jump.
        if let Some(track) = self.vtrack.as_ref() {
            self.vdec = vdec::make_decoder(track).ok();
        }
        if let Some(ADec::Sym(dec)) = self.adec.as_mut() {
            dec.reset();
        }
        if let Some(out) = self.out.as_mut() {
            out.reset();
        }
        shared.clear_frames();
        shared.audio.reset();
        shared.audio.set_free_run(false);
        self.on_wall_clock = self.adec.is_none();
        shared.restart_clock(resume, Some(self.adec.is_some()));
        self.resume_at = resume;
        self.audio_resume = resume;
        self.align_pending = true;
        self.next_audio_pts = None;
        self.eof = failed;
        if failed {
            self.vdec = None;
        }
        shared.force_show.store(true, Ordering::Relaxed);
        // Seeking away from the end resumes playback; a seek while paused stays paused.
        if std::mem::take(&mut self.finished) {
            shared.set_paused(false);
        }
        shared.update_status(|st| {
            if st.state == State::Ended {
                st.state = State::Playing;
            }
        });
    }

    /// Positions the demuxer on the last keyframe at or before `target` and returns the time
    /// playback should resume from.
    ///
    /// Containers do not tell us where keyframes are, so this looks progressively further
    /// back until it finds one. Searching backward rather than forward is what makes a small
    /// step back always land earlier than the current position.
    fn seek_video(&mut self, target: f64) -> Result<f64, String> {
        let track = self.vtrack.clone().ok_or("no video track")?;
        for back in [0.0, 1.0, 3.0, 8.0, 20.0, 60.0] {
            let start = (target - back).max(0.0);
            self.demux.seek(start)?;
            let mut key = None;
            while let Some(packet) = self.demux.next_packet()? {
                if let Packet::Video { pts, data } = packet {
                    if pts > target + 1e-3 {
                        break;
                    }
                    if vdec::is_keyframe(&track, &data) {
                        key = Some(pts);
                    }
                }
            }
            if let Some(k) = key {
                self.demux.seek(k)?;
                self.wait_key = Some(k);
                return Ok(if target - k <= PRECISE_SEEK_WINDOW { target } else { k });
            }
            if start == 0.0 {
                break;
            }
        }
        // No keyframe behind the target within reach: continue from the next one ahead.
        self.demux.seek(target)?;
        self.wait_key = Some(f64::NEG_INFINITY);
        Ok(target)
    }
}
