//! Unit tests for the preview stream tap (#178) — geometry, letterbox blit,
//! viewer gating. All Linux-runnable (synthetic NV12, no ffmpeg child).

use super::*;

/// Solid NV12 frame of `sw×sh` (stride ≥ sw) with luma `y` and chroma `c`. The
/// logical width `_sw` is documentation only — the buffer is sized by `stride`.
fn solid_nv12(_sw: usize, sh: usize, stride: usize, y: u8, c: u8) -> Vec<u8> {
    let y_size = stride * sh;
    let uv_size = stride * (sh / 2);
    let mut v = vec![0u8; y_size + uv_size];
    v[..y_size].fill(y);
    v[y_size..].fill(c);
    v
}

#[test]
fn placement_16by9_fills_canvas_exactly() {
    let p = placement_for(1920, 1080);
    assert_eq!(
        p,
        Placement {
            w: 640,
            h: 360,
            off_x: 0,
            off_y: 0
        }
    );
    // 1440p is also 16:9.
    assert_eq!(
        placement_for(2560, 1440),
        Placement {
            w: 640,
            h: 360,
            off_x: 0,
            off_y: 0
        }
    );
}

#[test]
fn placement_4by3_letterboxes_with_side_bars() {
    // 1440×1080 (4:3): width-limited by height → 480×360, centred → 80px bars.
    let p = placement_for(1440, 1080);
    assert_eq!(
        p,
        Placement {
            w: 480,
            h: 360,
            off_x: 80,
            off_y: 0
        }
    );
}

#[test]
fn placement_tall_source_letterboxes_top_and_bottom() {
    // 720×1280 (9:16 portrait): height-limited → very narrow, full height.
    let p = placement_for(720, 1280);
    // h_if_full_w = 1280*640/720 = 1137 > 360 → height-limited:
    // w = 720*360/1280 = 202 → floor even 202, h = 360.
    assert_eq!(p.h, 360);
    assert_eq!(p.w, 202);
    assert_eq!(p.off_x, ((640 - 202) / 2) & !1);
    assert_eq!(p.off_y, 0);
}

#[test]
fn placement_is_always_even_aligned_and_within_canvas() {
    for &(w, h) in &[
        (1000u32, 999u32),
        (333, 777),
        (1, 1),
        (639, 361),
        (641, 359),
    ] {
        let p = placement_for(w, h);
        assert_eq!(p.w % 2, 0, "w even for {w}x{h}");
        assert_eq!(p.h % 2, 0, "h even for {w}x{h}");
        assert_eq!(p.off_x % 2, 0, "off_x even for {w}x{h}");
        assert_eq!(p.off_y % 2, 0, "off_y even for {w}x{h}");
        assert!(p.off_x + p.w <= OUT_W, "fits width {w}x{h}");
        assert!(p.off_y + p.h <= OUT_H, "fits height {w}x{h}");
    }
}

#[test]
fn placement_ultrawide_letterboxes_top_and_bottom() {
    // 3840×1080 (32:9): width-limited → full 640 wide, short → top/bottom bars.
    // w = min(640, 3840*360/1080=1280) = 640; h = min(360, 1080*640/3840=180) = 180.
    let p = placement_for(3840, 1080);
    assert_eq!(
        p,
        Placement {
            w: 640,
            h: 180,
            off_x: 0,
            off_y: 90
        }
    );
}

#[test]
fn placement_extreme_aspect_clamps_to_min_2px() {
    // Degenerate-but-nonzero aspects round one axis to 0 before the `.max(2)`
    // floor — the result must never be a zero/one-pixel image.
    assert_eq!(placement_for(1, 720).w, 2, "ultra-tall clamps width to 2");
    assert_eq!(
        placement_for(641, 1).h,
        2,
        "ultra-wide-thin clamps height to 2"
    );
}

#[test]
fn placement_degenerate_source_is_zero_image() {
    assert_eq!(placement_for(0, 100).w, 0);
    assert_eq!(placement_for(100, 0).h, 0);
}

