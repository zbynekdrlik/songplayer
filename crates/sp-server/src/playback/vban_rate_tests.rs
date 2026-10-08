//! #233: a VBAN destination's rate conversion — 48 kHz passes untouched
//! (FOH's bytes), every other rate gives exactly rate/30 frames per block,
//! keeps a tone's pitch, and reports its delay (half the block FFT).

use super::*;
use std::io::Write;

use crate::playback::resample_quality as quality;

fn tone(blocks: usize) -> Vec<Vec<f32>> {
    (0..blocks)
        .map(|b| {
            (0..1600)
                .flat_map(|i| {
                    let n = (b * 1600 + i) as f32;
                    let x = (n * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).sin() * 0.5;
                    [x, x]
                })
                .collect()
        })
        .collect()
}

#[test]
fn the_48k_destination_is_passed_through_untouched() {
    let mut c = VbanRateConverter::new(48_000);
    let block = vec![0.3f32; 3200];
    let out = c.convert(Some(&block[..])).unwrap();
    assert!(
        std::ptr::eq(out.as_ptr(), block.as_ptr()),
        "the same samples, no copy"
    );
    assert_eq!(c.convert(None), None, "silence stays the encoder's zeros");
    let short = vec![0.3f32; 3199];
    let out = c.convert(Some(&short[..])).unwrap();
    assert_eq!(out.len(), 3199, "a wrong length is the encoder's to refuse");
    assert_eq!(c.delay_frames(), 0);
    assert_eq!(c.failed(), None);
}

#[test]
fn every_other_rate_gives_exactly_rate_over_30_frames_per_block() {
    for rate in [44_100u32, 88_200, 96_000, 192_000] {
        let mut c = VbanRateConverter::new(rate);
        assert_eq!(c.failed(), None, "{rate}");
        for block in tone(30) {
            let out = c.convert(Some(&block[..])).unwrap();
            assert_eq!(out.len(), rate as usize / 30 * 2, "{rate}");
        }
        let silence = c.convert(None).unwrap().len();
        assert_eq!(
            silence,
            rate as usize / 30 * 2,
            "silence goes through the filter too"
        );
        let short = c.convert(Some(&[0.5; 3199][..])).unwrap().len();
        assert_eq!(short, rate as usize / 30 * 2, "a wrong length is silence");
        assert_eq!(c.delay_frames(), fft_delay_frames(rate));
    }
    assert_eq!(fft_delay_frames(48_000), 0);
    assert_eq!(fft_delay_frames(44_100), 735, "half the block FFT");
    assert_eq!(fft_delay_frames(96_000), 1_600);
    assert_eq!(fft_delay_frames(192_000), 3_200);
}

#[test]
fn a_1khz_tone_stays_1khz_at_96k() {
    let mut c = VbanRateConverter::new(96_000);
    let mut left = Vec::new();
    for (b, block) in tone(30).iter().enumerate() {
        let out = c.convert(Some(&block[..])).unwrap();
        if b >= 3 {
            left.extend(out.iter().step_by(2).copied());
        }
    }
    assert_eq!(left.len(), 27 * 3_200);
    let rising = left
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count();
    assert!(
        (898..=902).contains(&rising),
        "{rising} rising zero crossings in 0.9 s"
    );
    let peak = left.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!((0.45..=0.55).contains(&peak), "the level is kept: {peak}");
}

#[test]
fn silence_in_is_silence_out() {
    let mut c = VbanRateConverter::new(96_000);
    for _ in 0..5 {
        let out = c.convert(Some(&[0.0; 3200][..])).unwrap();
        assert!(out.iter().all(|x| x.abs() < 1e-6));
    }
}

#[test]
fn a_converter_rubato_refuses_sends_silence_and_says_why() {
    let mut c = VbanRateConverter::new(0);
    assert!(c.failed().is_some());
    assert_eq!(c.convert(Some(&[0.5; 3200][..])), None);
    assert_eq!(c.convert(None), None);
    assert_eq!(c.delay_frames(), 0);
}

/// The VBAN converter, 48 → 96 kHz (rubato's `Fft`, BlackmanHarris², the
/// block as one FFT): a 1 kHz tone at −1 dBFS keeps a THD+N of at least
/// 120 dB (the scratch model: about 148 dB), and a 20 kHz tone leaves nothing
/// over −120 dBFS between 24 and 48 kHz (the model: about −178 dBFS). The
/// first two blocks (its 16.7 ms delay) are left out; the figures go to the
/// CI log.
#[test]
fn the_96k_converter_keeps_a_thd_n_over_120_db_and_its_upper_band_clean() {
    let converted = |hz: f64| {
        let mut c = VbanRateConverter::new(96_000);
        let mut out = Vec::new();
        for (i, b) in quality::tone_blocks(hz, 8).iter().enumerate() {
            let y = c.convert(Some(b.as_slice())).unwrap();
            if i >= 2 {
                out.extend_from_slice(y);
            }
        }
        quality::left(&out)
    };
    let db = quality::thd_n_db(&converted(1_000.0), 1_000.0 / 96_000.0);
    let (image, hz) =
        quality::band_peak_dbfs(&converted(20_000.0)[..8_192], 96_000.0, 24_000.0, 48_000.0);
    writeln!(
        std::io::stderr(),
        "vban 48 -> 96 kHz: THD+N {db:.1} dB, loudest in 24-48 kHz {image:.1} dBFS at {hz:.0} Hz"
    )
    .unwrap();
    assert!(db >= 120.0, "{db:.1} dB");
    assert!(image <= -120.0, "{image:.1} dBFS at {hz:.0} Hz");
}
