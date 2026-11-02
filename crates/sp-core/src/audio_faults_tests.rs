//! #257 lane 2: the audio fault ledger's wire shape, the HealthBar's
//! segment and the panel's texts.

use std::collections::BTreeMap;

use super::*;
use crate::health::HealthTone;

const NOW: i64 = 1_760_000_000_000;
const MIN: i64 = 60_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 86_400_000;
/// The ledger's 90 days.
const KEPT: i64 = 7_776_000_000;

/// A live path, clean for the whole day.
fn path(name: &str) -> PathFaults {
    PathFaults {
        path: name.to_string(),
        detail: String::new(),
        live: true,
        clean_since_ms: NOW - DAY,
        last_loss_ms: None,
        last_risk_ms: None,
        losses: 0,
        risks: 0,
        counts: BTreeMap::new(),
        events: Vec::new(),
        loss_buckets: Vec::new(),
        risk_buckets: Vec::new(),
        starts: Vec::new(),
        loop_check: None,
    }
}

/// A path with `losses` loss rows, the last at `at`.
fn lossy(name: &str, losses: i64, at: i64) -> PathFaults {
    PathFaults {
        losses,
        last_loss_ms: Some(at),
        clean_since_ms: at,
        ..path(name)
    }
}

/// A path with `risks` risk rows and no loss.
fn risky(name: &str, risks: i64) -> PathFaults {
    PathFaults {
        risks,
        last_risk_ms: Some(NOW - HOUR),
        ..path(name)
    }
}

/// The day's answer at [`NOW`].
fn day(paths: Vec<PathFaults>) -> FaultsAnswer {
    FaultsAnswer {
        now_ms: NOW,
        from_ms: NOW - DAY,
        hours: 24,
        kept_from_ms: NOW - KEPT,
        clean_since_ms: NOW - DAY,
        worst: None,
        truncated: false,
        paths,
    }
}

fn event(ts_ms: i64, kind: &str, severity: &str, amount: i64) -> FaultEvent {
    FaultEvent {
        ts_ms,
        kind: kind.to_string(),
        severity: severity.to_string(),
        amount,
    }
}

fn no_playlist(_: i64) -> Option<String> {
    None
}

fn worship(id: i64) -> Option<String> {
    (id == 3).then(|| "Worship".to_string())
}

fn local(day: u32, month: u32, hour: u32, minute: u32, second: u32) -> LocalTime {
    LocalTime {
        day,
        month,
        hour,
        minute,
        second,
    }
}

#[test]
fn the_wire_shape_reads_the_servers_answer_and_writes_its_loop_block_as_loop() {
    let text = r#"{"now_ms":5000,"from_ms":1000,"hours":1,"kept_from_ms":100,"clean_since_ms":900,
        "worst":"loop:out-2","truncated":true,
        "paths":[{"path":"loop:out-2","detail":"SG","live":true,"clean_since_ms":4000,
        "last_loss_ms":4000,"last_risk_ms":null,"losses":2,"risks":0,"counts":{"dropout":2},
        "events":[{"ts_ms":4000,"kind":"dropout","severity":"loss","amount":480}],
        "loop":{"checked_s":3540.0,"blind_s":60.0}},
        {"path":"program","detail":"SP-program","live":true,"clean_since_ms":1000,
        "last_loss_ms":null,"last_risk_ms":null}]}"#;
    let a: FaultsAnswer = serde_json::from_str(text).unwrap();
    assert_eq!((a.now_ms, a.from_ms, a.hours), (5000, 1000, 1));
    assert_eq!((a.kept_from_ms, a.clean_since_ms), (100, 900));
    assert_eq!(a.worst.as_deref(), Some("loop:out-2"));
    assert!(a.truncated);
    let looped = &a.paths[0];
    assert_eq!((looped.losses, looped.risks), (2, 0));
    assert_eq!(looped.counts, BTreeMap::from([("dropout".to_string(), 2)]));
    assert_eq!(looped.events, vec![event(4000, "dropout", "loss", 480)]);
    assert_eq!(
        looped.loop_check,
        Some(LoopSeconds {
            checked_s: 3540.0,
            blind_s: 60.0
        })
    );
    let program = &a.paths[1];
    assert_eq!(
        (program.losses, program.risks, program.loop_check.clone()),
        (0, 0, None),
        "an older server's path: no counts, no events, no loop block"
    );
    assert!(program.counts.is_empty() && program.events.is_empty());
    let json = serde_json::to_value(&a).unwrap();
    assert_eq!(json["paths"][0]["loop"]["checked_s"], 3540.0);
    assert!(json["paths"][1].get("loop").is_none());
    assert_eq!(json["paths"][0]["losses"], 2);
    assert_eq!(json["worst"], "loop:out-2");
    assert_eq!(json["kept_from_ms"], 100);
    assert_eq!(json["clean_since_ms"], 900);
    let older: FaultsAnswer =
        serde_json::from_str(r#"{"now_ms":5000,"from_ms":1000,"hours":1,"worst":null,"paths":[]}"#)
            .unwrap();
    assert_eq!(
        (older.kept_from_ms, older.clean_since_ms, older.truncated),
        (0, 0, false),
        "an older server's answer"
    );
}

