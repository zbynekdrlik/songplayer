//! #233 closing lane (test-only): the instruments that measure the
//! resamplers' quality — THD+N of a tone by a least-squares sine fit, and
//! the loudest component of a band by a windowed DFT. Used by the ASIO
//! output's `Asrc` and the VBAN output's converter tests; checked here
//! against signals whose answer is known.

use std::f64::consts::PI;

/// −1 dBFS: the test tones' peak.
pub const MINUS_1_DBFS: f64 = 0.891_250_938_133_745_6;

/// `blocks` program blocks (1600 interleaved stereo frames at 48 kHz) of a
/// `hz` tone at −1 dBFS, both channels alike, as f32 (the program's samples).
pub fn tone_blocks(hz: f64, blocks: usize) -> Vec<Vec<f32>> {
    (0..blocks)
        .map(|b| {
            (0..1600)
                .flat_map(|i| {
                    let n = (b * 1600 + i) as f64;
                    let x = (MINUS_1_DBFS * (2.0 * PI * hz * n / 48_000.0).sin()) as f32;
                    [x, x]
                })
                .collect()
        })
        .collect()
}

/// The left channel of interleaved stereo samples, as f64.
pub fn left(interleaved: &[f32]) -> Vec<f64> {
    interleaved
        .iter()
        .step_by(2)
        .map(|&x| f64::from(x))
        .collect()
}

/// THD+N, dB: the RMS of the best sine (+ DC) at `f_norm` cycles per sample
/// over its RMS residual. A least-squares fit of sin, cos and 1 (the normal
/// equations, solved by Cramer's rule), so the tone's phase and the filter's
/// delay need no knowing.
pub fn thd_n_db(y: &[f64], f_norm: f64) -> f64 {
    let basis = |n: usize| {
        let w = 2.0 * PI * f_norm * n as f64;
        [w.sin(), w.cos(), 1.0]
    };
    let mut g = [[0.0f64; 3]; 3];
    let mut b = [0.0f64; 3];
    for (n, &v) in y.iter().enumerate() {
        let u = basis(n);
        for ((bi, gi), ui) in b.iter_mut().zip(g.iter_mut()).zip(u) {
            *bi += ui * v;
            for (gij, uj) in gi.iter_mut().zip(u) {
                *gij += ui * uj;
            }
        }
    }
    let det = |m: &[[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let d = det(&g);
    let coef: Vec<f64> = (0..3)
        .map(|c| {
            let mut m = g;
            for (r, row) in m.iter_mut().enumerate() {
                row[c] = b[r];
            }
            det(&m) / d
        })
        .collect();
    let residual_ms = y
        .iter()
        .enumerate()
        .map(|(n, &v)| {
            let u = basis(n);
            let fit = coef[0] * u[0] + coef[1] * u[1] + coef[2] * u[2];
            (v - fit) * (v - fit)
        })
        .sum::<f64>()
        / y.len() as f64;
    let tone_ms = (coef[0] * coef[0] + coef[1] * coef[1]) / 2.0;
    10.0 * (tone_ms / residual_ms).log10()
}

/// The 7-term Blackman-Harris window (sidelobes near −180 dB), `n` points.
fn blackman_harris_7(n: usize) -> Vec<f64> {
    const A: [f64; 7] = [
        0.271_051_400_693_42,
        0.433_297_939_234_48,
        0.218_122_999_543_11,
        0.065_925_446_388_03,
        0.010_811_742_098_37,
        0.000_776_584_825_22,
        0.000_013_887_217_35,
    ];
    (0..n)
        .map(|k| {
            let x = 2.0 * PI * k as f64 / n as f64;
            A.iter()
                .enumerate()
                .map(|(i, a)| {
                    let sign = if i.is_multiple_of(2) { 1.0 } else { -1.0 };
                    sign * a * (i as f64 * x).cos()
                })
                .sum::<f64>()
        })
        .collect()
}

/// The loudest component of `y` (sampled at `fs`) between `lo` and `hi` Hz:
/// its level in dBFS (a sine's peak 1.0 = 0 dBFS) and its frequency — every
/// DFT bin of the Blackman-Harris-windowed samples in the band, by Goertzel.
pub fn band_peak_dbfs(y: &[f64], fs: f64, lo: f64, hi: f64) -> (f64, f64) {
    let n = y.len();
    let w = blackman_harris_7(n);
    let gain: f64 = w.iter().sum();
    let x: Vec<f64> = y.iter().zip(&w).map(|(v, w)| v * w).collect();
    let first = (lo * n as f64 / fs).ceil() as usize;
    let last = ((hi * n as f64 / fs).floor() as usize).min(n / 2);
    let mut best = (f64::MIN, 0.0);
    for k in first..=last {
        let coeff = 2.0 * (2.0 * PI * k as f64 / n as f64).cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in &x {
            let s0 = v + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        let amp = 2.0 * power.max(0.0).sqrt() / gain;
        let db = 20.0 * amp.max(1e-300).log10();
        if db > best.0 {
            best = (db, k as f64 * fs / n as f64);
        }
    }
    best
}

/// A sine at `f_norm` with a second one 1e-6 as loud: THD+N 120 dB.
#[test]
fn thd_n_reads_a_known_residual() {
    let y: Vec<f64> = (0..20_000)
        .map(|n| {
            let n = n as f64;
            0.5 * (2.0 * PI * 0.01 * n + 0.3).sin() + 0.25 + 0.5e-6 * (2.0 * PI * 0.137 * n).sin()
        })
        .collect();
    let db = thd_n_db(&y, 0.01);
    assert!((db - 120.0).abs() < 0.05, "{db}");
}

/// A −120 dBFS tone in the band reads −120 dBFS at its frequency; a 0 dBFS
/// tone outside it leaks nothing over −150 dBFS into it.
#[test]
fn the_band_peak_reads_a_known_tone() {
    let fs = 96_000.0;
    let y: Vec<f64> = (0..8_192)
        .map(|n| {
            let t = n as f64 / fs;
            (2.0 * PI * 20_000.0 * t).sin() + 1e-6 * (2.0 * PI * 30_000.0 * t).sin()
        })
        .collect();
    let (db, f) = band_peak_dbfs(&y, fs, 24_000.0, 48_000.0);
    assert!((db + 120.0).abs() < 0.1, "{db} dBFS");
    assert!((f - 30_000.0).abs() < 12.0, "{f} Hz");
    let quiet: Vec<f64> = (0..8_192)
        .map(|n| (2.0 * PI * 20_000.0 * n as f64 / fs).sin())
        .collect();
    let (db, _) = band_peak_dbfs(&quiet, fs, 24_000.0, 48_000.0);
    assert!(db < -150.0, "{db} dBFS");
}
