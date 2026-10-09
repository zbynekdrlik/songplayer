//! A playlist's own sound (#242): its volume and its parametric EQ.
//!
//! The legacy cg OBS shaped each playlist scene's audio with source filters
//! (a fader, a Gain filter, a 3-band EQ cutting the bass on ytfast and
//! ytslow). SongPlayer gives every playlist, on every node, the same kind of
//! control, with the owner's own values (owner, 9.10.2026: "chcem mat moznost
//! si nastavit vlastne hodnoty a eq"):
//!
//! - [`PlaylistFx::gain_db`]: the playlist's volume in dB;
//! - [`PlaylistFx::eq`]: up to [`MAX_BANDS`] [`EqBand`]s, each a second-order
//!   filter (RBJ Audio EQ Cookbook): high-pass, low shelf, peak, high shelf
//!   or low-pass at its frequency, gain and Q.
//!
//! This module is the model and the maths shared by the server, which runs
//! the filters on the playlist's audio, and the dashboard, which draws the
//! curve: [`validate`] the limits, [`coefficients`] of a band at a sample
//! rate, and [`response_db`] / [`curve`] the resulting magnitude response.
//! WASM-safe: pure `f64` arithmetic.

use serde::{Deserialize, Serialize};

/// Bands one playlist may carry.
pub const MAX_BANDS: usize = 8;
/// The playlist's volume and a band's gain, dB.
pub const GAIN_MIN_DB: f64 = -30.0;
pub const GAIN_MAX_DB: f64 = 12.0;
/// A band's frequency, Hz.
pub const FREQ_MIN_HZ: f64 = 20.0;
pub const FREQ_MAX_HZ: f64 = 20_000.0;
/// A band's Q (for a shelf: its slope).
pub const Q_MIN: f64 = 0.1;
pub const Q_MAX: f64 = 10.0;
/// The Q a new band starts with: a Butterworth high/low-pass, a gentle shelf.
pub const DEFAULT_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;
/// The rate a playlist's audio runs at (the program's).
pub const FX_RATE_HZ: f64 = 48_000.0;

/// The filter a band is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandKind {
    /// Cuts below the frequency (12 dB/octave); its gain is unused.
    HighPass,
    /// Raises or lowers everything below the frequency by the gain.
    LowShelf,
    /// Raises or lowers a bell around the frequency, its width set by Q.
    Peak,
    /// Raises or lowers everything above the frequency by the gain.
    HighShelf,
    /// Cuts above the frequency (12 dB/octave); its gain is unused.
    LowPass,
}

impl BandKind {
    /// Whether the band's gain shapes it (not for a high-/low-pass).
    pub fn uses_gain(self) -> bool {
        !matches!(self, BandKind::HighPass | BandKind::LowPass)
    }
}

fn default_q() -> f64 {
    DEFAULT_Q
}

fn default_enabled() -> bool {
    true
}

/// One EQ band.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EqBand {
    pub kind: BandKind,
    pub freq_hz: f64,
    #[serde(default)]
    pub gain_db: f64,
    #[serde(default = "default_q")]
    pub q: f64,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// A playlist's sound: its volume and its EQ. The default (0 dB, no band)
/// leaves the audio untouched.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaylistFx {
    #[serde(default)]
    pub gain_db: f64,
    #[serde(default)]
    pub eq: Vec<EqBand>,
}

impl PlaylistFx {
    /// Whether the audio passes unchanged: 0 dB and no enabled band.
    pub fn is_identity(&self) -> bool {
        self.gain_db == 0.0 && !self.eq.iter().any(|b| b.enabled)
    }
}

/// Why a [`PlaylistFx`] is refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FxError {
    #[error("audio_gain_db {0} is outside -30..=12 dB")]
    Gain(f64),
    #[error("audio_eq has {0} bands, at most 8")]
    TooManyBands(usize),
    #[error("audio_eq band {index}: freq_hz {value} is outside 20..=20000 Hz")]
    Freq { index: usize, value: f64 },
    #[error("audio_eq band {index}: gain_db {value} is outside -30..=12 dB")]
    BandGain { index: usize, value: f64 },
    #[error("audio_eq band {index}: q {value} is outside 0.1..=10")]
    Q { index: usize, value: f64 },
}

impl FxError {
    /// The refusal in Slovak, for the dashboard (bands counted from 1).
    pub fn sk(&self) -> String {
        match self {
            FxError::Gain(_) => "Hlasitosť musí byť od −30 do +12 dB".to_string(),
            FxError::TooManyBands(_) => "EQ môže mať najviac 8 pásiem".to_string(),
            FxError::Freq { index, .. } => {
                format!(
                    "Pásmo {}: frekvencia musí byť od 20 do 20 000 Hz",
                    index + 1
                )
            }
            FxError::BandGain { index, .. } => {
                format!("Pásmo {}: zisk musí byť od −30 do +12 dB", index + 1)
            }
            FxError::Q { index, .. } => format!("Pásmo {}: Q musí byť od 0,1 do 10", index + 1),
        }
    }
}

fn within(value: f64, min: f64, max: f64) -> bool {
    (min..=max).contains(&value)
}

