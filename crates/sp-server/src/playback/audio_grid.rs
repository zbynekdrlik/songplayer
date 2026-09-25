//! Media-time audio grid buffer (#148).
//!
//! A planar-float FIFO between the paced decode and the wall-clock grid: the
//! pacer pushes every decoded frame's audio, and each productive grid boundary
//! takes exactly `samples_per_boundary` (1600 @ 48 kHz / 30 fps) samples.
//!
//! **Media-time head (#148 design v2).** The first TIMED push after a
//! [`clear`](AudioGridBuffer::clear) fixes the media sample index of the head
//! (the chunk's 0-based media time, minus anything already buffered); the
//! decoded audio is contiguous, so every later sample's media time follows
//! from the count. The pacer uses the head to pin the audio to the picture:
//!
//! - [`align_to`](AudioGridBuffer::align_to) — the hard alignment at start /
//!   seek: early audio is DROPPED, late audio is PADDED with leading silence;
//! - [`take_block`](AudioGridBuffer::take_block) with the `extra` from
//!   [`correction_for`] — the continuous correction: past 5 ms of error at most
//!   48 samples per block are dropped or inserted, spread over the block by
//!   linear interpolation (no click), until the error is ≤ 1 ms.
//!
//! There is no level target: how deep the buffer runs has no bearing on what
//! plays (the removed #148 PLL level trim steered toward 3200 samples and moved
//! the A/V offset with it).
//!
//! Underrun → the available samples are played, the missing tail is
//! zero-filled and `underruns` is bumped; the head advances only by the real
//! samples consumed, so the deficit shows up as an A/V error the correction
//! then removes. Overflow past a hard 2 s cap → the oldest audio is dropped
//! (the head advances with it), `overflows` is bumped, and a one-per-song WARN
//! is armed (`take_overflow_warning`).
//!
//! Pure: no clock calls, no I/O. Fully unit-tested on Linux CI.

use std::collections::VecDeque;

use sp_core::genlock::UNITS_PER_SECOND;

/// |A/V error| above which the continuous correction engages: 5 ms @ 48 kHz.
pub const AV_CORRECT_START_SAMPLES: i64 = 240;

/// |A/V error| at or below which an engaged correction stops: 1 ms @ 48 kHz.
pub const AV_CORRECT_STOP_SAMPLES: i64 = 48;

/// Most samples dropped or inserted in one boundary block: 1 ms @ 48 kHz.
pub const AV_CORRECT_MAX_PER_BLOCK: i64 = 48;

/// A 100-ns duration in samples at `rate_hz`, rounded to the nearest sample
/// (ties up): `round(d · rate / 1e7)`. Exact for whole milliseconds at 48 kHz
/// (the decoder's media timestamps are integer ms).
pub fn samples_from_100ns(d_100ns: i64, rate_hz: u32) -> i64 {
    (d_100ns * rate_hz as i64 * 2 + UNITS_PER_SECOND).div_euclid(2 * UNITS_PER_SECOND)
}

/// The continuous-correction decision for one boundary block (#148 design
/// v2). `err` = head − expected media time, in samples (POSITIVE = the audio
/// is AHEAD of the picture). An idle controller engages when `|err|` exceeds
/// 5 ms; an engaged one keeps going while `|err|` exceeds 1 ms. Returns
/// `(extra, engaged)`: `extra` input samples are consumed on top of the block
/// (positive = drop, the audio catches up; negative = insert, the audio
/// waits), at most 48 either way.
pub fn correction_for(err: i64, engaged: bool) -> (i64, bool) {
    let limit = if engaged {
        AV_CORRECT_STOP_SAMPLES
    } else {
        AV_CORRECT_START_SAMPLES
    };
    if err.abs() <= limit {
        return (0, false);
    }
    (
        (-err).clamp(-AV_CORRECT_MAX_PER_BLOCK, AV_CORRECT_MAX_PER_BLOCK),
        true,
    )
}