#[test]
fn the_url_names_the_window() {
    assert_eq!(faults_url(24), "/api/v1/audio/faults?hours=24");
    assert_eq!(faults_url(720), "/api/v1/audio/faults?hours=720");
}

#[test]
fn each_level_has_its_marker_and_colour() {
    let table = [
        (AudioLevel::Unknown, "unknown", HealthTone::Off),
        (AudioLevel::Ok, "ok", HealthTone::Ok),
        (AudioLevel::Risk, "risk", HealthTone::Ok),
        (AudioLevel::Loss, "loss", HealthTone::Warn),
        (AudioLevel::Recent, "recent", HealthTone::Error),
    ];
    for (level, marker, tone) in table {
        assert_eq!((level.marker(), level.tone()), (marker, tone), "{level:?}");
    }
}

#[test]
fn no_answer_or_an_answer_with_no_path_is_unknown() {
    assert_eq!(level(None), AudioLevel::Unknown);
    assert_eq!(level(Some(&day(Vec::new()))), AudioLevel::Unknown);
}

#[test]
fn a_clean_chain_is_ok_and_risks_alone_stay_green() {
    assert_eq!(
        level(Some(&day(vec![path("program"), path("out:a")]))),
        AudioLevel::Ok
    );
    assert_eq!(
        level(Some(&day(vec![path("program"), risky("out:a", 1)]))),
        AudioLevel::Risk
    );
}

#[test]
fn a_loss_in_the_last_10_minutes_is_red_an_older_one_amber() {
    let at = |t: i64| level(Some(&day(vec![path("program"), lossy("out:a", 1, t)])));
    assert_eq!(at(NOW), AudioLevel::Recent);
    assert_eq!(at(NOW - RECENT_MS + 1), AudioLevel::Recent);
    assert_eq!(
        at(NOW - RECENT_MS),
        AudioLevel::Loss,
        "10 min ago is no longer recent"
    );
    assert_eq!(at(NOW - 3 * HOUR), AudioLevel::Loss);
    let with_risks = day(vec![risky("program", 4), lossy("out:a", 1, NOW - 3 * HOUR)]);
    assert_eq!(
        level(Some(&with_risks)),
        AudioLevel::Loss,
        "a loss beats risks"
    );
}

#[test]
fn the_totals_sum_every_path() {
    let a = day(vec![
        lossy("out:a", 2, NOW - HOUR),
        risky("out:b", 4),
        PathFaults {
            risks: 1,
            ..lossy("program", 1, NOW - DAY)
        },
    ]);
    assert_eq!((total_losses(&a), total_risks(&a)), (3, 5));
}

#[test]
fn the_worst_path_has_the_newest_loss_the_later_one_on_a_tie() {
    let a = day(vec![
        lossy("out:a", 1, NOW - 5 * MIN),
        risky("out:b", 9),
        lossy("out:c", 1, NOW - MIN),
        lossy("program", 1, NOW - 2 * MIN),
    ]);
    assert_eq!(worst_path(&a).map(|p| p.path.as_str()), Some("out:c"));
    let tie = day(vec![
        lossy("out:a", 1, NOW - MIN),
        lossy("out:b", 1, NOW - MIN),
    ]);
    assert_eq!(worst_path(&tie).map(|p| p.path.as_str()), Some("out:b"));
    assert_eq!(worst_path(&day(vec![risky("out:a", 1)])), None);
}

