//! #259: the wall report's vocabulary — display ids, every state, problem
//! code and Slovak text, the marker, the health segment and the wire shape.
//! #254: the guard's reasons, phases and texts, and the segment while its
//! last end is pending.

use serde_json::json;

use super::*;
use crate::health::HealthTone;

const NO_ID: &str = "is no display id (0x and 1 to 8 hex digits)";

#[test]
fn a_display_id_reads_and_writes_as_hex() {
    assert_eq!(
        "0x80061086".parse::<DisplayId>(),
        Ok(DisplayId(0x8006_1086))
    );
    assert_eq!(
        "0X8006108a".parse::<DisplayId>(),
        Ok(DisplayId(0x8006_108A))
    );
    assert_eq!("0x1".parse::<DisplayId>(), Ok(DisplayId(1)));
    assert_eq!("0xFFFFFFFF".parse::<DisplayId>(), Ok(DisplayId(u32::MAX)));
    assert_eq!(DisplayId(0x8006_1086).to_string(), "0x80061086");
    assert_eq!(DisplayId(0xA).to_string(), "0x0000000A");
}

#[test]
fn a_display_id_refuses_anything_else_by_name() {
    for bad in [
        "0x",
        "0x123456789",
        "0x8006108G",
        "80061086",
        "0x+8006108",
        " 0x80061086",
    ] {
        assert_eq!(
            bad.parse::<DisplayId>(),
            Err(format!("{bad:?} {NO_ID}")),
            "{bad}"
        );
    }
}

#[test]
fn a_display_id_travels_as_its_hex_text() {
    assert_eq!(
        serde_json::to_value(DisplayId(0x8006_1087)).unwrap(),
        json!("0x80061087")
    );
    assert_eq!(
        serde_json::from_value::<DisplayId>(json!("0x80061083")).unwrap(),
        DisplayId(0x8006_1083)
    );
    let bad = serde_json::from_value::<DisplayId>(json!("0x8006108G")).unwrap_err();
    assert!(bad.to_string().contains(NO_ID), "{bad}");
    assert!(serde_json::from_value::<DisplayId>(json!(2_147_880_070_u64)).is_err());
}

const EVERY_STATE: [WallState; 9] = [
    WallState::Unknown,
    WallState::ArenaOff,
    WallState::SimpleOutput,
    WallState::NoPreset,
    WallState::NoDisplayOutput,
    WallState::ScreensOff,
    WallState::DisplayMissing,
    WallState::ArenaNotDrawing,
    WallState::Ok,
];

/// Each state: its wire name, its words, its tone, whether a segment shows
/// and whether it holds.
type StateRow = (
    WallState,
    &'static str,
    &'static str,
    HealthTone,
    bool,
    bool,
);

#[test]
fn every_state_has_its_name_words_tone_and_rules() {
    let table: [StateRow; 9] = [
        (
            WallState::Unknown,
            "unknown",
            "nedá sa overiť",
            HealthTone::Off,
            true,
            false,
        ),
        (
            WallState::ArenaOff,
            "arena_off",
            "Resolume nebeží",
            HealthTone::Off,
            true,
            true,
        ),
        (
            WallState::SimpleOutput,
            "simple_output",
            "Resolume má jednoduchý výstup",
            HealthTone::Off,
            false,
            true,
        ),
        (
            WallState::NoPreset,
            "no_preset",
            "Resolume nemá uložený výstup",
            HealthTone::Off,
            true,
            true,
        ),
        (
            WallState::NoDisplayOutput,
            "no_display_output",
            "Resolume nekreslí na žiadnu obrazovku",
            HealthTone::Off,
            false,
            true,
        ),
        (
            WallState::ScreensOff,
            "screens_off",
            "obrazovky vypnuté",
            HealthTone::Off,
            true,
            true,
        ),
        (
            WallState::DisplayMissing,
            "display_missing",
            "Resolume nemá svoju obrazovku",
            HealthTone::Error,
            true,
            false,
        ),
        (
            WallState::ArenaNotDrawing,
            "arena_not_drawing",
            "Resolume nekreslí na stenu",
            HealthTone::Error,
            true,
            false,
        ),
        (WallState::Ok, "ok", "OK", HealthTone::Ok, true, true),
    ];
    for (state, name, text, tone, shown, holds) in table {
        assert_eq!(state.name(), name);
        assert_eq!(serde_json::to_value(state).unwrap(), json!(name));
        assert_eq!(state.text_sk(), text, "{name}");
        assert_eq!(state.tone(), tone, "{name}");
        assert_eq!(state.shown(), shown, "{name}");
        assert_eq!(state.holds(), holds, "{name}");
    }
    assert_eq!(table.map(|row| row.0), EVERY_STATE);
    assert_eq!(WallState::default(), WallState::Unknown);
}

