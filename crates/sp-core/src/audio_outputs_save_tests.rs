//! #233 release review: the dashboard's save decisions — the re-read check
//! (a changed list, a changed rate, a pending migration), the rate sent only
//! when changed, the unreadable number fields, "Uložené" after an edit, one
//! row removed, a waiting VBAN row's Slovak, every row's latency label.

use std::collections::HashMap;

use super::*;
use crate::audio_outputs::{OutputEntry, VbanDest, VbanSampleFormat};

const FOH: &str = r#"[{"id":"out-1","name":"FOH","type":"vban","enabled":true,"rate":48000,"delay_ms":0,"vban":{"host":"fohabl.lan","port":6980,"stream_name":"sp-program","format":"int24"}}]"#;
/// The same list as the server stores it after its own normalization would
/// differ only in layout: here, the keys reordered and spaced.
const FOH_RESPACED: &str = r#"[ {"name":"FOH","id":"out-1","type":"vban","rate":48000,"enabled":true,"delay_ms":0,"vban":{"port":6980,"host":"fohabl.lan","stream_name":"sp-program","format":"int24"}} ]"#;
const LV1: &str = r#"[{"id":"out-2","name":"lv1","type":"vban","enabled":true,"rate":"network","delay_ms":0,"vban":{"host":"lv1.lan","port":6980,"stream_name":"sp-program","format":"int24"}}]"#;

