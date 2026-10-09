//! #242: the playlist sound on real samples — passthrough identity, the
//! volume and its ramp, each band's measured gain against
//! `sp_core::audio_fx::response_db`, and the click-free EQ crossfade.
//! Wired via `#[cfg(test)] #[path = "playlist_fx_dsp_tests.rs"] mod tests;`.

use sp_core::audio_fx::{BandKind, EqBand, PlaylistFx, response_db};

use super::{FX_RAMP_FRAMES, FxProcessor, db_to_gain};

const RATE: u32 = 48_000;

fn band(kind: BandKind, freq_hz: f64, gain_db: f64, q: f64) -> EqBand {
    EqBand {
        kind,
        freq_hz,
        gain_db,
        q,
        enabled: true,
    }
}

fn fx(gain_db: f64, eq: Vec<EqBand>) -> PlaylistFx {
    PlaylistFx { gain_db, eq }
}

/// `frames` of a stereo sine at `freq` (amplitude 0.5), both channels.
fn sine(freq: f64, frames: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(frames * 2);
    for n in 0..frames {
        let v =
            (0.5 * (2.0 * std::f64::consts::PI * freq * n as f64 / f64::from(RATE)).sin()) as f32;
        out.push(v);
        out.push(v);
    }
    out
}

fn rms(samples: &[f32]) -> f64 {
    let sum: f64 = samples.iter().map(|s| f64::from(*s).powi(2)).sum();
    (sum / samples.len() as f64).sqrt()
}

/// The measured gain in dB of `fx` at `freq`, after 0.1 s of settling.
fn measured_db(settings: &PlaylistFx, freq: f64) -> f64 {
    let input = sine(freq, 24_000);
    let mut output = input.clone();
    let mut p = FxProcessor::new(settings, RATE, 2);
    p.process(&mut output);
    let skip = 4800 * 2;
    20.0 * (rms(&output[skip..]) / rms(&input[skip..])).log10()
}

#[test]
fn decibels_to_a_factor() {
    assert!((db_to_gain(-6.020_599_913) - 0.5).abs() < 1e-9);
    assert!((db_to_gain(20.0) - 10.0).abs() < 1e-12);
    assert_eq!(db_to_gain(0.0), 1.0);
    assert_eq!(FX_RAMP_FRAMES, 2400);
}

/// The default sound, and one whose bands are all off, leave every sample's
/// bits as they were.
#[test]
fn the_default_sound_leaves_the_samples_bit_identical() {
    let original: Vec<f32> = vec![
        0.1,
        -0.7,
        1.0e-40,
        -0.0,
        0.999_999,
        f32::MIN_POSITIVE,
        0.25,
        -0.25,
    ];
    let mut off = band(BandKind::Peak, 1000.0, 9.0, 1.0);
    off.enabled = false;
    for settings in [PlaylistFx::default(), fx(0.0, vec![off])] {
        let mut p = FxProcessor::new(&settings, RATE, 2);
        assert!(p.is_passthrough());
        let mut samples = original.clone();
        p.process(&mut samples);
        let bits = |v: &[f32]| v.iter().map(|s| s.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&samples), bits(&original));
    }
}

#[test]
fn the_volume_scales_every_sample() {
    let mut p = FxProcessor::new(&fx(-6.020_599_913_279_624, vec![]), RATE, 2);
    assert!(!p.is_passthrough());
    let mut samples = vec![0.8f32, -0.4, 0.2, 0.0];
    p.process(&mut samples);
    for (got, want) in samples.iter().zip([0.4f32, -0.2, 0.1, 0.0]) {
        assert!((got - want).abs() < 1e-6, "{got} vs {want}");
    }
}

