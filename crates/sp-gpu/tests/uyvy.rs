//! WARP proof of the UYVY pass (#279 P1a): the compositor converts one
//! target (the FHD program) to BT.709 limited-range UYVY on the GPU, copies
//! it into a ring of staging textures inside the compose, and reads it back
//! with `D3D11_MAP_FLAG_DO_NOT_WAIT`.
//!
//! Each readback is compared with `sp_gpu::uyvy::reference` computed from
//! the SAME target's own BGRA readback (`read_back_target`): every byte
//! within `UYVY_TOLERANCE` (one code, `uyvy.rs`). The composite itself is
//! pinned against `sp_gpu::reference` by `tests/warp.rs`, so its tolerance
//! never adds to this one. The worst difference of each run goes to stderr
//! past libtest's capture (`rust-workspace.md`), so the CI log records it.
//!
//! Windows only. A WARP capability the pass needs that is missing FAILS
//! here (the `DO_NOT_WAIT` readback was proven on WARP by #279 P0's probe,
//! step 5); nothing is skipped.

#![cfg(windows)]

mod common;

use std::io::Write;

use common::{pattern, picture, warp, warp_fhd};
use sp_gpu::uyvy::{UYVY_TOLERANCE, reference};
use sp_gpu::{
    ComposeStats, Composition, Compositor, FHD_FIT, FHD_HEIGHT, FHD_TARGET, FHD_WIDTH, GpuError,
    TargetId,
};

const FHD: (u32, u32) = (FHD_WIDTH, FHD_HEIGHT);

/// The bytes of one limited-range black pixel pair: U, Y0, V, Y1.
const BLACK_PAIR: [u8; 4] = [128, 16, 128, 16];

/// `uyvy` against the reference of `bgra` (a `size` BGRA frame): every byte
/// within the tolerance. The worst difference goes to stderr.
fn assert_uyvy_matches(uyvy: &[u8], bgra: &[u8], size: (u32, u32), what: &str) {
    let (w, h) = size;
    assert_eq!(uyvy.len(), (w * h * 2) as usize, "{what}: 2 bytes a pixel");
    let want = reference(bgra, w, h);
    assert_eq!(want.len(), uyvy.len(), "{what}: the reference's size");
    let row = (w * 2) as usize;
    let mut worst = 0u8;
    let mut over = 0usize;
    let mut first = Vec::new();
    for (i, (&got, &expected)) in uyvy.iter().zip(&want).enumerate() {
        let diff = got.abs_diff(expected);
        worst = worst.max(diff);
        if diff > UYVY_TOLERANCE {
            over += 1;
            if first.len() < 10 {
                first.push(format!(
                    "row {}, pair {}, byte {}: got {got}, want {expected}",
                    i / row,
                    (i % row) / 4,
                    i % 4
                ));
            }
        }
    }
    let _ = writeln!(
        std::io::stderr(),
        "#279 P1a {what}: worst UYVY difference {worst} code(s) over {} bytes",
        uyvy.len()
    );
    assert_eq!(
        over, 0,
        "{what}: {over} bytes outside ±{UYVY_TOLERANCE} (worst {worst}), first: {first:?}"
    );
}

/// Compose `composition`, then read the UYVY (it must read at once) and
/// the BGRA of `target`.
fn compose_and_read(
    compositor: &mut Compositor,
    target: TargetId,
    composition: &Composition<'_>,
) -> (ComposeStats, Vec<u8>, Vec<u8>) {
    let stats = compositor.compose(composition).expect("compose on WARP");
    let mut uyvy = Vec::new();
    assert_eq!(
        compositor.read_uyvy(&mut uyvy),
        Ok(true),
        "a finished compose's UYVY reads without a wait"
    );
    let bgra = compositor
        .read_back_target(target)
        .expect("read back the target's BGRA");
    (stats, uyvy, bgra)
}

