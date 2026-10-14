//! #254: the guard's rule, every branch — the crash reporter's window, a
//! hang with the CPU time still for 30 s, a hang while the CPU time moves,
//! an Arena that answers, reads that fail, once per process, a new process
//! guarded again, and the wait for the relaunch (`not_back` after 60 s with
//! one WARN, `idle` once a new Arena runs).

use sp_core::wall::{GuardEnd, GuardPhase, GuardWhy, WallGuard};

use super::*;

/// The first check's clock (UTC unix ms).
const T0: i64 = 1_791_608_681_366;

/// Arena's start time (its identity next to its id).
const CREATED: u64 = 134_049_123_000_000_000;

/// The crash reporter's process id.
const MONITOR: u32 = 9188;

/// No event.
const NONE: [GuardEvent; 0] = [];

/// The crash reporter, by id and start time (started after Arena).
fn monitor() -> ProcessKey {
    ProcessKey {
        pid: MONITOR,
        created_100ns: CREATED + 1_000,
    }
}

/// An Arena that answers, `cpu` 100 ns of CPU time used.
fn answers(pid: u32, cpu: u64) -> ArenaProcess {
    ArenaProcess {
        pid,
        times: Ok(ProcessTimes {
            created_100ns: CREATED,
            cpu_100ns: cpu,
        }),
        hung: Ok(false),
        monitors: Vec::new(),
        crash_report: Ok(false),
    }
}

/// An Arena whose visible window does not answer.
fn hung(pid: u32, cpu: u64) -> ArenaProcess {
    ArenaProcess {
        hung: Ok(true),
        ..answers(pid, cpu)
    }
}

/// An Arena whose crash reporter shows its "Crash Report" window.
fn crashed(pid: u32, cpu: u64) -> ArenaProcess {
    ArenaProcess {
        monitors: vec![monitor()],
        crash_report: Ok(true),
        ..answers(pid, cpu)
    }
}

/// The end `decide` orders for `arenas` at `T0 + at`.
fn order(guard: &mut Guard, arenas: Vec<ArenaProcess>, at: i64) -> Option<EndOrder> {
    decide(guard, &Ok(arenas), T0 + at).end
}

/// The order to end Arena `pid` for `why`.
fn ended(pid: u32, why: GuardWhy, monitors: Vec<ProcessKey>) -> Option<EndOrder> {
    Some(EndOrder {
        pid,
        created_100ns: CREATED,
        why,
        monitors,
    })
}

/// An end the glue ran with no failure.
fn ok(_: &EndOrder) -> Result<(), String> {
    Ok(())
}

#[test]
fn a_filetime_reads_as_one_count_of_100_ns() {
    assert_eq!(filetime_100ns(5, 1), 4_294_967_301);
    assert_eq!(filetime_100ns(0xFFFF_FFFF, 0), 4_294_967_295);
    assert_eq!(
        ProcessTimes::from_filetimes((5, 1), (7, 2), (11, 3)),
        ProcessTimes {
            created_100ns: 4_294_967_301,
            // kernel (2 << 32) + 7 and user (3 << 32) + 11
            cpu_100ns: (5_u64 << 32) + 18,
        }
    );
}

#[test]
fn an_image_is_its_file_name_in_any_case() {
    assert!(is_image(
        r"C:\Program Files\Resolume Arena\Arena.exe",
        "Arena.exe"
    ));
    assert!(is_image("arena.EXE", "Arena.exe"));
    assert!(is_image(
        "C:/Program Files/Resolume Arena/BugSplatMonitor.exe",
        "BugSplatMonitor.exe"
    ));
    assert!(!is_image(
        r"C:\Program Files\Resolume Arena\Arena-Bridge.exe",
        "Arena.exe"
    ));
    assert!(!is_image(r"C:\Arena.exe\Other.exe", "Arena.exe"));
    assert!(!is_image("", "Arena.exe"));
}

#[test]
fn a_crash_report_window_ends_its_arena_at_once() {
    let mut guard = Guard::default();
    let step = decide(&mut guard, &Ok(vec![crashed(5120, 100)]), T0);
    assert_eq!(
        step,
        Step {
            events: Vec::new(),
            end: ended(5120, GuardWhy::CrashReport, vec![monitor()]),
        }
    );
}

