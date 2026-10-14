//! #259: the wall report's decision, step by step, on SNV's box as observed
//! and the ways it differs (each an operator's possible setup or a power
//! cycle, never compared with a layout of SongPlayer's own).

use sp_core::wall::{
    ArenaOutput, ArenaWindow, Display, NvidiaRead, WallFact, WallProblem, WallRead, WallState,
    WantedDisplay,
};

use super::*;
use crate::wall::fake::{
    LEFT, MIDDLE, RIGHT, arena_main, arena_output, nvidia_read, screens, single_grid,
    snv_arena_process, snv_facts, snv_nvidia, snv_output, span, span_grid, vdd, wanted,
};
use crate::wall::guard::ArenaProcess;
use crate::wall::read::{Arena, NvidiaFacts, WallFacts};

/// SNV's box with Windows' displays `displays`.
fn with_displays(displays: Vec<Display>) -> WallFacts {
    WallFacts {
        displays: Ok(displays),
        ..snv_facts()
    }
}

/// SNV's box with NVAPI saying `nvidia`.
fn with_nvidia(facts: WallFacts, nvidia: NvidiaFacts) -> WallFacts {
    WallFacts { nvidia, ..facts }
}

/// SNV's box with Arena's windows `windows`.
fn with_windows(windows: Vec<ArenaWindow>) -> WallFacts {
    WallFacts {
        arena: Ok(Arena {
            running: true,
            windows,
        }),
        ..snv_facts()
    }
}

/// The span with another size (the Surround dissolved: one screen left).
fn span_of(width: u32, height: u32) -> Display {
    Display {
        width,
        height,
        ..span()
    }
}

/// The three screens connected, each alone.
fn dissolved() -> NvidiaFacts {
    nvidia_read(
        Ok(screens()),
        Ok(vec![
            single_grid(LEFT),
            single_grid(MIDDLE),
            single_grid(RIGHT),
        ]),
    )
}

/// NVAPI read, nothing connected (the screens powered off).
fn nothing_connected() -> NvidiaFacts {
    nvidia_read(Ok(Vec::new()), Ok(Vec::new()))
}

/// NVAPI read, both reads failed.
fn nvapi_failed() -> NvidiaFacts {
    nvidia_read(
        Err("NvAPI_GPU_GetConnectedDisplayIds answered NVAPI status -1".to_string()),
        Err("NvAPI_Mosaic_EnumDisplayGrids answered NVAPI status -1".to_string()),
    )
}

fn absent(not_joined: bool) -> WallProblem {
    WallProblem::DisplayAbsent {
        name: "Display 1".to_string(),
        width: 7680,
        height: 1080,
        not_joined,
    }
}

fn screens_off() -> WallProblem {
    WallProblem::ScreensOff {
        name: "Display 1".to_string(),
        width: 7680,
        height: 1080,
    }
}

fn verdict(state: WallState, problems: Vec<WallProblem>) -> Verdict {
    Verdict { state, problems }
}

#[test]
fn snvs_box_shows_arena_on_the_wall() {
    assert_eq!(decide(&snv_facts()), verdict(WallState::Ok, Vec::new()));
}

#[test]
fn arena_not_running_is_all_there_is_to_say() {
    let off = WallFacts {
        arena: Ok(Arena::default()),
        ..WallFacts::unread("not read")
    };
    assert_eq!(decide(&off), verdict(WallState::ArenaOff, Vec::new()));
    let unread = WallFacts {
        arena: Err("no snapshot".to_string()),
        ..snv_facts()
    };
    assert_eq!(
        decide(&unread),
        verdict(
            WallState::Unknown,
            vec![WallProblem::Unreadable(WallFact::Arena)]
        )
    );
}

