//! #215: the pure transition layer — the spec (setting / fallback), the
//! window's grid indices and its cue (the gate that waits for the incoming
//! source's first live pair), the Q8 picture weight + NV12 blend, the NV12 fit
//! of one layout into another, the equal-power gain curve and the audio mix.
//! Exact values, so every arithmetic mutant dies (the fit's pins come from a
//! scratch Python model of `FitPlan`). The `blend` / `fitted` helpers also run
//! the fused `mix_nv12_into` in every band count (#215 addendum 3; a plain
//! fit and a fade, #223 follow-up), so each blend and fit pin pins it too;
//! its own tests are in `nv12_mix_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "program_transition_tests.rs"] mod tests;`.

use super::*;
use crate::playback::band_pool::BandPool;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::submit_handoff::SubmitJob;
use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns, strict_next_boundary_100ns};
use sp_ndi::AudioFrame;
use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};

/// 2026-09 in 100 ns since the epoch — exactly on a second (slot 0).
const T0: i64 = 17_900_000_000_000_000;

/// The k-th grid boundary after `floor(T0)`.
fn b(k: usize) -> i64 {
    let mut x = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    for _ in 0..k {
        x = strict_next_boundary_100ns(x, GENLOCK_GRID_FPS);
    }
    x
}

#[test]
fn a_duration_rounds_to_the_nearest_whole_slot_at_least_one_at_most_ten_seconds() {
    assert_eq!(slots_for_ms(300), 9, "the design example: 300 ms = 9 slots");
    assert_eq!(slots_for_ms(1000), 30);
    assert_eq!(slots_for_ms(49), 1, "1.47 slots");
    assert_eq!(slots_for_ms(50), 2, "1.5 slots rounds up");
    assert_eq!(slots_for_ms(83), 2, "2.49 slots");
    assert_eq!(slots_for_ms(84), 3, "2.52 slots");
    assert_eq!(slots_for_ms(16), 1, "0.48 slots is still one slot");
    assert_eq!(slots_for_ms(0), 1);
    assert_eq!(slots_for_ms(10_000), 300);
    assert_eq!(slots_for_ms(10_017), 300, "300.51 slots is capped");
    assert_eq!(slots_for_ms(u32::MAX), MAX_TRANSITION_SLOTS);
    assert_eq!(MAX_TRANSITION_SLOTS, 300);
    assert_eq!(
        slots_for_ms(sp_core::config::MAX_PROGRAM_TRANSITION_MS),
        MAX_TRANSITION_SLOTS,
        "the Nastavenia maximum is exactly the longest window"
    );
}

#[test]
fn a_fade_carries_its_duration_and_slots_and_a_cut_has_none() {
    let fade = TransitionSpec::fade(300, SpecSource::Fallback);
    assert_eq!(
        (fade.kind, fade.duration_ms, fade.n_slots, fade.source),
        (TransitionKind::Fade, 300, 9, SpecSource::Fallback)
    );
    let cut = TransitionSpec::cut(SpecSource::Setting);
    assert_eq!(
        (cut.kind, cut.duration_ms, cut.n_slots, cut.source),
        (TransitionKind::Cut, 0, 0, SpecSource::Setting)
    );
}

/// #221 L5: the operator's choice, else the default fade of the setting's
/// length — never cg OBS's transition any more.
#[test]
fn the_setting_chooses_and_none_is_the_default_fade() {
    assert_eq!(
        effective_spec(Some(TransitionMode::Fade), 400),
        TransitionSpec::fade(400, SpecSource::Setting)
    );
    assert_eq!(
        effective_spec(Some(TransitionMode::Cut), 400),
        TransitionSpec::cut(SpecSource::Setting)
    );
    assert_eq!(
        effective_spec(None, 400),
        TransitionSpec::fade(400, SpecSource::Fallback)
    );
    assert_eq!(effective_spec(None, 400).n_slots, 12, "400 ms → 12 slots");
}

#[test]
fn the_transition_setting_parses_fade_and_cut_else_none() {
    assert_eq!(
        TransitionMode::parse(Some("fade")),
        Some(TransitionMode::Fade)
    );
    assert_eq!(
        TransitionMode::parse(Some(" cut ")),
        Some(TransitionMode::Cut)
    );
    assert_eq!(
        TransitionMode::parse(Some("obs")),
        None,
        "the retired follow"
    );
    assert_eq!(TransitionMode::parse(Some("wipe")), None);
    assert_eq!(
        TransitionMode::parse(Some("Fade")),
        None,
        "exact words only"
    );
    assert_eq!(TransitionMode::parse(None), None);
}

