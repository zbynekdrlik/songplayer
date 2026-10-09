//! #242: a playlist's volume and parametric EQ — the limits, the RBJ
//! filters' known values, the summed response and the dashboard's curve.
//! Wired via `#[cfg(test)] #[path = "audio_fx_tests.rs"] mod tests;`.

use super::{
    BandKind, Biquad, CURVE_DB_MAX, CURVE_DB_MIN, DEFAULT_Q, EqBand, FX_RATE_HZ, FxError,
    MAX_BANDS, PlaylistFx, biquad_db, coefficients, curve, curve_path, db_y, freq_x, response_db,
    validate,
};

const RATE: f64 = 48_000.0;

fn band(kind: BandKind, freq_hz: f64, gain_db: f64, q: f64) -> EqBand {
    EqBand {
        kind,
        freq_hz,
        gain_db,
        q,
        enabled: true,
    }
}

fn db(b: &EqBand, freq: f64) -> f64 {
    biquad_db(&coefficients(b, RATE), freq, RATE)
}

fn close(got: f64, want: f64, tol: f64) {
    assert!((got - want).abs() <= tol, "{got} vs {want} (±{tol})");
}

#[test]
fn the_limits_and_the_defaults() {
    assert_eq!(MAX_BANDS, 8);
    close(DEFAULT_Q, std::f64::consts::FRAC_1_SQRT_2, 0.0);
    assert_eq!(FX_RATE_HZ, 48_000.0);
    assert!(PlaylistFx::default().is_identity());
    assert!(validate(&PlaylistFx::default()).is_ok());
}

