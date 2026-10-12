//! The Spout locks a thread can be handed ABANDONED, as pure functions
//! (#243).
//!
//! Spout 2.007.017 guards its maps and each sender's shared texture with
//! named mutexes. When a program dies holding one (a crash, or a
//! `Stop-Process` mid-lock), Windows hands the mutex to the next waiter
//! with `WAIT_ABANDONED`, and that thread then OWNS it. Spout's
//! `SpoutSharedMemory::Lock` and `spoutFrameCount::CheckAccess` read that
//! as a failed wait and never release it: the thread keeps the lock until
//! it exits, and every other program's wait for it times out (67 ms). On
//! PP Arena kept Spout's sender list that way for 8 hours; a receiver that
//! keeps a sender's texture lock freezes that sender's picture.
//!
//! So the sender gives back every Spout lock its thread holds after each
//! SDK call (`win/spout_lock.rs`): Spout's own locks are balanced within a
//! call, so between calls the thread holds one only when it was handed it
//! abandoned, or when a C++ exception the shim caught left a lock taken
//! (Spout's `SpoutSharedMemory` object then still counts it as locked and
//! reads its map unlocked afterwards: no worse than keeping the lock). A
//! sender Spout renamed (`<name>_1`, refused at once as `TAKEN`) locks
//! names this module does not list. A lock another program keeps cannot be
//! given back from here:
//! a refusal names its holder ([`holder_text`]), and a sender whose texture
//! lock another program keeps is refused
//! ([`crate::spout_state::access_held`]), so it is made anew once that
//! program's receiver has let go of it.
//!
//! #243 follow-up (PP, 12.10.2026): when SongPlayer itself dies holding
//! Spout's sender list (a deploy's `taskkill /F`), the next waiter is a
//! RECEIVER, Arena, which then keeps the list for good. That program's
//! holding is read ([`LockHolder`]), and a sentinel thread queued on the
//! list's lock is handed it when the holder exits, and gives it back at
//! once ([`sentinel_step`] decides each of its waits, [`SentinelEnd`] says
//! how it ended). sp-server decides when to start it.

use crate::spout::{SENDER_NAMES_MAP, map_mutex_name};

/// The map Spout writes the active sender's name to.
pub(crate) const ACTIVE_SENDER_MAP: &str = "ActiveSenderName";

/// The most releases one check makes. A mutex owned more than once by one
/// thread between SDK calls takes a bug; past this the next check goes on.
pub(crate) const MAX_RELEASES: u32 = 64;

/// How long one wait of the list sentinel lasts before it looks at its stop
/// flag again, ms: a stop ends it within one wait, and it never spins.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const SENTINEL_WAIT_MS: u32 = 500;

/// `WaitForSingleObject`'s answers the sentinel tells apart.
const WAIT_TOOK: u32 = 0x0;
const WAIT_TOOK_ABANDONED: u32 = 0x80;
const WAIT_TIMED_OUT: u32 = 0x102;

/// #243 follow-up: a program holding a Spout lock: its pid, and its
/// program's file name (`Arena.exe`), `None` when its image cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockHolder {
    pub pid: u32,
    pub image: Option<String>,
}

/// #243 follow-up: how the list sentinel ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SentinelEnd {
    /// It was handed the lock and gave it back at once. `abandoned`: its
    /// holder died holding it (else the holder let go of it).
    GivenBack { abandoned: bool },
    /// It was asked to stop while the lock was still kept.
    Stopped,
    /// It could not wait (the mutex did not open, or a wait failed).
    Failed(String),
}

/// What the sentinel does after one wait ([`sentinel_step`]).
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SentinelStep {
    /// It was handed the lock: give it back at once and end.
    GiveBack { abandoned: bool },
    /// The lock is still kept: wait again.
    WaitAgain,
    /// The lock is still kept and it was asked to stop: end.
    Stop,
    /// The wait failed: end.
    Fail,
}

/// The sentinel's step after a wait that answered `wait` (`stop`: it was
/// asked to stop): a lock it was handed (`WAIT_OBJECT_0`, or
/// `WAIT_ABANDONED`: its holder died) is given back whatever `stop` says;
/// a timeout waits again unless asked to stop; anything else failed.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn sentinel_step(wait: u32, stop: bool) -> SentinelStep {
    match wait {
        WAIT_TOOK => SentinelStep::GiveBack { abandoned: false },
        WAIT_TOOK_ABANDONED => SentinelStep::GiveBack { abandoned: true },
        WAIT_TIMED_OUT if stop => SentinelStep::Stop,
        WAIT_TIMED_OUT => SentinelStep::WaitAgain,
        _ => SentinelStep::Fail,
    }
}

/// The named mutex guarding the sender `sender`'s shared texture
/// (`spoutFrameCount::CreateAccessMutex`).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn access_mutex_name(sender: &str) -> String {
    format!("{sender}_SpoutAccessMutex")
}

/// The Spout locks a call of the sender `sender` can take: the sender
/// list's, the active sender's, its own info map's and its texture's.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn sender_lock_names(sender: &str) -> [String; 4] {
    [
        map_mutex_name(SENDER_NAMES_MAP),
        map_mutex_name(ACTIVE_SENDER_MAP),
        map_mutex_name(sender),
        access_mutex_name(sender),
    ]
}

/// How many times the calling thread must release a mutex that
/// `NtQueryMutant` reports with `count` (1 = free, 0 = owned once, −n =
/// owned n + 1 times) and `owned_by_caller`: none unless the caller owns
/// it, at most [`MAX_RELEASES`].
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn releases_owed(count: i32, owned_by_caller: bool) -> u32 {
    if !owned_by_caller {
        return 0;
    }
    let held = 1 - i64::from(count);
    held.clamp(0, i64::from(MAX_RELEASES)) as u32
}

/// The program's file name in an image path (`C:\…\Arena.exe` →
/// `Arena.exe`); `None` for no path, or one that ends in a separator.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn program_name(image: Option<&str>) -> Option<&str> {
    image
        .and_then(|path| path.rsplit(['\\', '/']).next())
        .filter(|name| !name.is_empty())
}

/// Who holds a lock, as a refusal names it: the program's file name and
/// pid (`Arena.exe (pid 1500)`), the pid alone when its image cannot be
/// read, and ", this process" when the holder is this process.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn holder_text(pid: u32, image: Option<&str>, own_pid: u32) -> String {
    let program = program_name(image);
    let own = if pid == own_pid { ", this process" } else { "" };
    match program {
        Some(name) => format!("{name} (pid {pid}{own})"),
        None => format!("pid {pid}{own}"),
    }
}

#[cfg(test)]
#[path = "spout_lock_tests.rs"]
mod tests;