#[test]
fn a_window_spans_exactly_its_slots_and_is_served_boundary_by_boundary() {
    let w = Window::new(
        Some(1),
        2,
        b(3),
        &TransitionSpec::fade(300, SpecSource::Setting),
    );
    assert_eq!((w.from, w.to, w.kind), (Some(1), 2, TransitionKind::Fade));
    assert_eq!((w.start_100ns, w.n_slots, w.end_100ns), (b(3), 9, b(12)));
    assert_eq!(
        (w.cut_100ns, w.cue),
        (b(3), Cue::Open),
        "an open window starts mixing on its cut boundary"
    );
    assert_eq!(w.slot(b(2)), None, "before the window");
    for k in 0..9 {
        assert_eq!(w.slot(b(3 + k)), Some(k as u32), "slot of b({})", 3 + k);
    }
    assert_eq!(w.slot(b(12)), None, "the end is exclusive");
    assert_eq!(w.served(None), 0);
    assert_eq!(w.served(Some(b(1))), 0);
    assert_eq!(w.served(Some(b(2))), 0);
    assert_eq!(w.served(Some(b(3))), 1);
    assert_eq!(w.served(Some(b(7))), 5);
    assert_eq!(w.served(Some(b(11))), 9);
    assert_eq!(w.served(Some(b(40))), 9);

    let mut cut_short = w;
    cut_short.truncate(b(6));
    assert_eq!(cut_short.end_100ns, b(6));
    assert_eq!(cut_short.slot(b(5)), Some(2));
    assert_eq!(cut_short.slot(b(6)), None);
    assert_eq!(cut_short.served(Some(b(40))), 3);
    assert_eq!(cut_short.n_slots, 9, "the curve keeps its length");
    cut_short.truncate(b(9));
    assert_eq!(cut_short.end_100ns, b(6), "a truncation never extends");

    let cut = Window::new(Some(1), 2, b(3), &TransitionSpec::cut(SpecSource::Setting));
    assert_eq!(
        (cut.kind, cut.n_slots, cut.end_100ns),
        (TransitionKind::Cut, 0, b(3))
    );
    assert_eq!(cut.slot(b(3)), None, "a cut mixes no boundary");
    assert_eq!(cut.served(Some(b(40))), 0);
}

#[test]
fn the_picture_weight_is_the_slots_midpoint_fraction() {
    assert_eq!(Q8_ONE, 256);
    assert_eq!(weight_q8(0, 1), 128, "one slot: half and half");
    assert_eq!(weight_q8(0, 2), 64);
    assert_eq!(weight_q8(1, 2), 192);
    assert_eq!(weight_q8(0, 9), 14);
    assert_eq!(weight_q8(3, 9), 100);
    assert_eq!(weight_q8(4, 9), 128, "the middle slot of nine");
    assert_eq!(weight_q8(8, 9), 242);
    assert_eq!(weight_q8(0, 0), 128, "no slots counts as one");
    assert_eq!(weight_q8(20, 2), 256, "never past all-`to`");
}

/// A known 4×2 NV12 frame: 8 luma bytes, then 4 interleaved chroma bytes.
const FROM_4X2: [u8; 12] = [16, 32, 64, 100, 128, 200, 235, 0, 128, 128, 90, 240];
const TO_4X2: [u8; 12] = [235, 16, 64, 101, 0, 255, 16, 255, 16, 240, 128, 128];

/// The reference blend of FROM_4X2 over TO_4X2, and (#215 addendum 3) the
/// fused mix of the same layout, in every band count, equal to it.
fn blend(weight: u32) -> Vec<u8> {
    let mut out = Vec::new();
    blend_nv12_into(&FROM_4X2, &TO_4X2, weight, &mut out);
    for bands in 1..=MAX_MIX_BANDS {
        let pool = BandPool::new("transition-test", bands);
        let mut fused = Vec::new();
        let paint = Paint::Fade {
            from: Side::Same(&FROM_4X2),
            to: Side::Same(&TO_4X2),
            weight,
        };
        mix_nv12_into(L_4X2, paint, &pool, &mut fused);
        assert_eq!(fused, out, "the fused mix in {bands} bands");
    }
    out
}

