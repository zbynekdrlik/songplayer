//! #251 step 5: the heavy child's pacer — when it runs, when it is held.

use std::time::{Duration, Instant};

use sp_gpu::VblankGrid;

use super::{NO_GRID_RECHECK, QuietStep, quiet_step};
use crate::playback::program_video_quiet::QuietWindow;

/// 60.000 Hz.
const PERIOD: Duration = Duration::from_nanos(16_666_667);

fn us(n: u64) -> Duration {
    Duration::from_micros(n)
}

fn window() -> QuietWindow {
    QuietWindow {
        from: us(3_000),
        to: us(10_500),
    }
}

/// A grid whose vblanks lie on `vblank` (read five refreshes earlier).
fn grid_through(vblank: Instant) -> Option<VblankGrid> {
    Some(VblankGrid {
        at: vblank - PERIOD * 5,
        period: PERIOD,
    })
}

#[test]
fn the_child_runs_until_the_window_then_is_held_until_its_end() {
    let v = Instant::now();
    let (g, w) = (grid_through(v), window());
    assert_eq!(
        quiet_step(v, g, w),
        QuietStep::Run {
            until: v + us(3_000)
        },
        "on the vblank: run to the window"
    );
    assert_eq!(
        quiet_step(v + us(2_999), g, w),
        QuietStep::Run {
            until: v + us(3_000)
        }
    );
    assert_eq!(
        quiet_step(v + us(3_000), g, w),
        QuietStep::Pause {
            until: v + us(10_500)
        },
        "the window's start"
    );
    assert_eq!(
        quiet_step(v + us(7_000), g, w),
        QuietStep::Pause {
            until: v + us(10_500)
        }
    );
    assert_eq!(
        quiet_step(v + us(10_499), g, w),
        QuietStep::Pause {
            until: v + us(10_500)
        }
    );
    assert_eq!(
        quiet_step(v + us(10_500), g, w),
        QuietStep::Run {
            until: v + PERIOD + us(3_000)
        },
        "the window's end: run to the next refresh's window"
    );
    assert_eq!(
        quiet_step(v + us(16_000), g, w),
        QuietStep::Run {
            until: v + PERIOD + us(3_000)
        }
    );
    assert_eq!(
        quiet_step(v + PERIOD + us(5_000), g, w),
        QuietStep::Pause {
            until: v + PERIOD + us(10_500)
        },
        "the next refresh"
    );
}

#[test]
fn no_grid_or_a_window_past_a_refresh_lets_the_child_run() {
    let v = Instant::now();
    assert_eq!(quiet_step(v + us(5_000), None, window()), QuietStep::NoGrid);
    let past = QuietWindow {
        from: us(3_000),
        to: PERIOD + us(1),
    };
    assert_eq!(
        quiet_step(v + us(5_000), grid_through(v), past),
        QuietStep::NoGrid
    );
    // Up to the end of the refresh is a window.
    let whole = QuietWindow {
        from: us(3_000),
        to: PERIOD,
    };
    assert_eq!(
        quiet_step(v + us(16_000), grid_through(v), whole),
        QuietStep::Pause { until: v + PERIOD }
    );
    assert_eq!(NO_GRID_RECHECK, Duration::from_millis(100));
}

#[test]
fn a_window_whose_start_is_not_before_its_end_never_holds_the_child() {
    let v = Instant::now();
    let empty = QuietWindow {
        from: us(5_000),
        to: us(5_000),
    };
    assert_eq!(
        quiet_step(v + us(5_000), grid_through(v), empty),
        QuietStep::Run {
            until: v + PERIOD + us(5_000)
        }
    );
    let backwards = QuietWindow {
        from: us(9_000),
        to: us(4_000),
    };
    assert_eq!(
        quiet_step(v + us(6_000), grid_through(v), backwards),
        QuietStep::Run {
            until: v + us(9_000)
        }
    );
    assert_eq!(
        quiet_step(v + us(9_500), grid_through(v), backwards),
        QuietStep::Run {
            until: v + PERIOD + us(9_000)
        }
    );
}
