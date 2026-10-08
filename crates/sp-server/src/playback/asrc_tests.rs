//! #233: the ASIO resampler — 48 kHz → the card's rate with the servo's
//! correction (the frame counts prove the ratio), its bounds, its delay, the
//! tone kept; the splice — a pass-through delayed by its fade, an insert and a
//! skip with no click; the installer's notice for the rubato it links.
//!
//! The frame counts are exact: rubato's `Async` steps its interpolation index
//! in plain f64 (`asynchro.rs` `step_index`), so a scratch model of its
//! `FixedAsync::Input` sizing gives them (the first block is 3 196 frames at
//! 96 kHz: the sinc's start index is −255).

use super::*;
use std::io::Write;

use crate::playback::resample_quality as quality;
use rubato::{SincInterpolationType, WindowFunction};

/// `blocks` program blocks of a 1 kHz tone (0.5 peak), both channels alike.
fn tone(blocks: usize, hz: f32) -> Vec<Vec<f32>> {
    (0..blocks)
        .map(|b| {
            (0..1600)
                .flat_map(|i| {
                    let x = (((b * 1600 + i) as f32) * 2.0 * std::f32::consts::PI * hz / 48_000.0)
                        .sin()
                        * 0.5;
                    [x, x]
                })
                .collect()
        })
        .collect()
}

fn total_frames(rate: f64, ppm: f64, blocks: usize) -> usize {
    let mut a = Asrc::new(rate).unwrap();
    a.set_correction_ppm(ppm).unwrap();
    let block = vec![0.0f32; 3200];
    (0..blocks)
        .map(|_| a.process(&block).unwrap().len() / 2)
        .sum()
}

#[test]
fn the_ratio_is_the_rate_times_the_correction() {
    // 60 blocks (2 s of program): the ideal count less the sinc's start-up
    // (2·ratio frames) and half a block of the first block's ratio ramp.
    for (rate, ppm, frames) in [
        (96_000.0, 0.0, 191_996),
        (96_000.0, 100.0, 192_015),
        (96_000.0, -250.0, 191_948),
        (44_100.0, 0.0, 88_198),
        (48_000.0, 50.0, 96_002),
    ] {
        let got = total_frames(rate, ppm, 60);
        assert_eq!(got, frames, "{rate} Hz {ppm} ppm");
    }
}

#[test]
fn a_correction_past_1000_ppm_is_refused() {
    let mut a = Asrc::new(96_000.0).unwrap();
    assert!(a.set_correction_ppm(999.0).is_ok());
    assert!(a.set_correction_ppm(-999.0).is_ok());
    assert!(a.set_correction_ppm(1_001.0).is_err());
    assert!(a.set_correction_ppm(-1_001.0).is_err());
}

#[test]
fn the_delay_is_half_the_sinc_at_the_card_rate() {
    assert_eq!(Asrc::new(96_000.0).unwrap().delay_frames(), 256);
    assert_eq!(Asrc::new(48_000.0).unwrap().delay_frames(), 128);
}

/// One block gives at most `1600 · ratio · 1.001 + 10` frames (rubato's
/// bound for a fixed input), which sizes the buffer once.
#[test]
fn the_most_one_block_gives_is_known() {
    assert_eq!(Asrc::new(96_000.0).unwrap().max_out_frames(), 3_213);
    assert_eq!(Asrc::new(48_000.0).unwrap().max_out_frames(), 1_611);
}

#[test]
fn a_block_that_is_not_one_program_block_is_refused() {
    let mut a = Asrc::new(96_000.0).unwrap();
    assert!(a.process(&[0.0; 3198]).is_err());
    assert!(a.process(&[0.0; 3202]).is_err());
    assert!(a.process(&[0.0; 3200]).is_ok());
}