#[test]
fn the_nv12_blend_is_exact_at_alpha_zero_half_and_one() {
    assert_eq!(blend(0), FROM_4X2.to_vec(), "α = 0: the `from` frame");
    assert_eq!(blend(Q8_ONE), TO_4X2.to_vec(), "α = 1: the `to` frame");
    assert_eq!(
        blend(Q8_ONE / 2),
        vec![126, 24, 64, 101, 64, 228, 126, 128, 72, 184, 109, 184],
        "α = ½: (f + t + 1) / 2 on luma and chroma alike"
    );
    assert_eq!(
        blend(64),
        vec![71, 28, 64, 100, 96, 214, 180, 64, 100, 156, 100, 212],
        "α = ¼: (3f + t) / 4, rounded"
    );
    assert_eq!(blend(300), TO_4X2.to_vec(), "a weight past 256 is all `to`");
}

#[test]
fn the_blend_appends_after_what_the_buffer_holds() {
    let mut out = vec![9u8];
    blend_nv12_into(&FROM_4X2[..2], &TO_4X2[..2], 0, &mut out);
    assert_eq!(out, vec![9, 16, 32]);
}

/// FROM_4X2's layout: 4×2, stride 4, 12 bytes.
const L_4X2: Layout = Layout {
    width: 4,
    height: 2,
    stride: 4,
    len: 12,
};

/// A `width`×`height` NV12 layout with a tight stride.
const fn tight(width: u32, height: u32) -> Layout {
    Layout {
        width,
        height,
        stride: width,
        len: (width * height * 3 / 2) as usize,
    }
}

/// `src` of `src_layout` fitted into `dst`, checking that the fit appends,
/// and (#215 addendum 3) that the fused kernel's plain fit (#223 follow-up:
/// its one side) and its fade at weight 0 — the outgoing picture alone,
/// whatever the incoming one holds — draw the same bytes in every band
/// count, so every fit pin below also pins the fused kernel.
fn fitted(src: &[u8], src_layout: Layout, dst: Layout) -> Vec<u8> {
    let mut out = vec![9u8];
    fit_nv12_into(src, src_layout, dst, &mut out);
    assert_eq!(
        out.remove(0),
        9,
        "the fit appends after what the buffer holds"
    );
    assert_eq!(out.len(), dst.len, "exactly the destination's bytes");
    let (plan, to) = (FitPlan::new(src_layout, dst), vec![0x5a; dst.len]);
    for bands in 1..=MAX_MIX_BANDS {
        let pool = BandPool::new("transition-test", bands);
        let fitted = Side::Fitted(&plan, src);
        let fade = Paint::Fade {
            from: fitted,
            to: Side::Same(&to),
            weight: 0,
        };
        for (paint, what) in [(Paint::Fit(fitted), "fit"), (fade, "fade at weight 0")] {
            let mut fused = Vec::new();
            mix_nv12_into(dst, paint, &pool, &mut fused);
            assert_eq!(fused, out, "the fused {what} in {bands} bands");
        }
    }
    out
}

#[test]
fn fitting_a_picture_into_its_own_layout_copies_it_exactly() {
    assert_eq!(FitPlan::new(L_4X2, L_4X2).rect(), (0, 0, 4, 2));
    assert_eq!(fitted(&FROM_4X2, L_4X2, L_4X2), FROM_4X2.to_vec());
    // A wider stride: the same picture, its padding studio black.
    let padded = Layout {
        width: 4,
        height: 2,
        stride: 6,
        len: 18,
    };
    assert_eq!(
        fitted(&FROM_4X2, L_4X2, padded),
        vec![
            16, 32, 64, 100, 16, 16, 128, 200, 235, 0, 16, 16, 128, 128, 90, 240, 128, 128
        ]
    );
}

#[test]
fn a_4x2_picture_fits_2x2_by_averaging_each_pixel_pair() {
    // 2:1 into 1:1 keeps the full width; the aspect-kept height (1) rounds up
    // to the even 2, so the chroma rows still line up.
    let dst = tight(2, 2);
    assert_eq!(FitPlan::new(L_4X2, dst).rect(), (0, 0, 2, 2));
    assert_eq!(
        fitted(&FROM_4X2, L_4X2, dst),
        vec![24, 82, 164, 118, 109, 184],
        "luma (16+32)/2, (64+100)/2, (128+200)/2, (235+0)/2 rounded up; \
         chroma (128+90)/2, (128+240)/2"
    );
}

