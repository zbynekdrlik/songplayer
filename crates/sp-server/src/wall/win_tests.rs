//! #259: the wall's Windows glue on the CI runner (Build (Windows)): the
//! process list holds this very test process by its image's file name, the
//! window list reads, the display and window reads run per-monitor DPI
//! aware and give the thread back as it was, and the walk over the real
//! reads finds no Arena (the runner runs none). Read only: nothing on the
//! runner is changed.

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
fn the_reads_are_per_monitor_aware_and_give_the_thread_back_as_it_was() {
    use windows_sys::Win32::UI::HiDpi::{
        AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        GetThreadDpiAwarenessContext,
    };
    // SAFETY (each call): reads or compares this thread's DPI awareness.
    let before = unsafe { GetThreadDpiAwarenessContext() };
    {
        let _dpi = DpiAware::per_monitor();
        let during = unsafe { GetThreadDpiAwarenessContext() };
        let v2 = DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2;
        assert_ne!(unsafe { AreDpiAwarenessContextsEqual(during, v2) }, 0);
    }
    let after = unsafe { GetThreadDpiAwarenessContext() };
    assert_ne!(unsafe { AreDpiAwarenessContextsEqual(before, after) }, 0);
}

#[test]
fn the_walk_over_the_runner_finds_no_arena() {
    let facts = read_facts(&WinReader);
    assert_eq!(facts.arena, Ok(Arena::default()));
}
