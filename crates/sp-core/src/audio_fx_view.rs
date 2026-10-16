//! #278: the playlist EQ panel's view maths — the curve's box, a band
//! handle's drag, the log sliders, the values as the panel shows them, the
//! grid and the order of the band cards. Pure and WASM-safe: the dashboard
//! draws with these (sp-ui has no unit tests), and the curves themselves are
//! [`crate::audio_fx`]'s, the server's own formula.
//!
//! What the panel shows is what it sends: a dragged or slid value is
//! snapped to the step its label shows ([`snap_freq`]: three significant
//! digits, [`snap_gain`]: 0,1 dB, [`snap_q`]: 0,01).

use crate::audio_fx::{
    CURVE_DB_MAX, CURVE_DB_MIN, EqBand, FREQ_MAX_HZ, FREQ_MIN_HZ, GAIN_MAX_DB, GAIN_MIN_DB,
    PlaylistFx, Q_MAX, Q_MIN, curve_path, db_y, freq_x,
};

/// The curve's box: the SVG's `viewBox` (the SVG itself takes the card's
/// width and keeps this aspect).
pub const BOX_W: f64 = 800.0;
pub const BOX_H: f64 = 300.0;
/// Points a curve is drawn with.
pub const CURVE_POINTS: usize = 200;
/// Positions of a frequency or Q slider: 0 ..= this.
pub const SLIDER_STEPS: u32 = 1000;
/// Q's factor per wheel notch over a band's handle (a notch is 100 of the
/// wheel's `deltaY`; up narrows).
pub const WHEEL_Q_FACTOR: f64 = 1.1;

/// The frequencies the grid marks, with their labels.
pub const FREQ_GRID: [(f64, &str); 10] = [
    (20.0, "20"),
    (50.0, "50"),
    (100.0, "100"),
    (200.0, "200"),
    (500.0, "500"),
    (1000.0, "1k"),
    (2000.0, "2k"),
    (5000.0, "5k"),
    (10_000.0, "10k"),
    (20_000.0, "20k"),
];

/// The levels the grid marks (every 6 dB inside the box), with their labels.
pub const DB_GRID: [(f64, &str); 8] = [
    (12.0, "+12"),
    (6.0, "+6"),
    (0.0, "0"),
    (-6.0, "−6"),
    (-12.0, "−12"),
    (-18.0, "−18"),
    (-24.0, "−24"),
    (-30.0, "−30"),
];

/// A pointer `offset` px into an element drawn `rendered` px long, in the
/// units of a box `len` long (an element not drawn: 0).
pub fn to_box(offset: f64, rendered: f64, len: f64) -> f64 {
    if rendered > 0.0 {
        offset * len / rendered
    } else {
        0.0
    }
}

/// The frequency at `x` across a log axis `width` wide ([`freq_x`]'s
/// inverse), held to 20 ..= 20 000 Hz.
pub fn x_freq(x: f64, width: f64) -> f64 {
    let span = (FREQ_MAX_HZ / FREQ_MIN_HZ).ln();
    (FREQ_MIN_HZ * (span * x / width).exp()).clamp(FREQ_MIN_HZ, FREQ_MAX_HZ)
}

/// The level at `y` down an axis `height` tall ([`db_y`]'s inverse), held
/// to the curve's box.
pub fn y_db(y: f64, height: f64) -> f64 {
    let level = CURVE_DB_MAX - (CURVE_DB_MAX - CURVE_DB_MIN) * y / height;
    level.clamp(CURVE_DB_MIN, CURVE_DB_MAX)
}

/// A frequency held to 20 ..= 20 000 Hz and rounded to three significant
/// digits (whole hertz below 1 kHz): what [`freq_label`] shows.
pub fn snap_freq(hz: f64) -> f64 {
    let hz = hz.clamp(FREQ_MIN_HZ, FREQ_MAX_HZ);
    let digits = hz.log10().floor() as i32;
    let step = 10f64.powi((digits - 2).max(0));
    (hz / step).round() * step
}