#[test]
fn a_span_reads_in_whole_minutes_hours_and_days() {
    let table = [
        (-5, "menej ako minútu"),
        (0, "menej ako minútu"),
        (MIN - 1, "menej ako minútu"),
        (MIN, "1 min"),
        (13 * MIN - 1, "12 min"),
        (59 * MIN + 59_000, "59 min"),
        (2 * HOUR, "2 h"),
        (2 * HOUR + 14 * MIN + 30_000, "2 h 14 min"),
        (23 * HOUR + 59 * MIN, "23 h 59 min"),
        (DAY + 30 * MIN, "1 d"),
        (47 * HOUR, "1 d 23 h"),
        (3 * DAY + 4 * HOUR + 59 * MIN, "3 d 4 h"),
        (30 * DAY, "30 d"),
    ];
    for (ms, text) in table {
        assert_eq!(duration_sk(ms), text, "{ms}");
    }
}

#[test]
fn ago_reads_pred() {
    assert_eq!(ago_sk(MIN - 1), "pred menej ako minútou");
    assert_eq!(ago_sk(MIN), "pred 1 min");
    assert_eq!(ago_sk(5 * MIN + 30_000), "pred 5 min");
    assert_eq!(ago_sk(3 * HOUR), "pred 3 h");
}

#[test]
fn a_window_reads_as_the_selector_names_it() {
    let table = [
        (1, "1 h"),
        (24, "24 h"),
        (47, "47 h"),
        (48, "2 d"),
        (49, "2 d 1 h"),
        (168, "7 d"),
        (720, "30 d"),
    ];
    for (hours, text) in table {
        assert_eq!(window_label(hours), text, "{hours}");
    }
}

#[test]
fn counts_take_the_slovak_plural() {
    let table = [
        (0, "0 chýb", "0 rizík"),
        (1, "1 chyba", "1 riziko"),
        (2, "2 chyby", "2 riziká"),
        (4, "4 chyby", "4 riziká"),
        (5, "5 chýb", "5 rizík"),
        (21, "21 chýb", "21 rizík"),
    ];
    for (n, losses, risks) in table {
        assert_eq!(
            (losses_sk(n), risks_sk(n)),
            (losses.to_string(), risks.to_string())
        );
    }
}

#[test]
fn a_path_reads_by_its_name() {
    let table = [
        ("program", "SP-program", None, "Program"),
        ("out:out-1", "FOH", None, "FOH"),
        ("out:fohabl", "fohabl.lan:6980", None, "fohabl.lan:6980"),
        ("out:out-1", " ", None, "out-1"),
        (
            "loop:out-2",
            "Waves SoundGrid",
            None,
            "Slučka – Waves SoundGrid",
        ),
        ("loop:out-2", "", None, "Slučka – out-2"),
        ("pipeline:3", "SP-worship", Some("Worship"), "Worship"),
        ("pipeline:3", "SP-worship", Some("  "), "Playlist 3"),
        ("pipeline:3", "SP-worship", None, "Playlist 3"),
        ("other:x", "Niečo", None, "Niečo"),
        ("other:x", "", None, "other:x"),
        ("input", "OBS manuál", None, "Vstup OBS manuál"),
        ("input", "", Some("Worship"), "Vstup OBS manuál"),
    ];
    for (p, detail, playlist, name) in table {
        assert_eq!(path_name(p, detail, playlist), name, "{p} {detail:?}");
    }
}

#[test]
fn the_ids_inside_a_path() {
    assert_eq!(pipeline_id("pipeline:12"), Some(12));
    assert_eq!(pipeline_id("pipeline:x"), None);
    assert_eq!(pipeline_id("out:12"), None);
    assert_eq!(output_id("out:out-1"), Some("out-1"));
    assert_eq!(output_id("loop:out-2"), Some("out-2"));
    assert_eq!(output_id("pipeline:1"), None);
    assert_eq!(output_id("program"), None);
}

#[test]
fn a_pipeline_takes_its_playlists_name() {
    let p = PathFaults {
        detail: "SP-worship".to_string(),
        ..path("pipeline:3")
    };
    assert_eq!(name_of(&p, &worship), "Worship");
    assert_eq!(name_of(&p, &no_playlist), "Playlist 3");
    let out = PathFaults {
        detail: "FOH".to_string(),
        ..path("out:out-1")
    };
    assert_eq!(name_of(&out, &worship), "FOH");
}