#[test]
fn a_hang_with_the_cpu_time_still_for_30_s_ends_arena() {
    let mut guard = Guard::default();
    for at in [0, 5_000, 25_000, 29_999] {
        assert_eq!(order(&mut guard, vec![hung(5120, 777)], at), None, "{at}");
    }
    assert_eq!(
        order(&mut guard, vec![hung(5120, 777)], 30_000),
        ended(5120, GuardWhy::Hung, Vec::new())
    );
}

#[test]
fn a_hang_while_the_cpu_time_moves_is_never_ended() {
    let mut guard = Guard::default();
    // Two minutes of a composition load: no answer, the CPU time moving.
    for i in 0..=24_u64 {
        let at = i as i64 * 5_000;
        assert_eq!(
            order(&mut guard, vec![hung(5120, 1_000 + i)], at),
            None,
            "{at}"
        );
    }
    // It stops moving at 120 s: ended 30 s after that, not before.
    for at in [125_000, 149_999] {
        assert_eq!(order(&mut guard, vec![hung(5120, 1_024)], at), None, "{at}");
    }
    assert_eq!(
        order(&mut guard, vec![hung(5120, 1_024)], 150_000),
        ended(5120, GuardWhy::Hung, Vec::new())
    );
}

#[test]
fn an_arena_that_answers_is_never_ended_and_a_hang_is_watched_from_its_start() {
    let mut guard = Guard::default();
    // It answers, its CPU time still: never ended.
    for i in 0..=12 {
        assert_eq!(order(&mut guard, vec![answers(5120, 777)], i * 5_000), None);
    }
    // It stops answering at 60 s: ended 30 s later, not at once.
    for at in [60_000, 89_999] {
        assert_eq!(order(&mut guard, vec![hung(5120, 777)], at), None, "{at}");
    }
    assert_eq!(
        order(&mut guard, vec![hung(5120, 777)], 90_000),
        ended(5120, GuardWhy::Hung, Vec::new())
    );
}

#[test]
fn an_answer_in_between_starts_the_watch_again() {
    let mut guard = Guard::default();
    assert_eq!(order(&mut guard, vec![hung(5120, 777)], 0), None);
    assert_eq!(order(&mut guard, vec![answers(5120, 777)], 10_000), None);
    for at in [20_000, 35_000, 49_999] {
        assert_eq!(order(&mut guard, vec![hung(5120, 777)], at), None, "{at}");
    }
    assert_eq!(
        order(&mut guard, vec![hung(5120, 777)], 50_000),
        ended(5120, GuardWhy::Hung, Vec::new())
    );
}

#[test]
fn a_read_that_fails_never_ends_arena() {
    let unread_hang = ArenaProcess {
        hung: Err("EnumWindows failed (error 5)".to_string()),
        ..answers(5120, 777)
    };
    let unread_times = ArenaProcess {
        times: Err("OpenProcess(5120) failed (error 5)".to_string()),
        ..hung(5120, 777)
    };
    // Its identity unknown: not even its crash reporter's window acts.
    let unknown_crashed = ArenaProcess {
        times: Err("OpenProcess(5120) failed (error 5)".to_string()),
        ..crashed(5120, 777)
    };
    let unread_report = ArenaProcess {
        crash_report: Err("EnumWindows failed (error 5)".to_string()),
        ..answers(5120, 777)
    };
    for arena in [unread_hang, unread_times, unknown_crashed, unread_report] {
        let mut guard = Guard::default();
        for i in 0..=13 {
            assert_eq!(
                order(&mut guard, vec![arena.clone()], i * 5_000),
                None,
                "{arena:?}"
            );
        }
    }
}

#[test]
fn a_check_that_read_nothing_ends_nothing_and_starts_the_watch_again() {
    let mut guard = Guard::default();
    assert_eq!(order(&mut guard, vec![hung(5120, 777)], 0), None);
    let unread: GuardFacts = Err("the wall read did not finish within 10s".to_string());
    assert_eq!(decide(&mut guard, &unread, T0 + 5_000), Step::default());
    for at in [10_000, 30_000, 39_999] {
        assert_eq!(order(&mut guard, vec![hung(5120, 777)], at), None, "{at}");
    }
    assert_eq!(
        order(&mut guard, vec![hung(5120, 777)], 40_000),
        ended(5120, GuardWhy::Hung, Vec::new())
    );
}