/// Check the limits (a non-finite number is outside every one).
pub fn validate(fx: &PlaylistFx) -> Result<(), FxError> {
    if !within(fx.gain_db, GAIN_MIN_DB, GAIN_MAX_DB) {
        return Err(FxError::Gain(fx.gain_db));
    }
    if fx.eq.len() > MAX_BANDS {
        return Err(FxError::TooManyBands(fx.eq.len()));
    }
    for (index, band) in fx.eq.iter().enumerate() {
        if !within(band.freq_hz, FREQ_MIN_HZ, FREQ_MAX_HZ) {
            return Err(FxError::Freq {
                index,
                value: band.freq_hz,
            });
        }
        if !within(band.gain_db, GAIN_MIN_DB, GAIN_MAX_DB) {
            return Err(FxError::BandGain {
                index,
                value: band.gain_db,
            });
        }
        if !within(band.q, Q_MIN, Q_MAX) {
            return Err(FxError::Q {
                index,
                value: band.q,
            });
        }
    }
    Ok(())
}

/// A second-order filter, normalised so a0 = 1:
/// y = b0·x + b1·x₋₁ + b2·x₋₂ − a1·y₋₁ − a2·y₋₂.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

/// The band's filter at `rate` (RBJ Audio EQ Cookbook). A frequency at or
/// above 0.49 × rate is taken at 0.49 × rate, so the filter stays stable.
pub fn coefficients(band: &EqBand, rate: f64) -> Biquad {
    let freq = band.freq_hz.min(0.49 * rate);
    let w0 = 2.0 * std::f64::consts::PI * freq / rate;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * band.q);
    let a = 10f64.powf(band.gain_db / 40.0);
    let shelf = 2.0 * a.sqrt() * alpha;
    let (b, den) = match band.kind {
        BandKind::HighPass => (
            [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        ),
        BandKind::LowPass => (
            [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        ),
        BandKind::Peak => (
            [1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a],
            [1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a],
        ),
        BandKind::LowShelf => (
            [
                a * ((a + 1.0) - (a - 1.0) * cos + shelf),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - shelf),
            ],
            [
                (a + 1.0) + (a - 1.0) * cos + shelf,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - shelf,
            ],
        ),
        BandKind::HighShelf => (
            [
                a * ((a + 1.0) + (a - 1.0) * cos + shelf),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - shelf),
            ],
            [
                (a + 1.0) - (a - 1.0) * cos + shelf,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - shelf,
            ],
        ),
    };
    let a0 = den[0];
    Biquad {
        b0: b[0] / a0,
        b1: b[1] / a0,
        b2: b[2] / a0,
        a1: den[1] / a0,
        a2: den[2] / a0,
    }
}

/// The filter's gain at `freq`, dB.
pub fn biquad_db(filter: &Biquad, freq: f64, rate: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq / rate;
    let (s1, c1) = w.sin_cos();
    let (s2, c2) = (2.0 * w).sin_cos();
    let num_re = filter.b0 + filter.b1 * c1 + filter.b2 * c2;
    let num_im = filter.b1 * s1 + filter.b2 * s2;
    let den_re = 1.0 + filter.a1 * c1 + filter.a2 * c2;
    let den_im = filter.a1 * s1 + filter.a2 * s2;
    let num = num_re * num_re + num_im * num_im;
    let den = den_re * den_re + den_im * den_im;
    10.0 * (num / den).log10()
}

/// The whole playlist's gain at `freq`, dB: its volume plus every enabled
/// band.
pub fn response_db(fx: &PlaylistFx, freq: f64, rate: f64) -> f64 {
    let bands: f64 = fx
        .eq
        .iter()
        .filter(|b| b.enabled)
        .map(|b| biquad_db(&coefficients(b, rate), freq, rate))
        .sum();
    fx.gain_db + bands
}

/// The curve the dashboard draws: `points` frequencies spaced evenly on a
/// log axis from [`FREQ_MIN_HZ`] to [`FREQ_MAX_HZ`], each with its gain in dB
/// at [`FX_RATE_HZ`]. `points` below 2 is taken as 2.
pub fn curve(fx: &PlaylistFx, points: usize) -> Vec<(f64, f64)> {
    let n = points.max(2);
    let span = (FREQ_MAX_HZ / FREQ_MIN_HZ).ln();
    (0..n)
        .map(|i| {
            let freq = FREQ_MIN_HZ * (span * i as f64 / (n - 1) as f64).exp();
            (freq, response_db(fx, freq, FX_RATE_HZ))
        })
        .collect()
}

/// The dB range the dashboard's curve shows (a value past it is drawn at
/// its edge).
pub const CURVE_DB_MIN: f64 = -36.0;
pub const CURVE_DB_MAX: f64 = 18.0;

/// Where `freq` sits on a log axis `width` wide: [`FREQ_MIN_HZ`] at 0,
/// [`FREQ_MAX_HZ`] at `width`.
pub fn freq_x(freq: f64, width: f64) -> f64 {
    width * (freq / FREQ_MIN_HZ).ln() / (FREQ_MAX_HZ / FREQ_MIN_HZ).ln()
}

/// Where `db` sits on an axis `height` tall: [`CURVE_DB_MAX`] at 0 (the
/// top), [`CURVE_DB_MIN`] at `height`, clamped to the box.
pub fn db_y(db: f64, height: f64) -> f64 {
    let clamped = db.clamp(CURVE_DB_MIN, CURVE_DB_MAX);
    height * (CURVE_DB_MAX - clamped) / (CURVE_DB_MAX - CURVE_DB_MIN)
}

/// The curve as an SVG path in a `width` × `height` box ([`curve`]'s
/// points, one decimal).
pub fn curve_path(fx: &PlaylistFx, width: f64, height: f64, points: usize) -> String {
    curve(fx, points)
        .iter()
        .enumerate()
        .map(|(i, (freq, db))| {
            let lead = if i == 0 { "M" } else { " L" };
            format!("{lead}{:.1} {:.1}", freq_x(*freq, width), db_y(*db, height))
        })
        .collect()
}

#[cfg(test)]
#[path = "audio_fx_tests.rs"]
mod tests;