/// Each band type's measured gain matches the curve the dashboard draws.
#[test]
fn each_band_measures_what_the_curve_says() {
    let cases = [
        (
            band(BandKind::Peak, 1000.0, 6.0, 1.0),
            [1000.0, 300.0, 5000.0],
        ),
        (
            band(BandKind::LowShelf, 200.0, -20.0, 0.707),
            [60.0, 200.0, 2000.0],
        ),
        (
            band(BandKind::HighShelf, 4000.0, 8.0, 0.707),
            [500.0, 4000.0, 12_000.0],
        ),
        (
            band(BandKind::HighPass, 120.0, 0.0, 0.707),
            [60.0, 120.0, 1000.0],
        ),
        (
            band(BandKind::LowPass, 6000.0, 0.0, 0.707),
            [1000.0, 6000.0, 12_000.0],
        ),
    ];
    for (b, freqs) in cases {
        let settings = fx(0.0, vec![b]);
        for f in freqs {
            let want = response_db(&settings, f, f64::from(RATE));
            let got = measured_db(&settings, f);
            assert!(
                (got - want).abs() < 0.05,
                "{b:?} at {f} Hz: {got} vs {want}"
            );
        }
    }
}

/// Several bands and a volume add up, as the curve says.
#[test]
fn a_cascade_and_a_volume_add_up() {
    let settings = fx(
        -3.0,
        vec![
            band(BandKind::LowShelf, 200.0, -20.0, 0.707),
            band(BandKind::Peak, 2500.0, 4.0, 1.5),
            band(BandKind::HighPass, 40.0, 0.0, 0.707),
        ],
    );
    for f in [100.0, 400.0, 2500.0, 8000.0] {
        let want = response_db(&settings, f, f64::from(RATE));
        let got = measured_db(&settings, f);
        assert!((got - want).abs() < 0.05, "at {f} Hz: {got} vs {want}");
    }
}

/// The channels are filtered apart: a silent right channel stays silent.
#[test]
fn the_channels_are_filtered_apart() {
    let mut p = FxProcessor::new(
        &fx(-3.0, vec![band(BandKind::Peak, 500.0, 6.0, 1.0)]),
        RATE,
        2,
    );
    let mut samples: Vec<f32> = (0..2000)
        .flat_map(|n| [((n as f32) * 0.05).sin() * 0.5, 0.0])
        .collect();
    p.process(&mut samples);
    assert!(samples.iter().skip(1).step_by(2).all(|s| *s == 0.0));
    assert!(samples.iter().step_by(2).any(|s| s.abs() > 0.1));
    let mut mono = FxProcessor::new(&fx(-6.020_599_913_279_624, vec![]), RATE, 1);
    let mut one = vec![0.5f32, 0.5];
    mono.process(&mut one);
    assert!((one[0] - 0.25).abs() < 1e-6 && (one[1] - 0.25).abs() < 1e-6);
}

/// A volume change ramps over FX_RAMP_FRAMES, down and back up, and lands
/// exactly on the new volume.
#[test]
fn a_volume_change_ramps_and_lands_exactly() {
    let mut p = FxProcessor::new(&PlaylistFx::default(), RATE, 1);
    p.update(&fx(-20.0, vec![]));
    let mut dc = vec![1.0f32; 3000];
    p.process(&mut dc);
    let step = (0.1 - 1.0) / 2400.0;
    for k in [0usize, 1, 1200, 2399] {
        let want = 1.0 + step * k as f64;
        assert!(
            (f64::from(dc[k]) - want).abs() < 1e-5,
            "frame {k}: {} vs {want}",
            dc[k]
        );
    }
    assert!(
        dc[2400..].iter().all(|s| (s - 0.1).abs() < 1e-7),
        "{}",
        dc[2400]
    );
    for pair in dc.windows(2) {
        assert!(pair[1] <= pair[0] + 1e-7, "monotonic down");
    }
    p.update(&PlaylistFx::default());
    let mut back = vec![1.0f32; 3000];
    p.process(&mut back);
    assert!((back[1200] - 0.55).abs() < 1e-4, "{}", back[1200]);
    assert!(back[2400..].iter().all(|s| *s == 1.0));
    assert!(p.is_passthrough(), "back at the default: untouched again");
}