#[test]
fn an_arena_is_ended_once_per_process() {
    let mut guard = Guard::default();
    assert!(order(&mut guard, vec![crashed(5120, 100)], 0).is_some());
    // Still listed (its end takes a moment, or failed): never ordered again,
    // its window up and its hang frozen as long as it lasts.
    let both = ArenaProcess {
        hung: Ok(true),
        ..crashed(5120, 100)
    };
    for at in [5_000, 40_000, 120_000] {
        assert_eq!(order(&mut guard, vec![both.clone()], at), None, "{at}");
    }
}

#[test]
fn a_new_arena_process_is_guarded_again() {
    let mut guard = Guard::default();
    assert!(order(&mut guard, vec![crashed(5120, 100)], 0).is_some());
    assert_eq!(order(&mut guard, Vec::new(), 5_000), None);
    // The same process id, started anew (an id is reused).
    let reused = ArenaProcess {
        times: Ok(ProcessTimes {
            created_100ns: CREATED + 10,
            cpu_100ns: 3,
        }),
        ..crashed(5120, 3)
    };
    assert_eq!(
        order(&mut guard, vec![reused], 10_000),
        Some(EndOrder {
            pid: 5120,
            created_100ns: CREATED + 10,
            why: GuardWhy::CrashReport,
            monitors: vec![monitor()],
        })
    );
    // Another process id.
    assert_eq!(
        order(&mut guard, vec![crashed(6000, 3)], 15_000),
        ended(6000, GuardWhy::CrashReport, vec![monitor()])
    );
}

#[test]
fn after_an_end_the_guard_waits_for_a_new_arena_and_warns_once_after_60_s() {
    let mut guard = Guard::default();
    let mut runs = 0;
    let mut end = |_: &EndOrder| -> Result<(), String> {
        runs += 1;
        Ok(())
    };
    let last = GuardEnd {
        at_ms: T0,
        why: GuardWhy::CrashReport,
        pid: 5120,
        monitors: vec![MONITOR],
        error: None,
    };
    let events = step(&mut guard, &Ok(vec![crashed(5120, 1)]), T0, &mut end);
    assert_eq!(events, [GuardEvent::Ended(last.clone())]);
    assert_eq!(
        guard.view(),
        WallGuard {
            ends: 1,
            last: Some(last.clone()),
            phase: GuardPhase::Relaunching,
        }
    );
    // Gone, and waited for.
    for at in [5_000, 30_000, 59_999] {
        assert_eq!(step(&mut guard, &Ok(Vec::new()), T0 + at, &mut end), NONE);
        assert_eq!(guard.view().phase, GuardPhase::Relaunching, "{at}");
    }
    assert_eq!(
        step(&mut guard, &Ok(Vec::new()), T0 + 60_000, &mut end),
        [GuardEvent::NotBack(last.clone())]
    );
    assert_eq!(guard.view().phase, GuardPhase::NotBack);
    assert_eq!(
        step(&mut guard, &Ok(Vec::new()), T0 + 65_000, &mut end),
        NONE
    );
    // A check that read nothing changes nothing.
    let unread: GuardFacts = Err("the wall read did not finish within 10s".to_string());
    assert_eq!(step(&mut guard, &unread, T0 + 70_000, &mut end), NONE);
    assert_eq!(guard.view().phase, GuardPhase::NotBack);
    // A new Arena runs: the end stays on record, nothing pending, nothing
    // more to say.
    let relaunched = Ok(vec![answers(5124, 3)]);
    assert_eq!(
        step(&mut guard, &relaunched, T0 + 90_000, &mut end),
        [GuardEvent::Back { pid: 5124 }]
    );
    assert_eq!(
        guard.view(),
        WallGuard {
            ends: 1,
            last: Some(last),
            phase: GuardPhase::Idle,
        }
    );
    assert_eq!(step(&mut guard, &relaunched, T0 + 95_000, &mut end), NONE);
    assert_eq!(runs, 1);
}

