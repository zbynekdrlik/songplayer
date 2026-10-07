//! #233: the output list model — serde layout, defaults, every limit at its
//! exact edge, the error texts (English for the API, Slovak for the
//! dashboard), ids, the wire stream name.

use super::*;
use crate::config::{SETTING_AUDIO_NETWORK_RATE, SETTING_AUDIO_OUTPUTS, audio_network_rate};

fn dest(host: &str, port: u16) -> VbanDest {
    VbanDest {
        host: host.into(),
        port,
        stream_name: "sp-program".into(),
        format: VbanSampleFormat::Int24,
    }
}

fn foh() -> OutputEntry {
    let mut e = OutputEntry::vban("out-1", "fohabl.lan:6980", dest("fohabl.lan", 6980));
    e.rate = RateChoice::Fixed(48_000);
    e
}

#[test]
fn the_setting_keys() {
    assert_eq!(SETTING_AUDIO_OUTPUTS, "audio_outputs");
    assert_eq!(SETTING_AUDIO_NETWORK_RATE, "audio_network_rate");
}

#[test]
fn a_vban_entry_serializes_in_the_spec_layout() {
    let text = serde_json::to_string(&foh()).unwrap();
    assert_eq!(
        text,
        r#"{"id":"out-1","name":"fohabl.lan:6980","type":"vban","enabled":true,"rate":48000,"delay_ms":0,"vban":{"host":"fohabl.lan","port":6980,"stream_name":"sp-program","format":"int24"}}"#
    );
    let mut net = foh();
    net.rate = RateChoice::Network;
    assert!(
        serde_json::to_string(&net)
            .unwrap()
            .contains(r#""rate":"network""#)
    );
    let back: OutputEntry = serde_json::from_str(&text).unwrap();
    assert_eq!(back, foh());
}

#[test]
fn missing_optional_fields_take_their_defaults() {
    let e: OutputEntry = serde_json::from_str(
        r#"{"id":"out-2","name":"x","type":"vban","vban":{"host":"h","port":1}}"#,
    )
    .unwrap();
    assert!(e.enabled);
    assert_eq!(e.rate, RateChoice::Network);
    assert_eq!(e.delay_ms, 0);
    let v = e.vban.unwrap();
    assert_eq!(v.stream_name, "sp-program");
    assert_eq!(v.format, VbanSampleFormat::Int24);
}

#[test]
fn a_rate_is_network_or_a_whole_number() {
    let rate = |t: &str| serde_json::from_str::<RateChoice>(t);
    assert_eq!(rate(r#""network""#).unwrap(), RateChoice::Network);
    assert_eq!(rate("96000").unwrap(), RateChoice::Fixed(96_000));
    assert_eq!(rate("4294967295").unwrap(), RateChoice::Fixed(u32::MAX));
    assert!(rate(r#""96000""#).is_err(), "a quoted number is not a rate");
    assert!(rate("-1").is_err());
    assert!(rate("4294967296").is_err(), "over u32");
    assert!(rate("48000.5").is_err());
}

#[test]
fn a_rate_of_the_wrong_type_says_what_a_rate_is() {
    let rate = |t: &str| serde_json::from_str::<RateChoice>(t);
    for wrong in ["true", "-1", "48000.5", "null"] {
        let text = rate(wrong).unwrap_err().to_string();
        assert!(
            text.contains(r#"expected "network" or a rate in Hz"#),
            "{wrong}: {text}"
        );
    }
}

#[test]
fn the_names_of_the_types_and_formats() {
    assert_eq!(OutputType::Vban.as_str(), "vban");
    assert_eq!(OutputType::parse("vban"), Some(OutputType::Vban));
    assert_eq!(OutputType::parse("VBAN"), None);
    assert_eq!(OutputType::NAMES, "vban");
    for f in [
        VbanSampleFormat::Int16,
        VbanSampleFormat::Int24,
        VbanSampleFormat::Float32,
    ] {
        assert_eq!(VbanSampleFormat::parse(f.as_str()), Some(f));
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(json, format!("\"{}\"", f.as_str()));
    }
    assert_eq!(VbanSampleFormat::Int16.as_str(), "int16");
    assert_eq!(VbanSampleFormat::Float32.as_str(), "float32");
    assert_eq!(VbanSampleFormat::parse("int8"), None);
    assert_eq!(VbanSampleFormat::default(), VbanSampleFormat::Int24);
}

#[test]
fn every_limit_is_pinned_at_its_edge() {
    let check = |f: &dyn Fn(&mut OutputEntry)| {
        let mut e = foh();
        f(&mut e);
        validate_entry(0, &e)
    };
    assert!(check(&|_| {}).is_ok(), "the FOH entry itself");
    // id: 1..=32 of a-z 0-9 -
    assert!(check(&|e| e.id = "a".repeat(32)).is_ok());
    assert_eq!(
        check(&|e| e.id = "a".repeat(33)).unwrap_err().problem,
        Problem::TooLong
    );
    assert_eq!(
        check(&|e| e.id = String::new()).unwrap_err().problem,
        Problem::Empty
    );
    assert_eq!(
        check(&|e| e.id = "Out-1".into()).unwrap_err().problem,
        Problem::BadCharacters
    );
    assert!(check(&|e| e.id = "a-0".into()).is_ok());
    // name: 1..=64 characters, no control character, not blank
    assert!(
        check(&|e| e.name = "č".repeat(64)).is_ok(),
        "64 characters, not bytes"
    );
    assert_eq!(
        check(&|e| e.name = "č".repeat(65)).unwrap_err().problem,
        Problem::TooLong
    );
    assert_eq!(
        check(&|e| e.name = "  ".into()).unwrap_err().problem,
        Problem::Empty
    );
    assert_eq!(
        check(&|e| e.name = "a\tb".into()).unwrap_err().problem,
        Problem::BadCharacters
    );
    // rate: network or a supported rate
    for hz in SUPPORTED_RATES {
        assert!(check(&|e| e.rate = RateChoice::Fixed(hz)).is_ok(), "{hz}");
    }
    assert!(check(&|e| e.rate = RateChoice::Network).is_ok());
    let rate = check(&|e| e.rate = RateChoice::Fixed(32_000)).unwrap_err();
    assert_eq!(
        (rate.field, rate.problem),
        ("rate", Problem::UnsupportedRate)
    );
    // delay: 0..=2000 ms
    assert!(check(&|e| e.delay_ms = 2_000).is_ok());
    let delay = check(&|e| e.delay_ms = 2_001).unwrap_err();
    assert_eq!(
        (delay.field, delay.problem),
        ("delay_ms", Problem::TooLarge)
    );
    // vban.host: 1..=253 of A-Z a-z 0-9 . - _
    assert!(check(&|e| e.vban.as_mut().unwrap().host = "h".repeat(253)).is_ok());
    let long = check(&|e| e.vban.as_mut().unwrap().host = "h".repeat(254)).unwrap_err();
    assert_eq!((long.field, long.problem), ("vban.host", Problem::TooLong));
    let empty = check(&|e| e.vban.as_mut().unwrap().host = String::new()).unwrap_err();
    assert_eq!((empty.field, empty.problem), ("vban.host", Problem::Empty));
    assert_eq!(
        check(&|e| e.vban.as_mut().unwrap().host = "a b".into())
            .unwrap_err()
            .problem,
        Problem::BadCharacters
    );
    assert!(check(&|e| e.vban.as_mut().unwrap().host = "10.77.9.201".into()).is_ok());
    assert!(check(&|e| e.vban.as_mut().unwrap().host = "Fo_h-1.lan".into()).is_ok());
    // vban.port: 1..=65535
    let port = check(&|e| e.vban.as_mut().unwrap().port = 0).unwrap_err();
    assert_eq!((port.field, port.problem), ("vban.port", Problem::BadPort));
    assert!(check(&|e| e.vban.as_mut().unwrap().port = 1).is_ok());
    assert!(check(&|e| e.vban.as_mut().unwrap().port = 65_535).is_ok());
    // vban.stream_name: 1..=16 printable ASCII (0x20..=0x7e)
    assert!(check(&|e| e.vban.as_mut().unwrap().stream_name = "x".repeat(16)).is_ok());
    let stream = check(&|e| e.vban.as_mut().unwrap().stream_name = "x".repeat(17)).unwrap_err();
    assert_eq!(
        (stream.field, stream.problem),
        ("vban.stream_name", Problem::TooLong)
    );
    assert_eq!(
        check(&|e| e.vban.as_mut().unwrap().stream_name = String::new())
            .unwrap_err()
            .problem,
        Problem::Empty
    );
    assert!(check(&|e| e.vban.as_mut().unwrap().stream_name = " ~".into()).is_ok());
    assert_eq!(
        check(&|e| e.vban.as_mut().unwrap().stream_name = "\u{1f}".into())
            .unwrap_err()
            .problem,
        Problem::BadCharacters
    );
    assert_eq!(
        check(&|e| e.vban.as_mut().unwrap().stream_name = "\u{7f}".into())
            .unwrap_err()
            .problem,
        Problem::BadCharacters
    );
    // a vban entry needs its vban object
    let missing = check(&|e| e.vban = None).unwrap_err();
    assert_eq!((missing.field, missing.problem), ("vban", Problem::Missing));
    // the first problem wins: a bad id before a bad port
    let first = check(&|e| {
        e.id = String::new();
        e.vban.as_mut().unwrap().port = 0;
    })
    .unwrap_err();
    assert_eq!((first.index, first.field), (0, "id"));
}

#[test]
fn the_list_counts_types_and_duplicates() {
    let many: Vec<OutputEntry> = (1..=8)
        .map(|n| OutputEntry::vban(&format!("out-{n}"), "v", dest("h", 6980)))
        .collect();
    assert!(validate_list(&many).is_ok(), "8 VBAN entries");
    let mut nine = many.clone();
    nine.push(OutputEntry::vban("out-9", "v", dest("h", 6980)));
    assert_eq!(
        validate_list(&nine).unwrap_err(),
        ListError::TooManyOfType {
            kind: OutputType::Vban,
            count: 9,
            max: MAX_VBAN_OUTPUTS
        }
    );
    let sixteen: Vec<OutputEntry> = (1..=16)
        .map(|n| OutputEntry::vban(&format!("out-{n}"), "v", dest("h", 6980)))
        .collect();
    assert_eq!(
        validate_list(&sixteen).unwrap_err(),
        ListError::TooManyOfType {
            kind: OutputType::Vban,
            count: 16,
            max: MAX_VBAN_OUTPUTS
        },
        "16 entries pass the total cap and stop at the VBAN cap"
    );
    let seventeen: Vec<OutputEntry> = (1..=17)
        .map(|n| OutputEntry::vban(&format!("out-{n}"), "v", dest("h", 6980)))
        .collect();
    assert_eq!(
        validate_list(&seventeen).unwrap_err(),
        ListError::TooMany { count: 17 }
    );
    let dup = vec![foh(), foh()];
    match validate_list(&dup).unwrap_err() {
        ListError::Entry(e) => {
            assert_eq!((e.index, e.field, e.problem), (1, "id", Problem::Duplicate))
        }
        other => panic!("{other:?}"),
    }
    let mut bad_second = vec![foh(), OutputEntry::vban("out-2", "b", dest("h", 1))];
    bad_second[1].delay_ms = 2_001;
    match validate_list(&bad_second).unwrap_err() {
        ListError::Entry(e) => assert_eq!((e.index, e.field), (1, "delay_ms")),
        other => panic!("{other:?}"),
    }
    assert!(validate_list(&[]).is_ok());
}

#[test]
fn an_error_names_the_entry_the_id_and_the_field() {
    let mut e = foh();
    e.vban.as_mut().unwrap().port = 0;
    let err = validate_list(&[OutputEntry::vban("out-7", "a", dest("h", 1)), e]).unwrap_err();
    assert_eq!(
        err.to_string(),
        "entry 2 (id out-1): vban.port must be 1-65535"
    );
    assert_eq!(err.sk(), "Výstup 2 (out-1): port musí byť 1 až 65535");
    assert_eq!(
        ListError::TooMany { count: 17 }.to_string(),
        "audio_outputs has 17 entries (at most 16)"
    );
    assert_eq!(
        ListError::TooMany { count: 17 }.sk(),
        "Výstupov je 17, najviac môže byť 16"
    );
    let vban = ListError::TooManyOfType {
        kind: OutputType::Vban,
        count: 9,
        max: 8,
    };
    assert_eq!(
        vban.to_string(),
        "audio_outputs has 9 vban entries (at most 8)"
    );
    assert_eq!(vban.sk(), "Výstupov VBAN je 9, najviac môže byť 8");
}

#[test]
fn every_problem_and_field_has_its_english_and_slovak_text() {
    let cases = [
        (
            "id",
            Problem::Empty,
            "id is empty",
            "identifikátor je prázdne",
        ),
        (
            "name",
            Problem::TooLong,
            "name is too long",
            "názov je príliš dlhé",
        ),
        (
            "name",
            Problem::BadCharacters,
            "name has a character that is not allowed",
            "názov obsahuje nepovolený znak",
        ),
        (
            "id",
            Problem::Duplicate,
            "id is used by an earlier entry",
            "identifikátor už má iný výstup",
        ),
        (
            "rate",
            Problem::UnsupportedRate,
            "rate must be \"network\" or 44100, 48000, 88200, 96000 or 192000",
            "frekvencia musí byť podľa siete alebo 44100–192000 Hz",
        ),
        (
            "delay_ms",
            Problem::TooLarge,
            "delay_ms is over 2000 ms",
            "oneskorenie je viac ako 2000 ms",
        ),
        (
            "vban",
            Problem::Missing,
            "vban is missing",
            "nastavenie VBAN chýba",
        ),
        (
            "vban.host",
            Problem::Empty,
            "vban.host is empty",
            "cieľ je prázdne",
        ),
        (
            "vban.port",
            Problem::BadPort,
            "vban.port must be 1-65535",
            "port musí byť 1 až 65535",
        ),
        (
            "vban.stream_name",
            Problem::TooLong,
            "vban.stream_name is too long",
            "názov streamu je príliš dlhé",
        ),
        ("other", Problem::Missing, "other is missing", "pole chýba"),
    ];
    for (field, problem, en, sk) in cases {
        let e = EntryError {
            index: 2,
            id: "out-3".into(),
            field,
            problem,
        };
        assert_eq!(e.to_string(), format!("entry 3 (id out-3): {en}"));
        assert_eq!(ListError::Entry(e).sk(), format!("Výstup 3 (out-3): {sk}"));
    }
}

#[test]
fn a_shown_id_never_echoes_junk() {
    assert_eq!(shown_id("out-1"), "out-1");
    assert_eq!(shown_id("Out 1\u{0}"), "?ut?1?");
    assert_eq!(shown_id(&"a".repeat(40)).len(), MAX_ID_LEN);
    let junk = EntryError {
        index: 0,
        id: "Ä\"}".into(),
        field: "id",
        problem: Problem::BadCharacters,
    };
    assert_eq!(
        junk.to_string(),
        "entry 1 (id ???): id has a character that is not allowed"
    );
}

#[test]
fn ids_count_up_from_the_highest_and_a_new_vban_entry_is_ready_to_fill() {
    assert_eq!(next_id(&[]), "out-1");
    let list = vec![
        OutputEntry::vban("out-2", "a", dest("h", 1)),
        OutputEntry::vban("x", "b", dest("h", 1)),
        OutputEntry::vban("out-10", "c", dest("h", 1)),
    ];
    assert_eq!(next_id(&list), "out-11");
    let fresh = new_vban(&list);
    assert_eq!(fresh.id, "out-11");
    assert_eq!(fresh.name, "VBAN 11");
    assert_eq!(fresh.kind, OutputType::Vban);
    assert!(fresh.enabled);
    assert_eq!(fresh.rate, RateChoice::Network);
    assert_eq!(fresh.delay_ms, 0);
    let v = fresh.vban.unwrap();
    assert_eq!((v.host.as_str(), v.port), ("", DEFAULT_VBAN_PORT));
    assert_eq!(v.stream_name, "sp-program");
    assert_eq!(v.format, VbanSampleFormat::Int24);
    assert_eq!(new_vban(&[]).name, "VBAN 1");
    // a hand-edited id at the top of the range never overflows
    let top = vec![OutputEntry::vban(
        "out-18446744073709551615",
        "t",
        dest("h", 1),
    )];
    assert_eq!(next_id(&top), "out-18446744073709551615");
}

#[test]
fn the_effective_rate_and_the_network_rate_setting() {
    assert_eq!(effective_rate(RateChoice::Network, 96_000), 96_000);
    assert_eq!(effective_rate(RateChoice::Fixed(48_000), 96_000), 48_000);
    assert_eq!(audio_network_rate(None), 48_000);
    assert_eq!(audio_network_rate(Some("96000")), 96_000);
    assert_eq!(audio_network_rate(Some(" 44100 ")), 44_100);
    assert_eq!(audio_network_rate(Some("192000")), 192_000);
    assert_eq!(audio_network_rate(Some("32000")), 48_000);
    assert_eq!(audio_network_rate(Some("fast")), 48_000);
}

#[test]
fn the_wire_stream_name_is_what_vban_puts_on_the_wire() {
    assert_eq!(wire_stream_name("sp-program"), "sp-program");
    assert_eq!(wire_stream_name("abcdefghijklmnopq"), "abcdefghijklmnop");
    assert_eq!(wire_stream_name("čo\tje"), "_o_je");
}
