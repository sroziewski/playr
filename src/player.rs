//! The playback pipeline: one thread that demuxes, decodes and keeps the audio buffer and the
//! video frame queue topped up, plus the state the UI reads.

use std::collections::VecDeque;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::registry::CodecRegistry;

use crate::audio_out::{AudioOut, AudioShared};
use crate::bitrate::{Meter, Rate};
use crate::cookies::{self, CookieJar};
use crate::demux::{self, Demuxer};
use crate::history::{self, History};
use crate::hls;
use crate::lists;
use crate::media::{AudioPacket, AudioTrack, MediaInfo, Packet, VideoFrame, VideoTrack, trim_decoded};
use crate::net;
use crate::podcast;
use crate::sites::{self, Page};
use crate::spotify::{self, Playlist};
use crate::vdec::{self, VideoDecoder};
use crate::youtube::{self, Choice, Video};

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
/// How long the last picture of a list entry waits to be drawn before the next entry opens.
const LAST_PICTURE_GRACE: Duration = Duration::from_millis(250);
const YOUTUBE_CACHE_FOR: Duration = Duration::from_secs(3600);
pub const MIN_SPEED: f64 = 0.25;
pub const MAX_SPEED: f64 = 2.0;

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
    pub youtube: Option<Arc<Video>>,
    pub page: Option<Arc<Page>>,
    pub choice: Choice,
    pub playlist: Option<Arc<Playlist>>,
    pub entry: usize,
}

struct Clock {
    base: f64,
    use_audio: bool,
    wall_accum: f64,
    wall_anchor: Option<Instant>,
    speed: f64,
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
    youtube_cache: Mutex<Option<(String, Instant, Arc<Video>)>>,
    audio_only: AtomicBool,
    list_cache: Mutex<Option<(String, Instant, Option<Arc<Playlist>>)>>,
    spotify_next: Mutex<Option<Prefetched>>,
    skipped: Mutex<Vec<String>>,
    cookies: Mutex<Option<(PathBuf, Arc<CookieJar>)>>,
    history: Mutex<(History, Option<PathBuf>)>,
    bitrate: Mutex<Meter>,
    page_cache: Mutex<Option<(String, Instant, Arc<Page>)>>,
    repaint: Box<dyn Fn() + Send + Sync>,
}

impl Status {
    /// Marks playing as failed with `error`, explained as a block when a web filter caused it.
    fn set_failed(&mut self, error: String) {
        self.state = State::Failed;
        self.error = Some(crate::net::blocked_by_network(&error).unwrap_or(error));
    }
}

impl Shared {
    fn new(repaint: Box<dyn Fn() + Send + Sync>) -> Self {
        Self {
            audio: Arc::new(AudioShared::default()),
            frames: Mutex::new(VecDeque::new()),
            status: Mutex::new(Status::default()),
            clock: Mutex::new(Clock { base: 0.0, use_audio: false, wall_accum: 0.0, wall_anchor: None, speed: 1.0 }),
            force_show: AtomicBool::new(false),
            abort: Mutex::new(Arc::new(AtomicBool::new(false))),
            youtube_cache: Mutex::new(None),
            audio_only: AtomicBool::new(false),
            list_cache: Mutex::new(None),
            spotify_next: Mutex::new(None),
            skipped: Mutex::new(Vec::new()),
            cookies: Mutex::new(None),
            history: Mutex::new((History::default(), None)),
            bitrate: Mutex::new(Meter::default()),
            page_cache: Mutex::new(None),
            repaint,
        }
    }

    pub fn bitrate(&self) -> Option<Rate> {
        self.bitrate.lock().unwrap().rate(self.position())
    }

    /// Seconds of media read beyond the playing position; negative when the clock is past
    /// everything read.
    pub fn read_ahead(&self) -> Option<f64> {
        let newest = self.bitrate.lock().unwrap().newest()?;
        Some(newest - self.position())
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
        self.update_status(|st| st.set_failed(error));
    }