#[test]
fn a_1khz_tone_stays_1khz_at_96k_with_a_correction() {
    let mut a = Asrc::new(96_000.0).unwrap();
    a.set_correction_ppm(300.0).unwrap();
    let mut left = Vec::new();
    for (b, block) in tone(30, 1000.0).iter().enumerate() {
        let out = a.process(block).unwrap();
        if b >= 3 {
            left.extend(out.iter().step_by(2).copied());
        }
    }
    // 27 blocks are 0.9 s of program: 900 cycles.
    let rising = left
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count();
    assert!((898..=902).contains(&rising), "{rising}");
}

/// The largest step between neighbouring frames of the left channel.
fn max_step(samples: &[f32]) -> f32 {
    samples
        .chunks_exact(2)
        .collect::<Vec<_>>()
        .windows(2)
        .map(|w| (w[1][0] - w[0][0]).abs())
        .fold(0.0, f32::max)
}

/// One fade step of a 0.5 signal at 96 kHz (480 frames), plus rounding.
const FADE_STEP: f32 = 0.5 / 480.0 + 1e-6;

/// Silent frames (left channel exactly 0) after the hold's 480-frame start-up.
fn silent_frames(out: &[f32]) -> usize {
    out[960..].chunks_exact(2).filter(|f| f[0] == 0.0).count()
}

#[test]
fn the_splice_passes_audio_delayed_by_its_fade() {
    let mut s = Splice::new(96_000.0, 0, 3_200);
    assert_eq!(s.held_frames(), 480, "5 ms at 96 kHz");
    let input: Vec<f32> = (0..9_600).map(|i| i as f32).collect();
    let mut out = Vec::new();
    for chunk in input.chunks(3_200) {
        out.extend_from_slice(s.process(chunk));
    }
    let mut want = vec![0.0f32; 960];
    want.extend_from_slice(&input[..input.len() - 960]);
    assert_eq!(out, want, "bit for bit, 480 frames late");
}

#[test]
fn an_insert_is_a_fade_a_gap_and_a_fade_never_a_click() {
    let mut s = Splice::new(96_000.0, 4_800, 3_200);
    let dc = vec![0.5f32; 6_400];
    let mut out = Vec::new();
    out.extend_from_slice(s.process(&dc));
    s.insert(4_224); // 44 ms
    out.extend_from_slice(s.process(&dc));
    out.extend_from_slice(s.process(&dc));
    assert_eq!(out.len(), 3 * 6_400 + 2 * 4_224);
    // From frame 481 on: the first 480 frames are the hold's start-up silence.
    assert!(
        max_step(&out[962..]) <= FADE_STEP,
        "{}",
        max_step(&out[962..])
    );
    assert_eq!(
        silent_frames(&out),
        4_225,
        "the fade out ends on one silent frame, then the 4_224 inserted"
    );
    assert_eq!(out[out.len() - 2], 0.5, "back at full level");
}

#[test]
fn a_skip_is_a_fade_and_a_fade_and_spans_blocks() {
    let mut s = Splice::new(96_000.0, 0, 3_200);
    let dc = vec![0.5f32; 6_400];
    let mut out = Vec::new();
    out.extend_from_slice(s.process(&dc));
    s.skip(4_224);
    out.extend_from_slice(s.process(&dc));
    out.extend_from_slice(s.process(&dc));
    out.extend_from_slice(s.process(&dc));
    assert_eq!(out.len(), 4 * 6_400 - 2 * 4_224);
    assert!(max_step(&out[962..]) <= FADE_STEP);
    assert_eq!(silent_frames(&out), 1, "faded to silence once, then in");
    assert_eq!(out[out.len() - 2], 0.5, "back at full level");
    let mut big = Splice::new(96_000.0, 0, 3_200);
    big.process(&dc);
    big.skip(5_000); // more than one block (3_200 frames)
    assert_eq!(big.pending_skip_frames(), 5_000);
    assert_eq!(
        big.process(&dc).len(),
        0,
        "the whole block is skipped (the hold stays held)"
    );
    assert_eq!(big.pending_skip_frames(), 1_800, "the rest, for the servo");
    assert_eq!(
        big.process(&dc).len(),
        6_400 - 2 * (5_000 - 3_200),
        "the rest of the skip"
    );
    assert_eq!(big.pending_skip_frames(), 0);
}