/// A gain in 0,1 dB steps, held to −30 ..= +12 dB: what [`gain_label`]
/// shows (the playlist's volume too).
pub fn snap_gain(db: f64) -> f64 {
    ((db * 10.0).round() / 10.0).clamp(GAIN_MIN_DB, GAIN_MAX_DB)
}

/// A Q in 0,01 steps, held to 0,1 ..= 10: what [`q_label`] shows.
pub fn snap_q(q: f64) -> f64 {
    ((q * 100.0).round() / 100.0).clamp(Q_MIN, Q_MAX)
}

fn comma(number: String) -> String {
    number.replace('.', ",")
}

/// A frequency as the panel shows it: `80 Hz`, `1,25 kHz`, `12,5 kHz`.
pub fn freq_label(hz: f64) -> String {
    let hz = snap_freq(hz);
    if hz < 1000.0 {
        format!("{hz} Hz")
    } else {
        let khz = hz / 1000.0;
        format!("{} kHz", comma(khz.to_string()))
    }
}

/// A gain as the panel shows it: `+2,5 dB`, `0,0 dB`, `−3,5 dB`.
pub fn gain_label(db: f64) -> String {
    let db = snap_gain(db);
    let sign = if db > 0.0 {
        "+"
    } else if db < 0.0 {
        "−"
    } else {
        ""
    };
    format!("{sign}{} dB", comma(format!("{:.1}", db.abs())))
}

/// A Q as the panel shows it: `Q 0,71`.
pub fn q_label(q: f64) -> String {
    format!("Q {}", comma(format!("{:.2}", snap_q(q))))
}

/// Where `value` sits on a log slider from `min` to `max`.
fn log_position(value: f64, min: f64, max: f64) -> u32 {
    let share = (value / min).ln() / (max / min).ln();
    let steps = f64::from(SLIDER_STEPS);
    (share * steps).round().clamp(0.0, steps) as u32
}

/// The value at `position` of a log slider from `min` to `max`.
fn log_value(position: u32, min: f64, max: f64) -> f64 {
    let share = f64::from(position) / f64::from(SLIDER_STEPS);
    min * ((max / min).ln() * share).exp()
}

/// What a band card's slider shows (the #200 drag gate): while the operator
/// drags it, the position under the pointer; else the position of the
/// band's value. A log slider's snapped value maps back to a position a
/// step or ten away at low Q / low frequency, which would make the thumb
/// jump under the pointer.
pub fn slider_shown<T>(dragging: bool, dragged: T, value: T) -> T {
    if dragging { dragged } else { value }
}

/// A frequency's position on the band card's frequency slider (log scale).
pub fn freq_slider(hz: f64) -> u32 {
    log_position(hz, FREQ_MIN_HZ, FREQ_MAX_HZ)
}

/// The frequency at a position of that slider, snapped.
pub fn slider_freq(position: u32) -> f64 {
    snap_freq(log_value(position, FREQ_MIN_HZ, FREQ_MAX_HZ))
}

/// A Q's position on the band card's Q slider (log scale).
pub fn q_slider(q: f64) -> u32 {
    log_position(q, Q_MIN, Q_MAX)
}

/// The Q at a position of that slider, snapped.
pub fn slider_q(position: u32) -> f64 {
    snap_q(log_value(position, Q_MIN, Q_MAX))
}

/// Q after a wheel turn of `delta_y` over a band's handle:
/// [`WHEEL_Q_FACTOR`] per notch, up (a negative `deltaY`) narrows, snapped
/// and held to the Q limits. Many small events (a touchpad) go through
/// [`WheelTurn`].
pub fn wheel_q(q: f64, delta_y: f64) -> f64 {
    let notches = -delta_y / 100.0;
    snap_q(q * WHEEL_Q_FACTOR.powf(notches))
}

/// The wheel's turn over one band's handle, carried between events. A
/// turn too small to reach the next 0,01 of Q (a touchpad's few pixels, a
/// browser's percent-based scrolling) adds up until it does, so Q never
/// sticks and a touchpad moves it no faster than a mouse wheel; a turn the
/// other way starts afresh (so one past a Q limit is never owed).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WheelTurn {
    /// The turn not yet spent on Q, in `deltaY` units.
    pending: f64,
}