    /// Current playback position in seconds.
    pub fn position(&self) -> f64 {
        let c = self.clock.lock().unwrap();
        if c.use_audio {
            c.base + self.audio.heard_secs()
        } else {
            c.base + c.wall_accum + c.wall_anchor.map_or(0.0, |t| t.elapsed().as_secs_f64() * c.speed)
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
                c.wall_accum += t.elapsed().as_secs_f64() * c.speed;
            }
        } else if c.wall_anchor.is_none() {
            c.wall_anchor = Some(Instant::now());
        }
    }

    pub fn speed(&self) -> f64 {
        self.clock.lock().unwrap().speed
    }

    pub fn audio_only(&self) -> bool {
        self.audio_only.load(Ordering::Relaxed)
    }

    pub fn cookies_path(&self) -> Option<PathBuf> {
        self.cookies.lock().unwrap().as_ref().map(|(p, _)| p.clone())
    }

    fn cookie_jar(&self) -> Option<Arc<CookieJar>> {
        self.cookies.lock().unwrap().as_ref().map(|(_, j)| j.clone())
    }

    pub fn set_speed(&self, speed: f64) {
        let speed = if speed.is_finite() { speed.clamp(MIN_SPEED, MAX_SPEED) } else { 1.0 };
        let mut c = self.clock.lock().unwrap();
        if let Some(t) = c.wall_anchor {
            c.wall_accum += t.elapsed().as_secs_f64() * c.speed;
            c.wall_anchor = Some(Instant::now());
        }
        c.speed = speed;
        self.audio.set_speed(speed);
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

    /// Drops the queued audio and restarts the clock at `base` on the device, in one step: a
    /// position read in between would otherwise see the emptied device clock on the old base
    /// (the start of the file) while playback carries on.
    fn restart_clock_on_fresh_audio(&self, base: f64) {
        let paused = self.is_paused();
        let mut c = self.clock.lock().unwrap();
        self.audio.reset();
        c.base = base;
        c.use_audio = true;
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

    /// Drops queued pictures that a later queued picture has already replaced (both are due),
    /// keeping the newest due one for the screen. The UI does the same when it draws, but it
    /// does not draw while Windows withholds repaints from a covered window, or while a window
    /// is being dragged; without this the full queue would stop decoding, then reading, and
    /// the sound would run dry.
    pub fn drop_stale_frames(&self) {
        // Reading the clock takes the audio callback's lock; not every step needs to.
        if self.frames_len() < 2 {
            return;
        }
        let pos = self.position();
        let mut q = self.frames.lock().unwrap();
        while q.get(1).is_some_and(|f| f.pts <= pos + 0.002) {
            q.pop_front();
        }
    }

    /// Queued pictures not yet due. The end of the media is reached once none are left: a due
    /// picture is shown when the window next draws, which may not be soon (see
    /// [`drop_stale_frames`](Self::drop_stale_frames)).
    fn undue_frames(&self) -> usize {
        let pos = self.position();
        self.frames.lock().unwrap().iter().filter(|f| f.pts > pos + 0.002).count()
    }

    fn abort_current(&self) {
        self.abort.lock().unwrap().store(true, Ordering::Relaxed);
    }
}

enum Cmd {
    Open(String, Option<Choice>, Option<usize>),
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
        Self::with_history(repaint, None)
    }

    pub fn with_history(repaint: Box<dyn Fn() + Send + Sync>, history_file: Option<PathBuf>) -> Self {
        let shared = Arc::new(Shared::new(repaint));
        if let Some(path) = history_file {
            *shared.history.lock().unwrap() = (History::load(&path), Some(path));
        }
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
        let _ = self.tx.send(Cmd::Open(target.into(), None, None));
    }

    pub fn history(&self) -> Vec<history::Item> {
        self.shared.history.lock().unwrap().0.items.clone()
    }

    pub fn clear_history(&self) -> Result<(), String> {
        let mut guard = self.shared.history.lock().unwrap();
        guard.0.clear();
        match &guard.1 {
            Some(path) => guard.0.save(path),
            None => Ok(()),
        }
    }

    pub fn open_youtube(&self, target: impl Into<String>, choice: Choice) {
        self.shared.abort_current();
        let _ = self.tx.send(Cmd::Open(target.into(), Some(choice), None));
    }

    pub fn open_youtube_at(&self, target: impl Into<String>, entry: Option<usize>, choice: Choice) {
        self.shared.abort_current();
        let _ = self.tx.send(Cmd::Open(target.into(), Some(choice), entry));
    }

    pub fn open_entry(&self, target: impl Into<String>, index: usize) {
        self.shared.abort_current();
        let _ = self.tx.send(Cmd::Open(target.into(), None, Some(index)));
    }

    pub fn set_cookies(&self, path: Option<PathBuf>) -> Result<usize, String> {
        let Some(path) = path else {
            *self.shared.cookies.lock().unwrap() = None;
            let forgotten = cookies::save_path(None);
            let removed = cookies::remove_ytdlp_copy();
            return forgotten.and(removed).map(|_| 0);
        };
        let jar = cookies::load(&path)?;
        let count = jar.cookies.len();
        *self.shared.cookies.lock().unwrap() = Some((path.clone(), Arc::new(jar)));
        cookies::save_path(Some(&path))?;
        Ok(count)
    }

    pub fn set_speed(&self, speed: f64) {
        self.shared.set_speed(speed);
    }

    pub fn set_audio_only(&self, on: bool) {
        self.shared.audio_only.store(on, Ordering::Relaxed);
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
                        Ok(false) if s.finished && s.advance.is_some() && s.last_picture_had_its_chance(&shared) => {
                            if let Some(e) = shared.status().error.filter(|_| shared.status().state == State::Failed) {
                                shared.skipped.lock().unwrap().push(format!("the previous entry broke off: {e}"));
                            }
                            s.advance.take().map(|(target, index)| Cmd::Open(target, None, Some(index)))
                        }
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
            Some(Cmd::Open(mut target, mut choice, mut entry)) => {
                // Opening can block on the network for many seconds; if the user has already
                // asked for something else, do not make them wait for this one first.
                // A seek belongs to whatever was opened before it, so only one that follows
                // the newest open survives.
                let mut seek_after = None;
                loop {
                    match rx.try_recv() {
                        Ok(Cmd::Open(newer, newer_choice, newer_entry)) => {
                            target = newer;
                            choice = newer_choice;
                            entry = newer_entry;
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
                let opened = catch_unwind(AssertUnwindSafe(|| Session::open(&shared, &target, choice.as_ref(), entry)));
                let failed = !matches!(opened, Ok(Ok(_)));
                let failed_title = spotify_entry_label(&shared, &target, entry).unwrap_or_else(|| shared.status().title);
                match opened {
                    Ok(Ok(s)) => {
                        session = Some(s);
                        record_history(&shared, &target);
                    }
                    Ok(Err(e)) => shared.fail(e),
                    Err(_) => shared.fail("cannot open: internal error while reading the file".into()),
                }
                let aborted = shared.abort.lock().unwrap().load(Ordering::Relaxed);
                if failed
                    && !aborted
                    && !matches!(queued, Some(Cmd::Stop | Cmd::Quit))
                    && let Some(next) = next_entry(&shared, &target, entry)
                {
                    let why = shared.status().error.unwrap_or_default();
                    shared.skipped.lock().unwrap().push(format!("skipped {}: {why}", failed_title.trim_end_matches('…')));
                    queued = Some(Cmd::Open(target.clone(), None, Some(next)));
                } else if !failed {
                    shared.skipped.lock().unwrap().clear();
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
    shared.bitrate.lock().unwrap().clear();
    shared.audio.reset();
    shared.audio.set_free_run(false);
    shared.audio.take_device_error();
    shared.set_paused(false);
    shared.restart_clock(0.0, Some(false));
    shared.update_status(|st| {
        *st = Status { state: State::Loading, source: target.to_string(), title: display_name(target), ..Default::default() };
    });
}

fn record_history(shared: &Shared, target: &str) {
    let status = shared.status();
    let title = status.playlist.as_ref().map_or(status.title, |list| list.name.clone());
    let mut guard = shared.history.lock().unwrap();
    guard.0.record(target, &title, cookies::now_unix());
    if let Some(path) = &guard.1 {
        let _ = guard.0.save(path);
    }
}

fn display_name(target: &str) -> String {
    if net::is_url(target) {
        return target.to_string();
    }
    Path::new(target).file_name().and_then(|n| n.to_str()).unwrap_or(target).to_string()
}

const UNENCRYPTED_NOTE: &str = "\u{1f513} unencrypted connection (http): the network can see what is playing";

type Prefetched = (String, usize, Instant, Arc<Video>);

fn cached_list(shared: &Shared, target: &str) -> Option<Arc<Playlist>> {
    let cache = shared.list_cache.lock().unwrap();
    let (url, _, list) = cache.as_ref()?;
    (url == target.trim()).then(|| list.clone())?
}

fn spotify_entry_label(shared: &Shared, target: &str, entry: Option<usize>) -> Option<String> {
    let list = cached_list(shared, target)?;
    list.entries.get(entry.unwrap_or(list.start)).map(|e| e.label())
}

fn next_entry(shared: &Shared, target: &str, entry: Option<usize>) -> Option<usize> {
    let list = cached_list(shared, target)?;
    let next = entry.unwrap_or(list.start) + 1;
    (next < list.entries.len()).then_some(next)
}

fn is_list_target(target: &str) -> bool {
    spotify::is_spotify_url(target)
        || youtube::is_youtube_list_url(target)
        || net::is_playlist_name(target) && crate::radio::find_by_url(target).is_none()
}

enum ListRead {
    List(Playlist),
    NotAList,
    Unread,
}

fn read_list(shared: &Arc<Shared>, target: &str, abort: &Arc<AtomicBool>) -> Result<ListRead, String> {
    let found = |list: Option<Playlist>| list.map_or(ListRead::NotAList, ListRead::List);
    if spotify::is_spotify_url(target) {
        shared.update_status(|st| st.title = "Reading the Spotify list…".to_string());
        return spotify::fetch(target, shared.cookie_jar().as_deref()).map(|l| ListRead::List(l));
    }
    if youtube::is_youtube_list_url(target) {
        let progress = |message: &str| shared.update_status(|st| st.title = message.to_string());
        return match youtube::playlist(target, abort, &progress) {
            Err(_) if youtube::plays_video_when_list_unread(target, abort.load(Ordering::Relaxed)) => Ok(ListRead::Unread),
            other => other.map(found),
        };
    }
    let (entries, single_counts) = if net::is_url(target) {
        let text = crate::dash::get_text(target, &[], 256 * 1024)?;
        let entries = lists::parse_remote(&text, target);
        let only_relative = net::parse_playlist(&text).ok().flatten().is_none();
        (entries, only_relative)
    } else {
        let mut raw = Vec::new();
        std::fs::File::open(target)
            .and_then(|f| f.take(256 * 1024).read_to_end(&mut raw))
            .map_err(|e| format!("cannot read {target}: {e}"))?;
        let entries = lists::parse(&String::from_utf8_lossy(&raw), Path::new(target).parent());
        let local = entries.first().and_then(|e| e.audio.as_deref()).is_some_and(|a| !net::is_url(a));
        (entries, local)
    };
    if entries.is_empty() || entries.len() == 1 && !single_counts {
        return Ok(ListRead::NotAList);
    }
    Ok(ListRead::List(Playlist { name: lists::list_name(target), entries, start: 0 }))
}

fn resolve_list(
    shared: &Arc<Shared>,
    target: &str,
    entry: Option<usize>,
    abort: &Arc<AtomicBool>,
) -> Result<Option<Arc<Playlist>>, String> {
    if !is_list_target(target) {
        return Ok(None);
    }
    if entry.is_some()
        && let Some((cached, at, list)) = shared.list_cache.lock().unwrap().as_ref()
        && cached == target
        && at.elapsed() < YOUTUBE_CACHE_FOR
    {
        return Ok(list.clone());
    }
    let list = match read_list(shared, target, abort)? {
        ListRead::List(list) => Some(Arc::new(list)),
        ListRead::NotAList => None,
        ListRead::Unread => return Ok(None),
    };
    *shared.list_cache.lock().unwrap() = Some((target.to_string(), Instant::now(), list.clone()));
    Ok(list)
}

fn prefetch_entry(shared: &Arc<Shared>, target: String, list: Arc<Playlist>, index: usize, abort: Arc<AtomicBool>) {
    let Some(item) = list.entries.get(index).cloned().filter(|e| e.audio.is_none()) else { return };
    let shared = shared.clone();
    std::thread::spawn(move || {
        let quiet = |_: &str| {};
        let Ok(found) = youtube::search(&item.queries(), item.duration, &abort, &quiet) else { return };
        let Ok(video) = youtube::resolve(&found, ytdlp_cookies(&shared).as_deref(), &abort, &quiet) else { return };
        *shared.spotify_next.lock().unwrap() = Some((target, index, Instant::now(), Arc::new(video)));
    });
}

fn resolve_page(shared: &Arc<Shared>, url: &str, abort: &Arc<AtomicBool>) -> Result<Arc<Page>, String> {
    if let Some((cached, at, page)) = shared.page_cache.lock().unwrap().as_ref()
        && cached == url
        && sites::cache_fresh(at.elapsed())
    {
        return Ok(page.clone());
    }
    let asked = sites::ytdlp_address(url);
    let progress = |message: &str| shared.update_status(|st| st.title = message.to_string());
    let page = sites::parse_page(&youtube::page_info(&asked, abort, &progress)?)?;
    if let Some(why) = sites::unplayable_reason(&page) {
        return Err(why);
    }
    let page = Arc::new(page);
    *shared.page_cache.lock().unwrap() = Some((url.to_string(), Instant::now(), page.clone()));
    Ok(page)
}

struct PageOpened {
    demuxer: Box<dyn Demuxer>,
    secure: bool,
    page: Arc<Page>,
    choice: Choice,
    finite: bool,
}

/// Opens a web page's video through yt-dlp: the chosen quality, or its sound alone when
/// [`Shared::audio_only`] is on.
fn open_page(shared: &Arc<Shared>, url: &str, choice: Option<&Choice>, abort: &Arc<AtomicBool>) -> Result<PageOpened, String> {
    let page = resolve_page(shared, url, abort)?;
    let sound = shared.audio_only().then(|| page.sound_format()).flatten();
    let sound_only = sound.is_some();
    let (format, (demuxer, secure)) = match sound {
        Some(format) => {
            let opened = sites::open_sound(&page, &format, abort.clone())?;
            (format, opened)
        }
        None => {
            let format = page.pick(choice.and_then(|c| c.video.as_deref())).cloned().ok_or("the page has no video")?;
            let opened = sites::open(&page, &format, abort.clone())?;
            (format, opened)
        }
    };
    let finite = !format.hls || demuxer.info().duration.is_some();
    let choice = Choice { video: (!sound_only).then(|| format.id.clone()), audio: None };
    Ok(PageOpened { demuxer, secure, page, choice, finite })
}

fn ytdlp_cookies(shared: &Shared) -> Option<PathBuf> {
    cookies::ytdlp_copy(&*shared.cookie_jar()?)
}

fn resolve_youtube(shared: &Arc<Shared>, url: &str, abort: &Arc<AtomicBool>) -> Result<Arc<Video>, String> {
    let id = youtube::video_id(url).unwrap_or_default();
    if let Some((cached, at, video)) = shared.youtube_cache.lock().unwrap().as_ref()
        && *cached == id
        && at.elapsed() < YOUTUBE_CACHE_FOR
    {
        return Ok(video.clone());
    }
    let progress = |message: &str| shared.update_status(|st| st.title = message.to_string());
    let video = Arc::new(youtube::resolve(url, ytdlp_cookies(shared).as_deref(), abort, &progress)?);
    *shared.youtube_cache.lock().unwrap() = Some((id, Instant::now(), video.clone()));
    Ok(video)
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
    advance: Option<(String, usize)>,
    /// The note that something came over plain HTTP is shown.
    told_unencrypted: bool,
    finished_at: Option<Instant>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.abort.store(true, Ordering::Relaxed);
    }
}

impl Session {
    fn open(shared: &Arc<Shared>, target: &str, choice: Option<&Choice>, entry: Option<usize>) -> Result<Self, String> {
        let abort = Arc::new(AtomicBool::new(false));
        *shared.abort.lock().unwrap() = abort.clone();
        let mut target = target.trim().to_string();
        let list = resolve_list(shared, &target, entry, &abort)?;
        let mut local_hls = None;
        if list.is_none() && !net::is_url(&target) && net::is_playlist_name(&target) {
            let mut text = Vec::new();
            std::fs::File::open(&target)
                .and_then(|f| f.take(8 << 20).read_to_end(&mut text))
                .map_err(|e| format!("cannot read {target}: {e}"))?;
            let text = String::from_utf8_lossy(&text).into_owned();
            if hls::is_hls(&text) {
                local_hls = Some(text);
            } else {
                target = net::parse_playlist(&text)?.ok_or("playlist contains no stream address")?;
            }
        }

        let mut title = display_name(&target);
        let mut live = net::is_url(&target);
        let mut finite = false;
        let mut video_info = None;
        let mut chosen = Choice::default();
        let mut playlist = None;
        let mut stream_finite = None;
        let mut page_info: Option<Arc<Page>> = None;
        let mut unencrypted = false;
        let demux: Box<dyn Demuxer> = if let Some(list) = list {
            let index = entry.unwrap_or(list.start).min(list.entries.len().saturating_sub(1));
            let item = list.entries.get(index).cloned().ok_or("this list is empty")?;
            shared.update_status(|st| {
                st.title = match item.audio {
                    Some(_) => format!("Opening \u{201c}{}\u{201d}…", item.label()),
                    None => format!("Finding \u{201c}{}\u{201d} on YouTube…", item.label()),
                };
                st.playlist = Some(list.clone());
                st.entry = index;
            });
            let prefetched = shared
                .spotify_next
                .lock()
                .unwrap()
                .take()
                .filter(|(url, i, at, _)| *url == target && *i == index && at.elapsed() < YOUTUBE_CACHE_FOR)
                .map(|(_, _, _, video)| video);
            let demuxer = match &item.audio {
                Some(link) if youtube::is_youtube_url(link) => {
                    let video = resolve_youtube(shared, link, &abort)?;
                    let opened =
                        youtube::open(&video, choice.unwrap_or(&Choice::default()), shared.audio_only(), abort.clone())?;
                    chosen = opened.choice;
                    video_info = Some(video);
                    opened.demuxer
                }
                Some(link) if net::is_url(link) && spotify::is_spotify_url(&target) => {
                    let opened = podcast::open_link(link, abort.clone(), Box::new(|_| {}))?;
                    unencrypted = !opened.secure;
                    opened.demuxer
                }
                Some(link) if net::is_url(link) => {
                    let cb_shared = shared.clone();
                    let on_title = Box::new(move |t| cb_shared.update_status(|st| st.now_playing = Some(t)));
                    let opened = podcast::open_link(link, abort.clone(), on_title)?;
                    unencrypted = !opened.secure;
                    if !opened.ranged {
                        live = true;
                        stream_finite = Some(opened.finite);
                    }
                    opened.demuxer
                }
                Some(path) => demux::open_path(Path::new(path))?,
                None => {
                    let video = match prefetched {
                        Some(video) => video,
                        None => {
                            let progress = |message: &str| shared.update_status(|st| st.title = message.to_string());
                            let found = youtube::search(&item.queries(), item.duration, &abort, &progress)?;
                            resolve_youtube(shared, &found, &abort)?
                        }
                    };
                    youtube::open(&video, &Choice::default(), true, abort.clone())?.demuxer
                }
            };
            title = video_info.as_ref().map_or_else(|| item.label(), |v| v.title.clone());
            finite = stream_finite.unwrap_or(true);
            playlist = Some((list, index));
            demuxer
        } else if youtube::is_youtube_url(&target) {
            let video = resolve_youtube(shared, &target, &abort)?;
            let audio_only = shared.audio_only();
            let opened = youtube::open(&video, choice.unwrap_or(&Choice::default()), audio_only, abort.clone())?;
            title = video.title.clone();
            finite = !video.live;
            chosen = opened.choice;
            video_info = Some(video);
            opened.demuxer
        } else if sites::is_pornhub_url(&target) {
            let opened = open_page(shared, &target, choice, &abort)?;
            unencrypted = !opened.secure;
            title = opened.page.title.clone();
            finite = opened.finite;
            chosen = opened.choice;
            page_info = Some(opened.page);
            opened.demuxer
        } else if live {
            let cb_shared = shared.clone();
            let mut stream = net::open_url(
                &target,
                abort.clone(),
                Box::new(move |t| cb_shared.update_status(|st| st.now_playing = Some(t))),
            )?;
            if let Some(text) = stream.hls.take() {
                let opened = hls::open(&hls::Location::Url(stream.url.clone()), Some(text), &[], false, abort.clone())?;
                unencrypted = !stream.secure || !opened.secure;
                if let Some(name) = stream.name {
                    title = name;
                }
                finite = !opened.live;
                opened.demuxer
            } else if sites::is_html(stream.mime.as_deref()) {
                let stream_secure = stream.secure;
                drop(stream);
                let opened = open_page(shared, &target, choice, &abort)?;
                unencrypted = !stream_secure || !opened.secure;
                title = opened.page.title.clone();
                finite = opened.finite;
                chosen = opened.choice;
                page_info = Some(opened.page);
                opened.demuxer
            } else {
                unencrypted = !stream.secure;
                if let Some(name) = stream.name {
                    title = name;
                }
                finite = stream.finite;
                demux::open_stream(stream.source, stream.ext.as_deref(), stream.mime.as_deref())?
            }
        } else if let Some(text) = local_hls {
            let opened = hls::open(&hls::Location::File(PathBuf::from(&target)), Some(text), &[], false, abort.clone())?;
            unencrypted = !opened.secure;
            finite = !opened.live;
            live = opened.live;
            opened.demuxer
        } else {
            demux::open_path(Path::new(&target))?
        };

        let info = demux.info().clone();
        let mut notes = info.notes.clone();
        let told_unencrypted = unencrypted || demux.unencrypted();
        if told_unencrypted {
            notes.push(UNENCRYPTED_NOTE.into());
        }
        notes.append(&mut shared.skipped.lock().unwrap());
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
        if !seekable {
            shared.set_speed(1.0);
        }
        let vtrack = info.video.clone().filter(|_| vdec.is_some());
        let mut summary = match (&video_info, &page_info) {
            (Some(_), _) => "YouTube".to_string(),
            (None, Some(page)) => page.site.clone(),
            (None, None) => info.container.clone(),
        };
        if let Some(t) = &vtrack {
            summary.push_str(&format!(" · {} {}×{}", t.codec.name(), t.width, t.height));
        } else if video_info.is_some() || page_info.is_some() {
            summary.push_str(" · sound only");
        }
        if let Some((list, index)) = &playlist {
            let at = format!("{} · {}/{}", list.name, index + 1, list.entries.len());
            summary = if spotify::is_spotify_url(&target) {
                let from = if list.entries[*index].audio.is_some() { "podcast feed" } else { "sound from YouTube" };
                format!("Spotify · {at} · {from}")
            } else if video_info.is_some() {
                format!("{summary} · playlist {at}")
            } else {
                format!("{summary} · {at}")
            };
        }
        let advance = playlist
            .as_ref()
            .filter(|(list, index)| index + 1 < list.entries.len())
            .map(|(_, index)| (target.clone(), index + 1));
        if let (Some((list, _)), Some((_, next))) = (&playlist, &advance) {
            prefetch_entry(shared, target.clone(), list.clone(), *next, abort.clone());
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
            st.youtube = video_info;
            st.page = page_info.clone();
            st.choice = chosen;
            st.playlist = playlist.as_ref().map(|(list, _)| list.clone());
            st.entry = playlist.as_ref().map_or(0, |(_, index)| *index);
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
            advance,
            told_unencrypted,
            finished_at: None,
        })
    }

    /// How long the pipeline thread may sleep when a step found nothing to do. Commands wake
    /// it at once; the timeout only bounds how late it notices that the UI resumed playback
    /// or consumed pictures.
    fn idle_wait(&self, shared: &Shared) -> Duration {
        if self.finished && self.advance.is_some() {
            Duration::from_millis(20)
        } else if self.finished {
            Duration::from_millis(500)
        } else if shared.is_paused() {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(4)
        }
    }

    /// Whether the next list entry may be opened, which clears the pictures: once the window
    /// has drawn the last one, or after [`LAST_PICTURE_GRACE`] (a covered window may not draw
    /// at all).
    fn last_picture_had_its_chance(&self, shared: &Shared) -> bool {
        shared.frames_len() == 0 || self.finished_at.is_some_and(|t| t.elapsed() >= LAST_PICTURE_GRACE)
    }

    /// Does one unit of work. Returns `false` when every buffer is full (or the media is
    /// finished), which tells the caller to sleep.
    fn step(&mut self, shared: &Arc<Shared>) -> bool {
        if self.out.is_some() && shared.audio.take_device_error() {
            self.on_device_lost(shared);
        }
        shared.drop_stale_frames();
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
        shared.restart_clock_on_fresh_audio(position);
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
                shared.bitrate.lock().unwrap().record(pts, data.len(), true);
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
                shared.bitrate.lock().unwrap().record(pts, packet.data.len(), false);
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
        if !self.told_unencrypted && self.demux.unencrypted() {
            self.told_unencrypted = true;
            shared.update_status(|st| st.notes.push(UNENCRYPTED_NOTE.into()));
        }
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
        if audio_done && shared.undue_frames() == 0 {
            self.finished = true;
            self.finished_at = Some(Instant::now());
            shared.set_paused(true);
            let lost = self.live && !self.finite;
            // A file that broke before anything could be shown or heard did not play.
            let nothing_played = self.frames_out == 0 && !self.audio_written;
            let broken = self.read_error.take().filter(|_| nothing_played);
            shared.update_status(|st| {
                if let Some(e) = broken {
                    st.set_failed(e);
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
                shared.restart_clock_on_fresh_audio(position);
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
        shared.bitrate.lock().unwrap().clear();
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
