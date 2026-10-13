//! #259: the walk over a scripted box — Arena's process and windows found,
//! Arena's saved output setup read file by file, NVAPI asked only when the
//! decision needs it and decoded read by read.

use sp_core::wall::ArenaOutput;

use super::*;
use crate::wall::fake::{
    ADVANCED, ARENA_PID, FakeReader, arena_main, arena_output as output_window, documents,
    snv_facts, snv_nvidia, snv_nvidia_raw, snv_output, span, vdd,
};
use crate::wall::resolume;

#[test]
fn snvs_box_reads_as_observed_without_asking_nvapi() {
    let reader = FakeReader::snv();
    assert_eq!(read_facts(&reader), snv_facts());
    assert_eq!(reader.nvidia_asked.get(), 0);
}

/// SNV's box with the span gone from Windows' displays.
fn span_gone() -> FakeReader {
    FakeReader {
        displays: Ok(vec![vdd()]),
        ..FakeReader::snv()
    }
}

#[test]
fn nvapi_is_asked_once_when_a_wanted_display_is_gone_or_resized() {
    let gone = span_gone();
    let facts = read_facts(&gone);
    assert_eq!(facts.nvidia, snv_nvidia());
    assert_eq!(gone.nvidia_asked.get(), 1);
    let resized = FakeReader {
        displays: Ok(vec![
            vdd(),
            sp_core::wall::Display {
                width: 2560,
                ..span()
            },
        ]),
        ..FakeReader::snv()
    };
    assert_eq!(read_facts(&resized).nvidia, snv_nvidia());
    assert_eq!(resized.nvidia_asked.get(), 1);
}

#[test]
fn no_nvidia_driver_is_no_nvapi_not_an_error() {
    let reader = FakeReader {
        nvidia: None,
        ..span_gone()
    };
    assert_eq!(read_facts(&reader).nvidia, NvidiaFacts::NoDriver);
}

#[test]
fn a_failed_nvapi_read_keeps_the_other() {
    let grids_failed = FakeReader {
        nvidia: Some(NvidiaRaw {
            grids: Err("NvAPI_Mosaic_EnumDisplayGrids answered NVAPI status -3".to_string()),
            ..snv_nvidia_raw()
        }),
        ..span_gone()
    };
    let NvidiaFacts::Read { connected, grids } = read_facts(&grids_failed).nvidia else {
        panic!("NVAPI was read");
    };
    assert_eq!(connected.map(|c| c.len()), Ok(3));
    assert_eq!(
        grids,
        Err("NvAPI_Mosaic_EnumDisplayGrids answered NVAPI status -3".to_string())
    );
    let connected_failed = FakeReader {
        nvidia: Some(NvidiaRaw {
            connected: Err("NvAPI_Initialize answered NVAPI status -1".to_string()),
            ..snv_nvidia_raw()
        }),
        ..span_gone()
    };
    let NvidiaFacts::Read { connected, grids } = read_facts(&connected_failed).nvidia else {
        panic!("NVAPI was read");
    };
    assert_eq!(
        connected,
        Err("NvAPI_Initialize answered NVAPI status -1".to_string())
    );
    assert_eq!(grids.map(|g| g.len()), Ok(1));
}

#[test]
fn only_arenas_own_windows_are_read() {
    let reader = FakeReader::snv();
    assert_eq!(
        arena(&reader),
        Ok(Arena {
            running: true,
            windows: vec![arena_main(), output_window()],
        })
    );
    // SongPlayer's own window titled "Display" (pid 777) is not Arena's.
    assert_eq!(*reader.asked.borrow(), [vec![ARENA_PID]]);
}

#[test]
fn every_arena_process_counts_whatever_its_names_case() {
    let reader = FakeReader {
        processes: Ok(vec![
            (ARENA_PID, "Arena.exe".to_string()),
            (6000, "ARENA.EXE".to_string()),
            (7000, "Arena.exe.bak".to_string()),
        ]),
        windows: Ok(vec![(6000, output_window()), (7000, arena_main())]),
        ..FakeReader::snv()
    };
    assert_eq!(
        arena(&reader),
        Ok(Arena {
            running: true,
            windows: vec![output_window()],
        })
    );
    assert_eq!(*reader.asked.borrow(), [vec![ARENA_PID, 6000]]);
}

#[test]
fn no_arena_process_reads_no_window() {
    let reader = FakeReader {
        processes: Ok(vec![(4, "System".to_string())]),
        ..FakeReader::snv()
    };
    assert_eq!(arena(&reader), Ok(Arena::default()));
    assert!(reader.asked.borrow().is_empty());
}

#[test]
fn a_failed_process_or_window_read_is_unreadable() {
    let no_list = FakeReader {
        processes: Err("CreateToolhelp32Snapshot failed (error 5)".to_string()),
        ..FakeReader::snv()
    };
    assert_eq!(
        arena(&no_list),
        Err("CreateToolhelp32Snapshot failed (error 5)".to_string())
    );
    let no_windows = FakeReader {
        windows: Err("EnumWindows failed (error 5)".to_string()),
        ..FakeReader::snv()
    };
    assert_eq!(
        arena(&no_windows),
        Err("EnumWindows failed (error 5)".to_string())
    );
}

#[test]
fn arenas_output_is_its_saved_preset() {
    assert_eq!(arena_output(&FakeReader::snv()), Ok(snv_output()));
}