#[test]
fn an_arena_that_drives_no_physical_display_has_nothing_to_check() {
    let unread = WallFacts {
        arena_output: Err("bad preset".to_string()),
        ..snv_facts()
    };
    assert_eq!(
        decide(&unread),
        verdict(
            WallState::Unknown,
            vec![WallProblem::Unreadable(WallFact::ArenaOutput)]
        )
    );
    let simple = WallFacts {
        arena_output: Ok(ArenaOutput::default()),
        displays: Err("not read".to_string()),
        ..snv_facts()
    };
    assert_eq!(
        decide(&simple),
        verdict(WallState::SimpleOutput, Vec::new())
    );
    let spout_only = WallFacts {
        arena_output: Ok(ArenaOutput {
            displays: Vec::new(),
            ..snv_output()
        }),
        displays: Err("not read".to_string()),
        ..snv_facts()
    };
    assert_eq!(
        decide(&spout_only),
        verdict(WallState::NoDisplayOutput, Vec::new())
    );
}

#[test]
fn an_advanced_output_with_no_preset_saved_is_calm() {
    // No `AdvancedOutput.xml`, no preset named, or a preset file that does
    // not exist: the operator's own setup, never `unknown`.
    for preset in [None, Some("deleted preset".to_string())] {
        let facts = WallFacts {
            arena_output: Ok(ArenaOutput {
                advanced: true,
                preset,
                preset_found: false,
                displays: Vec::new(),
            }),
            displays: Err("not read".to_string()),
            ..snv_facts()
        };
        assert_eq!(decide(&facts), verdict(WallState::NoPreset, Vec::new()));
    }
    // Displays count only from a preset that was found.
    let not_found = WallFacts {
        arena_output: Ok(ArenaOutput {
            preset_found: false,
            ..snv_output()
        }),
        ..snv_facts()
    };
    assert_eq!(decide(&not_found), verdict(WallState::NoPreset, Vec::new()));
}

#[test]
fn windows_displays_unread_cannot_tell() {
    let unread = WallFacts {
        displays: Err("EnumDisplaySettingsW failed".to_string()),
        ..snv_facts()
    };
    assert_eq!(
        decide(&unread),
        verdict(
            WallState::Unknown,
            vec![WallProblem::Unreadable(WallFact::Displays)]
        )
    );
}

#[test]
fn the_span_gone_with_no_screen_connected_is_screens_off() {
    let night = with_nvidia(with_displays(vec![vdd()]), nothing_connected());
    assert_eq!(
        decide(&night),
        verdict(WallState::ScreensOff, vec![screens_off()])
    );
    // A box with no NVIDIA driver cannot tell more: calm too.
    let no_driver = with_nvidia(with_displays(vec![vdd()]), NvidiaFacts::NoDriver);
    assert_eq!(
        decide(&no_driver),
        verdict(WallState::ScreensOff, vec![screens_off()])
    );
    // The grids unread: nothing connected still says it.
    let grids_unread = with_nvidia(
        with_displays(vec![vdd()]),
        nvidia_read(Ok(Vec::new()), Err("no grids".to_string())),
    );
    assert_eq!(
        decide(&grids_unread),
        verdict(WallState::ScreensOff, vec![screens_off()])
    );
}

#[test]
fn the_span_gone_while_screens_are_connected_is_missing() {
    let dissolved_box = with_nvidia(with_displays(vec![vdd()]), dissolved());
    assert_eq!(
        decide(&dissolved_box),
        verdict(WallState::DisplayMissing, vec![absent(true)])
    );
    // The screens joined into a span Windows shows under another key (the
    // operator re-cabled): missing, with no "not joined" detail.
    let other_key = Display {
        device_key: "another key".to_string(),
        ..span()
    };
    let joined_elsewhere = with_nvidia(with_displays(vec![vdd(), other_key]), snv_nvidia());
    assert_eq!(
        decide(&joined_elsewhere),
        verdict(WallState::DisplayMissing, vec![absent(false)])
    );
    // A failed grid read keeps the connected screens: missing, with no
    // "not joined" detail (the grids were not read).
    let grids_unread = with_nvidia(
        with_displays(vec![vdd()]),
        nvidia_read(Ok(screens()), Err("no grids".to_string())),
    );
    assert_eq!(
        decide(&grids_unread),
        verdict(WallState::DisplayMissing, vec![absent(false)])
    );
}

