//! #259: the wall's Windows glue on the CI runner (Build (Windows)): the
//! process list holds this very test process by its image's file name, the
//! window list reads, the display and window reads run per-monitor DPI
//! aware and give the thread back as it was, and the walk over the real
//! reads finds no Arena (the runner runs none). Read only: nothing on the
//! runner is changed.
//!
//! #254: the guard's reads and its end, on a child process of this test (a
//! copy of this test binary that waits, run as `Arena.exe` or under another
//! name): its parent, start and CPU time, no hung window, and the end — the
//! child is ended only when its image and start time are the ones ordered.
//! The tests that start such a child and the one that expects no Arena hold
//! one lock, so no Arena child runs while the walk looks for none.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use sp_core::wall::GuardWhy;

use super::*;
use crate::wall::guard::{EndOrder, ProcessKey};
use crate::wall::guard_win;
use crate::wall::read::{Arena, read_facts};

/// Held by every test that starts an `Arena.exe` child, and by the one that
/// expects no Arena.
static ARENA_CHILDREN: Mutex<()> = Mutex::new(());

/// Numbers each child's directory (a removed one may still be held).
static NEXT_CHILD: AtomicU32 = AtomicU32::new(0);

/// Set in the child's environment: the waiting test waits only then.
const CHILD_ENV: &str = "SP_WALL_GUARD_CHILD";

/// The waiting test's full name, for the child's harness.
const WAITER: &str = "wall::win::tests::the_child_waits_to_be_ended";

/// The child process's body (not a check of its own): a copy of this test
/// binary run with only this test and [`CHILD_ENV`] set waits here until
/// the test that started it ends it. Run by the harness itself it returns
/// at once.
#[test]
fn the_child_waits_to_be_ended() {
    if std::env::var_os(CHILD_ENV).is_some() {
        std::thread::sleep(Duration::from_secs(120));
    }
}

/// How long a child may take to start its harness.
const CHILD_START: Duration = Duration::from_secs(30);

/// A child of this test: a copy of this test binary named `image` in a
/// directory of its own, waiting; killed and removed when dropped. Its
/// output is kept open (`out`) so its harness never meets a closed pipe.
struct Waiting {
    child: Child,
    dir: PathBuf,
    #[allow(dead_code)] // held, never read: its pipe stays open
    out: BufReader<ChildStdout>,
}