#[test]
fn a_wider_picture_is_letterboxed_with_studio_black_bars() {
    // 4×2 into 4×4: full width, height 2. The one bar row each side would put
    // the picture on an odd row, so it rounds to the even offset 0.
    let square = tight(4, 4);
    assert_eq!(FitPlan::new(L_4X2, square).rect(), (0, 0, 4, 2));
    let mut want = FROM_4X2[..8].to_vec(); // luma rows 0-1: the picture
    want.extend([16; 8]); // luma rows 2-3: the bar
    want.extend(&FROM_4X2[8..]); // chroma row 0: the picture's
    want.extend([128; 4]); // chroma row 1: the bar's
    assert_eq!(fitted(&FROM_4X2, L_4X2, square), want);
    // 4×2 into 4×6: two bar rows each side, centred.
    let tall = tight(4, 6);
    assert_eq!(FitPlan::new(L_4X2, tall).rect(), (0, 2, 4, 2));
    let mut want = vec![16; 8];
    want.extend(&FROM_4X2[..8]);
    want.extend([16; 8]);
    want.extend([128; 4]);
    want.extend(&FROM_4X2[8..]);
    want.extend([128; 4]);
    assert_eq!(fitted(&FROM_4X2, L_4X2, tall), want);
}

#[test]
fn a_taller_picture_is_pillarboxed_centred() {
    // 4×2 into 8×2: full height, width 4, centred at x = 2.
    let wide = tight(8, 2);
    assert_eq!(FitPlan::new(L_4X2, wide).rect(), (2, 0, 4, 2));
    assert_eq!(
        fitted(&FROM_4X2, L_4X2, wide),
        vec![
            16, 16, 16, 32, 64, 100, 16, 16, 16, 16, 128, 200, 235, 0, 16, 16, 128, 128, 128, 128,
            90, 240, 128, 128
        ]
    );
}

#[test]
fn an_upscale_interpolates_along_both_axes_and_clamps_at_the_edges() {
    // 4×2 into 8×4: every destination pixel centre between four source ones.
    assert_eq!(FitPlan::new(L_4X2, tight(8, 4)).rect(), (0, 0, 8, 4));
    assert_eq!(
        fitted(&FROM_4X2, L_4X2, tight(8, 4)),
        vec![
            16, 20, 28, 40, 56, 73, 91, 100, 44, 52, 67, 82, 99, 99, 83, 75, 100, 115, 144, 167,
            184, 150, 67, 25, 128, 146, 182, 209, 226, 176, 59, 0, 128, 128, 119, 156, 100, 212,
            90, 240, 128, 128, 119, 156, 100, 212, 90, 240
        ]
    );
}

/// A 4×4 NV12 picture whose luma rows AND whose two chroma rows all differ.
const SRC_4X4: [u8; 24] = [
    10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160, 100, 200, 110, 210, 60,
    150, 70, 160,
];

#[test]
fn a_picture_with_two_chroma_rows_scales_them_like_its_luma() {
    let l_4x4 = tight(4, 4);
    assert_eq!(fitted(&SRC_4X4, l_4x4, l_4x4), SRC_4X4.to_vec(), "identity");
    assert_eq!(
        fitted(&SRC_4X4, l_4x4, tight(2, 2)),
        vec![35, 55, 115, 135, 85, 180],
        "each 2×2 luma block and both chroma rows averaged"
    );
    assert_eq!(
        fitted(&SRC_4X4, l_4x4, tight(8, 8)),
        vec![
            10, 13, 18, 23, 28, 33, 38, 40, 20, 23, 28, 33, 38, 43, 48, 50, 40, 43, 48, 53, 58, 63,
            68, 70, 60, 63, 68, 73, 78, 83, 88, 90, 80, 83, 88, 93, 98, 103, 108, 110, 100, 103,
            108, 113, 118, 123, 128, 130, 120, 123, 128, 133, 138, 143, 148, 150, 130, 133, 138,
            143, 148, 153, 158, 160, 100, 200, 103, 203, 108, 208, 110, 210, 90, 188, 93, 190, 98,
            195, 100, 198, 70, 163, 73, 165, 78, 170, 80, 173, 60, 150, 63, 153, 68, 158, 70, 160
        ],
        "an upscale interpolates between the two chroma rows"
    );
}

#[test]
fn a_downscale_takes_its_second_chroma_tap_from_the_next_pixel_pair() {
    // Review round 1 (mutation `2 * c.i1` → `2 / c.i1`): a 6×2 source has
    // THREE chroma pixels, so the second tap of a destination chroma pixel
    // can be pixel 2 (bytes 4, 5), which only `2 * i1` reaches.
    let src: [u8; 18] = [
        10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 100, 200, 110, 210, 120, 220,
    ];
    let six = tight(6, 2);
    assert_eq!(FitPlan::new(six, tight(4, 2)).rect(), (0, 0, 4, 2));
    assert_eq!(
        fitted(&src, six, tight(4, 2)),
        vec![13, 28, 43, 58, 73, 88, 103, 118, 103, 203, 118, 218]
    );
}

