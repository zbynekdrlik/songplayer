//! #243 follow-up, WARP-free: who holds Spout's sender list
//! (`sp_gpu::spout_list_holder`, `mutex_holder`), and the sentinel that is
//! handed the list's lock when its holder exits and gives it back at once
//! (`sp_gpu::ListSentinel`). PP, 12.10.2026: a deploy's `taskkill /F` left
//! Spout's list to Arena, which never gave it back (Spout 2.007.017 keeps a
//! lock it was handed abandoned).
//!
//! The sentinel tests wait on their own named mutexes; only the holder
//! test takes Spout's own list lock, under the one-at-a-time lock (the
//! list is machine-wide). Windows only; nothing is skipped.

#![cfg(windows)]

mod common;

use std::sync::{Mutex, MutexGuard};

use common::locks::{Doomed, Hold, TestMutex, end_of, took, wait_elsewhere};
use sp_gpu::{ListSentinel, SENDER_NAMES_MAP, SentinelEnd, mutex_holder, spout_list_holder};
use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{ReleaseMutex, WaitForSingleObject};

/// One test of Spout's own list lock at a time.
static SPOUT: Mutex<()> = Mutex::new(());

fn one_at_a_time() -> MutexGuard<'static, ()> {
    SPOUT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// #243 follow-up: the program that keeps Spout's sender list dies holding
/// it (Arena killed, PP 12.10.2026): the sentinel queued on it is handed
/// it ABANDONED and gives it back at once, so the next waiter takes it
/// normally (0x0), never abandoned (0x80) or kept (0x102). Its own mutex:
/// Spout's list is machine-wide. `_alive` keeps the mutex object, so a
/// state the test reads is never a fresh mutex's.
#[test]
fn the_list_sentinel_is_handed_a_dead_holder_s_lock_and_gives_it_back() {
    let name = "SP-gpu-test-sentinel-abandoned_mutex";
    let _alive = TestMutex::open(name);
    let holder = Doomed::take(name);
    let mut sentinel = ListSentinel::on(name).expect("the sentinel starts");
    assert_eq!(
        sentinel.ended(),
        None,
        "it waits while the holder keeps the lock"
    );
    holder.die();
    assert_eq!(
        end_of(&sentinel),
        Some(SentinelEnd::GivenBack { abandoned: true })
    );
    assert_eq!(
        wait_elsewhere(name),
        WAIT_OBJECT_0.0,
        "free again (0x80 = the sentinel ended still holding it, 0x102 = it kept it)"
    );
    sentinel.stop();
}

/// #243 follow-up: the sentinel never keeps what it was not handed: asked
/// to stop while the lock is kept, it ends within one wait and the holder
/// still has the lock; a holder that lets go hands it over, and it is given
/// back at once.
#[test]
fn the_list_sentinel_stops_when_asked_and_gives_back_a_released_lock() {
    let name = "SP-gpu-test-sentinel-released_mutex";
    let _alive = TestMutex::open(name);
    let hold = Hold::take(name);
    let mut sentinel = ListSentinel::on(name).expect("the sentinel starts");
    sentinel.stop();
    assert_eq!(sentinel.ended(), Some(SentinelEnd::Stopped));
    assert_eq!(
        wait_elsewhere(name),
        WAIT_TIMEOUT.0,
        "the holder still keeps it"
    );
    let sentinel = ListSentinel::on(name).expect("a second sentinel starts");
    drop(hold);
    assert_eq!(
        end_of(&sentinel),
        Some(SentinelEnd::GivenBack { abandoned: false })
    );
    assert_eq!(wait_elsewhere(name), WAIT_OBJECT_0.0, "given back");
}

/// #243 follow-up: a kept lock names its holder by pid and program (here
/// the test exe, another thread of this process); a free lock, one the
/// calling thread holds, or no such mutex names none. `spout_list_holder`
/// reads Spout's own list lock.
#[test]
fn the_holder_of_a_kept_lock_is_named_by_pid_and_program() {
    let _one = one_at_a_time();
    let exe = std::env::current_exe().expect("the test's exe");
    let program = exe
        .file_name()
        .expect("its file name")
        .to_string_lossy()
        .to_ascii_lowercase();
    let lowered = |image: Option<String>| image.map(|image| image.to_ascii_lowercase());

    let name = "SP-gpu-test-holder_mutex";
    let mine = TestMutex::open(name);
    assert_eq!(mutex_holder(name), None, "free");
    let hold = Hold::take(name);
    let holder = mutex_holder(name).expect("another thread holds it");
    drop(hold);
    assert_eq!(holder.pid, std::process::id());
    assert_eq!(lowered(holder.image), Some(program.clone()));
    assert_eq!(mutex_holder(name), None, "given back");
    assert!(took(unsafe { WaitForSingleObject(mine.0, 1_000) }.0));
    let own = mutex_holder(name);
    unsafe { ReleaseMutex(mine.0) }.expect("give it back");
    assert_eq!(own, None, "the calling thread's own lock is no holder");
    assert_eq!(mutex_holder("SP-gpu-test-no-such-mutex"), None);

    let list = format!("{SENDER_NAMES_MAP}_mutex");
    let hold = Hold::take(&list);
    let holder = spout_list_holder();
    drop(hold);
    let holder = holder.expect("readable").expect("the list is held");
    assert_eq!(holder.pid, std::process::id());
    assert_eq!(lowered(holder.image), Some(program));
}