#[test]
fn frames_count_at_the_programs_rate_or_their_outputs() {
    let rates = |id: &str| (id == "out-2").then_some(96_000_u32);
    assert_eq!(rate_of("program", &rates), Some(48_000));
    assert_eq!(rate_of("pipeline:3", &rates), Some(48_000));
    assert_eq!(rate_of("out:out-2", &rates), Some(96_000));
    assert_eq!(rate_of("loop:out-2", &rates), Some(96_000));
    assert_eq!(rate_of("loop:out-1", &rates), None);
    assert_eq!(rate_of("other", &rates), None);
}

#[test]
fn an_outputs_rate_is_its_drivers_when_it_reads_one() {
    assert_eq!(output_rate(48_000, Some(96_000)), Some(96_000));
    assert_eq!(output_rate(48_000, Some(0)), Some(48_000));
    assert_eq!(output_rate(48_000, None), Some(48_000));
    assert_eq!(output_rate(0, Some(0)), None);
    assert_eq!(output_rate(0, None), None);
}

#[test]
fn every_kind_reads_in_slovak_an_unknown_one_as_sent() {
    let table = [
        ("dropout", "výpadok"),
        ("mismatch", "nesedí zvuk"),
        ("slip", "posun"),
        ("underrun", "podtečenie"),
        ("underrun_start", "podtečenie na začiatku"),
        ("underrun_end", "podtečenie na konci"),
        ("resync", "resync"),
        ("hard_recentre", "tvrdé re-centrovanie"),
        ("reset", "reset"),
        ("blocks_dropped", "zahodený blok"),
        ("blocks_substituted", "nahradený blok"),
        ("send_error", "chyba odoslania"),
        ("late_packet", "oneskorený paket"),
        ("filled", "vyplnené tichom"),
        ("late_callback", "oneskorené volanie"),
        ("overflow", "pretečenie"),
        ("host_down", "hostiteľ ASIO nebeží"),
        ("no_return", "slučka sa nevracia"),
        ("late_dropped", "neskoro zahodené"),
        ("coalesced", "zahodený blok programu"),
        ("interrupted", "prerušenie"),
        ("fast_callback", "predčasné volanie"),
        ("overload", "preťaženie"),
        ("no_source", "bez zdroja"),
        ("unsupported", "nepodporovaný formát"),
        ("vban_feed_late", "VBAN dodaný neskoro"),
        ("limited", "limiter"),
        ("ndi_late", "NDI neskoro"),
        ("unserviced", "neobslúžený slot"),
        ("pump_missed", "vynechaný blok Dante"),
        ("pump_late", "oneskorený blok Dante"),
        ("pump_overrun", "prekročený blok Dante"),
        ("start", "spustenie"),
        ("brand_new_kind", "brand_new_kind"),
    ];
    for (kind, text) in table {
        assert_eq!(kind_sk(kind), text);
    }
}

/// #257 review: `overflow` counts frames on an ASIO output and the times
/// its audio grid dropped its oldest samples on a pipeline.
#[test]
fn each_kind_measures_its_amount_in_its_unit_on_its_path() {
    let table = [
        ("loop:out-2", "mismatch", Unit::Frames),
        ("loop:out-2", "dropout", Unit::Frames),
        ("loop:out-2", "slip", Unit::Frames),
        ("out:out-2", "overflow", Unit::Frames),
        ("pipeline:3", "overflow", Unit::Count),
        ("pipeline:x", "overflow", Unit::Count),
        ("pipeline:3", "underrun", Unit::Count),
        ("pipeline:3", "underrun_start", Unit::Count),
        ("pipeline:3", "underrun_end", Unit::Count),
        ("program", "limited", Unit::Frames),
        ("out:out-1", "late_packet", Unit::Micros),
        ("out:out-2", "host_down", Unit::Nothing),
        ("loop:out-2", "no_return", Unit::Nothing),
        ("program", "start", Unit::Nothing),
        ("out:out-2", "underrun", Unit::Count),
        ("program", "filled", Unit::Count),
        ("program", "brand_new_kind", Unit::Count),
    ];
    for (path, kind, unit) in table {
        assert_eq!(unit_of(path, kind), unit, "{path} {kind}");
    }
}