/// An EQ change fades from the old cascade's output into the new one's: at
/// frame k of the fade the output is w·old + (1 − w)·new, w = (2400 − k) /
/// 2400, and from the end on it is the new cascade alone.
#[test]
fn an_eq_change_crossfades_from_the_old_filters_to_the_new() {
    let hp = fx(0.0, vec![band(BandKind::HighPass, 100.0, 0.0, 0.707)]);
    let mut switched = FxProcessor::new(&PlaylistFx::default(), RATE, 1);
    let mut warm = vec![1.0f32; 100];
    switched.process(&mut warm);
    switched.update(&hp);
    assert!(!switched.is_passthrough());
    let mut fresh = FxProcessor::new(&hp, RATE, 1);
    let mut out = vec![1.0f32; 3000];
    let mut new_only = vec![1.0f32; 3000];
    switched.process(&mut out);
    fresh.process(&mut new_only);
    for k in [0usize, 1, 600, 1200, 2399] {
        let w = (2400 - k) as f64 / 2400.0;
        let want = w * 1.0 + (1.0 - w) * f64::from(new_only[k]);
        assert!(
            (f64::from(out[k]) - want).abs() < 1e-6,
            "frame {k}: {} vs {want}",
            out[k]
        );
    }
    for (k, (got, want)) in out.iter().zip(&new_only).enumerate().skip(2400) {
        assert_eq!(got, want, "frame {k}");
    }
}

/// An EQ change that lands while a crossfade still runs waits for its end:
/// the running fade is never cut short (a jump, a click), and the new
/// filters then fade in exactly as if they had been set at that moment.
#[test]
fn an_eq_change_during_a_crossfade_waits_for_its_end() {
    let a = fx(0.0, vec![band(BandKind::HighPass, 100.0, 0.0, 0.707)]);
    let b = fx(0.0, vec![band(BandKind::LowShelf, 200.0, -12.0, 0.707)]);
    let input: Vec<f32> = (0..7200).map(|n| ((n as f32) * 0.01).sin() * 0.5).collect();

    let mut early = FxProcessor::new(&PlaylistFx::default(), RATE, 1);
    early.update(&a);
    let mut out_early = input[..1200].to_vec();
    early.process(&mut out_early);
    early.update(&b);
    let mut rest = input[1200..].to_vec();
    early.process(&mut rest);
    out_early.extend(rest);

    let mut at_end = FxProcessor::new(&PlaylistFx::default(), RATE, 1);
    at_end.update(&a);
    let mut out_at_end = input[..2400].to_vec();
    at_end.process(&mut out_at_end);
    at_end.update(&b);
    let mut rest = input[2400..].to_vec();
    at_end.process(&mut rest);
    out_at_end.extend(rest);

    for (k, (got, want)) in out_early.iter().zip(&out_at_end).enumerate() {
        assert_eq!(got, want, "frame {k}");
    }
    assert!(!early.is_passthrough(), "B's shelf stays on");
}

/// A volume-only change keeps the filters running: no crossfade, the same
/// samples as a processor that never changed its EQ.
#[test]
fn a_volume_only_change_keeps_the_filters_state() {
    let eq = vec![band(BandKind::Peak, 800.0, 6.0, 1.0)];
    let mut changed = FxProcessor::new(&fx(0.0, eq.clone()), RATE, 1);
    let mut steady = FxProcessor::new(&fx(0.0, eq.clone()), RATE, 1);
    let input: Vec<f32> = (0..4000).map(|n| ((n as f32) * 0.1).sin() * 0.5).collect();
    let (mut a, mut b) = (input[..2000].to_vec(), input[..2000].to_vec());
    changed.process(&mut a);
    steady.process(&mut b);
    changed.update(&fx(0.0, eq));
    let (mut a2, mut b2) = (input[2000..].to_vec(), input[2000..].to_vec());
    changed.process(&mut a2);
    steady.process(&mut b2);
    assert_eq!(a2, b2);
}