fn settings(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn vban(id: &str, name: &str) -> OutputEntry {
    OutputEntry::vban(
        id,
        name,
        VbanDest {
            host: "h".into(),
            port: 6980,
            stream_name: "sp-program".into(),
            format: VbanSampleFormat::Int24,
        },
    )
}

#[test]
fn the_same_list_is_compared_as_a_list_else_as_text() {
    assert!(same_list(Some(FOH), Some(FOH_RESPACED)), "layout only");
    assert!(!same_list(Some(FOH), Some(LV1)));
    assert!(same_list(None, Some("  ")), "absent = blank");
    assert!(same_list(Some(""), None));
    assert!(
        !same_list(None, Some("[]")),
        "an empty list is a stored list"
    );
    assert!(
        same_list(Some("not json"), Some(" not json ")),
        "unreadable: text"
    );
    assert!(!same_list(Some("not json"), Some("other")));
    assert!(!same_list(Some(FOH), Some("not json")));
}

#[test]
fn the_migration_is_pending_while_no_list_and_an_old_key_exist() {
    for key in ["vban_enabled", "vban_stream_name", "vban_targets"] {
        assert!(migration_pending(&settings(&[(key, "x")])), "{key}");
    }
    assert!(
        !migration_pending(&settings(&[
            ("vban_targets", "fohabl.lan:6980"),
            ("audio_outputs", "")
        ])),
        "a stored list, even blank, ends it (the server's migration reads the key)"
    );
    assert!(!migration_pending(&settings(&[("gemini_model", "m")])));
}

#[test]
fn a_save_is_refused_when_the_server_moved_since_the_load() {
    let now = settings(&[("audio_outputs", FOH), ("audio_network_rate", "96000")]);
    assert_eq!(
        save_refusal(Some(FOH_RESPACED), Some("96000"), &now, true),
        None
    );
    assert_eq!(
        save_refusal(Some(LV1), Some("96000"), &now, false),
        Some(CHANGED_ON_SERVER),
        "the list changed elsewhere"
    );
    assert_eq!(
        save_refusal(None, Some("96000"), &now, false),
        Some(CHANGED_ON_SERVER),
        "a list stored elsewhere since an empty load"
    );
    assert_eq!(
        save_refusal(Some(FOH), Some("48000"), &now, true),
        Some(CHANGED_ON_SERVER),
        "the rate changed elsewhere and this save sends one"
    );
    assert_eq!(
        save_refusal(Some(FOH), Some("48000"), &now, false),
        None,
        "a rate this save does not send is not its concern"
    );
    assert_eq!(
        save_refusal(Some(FOH), None, &settings(&[("audio_outputs", FOH)]), true),
        None,
        "absent both times: the default"
    );
    let pending = settings(&[("vban_targets", "fohabl.lan:6980")]);
    assert_eq!(
        save_refusal(None, None, &pending, false),
        Some(MIGRATION_PENDING),
        "first, before any other check"
    );
}

#[test]
fn the_rate_is_sent_only_when_it_changed() {
    assert_eq!(rate_to_send(None, "48000"), None, "the default, untouched");
    assert_eq!(rate_to_send(None, " 96000 "), Some("96000".to_string()));
    assert_eq!(rate_to_send(Some("96000"), "96000"), None);
    assert_eq!(
        rate_to_send(Some("96000"), "48000"),
        Some("48000".to_string())
    );
}

#[test]
fn a_number_field_holds_a_whole_number_or_is_refused_in_slovak() {
    assert_eq!(parse_whole(" 25 "), Some(25));
    assert_eq!(parse_whole("0"), Some(0));
    for bad in ["", " ", "2.5", "-1", "1e3", "25ms"] {
        assert_eq!(parse_whole(bad), None, "{bad:?}");
    }
    assert_eq!(
        not_a_number_sk(0, "out-1", "delay_ms"),
        "Výstup 1 (out-1): pole „oneskorenie“ nie je celé číslo"
    );
    assert_eq!(
        not_a_number_sk(2, "Out_3", "asio.channels"),
        "Výstup 3 (?ut?3): pole „kanály“ nie je celé číslo"
    );
}

#[test]
fn the_first_unreadable_field_follows_the_rows_and_forgets_a_removed_one() {
    let list = vec![vban("out-1", "a"), vban("out-2", "b")];
    let bad = vec![
        ("out-9".to_string(), "delay_ms"),
        ("out-2".to_string(), "asio.channels"),
        ("out-1".to_string(), "delay_ms"),
    ];
    assert_eq!(
        first_unreadable(&list, &bad).as_deref(),
        Some("Výstup 1 (out-1): pole „oneskorenie“ nie je celé číslo"),
        "the first row's, whatever the order typed"
    );
    assert_eq!(
        first_unreadable(&list[1..], &bad).as_deref(),
        Some("Výstup 1 (out-2): pole „kanály“ nie je celé číslo")
    );
    assert_eq!(first_unreadable(&list, &bad[..1]), None, "out-9 is gone");
    assert_eq!(first_unreadable(&list, &[]), None);
}

#[test]
fn saved_shows_only_while_nothing_changed_since() {
    assert_eq!(shown_message(SAVED, true), SAVED);
    assert_eq!(shown_message(SAVED, false), "");
    assert_eq!(
        shown_message("Chyba pri ukladaní", false),
        "Chyba pri ukladaní"
    );
    assert_eq!(SAVED, "Uložené");
}

#[test]
fn remove_takes_one_row_of_a_repeated_id() {
    let mut list = vec![vban("out-1", "a"), vban("out-1", "b"), vban("out-2", "c")];
    remove_one(&mut list, "out-1");
    assert_eq!(
        list.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
        ["b", "c"]
    );
    remove_one(&mut list, "out-9");
    assert_eq!(list.len(), 2, "an unknown id removes nothing");
}

#[test]
fn a_waiting_vban_output_reads_its_reason_in_slovak() {
    for (code, text) in [
        ("not_built", "výstup sa nedá zostaviť"),
        ("not_started", "vlákno výstupu nebeží — spustí sa znova"),
        ("converter", "prevod frekvencie zlyhal — posiela ticho"),
        ("unresolved", "cieľ sa nedá preložiť na adresu"),
        ("resolving", "cieľ sa ešte prekladá na adresu"),
        ("something_new", "neznámy dôvod"),
    ] {
        assert_eq!(vban_reason_sk(code), text, "{code}");
    }
    assert_eq!(
        vban_waiting_text("čaká", Some("unresolved")),
        "čaká · cieľ sa nedá preložiť na adresu"
    );
    assert_eq!(vban_waiting_text("čaká", None), "čaká");
}

/// One latency label on every row (review round 2): whole ms, half away
/// from zero; 0 (and a negative reading) is "meria sa".
#[test]
fn every_rows_latency_reads_oneskorenie_in_whole_ms() {
    assert_eq!(latency_sk(66.6666), "oneskorenie 67 ms");
    assert_eq!(latency_sk(70.5), "oneskorenie 71 ms");
    assert_eq!(latency_sk(83.3333), "oneskorenie 83 ms");
    assert_eq!(latency_sk(0.0), "oneskorenie: meria sa");
    assert_eq!(latency_sk(-1.0), "oneskorenie: meria sa");
}