impl WheelTurn {
    /// Q after one wheel event of `delta_y` over a handle at `q`.
    pub fn turn(&mut self, q: f64, delta_y: f64) -> f64 {
        let same_way = self.pending * delta_y >= 0.0;
        self.pending = if same_way {
            self.pending + delta_y
        } else {
            delta_y
        };
        let next = wheel_q(q, self.pending);
        if next != snap_q(q) {
            self.pending = 0.0;
        }
        next
    }
}

/// Where a band's handle sits in a `width` × `height` box: its frequency
/// across; its gain up, or (a high-/low-pass) the 0 dB line.
pub fn handle_xy(band: &EqBand, width: f64, height: f64) -> (f64, f64) {
    let level = if band.kind.uses_gain() {
        band.gain_db
    } else {
        0.0
    };
    (freq_x(band.freq_hz, width), db_y(level, height))
}

/// `band` with its handle dragged to (`x`, `y`) in a `width` × `height`
/// box: the frequency under the pointer and, for a shelf or a peak, the
/// gain, each snapped; a high-/low-pass moves on its frequency only.
pub fn dragged(band: &EqBand, x: f64, y: f64, width: f64, height: f64) -> EqBand {
    let gain_db = if band.kind.uses_gain() {
        snap_gain(y_db(y, height))
    } else {
        band.gain_db
    };
    EqBand {
        freq_hz: snap_freq(x_freq(x, width)),
        gain_db,
        ..*band
    }
}

/// Where a drag of `band`'s handle took hold: the pointer's (`x`, `y`) less
/// the handle's centre ([`handle_xy`]). The grip reaches well around the
/// dot, so a drag moves the band BY the pointer's travel ([`dragged_from`]),
/// never makes it jump to the pointer (live audio: a tap off the centre
/// would change the sound).
pub fn grab_offset(band: &EqBand, x: f64, y: f64, width: f64, height: f64) -> (f64, f64) {
    let (hx, hy) = handle_xy(band, width, height);
    (x - hx, y - hy)
}

/// `band` dragged with the pointer at (`x`, `y`), held at `grab`
/// ([`grab_offset`]): [`dragged`] at the point the handle's centre is
/// carried to.
pub fn dragged_from(
    band: &EqBand,
    grab: (f64, f64),
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> EqBand {
    dragged(band, x - grab.0, y - grab.1, width, height)
}

/// One band's own curve (its filter alone, no volume) as an SVG path in a
/// `width` × `height` box; a band switched off draws none.
pub fn band_path(band: &EqBand, width: f64, height: f64, points: usize) -> String {
    if !band.enabled {
        return String::new();
    }
    let alone = PlaylistFx {
        gain_db: 0.0,
        eq: vec![*band],
    };
    curve_path(&alone, width, height, points)
}

/// The whole playlist's `curve` (as [`curve_path`] drew it) closed down to
/// the 0 dB line: the soft fill under (or over) it. It takes the drawn
/// curve, so a drag computes the curve once.
pub fn fill_path(curve: &str, width: f64, height: f64) -> String {
    let zero = db_y(0.0, height);
    format!("{curve} L{width:.1} {zero:.1} L0.0 {zero:.1} Z")
}

/// The order of the band cards (and the handles' numbers): by frequency,
/// low to high; a tie by type (iemmixer's rank: high-pass first, low-pass
/// last); then as stored.
pub fn display_order(eq: &[EqBand]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..eq.len()).collect();
    // `BandKind` is declared low to high (`BandKind::ALL`), so its
    // discriminant is its rank.
    order.sort_by(|&a, &b| {
        let (a, b) = (&eq[a], &eq[b]);
        a.freq_hz
            .total_cmp(&b.freq_hz)
            .then((a.kind as u8).cmp(&(b.kind as u8)))
    });
    order
}

#[cfg(test)]
#[path = "audio_fx_view_tests.rs"]
mod tests;