#[test]
fn the_catalogs_resolutions_fit_centred_on_even_offsets() {
    let fit = |sw, sh, dw, dh| FitPlan::new(tight(sw, sh), tight(dw, dh)).rect();
    assert_eq!(
        fit(2560, 1080, 2560, 1440),
        (0, 180, 2560, 1080),
        "21:9 into 16:9: 180-row bars above and below"
    );
    assert_eq!(
        fit(2560, 1440, 2560, 1080),
        (320, 0, 1920, 1080),
        "16:9 into 21:9: 320-column bars left and right"
    );
    assert_eq!(
        fit(1920, 1080, 2560, 1440),
        (0, 0, 2560, 1440),
        "same aspect"
    );
    assert_eq!(
        fit(2560, 1080, 1920, 1080),
        (0, 134, 1920, 810),
        "the 135-row bar rounds to the even offset 134"
    );
}

#[test]
fn a_picture_that_is_not_whole_nv12_fits_as_the_black_canvas_alone() {
    let black = |dst: Layout| {
        let mut out = Vec::new();
        black_nv12_into(dst, &mut out);
        out
    };
    let dst = tight(4, 4);
    assert_eq!(
        fitted(&FROM_4X2[..11], L_4X2, dst),
        black(dst),
        "one byte short of 4×2 NV12"
    );
    let short_stride = Layout {
        width: 4,
        height: 2,
        stride: 3,
        len: 9,
    };
    assert_eq!(
        fitted(&FROM_4X2[..9], short_stride, dst),
        black(dst),
        "a stride shorter than the row"
    );
    let empty = Layout {
        width: 0,
        height: 2,
        stride: 4,
        len: 12,
    };
    assert_eq!(fitted(&FROM_4X2, empty, dst), black(dst), "no width");
    assert_eq!(
        FitPlan::new(empty, dst).rect(),
        (2, 2, 0, 0),
        "a degenerate source: an empty rectangle at the centre"
    );
    let flat = Layout {
        width: 4,
        height: 0,
        stride: 4,
        len: 12,
    };
    assert_eq!(fitted(&FROM_4X2, flat, dst), black(dst), "no height");
    let cut_short = Layout {
        width: 4,
        height: 4,
        stride: 4,
        len: 20,
    };
    assert_eq!(
        fitted(&FROM_4X2, L_4X2, cut_short),
        black(cut_short),
        "a destination buffer too short for its layout: the canvas alone"
    );
}

#[test]
fn a_plan_fits_exactly_its_own_pair_of_layouts() {
    let plan = FitPlan::new(L_4X2, tight(8, 2));
    assert!(plan.fits(L_4X2, tight(8, 2)));
    assert!(!plan.fits(L_4X2, tight(8, 4)), "another destination");
    assert!(!plan.fits(tight(6, 2), tight(8, 2)), "another source");
}

#[test]
fn a_layout_is_read_from_the_sources_job() {
    let job = SubmitJob {
        width: 8,
        height: 6,
        stride: 10,
        video: SharedFrame::new(vec![0u8; 90]),
        audio: Vec::new(),
        video_tc_100ns: T0,
        audio_tc_100ns: T0,
        live: true,
    };
    assert_eq!(
        Layout::of(&job),
        Layout {
            width: 8,
            height: 6,
            stride: 10,
            len: 90
        }
    );
}

#[test]
fn the_crossfade_is_equal_power_at_every_sample_of_a_window() {
    let total = 9 * 1600;
    for j in 0..total {
        let (f, t) = crossfade_gains(j, total);
        let power = f * f + t * t;
        assert!((power - 1.0).abs() < 1e-6, "sample {j}: power {power}");
        assert!(f >= 0.0 && t >= 0.0, "sample {j}: never a phase flip");
    }
}

#[test]
fn the_gain_curve_steps_evenly_across_boundary_edges() {
    let total = 9 * 1600u64;
    let step = FRAC_PI_2 / total as f64;
    let theta = |j: u64| {
        let (c, s) = crossfade_gains(j, total);
        f64::from(s).atan2(f64::from(c))
    };
    for k in 1..9u64 {
        let edge = k * 1600;
        let d = theta(edge) - theta(edge - 1);
        assert!(
            (d - step).abs() < 1e-6,
            "boundary edge {k}: θ steps {d}, not {step}"
        );
    }
    assert!(
        (theta(0) - step / 2.0).abs() < 1e-6,
        "starts next to all-`from`"
    );
    assert!(
        (theta(total - 1) - (FRAC_PI_2 - step / 2.0)).abs() < 1e-6,
        "ends next to all-`to`"
    );
}

