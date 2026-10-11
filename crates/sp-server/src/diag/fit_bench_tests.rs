//! Tests for the program canvas fit bench (#223 S10a): what a run takes, the
//! synthetic picture, and the report's gate. The timings themselves are the
//! box's to measure.

use super::*;

#[test]
fn a_size_and_count_are_checked() {
    assert_eq!(check(3840, 2160, 600), Ok(()));
    assert_eq!(check(2, 2, 1), Ok(()));
    let size = Err("width must be 2..=3840 and height 2..=2160");
    assert_eq!(check(0, 2160, 1), size);
    assert_eq!(check(3842, 2160, 1), size);
    assert_eq!(check(3840, 2162, 1), size);
    assert_eq!(check(3840, 0, 1), size);
    let odd = Err("width and height must be even (NV12)");
    assert_eq!(check(1919, 1080, 1), odd);
    assert_eq!(check(1920, 1081, 1), odd);
    let frames = Err("frames must be 1..=600");
    assert_eq!(check(1920, 1080, 0), frames);
    assert_eq!(check(1920, 1080, 601), frames);
}

#[test]
fn the_synthetic_picture_is_a_whole_nv12_with_ramps() {
    let p = synthetic(224, 2);
    assert_eq!(p.len(), nv12_len(224, 2));
    // Luma: 16 + (x + y) % 220 along each row.
    assert_eq!((p[0], p[1], p[219], p[220], p[223]), (16, 17, 235, 16, 19));
    assert_eq!((p[224], p[224 + 219]), (17, 16));
    // Chroma after the luma plane: 64 + i % 128.
    let chroma = &p[224 * 2..];
    assert_eq!(chroma.len(), 224);
    assert_eq!(
        (chroma[0], chroma[127], chroma[128], chroma[223]),
        (64, 191, 64, 159)
    );
}

/// 100 samples: 98 at `p50`, the last two at `p99` (nearest-rank p99 = the
/// 99th sorted).
fn us(p50: u64, p99: u64) -> Vec<u64> {
    let mut v = vec![p50; 98];
    v.extend([p99, p99]);
    v
}

#[test]
fn the_report_carries_the_run_and_the_canvas() {
    let fit = us(4_000, 9_000);
    let fade = us(6_000, 12_000);
    let r = report(3840, 2160, 6, &fit, &fade);
    assert_eq!((r.width, r.height, r.bands, r.frames), (3840, 2160, 6, 100));
    assert_eq!((r.canvas_width, r.canvas_height), (1920, 1080));
    assert_eq!((r.fit_us.p50, r.fit_us.p99), (4_000, 9_000));
    assert_eq!((r.fade_us.p50, r.fade_us.p99), (6_000, 12_000));
    assert_eq!(r.budget_us, 16_666);
    assert!(!r.over_budget);
}

/// G3 fails when EITHER p99 is over half a slot; exactly at it passes.
#[test]
fn the_gate_is_either_p99_over_half_a_slot() {
    let at = us(1_000, BUDGET_US);
    let over = us(1_000, BUDGET_US + 1);
    assert!(!report(3840, 2160, 6, &at, &at).over_budget);
    assert!(report(3840, 2160, 6, &over, &at).over_budget);
    assert!(report(3840, 2160, 6, &at, &over).over_budget);
}

/// A small run end to end: every fit and fade timed, on the asked bands.
#[test]
fn a_small_run_times_every_frame() {
    let r = run(64, 36, 3, 2);
    assert_eq!((r.width, r.height, r.frames, r.bands), (64, 36, 3, 2));
    assert!(r.fit_us.max >= r.fit_us.p50 && r.fade_us.max >= r.fade_us.p50);
}
