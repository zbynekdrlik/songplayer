//! #233: the outputs' settings on the server — the strict PATCH parse (every
//! error names the entry, the id and the field, never the input), the
//! lenient stored read (a bad entry is skipped and named, the rest run).

use super::*;
use sp_core::audio_outputs::{OutputEntry, RateChoice, VbanDest, VbanSampleFormat};

const FOH: &str = r#"{"id":"out-1","name":"FOH","type":"vban","enabled":true,"rate":48000,"delay_ms":0,"vban":{"host":"fohabl.lan","port":6980,"stream_name":"sp-program","format":"int24"}}"#;

fn foh() -> OutputEntry {
    let mut e = OutputEntry::vban(
        "out-1",
        "FOH",
        VbanDest {
            host: "fohabl.lan".into(),
            port: 6980,
            stream_name: "sp-program".into(),
            format: VbanSampleFormat::Int24,
        },
    );
    e.rate = RateChoice::Fixed(48_000);
    e
}

#[test]
fn a_valid_list_parses_and_normalizes() {
    assert_eq!(parse_list(&format!("[{FOH}]")).unwrap(), vec![foh()]);
    assert_eq!(parse_list("[]").unwrap(), Vec::<OutputEntry>::new());
    let short = r#"[{"id":"out-2","name":"x","type":"vban","vban":{"host":"h","port":1}}]"#;
    let e = &parse_list(short).unwrap()[0];
    assert!(e.enabled);
    assert_eq!((e.rate, e.delay_ms), (RateChoice::Network, 0));
    let v = e.vban.as_ref().unwrap();
    assert_eq!(
        (v.stream_name.as_str(), v.format),
        ("sp-program", VbanSampleFormat::Int24)
    );
    let full = r#"[{"id":"out-3","name":"y","type":"vban","enabled":false,"rate":96000,"delay_ms":40,"vban":{"host":"h","port":2,"stream_name":"s","format":"float32"}}]"#;
    let e = &parse_list(full).unwrap()[0];
    assert!(!e.enabled);
    assert_eq!((e.rate, e.delay_ms), (RateChoice::Fixed(96_000), 40));
    let v = e.vban.as_ref().unwrap();
    assert_eq!(
        (v.stream_name.as_str(), v.format),
        ("s", VbanSampleFormat::Float32)
    );
}

#[test]
fn each_type_error_names_the_entry_and_the_field_never_the_value() {
    let cases = [
        (
            r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"h","port":"secret-ish-text"}}]"#,
            "entry 1 (id out-1): vban.port has the wrong type",
        ),
        (
            r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"h","port":70000}}]"#,
            "entry 1 (id out-1): vban.port has the wrong type",
        ),
        (
            r#"[{"id":"out-1","name":"a","type":"vban"}]"#,
            "entry 1 (id out-1): vban is missing",
        ),
        (
            r#"[{"id":"out-1","name":"a","type":"vban","vban":"secret-ish-text"}]"#,
            "entry 1 (id out-1): vban is not a JSON object",
        ),
        (
            r#"[{"id":"out-1","name":"a","type":"vban","vban":{"port":1}}]"#,
            "entry 1 (id out-1): vban.host is missing",
        ),
        (
            r#"[{"id":"out-1","name":"a","type":"midi","vban":{"host":"h","port":1}}]"#,
            "entry 1 (id out-1): type must be vban",
        ),
        (
            r#"[{"id":"out-1","name":"a","vban":{"host":"h","port":1}}]"#,
            "entry 1 (id out-1): type is missing",
        ),
        (
            r#"[{"id":"out-1","type":"vban","vban":{"host":"h","port":1}}]"#,
            "entry 1 (id out-1): name is missing",
        ),
        (r#"[{"name":"a","type":"vban"}]"#, "entry 1: id is missing"),
        (r#"[{"id":7}]"#, "entry 1: id has the wrong type"),
        (
            r#"[{"id":"out-1","name":"a","type":"vban","rate":"fast","vban":{"host":"h","port":1}}]"#,
            "entry 1 (id out-1): rate has the wrong type",
        ),
        (
            r#"[{"id":"out-1","name":"a","type":"vban","delay_ms":-5,"vban":{"host":"h","port":1}}]"#,
            "entry 1 (id out-1): delay_ms has the wrong type",
        ),
        (
            r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"h","port":1,"format":"int8"}}]"#,
            "entry 1 (id out-1): vban.format must be int16, int24 or float32",
        ),
        (
            r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"h","port":1,"stream_name":7}}]"#,
            "entry 1 (id out-1): vban.stream_name has the wrong type",
        ),
        (
            r#"[{"id":"out-1","name":"a","type":"vban","enabled":"yes","vban":{"host":"h","port":1}}]"#,
            "entry 1 (id out-1): enabled has the wrong type",
        ),
        (r#"[1]"#, "entry 1 is not a JSON object"),
        (
            r#"[{"id":"Secret-Ish\u0000","name":"a","type":"x"}]"#,
            "entry 1 (id ?ecret-?sh?): type must be vban",
        ),
    ];
    for (input, want) in cases {
        let err = parse_list(input).unwrap_err();
        assert!(!err.contains("secret-ish"), "no echo of the input: {err}");
        assert_eq!(err, want, "{input}");
    }
}

