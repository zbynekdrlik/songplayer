//! #259: the wall's Windows glue on the CI runner (Build (Windows)): the
//! process list holds this very test process by its image's file name, the
//! window list reads, and the walk over the real reads finds no Arena (the
//! runner runs none). Read only: nothing on the runner is changed.

use super::*;
use crate::wall::read::{Arena, read_facts};

#[test]
fn the_process_list_holds_this_test_process_by_its_file_name() {
    let me = std::process::id();
    let exe = std::env::current_exe().unwrap();
    let name = exe.file_name().unwrap().to_string_lossy().into_owned();
    let processes = WinReader.processes().unwrap();
    assert!(
        processes
            .iter()
            .any(|(pid, image)| *pid == me && image.eq_ignore_ascii_case(&name)),
        "pid {me} ({name}) is not in the {} processes listed",
        processes.len()
    );
}

#[test]
fn no_process_asked_for_has_no_window() {
    assert_eq!(WinReader.windows_of(&[]), Ok(Vec::new()));
}

#[test]
fn the_walk_over_the_runner_finds_no_arena() {
    let facts = read_facts(&WinReader);
    assert_eq!(facts.arena, Ok(Arena::default()));
}