/// A re-centre that comes while the splice is still muted (an insert during
/// a skip) fades nothing twice: the gap and the rest of the skip, one fade in.
#[test]
fn a_second_re_centre_while_muted_fades_once() {
    let mut s = Splice::new(96_000.0, 4_800, 3_200);
    let dc = vec![0.5f32; 6_400];
    let mut out = Vec::new();
    out.extend_from_slice(s.process(&dc));
    s.skip(5_000);
    assert!(s.process(&dc).is_empty(), "all skipped");
    s.insert(1_000);
    out.extend_from_slice(s.process(&dc));
    out.extend_from_slice(s.process(&dc));
    assert_eq!(out.len() / 2, 4 * 3_200 - 5_000 + 1_000);
    assert!(
        max_step(&out[962..]) <= FADE_STEP,
        "{}",
        max_step(&out[962..])
    );
    assert_eq!(silent_frames(&out), 1_001, "one fade out's end + the gap");
    assert_eq!(out[out.len() - 2], 0.5, "back at full level");
}

/// The installer ships THIRD-PARTY-NOTICES.txt; it must carry the MIT notice
/// of the rubato this crate pins (re-copy it when the pin moves).
#[test]
fn the_installer_notice_carries_the_pinned_rubatos_license() {
    const NOTICE: &str = include_str!("../../../../src-tauri/resources/THIRD-PARTY-NOTICES.txt");
    const MANIFEST: &str = include_str!("../../Cargo.toml");
    let notice = NOTICE.replace("\r\n", "\n");
    assert!(MANIFEST.contains("rubato = \"=5.0.1\""), "the pin");
    assert!(
        notice.contains("rubato 5.0.1"),
        "the notice names the pinned version"
    );
    assert!(notice.contains("Copyright (c) 2020 Henrik Enquist"));
    assert!(notice.contains(
        "The above copyright notice and this permission notice shall be included in all\n\
         copies or substantial portions of the Software."
    ));
}

/// The lane's measured choice of rubato's async sinc (#233 closing lane,
/// the owner: a state-of-the-art resampler): 256 taps, the table
/// oversampled 256× (rubato's own default is 128; the scratch model's images
/// fall from −146 to −149 dBFS), BlackmanHarris², cubic between its rows
/// (rubato's documented best quality-per-oversampling), the cutoff
/// automatic.
#[test]
fn the_resampler_runs_the_measured_sinc_setting() {
    let p = asrc_params();
    assert_eq!((p.sinc_len, p.oversampling_factor), (256, 256));
    assert_eq!(p.interpolation, SincInterpolationType::Cubic);
    assert_eq!(p.window, WindowFunction::BlackmanHarris2);
    assert_eq!(p.f_cutoff, None, "automatic");
}

/// Through the output's own resampler, 48 → 96 kHz: a 1 kHz tone at
/// −1 dBFS, the correction at −300, 0 and +300 ppm: THD+N at least 120 dB
/// (the scratch model of rubato: about 142 dB). The first two blocks (the
/// sinc's start-up, the correction's ramp) are left out; the figure goes to
/// the CI log.
#[test]
fn a_1_khz_tone_keeps_a_thd_n_over_120_db_across_300_ppm() {
    let blocks = quality::tone_blocks(1_000.0, 10);
    for ppm in [-300.0, 0.0, 300.0] {
        let mut a = Asrc::new(96_000.0).unwrap();
        a.set_correction_ppm(ppm).unwrap();
        let mut out = Vec::new();
        for (i, b) in blocks.iter().enumerate() {
            let y = a.process(b).unwrap();
            if i >= 2 {
                out.extend_from_slice(y);
            }
        }
        let f_norm = 1_000.0 / (96_000.0 * (1.0 + ppm * 1e-6));
        let db = quality::thd_n_db(&quality::left(&out), f_norm);
        writeln!(
            std::io::stderr(),
            "asrc 48 -> 96 kHz at {ppm:+} ppm: THD+N {db:.1} dB"
        )
        .unwrap();
        assert!(db >= 120.0, "{ppm} ppm: {db:.1} dB");
    }
}

