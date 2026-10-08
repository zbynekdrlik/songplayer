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
    assert_eq!(OutputType::NAMES, "vban or asio");
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
    assert_eq!(
        err.sk(),
        "Výstup 2 (out-1): pole „port“ musí byť 1 až 65535"
    );
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
            "pole „identifikátor“ je prázdne",
        ),
        (
            "name",
            Problem::TooLong,
            "name is too long",
            "pole „názov“ je príliš dlhé",
        ),
        (
            "name",
            Problem::BadCharacters,
            "name has a character that is not allowed",
            "pole „názov“ obsahuje nepovolený znak",
        ),
        (
            "id",
            Problem::Duplicate,
            "id is used by an earlier entry",
            "pole „identifikátor“ má rovnakú hodnotu ako iný výstup",
        ),
        (
            "rate",
            Problem::UnsupportedRate,
            "rate must be \"network\" or 44100, 48000, 88200, 96000 or 192000",
            "pole „frekvencia“ musí byť podľa siete alebo 44100, 48000, 88200, 96000 či 192000 Hz",
        ),
        (
            "delay_ms",
            Problem::TooLarge,
            "delay_ms is over 2000 ms",
            "pole „oneskorenie“ má viac ako 2000 ms",
        ),
        (
            "vban",
            Problem::Missing,
            "vban is missing",
            "pole „nastavenie VBAN“ chýba",
        ),
        (
            "vban.host",
            Problem::Empty,
            "vban.host is empty",
            "pole „cieľ“ je prázdne",
        ),
        (
            "vban.port",
            Problem::BadPort,
            "vban.port must be 1-65535",
            "pole „port“ musí byť 1 až 65535",
        ),
        (
            "vban.stream_name",
            Problem::TooLong,
            "vban.stream_name is too long",
            "pole „názov streamu“ je príliš dlhé",
        ),
        (
            "other",
            Problem::Missing,
            "other is missing",
            "pole „other“ chýba",
        ),
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

// #233 lane 3: ASIO entries.

fn dvs(id: &str) -> OutputEntry {
    OutputEntry::asio(
        id,
        "DVS",
        AsioDest {
            driver: "Dante Virtual Soundcard (x64)".into(),
            channels: [0, 1],
        },
    )
}

fn asio_mut(e: &mut OutputEntry) -> &mut AsioDest {
    e.asio.as_mut().unwrap()
}

#[test]
fn an_asio_entry_serializes_in_the_spec_layout() {
    let text = serde_json::to_string(&dvs("out-3")).unwrap();
    assert_eq!(
        text,
        r#"{"id":"out-3","name":"DVS","type":"asio","enabled":true,"rate":"network","delay_ms":0,"asio":{"driver":"Dante Virtual Soundcard (x64)","channels":[0,1]}}"#
    );
    let back: OutputEntry = serde_json::from_str(&text).unwrap();
    assert_eq!(back, dvs("out-3"));
    assert!(validate_entry(0, &dvs("out-3")).is_ok());
    assert_eq!(OutputType::Asio.as_str(), "asio");
    assert_eq!(OutputType::parse("asio"), Some(OutputType::Asio));
    assert_eq!(OutputType::parse("ASIO"), None);
    // A VBAN entry has no `asio` block, an ASIO entry no `vban` block.
    assert!(!serde_json::to_string(&foh()).unwrap().contains("asio"));
    assert_eq!(dvs("out-3").vban, None);
}