#[test]
fn an_amount_reads_with_its_unit() {
    let table = [
        ("out:a", "underrun", 2, None, Some("×2")),
        ("out:a", "underrun", 1, None, None),
        ("out:a", "late_packet", 12_500, None, Some("12,5 ms")),
        ("loop:a", "dropout", 480, Some(48_000), Some("10,0 ms")),
        ("loop:a", "slip", -96, Some(96_000), Some("-1,0 ms")),
        ("program", "limited", 72, Some(48_000), Some("1,5 ms")),
        ("out:a", "overflow", 4096, None, Some("4096 vzoriek")),
        ("out:a", "overflow", 4096, Some(0), Some("4096 vzoriek")),
        ("pipeline:3", "overflow", 3, Some(48_000), Some("×3")),
        ("loop:a", "mismatch", 1, None, Some("1 vzorka")),
        ("loop:a", "slip", -3, None, Some("-3 vzorky")),
        ("out:a", "host_down", 1, Some(48_000), None),
    ];
    for (path, kind, amount, rate, text) in table {
        assert_eq!(
            amount_sk(path, kind, amount, rate).as_deref(),
            text,
            "{path} {kind} {amount} {rate:?}"
        );
    }
}

#[test]
fn the_segment_waits_for_its_first_answer() {
    let s = segment(None, &no_playlist);
    assert_eq!(
        s,
        Segment {
            level: AudioLevel::Unknown,
            text: "Zvuk: —".to_string(),
            risks: None,
            tip: "História chýb zvuku sa ešte nenačítala.".to_string(),
        }
    );
    assert_eq!(
        segment(Some(&day(Vec::new())), &no_playlist).text,
        "Zvuk: —"
    );
}

/// #257 (main session's ruling): a planned restart is no fault. The paths
/// restarted 2 h 14 min ago (a deploy), the chain's last loss was 3 d 4 h
/// ago: the chain is clean since the loss, across the deploy.
#[test]
fn a_clean_chain_reads_how_long_it_is_without_a_loss_across_a_restart() {
    let a = FaultsAnswer {
        clean_since_ms: NOW - (3 * DAY + 4 * HOUR + 30_000),
        ..day(vec![
            PathFaults {
                clean_since_ms: NOW - (2 * HOUR + 14 * MIN + 30_000),
                ..path("program")
            },
            PathFaults {
                clean_since_ms: NOW - HOUR,
                ..path("out:a")
            },
        ])
    };
    assert_eq!(
        segment(Some(&a), &no_playlist),
        Segment {
            level: AudioLevel::Ok,
            text: "Zvuk: bez chyby 3 d 4 h".to_string(),
            risks: None,
            tip: "Všetky zvukové cesty za posledných 24 h: 0 chýb, 0 rizík. Kliknutím zobrazíte \
                  podrobnosti."
                .to_string(),
        }
    );
}

#[test]
fn a_chain_clean_for_all_the_ledger_keeps_says_more_than_it() {
    let kept = FaultsAnswer {
        clean_since_ms: NOW - KEPT,
        ..day(vec![path("program")])
    };
    assert_eq!(
        segment(Some(&kept), &no_playlist).text,
        "Zvuk: bez chyby viac ako 90 d"
    );
    let just_after = FaultsAnswer {
        clean_since_ms: NOW - KEPT + 1,
        ..day(vec![path("program")])
    };
    assert_eq!(
        segment(Some(&just_after), &no_playlist).text,
        "Zvuk: bez chyby 89 d 23 h"
    );
    let one = FaultsAnswer {
        from_ms: NOW - HOUR,
        hours: 1,
        clean_since_ms: NOW - 5 * HOUR,
        ..day(vec![PathFaults {
            clean_since_ms: NOW - HOUR + 1,
            ..path("program")
        }])
    };
    assert_eq!(
        segment(Some(&one), &no_playlist).text,
        "Zvuk: bez chyby 5 h",
        "a loss before the asked window still bounds it"
    );
}

#[test]
fn a_clean_span_reads_its_length_or_more_than_the_ledger_keeps() {
    let a = day(Vec::new());
    assert_eq!(clean_span_sk(NOW - (3 * DAY + 4 * HOUR), &a), "3 d 4 h");
    assert_eq!(clean_span_sk(NOW - KEPT + 1, &a), "89 d 23 h");
    assert_eq!(clean_span_sk(NOW - KEPT, &a), "viac ako 90 d");
    assert_eq!(clean_span_sk(NOW - KEPT - DAY, &a), "viac ako 90 d");
    let older = FaultsAnswer {
        kept_from_ms: 0,
        ..day(Vec::new())
    };
    assert_eq!(
        clean_span_sk(NOW - 5 * HOUR, &older),
        "5 h",
        "an older server's answer keeps nothing it could name"
    );
}

