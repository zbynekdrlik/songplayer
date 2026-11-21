//! #289 step 1: a scope ends by `PacedFeed::finish`, which hands out the
//! pair its pacer committed one slot ahead before it detaches, so the next
//! scope (and the consumer's fills) continue after that pair, never over it.

use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::pacer::{AUDIO_LEAD_SLOTS, Pacer, ServiceOutcome, StandbyBlack};
use crate::playback::picture::Picture;
use crate::playback::submit_handoff::SUBMIT_HANDOFF_BOUND;
use crate::playback::wallclock::{SettableClock, WallClock};

/// The k-th exact-rational grid boundary at 30 fps, in 100-ns units.
fn b(k: i64) -> i64 {
    k * 10_000_000 / 30
}

/// The real wall reaches the instant the pacer's decision clock reads
/// `until`.
fn wait_to(clk: &SettableClock, lead: i64, until: i64) {
    if clk.get() < until - lead {
        clk.set(until - lead);
    }
}

/// A production-lead pacer that served `n` idle boundaries into a scope of
/// `handoff`; returns the pacer and its wall.
fn idle_scope(handoff: &SharedHandoff, n: usize) -> (Pacer, SettableClock, PacedFeed<'_>) {
    let lead = AUDIO_LEAD_SLOTS * 333_333;
    let (wall, clk) = WallClock::settable(-lead);
    let mut pacer = Pacer::with_lead(30, true, wall, AUDIO_LEAD_SLOTS);
    let black: Picture = SharedFrame::new(vec![0u8; 12]).into();
    let feed = PacedFeed::attach(handoff);
    let mut sink = feed.sink();
    let mut served = 0;
    while served < n {
        let standby = StandbyBlack {
            width: 4,
            height: 2,
            stride: 4,
            video: &black,
        };
        match pacer.service_standby(standby.standby(), &mut sink) {
            ServiceOutcome::Wait { until_100ns } => wait_to(&clk, lead, until_100ns),
            _ => {
                pacer.tick_wall();
                served += 1;
            }
        }
    }
    (pacer, clk, feed)
}

#[test]
fn a_finished_scope_hands_out_its_committed_pair_before_it_detaches() {
    let handoff = SharedHandoff::new(SUBMIT_HANDOFF_BOUND);
    let (mut pacer, clk, feed) = idle_scope(&handoff, 4);
    assert_eq!(pacer.committed(), 1, "b(4) committed, not yet out");
    let lead = pacer.lead_100ns();
    feed.finish(&mut pacer, |_, until| wait_to(&clk, lead, until));
    assert_eq!(pacer.committed(), 0);
    assert_eq!(clk.get(), b(4), "it left when the wall reached its stamp");
    let next = PacedFeed::attach(&handoff);
    assert_eq!(next.continue_after_100ns(), Some(b(4)));
}

#[test]
fn a_scope_dropped_without_its_finish_leaves_the_committed_pair_behind() {
    let handoff = SharedHandoff::new(SUBMIT_HANDOFF_BOUND);
    let (pacer, _clk, feed) = idle_scope(&handoff, 4);
    drop(feed);
    assert_eq!(pacer.committed(), 1);
    let next = PacedFeed::attach(&handoff);
    assert_eq!(next.continue_after_100ns(), Some(b(3)));
}