#[test]
fn letterbox_16by9_fills_whole_canvas_with_the_image() {
    // A mid-grey 16:9 source → every luma pixel is the source luma (no bars).
    let src = solid_nv12(1920, 1080, 1920, 200, 128);
    let mut dst = vec![0u8; OUT_NV12_LEN];
    letterbox_nv12_into(1920, 1080, 1920, &src, &mut dst);
    let y_plane = (OUT_W * OUT_H) as usize;
    assert!(
        dst[..y_plane].iter().all(|&b| b == 200),
        "no black bars for 16:9"
    );
}

#[test]
fn letterbox_4by3_paints_black_side_bars_and_grey_centre() {
    // 4:3 grey source → 80px black bars left/right, grey in the centre columns.
    let src = solid_nv12(1440, 1080, 1440, 200, 128);
    let mut dst = vec![0u8; OUT_NV12_LEN];
    letterbox_nv12_into(1440, 1080, 1440, &src, &mut dst);
    let ow = OUT_W as usize;
    // Row 180 (mid): left edge is a black bar, centre is the image, right edge bar.
    let row = 180 * ow;
    assert_eq!(dst[row], BLACK_Y, "left black bar (x=0)");
    assert_eq!(dst[row + ow / 2], 200, "image centre (x=320)");
    assert_eq!(dst[row + ow - 1], BLACK_Y, "right black bar (x=639)");
    // Column just inside the image (x=80) is grey; x=79 is still a bar.
    assert_eq!(dst[row + 80], 200, "first image column (x=80)");
    assert_eq!(dst[row + 79], BLACK_Y, "last bar column (x=79)");
}

#[test]
fn letterbox_rejects_short_source_leaving_black_canvas() {
    let mut dst = vec![7u8; OUT_NV12_LEN]; // pre-dirty
    // Declares 1920x1080 but supplies far too few bytes → canvas painted black,
    // no image blitted.
    letterbox_nv12_into(1920, 1080, 1920, &[0u8; 100], &mut dst);
    let y_plane = (OUT_W * OUT_H) as usize;
    assert!(dst[..y_plane].iter().all(|&b| b == BLACK_Y));
    assert!(dst[y_plane..].iter().all(|&b| b == NEUTRAL_C));
}

#[test]
fn offer_video_is_a_noop_with_no_viewer() {
    let tap = StreamTap::new("t".into(), 0);
    let src = solid_nv12(64, 48, 64, 128, 128);
    // No viewer → nothing queued for the feeder.
    tap.try_offer_video(64, 48, 64, &src);
    assert!(
        tap.shared().video_receiver().try_recv().is_err(),
        "no frame queued without a viewer"
    );
}

#[test]
fn offer_video_queues_a_fixed_canvas_frame_with_a_viewer() {
    let tap = StreamTap::new("t".into(), 0);
    let (_guard, _relay) = ViewerGuard::subscribe(&tap);
    assert!(tap.shared().has_viewer());
    let src = solid_nv12(1920, 1080, 1920, 128, 128);
    tap.try_offer_video(1920, 1080, 1920, &src);
    let frame = tap
        .shared()
        .video_receiver()
        .try_recv()
        .expect("a frame is queued while watched");
    assert_eq!(frame.len(), OUT_NV12_LEN, "always the fixed 640x360 canvas");
}

#[test]
fn offer_audio_is_a_noop_with_no_viewer_and_queues_with_one() {
    let tap = StreamTap::new("t".into(), 0);
    let block = [0.1f32, -0.1, 0.2, -0.2];
    tap.try_offer_audio(&block, 48_000, 2);
    assert!(
        tap.shared().audio_receiver().try_recv().is_err(),
        "no viewer, no audio"
    );

    let (_g, _r) = ViewerGuard::subscribe(&tap);
    tap.try_offer_audio(&block, 48_000, 2);
    let got = tap
        .shared()
        .audio_receiver()
        .try_recv()
        .expect("audio queued");
    assert_eq!(got, block.to_vec());
}

#[test]
fn viewer_guard_counts_up_and_saturates_down() {
    let tap = StreamTap::new("t".into(), 0);
    assert!(!tap.shared().has_viewer());
    let g1 = ViewerGuard::subscribe(&tap).0;
    let g2 = ViewerGuard::subscribe(&tap).0;
    assert_eq!(
        tap.shared()
            .viewers
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );
    drop(g1);
    assert!(tap.shared().has_viewer());
    drop(g2);
    assert!(
        !tap.shared().has_viewer(),
        "last viewer gone → offers early-out again"
    );
}