#[test]
fn asio_limits_at_their_edges() {
    let check = |f: &dyn Fn(&mut OutputEntry)| {
        let mut e = dvs("out-3");
        f(&mut e);
        validate_entry(0, &e).map_err(|err| (err.field, err.problem))
    };
    assert!(check(&|e| asio_mut(e).driver = "d".repeat(128)).is_ok());
    assert!(
        check(&|e| asio_mut(e).driver = "ď".repeat(128)).is_ok(),
        "counted in characters"
    );
    assert_eq!(
        check(&|e| asio_mut(e).driver = "d".repeat(129)),
        Err(("asio.driver", Problem::TooLong))
    );
    assert_eq!(
        check(&|e| asio_mut(e).driver = String::new()),
        Err(("asio.driver", Problem::Empty))
    );
    assert_eq!(
        check(&|e| asio_mut(e).driver = "  ".into()),
        Err(("asio.driver", Problem::Empty))
    );
    assert_eq!(
        check(&|e| asio_mut(e).driver = "DVS\u{7}".into()),
        Err(("asio.driver", Problem::BadCharacters))
    );
    assert!(check(&|e| asio_mut(e).channels = [511, 0]).is_ok());
    assert!(check(&|e| asio_mut(e).channels = [0, 511]).is_ok());
    assert_eq!(
        check(&|e| asio_mut(e).channels = [0, 512]),
        Err(("asio.channels", Problem::OutOfRange))
    );
    assert_eq!(
        check(&|e| asio_mut(e).channels = [512, 1]),
        Err(("asio.channels", Problem::OutOfRange))
    );
    assert_eq!(
        check(&|e| asio_mut(e).channels = [3, 3]),
        Err(("asio.channels", Problem::SameChannel))
    );
    assert_eq!(check(&|e| e.asio = None), Err(("asio", Problem::Missing)));
    // The shared rules hold for an ASIO entry too.
    assert_eq!(
        check(&|e| e.delay_ms = 2_001),
        Err(("delay_ms", Problem::TooLarge))
    );
}

#[test]
fn two_asio_entries_on_one_driver_are_refused() {
    let err = validate_list(&[dvs("out-1"), dvs("out-2")]).unwrap_err();
    assert_eq!(
        err.to_string(),
        "entry 2 (id out-2): asio.driver is already used by an earlier ASIO entry (a driver takes one client)"
    );
    assert_eq!(
        err.sk(),
        "Výstup 2 (out-2): pole „ovládač“ je už použité iným výstupom ASIO (ovládač berie jedného klienta)"
    );
    let mut off = dvs("out-2");
    off.enabled = false;
    assert_eq!(
        validate_list(&[dvs("out-1"), off]),
        Err(err),
        "a switched-off entry still names the driver"
    );
    // Another driver is fine, a VBAN entry between them too (foh is out-1).
    let mut other = dvs("out-3");
    asio_mut(&mut other).driver = "Blackmagic ASIO".into();
    assert!(validate_list(&[dvs("out-2"), foh(), other]).is_ok());
}

#[test]
fn at_most_four_asio_entries() {
    let drivers = |n: u32| -> Vec<OutputEntry> {
        (1..=n)
            .map(|k| {
                OutputEntry::asio(
                    &format!("out-{k}"),
                    "a",
                    AsioDest {
                        driver: format!("d{k}"),
                        channels: [0, 1],
                    },
                )
            })
            .collect()
    };
    assert!(validate_list(&drivers(4)).is_ok());
    let five = validate_list(&drivers(5)).unwrap_err();
    assert_eq!(
        five,
        ListError::TooManyOfType {
            kind: OutputType::Asio,
            count: 5,
            max: MAX_ASIO_OUTPUTS
        }
    );
    assert_eq!(MAX_ASIO_OUTPUTS, 4);
    assert_eq!(
        five.to_string(),
        "audio_outputs has 5 asio entries (at most 4)"
    );
    assert_eq!(five.sk(), "Výstupov ASIO je 5, najviac môže byť 4");
    // 8 VBAN and 4 ASIO entries are a whole list.
    let mut full: Vec<OutputEntry> = (1..=8)
        .map(|n| OutputEntry::vban(&format!("v-{n}"), "v", dest("h", 6980)))
        .collect();
    full.extend(drivers(4));
    assert!(validate_list(&full).is_ok());
}