#[test]
fn the_span_gone_with_nvapi_unread_cannot_tell() {
    let connected_unread = nvidia_read(Err("no connected".to_string()), Ok(vec![span_grid()]));
    for nvidia in [nvapi_failed(), connected_unread, NvidiaFacts::NotRead] {
        let unread = with_nvidia(with_displays(vec![vdd()]), nvidia);
        assert_eq!(
            decide(&unread),
            verdict(
                WallState::Unknown,
                vec![WallProblem::Unreadable(WallFact::Nvapi)]
            )
        );
    }
    // NVAPI is never needed while every display is there.
    let fine = with_nvidia(snv_facts(), nvapi_failed());
    assert_eq!(decide(&fine), verdict(WallState::Ok, Vec::new()));
}

#[test]
fn nvapi_is_needed_only_for_a_display_gone_or_resized() {
    let needs = |facts: WallFacts| needs_nvidia(&facts);
    assert!(!needs(snv_facts()));
    assert!(needs(with_displays(vec![vdd()])));
    assert!(needs(with_displays(vec![vdd(), span_of(2560, 1080)])));
    assert!(needs(with_displays(vec![vdd(), span_of(7680, 2160)])));
    // Arena not running, or no advanced output: the decision stops before.
    assert!(!needs(WallFacts {
        arena: Ok(Arena::default()),
        ..with_displays(vec![vdd()])
    }));
    assert!(!needs(WallFacts {
        arena_output: Ok(ArenaOutput {
            advanced: false,
            ..snv_output()
        }),
        ..with_displays(vec![vdd()])
    }));
    // A fact before it unread: `unknown` already, NVAPI not asked.
    for unread in [
        WallFacts {
            arena: Err("no snapshot".to_string()),
            ..with_displays(vec![vdd()])
        },
        WallFacts {
            arena_output: Err("bad preset".to_string()),
            ..with_displays(vec![vdd()])
        },
        WallFacts {
            displays: Err("no displays".to_string()),
            ..snv_facts()
        },
    ] {
        assert!(!needs(unread));
    }
}

#[test]
fn a_wanted_display_fits_only_with_its_key_and_size() {
    let all = [vdd(), span()];
    assert!(fits(&wanted(), &all));
    assert!(!fits(&wanted(), &[vdd()]));
    assert!(!fits(&wanted(), &[vdd(), span_of(2560, 1080)]));
    assert!(!fits(&wanted(), &[vdd(), span_of(7680, 2160)]));
}

#[test]
fn the_display_with_another_size_is_missing() {
    let one_screen = with_nvidia(with_displays(vec![vdd(), span_of(2560, 1080)]), dissolved());
    let size = |actual_width, actual_height, not_joined| WallProblem::DisplaySize {
        name: "Display 1".to_string(),
        width: 7680,
        height: 1080,
        actual_width,
        actual_height,
        not_joined,
    };
    assert_eq!(
        decide(&one_screen),
        verdict(WallState::DisplayMissing, vec![size(2560, 1080, true)])
    );
    // Only the height differs; the span is formed.
    let taller = with_displays(vec![vdd(), span_of(7680, 2160)]);
    assert_eq!(
        decide(&taller),
        verdict(WallState::DisplayMissing, vec![size(7680, 2160, false)])
    );
    // NVAPI unread, absent, or its grids unread: no detail, never
    // "unknown" (the size alone tells it).
    let grids_unread = nvidia_read(Ok(screens()), Err("no grids".to_string()));
    for nvidia in [
        nvapi_failed(),
        NvidiaFacts::NoDriver,
        NvidiaFacts::NotRead,
        grids_unread,
    ] {
        let facts = with_nvidia(with_displays(vec![vdd(), span_of(2560, 1080)]), nvidia);
        assert_eq!(
            decide(&facts),
            verdict(WallState::DisplayMissing, vec![size(2560, 1080, false)])
        );
    }
}

#[test]
fn the_device_key_matches_in_any_case() {
    let lower = Display {
        device_key: span().device_key.to_ascii_lowercase(),
        ..span()
    };
    assert_eq!(
        decide(&with_displays(vec![vdd(), lower])),
        verdict(WallState::Ok, Vec::new())
    );
}

