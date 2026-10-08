//! #233: a running ASIO output's chips — the Slovak numbers and their
//! rounding edges, the two lines, the tooltips.

use super::*;

const MINUS: &str = "\u{2212}";

#[test]
fn tenths_round_half_away_from_zero_with_a_comma_and_a_true_minus() {
    assert_eq!(tenths_sk(0.0), "0,0");
    assert_eq!(tenths_sk(0.04), "0,0");
    assert_eq!(tenths_sk(-0.04), "0,0", "no sign on a zero");
    assert_eq!(tenths_sk(0.25), "+0,3", "half away from zero");
    assert_eq!(tenths_sk(0.2499), "+0,2");
    assert_eq!(tenths_sk(-0.25), format!("{MINUS}0,3"));
    assert_eq!(tenths_sk(12.34), "+12,3");
    assert_eq!(tenths_sk(-65.04), format!("{MINUS}65,0"));
    assert_eq!(tenths_sk(300.0), "+300,0");
}

#[test]
fn a_rate_reads_in_khz_with_a_comma() {
    assert_eq!(khz_sk(48_000), "48");
    assert_eq!(khz_sk(96_000), "96");
    assert_eq!(khz_sk(192_000), "192");
    assert_eq!(khz_sk(44_100), "44,1");
    assert_eq!(khz_sk(88_200), "88,2");
    assert_eq!(khz_sk(22_050), "22,05");
    assert_eq!(khz_sk(11_025), "11,025");
}

#[test]
fn ago_is_seconds_then_minutes_then_hours() {
    assert_eq!(ago_sk(0.0), "pred 0 s");
    assert_eq!(ago_sk(-3.0), "pred 0 s", "a clock read back");
    assert_eq!(ago_sk(59.9), "pred 59 s");
    assert_eq!(ago_sk(60.0), "pred 1 min");
    assert_eq!(ago_sk(185.0), "pred 3 min");
    assert_eq!(ago_sk(3_599.9), "pred 59 min");
    assert_eq!(ago_sk(3_600.0), "pred 1 h");
    assert_eq!(ago_sk(7_300.0), "pred 2 h");
}

#[test]
fn the_time_left_is_seconds_under_two_minutes_then_minutes() {
    assert_eq!(eta_sk(44.6), "ešte asi 45 s");
    assert_eq!(eta_sk(119.4), "ešte asi 119 s");
    assert_eq!(eta_sk(119.5), "ešte asi 2 min", "rounded to 120");
    assert_eq!(eta_sk(149.0), "ešte asi 2 min");
    assert_eq!(eta_sk(150.0), "ešte asi 3 min", "the nearest minute");
    assert_eq!(eta_sk(-1.0), "ešte asi 0 s");
}

#[test]
fn a_faults_cause_reads_in_slovak() {
    assert_eq!(fault_cause_sk("deficit"), "zásobník by vyschol");
    assert_eq!(fault_cause_sk("excess"), "zásobník by pretiekol");
    assert_eq!(fault_cause_sk("later"), "neznámy dôvod");
}

fn texts(chips: &[Chip]) -> Vec<(&'static str, &str)> {
    chips.iter().map(|c| (c.key, c.text.as_str())).collect()
}

fn running() -> AsioFigures {
    AsioFigures {
        latency_ms: 70.7,
        underruns: 0,
        hard_recentres: 0,
        driver_rate: 96_000,
        rate_ppm: 0.4,
        locked: true,
        ppm: 0.4,
        offset_ms: 0.0,
        slew_eta_s: None,
        last_fault: None,
    }
}

/// The owner's example: `48 → 96 kHz`, the card's clock against
/// SongPlayer's with the lock, the correction and its sign.
#[test]
fn a_settled_output_reads_its_two_lines() {
    let f = running();
    assert_eq!(
        texts(&asio_state_chips("beží", &f)),
        [
            ("asio-state", "beží"),
            ("asio-latency", "oneskorenie 71 ms"),
            ("asio-underruns", "výpadky 0"),
            ("asio-faults", "núdzové skoky 0"),
        ]
    );
    assert_eq!(
        texts(&asio_resampling_chips(&f)),
        [
            ("asio-conversion", "48 → 96 kHz"),
            (
                "asio-card",
                "karta +0,4 ppm voči SongPlayeru (odhad zamknutý)"
            ),
            ("asio-correction", "korekcia +0,4 ppm (pridáva vzorky)"),
            ("asio-slew", "oneskorenie v cieli"),
        ]
    );
}

