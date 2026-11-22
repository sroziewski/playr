//! Playback speed without a change in pitch (WSOLA: waveform-similarity overlap-add).
//!
//! Output is built from short pieces of the input, one every `hop` output frames. Playing at
//! speed `s` means each piece starts `hop * s` input frames after the previous one, which on
//! its own would leave audible seams; so each piece's exact start is searched for, within a
//! few milliseconds of that nominal point, as the spot whose waveform best matches what would
//! naturally have followed the previous piece, and the two are cross-faded.
//!
//! Resampling would be simpler but raises the pitch with the speed, the way a tape does.

use std::collections::VecDeque;

/// Output produced per step, in milliseconds.
const HOP_MS: u32 = 12;
/// How far a piece's start may move from its nominal position, as a fraction of the hop.
const SEARCH_FRACTION: usize = 2;
/// Every how many frames the similarity search looks (a coarse search costs a quarter).
const SEARCH_STRIDE: usize = 2;

/// What one [`Stretcher::render`] call did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    /// Samples written to the front of the output buffer (whole frames).
    pub samples: usize,
    /// Input frames played: how far the media position advanced.
    pub media_frames: u64,
}

/// Streaming time-stretcher over interleaved samples. It reads from the front of the caller's
/// queue and removes input once nothing later can still refer back to it.
#[derive(Debug, Default)]
pub struct Stretcher {
    channels: usize,
    rate: u32,
    hop: usize,
    search: usize,
    /// Input frame (in queue coordinates) where the last piece started; `None` before the first.
    last_start: Option<usize>,
    /// Where the next piece nominally starts, in input frames from the front of the queue.
    nominal: f64,
    /// Frames of `nominal` already counted as played.
    counted: u64,
    /// Produced output not yet handed out.
    pending: VecDeque<f32>,
    /// Mono sums for the similarity search, kept to avoid allocating in the audio callback.
    scratch: Vec<f32>,
}

impl Stretcher {
    /// Whether a stretch is in progress (input is held, or output is waiting).
    pub fn is_active(&self) -> bool {
        self.last_start.is_some() || !self.pending.is_empty()
    }

    /// Input frames at the front of the queue that have already been played (kept only as
    /// context for the next search).
    pub fn played_frames_held(&self) -> usize {
        if self.is_active() { self.nominal.floor().max(0.0) as usize } else { 0 }
    }

    /// Forgets everything (after a seek or a flush of the queue).
    pub fn reset(&mut self) {
        self.last_start = None;
        self.nominal = 0.0;
        self.counted = 0;
        self.pending.clear();
    }

    fn configure(&mut self, rate: u32, channels: usize) {
        if self.rate != rate || self.channels != channels {
            self.reset();
            self.rate = rate.max(1);
            self.channels = channels.max(1);
            self.hop = ((rate as usize * HOP_MS as usize) / 1000).max(16);
            self.search = self.hop / SEARCH_FRACTION;
        }
    }

    /// Fills `out` from `input` at `speed` (input frames per output frame). With `speed` 1 it
    /// only hands out what is still pending and lets go of the input it holds, after which
    /// [`is_active`](Self::is_active) is false and the caller copies input directly again.
    /// When the input runs short (the end of the stream, or an underrun) it stops early and lets
    /// go of the input as well, so the caller plays what is left unstretched instead of holding
    /// it back forever.
    pub fn render(&mut self, input: &mut VecDeque<f32>, out: &mut [f32], speed: f64, rate: u32, channels: usize) -> Rendered {
        self.configure(rate, channels);
        let ch = self.channels;
        let mut done = Rendered::default();
        loop {
            let n = (out.len() - done.samples).min(self.pending.len()) / ch * ch;
            for (dst, src) in out[done.samples..done.samples + n].iter_mut().zip(self.pending.drain(..n)) {
                *dst = src;
            }
            done.samples += n;
            if done.samples + ch > out.len() {
                break;
            }
            if speed == 1.0 {
                self.release(input);
                break;
            }
            match self.step(input, speed) {
                Some(frames) => done.media_frames += frames,
                None => {
                    self.release(input);
                    break;
                }
            }
        }
        done
    }

    /// Drops the input that has been played and returns to pass-through.
    fn release(&mut self, input: &mut VecDeque<f32>) {
        if self.pending.is_empty() && self.last_start.is_some() {
            let played = (self.counted as usize).min(input.len() / self.channels);
            input.drain(..played * self.channels);
            self.reset();
        }
    }

    /// Produces one hop of output. Returns the input frames it advanced the media position by,
    /// or `None` when there is not enough input yet.
    fn step(&mut self, input: &mut VecDeque<f32>, speed: f64) -> Option<u64> {
        let ch = self.channels;
        let hop = self.hop;
        let available = input.len() / ch;
        let nominal = self.nominal.round() as usize;
        let frame = |i: usize, c: usize| input[i * ch + c];
        match self.last_start {
            None => {
                if available < nominal + hop {
                    return None;
                }
                for i in nominal..nominal + hop {
                    self.pending.extend((0..ch).map(|c| frame(i, c)));
                }
                self.last_start = Some(nominal);
            }
            Some(last) => {
                let target = last + hop;
                let lowest = nominal.saturating_sub(self.search);
                let highest = nominal + self.search;
                if available < (highest + hop).max(target + hop) {
                    return None;
                }
                let mono = |i: usize| (0..ch).map(|c| frame(i, c)).sum::<f32>();
                let window = highest + hop - lowest;
                self.scratch.clear();
                self.scratch.extend((0..hop).map(|i| mono(target + i)));
                self.scratch.extend((0..window).map(|i| mono(lowest + i)));
                let (wanted, candidates) = self.scratch.split_at(hop);
                let mut best = (f32::NEG_INFINITY, nominal);
                for start in (lowest..=highest).step_by(SEARCH_STRIDE) {
                    let (mut dot, mut energy) = (0.0f32, 1e-9f32);
                    let here = &candidates[start - lowest..];
                    for i in (0..hop).step_by(SEARCH_STRIDE) {
                        let x = here[i];
                        dot += x * wanted[i];
                        energy += x * x;
                    }
                    let score = dot / energy.sqrt();
                    if score > best.0 {
                        best = (score, start);
                    }
                }
                let start = best.1;
                for i in 0..hop {
                    let fade = (i as f32 + 0.5) / hop as f32;
                    self.pending.extend((0..ch).map(|c| frame(target + i, c) * (1.0 - fade) + frame(start + i, c) * fade));
                }
                self.last_start = Some(start);
            }
        }
        self.nominal += hop as f64 * speed;
        let played = (self.nominal.floor().max(0.0) as u64).min(available as u64);
        let advanced = played.saturating_sub(self.counted);
        self.counted = played;
        // Input before both the last piece and the next search window is never read again.
        let keep_from = self.last_start.unwrap_or(0).min((self.nominal.floor() as usize).saturating_sub(self.search));
        if keep_from > 0 {
            input.drain(..keep_from * ch);
            self.last_start = self.last_start.map(|s| s - keep_from);
            self.nominal -= keep_from as f64;
            self.counted -= keep_from as u64;
        }
        Some(advanced)
    }
}