#[test]
fn a_band_reads_from_json_with_its_defaults_and_refuses_unknown_fields() {
    let b: EqBand = serde_json::from_str(r#"{"kind":"low_shelf","freq_hz":200}"#).unwrap();
    assert_eq!(b, band(BandKind::LowShelf, 200.0, 0.0, DEFAULT_Q));
    for (json, kind) in [
        ("high_pass", BandKind::HighPass),
        ("low_shelf", BandKind::LowShelf),
        ("peak", BandKind::Peak),
        ("high_shelf", BandKind::HighShelf),
        ("low_pass", BandKind::LowPass),
    ] {
        let parsed: BandKind = serde_json::from_str(&format!("\"{json}\"")).unwrap();
        assert_eq!(parsed, kind);
    }
    assert!(serde_json::from_str::<EqBand>(r#"{"kind":"peak","freq_hz":1,"x":1}"#).is_err());
    assert!(serde_json::from_str::<PlaylistFx>(r#"{"gain_db":0,"bands":[]}"#).is_err());
    let off: EqBand =
        serde_json::from_str(r#"{"kind":"peak","freq_hz":1000,"enabled":false}"#).unwrap();
    assert!(!off.enabled);
}

#[test]
fn only_a_shelf_or_a_peak_uses_its_gain() {
    assert!(!BandKind::HighPass.uses_gain());
    assert!(!BandKind::LowPass.uses_gain());
    assert!(BandKind::LowShelf.uses_gain());
    assert!(BandKind::Peak.uses_gain());
    assert!(BandKind::HighShelf.uses_gain());
}

/// Every limit is inclusive; a value past it, or not a number, is refused
/// with the field and the band named.
#[test]
fn every_limit_is_inclusive_and_names_what_is_refused() {
    let ok = |fx: PlaylistFx| assert!(validate(&fx).is_ok(), "{fx:?}");
    let with = |b: EqBand| PlaylistFx {
        gain_db: 0.0,
        eq: vec![band(BandKind::Peak, 1000.0, 0.0, 1.0), b],
    };
    for gain_db in [-30.0, 12.0] {
        ok(PlaylistFx {
            gain_db,
            eq: vec![],
        });
    }
    for g in [-30.000_1, 12.000_1, f64::NAN] {
        let err = validate(&PlaylistFx {
            gain_db: g,
            eq: vec![],
        })
        .unwrap_err();
        assert!(matches!(err, FxError::Gain(_)), "{err:?}");
    }
    ok(with(band(BandKind::Peak, 20.0, -30.0, 0.1)));
    ok(with(band(BandKind::Peak, 20_000.0, 12.0, 10.0)));
    assert_eq!(
        validate(&with(band(BandKind::Peak, 19.9, 0.0, 1.0))),
        Err(FxError::Freq {
            index: 1,
            value: 19.9
        })
    );
    assert_eq!(
        validate(&with(band(BandKind::Peak, 20_001.0, 0.0, 1.0))),
        Err(FxError::Freq {
            index: 1,
            value: 20_001.0
        })
    );
    assert_eq!(
        validate(&with(band(BandKind::Peak, 100.0, 12.5, 1.0))),
        Err(FxError::BandGain {
            index: 1,
            value: 12.5
        })
    );
    assert_eq!(
        validate(&with(band(BandKind::Peak, 100.0, -30.5, 1.0))),
        Err(FxError::BandGain {
            index: 1,
            value: -30.5
        })
    );
    assert_eq!(
        validate(&with(band(BandKind::Peak, 100.0, 0.0, 0.09))),
        Err(FxError::Q {
            index: 1,
            value: 0.09
        })
    );
    assert_eq!(
        validate(&with(band(BandKind::Peak, 100.0, 0.0, 10.1))),
        Err(FxError::Q {
            index: 1,
            value: 10.1
        })
    );
    let eight = PlaylistFx {
        gain_db: 0.0,
        eq: vec![band(BandKind::Peak, 1000.0, 0.0, 1.0); 8],
    };
    ok(eight.clone());
    let mut nine = eight;
    nine.eq.push(band(BandKind::Peak, 1000.0, 0.0, 1.0));
    assert_eq!(validate(&nine), Err(FxError::TooManyBands(9)));
}

#[test]
fn a_refusal_reads_in_slovak_with_the_band_counted_from_one() {
    assert_eq!(
        FxError::Gain(20.0).sk(),
        "Hlasitosť musí byť od −30 do +12 dB"
    );
    assert_eq!(
        FxError::TooManyBands(9).sk(),
        "EQ môže mať najviac 8 pásiem"
    );
    assert_eq!(
        FxError::Freq {
            index: 0,
            value: 5.0
        }
        .sk(),
        "Pásmo 1: frekvencia musí byť od 20 do 20 000 Hz"
    );
    assert_eq!(
        FxError::BandGain {
            index: 2,
            value: 50.0
        }
        .sk(),
        "Pásmo 3: zisk musí byť od −30 do +12 dB"
    );
    assert_eq!(
        FxError::Q {
            index: 1,
            value: 20.0
        }
        .sk(),
        "Pásmo 2: Q musí byť od 0,1 do 10"
    );
    assert_eq!(
        FxError::Freq {
            index: 0,
            value: 5.0
        }
        .to_string(),
        "audio_eq band 0: freq_hz 5 is outside 20..=20000 Hz"
    );
}

/// A peak reaches its gain at its frequency and leaves far frequencies alone.
#[test]
fn a_peak_reaches_its_gain_at_its_frequency() {
    close(
        db(&band(BandKind::Peak, 1000.0, 6.0, 1.0), 1000.0),
        6.0,
        1e-9,
    );
    close(
        db(&band(BandKind::Peak, 1000.0, -12.0, 2.0), 1000.0),
        -12.0,
        1e-9,
    );
    close(
        db(&band(BandKind::Peak, 1000.0, -12.0, 2.0), 20.0),
        0.0,
        0.01,
    );
}

/// A shelf holds its full gain at its own end of the spectrum, half of it
/// (in dB) at its frequency, and 0 dB at the other end.
#[test]
fn a_shelf_holds_its_gain_on_its_side_and_half_at_its_frequency() {
    let low = band(BandKind::LowShelf, 200.0, -20.0, DEFAULT_Q);
    close(db(&low, 10.0), -20.0, 0.001);
    close(db(&low, 200.0), -10.0, 1e-9);
    close(db(&low, 20_000.0), 0.0, 1e-6);
    let high = band(BandKind::HighShelf, 5000.0, 6.0, DEFAULT_Q);
    close(db(&high, 23_900.0), 6.0, 1e-6);
    close(db(&high, 5000.0), 3.0, 1e-9);
    close(db(&high, 20.0), 0.0, 1e-6);
}

/// A high-/low-pass is 20·log10(Q) at its frequency (−3.01 dB for the
/// default Q), falls 12 dB per octave beyond it and passes the other side.
#[test]
fn a_pass_filter_reads_q_at_its_frequency_and_cuts_beyond_it() {
    let hp = band(BandKind::HighPass, 100.0, 0.0, DEFAULT_Q);
    close(db(&hp, 100.0), -3.010_299_956_6, 1e-6);
    close(db(&hp, 10.0), -40.0, 0.01);
    close(db(&hp, 10_000.0), 0.0, 1e-6);
    close(
        db(&band(BandKind::HighPass, 100.0, 0.0, 2.0), 100.0),
        6.020_599_913,
        1e-6,
    );
    let lp = band(BandKind::LowPass, 8000.0, 0.0, DEFAULT_Q);
    close(db(&lp, 8000.0), -3.010_299_956_6, 1e-6);
    close(db(&lp, 100.0), 0.0, 1e-6);
    assert!(db(&lp, 20_000.0) < -15.0, "{}", db(&lp, 20_000.0));
    let hp_gain = band(BandKind::HighPass, 100.0, 9.0, DEFAULT_Q);
    close(db(&hp_gain, 100.0), db(&hp, 100.0), 1e-12);
}

/// A frequency at or past 0.49 × the rate is filtered at 0.49 × the rate.
#[test]
fn a_frequency_past_the_nyquist_margin_is_held_at_it() {
    let at = coefficients(&band(BandKind::Peak, 23_520.0, 6.0, 1.0), RATE);
    let past = coefficients(&band(BandKind::Peak, 30_000.0, 6.0, 1.0), RATE);
    assert_eq!(at, past);
    let filter: Biquad = at;
    close(biquad_db(&filter, 23_520.0, RATE), 6.0, 1e-9);
}

/// One cookbook pin: kind, freq, gain, Q, rate, then b0 b1 b2 a1 a2.
type Pin = (BandKind, f64, f64, f64, f64, [f64; 5]);

/// Every band type's five coefficients equal an independent model of the
/// RBJ Audio EQ Cookbook (a scratch Python implementation, off the
/// filter's centre so Q and the shelf term both count), at two rates.
#[test]
fn the_coefficients_equal_the_cookbook() {
    let pins: [Pin; 7] = [
        (
            BandKind::HighPass,
            1234.0,
            -7.5,
            0.8,
            48000.0,
            [
                0.9027487292187271,
                -1.8054974584374541,
                0.9027487292187271,
                -1.7936687873962442,
                0.8173261294786641,
            ],
        ),
        (
            BandKind::LowPass,
            1234.0,
            -7.5,
            0.8,
            48000.0,
            [
                0.00591433552060498,
                0.01182867104120996,
                0.00591433552060498,
                -1.7936687873962442,
                0.8173261294786641,
            ],
        ),
        (
            BandKind::Peak,
            1234.0,
            -7.5,
            0.8,
            48000.0,
            [
                0.9224831216335619,
                -1.7093707677260486,
                0.8094331513061258,
                -1.7093707677260486,
                0.7319162729396876,
            ],
        ),
        (
            BandKind::LowShelf,
            1234.0,
            -7.5,
            0.8,
            48000.0,
            [
                0.9560990194670183,
                -1.7536250942501945,
                0.812511098240339,
                -1.7433500606762562,
                0.7788851512812958,
            ],
        ),
        (
            BandKind::HighShelf,
            1234.0,
            -7.5,
            0.8,
            48000.0,
            [
                0.44105944556209137,
                -0.7689210111825079,
                0.3435346529806739,
                -1.8341458975951683,
                0.8498189849554255,
            ],
        ),
        (
            BandKind::LowShelf,
            321.0,
            5.0,
            2.5,
            44100.0,
            [
                1.0029232506164754,
                -1.9821243358962148,
                0.981968332710679,
                -1.9827298908179622,
                0.9842860284054064,
            ],
        ),
        (
            BandKind::HighShelf,
            7000.0,
            5.0,
            2.5,
            44100.0,
            [
                1.5234787580656226,
                -1.6791803852920715,
                1.11614706441306,
                -0.7340988844657508,
                0.6945443216523614,
            ],
        ),
    ];
    for (kind, freq, gain, q, rate, want) in pins {
        let c = coefficients(&band(kind, freq, gain, q), rate);
        let got = [c.b0, c.b1, c.b2, c.a1, c.a2];
        for (i, (g, w)) in got.iter().zip(want).enumerate() {
            assert!(
                (g - w).abs() <= 1e-12,
                "{kind:?} {freq} Hz at {rate}: coefficient {i} {g} vs {w}"
            );
        }
    }
}

/// The filter's gain away from its centre equals the model's |H(e^jw)|.
#[test]
fn the_gain_off_centre_equals_the_transfer_function() {
    let peak = band(BandKind::Peak, 1234.0, -7.5, 0.8);
    for (freq, want) in [
        (100.0, -0.08640456054347673),
        (900.0, -5.840719510889913),
        (3000.0, -2.1121171008690003),
        (15000.0, -0.03867757263738937),
    ] {
        close(db(&peak, freq), want, 1e-9);
    }
    let shelf = coefficients(&band(BandKind::LowShelf, 321.0, 5.0, 2.5), 44_100.0);
    close(biquad_db(&shelf, 150.0, 44_100.0), 6.210337097116182, 1e-9);
    close(
        biquad_db(&shelf, 500.0, 44_100.0),
        -2.5796194719951027,
        1e-9,
    );
}

/// The playlist's response is its volume plus every ENABLED band.
#[test]
fn the_response_adds_the_volume_and_every_enabled_band() {
    let peak = band(BandKind::Peak, 1000.0, 6.0, 1.0);
    let shelf = band(BandKind::LowShelf, 200.0, -20.0, DEFAULT_Q);
    let mut off = band(BandKind::HighShelf, 5000.0, 12.0, DEFAULT_Q);
    off.enabled = false;
    let fx = PlaylistFx {
        gain_db: -5.0,
        eq: vec![peak, shelf, off],
    };
    for freq in [30.0, 200.0, 1000.0, 5000.0, 15_000.0] {
        close(
            response_db(&fx, freq, RATE),
            -5.0 + db(&peak, freq) + db(&shelf, freq),
            1e-9,
        );
    }
    close(
        response_db(&fx, 1000.0, RATE),
        -5.0 + 6.0 + db(&shelf, 1000.0),
        1e-9,
    );
    assert!(!fx.is_identity());
    let only_off = PlaylistFx {
        gain_db: 0.0,
        eq: vec![off],
    };
    assert!(only_off.is_identity());
    close(response_db(&only_off, 1000.0, RATE), 0.0, 0.0);
    assert!(
        !PlaylistFx {
            gain_db: 0.5,
            eq: vec![]
        }
        .is_identity()
    );
}

/// The curve runs from 20 Hz to 20 kHz on a log axis, at the program rate.
#[test]
fn the_curve_spans_20_hz_to_20_khz_on_a_log_axis() {
    let fx = PlaylistFx {
        gain_db: -5.0,
        eq: vec![band(BandKind::Peak, 1000.0, 6.0, 1.0)],
    };
    let points = curve(&fx, 4);
    assert_eq!(points.len(), 4);
    let freqs: Vec<f64> = points.iter().map(|p| p.0).collect();
    close(freqs[0], 20.0, 1e-9);
    close(freqs[1], 200.0, 1e-6);
    close(freqs[2], 2000.0, 1e-5);
    close(freqs[3], 20_000.0, 1e-4);
    for (freq, gain) in &points {
        close(*gain, response_db(&fx, *freq, FX_RATE_HZ), 0.0);
    }
    assert_eq!(curve(&fx, 0).len(), 2);
    assert_eq!(curve(&fx, 1).len(), 2);
}

/// The dashboard's axes: 20 Hz…20 kHz on a log x axis, +18…−36 dB down the
/// y axis, clamped to the box.
#[test]
fn the_curve_axes_map_frequency_and_decibels_into_the_box() {
    assert_eq!((CURVE_DB_MIN, CURVE_DB_MAX), (-36.0, 18.0));
    close(freq_x(20.0, 300.0), 0.0, 1e-12);
    close(freq_x(20_000.0, 300.0), 300.0, 1e-9);
    close(freq_x((20.0f64 * 20_000.0).sqrt(), 300.0), 150.0, 1e-9);
    close(freq_x(200.0, 300.0), 100.0, 1e-9);
    close(db_y(18.0, 108.0), 0.0, 1e-12);
    close(db_y(-36.0, 108.0), 108.0, 1e-12);
    close(db_y(0.0, 108.0), 36.0, 1e-12);
    close(db_y(-9.0, 108.0), 54.0, 1e-12);
    close(db_y(40.0, 108.0), 0.0, 0.0);
    close(db_y(-90.0, 108.0), 108.0, 0.0);
}

#[test]
fn the_curve_path_runs_through_the_mapped_points() {
    assert_eq!(
        curve_path(&PlaylistFx::default(), 300.0, 108.0, 3),
        "M0.0 36.0 L150.0 36.0 L300.0 36.0"
    );
    let quieter = PlaylistFx {
        gain_db: -9.0,
        eq: vec![],
    };
    assert_eq!(
        curve_path(&quieter, 300.0, 108.0, 2),
        "M0.0 54.0 L300.0 54.0"
    );
}