/// A planar-float FIFO delivering fixed-size boundary blocks on the wall grid,
/// with a media-time head.
pub struct AudioGridBuffer {
    /// Channel count, established on the first `push` (1–2 in practice). 0 until
    /// then (and after [`clear`](Self::clear)).
    channels: usize,
    /// Sample rate (Hz) — 48 000, enforced upstream by the decoder.
    rate: u32,
    /// One FIFO per channel (planar).
    fifo: Vec<VecDeque<f32>>,
    /// Media sample index (0-based) of the FIFO's front sample; `None` until a
    /// timed push after a clear.
    head: Option<i64>,
    /// Hard cap (samples/channel) = 2 s; oldest audio is dropped past it.
    cap_samples: usize,
    underruns: u64,
    overflows: u64,
    /// True once an overflow WARN has been emitted this song; a latch so the log
    /// carries at most one overflow warning per song. Cleared by [`clear`].
    warned_overflow: bool,
}

impl AudioGridBuffer {
    /// A fresh buffer at `rate_hz` with a 2-second hard cap.
    pub fn new(rate_hz: u32) -> Self {
        Self {
            channels: 0,
            rate: rate_hz,
            fifo: Vec::new(),
            head: None,
            cap_samples: (rate_hz as usize) * 2,
            underruns: 0,
            overflows: 0,
            warned_overflow: false,
        }
    }

    /// Append untimed planar audio (one `Vec<f32>` per channel) — see
    /// [`push_media`](Self::push_media).
    pub fn push(&mut self, planar: &[Vec<f32>]) {
        self.push_media(planar, None);
    }

    /// Append planar audio whose first sample has the 0-based media time
    /// `media_100ns`. The channel count is fixed on the first non-empty push;
    /// later pushes use the established count (a mismatch is clamped to the
    /// common minimum rather than desyncing). The head's media time is taken
    /// from the FIRST timed push while it is unknown (minus the samples already
    /// buffered); later pushes are contiguous and only counted.
    pub fn push_media(&mut self, planar: &[Vec<f32>], media_100ns: Option<i64>) {
        if planar.is_empty() {
            return;
        }
        if self.channels == 0 {
            self.channels = planar.len();
            self.fifo = (0..self.channels).map(|_| VecDeque::new()).collect();
        }
        if self.head.is_none()
            && let Some(t) = media_100ns
        {
            self.head = Some(samples_from_100ns(t, self.rate) - self.level_samples() as i64);
        }
        let ch = self.channels.min(planar.len());
        let n = planar.iter().take(ch).map(|c| c.len()).min().unwrap_or(0);
        for (c, fifo) in self.fifo.iter_mut().enumerate().take(ch) {
            for &s in &planar[c][..n] {
                fifo.push_back(s);
            }
        }
        self.enforce_cap();
    }

    /// Take `n` output samples per channel (`n ≥ 2`) consuming `n + extra`
    /// input samples (`|extra| < n`, the value from [`correction_for`]),
    /// spread over the block by linear interpolation: output `j` reads input
    /// position `j · (m−1)/(n−1)` (`m = n + extra`), so output 0 is input 0 and
    /// the last output is input `m−1`; `extra == 0` reproduces the input
    /// exactly. With fewer than `m` samples buffered the correction is skipped:
    /// up to `n` real samples are played, and a shortfall zero-fills the tail
    /// and counts one underrun. The head advances by the input consumed.
    /// Returns the planar block (empty when no channels have been seen) and the
    /// `extra` actually applied.
    pub fn take_block(&mut self, n: usize, extra: i64) -> (Vec<Vec<f32>>, i64) {
        if self.channels == 0 || n == 0 {
            return (Vec::new(), 0);
        }
        let level = self.level_samples();
        let m = n as i64 + extra;
        let mut out: Vec<Vec<f32>> = (0..self.channels).map(|_| vec![0.0f32; n]).collect();
        if (level as i64) < m {
            // Not enough for the correction (or even the block): play what is
            // there, bit-exact, and zero-fill a shortfall.
            let k = n.min(level);
            if k < n {
                self.underruns += 1;
            }
            for (c, out_ch) in out.iter_mut().enumerate() {
                for (dst, &src) in out_ch.iter_mut().zip(self.fifo[c].iter().take(k)) {
                    *dst = src;
                }
            }
            self.drain_front(k);
            return (out, 0);
        }
        let m = m as usize;
        let last = (m - 1) as f64;
        let ratio = last / (n - 1) as f64;
        for j in 0..n {
            let p = (j as f64 * ratio).min(last);
            let i = p as usize;
            let f = p - i as f64;
            let partner = (i + 1).min(m - 1);
            for (c, out_ch) in out.iter_mut().enumerate() {
                let a = self.fifo[c][i] as f64;
                let b = self.fifo[c][partner] as f64;
                out_ch[j] = (a + (b - a) * f) as f32;
            }
        }
        self.drain_front(m);
        (out, extra)
    }