#[test]
fn the_asio_problems_and_fields_have_their_english_and_slovak_text() {
    let cases = [
        (
            "asio",
            Problem::Missing,
            "asio is missing",
            "pole „nastavenie ASIO“ chýba",
        ),
        (
            "asio.driver",
            Problem::Empty,
            "asio.driver is empty",
            "pole „ovládač“ je prázdne",
        ),
        (
            "asio.driver",
            Problem::DriverTaken,
            "asio.driver is already used by an earlier ASIO entry (a driver takes one client)",
            "pole „ovládač“ je už použité iným výstupom ASIO (ovládač berie jedného klienta)",
        ),
        (
            "asio.channels",
            Problem::OutOfRange,
            "asio.channels must be 0-511",
            "pole „kanály“ musí byť 1 až 512",
        ),
        (
            "asio.channels",
            Problem::SameChannel,
            "asio.channels must name two different channels",
            "pole „kanály“ musí obsahovať dva rôzne kanály",
        ),
    ];
    for (field, problem, en, sk) in cases {
        let e = EntryError {
            index: 0,
            id: "out-3".into(),
            field,
            problem,
        };
        assert_eq!(e.to_string(), format!("entry 1 (id out-3): {en}"));
        assert_eq!(ListError::Entry(e).sk(), format!("Výstup 1 (out-3): {sk}"));
    }
}

#[test]
fn a_new_asio_entry_takes_the_next_id_and_channels_1_and_2() {
    let e = new_asio(&[dvs("out-4")], "Blackmagic ASIO");
    assert_eq!(
        (e.id.as_str(), e.name.as_str(), e.kind),
        ("out-5", "ASIO 5", OutputType::Asio)
    );
    assert!(e.enabled);
    assert_eq!((e.rate, e.delay_ms), (RateChoice::Network, 0));
    assert_eq!(e.vban, None);
    assert_eq!(
        e.asio.unwrap(),
        AsioDest {
            driver: "Blackmagic ASIO".into(),
            channels: [0, 1]
        }
    );
    let first = new_asio(&[], "");
    assert_eq!(
        (first.id.as_str(), first.name.as_str()),
        ("out-1", "ASIO 1")
    );
}

#[test]
fn every_asio_reason_code_reads_in_slovak() {
    let table = [
        ("not_found", "ovládač nie je v systéme"),
        ("busy", "ovládač používa iný program"),
        ("refused", "ovládač sa nedá použiť"),
        ("failed", "chyba ovládača"),
        ("reset", "ovládač sa reštartuje"),
        ("rate_changed", "ovládač zmenil frekvenciu"),
        ("stalled", "ovládač neodpovedá"),
        ("windows_only", "ASIO funguje len vo Windows"),
        ("later", "neznámy dôvod"),
        ("", "neznámy dôvod"),
    ];
    for (code, sk) in table {
        assert_eq!(asio_reason_sk(code), sk, "{code}");
    }
}

/// #233 review round 1: "(nenájdený)" only against a KNOWN list.
#[test]
fn the_driver_options_mark_a_stored_driver_only_a_known_list_lacks() {
    let dvs = "Dante Virtual Soundcard (x64)";
    let blackmagic = "Blackmagic ASIO";
    let listed = vec![dvs.to_string(), blackmagic.to_string()];
    let pair = |value: &str, label: &str| (value.to_string(), label.to_string());
    let both = vec![pair(dvs, dvs), pair(blackmagic, blackmagic)];
    assert_eq!(asio_driver_options(Some(&listed), blackmagic), both);
    assert_eq!(asio_driver_options(Some(&listed), ""), both);
    let mut with_old = both.clone();
    with_old.push(pair("Old Card ASIO", "Old Card ASIO (nenájdený)"));
    assert_eq!(
        asio_driver_options(Some(&listed), "Old Card ASIO"),
        with_old
    );
    assert_eq!(
        asio_driver_options(Some(&[]), "Old Card ASIO"),
        vec![pair("Old Card ASIO", "Old Card ASIO (nenájdený)")],
        "a known empty list lacks it too"
    );
    assert_eq!(
        asio_driver_options(None, "Old Card ASIO"),
        vec![pair("Old Card ASIO", "Old Card ASIO")],
        "an unknown list marks nothing"
    );
    assert_eq!(asio_driver_options(None, ""), Vec::new());
}