#[test]
fn the_gains_are_exact_on_short_windows() {
    let quarter = (FRAC_PI_4.cos() as f32, FRAC_PI_4.sin() as f32);
    assert_eq!(crossfade_gains(0, 1), quarter, "one sample sits at θ = π/4");
    assert_eq!(
        crossfade_gains(0, 2),
        ((PI / 8.0).cos() as f32, (PI / 8.0).sin() as f32)
    );
    assert_eq!(
        crossfade_gains(1, 2),
        ((3.0 * PI / 8.0).cos() as f32, (3.0 * PI / 8.0).sin() as f32)
    );
    assert_eq!(crossfade_gains(0, 0), quarter, "no samples counts as one");
}

/// A stereo block whose every frame differs: frame i = (l + i, r − i).
fn stereo(frames: usize, l: f32, r: f32) -> AudioFrame {
    AudioFrame {
        data: (0..frames)
            .flat_map(|i| [l + i as f32, r - i as f32])
            .collect(),
        channels: 2,
        sample_rate: 48_000,
        timecode_100ns: None,
    }
}

const FMT: AudioFormat = AudioFormat {
    frames: 3,
    channels: 2,
    sample_rate: 48_000,
};

#[test]
fn a_block_crossfades_every_sample_on_the_window_curve() {
    let from = stereo(3, 0.5, -0.5);
    let to = stereo(3, 0.25, 1.0);
    let out = mix_audio_block(Some(&from), Some(&to), 3, 9, FMT);
    assert_eq!((out.channels, out.sample_rate), (2, 48_000));
    assert_eq!(out.timecode_100ns, None);
    assert_eq!(out.data.len(), 6);
    for i in 0..3 {
        let (gf, gt) = crossfade_gains(3 + i as u64, 9);
        let fi = i as f32;
        assert_eq!(out.data[2 * i], gf * (0.5 + fi) + gt * (0.25 + fi), "L {i}");
        assert_eq!(
            out.data[2 * i + 1],
            gf * (-0.5 - fi) + gt * (1.0 - fi),
            "R {i}"
        );
    }
}

#[test]
fn a_mono_side_feeds_both_channels_and_a_missing_side_is_silence() {
    let mono = AudioFrame {
        data: vec![0.1, 0.2, 0.3],
        channels: 1,
        sample_rate: 48_000,
        timecode_100ns: None,
    };
    let out = mix_audio_block(Some(&mono), None, 0, 3, FMT);
    for i in 0..3 {
        let (gf, gt) = crossfade_gains(i as u64, 3);
        let want = gf * mono.data[i] + gt * 0.0;
        assert_eq!(
            (out.data[2 * i], out.data[2 * i + 1]),
            (want, want),
            "frame {i}"
        );
    }
    let to_only = mix_audio_block(None, Some(&mono), 0, 3, FMT);
    let (gf, gt) = crossfade_gains(2, 3);
    assert_eq!(to_only.data[4], gf * 0.0 + gt * 0.3);

    let silence = mix_audio_block(None, None, 0, 3, FMT);
    assert_eq!(silence.data, vec![0.0; 6]);

    let short = stereo(1, 0.5, 0.5);
    let out = mix_audio_block(Some(&short), None, 0, 3, FMT);
    assert_ne!(out.data[0], 0.0, "the frame it has");
    assert_eq!(&out.data[2..], &[0.0; 4], "frames past its end are silence");

    let no_channels = AudioFrame {
        data: vec![1.0; 6],
        channels: 0,
        sample_rate: 48_000,
        timecode_100ns: None,
    };
    assert_eq!(
        mix_audio_block(Some(&no_channels), None, 0, 3, FMT).data,
        vec![0.0; 6]
    );
}

#[test]
fn a_mix_job_names_its_weight_and_its_span_of_the_window() {
    let mix = MixJob {
        stamp_100ns: T0,
        from: None,
        to: None,
        slot: 4,
        n_slots: 9,
    };
    assert_eq!(mix.weight_q8(), 128);
    assert_eq!(mix.sample_span(1600), (6400, 14_400));
    let first = MixJob { slot: 0, ..mix };
    assert_eq!(first.weight_q8(), 14);
    assert_eq!(first.sample_span(3), (0, 27));
}