impl Waiting {
    fn start(image: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "sp-wall-guard-{}-{}",
            std::process::id(),
            NEXT_CHILD.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(image);
        let me = std::env::current_exe().unwrap();
        std::fs::copy(&me, &exe).unwrap();
        // The test binary starts only with its side-by-side manifest
        // (Common-Controls 6.0 for Spout's TaskDialogIndirect, gpu-max.md):
        // a copy without `<exe>.manifest` is refused at load and ends at once.
        let manifest = me.with_file_name(format!(
            "{}.manifest",
            me.file_name().unwrap().to_string_lossy()
        ));
        assert!(manifest.exists(), "{} has no manifest", me.display());
        std::fs::copy(&manifest, dir.join(format!("{image}.manifest"))).unwrap();
        let mut child = Command::new(&exe)
            .args(["--exact", WAITER, "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // CI 38626241313 / 38628473272: a child without its manifest died
        // while loading (QueryFullProcessImageNameW error 31 on it). Its
        // harness prints "running 1 test" once it runs: wait for that.
        let out = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut out = out;
            let mut line = String::new();
            let started = loop {
                line.clear();
                match out.read_line(&mut line) {
                    Ok(0) | Err(_) => break false,
                    Ok(_) if line.contains("running 1 test") => break true,
                    Ok(_) => {}
                }
            };
            let _ = tx.send((started, out));
        });
        let (started, out) = rx
            .recv_timeout(CHILD_START)
            .unwrap_or_else(|_| panic!("{image} did not start its harness"));
        assert!(started, "{image} ended before its harness ran");
        Self { child, dir, out }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// It still runs.
    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The order to end `child` as an Arena started at `created_100ns`.
fn order(child: &Waiting, created_100ns: u64) -> EndOrder {
    EndOrder {
        pid: child.pid(),
        created_100ns,
        why: GuardWhy::Hung,
        monitors: Vec::new(),
    }
}

#[test]
fn the_process_list_holds_this_test_process_by_its_file_name() {
    let me = std::process::id();
    let exe = std::env::current_exe().unwrap();
    let name = exe.file_name().unwrap().to_string_lossy().into_owned();
    let processes = WinReader.processes().unwrap();
    assert!(
        processes
            .iter()
            .any(|p| p.pid == me && p.image.eq_ignore_ascii_case(&name)),
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
    let _alone = ARENA_CHILDREN.lock().unwrap_or_else(|e| e.into_inner());
    let facts = read_facts(&WinReader);
    assert_eq!(facts.arena, Ok(Arena::default()));
    assert_eq!(facts.guard, Ok(Vec::new()));
}

#[test]
fn the_guard_reads_an_arenas_parent_times_and_windows() {
    let _alone = ARENA_CHILDREN.lock().unwrap_or_else(|e| e.into_inner());
    let child = Waiting::start("Arena.exe");
    let me = std::process::id();
    let listed = WinReader.processes().unwrap();
    let entry = listed.iter().find(|p| p.pid == child.pid()).unwrap();
    assert_eq!((entry.parent, entry.image.as_str()), (me, "Arena.exe"));
    let times = WinReader.times(child.pid()).unwrap();
    assert!(times.created_100ns > 0, "{times:?}");
    // A console process has no window to hang.
    assert_eq!(WinReader.hung(child.pid()), Ok(false));
    let facts = read_facts(&WinReader);
    let arena = facts
        .guard
        .unwrap()
        .into_iter()
        .find(|a| a.pid == child.pid())
        .unwrap();
    assert_eq!(
        arena.times.map(|t| t.created_100ns),
        Ok(times.created_100ns)
    );
    assert_eq!((arena.hung, arena.crash_report), (Ok(false), Ok(false)));
    assert!(arena.monitors.is_empty());
}

#[test]
fn the_guard_ends_an_arena_it_was_ordered_to_end() {
    let _alone = ARENA_CHILDREN.lock().unwrap_or_else(|e| e.into_inner());
    let mut child = Waiting::start("Arena.exe");
    let created = WinReader.times(child.pid()).unwrap().created_100ns;
    assert!(child.running());
    assert_eq!(guard_win::end(&order(&child, created)), Ok(()));
    let status = child.child.wait().unwrap();
    assert_eq!(status.code(), Some(1));
}

#[test]
fn the_guard_leaves_another_program_or_a_later_arena_running() {
    let _alone = ARENA_CHILDREN.lock().unwrap_or_else(|e| e.into_inner());
    let mut bridge = Waiting::start("Arena-Bridge.exe");
    let created = WinReader.times(bridge.pid()).unwrap().created_100ns;
    let refused = guard_win::end(&order(&bridge, created)).unwrap_err();
    assert!(refused.contains("not Arena.exe: left running"), "{refused}");
    assert!(bridge.running());
    let mut later = Waiting::start("Arena.exe");
    let created = WinReader.times(later.pid()).unwrap().created_100ns;
    let refused = guard_win::end(&order(&later, created + 1)).unwrap_err();
    assert!(refused.contains("started since the read"), "{refused}");
    assert!(later.running());
}

#[test]
fn the_guard_ends_arenas_reporter_first_only_when_it_is_the_one_read() {
    let _alone = ARENA_CHILDREN.lock().unwrap_or_else(|e| e.into_inner());
    // A reporter started since the read: left running, Arena still ended.
    let mut reporter = Waiting::start("BugSplatMonitor.exe");
    let mut arena = Waiting::start("Arena.exe");
    let reporter_created = WinReader.times(reporter.pid()).unwrap().created_100ns;
    let arena_created = WinReader.times(arena.pid()).unwrap().created_100ns;
    let later = EndOrder {
        monitors: vec![ProcessKey {
            pid: reporter.pid(),
            created_100ns: reporter_created + 1,
        }],
        ..order(&arena, arena_created)
    };
    let refused = guard_win::end(&later).unwrap_err();
    assert!(
        refused.contains("another BugSplatMonitor.exe (started since the read)"),
        "{refused}"
    );
    assert!(reporter.running());
    assert_eq!(arena.child.wait().unwrap().code(), Some(1));
    // The reporter read: ended first, then Arena.
    let mut reporter = Waiting::start("BugSplatMonitor.exe");
    let mut arena = Waiting::start("Arena.exe");
    let read = EndOrder {
        monitors: vec![ProcessKey {
            pid: reporter.pid(),
            created_100ns: WinReader.times(reporter.pid()).unwrap().created_100ns,
        }],
        ..order(&arena, WinReader.times(arena.pid()).unwrap().created_100ns)
    };
    assert_eq!(guard_win::end(&read), Ok(()));
    assert_eq!(reporter.child.wait().unwrap().code(), Some(1));
    assert_eq!(arena.child.wait().unwrap().code(), Some(1));
}