/// `SP-program`'s 1920×1080 target as a compositor's primary, with its
/// UYVY pass.
fn fhd_with_uyvy() -> Compositor {
    let mut compositor = warp_fhd();
    compositor
        .enable_uyvy(TargetId::PRIMARY)
        .unwrap_or_else(|e| panic!("WARP must make the UYVY pass: {e}"));
    compositor
}

/// Nothing to read before a compose; then a 1440p picture scaled into the
/// FHD target converts within one code, and the compose reports the
/// pass's part of its draw.
#[test]
fn a_picture_converts_within_one_code_of_the_reference() {
    let mut compositor = fhd_with_uyvy();
    let mut untouched = vec![7u8];
    assert_eq!(compositor.read_uyvy(&mut untouched), Ok(false));
    assert_eq!(untouched, [7], "nothing written before a compose");

    let (stride, data) = pattern(2560, 1440, 7);
    let scaled = Composition::Picture(picture(1, 2560, 1440, stride, &data));
    let (stats, uyvy, bgra) = compose_and_read(&mut compositor, TargetId::PRIMARY, &scaled);
    let pass = stats.uyvy_us.expect("the UYVY pass ran in the compose");
    let fhd = compositor.draw_us(TargetId::PRIMARY).expect("drawn");
    assert!(
        fhd + pass <= stats.draw_us,
        "each part of the one draw: {fhd} + {pass} > {stats:?}"
    );
    assert_uyvy_matches(&uyvy, &bgra, FHD, "2560x1440 in 1920x1080");
}

/// A 4:3 picture keeps `SP-program`'s bars: columns 0..240 and 1680..1920
/// are black, so pairs 0..120 and 840..960 of each row are exactly the
/// limited-range black, and no pair of the picture is (a scratch model of
/// the composite puts each ≥ 11 codes from it in these rows); the whole
/// frame converts within one code.
#[test]
fn a_4_by_3_picture_keeps_its_bars_as_limited_range_black() {
    let mut compositor = fhd_with_uyvy();
    let (stride, data) = pattern(1440, 1080, 51);
    let narrow = Composition::Picture(picture(1, 1440, 1080, stride, &data));
    let (_, uyvy, bgra) = compose_and_read(&mut compositor, TargetId::PRIMARY, &narrow);
    for y in [0usize, 540, 1079] {
        let row = &uyvy[y * 3840..(y + 1) * 3840];
        for (pair, bytes) in row.chunks_exact(4).enumerate() {
            let bar = !(120..840).contains(&pair);
            assert_eq!(bytes == BLACK_PAIR, bar, "row {y}, pair {pair}: {bytes:?}");
        }
    }
    assert_uyvy_matches(&uyvy, &bgra, FHD, "1440x1080 in 1920x1080");
}

#[test]
fn a_half_way_fade_converts_within_one_code_of_the_reference() {
    let mut compositor = fhd_with_uyvy();
    let (from_stride, from) = pattern(2560, 1440, 7);
    let (to_stride, to) = pattern(1440, 1080, 120);
    let fade = Composition::Fade {
        from: Some(picture(1, 2560, 1440, from_stride, &from)),
        to: Some(picture(2, 1440, 1080, to_stride, &to)),
        weight_q8: 128,
    };
    let (_, uyvy, bgra) = compose_and_read(&mut compositor, TargetId::PRIMARY, &fade);
    assert_uyvy_matches(&uyvy, &bgra, FHD, "fade 50 % in 1920x1080");
}

/// The program's standby: every pair is the limited-range black.
#[test]
fn black_is_limited_range_black_everywhere() {
    let mut compositor = fhd_with_uyvy();
    let (_, uyvy, bgra) = compose_and_read(&mut compositor, TargetId::PRIMARY, &Composition::Black);
    assert_eq!(uyvy.len(), 1920 * 1080 * 2);
    assert!(
        uyvy.chunks_exact(4).all(|pair| pair == BLACK_PAIR),
        "the standby is black"
    );
    assert_uyvy_matches(&uyvy, &bgra, FHD, "black");
}