#[test]
fn no_simple_output_file_or_the_advanced_output_off_is_the_simple_output() {
    let docs = documents();
    let simple = resolume::simple_output_file(&docs);
    let no_file = FakeReader::snv().with_file(simple.clone(), None);
    assert_eq!(arena_output(&no_file), Ok(ArenaOutput::default()));
    let off = FakeReader::snv().with_file(
        simple,
        Some(Ok(r#"<SimpleSetup advancedModeEnabled="0"/>"#.to_string())),
    );
    assert_eq!(arena_output(&off), Ok(ArenaOutput::default()));
}

#[test]
fn an_advanced_output_with_no_preset_saved_is_no_preset_never_an_error() {
    let docs = documents();
    let setup = resolume::advanced_output_file(&docs);
    let preset = resolume::preset_file(&docs, "snv surround 7680");
    let none = |preset: Option<&str>| {
        Ok(ArenaOutput {
            advanced: true,
            preset: preset.map(str::to_string),
            preset_found: false,
            displays: Vec::new(),
        })
    };
    let named = ADVANCED.replace("snv surround 7680", "../escape");
    let cases = [
        (FakeReader::snv().with_file(setup.clone(), None), none(None)),
        (
            FakeReader::snv().with_file(setup.clone(), Some(Ok("<XmlState/>".to_string()))),
            none(None),
        ),
        (
            FakeReader::snv().with_file(
                setup.clone(),
                Some(Ok(ADVANCED.replace("snv surround 7680", ""))),
            ),
            none(None),
        ),
        (
            FakeReader::snv().with_file(setup, Some(Ok(named))),
            none(Some("../escape")),
        ),
        (
            FakeReader::snv().with_file(preset, None),
            none(Some("snv surround 7680")),
        ),
    ];
    for (reader, output) in cases {
        assert_eq!(arena_output(&reader), output);
    }
}

#[test]
fn a_file_the_advanced_output_reads_that_is_bad_is_named() {
    let docs = documents();
    let setup = resolume::advanced_output_file(&docs);
    let preset = resolume::preset_file(&docs, "snv surround 7680");
    let simple = resolume::simple_output_file(&docs);
    let cases = [
        (
            FakeReader::snv().with_file(setup.clone(), Some(Ok("<ScreenSetup".to_string()))),
            format!("{}: ", setup.display()),
        ),
        (
            FakeReader::snv().with_file(setup.clone(), Some(Err("access denied".to_string()))),
            "access denied".to_string(),
        ),
        (
            FakeReader::snv().with_file(preset.clone(), Some(Err("sharing violation".to_string()))),
            "sharing violation".to_string(),
        ),
        (
            FakeReader::snv().with_file(
                preset.clone(),
                Some(Ok("<XmlState><OutputDeviceDisplay/></XmlState>".to_string())),
            ),
            format!("{}: <OutputDeviceDisplay> has no name", preset.display()),
        ),
        (
            FakeReader::snv().with_file(simple.clone(), Some(Ok("<SimpleSetup/>".to_string()))),
            format!(
                "{}: <SimpleSetup> advancedModeEnabled is None, not 0 or 1",
                simple.display()
            ),
        ),
        (
            FakeReader::snv().with_file(simple, Some(Err("access denied".to_string()))),
            "access denied".to_string(),
        ),
        (
            FakeReader {
                documents: Err("USERPROFILE is not set".to_string()),
                ..FakeReader::snv()
            },
            "USERPROFILE is not set".to_string(),
        ),
    ];
    for (reader, why) in cases {
        let read = arena_output(&reader);
        assert!(
            read.as_ref().is_err_and(|err| err.starts_with(&why)),
            "{read:?} should fail with {why:?}"
        );
    }
}

#[test]
fn the_preset_named_is_the_one_read() {
    let docs = documents();
    let other = resolume::preset_file(&docs, "pp wall");
    let reader = FakeReader::snv()
        .with_file(
            resolume::advanced_output_file(&docs),
            Some(Ok(ADVANCED.replace("snv surround 7680", "pp wall"))),
        )
        .with_file(
            other,
            Some(Ok("<XmlState><OutputDeviceSpout/></XmlState>".to_string())),
        );
    assert_eq!(
        arena_output(&reader),
        Ok(ArenaOutput {
            advanced: true,
            preset: Some("pp wall".to_string()),
            preset_found: true,
            displays: Vec::new(),
        })
    );
}

#[test]
fn nothing_read_is_every_fact_unread_for_one_reason() {
    let unread = WallFacts::unread("not Windows");
    assert_eq!(unread.arena, Err("not Windows".to_string()));
    assert_eq!(unread.arena_output, Err("not Windows".to_string()));
    assert_eq!(unread.displays, Err("not Windows".to_string()));
    assert_eq!(unread.nvidia, NvidiaFacts::NotRead);
}

#[test]
fn nvapis_structures_are_decoded() {
    let decoded = nvidia(&FakeReader::snv());
    assert_eq!(decoded, snv_nvidia());
    let NvidiaFacts::Read {
        connected: Ok(connected),
        grids: Ok(grids),
    } = decoded
    else {
        panic!("both reads answered");
    };
    assert_eq!(connected[0].connector, "HDMI");
    assert_eq!(grids[0].displays.len(), 3);
}
