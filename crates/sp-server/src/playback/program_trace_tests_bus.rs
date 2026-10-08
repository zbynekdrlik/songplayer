//! #147: the source each queued program boundary shows, as the bus decides
//! it when it commits the boundary (`ProgramCore::take_queued`): the owner of
//! a forwarded or filled boundary, nobody before a source is selected, a
//! held window boundary's outgoing source, a mix's incoming one. Decided at
//! the commit, so the outgoing source's last boundaries still name it when
//! the sender takes them after the cut's first boundary was queued.
//! Wired via `#[cfg(test)] #[path = "program_trace_tests_bus.rs"] mod tests_bus;`.

use sp_core::genlock::{GENLOCK_GRID_FPS, grid_boundary_100ns, interval_100ns};

use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_bus::{
    OfferOutcome, PROGRAM_FILL_GRACE_SLOTS, ProgramCore, ProgramJob,
};
use crate::playback::program_transition::{SpecSource, TransitionSpec};
use crate::playback::submit_handoff::SubmitJob;

const A: i64 = 11;
const B: i64 = 22;

/// The `k`-th grid boundary after 2025-10-08 00:13:20 UTC.
fn b(k: i64) -> i64 {
    grid_boundary_100ns(1_759_882_400 * GENLOCK_GRID_FPS + k, GENLOCK_GRID_FPS)
}

fn grace() -> i64 {
    PROGRAM_FILL_GRACE_SLOTS * interval_100ns(GENLOCK_GRID_FPS)
}

/// A pair `w` wide (A's are 4, B's 8) on `stamp`.
fn job(w: u32, stamp: i64, live: bool) -> SubmitJob {
    SubmitJob {
        width: w,
        height: 2,
        stride: w,
        video: SharedFrame::new(vec![0; (w * 3) as usize]),
        audio: Vec::new(),
        video_tc_100ns: stamp,
        audio_tc_100ns: stamp,
        live,
    }
}

/// Every queued boundary as `(what, stamp, source)`: `"A"` / `"B"` for a
/// forwarded pair by its width, `"fill"`, `"mix"`.
fn queued(core: &mut ProgramCore) -> Vec<(&'static str, i64, Option<i64>)> {
    let mut out = Vec::new();
    while let Some((job, source)) = core.take_queued() {
        let (what, stamp) = match &job {
            ProgramJob::Source(j) if j.width == 4 => ("A", j.video_tc_100ns),
            ProgramJob::Source(j) => ("B", j.video_tc_100ns),
            ProgramJob::Standby { stamp_100ns } => ("fill", *stamp_100ns),
            ProgramJob::Mix(mix) => ("mix", mix.stamp_100ns),
        };
        out.push((what, stamp, source));
    }
    out
}

/// Nothing selected: the fill shows nobody. A selected source's forward and
/// fill show it.
#[test]
fn a_forwarded_or_filled_boundary_shows_its_owner() {
    let mut core = ProgramCore::new();
    core.release(b(0) + 10_000);
    assert_eq!(queued(&mut core), [("fill", b(0), None)]);

    let mut core = ProgramCore::new();
    core.select_initial(A);
    assert_eq!(core.offer(A, job(4, b(0), true)), OfferOutcome::Accepted);
    core.release(b(1) + grace());
    assert_eq!(
        queued(&mut core),
        [("A", b(0), Some(A)), ("fill", b(1), Some(A))]
    );
}

/// A cut from A to B on b(2): A's last boundary b(1) is queued, which
/// prunes A's segment at once (the next boundary is B's: nobody owns b(1)
/// any more), then B's first. Taken after that, A's still show A.
#[test]
fn the_outgoing_source_s_last_boundaries_still_show_it_after_the_cut() {
    let mut core = ProgramCore::new();
    core.select_initial(A);
    core.offer(A, job(4, b(0), true));
    core.cut(B, b(0));
    assert_eq!(core.status().cut_boundary_100ns, Some(b(2)));
    core.offer(A, job(4, b(1), true));
    core.offer(B, job(8, b(2), true));
    assert_eq!(core.owner_of(b(1)), None, "A's segment is pruned");
    assert_eq!(
        queued(&mut core),
        [
            ("A", b(0), Some(A)),
            ("A", b(1), Some(A)),
            ("B", b(2), Some(B))
        ]
    );
}

/// A fade from A to B on b(2): while B's cue waits, the held boundaries show
/// A (its own pair at b(2), the standby where it missed b(3)); the mix that
/// B's first live pair opens at b(4) shows B.
#[test]
fn a_held_window_boundary_shows_the_outgoing_source_a_mix_the_incoming_one() {
    let mut core = ProgramCore::new();
    assert!(core.set_transition(TransitionSpec::fade(300, SpecSource::Setting)));
    core.select_initial(A);
    core.offer(A, job(4, b(0), true));
    core.cut(B, b(0));
    core.offer(A, job(4, b(1), true));
    core.offer(A, job(4, b(2), true));
    core.offer(B, job(8, b(2), false));
    core.offer(B, job(8, b(3), false));
    core.release(b(3) + grace());
    core.offer(A, job(4, b(4), true));
    core.offer(B, job(8, b(4), true));
    assert_eq!(
        queued(&mut core),
        [
            ("A", b(0), Some(A)),
            ("A", b(1), Some(A)),
            ("A", b(2), Some(A)),
            ("fill", b(3), Some(A)),
            ("mix", b(4), Some(B))
        ]
    );
}

/// A fill inside a window shows the side on air: A while B's cue waits (as
/// a held boundary does, so A's next held pair is no cut), B once the fade
/// runs (as a mix does). Neither side sent b(3), nor later b(5).
#[test]
fn a_fill_inside_a_window_shows_the_side_on_air() {
    let mut core = ProgramCore::new();
    assert!(core.set_transition(TransitionSpec::fade(300, SpecSource::Setting)));
    core.select_initial(A);
    core.offer(A, job(4, b(0), true));
    core.cut(B, b(0));
    core.offer(A, job(4, b(1), true));
    core.offer(A, job(4, b(2), true));
    core.offer(B, job(8, b(2), false));
    core.release(b(3) + grace()); // the cue still waits
    core.offer(A, job(4, b(4), true));
    core.offer(B, job(8, b(4), true)); // B's first live pair: the fade starts
    core.release(b(5) + grace()); // the fade runs
    assert_eq!(
        queued(&mut core),
        [
            ("A", b(0), Some(A)),
            ("A", b(1), Some(A)),
            ("A", b(2), Some(A)),
            ("fill", b(3), Some(A)),
            ("mix", b(4), Some(B)),
            ("fill", b(5), Some(B))
        ]
    );
}
