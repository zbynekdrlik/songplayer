//! A playlist's own sound on its audio (#242): the volume and the EQ bands
//! of `sp_core::audio_fx`, run on the interleaved f32 samples of the song's
//! audio stream (`playlist_fx.rs` wraps the stream).
//!
//! - Each enabled band is one biquad per channel (transposed direct form
//!   II, f64 state), the bands in their order.
//! - A volume change ramps over [`FX_RAMP_FRAMES`] (50 ms at 48 kHz, like
//!   the mixer's faders).
//! - An EQ change builds the new cascade and fades from the old cascade's
//!   output into the new one's over [`FX_RAMP_FRAMES`], so a change made
//!   while the song plays never clicks. A change that lands while a fade
//!   still runs waits for its end (the latest one wins).
//! - #278: the new cascade first runs over the song's last [`WARM_MS`] of
//!   input (kept in a ring), so it fades in SETTLED. From zero state a
//!   narrow boost rang up inside every fade of a live drag (an edit every
//!   50 ms): a wobble at -28 dB, modelled on #278 comment 6224608759.
//! - At 0 dB with no enabled band and no ramp running, the samples are not
//!   touched at all (bit-identical).

use std::collections::VecDeque;

use sp_core::audio_fx::{Biquad, PlaylistFx, coefficients};

/// Frames a volume ramp and an EQ crossfade last: 50 ms at 48 kHz.
pub const FX_RAMP_FRAMES: u32 = 2400;

/// How much of its input the processor keeps to warm a new cascade, ms:
/// a 120 Hz Q 5 peak settles to -131 dB in it; the limits' worst, a 20 Hz
/// Q 10 peak, to -51 dB (#278 comment 6224608759).
pub const WARM_MS: u32 = 300;

/// The frames [`WARM_MS`] holds at `rate`.
pub fn warm_frames(rate: u32) -> usize {
    (u64::from(rate) * u64::from(WARM_MS) / 1000) as usize
}

/// The linear factor of `db`.
pub fn db_to_gain(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// The enabled bands' filters, with one state pair per filter and channel.
#[derive(Debug, Clone)]
struct Cascade {
    filters: Vec<Biquad>,
    /// `state[filter * channels + channel]` = (z1, z2).
    state: Vec<(f64, f64)>,
}

impl Cascade {
    fn new(fx: &PlaylistFx, rate: f64, channels: usize) -> Self {
        let filters: Vec<Biquad> = fx
            .eq
            .iter()
            .filter(|b| b.enabled)
            .map(|b| coefficients(b, rate))
            .collect();
        let state = vec![(0.0, 0.0); filters.len() * channels];
        Self { filters, state }
    }

    fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }

    /// Run the filters over `history` (interleaved, oldest first, starting
    /// on a frame) and drop the output: their state as if they had played it.
    fn warm(&mut self, history: &VecDeque<f32>, channels: usize) {
        for (i, x) in history.iter().enumerate() {
            self.run(i % channels, channels, f64::from(*x));
        }
    }

    /// One sample of `channel` through every filter.
    fn run(&mut self, channel: usize, channels: usize, x: f64) -> f64 {
        let mut y = x;
        for (i, f) in self.filters.iter().enumerate() {
            let (z1, z2) = &mut self.state[i * channels + channel];
            let input = y;
            y = f.b0 * input + *z1;
            *z1 = f.b1 * input - f.a1 * y + *z2;
            *z2 = f.b2 * input - f.a2 * y;
        }
        y
    }
}

/// The running sound of one song's audio.
#[derive(Debug)]
pub struct FxProcessor {
    rate: f64,
    channels: usize,
    gain: f64,
    target_gain: f64,
    gain_step: f64,
    /// Frames the volume ramp has left (0: at the target).
    ramp_left: u32,
    cascade: Cascade,
    /// The cascade an EQ change fades out of, and the frames left.
    fading: Option<(Cascade, u32)>,
    /// An EQ change that landed during a fade: it fades in at the fade's end.
    pending: Option<Cascade>,
    /// The last [`warm_frames`] of input, interleaved, oldest first.
    history: VecDeque<f32>,
    /// The samples `history` holds once full.
    history_cap: usize,
}