/// Arena wants Display 2 (1920×1080, absent, listed first) and Display 1.
fn two_wanted() -> ArenaOutput {
    ArenaOutput {
        displays: vec![
            WantedDisplay {
                name: "Display 2".to_string(),
                device_id: "the second key".to_string(),
                width: 1920,
                height: 1080,
                fullscreen: true,
            },
            wanted(),
        ],
        ..snv_output()
    }
}

#[test]
fn a_missing_display_is_named_before_one_switched_off() {
    let facts = WallFacts {
        arena_output: Ok(two_wanted()),
        displays: Ok(vec![vdd(), span_of(2560, 1080)]),
        nvidia: nothing_connected(),
        ..snv_facts()
    };
    let second_off = WallProblem::ScreensOff {
        name: "Display 2".to_string(),
        width: 1920,
        height: 1080,
    };
    assert_eq!(
        decide(&facts),
        verdict(
            WallState::DisplayMissing,
            vec![
                WallProblem::DisplaySize {
                    name: "Display 1".to_string(),
                    width: 7680,
                    height: 1080,
                    actual_width: 2560,
                    actual_height: 1080,
                    not_joined: false,
                },
                second_off.clone(),
            ]
        )
    );
    let both_off = WallFacts {
        displays: Ok(vec![vdd()]),
        ..facts
    };
    assert_eq!(
        decide(&both_off),
        verdict(WallState::ScreensOff, vec![second_off, screens_off()])
    );
}

/// The span's rectangle as the problem names it.
fn not_drawing() -> WallProblem {
    WallProblem::ArenaNotDrawing {
        name: "Display 1".to_string(),
        left: 3840,
        top: 0,
        right: 11520,
        bottom: 1080,
    }
}

#[test]
fn arena_with_no_output_window_on_its_display_is_not_drawing() {
    // Observed on SNV: after the span came back Arena's output window did
    // not reopen; its main window was stretched over the wall.
    let stretched = ArenaWindow {
        right: 7688,
        bottom: 1040,
        left: -7,
        top: -7,
        ..arena_main()
    };
    for windows in [vec![arena_main()], vec![stretched]] {
        assert_eq!(
            decide(&with_windows(windows)),
            verdict(WallState::ArenaNotDrawing, vec![not_drawing()])
        );
    }
}

#[test]
fn only_a_visible_window_titled_display_that_covers_the_display_draws() {
    let output = arena_output();
    let not_drawn = [
        ArenaWindow {
            visible: false,
            ..output.clone()
        },
        ArenaWindow {
            title: "Display 1".to_string(),
            ..output.clone()
        },
        ArenaWindow {
            left: 3841,
            ..output.clone()
        },
        ArenaWindow {
            top: 1,
            ..output.clone()
        },
        ArenaWindow {
            right: 11519,
            ..output.clone()
        },
        ArenaWindow {
            bottom: 1079,
            ..output.clone()
        },
    ];
    for window in not_drawn {
        assert_eq!(
            decide(&with_windows(vec![arena_main(), window.clone()])),
            verdict(WallState::ArenaNotDrawing, vec![not_drawing()]),
            "{window:?}"
        );
    }
    let larger = ArenaWindow {
        left: -8,
        top: -8,
        right: 11528,
        bottom: 1088,
        ..output
    };
    assert_eq!(
        decide(&with_windows(vec![larger])),
        verdict(WallState::Ok, Vec::new())
    );
}

#[test]
fn a_windowed_output_is_never_checked_for_drawing() {
    let windowed = ArenaOutput {
        displays: vec![WantedDisplay {
            fullscreen: false,
            ..wanted()
        }],
        ..snv_output()
    };
    let facts = WallFacts {
        arena_output: Ok(windowed),
        ..with_windows(vec![arena_main()])
    };
    assert_eq!(decide(&facts), verdict(WallState::Ok, Vec::new()));
}

