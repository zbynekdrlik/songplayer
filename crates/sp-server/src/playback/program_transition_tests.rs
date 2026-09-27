//! #215: the pure transition layer — the spec (OBS / override / fallback), the
//! window's grid indices, the Q8 picture weight + NV12 blend, the equal-power
//! gain curve and the audio mix. Exact values, so every arithmetic mutant dies.
//! Wired via `#[cfg(test)] #[path = "program_transition_tests.rs"] mod tests;`.

use super::*;
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

fn obs(kind: &str, duration_ms: Option<u32>) -> ObsTransition {
    ObsTransition {
        name: "Prechod".to_string(),
        kind: kind.to_string(),
        duration_ms,
    }
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
    let fade = TransitionSpec::fade(300, SpecSource::Obs);
    assert_eq!(
        (fade.kind, fade.duration_ms, fade.n_slots, fade.source),
        (TransitionKind::Fade, 300, 9, SpecSource::Obs)
    );
    let cut = TransitionSpec::cut(SpecSource::Setting);
    assert_eq!(
        (cut.kind, cut.duration_ms, cut.n_slots, cut.source),
        (TransitionKind::Cut, 0, 0, SpecSource::Setting)
    );
}

#[test]
fn the_duration_and_kind_come_from_obs() {
    let fade = spec_from_obs(&obs("fade_transition", Some(300)), 500);
    assert_eq!(fade, TransitionSpec::fade(300, SpecSource::Obs));
    assert_eq!(fade.n_slots, 9, "fade 300 ms → 9 slots");
    let cut = spec_from_obs(&obs("cut_transition", Some(300)), 500);
    assert_eq!(cut, TransitionSpec::cut(SpecSource::Obs));
    assert_eq!(cut.n_slots, 0, "cut → 0");
    assert_eq!(
        spec_from_obs(&obs("swipe_transition", Some(700)), 500),
        TransitionSpec::fade(700, SpecSource::Obs),
        "any other kind is a fade of its duration"
    );
    assert_eq!(
        spec_from_obs(&obs("obs_stinger_transition", None), 500),
        TransitionSpec::fade(500, SpecSource::Obs),
        "a fixed-duration transition fades for the setting's length"
    );
}

#[test]
fn the_override_wins_and_an_unknown_obs_transition_falls_back_to_the_settings_fade() {
    let cut = obs("cut_transition", None);
    assert_eq!(
        effective_spec(TransitionMode::Obs, 400, Some(&cut)),
        TransitionSpec::cut(SpecSource::Obs)
    );
    assert_eq!(
        effective_spec(TransitionMode::Obs, 400, None),
        TransitionSpec::fade(400, SpecSource::Fallback)
    );
    assert_eq!(
        effective_spec(TransitionMode::Fade, 400, Some(&cut)),
        TransitionSpec::fade(400, SpecSource::Setting)
    );
    assert_eq!(
        effective_spec(TransitionMode::Cut, 400, None),
        TransitionSpec::cut(SpecSource::Setting)
    );
}

#[test]
fn the_transition_setting_parses_with_obs_as_the_default() {
    assert_eq!(TransitionMode::parse(Some("fade")), TransitionMode::Fade);
    assert_eq!(TransitionMode::parse(Some(" cut ")), TransitionMode::Cut);
    assert_eq!(TransitionMode::parse(Some("obs")), TransitionMode::Obs);
    assert_eq!(TransitionMode::parse(Some("wipe")), TransitionMode::Obs);
    assert_eq!(TransitionMode::parse(None), TransitionMode::Obs);
}

#[test]
fn a_window_spans_exactly_its_slots_and_is_served_boundary_by_boundary() {
    let w = Window::new(
        Some(1),
        2,
        b(3),
        &TransitionSpec::fade(300, SpecSource::Obs),
    );
    assert_eq!((w.from, w.to, w.kind), (Some(1), 2, TransitionKind::Fade));
    assert_eq!((w.start_100ns, w.n_slots, w.end_100ns), (b(3), 9, b(12)));
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

fn blend(weight: u32) -> Vec<u8> {
    let mut out = Vec::new();
    blend_nv12_into(&FROM_4X2, &TO_4X2, weight, &mut out);
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

fn layout(width: u32, stride: u32, len: usize) -> Layout {
    Layout {
        width,
        height: 2,
        stride,
        len,
    }
}

#[test]
fn equal_layouts_blend_and_different_ones_cut_at_the_midpoint() {
    let a = layout(4, 4, 12);
    let wide = layout(8, 8, 24);
    assert_eq!(picture_mix(a, a, 14), Picture::Blend);
    assert_eq!(picture_mix(a, wide, 0), Picture::From);
    assert_eq!(picture_mix(a, wide, 127), Picture::From);
    assert_eq!(
        picture_mix(a, wide, 128),
        Picture::To,
        "from the midpoint on"
    );
    assert_eq!(picture_mix(a, wide, 242), Picture::To);
    assert_eq!(
        picture_mix(a, layout(4, 6, 12), 200),
        Picture::To,
        "a different stride alone never blends"
    );
    assert_eq!(picture_mix(a, layout(4, 4, 13), 1), Picture::From);
}

#[test]
fn a_size_cut_is_logged_once_per_run() {
    assert!(starts_size_cut(false, Picture::From));
    assert!(starts_size_cut(false, Picture::To));
    assert!(!starts_size_cut(true, Picture::From));
    assert!(!starts_size_cut(true, Picture::To));
    assert!(!starts_size_cut(false, Picture::Blend));
    assert!(!starts_size_cut(true, Picture::Blend));
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
        &TransitionSpec::fade(300, SpecSource::Obs),
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
    let cut = Window::new(None, 2, b(3), &TransitionSpec::cut(SpecSource::Obs));
    assert_eq!(ActiveWindow::of(&cut, Some(b(9))).progress, 0);
}

#[test]
fn a_window_covers_its_slots_until_a_later_cut_truncates_it() {
    let mut w = Window::new(
        Some(1),
        2,
        b(3),
        &TransitionSpec::fade(300, SpecSource::Obs),
    );
    assert_eq!(w.covered(), 9);
    w.truncate(b(6));
    assert_eq!(w.covered(), 3);
    let cut = Window::new(Some(1), 2, b(3), &TransitionSpec::cut(SpecSource::Obs));
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
        &TransitionSpec::fade(300, SpecSource::Obs),
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