#[test]
fn try_claim_encoder_admits_exactly_one() {
    let tap = StreamTap::new("t".into(), 0);
    assert!(tap.shared().try_claim_encoder(), "first claim wins");
    assert!(!tap.shared().try_claim_encoder(), "second claim blocked");
    tap.shared().release_encoder();
    assert!(
        tap.shared().try_claim_encoder(),
        "claimable again after release"
    );
}

#[test]
fn shared_label_is_the_name_the_tap_was_built_with() {
    // The label names the encoder thread + every encoder log line.
    let tap = StreamTap::new("playlist-7".into(), 0);
    assert_eq!(tap.shared().label(), "playlist-7");
}

#[test]
fn shared_reports_its_configured_lead_ms() {
    // The getter must return the constructed lead verbatim — the encoder child
    // reads it to place `-itsoffset`. Exact values kill a "return 0"/"return 1"
    // mutant on the getter.
    assert_eq!(StreamTap::new("t".into(), 100).shared().lead_ms(), 100);
    assert_eq!(StreamTap::new("t".into(), 0).shared().lead_ms(), 0);
}

#[test]
fn lead_ms_for_sdk_path_is_the_emitter_lookahead() {
    // genlock_pacing == false → the #192 wall-clock emitter is present → the
    // decoder opens with the AUDIO_LOOKAHEAD_MS audio read-ahead (1500 ms since
    // #192 round 3) → lead = (40 + lookahead) − 40 = lookahead. Pinned to the
    // SAME constant the emitter uses so the preview preroll can never drift
    // from the cushion; the exact value kills the `!` delete, the `- → +` and
    // the `- → /` mutants on the pure formula.
    let lookahead = crate::playback::pipeline::audio_emitter::AUDIO_LOOKAHEAD_MS as u32;
    assert_eq!(lookahead, 1500);
    assert_eq!(lead_ms_for(false), lookahead);
}

#[test]
fn lead_ms_for_paced_path_has_no_lead() {
    // genlock_pacing == true → no emitter, plain 40 ms pairing → lead 0.
    assert_eq!(lead_ms_for(true), 0);
}

