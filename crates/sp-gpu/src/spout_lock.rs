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

use crate::spout::{SENDER_NAMES_MAP, map_mutex_name};

/// The map Spout writes the active sender's name to.
pub(crate) const ACTIVE_SENDER_MAP: &str = "ActiveSenderName";

/// The most releases one check makes. A mutex owned more than once by one
/// thread between SDK calls takes a bug; past this the next check goes on.
pub(crate) const MAX_RELEASES: u32 = 64;

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

/// Who holds a lock, as a refusal names it: the program's file name and
/// pid (`Arena.exe (pid 1500)`), the pid alone when its image cannot be
/// read, and ", this process" when the holder is this process.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn holder_text(pid: u32, image: Option<&str>, own_pid: u32) -> String {
    let program = image
        .and_then(|path| path.rsplit(['\\', '/']).next())
        .filter(|name| !name.is_empty());
    let own = if pid == own_pid { ", this process" } else { "" };
    match program {
        Some(name) => format!("{name} (pid {pid}{own})"),
        None => format!("pid {pid}{own}"),
    }
}

#[cfg(test)]
#[path = "spout_lock_tests.rs"]
mod tests;