#[test]
fn risks_alone_keep_the_green_text_and_count_beside_it() {
    let a = FaultsAnswer {
        clean_since_ms: NOW - 3 * HOUR,
        ..day(vec![
            PathFaults {
                clean_since_ms: NOW - 30 * MIN,
                ..path("program")
            },
            PathFaults {
                clean_since_ms: NOW - 30 * MIN,
                ..risky("out:a", 3)
            },
        ])
    };
    let s = segment(Some(&a), &no_playlist);
    assert_eq!(s.level, AudioLevel::Risk);
    assert_eq!(s.text, "Zvuk: bez chyby 3 h", "the chain's own clean time");
    assert_eq!(s.risks.as_deref(), Some(" · riziká: 3"));
    assert!(s.tip.contains(": 0 chýb, 3 riziká."), "{}", s.tip);
}

#[test]
fn an_older_loss_reads_amber_with_its_count_age_and_path() {
    let a = day(vec![lossy("program", 1, NOW - 3 * HOUR), risky("out:a", 2)]);
    let s = segment(Some(&a), &no_playlist);
    assert_eq!(s.level, AudioLevel::Loss);
    assert_eq!(
        s.text,
        "Zvuk: 1 chyba za 24 h · posledná pred 3 h · Program"
    );
    assert_eq!(
        s.risks, None,
        "the count of risks is in the tip, the text names the loss"
    );
    assert!(s.tip.contains(": 1 chyba, 2 riziká."), "{}", s.tip);
}

#[test]
fn a_recent_loss_reads_red_naming_the_worst_path() {
    let a = day(vec![
        PathFaults {
            detail: "Waves SoundGrid".to_string(),
            ..lossy("loop:out-2", 1, NOW - 40 * MIN)
        },
        PathFaults {
            detail: "SP-worship".to_string(),
            ..lossy("pipeline:3", 2, NOW - (5 * MIN + 30_000))
        },
        path("program"),
    ]);
    let s = segment(Some(&a), &worship);
    assert_eq!(s.level, AudioLevel::Recent);
    assert_eq!(
        s.text,
        "Zvuk: 3 chyby za 24 h · posledná pred 5 min · Worship"
    );
    assert_eq!(s.risks, None);
}

#[test]
fn local_times_read_day_month_and_padded_clock() {
    let t = local(5, 3, 7, 4, 9);
    assert_eq!(hm_sk(t), "07:04");
    assert_eq!(dm_sk(t), "5.3.");
    assert_eq!(dm_hm_sk(t), "5.3. 07:04");
    assert_eq!(time_sk(t), "5.3. 07:04:09");
    assert_eq!(time_sk(local(12, 10, 14, 5, 59)), "12.10. 14:05:59");
}

#[test]
fn a_paths_clean_line_names_when_and_how_long() {
    let a = day(Vec::new());
    let at = NOW - (2 * HOUR + 14 * MIN);
    assert_eq!(
        clean_sk(at, &a, local(12, 10, 14, 5, 0)),
        "bez chyby od 12.10. 14:05 (2 h 14 min)"
    );
    assert_eq!(
        clean_sk(NOW - DAY + 1, &a, local(11, 10, 16, 19, 0)),
        "bez chyby od 11.10. 16:19 (23 h 59 min)"
    );
    assert_eq!(
        clean_sk(NOW - DAY, &a, local(8, 10, 8, 53, 0)),
        "bez chyby od 8.10. 08:53 (1 d)",
        "the window's start is no bound any more"
    );
    assert_eq!(
        clean_sk(NOW - (3 * DAY + 4 * HOUR), &a, local(6, 10, 4, 53, 0)),
        "bez chyby od 6.10. 04:53 (3 d 4 h)"
    );
    assert_eq!(
        clean_sk(NOW - KEPT + 1, &a, local(11, 7, 8, 53, 0)),
        "bez chyby od 11.7. 08:53 (89 d 23 h)"
    );
    assert_eq!(
        clean_sk(NOW - KEPT, &a, local(11, 7, 8, 53, 0)),
        "bez chyby viac ako 90 d"
    );
    let hour = FaultsAnswer {
        from_ms: NOW - HOUR,
        hours: 1,
        ..day(Vec::new())
    };
    assert_eq!(
        clean_sk(NOW - 5 * HOUR, &hour, local(9, 10, 3, 53, 0)),
        "bez chyby od 9.10. 03:53 (5 h)",
        "a loss before the asked window"
    );
}