/// Ten composes in a row, three pictures in turn: each readback reads at
/// once (never `DXGI_ERROR_WAS_STILL_DRAWING`: the ring of three wraps
/// three times) and holds THAT compose's picture. With three pictures over
/// a ring of three, every other staging texture holds another picture (or
/// nothing yet), so a read of any slot but the newest fails the reference.
#[test]
fn ten_composes_in_a_row_each_read_back_without_a_wait() {
    let mut compositor = fhd_with_uyvy();
    let (stride, a) = pattern(1920, 1080, 3);
    let (_, b) = pattern(1920, 1080, 200);
    let (_, c) = pattern(1920, 1080, 97);
    let pictures = [
        Composition::Picture(picture(1, 1920, 1080, stride, &a)),
        Composition::Picture(picture(2, 1920, 1080, stride, &b)),
        Composition::Picture(picture(3, 1920, 1080, stride, &c)),
    ];
    let mut last: Option<Vec<u8>> = None;
    for i in 0..10 {
        let (stats, uyvy, bgra) =
            compose_and_read(&mut compositor, TargetId::PRIMARY, &pictures[i % 3]);
        assert!(stats.uyvy_us.is_some(), "compose {i}: the pass ran");
        assert_uyvy_matches(&uyvy, &bgra, FHD, &format!("compose {i}"));
        if let Some(previous) = &last {
            assert_ne!(
                &uyvy, previous,
                "compose {i}: the newest copy, not the last one"
            );
        }
        last = Some(uyvy);
    }
}

/// A pass packs pixel pairs: a target of odd width is refused, and the
/// compositor composes on with no pass.
#[test]
fn a_target_of_odd_width_is_refused() {
    let mut compositor = Compositor::new_warp_with_size(1919, 1080, FHD_FIT)
        .unwrap_or_else(|e| panic!("WARP must build a 1919x1080 compositor: {e}"));
    assert_eq!(
        compositor.enable_uyvy(TargetId::PRIMARY),
        Err(GpuError::UyvyOddWidth { width: 1919 })
    );
    let mut out = Vec::new();
    assert!(matches!(
        compositor.read_uyvy(&mut out),
        Err(GpuError::NoObject { .. })
    ));
    let stats = compositor.compose(&Composition::Black).expect("compose");
    assert_eq!(stats.uyvy_us, None, "no pass");
}

/// The production shape: MAX's compositor with `SP-program`'s target added
/// (#251 step 2) and the pass on that target. The pass converts the FHD
/// target, not MAX's; a removed target takes its pass with it, and MAX
/// composes on.
#[test]
fn a_removed_target_takes_its_uyvy_pass_with_it() {
    let mut compositor = warp();
    let fhd = compositor
        .add_target(FHD_TARGET)
        .unwrap_or_else(|e| panic!("WARP must add the 1920x1080 target: {e}"));
    compositor
        .enable_uyvy(fhd)
        .unwrap_or_else(|e| panic!("WARP must make the FHD target's UYVY pass: {e}"));
    let (stride, data) = pattern(1440, 1080, 51);
    let narrow = Composition::Picture(picture(1, 1440, 1080, stride, &data));
    let (stats, uyvy, bgra) = compose_and_read(&mut compositor, fhd, &narrow);
    let max = compositor.draw_us(TargetId::PRIMARY).expect("MAX drawn");
    let fhd_part = compositor.draw_us(fhd).expect("FHD drawn");
    let pass = stats.uyvy_us.expect("the pass ran");
    assert!(
        max + fhd_part + pass <= stats.draw_us,
        "each part of the one draw: {max} + {fhd_part} + {pass} > {stats:?}"
    );
    assert_uyvy_matches(&uyvy, &bgra, FHD, "4:3 on MAX's compositor, FHD target");
    assert_eq!(uyvy[540 * 3840..540 * 3840 + 4], BLACK_PAIR, "FHD's bar");

    assert!(compositor.remove_target(fhd));
    let mut out = Vec::new();
    assert!(matches!(
        compositor.read_uyvy(&mut out),
        Err(GpuError::NoObject { .. })
    ));
    assert!(matches!(
        compositor.enable_uyvy(fhd),
        Err(GpuError::NoObject { .. })
    ));
    let stats = compositor.compose(&narrow).expect("MAX composes on");
    assert_eq!(stats.uyvy_us, None, "no pass after its target went");
    assert!(!compositor.disable_uyvy(), "nothing left to disable");
}

