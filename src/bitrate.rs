//! The bitrate of what is playing: compressed bytes per second over the last few seconds of
//! the media, measured from the packets the demuxer hands out.
//!
//! Packets are timed by their presentation time, not by when they were read: the reader runs
//! ahead in bursts while buffering, which would make a wall-clock rate jump about. A record is
//! a few bytes and the store is capped, so the demuxing thread pays one short push per packet.

use std::collections::VecDeque;

/// The averaging window, in seconds of media.
pub const WINDOW: f64 = 5.0;
/// Below this much covered media (just after opening or seeking) there is no figure yet.
pub const MIN_SPAN: f64 = 0.5;
/// Records further than this behind the newest are dropped.
pub const KEEP: f64 = 90.0;
pub const MAX_RECORDS: usize = 20_000;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rate {
    /// Bits per second, picture and sound together.
    pub total: f64,
    pub video: f64,
    pub audio: f64,
}

#[derive(Debug, Default)]
pub struct Meter {
    records: VecDeque<(f64, u32, bool)>,
    newest: f64,
}

impl Meter {
    pub fn record(&mut self, pts: f64, bytes: usize, video: bool) {
        if !pts.is_finite() {
            return;
        }
        self.newest = if self.records.is_empty() { pts } else { self.newest.max(pts) };
        self.records.push_back((pts, bytes.min(u32::MAX as usize) as u32, video));
        let oldest_kept = self.newest - KEEP;
        while self.records.len() > MAX_RECORDS || self.records.front().is_some_and(|r| r.0 < oldest_kept) {
            self.records.pop_front();
        }
    }

    /// The latest presentation time recorded.
    pub fn newest(&self) -> Option<f64> {
        (!self.records.is_empty()).then_some(self.newest)
    }

    pub fn clear(&mut self) {
        self.records.clear();
        self.newest = 0.0;
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The rate over the [`WINDOW`] of media ending at `position`, or over the part of it the
    /// records cover (from the earliest one) when that is shorter. `None` under [`MIN_SPAN`].
    ///
    /// When the clock has run past everything read (after the last packet of the media, while
    /// the clock carries on without sound for the pictures that remain), the window ends at
    /// the newest record instead, where an empty window would read 0.
    pub fn rate(&self, position: f64) -> Option<Rate> {
        if !position.is_finite() {
            return None;
        }
        let starved = !self.records.is_empty() && position > self.newest;
        let end = if starved { self.newest } else { position };
        let from = end - WINDOW;
        let earliest = self.records.iter().map(|r| r.0).fold(f64::INFINITY, f64::min);
        let span = end - from.max(earliest);
        if !(span >= MIN_SPAN) {
            return None;
        }
        let (mut video, mut audio) = (0u64, 0u64);
        for &(pts, bytes, is_video) in &self.records {
            if pts >= from && (pts < end || starved && pts <= end) {
                if is_video { video += bytes as u64 } else { audio += bytes as u64 }
            }
        }
        let per_sec = |bytes: u64| bytes as f64 * 8.0 / span;
        Some(Rate { total: per_sec(video + audio), video: per_sec(video), audio: per_sec(audio) })
    }
}

/// "128 kb/s", "2.41 Mb/s".
pub fn format(bits_per_sec: f64) -> String {
    if !bits_per_sec.is_finite() || bits_per_sec <= 0.0 {
        return "0 kb/s".into();
    }
    if bits_per_sec >= 999_500.0 {
        format!("{:.2} Mb/s", bits_per_sec / 1_000_000.0)
    } else {
        format!("{:.0} kb/s", bits_per_sec / 1000.0)
    }
}

/// How often a new figure is taken from the meter.
pub const SAMPLE_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

/// What the bitrate label shows: sampled from the meter every [`SAMPLE_EVERY`], eased toward
/// the sample on every frame, held while paused.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Display {
    pub shown: f64,
    pub rate: Rate,
    sampled: std::time::Instant,
    drawn: std::time::Instant,
}

impl Display {
    /// The next state at `now`. `sample` is asked for a new figure only when one is due and not
    /// paused; its `None` (too little measured) clears the label.
    pub fn step(
        state: Option<Display>,
        now: std::time::Instant,
        paused: bool,
        sample: impl FnOnce() -> Option<Rate>,
    ) -> Option<Display> {
        let due = state.is_none_or(|s| now.saturating_duration_since(s.sampled) >= SAMPLE_EVERY);
        let mut state = state;
        if due && !paused {
            state = sample().map(|rate| match state {
                Some(s) => Display { rate, sampled: now, ..s },
                None => Display { shown: rate.total, rate, sampled: now, drawn: now },
            });
        }
        state.map(|s| {
            let shown = if paused { s.shown } else { ease(s.shown, s.rate.total, now.saturating_duration_since(s.drawn).as_secs_f64()) };
            Display { shown, drawn: now, ..s }
        })
    }

    /// Still moving toward the sample, so another frame is worth drawing.
    pub fn settling(&self) -> bool {
        (self.shown - self.rate.total).abs() > self.rate.total * 0.005
    }
}

/// How long the shown figure takes to cover about two thirds of the way to a new value.
pub const EASE_SECS: f64 = 0.6;

/// The shown figure `dt` seconds later, moving smoothly toward `target`.
pub fn ease(shown: f64, target: f64, dt: f64) -> f64 {
    if !shown.is_finite() || !(dt > 0.0) {
        return target;
    }
    let k = 1.0 - (-dt / EASE_SECS).exp();
    shown + (target - shown) * k
}
