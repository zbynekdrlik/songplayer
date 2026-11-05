//! #285: a Dante output's chips and reasons.

use super::*;
use crate::asio_resampling::LastFault;

fn figures() -> AsioFigures {
    AsioFigures {
        latency_ms: 76.5,
        underruns: 2,
        hard_recentres: 1,
        driver_rate: 96_000,
        rate_ppm: 1.5,
        locked: true,
        ppm: -0.5,
        offset_ms: 0.0,
        slew_eta_s: None,
        cushion_ms: 0.0,
        last_fault: Some(LastFault {
            cause: "deficit".into(),
            ms: 2.0,
            lateness_ms: 1.0,
            ago_s: 5.0,
        }),
        resyncs: 1,
    }
}

fn dante() -> DanteFigures {
    DanteFigures {
        clock_ready: true,
        live_flows: 1,
        late_bins: vec![10, 0, 0, 0, 0, 0],
        late_max_us: 1_240,
        send_max_us: 80,
        ..DanteFigures::default()
    }
}

fn text_of(chips: &[Chip], key: &str) -> String {
    chips
        .iter()
        .find(|c| c.key == key)
        .map(|c| c.text.clone())
        .unwrap_or_default()
}

#[test]
fn no_chip_of_a_dante_output_speaks_of_a_card_or_a_driver() {
    let f = figures();
    let chips = [
        dante_state_chips("beží", &f, Some(&dante())),
        dante_resampling_chips(&f),
    ]
    .concat();
    assert!(chips.len() >= 10);
    for c in chips {
        let all = format!("{} {}", c.text, c.title.unwrap_or_default());
        for word in ["kart", "Kart", "ovládač", "Ovládač", "ASIO"] {
            assert!(!all.contains(word), "{word} in {all}");
        }
    }
}

#[test]
fn the_card_line_names_the_network() {
    let chips = dante_resampling_chips(&figures());
    assert_eq!(
        text_of(&chips, "asio-card"),
        "sieť Dante +1,5 ppm voči SongPlayeru (odhad zamknutý)"
    );
    let measuring = AsioFigures {
        locked: false,
        ..figures()
    };
    assert_eq!(
        text_of(&dante_resampling_chips(&measuring), "asio-card"),
        "sieť Dante voči SongPlayeru: odhad sa ešte meria"
    );
}

#[test]
fn the_state_line_adds_the_clock_the_flows_and_the_lateness() {
    let chips = dante_state_chips("beží", &figures(), Some(&dante()));
    assert_eq!(text_of(&chips, "dante-clock"), "hodiny Dante v poriadku");
    assert_eq!(text_of(&chips, "dante-flows"), "prúdy 1");
    assert_eq!(
        text_of(&chips, "dante-late"),
        "paket neskoro max +1,2 ms · odoslanie max +0,1 ms"
    );
    let waiting = DanteFigures {
        clock_ready: false,
        ..dante()
    };
    let chips = dante_state_chips("beží", &figures(), Some(&waiting));
    assert_eq!(text_of(&chips, "dante-clock"), "čaká na hodiny Dante");
}

#[test]
fn the_pump_shows_only_once_it_slipped() {
    let clean = dante_state_chips("beží", &figures(), Some(&dante()));
    assert!(clean.iter().all(|c| c.key != "dante-pump"));
    for (late, missed, over) in [(1, 0, 0), (0, 1, 0), (0, 0, 1)] {
        let d = DanteFigures {
            pump_late: late,
            pump_missed: missed,
            pump_overruns: over,
            ..dante()
        };
        let chips = dante_state_chips("beží", &figures(), Some(&d));
        assert_eq!(
            text_of(&chips, "dante-pump"),
            format!("bloky: neskoro {late} · vynechané {missed} · nestihnuté {over}")
        );
    }
}

#[test]
fn with_no_answer_from_the_stack_the_line_claims_nothing_of_it() {
    let chips = dante_state_chips("beží", &figures(), None);
    assert!(chips.iter().all(|c| !c.key.starts_with("dante-")));
}

#[test]
fn every_reason_says_dante_and_an_unknown_code_reads_as_such() {
    for code in [
        "refused",
        "failed",
        "reset",
        "rate_changed",
        "clock_lost",
        "stalled",
        "held",
        "parked",
        "no_clock",
    ] {
        let text = dante_reason_sk(code);
        assert!(text.contains("Dante"), "{code}: {text}");
        assert!(!text.contains("ovládač"), "{code}: {text}");
    }
    assert_eq!(dante_reason_sk("later_version"), "neznámy dôvod");
}

#[test]
fn a_waiting_output_reads_its_reason_and_next_try() {
    assert_eq!(
        dante_waiting_text("čaká", Some("failed"), Some(10.0)),
        "čaká · chyba zariadenia Dante · ďalší pokus o 10 s"
    );
    assert_eq!(dante_waiting_text("čaká", None, None), "čaká");
}

#[test]
fn a_parked_device_shows_no_next_try() {
    assert_eq!(
        dante_waiting_text("čaká", Some("parked"), Some(10.0)),
        "čaká · zariadenie Dante zamrzlo — pomôže len reštart SongPlayera"
    );
}

#[test]
fn the_figures_read_with_any_field_missing() {
    let read: DanteFigures = serde_json::from_str(r#"{"clock_ready":true}"#).unwrap();
    assert!(read.clock_ready);
    assert_eq!(read.late_max_us, 0);
}