/// The pass belongs to its own target: removing ANOTHER target keeps it
/// and its newest copy. Here the pass's target is drawn first and the
/// other one after it, so the pass also runs with another target left
/// bound as the output.
#[test]
fn removing_another_target_keeps_the_pass() {
    let mut compositor = fhd_with_uyvy();
    let other = compositor
        .add_target(FHD_TARGET)
        .unwrap_or_else(|e| panic!("WARP must add a second target: {e}"));
    let (stride, data) = pattern(1280, 720, 17);
    let plain = Composition::Picture(picture(1, 1280, 720, stride, &data));
    let (_, uyvy, bgra) = compose_and_read(&mut compositor, TargetId::PRIMARY, &plain);
    assert_uyvy_matches(&uyvy, &bgra, FHD, "the first of two targets");

    assert!(compositor.remove_target(other));
    let mut kept = Vec::new();
    assert_eq!(
        compositor.read_uyvy(&mut kept),
        Ok(true),
        "the pass and its newest copy stay"
    );
    assert_eq!(kept, uyvy, "the same copy");
    let (stride, next) = pattern(1920, 1080, 61);
    let after = Composition::Picture(picture(2, 1920, 1080, stride, &next));
    let (stats, uyvy, bgra) = compose_and_read(&mut compositor, TargetId::PRIMARY, &after);
    assert!(stats.uyvy_us.is_some(), "the pass still runs");
    assert_uyvy_matches(&uyvy, &bgra, FHD, "after the other target went");
}

/// Disabled, the pass reads nothing and costs nothing; enabled again (or
/// enabled over a pass) it starts afresh: nothing to read before the next
/// compose.
#[test]
fn a_pass_disabled_or_enabled_again_starts_afresh() {
    let mut compositor = fhd_with_uyvy();
    let (stride, data) = pattern(1280, 720, 17);
    let plain = Composition::Picture(picture(1, 1280, 720, stride, &data));
    compose_and_read(&mut compositor, TargetId::PRIMARY, &plain);

    assert!(compositor.disable_uyvy());
    assert!(!compositor.disable_uyvy(), "disabled once");
    let mut out = Vec::new();
    assert!(matches!(
        compositor.read_uyvy(&mut out),
        Err(GpuError::NoObject { .. })
    ));
    let stats = compositor.compose(&plain).expect("compose with no pass");
    assert_eq!(stats.uyvy_us, None);

    compositor
        .enable_uyvy(TargetId::PRIMARY)
        .expect("enabled again");
    assert_eq!(
        compositor.read_uyvy(&mut out),
        Ok(false),
        "no compose since"
    );
    compose_and_read(&mut compositor, TargetId::PRIMARY, &plain);
    compositor
        .enable_uyvy(TargetId::PRIMARY)
        .expect("enabled over the pass");
    assert_eq!(
        compositor.read_uyvy(&mut out),
        Ok(false),
        "a new pass, a new ring"
    );
    let (_, uyvy, bgra) = compose_and_read(&mut compositor, TargetId::PRIMARY, &plain);
    assert_uyvy_matches(&uyvy, &bgra, FHD, "1280x720 after a re-enable");
}