#[test]
fn screens_are_not_joined_when_connected_in_no_multi_screen_grid() {
    assert!(not_joined(&dissolved()));
    assert!(!not_joined(&snv_nvidia()));
    assert!(not_joined(&nvidia_read(Ok(screens()), Ok(Vec::new()))));
    assert!(!not_joined(&nothing_connected()));
    // Told only from both reads.
    for unread in [
        nvidia_read(Ok(screens()), Err("no grids".to_string())),
        nvidia_read(Err("no connected".to_string()), Ok(Vec::new())),
        NvidiaFacts::NotRead,
        NvidiaFacts::NoDriver,
    ] {
        assert!(!not_joined(&unread), "{unread:?}");
    }
}

#[test]
fn a_rectangle_runs_from_the_displays_place_by_its_size() {
    assert_eq!(
        rect(&span()),
        Rect {
            left: 3840,
            top: 0,
            right: 11520,
            bottom: 1080,
        }
    );
    let below = Display {
        x: -1920,
        y: 2160,
        width: 1920,
        height: 1200,
        ..span()
    };
    assert_eq!(
        rect(&below),
        Rect {
            left: -1920,
            top: 2160,
            right: 0,
            bottom: 3360,
        }
    );
}

#[test]
fn the_status_shows_every_fact_read_and_each_reason_once() {
    assert_eq!(
        read_view(&snv_facts()),
        WallRead {
            arena_running: Some(true),
            arena_windows: Some(vec![arena_main(), arena_output()]),
            arena_output: Some(snv_output()),
            displays: Some(vec![vdd(), span()]),
            nvidia: NvidiaRead::NotRead,
            connected: None,
            grids: None,
        }
    );
    assert_eq!(read_errors(&snv_facts()), Vec::<String>::new());
    let nvidia_view = |nvidia: NvidiaFacts| {
        let view = read_view(&with_nvidia(snv_facts(), nvidia));
        (view.nvidia, view.connected, view.grids)
    };
    assert_eq!(
        nvidia_view(snv_nvidia()),
        (NvidiaRead::Read, Some(screens()), Some(vec![span_grid()]))
    );
    assert_eq!(
        nvidia_view(NvidiaFacts::NoDriver),
        (NvidiaRead::NoDriver, None, None)
    );
    // One read failing never hides the other.
    assert_eq!(
        nvidia_view(nvidia_read(Ok(screens()), Err("no grids".to_string()))),
        (NvidiaRead::Read, Some(screens()), None)
    );
    assert_eq!(
        nvidia_view(nvidia_read(
            Err("no connected".to_string()),
            Ok(vec![span_grid()])
        )),
        (NvidiaRead::Read, None, Some(vec![span_grid()]))
    );
    let unread = WallFacts::unread("the wall read did not finish within 10s");
    assert_eq!(read_view(&unread), WallRead::default());
    assert_eq!(
        read_errors(&unread),
        ["the wall read did not finish within 10s"]
    );
    let three = WallFacts {
        displays: Err("no displays".to_string()),
        nvidia: nvidia_read(Err("no connected".to_string()), Err("no grids".to_string())),
        ..snv_facts()
    };
    assert_eq!(
        read_errors(&three),
        ["no displays", "no connected", "no grids"]
    );
    let no_driver = with_nvidia(snv_facts(), NvidiaFacts::NoDriver);
    assert_eq!(read_errors(&no_driver), Vec::<String>::new());
}

#[test]
fn the_guards_read_errors_follow_the_others_each_once() {
    let list_failed = WallFacts {
        guard: Err("no process list".to_string()),
        ..snv_facts()
    };
    assert_eq!(read_errors(&list_failed), ["no process list"]);
    let each = WallFacts {
        displays: Err("no displays".to_string()),
        guard: Ok(vec![
            ArenaProcess {
                times: Err("no times".to_string()),
                hung: Err("no windows".to_string()),
                crash_report: Err("no windows".to_string()),
                ..snv_arena_process()
            },
            ArenaProcess {
                pid: 6000,
                crash_report: Err("no report".to_string()),
                ..snv_arena_process()
            },
        ]),
        ..snv_facts()
    };
    assert_eq!(
        read_errors(&each),
        ["no displays", "no times", "no windows", "no report"]
    );
    // A guard read that fails changes nothing the decision reads.
    assert_eq!(
        decide(&each),
        decide(&WallFacts {
            guard: Ok(Vec::new()),
            ..each.clone()
        })
    );
}