#[test]
fn not_a_list_gives_line_and_column_only() {
    let err = parse_list("{\"a\":1}").unwrap_err();
    assert_eq!(err, "audio_outputs is not a JSON list (line 1, column 0)");
    let err = parse_list("[\n{\"id\":\"out-1\"},\n secret-ish]").unwrap_err();
    assert!(
        err.starts_with("audio_outputs is not a JSON list (line 3, column "),
        "{err}"
    );
    assert!(!err.contains("secret-ish"));
}

#[test]
fn a_value_error_comes_from_the_shared_validation() {
    let input = r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"h","port":0}}]"#;
    assert_eq!(
        parse_list(input).unwrap_err(),
        "entry 1 (id out-1): vban.port must be 1-65535"
    );
    let dup = format!("[{FOH},{FOH}]");
    assert_eq!(
        parse_list(&dup).unwrap_err(),
        "entry 2 (id out-1): id is used by an earlier entry"
    );
}

#[test]
fn a_stored_entry_this_version_cannot_read_is_skipped_and_the_rest_run() {
    let raw = format!(
        r#"[{FOH},{{"id":"out-2","name":"DVS","type":"asio","asio":{{"driver":"Dante Virtual Soundcard (x64)","channels":[0,1]}}}},{FOH},{{"id":"out-4","name":"x","type":"vban","vban":{{"host":"","port":1}}}}]"#
    );
    let stored = parse_stored(Some(&raw));
    assert_eq!(stored.entries, vec![foh()]);
    assert_eq!(
        stored.problems,
        vec![
            "entry 2 (id out-2): type must be vban".to_string(),
            "entry 3 (id out-1): id is used by an earlier entry".to_string(),
            "entry 4 (id out-4): vban.host is empty".to_string(),
        ]
    );
}

#[test]
fn a_stored_value_that_is_not_a_list_is_flagged_and_named() {
    let stored = parse_stored(Some("not json"));
    assert!(stored.entries.is_empty());
    assert!(stored.not_a_list, "the task changes nothing on it");
    assert_eq!(stored.problems.len(), 1);
    assert!(stored.problems[0].starts_with("audio_outputs is not a JSON list"));
    assert!(parse_stored(Some(r#"{"a":1}"#)).not_a_list);
    assert!(!parse_stored(Some(&format!("[{FOH}]"))).not_a_list);
    assert_eq!(parse_stored(None), Stored::default());
    assert_eq!(parse_stored(Some("  ")), Stored::default());
    assert_eq!(parse_stored(Some(" [] ")), Stored::default());
}

#[test]
fn the_stored_read_keeps_the_count_limits() {
    let entries: Vec<String> = (1..=10)
        .map(|n| {
            format!(r#"{{"id":"out-{n}","name":"v","type":"vban","vban":{{"host":"h","port":1}}}}"#)
        })
        .collect();
    let stored = parse_stored(Some(&format!("[{}]", entries.join(","))));
    assert_eq!(stored.entries.len(), 8, "the first 8 VBAN entries run");
    assert_eq!(stored.entries[7].id, "out-8");
    assert_eq!(
        stored.problems,
        vec![
            "entry 9 (id out-9): over the 8 vban entries".to_string(),
            "entry 10 (id out-10): over the 8 vban entries".to_string(),
        ]
    );
}

#[test]
fn the_patch_check_normalizes_the_list_and_the_rate() {
    assert_eq!(
        checked("audio_outputs", &format!("[ {FOH} ]")).unwrap(),
        format!("[{FOH}]")
    );
    assert_eq!(checked("audio_outputs", "  ").unwrap(), "[]");
    assert_eq!(
        checked("audio_outputs", r#"[{"id":"out-1"}]"#).unwrap_err(),
        "entry 1 (id out-1): type is missing"
    );
    assert_eq!(checked("audio_network_rate", " 96000 ").unwrap(), "96000");
    assert_eq!(checked("audio_network_rate", "44100").unwrap(), "44100");
    for bad in ["32000", "96k", "", "-1"] {
        assert_eq!(
            checked("audio_network_rate", bad).unwrap_err(),
            "audio_network_rate must be one of 44100, 48000, 88200, 96000, 192000"
        );
    }
    assert_eq!(
        checked("gemini_model", "x").unwrap(),
        "x",
        "other keys pass"
    );
    assert_eq!(rates_text(), "44100, 48000, 88200, 96000, 192000");
}

#[tokio::test]
async fn load_reads_the_list_leniently_and_the_network_rate() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    assert_eq!(
        load(&pool).await.unwrap(),
        OutputsSettings {
            entries: vec![],
            network_rate: 48_000,
            problems: vec![],
            not_a_list: false,
        }
    );
    let raw = format!(r#"[{FOH},{{"id":"out-2","name":"x","type":"asio"}}]"#);
    crate::db::models::set_setting(&pool, "audio_outputs", &raw)
        .await
        .unwrap();
    crate::db::models::set_setting(&pool, "audio_network_rate", "96000")
        .await
        .unwrap();
    assert_eq!(
        load(&pool).await.unwrap(),
        OutputsSettings {
            entries: vec![foh()],
            network_rate: 96_000,
            problems: vec!["entry 2 (id out-2): type must be vban".to_string()],
            not_a_list: false,
        }
    );
}