#[test]
fn an_arena_back_within_60_s_is_never_not_back() {
    let mut guard = Guard::default();
    step(&mut guard, &Ok(vec![crashed(5120, 1)]), T0, &mut ok);
    let relaunched = Ok(vec![answers(5124, 3)]);
    assert_eq!(
        step(&mut guard, &relaunched, T0 + 3_000, &mut ok),
        [GuardEvent::Back { pid: 5124 }]
    );
    for at in [60_000, 120_000] {
        assert_eq!(step(&mut guard, &relaunched, T0 + at, &mut ok), NONE);
        assert_eq!(guard.view().phase, GuardPhase::Idle);
    }
}

#[test]
fn the_ended_process_still_listed_is_no_new_arena() {
    let mut guard = Guard::default();
    step(&mut guard, &Ok(vec![crashed(5120, 1)]), T0, &mut ok);
    let lingering = Ok(vec![crashed(5120, 1)]);
    let unreadable = Ok(vec![ArenaProcess {
        times: Err("OpenProcess(5120) failed (error 5)".to_string()),
        ..crashed(5120, 1)
    }]);
    for (at, facts) in [
        (5_000, &lingering),
        (30_000, &unreadable),
        (59_999, &lingering),
    ] {
        assert_eq!(step(&mut guard, facts, T0 + at, &mut ok), NONE, "{at}");
        assert_eq!(guard.view().phase, GuardPhase::Relaunching, "{at}");
    }
    let events = step(&mut guard, &lingering, T0 + 60_000, &mut ok);
    assert!(
        matches!(events.as_slice(), [GuardEvent::NotBack(_)]),
        "{events:?}"
    );
}

#[test]
fn a_failed_end_is_recorded_with_why_and_never_ordered_again() {
    let mut guard = Guard::default();
    let why = "TerminateProcess(5120, Arena.exe) failed (error 5)".to_string();
    let mut refused = |_: &EndOrder| -> Result<(), String> { Err(why.clone()) };
    let events = step(&mut guard, &Ok(vec![hung(5120, 1)]), T0, &mut refused);
    assert_eq!(events, NONE);
    let events = step(
        &mut guard,
        &Ok(vec![hung(5120, 1)]),
        T0 + 30_000,
        &mut refused,
    );
    let last = GuardEnd {
        at_ms: T0 + 30_000,
        why: GuardWhy::Hung,
        pid: 5120,
        monitors: Vec::new(),
        error: Some(why.clone()),
    };
    assert_eq!(events, [GuardEvent::Ended(last.clone())]);
    assert_eq!(
        guard.view(),
        WallGuard {
            ends: 1,
            last: Some(last),
            phase: GuardPhase::Relaunching,
        }
    );
    assert_eq!(
        step(
            &mut guard,
            &Ok(vec![hung(5120, 1)]),
            T0 + 65_000,
            &mut refused
        ),
        NONE
    );
}

#[test]
fn a_new_arena_that_crashed_too_is_back_then_ended_again() {
    let mut guard = Guard::default();
    step(&mut guard, &Ok(vec![crashed(5120, 1)]), T0, &mut ok);
    step(&mut guard, &Ok(Vec::new()), T0 + 5_000, &mut ok);
    let events = step(
        &mut guard,
        &Ok(vec![crashed(5124, 1)]),
        T0 + 10_000,
        &mut ok,
    );
    let second = GuardEnd {
        at_ms: T0 + 10_000,
        why: GuardWhy::CrashReport,
        pid: 5124,
        monitors: vec![MONITOR],
        error: None,
    };
    assert_eq!(
        events,
        [
            GuardEvent::Back { pid: 5124 },
            GuardEvent::Ended(second.clone())
        ]
    );
    assert_eq!(
        guard.view(),
        WallGuard {
            ends: 2,
            last: Some(second),
            phase: GuardPhase::Relaunching,
        }
    );
}

#[test]
fn a_relaunched_arena_whose_start_cannot_be_read_is_back() {
    let mut guard = Guard::default();
    step(&mut guard, &Ok(vec![crashed(5120, 1)]), T0, &mut ok);
    let unread = ArenaProcess {
        times: Err("OpenProcess(5124) failed (error 5)".to_string()),
        ..answers(5124, 3)
    };
    assert_eq!(
        step(&mut guard, &Ok(vec![unread]), T0 + 5_000, &mut ok),
        [GuardEvent::Back { pid: 5124 }]
    );
    assert_eq!(guard.view().phase, GuardPhase::Idle);
}