/// A 20 kHz tone at −1 dBFS, 48 → 96 kHz: its image (28 kHz) and anything
/// else between 24 and 48 kHz stays under −120 dBFS (the model: ≤ −149 dBFS),
/// at 0 and +300 ppm.
#[test]
fn a_20_khz_tone_leaves_nothing_over_minus_120_dbfs_from_24_to_48_khz() {
    let blocks = quality::tone_blocks(20_000.0, 8);
    for ppm in [0.0, 300.0] {
        let mut a = Asrc::new(96_000.0).unwrap();
        a.set_correction_ppm(ppm).unwrap();
        let mut out = Vec::new();
        for (i, b) in blocks.iter().enumerate() {
            let y = a.process(b).unwrap();
            if i >= 2 {
                out.extend_from_slice(y);
            }
        }
        let y = quality::left(&out);
        let fs = 96_000.0 * (1.0 + ppm * 1e-6);
        let (db, hz) = quality::band_peak_dbfs(&y[..8_192], fs, 24_000.0, 48_000.0);
        writeln!(
            std::io::stderr(),
            "asrc 48 -> 96 kHz at {ppm:+} ppm: loudest in 24-48 kHz {db:.1} dBFS at {hz:.0} Hz"
        )
        .unwrap();
        assert!(db <= -120.0, "{ppm} ppm: {db:.1} dBFS at {hz:.0} Hz");
    }
}

/// The dashboard's tooltips (`sp_core::asio_resampling`, Slovak) name the
/// output's own figures: the 66,7 ms target, the 5 ms splice, the ±300 ppm
/// budget, the 5 ppm/s slew, the lock's 30 points in a minute, the
/// resampler's 256 taps and 256× table. Any change here must change the
/// text too (review round 2: they were written as literals).
#[test]
fn the_dashboards_tooltips_name_the_outputs_own_figures() {
    use crate::playback::asrc_servo::{
        BASE_LATENCY_100NS, MAX_PPM, MAX_SLEW_PPM_PER_S, REGRESSION_LOCK_SPAN_S,
        REGRESSION_MIN_POINTS,
    };
    use sp_core::asio_resampling::{
        CARD_TIP, CONVERSION_TIP, CORRECTION_TIP, FAULTS_TIP, LATENCY_TIP, SLEW_TIP,
    };
    let target_ms = format!("{:.1}", BASE_LATENCY_100NS as f64 / 10_000.0).replace('.', ",");
    let has = |tip: &str, figure: String| assert!(tip.contains(&figure), "{figure:?} in {tip:?}");
    has(LATENCY_TIP, format!("cieľ {target_ms} ms"));
    has(
        FAULTS_TIP,
        format!("s {} ms prelínaním", SPLICE_FADE_S * 1_000.0),
    );
    has(CORRECTION_TIP, format!("±{MAX_PPM} ppm"));
    has(SLEW_TIP, format!("o {MAX_SLEW_PPM_PER_S} ppm za sekundu"));
    has(CARD_TIP, format!("({REGRESSION_MIN_POINTS} bodov)"));
    assert_eq!(REGRESSION_LOCK_SPAN_S, 60.0, "CARD_TIP: po minúte");
    has(CARD_TIP, "po minúte".to_string());
    let p = asrc_params();
    has(CONVERSION_TIP, format!("{} koeficientov", p.sinc_len));
    has(
        CONVERSION_TIP,
        format!("tabuľka {}×", p.oversampling_factor),
    );
    assert_eq!(p.window, WindowFunction::BlackmanHarris2);
    has(CONVERSION_TIP, "BlackmanHarris²".to_string());
}