#[test]
fn an_active_window_reports_its_progress_in_percent() {
    let w = Window::new(
        Some(1),
        2,
        b(3),
        &TransitionSpec::fade(300, SpecSource::Setting),
    );
    assert_eq!(
        ActiveWindow::of(&w, Some(b(6))),
        ActiveWindow {
            from: Some(1),
            to: 2,
            start_boundary_100ns: b(3),
            n_slots: 9,
            served_slots: 4,
            progress: 44,
        }
    );
    assert_eq!(ActiveWindow::of(&w, None).progress, 0);
    assert_eq!(ActiveWindow::of(&w, Some(b(11))).progress, 100);
    let cut = Window::new(None, 2, b(3), &TransitionSpec::cut(SpecSource::Setting));
    assert_eq!(ActiveWindow::of(&cut, Some(b(9))).progress, 0);
}

#[test]
fn a_window_covers_its_slots_until_a_later_cut_truncates_it() {
    let mut w = Window::new(
        Some(1),
        2,
        b(3),
        &TransitionSpec::fade(300, SpecSource::Setting),
    );
    assert_eq!(w.covered(), 9);
    w.truncate(b(6));
    assert_eq!(w.covered(), 3);
    let cut = Window::new(Some(1), 2, b(3), &TransitionSpec::cut(SpecSource::Setting));
    assert_eq!(cut.covered(), 0);
}

#[test]
fn the_black_of_a_layout_is_studio_black_over_its_padded_planes() {
    let black = |width, stride, len| {
        let mut out = vec![9u8];
        black_nv12_into(
            Layout {
                width,
                height: 2,
                stride,
                len,
            },
            &mut out,
        );
        out
    };
    let mut want = vec![9u8];
    want.extend([16; 8]);
    want.extend([128; 4]);
    assert_eq!(black(4, 4, 12), want, "4×2: 8 luma bytes, 4 chroma");
    let mut want = vec![9u8];
    want.extend([16; 12]);
    want.extend([128; 6]);
    assert_eq!(
        black(4, 6, 18),
        want,
        "stride 6: the luma plane is 6 × 2 bytes, padding included"
    );
    let mut want = vec![9u8];
    want.extend([16; 10]);
    assert_eq!(black(4, 6, 10), want, "a short buffer is all luma");
}

#[test]
fn a_truncated_window_reports_its_progress_against_the_boundaries_it_covers() {
    let mut w = Window::new(
        Some(1),
        2,
        b(3),
        &TransitionSpec::fade(300, SpecSource::Setting),
    );
    w.truncate(b(6)); // a later cut: the window covers b(3)..=b(5)
    assert_eq!(ActiveWindow::of(&w, Some(b(4))).progress, 66, "2 of 3");
    let done = ActiveWindow::of(&w, Some(b(5)));
    assert_eq!(
        (done.n_slots, done.served_slots, done.progress),
        (9, 3, 100),
        "served to its end: 100 %, while the curve keeps its 9 slots"
    );
}

#[test]
fn a_fade_waits_for_its_cue_at_most_fifteen_boundaries_and_a_cut_never_waits() {
    assert_eq!(CUE_WAIT_MAX_SLOTS, 15, "CUE_WAIT_MAX = 500 ms at 30 fps");
    let w = Window::cued(
        Some(1),
        2,
        b(3),
        &TransitionSpec::fade(300, SpecSource::Setting),
    );
    assert_eq!(
        w.cue,
        Cue::Waiting {
            deadline_100ns: b(18)
        }
    );
    assert_eq!(
        (w.cut_100ns, w.start_100ns, w.n_slots, w.end_100ns),
        (b(3), b(3), 9, b(27)),
        "until the cue opens it may end as late as 15 + 9 slots after the cut"
    );
    assert!(!w.covers(b(2)), "before the cut");
    assert!(w.covers(b(3)) && w.covers(b(26)));
    assert!(!w.covers(b(27)), "the end is exclusive");
    assert_eq!(w.slot(b(3)), None, "nothing mixes while the cue waits");
    assert_eq!((w.covered(), w.served(Some(b(20)))), (9, 0));
    assert_eq!(ActiveWindow::of(&w, Some(b(20))).progress, 0);

    let cut = Window::cued(Some(1), 2, b(3), &TransitionSpec::cut(SpecSource::Setting));
    assert_eq!(
        (cut.cue, cut.cut_100ns, cut.end_100ns),
        (Cue::Open, b(3), b(3)),
        "a Cut is open at once and mixes nothing"
    );
}