    /// Take exactly `n` samples per channel with no correction (EOS tail,
    /// untimed audio): `take_block(n, 0)`.
    pub fn take_boundary_chunk(&mut self, n: usize) -> Vec<Vec<f32>> {
        self.take_block(n, 0).0
    }

    /// Hard alignment at start / seek (#148 design v2): make the head's media
    /// time equal `expected`. Audio AHEAD of it (head > expected) is PADDED
    /// with leading silence; audio BEHIND it is DROPPED. Returns
    /// `(aligned, delta)` — `delta` = samples dropped (positive) or padded
    /// (negative). `aligned` is false while fewer samples are buffered than
    /// must be dropped (all of them are dropped; the rest on a later call), and
    /// it is `(false, 0)` with nothing touched when there is no media head or
    /// the pad would exceed the 2 s cap.
    pub fn align_to(&mut self, expected: i64) -> (bool, i64) {
        let Some(head) = self.head else {
            return (false, 0);
        };
        let diff = expected - head;
        let pad = (-diff).max(0);
        if pad > self.cap_samples as i64 {
            // The audio starts more than the 2 s cap after the picture: stay
            // silent (nothing touched) until the expected time comes in reach,
            // never allocate an unbounded run of silence.
            return (false, 0);
        }
        self.pad_front(pad as usize);
        let drop = diff.max(0).min(self.level_samples() as i64);
        self.drain_front(drop as usize);
        (self.head == Some(expected), drop - pad)
    }

    /// Prepend `k` samples of silence to every channel; the head moves back.
    fn pad_front(&mut self, k: usize) {
        for ch in &mut self.fifo {
            for _ in 0..k {
                ch.push_front(0.0);
            }
        }
        self.head = self.head.map(|h| h - k as i64);
    }

    /// Drop the `k` oldest samples of every channel; the head moves forward.
    /// Callers never ask for more than [`level_samples`](Self::level_samples).
    fn drain_front(&mut self, k: usize) {
        for ch in &mut self.fifo {
            ch.drain(..k.min(ch.len()));
        }
        self.head = self.head.map(|h| h + k as i64);
    }

    /// Drop the oldest audio when the level exceeds the 2 s cap (bumps
    /// `overflows`; the head advances past the dropped samples).
    fn enforce_cap(&mut self) {
        let level = self.level_samples();
        if level > self.cap_samples {
            self.drain_front(level - self.cap_samples);
            self.overflows += 1;
        }
    }

    /// Media sample index of the next sample to play; `None` before a timed push.
    pub fn head_media(&self) -> Option<i64> {
        self.head
    }

    /// Channel count established by the first push (0 before it / after a clear).
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Samples per channel currently buffered.
    pub fn level_samples(&self) -> usize {
        self.fifo.first().map(|c| c.len()).unwrap_or(0)
    }

    /// Buffered audio in milliseconds (`level / rate`).
    pub fn buffer_ms(&self) -> u64 {
        if self.rate == 0 {
            return 0;
        }
        (self.level_samples() as u64) * 1000 / self.rate as u64
    }

    pub fn cap_samples(&self) -> usize {
        self.cap_samples
    }

    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    pub fn overflows(&self) -> u64 {
        self.overflows
    }

    /// Returns `true` exactly once per song after the first overflow, so the
    /// caller emits a single WARN per song rather than one per dropped chunk
    /// (#148 rework, item 4). Re-armed by [`clear`](Self::clear) (anchor).
    pub fn take_overflow_warning(&mut self) -> bool {
        if self.overflows > 0 && !self.warned_overflow {
            self.warned_overflow = true;
            true
        } else {
            false
        }
    }

    /// Empty the FIFO and forget the media head (called on play / seek / new
    /// song via the pacer's `anchor`, and on Resume). Cumulative `underruns` /
    /// `overflows` survive — they are lifetime telemetry, like the pacing
    /// counters.
    pub fn clear(&mut self) {
        self.channels = 0;
        self.fifo = Vec::new();
        self.head = None;
        self.warned_overflow = false;
    }
}

#[cfg(test)]
#[path = "audio_grid_tests.rs"]
mod audio_grid_tests;

#[cfg(test)]
#[path = "audio_grid_tests_mutants.rs"]
mod audio_grid_tests_mutants;