/// Every figure carries its own Slovak tooltip; the state shows the row's.
#[test]
fn every_figure_but_the_state_has_its_tooltip() {
    let f = AsioFigures {
        last_fault: Some(LastFault::default()),
        ..running()
    };
    let titles: Vec<_> = asio_state_chips("beží", &f)
        .into_iter()
        .chain(asio_resampling_chips(&f))
        .map(|c| (c.key, c.title))
        .collect();
    assert_eq!(titles[0], ("asio-state", None));
    assert_eq!(titles[1].1.as_deref(), Some(LATENCY_TIP));
    assert_eq!(titles[2].1.as_deref(), Some(UNDERRUNS_TIP));
    assert_eq!(titles[3].1.as_deref(), Some(FAULTS_TIP));
    assert_eq!(titles[4].1.as_deref(), Some(CONVERSION_TIP));
    assert_eq!(titles[5].1.as_deref(), Some(CARD_TIP));
    assert_eq!(titles[6].1.as_deref(), Some(CORRECTION_TIP));
    assert_eq!(titles[7].1.as_deref(), Some(SLEW_TIP));
    assert_eq!(titles[8].0, "asio-last-fault");
    assert!(
        titles[8]
            .1
            .as_deref()
            .is_some_and(|t| t.starts_with("Núdzový skok"))
    );
}

/// While it slews, before the lock, after a fault: what each line says.
#[test]
fn a_slewing_output_with_a_fault_reads_what_works_and_what_failed() {
    let f = AsioFigures {
        latency_ms: 0.0,
        underruns: 3,
        hard_recentres: 2,
        driver_rate: 44_100,
        rate_ppm: -0.83,
        locked: false,
        ppm: -0.66,
        offset_ms: 12.34,
        slew_eta_s: Some(44.6),
        last_fault: Some(LastFault {
            cause: "deficit".into(),
            ms: 65.04,
            lateness_ms: -60.0,
            ago_s: 185.0,
        }),
    };
    assert_eq!(
        texts(&asio_state_chips("beží", &f))[1..],
        [
            ("asio-latency", "oneskorenie: meria sa"),
            ("asio-underruns", "výpadky 3"),
            ("asio-faults", "núdzové skoky 2"),
        ]
    );
    let line = asio_resampling_chips(&f);
    assert_eq!(
        texts(&line),
        [
            ("asio-conversion", "48 → 44,1 kHz"),
            ("asio-card", "karta voči SongPlayeru: odhad sa ešte meria"),
            (
                "asio-correction",
                &format!("korekcia {MINUS}0,7 ppm (uberá vzorky)")[..]
            ),
            ("asio-slew", "dorovnáva odchýlku +12,3 ms · ešte asi 45 s"),
            (
                "asio-last-fault",
                "posledný núdzový skok pred 3 min: +65,0 ms (zásobník by vyschol)"
            ),
        ]
    );
    assert_eq!(
        line[4].title.as_deref(),
        Some(
            &format!(
                "Núdzový skok: +65,0 ms (+ vložené ticho, − preskočený zvuk), lebo zásobník by vyschol. Blok programu vtedy prišiel {MINUS}60,0 ms po svojej hranici."
            )[..]
        )
    );
}

/// The program's own rate needs no conversion; a correction that rounds to
/// zero has no sign and no gloss.
#[test]
fn a_48k_driver_converts_nothing_and_a_zero_correction_has_no_gloss() {
    let f = AsioFigures {
        driver_rate: 48_000,
        ppm: 0.04,
        ..running()
    };
    let line = asio_resampling_chips(&f);
    assert_eq!(line[0].text, "48 kHz bez prevodu");
    assert_eq!(line[2].text, "korekcia 0,0 ppm");
    let f = AsioFigures {
        ppm: -0.05,
        ..running()
    };
    assert_eq!(
        asio_resampling_chips(&f)[2].text,
        format!("korekcia {MINUS}0,1 ppm (uberá vzorky)")
    );
}