/// Each problem: its code, its few words, its full text.
type ProblemRow = (WallProblem, &'static str, &'static str, &'static str);

#[test]
fn every_problem_has_its_code_and_slovak_texts() {
    let table: Vec<ProblemRow> = vec![
        (
            WallProblem::Unreadable(WallFact::Arena),
            "unreadable:arena",
            "nedá sa overiť",
            "proces a okná Resolume Arena sa nepodarilo zistiť",
        ),
        (
            WallProblem::Unreadable(WallFact::ArenaOutput),
            "unreadable:arena_output",
            "nedá sa overiť",
            "uložený výstup Resolume Arena sa nepodarilo zistiť",
        ),
        (
            WallProblem::Unreadable(WallFact::Displays),
            "unreadable:displays",
            "nedá sa overiť",
            "zoznam obrazoviek Windows sa nepodarilo zistiť",
        ),
        (
            WallProblem::Unreadable(WallFact::Nvapi),
            "unreadable:nvapi",
            "nedá sa overiť",
            "obrazovky karty NVIDIA (NVAPI) sa nepodarilo zistiť",
        ),
        (
            WallProblem::ScreensOff {
                name: "Display 1".to_string(),
                width: 7680,
                height: 1080,
            },
            "screens_off",
            "obrazovky vypnuté",
            "Resolume chce obrazovku 7680×1080 (Display 1); Windows ju nemá a žiadna obrazovka \
             karty NVIDIA nie je pripojená — obrazovky sú vypnuté",
        ),
        (
            WallProblem::DisplayAbsent {
                name: "Display 1".to_string(),
                width: 7680,
                height: 1080,
                not_joined: false,
            },
            "display_absent",
            "Resolume chce obrazovku 7680×1080 (Display 1), Windows ju nemá",
            "Resolume chce obrazovku 7680×1080 (Display 1), Windows ju nemá",
        ),
        (
            WallProblem::DisplayAbsent {
                name: "Display 2".to_string(),
                width: 1920,
                height: 1080,
                not_joined: true,
            },
            "display_absent",
            "Resolume chce obrazovku 1920×1080 (Display 2), Windows ju nemá",
            "Resolume chce obrazovku 1920×1080 (Display 2), Windows ju nemá — obrazovky sú \
             pripojené, ale nie spojené (Surround)",
        ),
        (
            WallProblem::DisplaySize {
                name: "Display 1".to_string(),
                width: 7680,
                height: 1080,
                actual_width: 2560,
                actual_height: 1080,
                not_joined: true,
            },
            "display_size",
            "Resolume chce obrazovku 7680×1080 (Display 1), Windows ju má 2560×1080",
            "Resolume chce obrazovku 7680×1080 (Display 1), Windows ju má 2560×1080 — \
             obrazovky sú pripojené, ale nie spojené (Surround)",
        ),
        (
            WallProblem::DisplaySize {
                name: "Display 1".to_string(),
                width: 7680,
                height: 1080,
                actual_width: 7680,
                actual_height: 2160,
                not_joined: false,
            },
            "display_size",
            "Resolume chce obrazovku 7680×1080 (Display 1), Windows ju má 7680×2160",
            "Resolume chce obrazovku 7680×1080 (Display 1), Windows ju má 7680×2160",
        ),
        (
            WallProblem::ArenaNotDrawing {
                name: "Display 1".to_string(),
                left: 3840,
                top: 0,
                right: 11520,
                bottom: 1080,
            },
            "arena_not_drawing",
            "Resolume nekreslí na Display 1",
            "Resolume Arena nemá na Display 1 (3840,0 – 11520,1080) viditeľné výstupné okno \
             „Display“",
        ),
    ];
    for (problem, code, short, text) in table {
        assert_eq!(problem.code(), code);
        assert_eq!(problem.short_sk(), short, "{code}");
        assert_eq!(problem.text_sk(), text, "{code}");
        assert_eq!(
            problem.view(),
            WallProblemView {
                code: code.to_string(),
                short_sk: short.to_string(),
                text_sk: text.to_string(),
            }
        );
    }
}

/// SNV's Arena: preset `snv surround 7680`, one fullscreen display.
fn snv_output() -> ArenaOutput {
    ArenaOutput {
        advanced: true,
        preset: Some("snv surround 7680".to_string()),
        preset_found: true,
        displays: vec![WantedDisplay {
            name: "Display 1".to_string(),
            device_id: r"\Registry\Machine\System\CurrentControlSet\Control\Video\{49D71F75-AF5A-11EE-BACA-806E6F6E6963}\0000".to_string(),
            width: 7680,
            height: 1080,
            fullscreen: true,
        }],
    }
}

/// A checked status in `state` with `problems`, Arena's output as read.
fn checked(state: WallState, problems: &[WallProblem], output: Option<ArenaOutput>) -> WallStatus {
    WallStatus {
        checked: true,
        state,
        problems: problems.iter().map(WallProblem::view).collect(),
        read: WallRead {
            arena_output: output,
            ..WallRead::default()
        },
        checked_at_ms: Some(1_791_608_681_366),
        ..WallStatus::default()
    }
}

#[test]
fn the_marker_says_unchecked_until_the_first_answer_then_the_state() {
    assert_eq!(marker(&WallStatus::default()), "unchecked");
    let not_checked = WallStatus {
        state: WallState::Ok,
        ..WallStatus::default()
    };
    assert_eq!(marker(&not_checked), "unchecked");
    assert_eq!(marker(&checked(WallState::Ok, &[], None)), "ok");
    assert_eq!(
        marker(&checked(WallState::SimpleOutput, &[], None)),
        "simple_output"
    );
}

#[test]
fn no_segment_before_the_first_check_nor_without_a_display_output() {
    let not_checked = WallStatus {
        state: WallState::Ok,
        ..WallStatus::default()
    };
    assert_eq!(label(&not_checked), None);
    assert_eq!(label(&checked(WallState::SimpleOutput, &[], None)), None);
    assert_eq!(label(&checked(WallState::NoDisplayOutput, &[], None)), None);
}

#[test]
fn a_segment_says_the_first_problem_or_else_the_state() {
    let absent = WallProblem::DisplayAbsent {
        name: "Display 1".to_string(),
        width: 7680,
        height: 1080,
        not_joined: false,
    };
    let off = WallProblem::ScreensOff {
        name: "Display 2".to_string(),
        width: 1920,
        height: 1080,
    };
    let table = [
        (
            checked(WallState::Ok, &[], Some(snv_output())),
            HealthTone::Ok,
            "Stena: OK",
        ),
        (
            checked(WallState::ArenaOff, &[], None),
            HealthTone::Off,
            "Stena: Resolume nebeží",
        ),
        (
            checked(WallState::NoPreset, &[], None),
            HealthTone::Off,
            "Stena: Resolume nemá uložený výstup",
        ),
        (
            checked(WallState::ScreensOff, std::slice::from_ref(&off), None),
            HealthTone::Off,
            "Stena: obrazovky vypnuté",
        ),
        (
            checked(WallState::DisplayMissing, &[absent, off], None),
            HealthTone::Error,
            "Stena: Resolume chce obrazovku 7680×1080 (Display 1), Windows ju nemá",
        ),
        (
            checked(
                WallState::Unknown,
                &[WallProblem::Unreadable(WallFact::Displays)],
                None,
            ),
            HealthTone::Off,
            "Stena: nedá sa overiť",
        ),
    ];
    for (status, tone, text) in table {
        let (got_tone, got_text, _) = label(&status).unwrap();
        assert_eq!((got_tone, got_text.as_str()), (tone, text));
    }
}

#[test]
fn the_tooltip_holds_the_rule_what_arena_wants_and_every_problem() {
    let (_, _, ok_tip) = label(&checked(WallState::Ok, &[], Some(snv_output()))).unwrap();
    assert_eq!(
        ok_tip,
        format!("{RULE_SK} Resolume (preset „snv surround 7680“) chce: Display 1 7680×1080.")
    );
    let two = ArenaOutput {
        preset: None,
        displays: vec![
            snv_output().displays[0].clone(),
            WantedDisplay {
                name: "Display 2".to_string(),
                width: 1920,
                height: 1200,
                ..WantedDisplay::default()
            },
        ],
        ..snv_output()
    };
    let drawing = WallProblem::ArenaNotDrawing {
        name: "Display 2".to_string(),
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1200,
    };
    let (_, text, tip) = label(&checked(
        WallState::ArenaNotDrawing,
        std::slice::from_ref(&drawing),
        Some(two),
    ))
    .unwrap();
    assert_eq!(text, "Stena: Resolume nekreslí na Display 2");
    assert_eq!(
        tip,
        format!(
            "{RULE_SK} Resolume (preset „bez názvu“) chce: Display 1 7680×1080 · Display 2 \
             1920×1200. Zistené: Resolume Arena nemá na Display 2 (0,0 – 1920,1200) \
             viditeľné výstupné okno „Display“."
        )
    );
    // Arena's output read but wanting no display: the rule alone.
    let none = ArenaOutput {
        displays: Vec::new(),
        ..snv_output()
    };
    let (_, _, arena_off_tip) = label(&checked(WallState::ArenaOff, &[], Some(none))).unwrap();
    assert_eq!(arena_off_tip, RULE_SK);
}

#[test]
fn the_status_travels_with_every_field_named() {
    let status = WallStatus {
        read: WallRead {
            arena_running: Some(true),
            arena_windows: Some(vec![ArenaWindow {
                title: "Display".to_string(),
                visible: true,
                left: 3840,
                top: 0,
                right: 11520,
                bottom: 1080,
            }]),
            arena_output: Some(snv_output()),
            displays: Some(vec![Display {
                name: r"\\.\DISPLAY1".to_string(),
                device_key: "key".to_string(),
                primary: false,
                x: 3840,
                y: 0,
                width: 7680,
                height: 1080,
                hz: 60,
            }]),
            nvidia: NvidiaRead::Read,
            connected: Some(vec![Connected {
                id: DisplayId(0x8006_1083),
                connector: "HDMI".to_string(),
                flags: 0x0002_0055,
            }]),
            grids: Some(vec![Grid {
                rows: 1,
                columns: 3,
                displays: vec![DisplayId(0x8006_1086)],
                width: 2560,
                height: 1080,
                hz: 60,
            }]),
        },
        read_errors: vec!["why".to_string()],
        ..checked(WallState::Ok, &[], None)
    };
    let wire = serde_json::to_value(&status).unwrap();
    assert_eq!(wire["state"], "ok");
    assert_eq!(wire["checked"], true);
    assert_eq!(
        wire["read"]["arena_output"],
        json!({
            "advanced": true,
            "preset": "snv surround 7680",
            "preset_found": true,
            "displays": [{
                "name": "Display 1",
                "device_id": r"\Registry\Machine\System\CurrentControlSet\Control\Video\{49D71F75-AF5A-11EE-BACA-806E6F6E6963}\0000",
                "width": 7680, "height": 1080, "fullscreen": true,
            }],
        })
    );
    assert_eq!(
        wire["read"]["displays"][0],
        json!({"name": r"\\.\DISPLAY1", "device_key": "key", "primary": false, "x": 3840,
               "y": 0, "width": 7680, "height": 1080, "hz": 60})
    );
    assert_eq!(
        wire["read"]["connected"],
        json!([{"id": "0x80061083", "connector": "HDMI", "flags": 131157}])
    );
    assert_eq!(wire["read"]["grids"][0]["displays"], json!(["0x80061086"]));
    assert_eq!(wire["read"]["nvidia"], "read");
    assert_eq!(
        serde_json::to_value([NvidiaRead::NotRead, NvidiaRead::NoDriver, NvidiaRead::Read])
            .unwrap(),
        json!(["not_read", "no_driver", "read"])
    );
    assert_eq!(WallRead::default().nvidia, NvidiaRead::NotRead);
    assert_eq!(
        wire["read"]["arena_windows"][0],
        json!({"title": "Display", "visible": true, "left": 3840, "top": 0,
               "right": 11520, "bottom": 1080})
    );
    assert_eq!(wire["read_errors"], json!(["why"]));
    assert_eq!(wire["checked_at_ms"], 1_791_608_681_366_i64);
    let problems = serde_json::to_value(
        checked(
            WallState::Unknown,
            &[WallProblem::Unreadable(WallFact::Nvapi)],
            None,
        )
        .problems,
    )
    .unwrap();
    assert_eq!(
        problems,
        json!([{"code": "unreadable:nvapi", "short_sk": "nedá sa overiť",
                "text_sk": "obrazovky karty NVIDIA (NVAPI) sa nepodarilo zistiť"}])
    );
    let back: WallStatus = serde_json::from_value(wire).unwrap();
    assert_eq!(back, status);
    // A status with nothing in it (an older server, a mock) is the default.
    let empty: WallStatus = serde_json::from_value(json!({})).unwrap();
    assert_eq!(empty, WallStatus::default());
}

#[test]
fn the_guards_reasons_and_phases_have_names_and_slovak_words() {
    assert_eq!(GuardWhy::CrashReport.name(), "crash_report");
    assert_eq!(GuardWhy::Hung.name(), "hung");
    assert_eq!(GuardWhy::CrashReport.short_sk(), "Resolume spadol");
    assert_eq!(GuardWhy::Hung.short_sk(), "Resolume zamrzol");
    assert_eq!(
        GuardWhy::CrashReport.text_sk(),
        "Resolume Arena spadol a jeho hlásenie chýb (BugSplat) čakalo na klik v okne \
         „Crash Report“"
    );
    assert_eq!(
        GuardWhy::Hung.text_sk(),
        "Resolume Arena neodpovedal a jeho procesor stál 30 s"
    );
    assert_eq!(GuardPhase::Idle.name(), "idle");
    assert_eq!(GuardPhase::Relaunching.name(), "relaunching");
    assert_eq!(GuardPhase::NotBack.name(), "not_back");
    assert_eq!(
        serde_json::to_value([
            GuardPhase::Idle,
            GuardPhase::Relaunching,
            GuardPhase::NotBack
        ])
        .unwrap(),
        json!(["idle", "relaunching", "not_back"])
    );
    assert_eq!(
        serde_json::to_value([GuardWhy::CrashReport, GuardWhy::Hung]).unwrap(),
        json!(["crash_report", "hung"])
    );
}

/// The guard's record of an end of Arena 15176 for `why`, in `phase`.
fn ended(why: GuardWhy, phase: GuardPhase, error: Option<&str>) -> WallGuard {
    WallGuard {
        ends: 1,
        last: Some(GuardEnd {
            at_ms: 1_791_608_681_366,
            why,
            pid: 15176,
            monitors: vec![9188],
            error: error.map(str::to_string),
        }),
        phase,
    }
}

#[test]
fn the_guard_names_its_last_end_only_until_arena_runs_again() {
    assert_eq!(WallGuard::default().pending(), None);
    assert_eq!(WallGuard::default().short_sk(), None);
    assert_eq!(WallGuard::default().text_sk(), None);
    let idle = ended(GuardWhy::CrashReport, GuardPhase::Idle, None);
    assert_eq!(idle.pending(), None);
    assert_eq!(idle.short_sk(), None);
    assert_eq!(idle.text_sk(), None);
    let relaunching = ended(GuardWhy::CrashReport, GuardPhase::Relaunching, None);
    assert_eq!(relaunching.pending(), relaunching.last.as_ref());
    assert_eq!(
        relaunching.short_sk().as_deref(),
        Some("Resolume spadol — ukončený, čaká na nové spustenie")
    );
    assert_eq!(
        relaunching.text_sk().as_deref(),
        Some(
            "SongPlayer ukončil Resolume Arena (proces 15176), lebo Resolume Arena spadol a \
             jeho hlásenie chýb (BugSplat) čakalo na klik v okne „Crash Report“. Znova ho \
             spúšťa skript operátora; klipy spúšťa len Stream Deck."
        )
    );
    let not_back = ended(GuardWhy::Hung, GuardPhase::NotBack, None);
    assert_eq!(
        not_back.short_sk().as_deref(),
        Some("Resolume zamrzol — ukončený, ale znova sa nespustil")
    );
    assert_eq!(
        not_back.text_sk().as_deref(),
        Some(
            "SongPlayer ukončil Resolume Arena (proces 15176), lebo Resolume Arena \
             neodpovedal a jeho procesor stál 30 s. Znova ho spúšťa skript operátora; klipy \
             spúšťa len Stream Deck. Do 60 s sa znova nespustil."
        )
    );
    let refused = ended(
        GuardWhy::Hung,
        GuardPhase::Relaunching,
        Some("TerminateProcess(15176, Arena.exe) failed (error 5)"),
    );
    assert_eq!(
        refused.short_sk().as_deref(),
        Some("Resolume zamrzol — nepodarilo sa ho ukončiť")
    );
    assert_eq!(
        refused.text_sk().as_deref(),
        Some(
            "SongPlayer chcel ukončiť Resolume Arena (proces 15176), lebo Resolume Arena \
             neodpovedal a jeho procesor stál 30 s, ale nepodarilo sa: \
             TerminateProcess(15176, Arena.exe) failed (error 5)."
        )
    );
    let refused_long_ago = ended(
        GuardWhy::Hung,
        GuardPhase::NotBack,
        Some("OpenProcess(15176) failed (error 5)"),
    );
    assert_eq!(
        refused_long_ago.short_sk().as_deref(),
        Some("Resolume zamrzol — nepodarilo sa ho ukončiť")
    );
    assert!(refused_long_ago.text_sk().unwrap().ends_with(
        "ale nepodarilo sa: OpenProcess(15176) failed (error 5). Do 60 s sa znova nespustil."
    ));
}

#[test]
fn a_pending_end_reads_red_on_the_segment_until_arena_runs_again() {
    let off = WallStatus {
        guard: ended(GuardWhy::CrashReport, GuardPhase::Relaunching, None),
        ..checked(WallState::ArenaOff, &[], None)
    };
    let (tone, text, tip) = label(&off).unwrap();
    assert_eq!(tone, HealthTone::Error);
    assert_eq!(
        text,
        "Stena: Resolume spadol — ukončený, čaká na nové spustenie"
    );
    assert_eq!(tip, format!("{RULE_SK} {}", off.guard.text_sk().unwrap()));
    // The old process still drawing (its end failed): the end, not "OK".
    let still_drawing = WallStatus {
        guard: ended(
            GuardWhy::Hung,
            GuardPhase::Relaunching,
            Some("access denied"),
        ),
        ..checked(WallState::Ok, &[], Some(snv_output()))
    };
    let (tone, text, tip) = label(&still_drawing).unwrap();
    assert_eq!(
        (tone, text.as_str()),
        (
            HealthTone::Error,
            "Stena: Resolume zamrzol — nepodarilo sa ho ukončiť"
        )
    );
    assert!(
        tip.starts_with(&format!(
            "{RULE_SK} Resolume (preset „snv surround 7680“) chce: Display 1 7680×1080. \
             SongPlayer chcel ukončiť"
        )),
        "{tip}"
    );
    // Arena runs again: the segment is the wall's own again.
    let back = WallStatus {
        guard: ended(GuardWhy::CrashReport, GuardPhase::Idle, None),
        ..checked(WallState::Ok, &[], Some(snv_output()))
    };
    assert_eq!(
        label(&back),
        label(&checked(WallState::Ok, &[], Some(snv_output())))
    );
    // Nothing to show for an Arena that drives no physical display.
    let simple = WallStatus {
        guard: ended(GuardWhy::CrashReport, GuardPhase::Relaunching, None),
        ..checked(WallState::SimpleOutput, &[], None)
    };
    assert_eq!(label(&simple), None);
}

#[test]
fn the_rule_names_the_guards_one_exception() {
    assert!(RULE_SK.starts_with(
        "SongPlayer porovnáva obrazovky Windows s výstupom, ktorý má Resolume Arena uložený, a \
         len hlási — nič nenastavuje ani nemení."
    ));
    assert!(RULE_SK.ends_with(
        "Jediná výnimka: spadnutý alebo zamrznutý Resolume ukončí, aby sa mohol znova spustiť."
    ));
}

#[test]
fn the_guard_travels_with_every_field_named() {
    let status = WallStatus {
        guard: ended(GuardWhy::Hung, GuardPhase::NotBack, Some("why")),
        ..checked(WallState::ArenaOff, &[], None)
    };
    let wire = serde_json::to_value(&status).unwrap();
    assert_eq!(
        wire["guard"],
        json!({
            "ends": 1,
            "last": {"at_ms": 1_791_608_681_366_i64, "why": "hung", "pid": 15176,
                     "monitors": [9188], "error": "why"},
            "phase": "not_back",
        })
    );
    let back: WallStatus = serde_json::from_value(wire).unwrap();
    assert_eq!(back, status);
    // An older server sends no guard: nothing ended.
    let old: WallStatus = serde_json::from_value(json!({"checked": true, "state": "ok"})).unwrap();
    assert_eq!(old.guard, WallGuard::default());
    let bare: GuardEnd = serde_json::from_value(json!({})).unwrap();
    assert_eq!(bare, GuardEnd::default());
}
