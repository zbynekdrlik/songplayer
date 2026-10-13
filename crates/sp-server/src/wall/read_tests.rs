//! #259: the walk over a scripted box — NVAPI decoded, Arena's process and
//! windows found, Arena's saved output setup read file by file.

use sp_core::wall::ArenaOutput;

use super::*;
use crate::wall::fake::{
    ADVANCED, ARENA_PID, FakeReader, arena_main, arena_output as output_window, documents,
    snv_facts, snv_output,
};
use crate::wall::resolume;

#[test]
fn snvs_box_reads_as_observed() {
    assert_eq!(read_facts(&FakeReader::snv()), snv_facts());
}

#[test]
fn no_nvidia_driver_is_no_nvapi_not_an_error() {
    let reader = FakeReader {
        nvidia: Ok(None),
        ..FakeReader::snv()
    };
    assert_eq!(read_facts(&reader).nvidia, Ok(None));
    let failing = FakeReader {
        nvidia: Err("NvAPI_Initialize answered NVAPI status -1".to_string()),
        ..FakeReader::snv()
    };
    assert_eq!(
        read_facts(&failing).nvidia,
        Err("NvAPI_Initialize answered NVAPI status -1".to_string())
    );
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
fn a_file_the_advanced_output_needs_that_is_missing_or_bad_is_named() {
    let docs = documents();
    let setup = resolume::advanced_output_file(&docs);
    let preset = resolume::preset_file(&docs, "snv surround 7680");
    let simple = resolume::simple_output_file(&docs);
    let cases = [
        (
            FakeReader::snv().with_file(setup.clone(), None),
            format!("{} does not exist", setup.display()),
        ),
        (
            FakeReader::snv().with_file(preset.clone(), None),
            format!("{} does not exist", preset.display()),
        ),
        (
            FakeReader::snv().with_file(setup.clone(), Some(Ok("<XmlState/>".to_string()))),
            format!("{}: no <ScreenSetup>", setup.display()),
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
        assert_eq!(arena_output(&reader), Err(why));
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
    assert_eq!(unread.nvidia, Err("not Windows".to_string()));
}

#[test]
fn nvapis_structures_are_decoded() {
    let raw = FakeReader::snv().nvidia.unwrap().unwrap();
    let decoded = nvidia(&raw);
    assert_eq!(decoded, snv_facts().nvidia.unwrap().unwrap());
    assert_eq!(decoded.connected[0].connector, "HDMI");
    assert_eq!(decoded.grids[0].displays.len(), 3);
}