#[test]
fn a_loop_checks_line_reads_its_checked_and_quiet_time() {
    let l = |checked_s: f64, blind_s: f64| loop_sk(&LoopSeconds { checked_s, blind_s });
    assert_eq!(l(3540.0, 60.0), "skontrolované 59 min · ticho 1 min");
    assert_eq!(l(0.0, 0.0), "skontrolované 0 min · ticho 0 min");
    assert_eq!(
        l(30.0, 7200.5),
        "skontrolované menej ako minútu · ticho 2 h"
    );
}

#[test]
fn the_rows_go_program_outputs_with_their_loops_pipelines_then_the_rest() {
    let a = day([
        "pipeline:12",
        "loop:b",
        "weird",
        "out:b",
        "input",
        "pipeline:3",
        "program",
        "out:a",
    ]
    .into_iter()
    .map(path)
    .collect());
    let order: Vec<&str> = ordered_paths(&a).iter().map(|p| p.path.as_str()).collect();
    assert_eq!(
        order,
        [
            "program",
            "input",
            "out:a",
            "out:b",
            "loop:b",
            "pipeline:3",
            "pipeline:12",
            "weird"
        ]
    );
}

#[test]
fn the_event_list_holds_every_loss_and_risk_newest_first() {
    let a = day(vec![
        PathFaults {
            events: vec![
                event(NOW - 9 * MIN, "start", "info", 1),
                event(NOW - 5 * MIN, "underrun", "loss", 2),
                event(NOW - 2 * MIN, "late_callback", "risk", 1),
            ],
            ..path("out:b")
        },
        PathFaults {
            events: vec![
                event(NOW - 7 * MIN, "filled", "loss", 3),
                event(NOW - 5 * MIN, "resync", "loss", 1),
            ],
            ..path("program")
        },
    ]);
    let listed: Vec<(&str, i64, &str)> = listed_events(&a)
        .into_iter()
        .map(|(p, e)| (p.path.as_str(), e.ts_ms, e.kind.as_str()))
        .collect();
    assert_eq!(
        listed,
        [
            ("out:b", NOW - 2 * MIN, "late_callback"),
            ("program", NOW - 5 * MIN, "resync"),
            ("out:b", NOW - 5 * MIN, "underrun"),
            ("program", NOW - 7 * MIN, "filled"),
        ],
        "one instant's rows in the rows' order; a start is no fault"
    );
}

#[test]
fn the_first_poll_asks_the_day_and_then_every_10_s() {
    let none = Polls::default();
    assert_eq!(next_fetch(none, 0, None), Some(Fetch::Day));
    let asked = Polls {
        day_at: Some(1_000),
        window: None,
    };
    assert_eq!(next_fetch(asked, 10_999, None), None);
    assert_eq!(next_fetch(asked, 11_000, None), Some(Fetch::Day));
}

#[test]
fn the_panels_day_is_the_segments_answer() {
    let asked = Polls {
        day_at: Some(1_000),
        window: None,
    };
    assert_eq!(next_fetch(asked, 2_000, Some(24)), None);
}

#[test]
fn a_window_the_panel_switches_to_is_asked_at_once() {
    let day_due = Polls {
        day_at: Some(0),
        window: Some((1, 500)),
    };
    assert_eq!(
        next_fetch(day_due, 20_000, Some(168)),
        Some(Fetch::Window(168))
    );
    let fresh = Polls {
        day_at: Some(19_000),
        window: None,
    };
    assert_eq!(next_fetch(fresh, 20_000, Some(1)), Some(Fetch::Window(1)));
}

