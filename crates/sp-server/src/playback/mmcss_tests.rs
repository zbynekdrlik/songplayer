//! #210 part 2: the MMCSS join's pure decisions (the outcome of the two
//! calls, the TIME_CRITICAL fallback, the error names, the wide task name)
//! and `join` itself per platform: the Linux stub, and on the Windows job the
//! real avrt call for a task that does not exist.
//! Wired via `#[cfg(test)] #[path = "mmcss_tests.rs"] mod tests;`.

use super::*;

#[test]
fn the_outcome_is_read_off_the_two_calls() {
    assert_eq!(
        MmcssOutcome::of(true, true, 7, 0),
        MmcssOutcome::High { task_index: 7 }
    );
    assert_eq!(
        MmcssOutcome::of(true, false, 7, 87),
        MmcssOutcome::TaskOnly {
            task_index: 7,
            error: 87,
        }
    );
    assert_eq!(
        MmcssOutcome::of(false, false, 0, 1550),
        MmcssOutcome::Refused { error: 1550 }
    );
    assert_eq!(
        MmcssOutcome::of(false, true, 3, 1314),
        MmcssOutcome::Refused { error: 1314 },
        "no handle: nothing joined, whatever the priority flag"
    );
}

#[test]
fn only_a_refused_join_falls_back_to_time_critical() {
    assert!(MmcssOutcome::Refused { error: 1550 }.needs_fallback());
    assert!(MmcssOutcome::Refused { error: 0 }.needs_fallback());
    assert!(!MmcssOutcome::High { task_index: 1 }.needs_fallback());
    assert!(
        !MmcssOutcome::TaskOnly {
            task_index: 1,
            error: 5,
        }
        .needs_fallback(),
        "in the task: MMCSS's real-time band, no fallback"
    );
}

#[test]
fn the_documented_errors_are_named_and_the_rest_is_other() {
    assert_eq!(avrt_error_name(1550), "ERROR_INVALID_TASK_NAME");
    assert_eq!(avrt_error_name(1551), "ERROR_INVALID_TASK_INDEX");
    assert_eq!(avrt_error_name(1314), "ERROR_PRIVILEGE_NOT_HELD");
    assert_eq!(avrt_error_name(0), "other");
    assert_eq!(avrt_error_name(5), "other");
}

#[test]
fn the_task_name_goes_over_as_nul_terminated_utf16() {
    assert_eq!(MMCSS_PRO_AUDIO, "Pro Audio");
    assert_eq!(
        wide_nul(MMCSS_PRO_AUDIO),
        vec![0x50, 0x72, 0x6f, 0x20, 0x41, 0x75, 0x64, 0x69, 0x6f, 0]
    );
    assert_eq!(wide_nul(""), vec![0]);
    assert_eq!(wide_nul("é"), vec![0xe9, 0]);
}

#[cfg(not(windows))]
#[test]
fn off_windows_the_join_is_always_refused() {
    let task = MmcssTask::join(MMCSS_PRO_AUDIO);
    assert_eq!(task.outcome(), MmcssOutcome::Refused { error: 0 });
    assert!(task.outcome().needs_fallback());
}

#[cfg(windows)]
#[test]
fn an_unknown_task_is_refused_with_its_windows_error() {
    // The real avrt call: a task name that is no subkey of the MMCSS task
    // list never joins, so the guard has nothing to revert.
    let task = MmcssTask::join("SongPlayer no such task");
    assert!(
        matches!(task.outcome(), MmcssOutcome::Refused { error } if error != 0),
        "{:?}",
        task.outcome()
    );
    assert!(task.outcome().needs_fallback());
}

#[cfg(windows)]
#[test]
fn the_named_codes_are_the_windows_ones() {
    use windows_sys::Win32::Foundation::{
        ERROR_INVALID_TASK_INDEX, ERROR_INVALID_TASK_NAME, ERROR_PRIVILEGE_NOT_HELD,
    };
    assert_eq!(
        avrt_error_name(ERROR_INVALID_TASK_NAME),
        "ERROR_INVALID_TASK_NAME"
    );
    assert_eq!(
        avrt_error_name(ERROR_INVALID_TASK_INDEX),
        "ERROR_INVALID_TASK_INDEX"
    );
    assert_eq!(
        avrt_error_name(ERROR_PRIVILEGE_NOT_HELD),
        "ERROR_PRIVILEGE_NOT_HELD"
    );
}