#[test]
fn a_viewer_guard_dropped_at_zero_viewers_stays_at_zero() {
    let tap = StreamTap::new("t".into(), 0);
    let guard = ViewerGuard::subscribe(&tap).0;
    // Force the count to 0 behind the guard's back; its drop must saturate.
    tap.shared()
        .viewers
        .store(0, std::sync::atomic::Ordering::Relaxed);
    drop(guard);
    assert_eq!(
        tap.shared()
            .viewers
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[test]
fn offer_frame_feeds_the_stream_video_and_audio_taps() {
    let stream = StreamTap::new("t".into(), 0);
    let taps = DecodeTaps {
        preview: crate::playback::preview::PreviewTap::new(Default::default(), "t".into()),
        stream: stream.clone(),
    };
    let (_guard, _relay) = ViewerGuard::subscribe(&stream);
    let video = sp_decoder::DecodedVideoFrame {
        data: solid_nv12(64, 48, 64, 128, 128),
        width: 64,
        height: 48,
        stride: 64,
        timestamp_ms: 0,
        pixel_format: sp_decoder::PixelFormat::Nv12,
    };
    let audio = [sp_decoder::DecodedAudioFrame {
        data: vec![0.25f32, -0.25, 0.5, -0.5],
        channels: 2,
        sample_rate: 48_000,
        timestamp_ms: 0,
    }];
    taps.offer_frame(&video, &audio);
    let frame = stream
        .shared()
        .video_receiver()
        .try_recv()
        .expect("the video tap got the frame");
    assert_eq!(frame.len(), OUT_NV12_LEN);
    let got = stream
        .shared()
        .audio_receiver()
        .try_recv()
        .expect("the audio tap got the block");
    assert_eq!(got, vec![0.25f32, -0.25, 0.5, -0.5]);
}

#[test]
fn to_stereo_upmixes_mono_by_duplicating_each_sample() {
    // Mono → stereo: each sample becomes an L,R pair (correct speed, not double).
    assert_eq!(
        to_stereo(&[1.0, 2.0, 3.0], 1),
        Some(vec![1.0, 1.0, 2.0, 2.0, 3.0, 3.0])
    );
    // The interleaved length is exactly doubled.
    assert_eq!(to_stereo(&[0.5f32; 4], 1).map(|b| b.len()), Some(8));
}

#[test]
fn to_stereo_forwards_stereo_verbatim() {
    assert_eq!(
        to_stereo(&[1.0, -1.0, 0.5, -0.5], 2),
        Some(vec![1.0, -1.0, 0.5, -0.5])
    );
    // An empty stereo block stays empty (never panics).
    assert_eq!(to_stereo(&[], 2), Some(vec![]));
}

#[test]
fn to_stereo_drops_unexpected_channel_counts() {
    // Exact-boundary around the two accepted counts (1 and 2): everything else
    // is dropped rather than mis-fed into the fixed-stereo child input.
    assert_eq!(to_stereo(&[1.0, 2.0, 3.0], 3), None);
    assert_eq!(to_stereo(&[1.0], 0), None);
    assert_eq!(to_stereo(&[1.0f32; 6], 6), None);
}

#[test]
fn audio_preroll_samples_is_the_interleaved_stereo_silence_count() {
    // (min(gap,5000)+lead_ms) ms of silence, 48 kHz stereo = 48*2 samples/ms.
    // No preroll at all when neither the video nor the emitter is ahead.
    assert_eq!(audio_preroll_samples(0, 0), 0);
    // The canonical case from the design: a 250 ms connect gap + the 100 ms
    // decode-seam lead → 350 ms → 350 * 48 * 2 interleaved f32 samples.
    assert_eq!(audio_preroll_samples(250, 100), 33_600);
    // Lead alone (paced path has 0), no connect gap.
    assert_eq!(audio_preroll_samples(0, 100), 9_600);
    // Connect gap alone (SDK path could be paced=false but a huge gap).
    assert_eq!(audio_preroll_samples(250, 0), 24_000);
}

#[test]
fn audio_preroll_samples_caps_the_connect_gap_at_5s() {
    // A late-connecting audio input can never prepend more than 5 s (+lead) of
    // silence. Exact boundary at the 5000 ms cap and well past it.
    assert_eq!(audio_preroll_samples(5_000, 0), 480_000);
    assert_eq!(audio_preroll_samples(5_001, 0), 480_000);
    assert_eq!(audio_preroll_samples(9_000, 100), 489_600);
    // The lead is NOT capped — only the connect gap is.
    assert_eq!(audio_preroll_samples(9_000, 0), 480_000);
}

// ── #178 item 15: preview audio-continuity gap fill ──────────────────────────

#[test]
fn gap_fill_is_zero_at_or_below_150ms_and_fills_above() {
    // In sync (0 frames, 0 ms wall) → no gap.
    assert_eq!(gap_fill_samples(0, 0), 0);
    // Exactly 150 ms behind → still at the threshold, no fill.
    assert_eq!(gap_fill_samples(150, 0), 0);
    // 151 ms behind → fills 151 ms of interleaved-stereo silence (151*48*2).
    assert_eq!(gap_fill_samples(151, 0), 151 * 48 * 2);
    // Audio has kept up with the wall clock → no gap.
    // 48_000 frames written = 1000 ms; wall 1000 ms → gap 0.
    assert_eq!(gap_fill_samples(1000, 48_000), 0);
    // A small drift under the threshold does not fill: 48_000 frames = 1000 ms,
    // wall 1100 ms → 100 ms gap ≤ 150.
    assert_eq!(gap_fill_samples(1100, 48_000), 0);
    // Just over: wall 1151 ms → 151 ms gap → fill.
    assert_eq!(gap_fill_samples(1151, 48_000), 151 * 48 * 2);
}

#[test]
fn gap_fill_is_capped_at_10s_per_gap() {
    // Exactly 10 s gap fills 10 s.
    assert_eq!(gap_fill_samples(10_000, 0), 10_000 * 48 * 2);
    // 10 s + 1 ms is capped to 10 s.
    assert_eq!(gap_fill_samples(10_001, 0), 10_000 * 48 * 2);
    // A 30 s gap still only fills 10 s.
    assert_eq!(gap_fill_samples(30_000, 0), 10_000 * 48 * 2);
}