#[test]
fn an_asked_window_is_asked_again_every_10_s_after_the_day() {
    let asked = Polls {
        day_at: Some(5_000),
        window: Some((1, 3_000)),
    };
    assert_eq!(next_fetch(asked, 12_999, Some(1)), None);
    assert_eq!(next_fetch(asked, 13_000, Some(1)), Some(Fetch::Window(1)));
    assert_eq!(
        next_fetch(asked, 15_000, Some(1)),
        Some(Fetch::Day),
        "both due: the segment's first"
    );
    assert_eq!(
        next_fetch(asked, 13_000, None),
        None,
        "a closed panel asks nothing"
    );
}

/// #257 review: a closed panel forgets its window, so the reopened panel
/// asks it at once; an open one keeps what it asked.
#[test]
fn a_closed_panel_forgets_its_window() {
    let asked = Polls {
        day_at: Some(5_000),
        window: Some((168, 4_000)),
    };
    assert_eq!(polls_for(asked, true), asked);
    let closed = polls_for(asked, false);
    assert_eq!(
        closed,
        Polls {
            day_at: Some(5_000),
            window: None,
        }
    );
    assert_eq!(
        next_fetch(closed, 6_000, Some(168)),
        Some(Fetch::Window(168)),
        "reopened: asked at once"
    );
    assert_eq!(next_fetch(asked, 6_000, Some(168)), None, "kept: not due");
}

#[test]
fn failed_requests_count_in_a_row_until_an_answer() {
    assert_eq!(failed_after(0, false), 1);
    assert_eq!(failed_after(2, false), 3);
    assert_eq!(failed_after(u32::MAX, false), u32::MAX);
    assert_eq!(failed_after(5, true), 0);
}

/// #257 review: after three failed requests in a row the segment stops
/// showing its last answer as current.
#[test]
fn the_segment_reads_unavailable_after_three_failed_requests_in_a_row() {
    assert_eq!(unavailable(0), None);
    assert_eq!(unavailable(2), None);
    let segment = unavailable(3).unwrap();
    assert_eq!(segment.level, AudioLevel::Unknown);
    assert_eq!(segment.level.marker(), "unknown");
    assert_eq!(segment.level.tone(), HealthTone::Off);
    assert_eq!(segment.text, "Zvuk: nedostupné");
    assert_eq!(segment.risks, None);
    assert_eq!(
        segment.tip,
        "História chýb zvuku sa nedá načítať (3× po sebe)."
    );
    assert_eq!(
        unavailable(7).map(|s| s.tip).as_deref(),
        Some("História chýb zvuku sa nedá načítať (7× po sebe).")
    );
}

#[test]
fn the_panel_says_how_old_its_answer_is_while_the_ledger_does_not_answer() {
    assert_eq!(stale_sk(2, Some(90_000)), None);
    assert_eq!(
        stale_sk(3, Some(90_000)).as_deref(),
        Some("Server neodpovedá (3× po sebe) — údaje sú z poslednej odpovede pred 1 min.")
    );
    assert_eq!(
        stale_sk(4, Some(30_000)).as_deref(),
        Some(
            "Server neodpovedá (4× po sebe) — údaje sú z poslednej odpovede pred menej ako minútou."
        )
    );
    assert_eq!(
        stale_sk(9, Some(7_500_000)).as_deref(),
        Some("Server neodpovedá (9× po sebe) — údaje sú z poslednej odpovede pred 2 h 5 min.")
    );
    assert_eq!(
        stale_sk(3, None).as_deref(),
        Some("Server neodpovedá (3× po sebe) — história sa ešte nenačítala.")
    );
}

/// #257, CI 38305196432: a close and a reopen inside one of the poll's
/// naps is never seen as closed; the reopen alone makes the poll forget its
/// window, so the reopened panel asks it at once.
#[test]
fn a_panel_reopened_between_two_looks_forgets_its_window() {
    let asked = Polls {
        day_at: Some(5_000),
        window: Some((168, 4_000)),
    };
    let forgot = Polls {
        day_at: Some(5_000),
        window: None,
    };
    assert_eq!(polls_seen(asked, true, true), forgot);
    assert_eq!(
        next_fetch(polls_seen(asked, true, true), 6_000, Some(168)),
        Some(Fetch::Window(168))
    );
    // Not reopened: what `polls_for` keeps or forgets.
    assert_eq!(polls_seen(asked, true, false), asked);
    assert_eq!(polls_seen(asked, false, false), forgot);
    assert_eq!(
        next_fetch(polls_seen(asked, true, false), 6_000, Some(168)),
        None,
        "an open panel asked 2 s ago waits for its 10 s"
    );
}