#[test]
fn a_cue_opens_on_its_boundary_and_lays_the_fade_from_there() {
    let w = Window::cued(
        Some(1),
        2,
        b(3),
        &TransitionSpec::fade(300, SpecSource::Setting),
    );
    let mut opened = w;
    assert_eq!(opened.open(b(8)), 5, "it waited b(3)..=b(7)");
    assert_eq!(opened.cue, Cue::Open);
    assert_eq!(
        (opened.cut_100ns, opened.start_100ns, opened.end_100ns),
        (b(3), b(8), b(17))
    );
    assert!(opened.covers(b(3)), "the held boundaries stay the window's");
    assert!(opened.covers(b(16)) && !opened.covers(b(17)));
    assert_eq!(opened.slot(b(7)), None, "held, not mixed");
    assert_eq!(opened.slot(b(8)), Some(0));
    assert_eq!(opened.slot(b(16)), Some(8));
    assert_eq!(opened.slot(b(17)), None);
    assert_eq!(opened.covered(), 9);
    assert_eq!(opened.served(Some(b(7))), 0);
    assert_eq!(opened.served(Some(b(10))), 3);
    assert_eq!(
        ActiveWindow::of(&opened, Some(b(10))),
        ActiveWindow {
            from: Some(1),
            to: 2,
            start_boundary_100ns: b(8),
            n_slots: 9,
            served_slots: 3,
            progress: 33,
        }
    );

    let mut at_once = w;
    assert_eq!(at_once.open(b(3)), 0, "live on the cut boundary");
    assert_eq!((at_once.start_100ns, at_once.end_100ns), (b(3), b(12)));
    let mut timed_out = w;
    assert_eq!(timed_out.open(b(18)), 15, "the deadline");
    assert_eq!((timed_out.start_100ns, timed_out.end_100ns), (b(18), b(27)));
}

#[test]
fn a_later_cut_freezes_a_waiting_cue_but_only_truncates_an_open_one() {
    let w = Window::cued(
        Some(1),
        2,
        b(3),
        &TransitionSpec::fade(300, SpecSource::Setting),
    );
    let mut frozen = w;
    frozen.truncate(b(6));
    assert_eq!((frozen.cue, frozen.end_100ns), (Cue::Frozen, b(6)));
    assert!(frozen.covers(b(5)) && !frozen.covers(b(6)));
    assert_eq!(frozen.slot(b(4)), None, "it never mixes");
    assert_eq!(
        (frozen.covered(), frozen.served(Some(b(20)))),
        (0, 0),
        "a frozen cue covers no mixed boundary"
    );

    let mut open = w;
    open.open(b(4));
    open.truncate(b(8));
    assert_eq!((open.cue, open.end_100ns), (Cue::Open, b(8)));
    assert_eq!((open.covered(), open.served(Some(b(20)))), (4, 4));
}

#[test]
fn a_window_holds_its_outgoing_source_from_its_cut_to_its_end_while_its_cue_does_not_run() {
    // #215 review rounds 4 + 5: the ONE predicate behind `on_air` and the
    // freeze. The waiting fade: cut b(3), latest end b(3 + 15 + 9) = b(27).
    let waiting = Window::cued(
        Some(1),
        2,
        b(3),
        &TransitionSpec::fade(300, SpecSource::Setting),
    );
    assert!(!waiting.holds_on_air(b(2)), "a cut before it replaces it");
    assert!(waiting.holds_on_air(b(3)), "a same-slot re-cut");
    assert!(waiting.holds_on_air(b(27)), "a cut on its latest end");
    assert!(!waiting.holds_on_air(b(28)), "past its latest end");
    let mut open = waiting;
    open.open(b(4));
    assert!(
        !open.holds_on_air(b(5)),
        "an open fade mixes, it holds nothing"
    );

    let mut frozen = waiting;
    assert!(frozen.truncate(b(6)), "a cut inside its span freezes it");
    assert!(frozen.holds_on_air(b(6)), "frozen, ending on the boundary");
    assert!(!frozen.truncate(b(5)), "already frozen: only truncated");
    assert_eq!((frozen.cue, frozen.end_100ns), (Cue::Frozen, b(5)));

    let mut on_end = waiting;
    assert!(on_end.truncate(b(27)), "a cut on its latest end freezes it");
    assert_eq!(on_end.cue, Cue::Frozen);
    let mut late = waiting;
    assert!(
        !late.truncate(b(28)),
        "a cut after its latest end leaves it waiting"
    );
    assert_eq!((late.cue, late.end_100ns), (waiting.cue, b(27)));

    let mut running = open;
    assert!(!running.truncate(b(8)), "an open fade is only truncated");
    assert_eq!((running.cue, running.end_100ns), (Cue::Open, b(8)));
}
