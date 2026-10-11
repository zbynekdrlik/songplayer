//! #233 (review): SongPlayer's program timeline, read in the ASIO host — ONE
//! source of truth for time.
//!
//! The worker compares a block's boundary (SongPlayer's program timeline)
//! with "now" on the same timeline. Every wall's timeline is UTC − D(K),
//! and K, the fleet relabel (`fleet_shift.rs`, #224 part 2), is per
//! PROCESS: a host with walls of its own would follow each date step itself
//! and could register an N one apart from SongPlayer's, putting every block
//! a slot off (a ~33 ms deficit: under the 38.3 ms floor, a pop). So the
//! host has NO wall: SongPlayer pairs a reading of its own timeline with the
//! box's one monotonic counter (`mono.rs`: QueryPerformanceCounter, the
//! same in every process) — an [`Anchor`] — in the hello and in every
//! block it writes, and the host's [`RemoteClock`] carries the latest
//! anchor forward on the counter. The block's boundary crosses untouched.
//!
//! When SongPlayer's wall follows a date step, slews or holds, the next
//! anchor moves with it, so the host's "now" moves as the in-process
//! worker's own wall did (a jump of the remainder r, never whole slots). The
//! clock never goes back: through a hold it stays where it was until the
//! anchor's line catches up (the wall's own hold).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::playback::audio_out_queue::lock;
use crate::playback::vban_out::VbanClock;

/// A reading of SongPlayer's program timeline and the box's monotonic
/// counter taken at the same instant (both 100 ns).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub timeline_100ns: i64,
    pub mono_100ns: i64,
}

impl Anchor {
    /// The timeline at the monotonic instant `mono_100ns`, carried from this
    /// anchor.
    pub fn at(&self, mono_100ns: i64) -> i64 {
        self.timeline_100ns + (mono_100ns - self.mono_100ns)
    }
}

/// A session's latest anchor: the frame loop sets it, every worker's clock
/// reads it.
#[derive(Debug, Default)]
pub struct SharedAnchor(Mutex<Option<Anchor>>);

impl SharedAnchor {
    pub fn set(&self, anchor: Anchor) {
        *lock(&self.0) = Some(anchor);
    }

    pub fn get(&self) -> Option<Anchor> {
        *lock(&self.0)
    }
}

/// The box's monotonic counter, 100 ns (`mono::mono_100ns`; a fake in tests).
pub type Mono = Arc<dyn Fn() -> i64 + Send + Sync>;

/// SongPlayer's timeline in the host (module doc). Before any anchor it
/// reads the counter itself (the hello carries one, so a worker never
/// sees that).
pub struct RemoteClock {
    anchor: Arc<SharedAnchor>,
    mono: Mono,
    last: Option<i64>,
}

impl RemoteClock {
    pub fn new(anchor: Arc<SharedAnchor>, mono: Mono) -> Self {
        Self {
            anchor,
            mono,
            last: None,
        }
    }

    /// Now on SongPlayer's timeline, never before the last reading.
    pub fn read(&mut self) -> i64 {
        let mono = (self.mono)();
        let carried = self.anchor.get().map_or(mono, |a| a.at(mono));
        let now = self.last.map_or(carried, |last| last.max(carried));
        self.last = Some(now);
        now
    }
}

impl VbanClock for RemoteClock {
    fn now_100ns(&mut self) -> i64 {
        self.read()
    }

    fn sleep_100ns(&mut self, d_100ns: i64) {
        std::thread::sleep(Duration::from_nanos(d_100ns.max(0) as u64 * 100));
    }

    fn slew_owed_100ns(&self) -> i64 {
        0
    }
}

#[cfg(test)]
#[path = "remote_clock_tests.rs"]
mod tests;
