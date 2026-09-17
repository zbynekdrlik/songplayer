//! Live "which song is being separated right now" signal (#177).
//!
//! This is the honest source of the karaoke panel's ⚙ "spracúvam" state. There
//! is deliberately NO DB `'processing'` status — a crash mid-separation would
//! strand a persisted one, and the selector would have to special-case it — so
//! the in-flight video id lives ONLY in memory, in a process-global atomic the
//! stem worker sets while its separation child runs and clears when it ends.
//! The karaoke API reads it (`in_flight() == Some(video_id)` feeds
//! `stems_state_of`'s `is_processing`). Mirrors `stems::control::global()`.
//!
//! `-1` = nothing in flight (video ids are always positive row ids).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};

const NONE: i64 = -1;

fn cell() -> &'static AtomicI64 {
    static CELL: OnceLock<AtomicI64> = OnceLock::new();
    CELL.get_or_init(|| AtomicI64::new(NONE))
}

/// The video id currently being separated, or `None`.
pub fn in_flight() -> Option<i64> {
    match cell().load(Ordering::Relaxed) {
        NONE => None,
        id => Some(id),
    }
}

/// Set the in-flight video id. Prefer [`begin`], whose guard clears on drop.
fn set(video_id: i64) {
    cell().store(video_id, Ordering::Relaxed);
}

/// Clear the in-flight signal.
fn clear() {
    cell().store(NONE, Ordering::Relaxed);
}

/// RAII guard: mark `video_id` in flight now, clear it when the guard drops
/// (end of the separation scope, including any early return or panic). A ZST, so
/// it is `Send` and safe to hold across the separation `.await`.
#[must_use]
pub struct InFlightGuard;

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        clear();
    }
}

/// Mark `video_id` as the song currently being separated; the returned guard
/// clears the signal when it drops.
pub fn begin(video_id: i64) -> InFlightGuard {
    set(video_id);
    InFlightGuard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_sets_and_clears_in_flight() {
        // A distinctive id no other test uses, so the post-drop check is robust
        // against this process-global being touched by a parallel test (the
        // signal after our clear is never our own id again).
        let g = begin(4242);
        assert_eq!(in_flight(), Some(4242));
        drop(g);
        assert_ne!(in_flight(), Some(4242));
    }
}