impl FxProcessor {
    /// A processor already at `fx` (no ramp from silence: a song starts at
    /// its playlist's sound).
    pub fn new(fx: &PlaylistFx, rate: u32, channels: u16) -> Self {
        let channels = usize::from(channels.max(1));
        let history_cap = warm_frames(rate) * channels;
        let rate = f64::from(rate);
        let gain = db_to_gain(fx.gain_db);
        Self {
            rate,
            channels,
            gain,
            target_gain: gain,
            gain_step: 0.0,
            ramp_left: 0,
            cascade: Cascade::new(fx, rate, channels),
            fading: None,
            pending: None,
            history: VecDeque::with_capacity(history_cap),
            history_cap,
        }
    }

    /// Move to `fx`: the volume ramps there, an EQ change crossfades. An
    /// unchanged EQ keeps its filters' state. During a running crossfade the
    /// new filters wait for its end (cutting it short would jump).
    pub fn update(&mut self, fx: &PlaylistFx) {
        self.target_gain = db_to_gain(fx.gain_db);
        self.gain_step = (self.target_gain - self.gain) / f64::from(FX_RAMP_FRAMES);
        self.ramp_left = FX_RAMP_FRAMES;
        let next = Cascade::new(fx, self.rate, self.channels);
        let changed = next.filters != self.cascade.filters;
        if self.fading.is_some() {
            self.pending = changed.then_some(next);
        } else if changed {
            self.fade_to(next);
        }
    }

    /// Start fading from the current cascade into `next`, warmed first on
    /// the input kept.
    fn fade_to(&mut self, mut next: Cascade) {
        next.warm(&self.history, self.channels);
        let old = std::mem::replace(&mut self.cascade, next);
        self.fading = Some((old, FX_RAMP_FRAMES));
    }

    /// Whether the samples pass untouched now.
    pub fn is_passthrough(&self) -> bool {
        self.ramp_left == 0 && self.gain == 1.0 && self.cascade.is_empty() && self.fading.is_none()
    }

    /// Run `samples` (interleaved, `channels` per frame) in place.
    pub fn process(&mut self, samples: &mut [f32]) {
        if self.is_passthrough() {
            self.remember(samples);
            return;
        }
        let channels = self.channels;
        for frame in samples.chunks_mut(channels) {
            self.remember(frame);
            let fade = self
                .fading
                .as_ref()
                .map(|(_, left)| f64::from(*left) / f64::from(FX_RAMP_FRAMES));
            for (channel, sample) in frame.iter_mut().enumerate() {
                let x = f64::from(*sample);
                let mut y = self.cascade.run(channel, channels, x);
                if let (Some(w), Some((old, _))) = (fade, self.fading.as_mut()) {
                    let y_old = old.run(channel, channels, x);
                    y = y_old * w + y * (1.0 - w);
                }
                *sample = (y * self.gain) as f32;
            }
            self.step_ramps();
        }
    }

    /// Keep `samples` (whole frames of input) in the history, forgetting
    /// the oldest beyond [`warm_frames`].
    fn remember(&mut self, samples: &[f32]) {
        for x in samples {
            if self.history.len() >= self.history_cap {
                self.history.pop_front();
            }
            self.history.push_back(*x);
        }
    }

    /// One frame of the volume ramp and the EQ crossfade.
    fn step_ramps(&mut self) {
        if self.ramp_left > 0 {
            self.ramp_left -= 1;
            self.gain = if self.ramp_left == 0 {
                self.target_gain
            } else {
                self.gain + self.gain_step
            };
        }
        if let Some((_, left)) = self.fading.as_mut() {
            *left -= 1;
            if *left == 0 {
                self.fading = None;
                if let Some(next) = self.pending.take() {
                    self.fade_to(next);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "playlist_fx_dsp_tests.rs"]
mod tests;