/// #233 review round 1: a shown 0 is refused, never channel 1.
#[test]
fn an_asio_channel_is_shown_1_based_and_a_shown_0_is_refused() {
    assert_eq!(asio_channel_index(1), 0);
    assert_eq!(asio_channel_index(512), 511);
    assert_eq!(asio_channel_index(0), u32::MAX);
    assert_eq!(asio_channel_shown(0), 1);
    assert_eq!(asio_channel_shown(511), 512);
    assert_eq!(asio_channel_shown(u32::MAX), 0, "a typed 0 reads back as 0");
    for shown in [0, 1, 2, 512, 513, u32::MAX] {
        assert_eq!(asio_channel_shown(asio_channel_index(shown)), shown);
    }
    let mut e = new_asio(&[], "Dante Virtual Soundcard (x64)");
    e.asio.as_mut().expect("an ASIO entry").channels[0] = asio_channel_index(0);
    let err = validate_list(&[e]).expect_err("a shown 0 is no channel");
    assert_eq!(
        err.sk(),
        "Výstup 1 (out-1): pole „kanály“ musí byť 1 až 512"
    );
}

/// #233 review round 2: the two codes added for a lost clock and a driver
/// another output still holds.
#[test]
fn a_lost_clock_and_a_held_driver_read_in_slovak() {
    assert_eq!(
        asio_reason_sk("clock_lost"),
        "ovládač stratil hodinový signál"
    );
    assert_eq!(
        asio_reason_sk("held"),
        "predchádzajúci výstup ešte neuvoľnil ovládač"
    );
}

/// #233 review round 2: "Pridať výstup ASIO" is off while the list loads,
/// after a failed read, and on a box that lists no driver.
#[test]
fn the_add_asio_button_says_why_it_is_off() {
    let listed = vec!["Dante Virtual Soundcard (x64)".to_string()];
    assert_eq!(asio_add_refusal(Some(&listed), false), None);
    assert_eq!(
        asio_add_refusal(Some(&listed), true),
        None,
        "a list was read"
    );
    assert_eq!(
        asio_add_refusal(Some(&[]), false),
        Some("V systéme nie je žiadny ovládač ASIO")
    );
    assert_eq!(
        asio_add_refusal(None, true),
        Some("Zoznam ovládačov ASIO sa nenačítal")
    );
    assert_eq!(asio_add_refusal(None, false), Some(""), "still loading");
}

/// #233 review round 4: a running ASIO output's line; "meria sa" until
/// the server measured the latency.
#[test]
fn a_running_asio_output_reads_its_latency_or_that_it_is_measured() {
    assert_eq!(
        asio_running_text("beží", 70.625, 0.4, 0),
        "beží · 71 ms · +0.4 ppm · výpadky 0"
    );
    assert_eq!(
        asio_running_text("beží", 0.0, -1.26, 3),
        "beží · meria sa · -1.3 ppm · výpadky 3"
    );
}

/// #233 review round 4: a parked driver's Slovak says only a restart helps.
#[test]
fn a_parked_driver_reads_in_slovak() {
    assert_eq!(
        asio_reason_sk("parked"),
        "ovládač zamrzol — pomôže len reštart SongPlayera"
    );
}

/// #233 review round 4: a waiting ASIO output's line; no next try for a
/// parked driver.
#[test]
fn a_waiting_asio_output_reads_its_reason_and_next_try() {
    assert_eq!(
        asio_waiting_text("čaká", Some("busy"), Some(10.0)),
        "čaká · ovládač používa iný program · ďalší pokus o 10 s"
    );
    assert_eq!(
        asio_waiting_text("čaká", Some("parked"), Some(60.0)),
        "čaká · ovládač zamrzol — pomôže len reštart SongPlayera"
    );
    assert_eq!(
        asio_waiting_text("čaká", None, Some(2.0)),
        "čaká · ďalší pokus o 2 s"
    );
    assert_eq!(asio_waiting_text("otvára sa", None, None), "otvára sa");
}
