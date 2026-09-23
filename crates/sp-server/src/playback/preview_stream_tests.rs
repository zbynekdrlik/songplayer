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
fn relay_capacity_is_four_fragments() {
    // #184 round F: the broadcast backlog is 4 fragments (= 2 s at
    // -frag_duration 500000), NOT 64 (= 32 s). A slow remote viewer drops
    // fragments and resyncs on the next keyframe-aligned fragment instead of
    // sitting a full 32 s behind the wall. Exact value kills any off-by-N mutant.
    assert_eq!(RELAY_CAPACITY, 4);
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

// ── #184 round G2: the preview audio is kept on the wall clock BOTH ways ─────

/// Stereo frames per millisecond at 48 kHz (test-side literal, so a mutant of
/// the production constant cannot hide behind the same expression).
const F: u64 = 48;

#[test]
fn align_constants_are_the_design_values() {
    // #184 G2 design comment 5802429990: pad when > 150 ms behind, trim a
    // burst that would run > 300 ms ahead back to 100 ms ahead.
    assert_eq!(ALIGN_PAD_THRESHOLD_MS, 150);
    assert_eq!(MAX_AHEAD_MS, 300);
    assert_eq!(ALIGN_TARGET_AHEAD_MS, 100);
    assert_eq!(PREVIEW_AUDIO_FRAMES_PER_MS, 48);
}

#[test]
fn align_block_on_time_block_is_written_verbatim() {
    // 20 ms behind the wall, a 20 ms block arrives → nothing padded, nothing cut.
    let a = align_block(1_000 * F, 980 * F, (20 * F) as usize);
    assert_eq!(
        a,
        AlignAction {
            pad_frames: 0,
            skip_frames: 0
        }
    );
    // The very first block of a fresh feeder (wall 0, written 0).
    let a = align_block(0, 0, 960);
    assert_eq!(
        a,
        AlignAction {
            pad_frames: 0,
            skip_frames: 0
        }
    );
    // Slightly AHEAD of the wall (the previous block overshot): no pad, and the
    // result stays under the 300 ms bound → verbatim.
    let a = align_block(1_000 * F, 1_000 * F + 1, 960);
    assert_eq!(
        a,
        AlignAction {
            pad_frames: 0,
            skip_frames: 0
        }
    );
}

#[test]
fn align_block_late_block_is_padded_up_to_the_wall_first() {
    // 1 s behind → 1 s of silence, then the block (a gap in the source).
    let a = align_block(2_000 * F, 1_000 * F, 960);
    assert_eq!(
        a,
        AlignAction {
            pad_frames: (1_000 * F) as usize,
            skip_frames: 0
        }
    );
}

#[test]
fn align_block_pad_threshold_is_exclusive_at_150ms() {
    // Exactly 150 ms behind → at the threshold, no pad.
    let a = align_block(1_000 * F, 850 * F, 960);
    assert_eq!(a.pad_frames, 0);
    // One frame more → pad the whole gap up to the wall.
    let a = align_block(1_000 * F, 850 * F - 1, 960);
    assert_eq!(a.pad_frames, (150 * F + 1) as usize);
    assert_eq!(a.skip_frames, 0);
}

#[test]
fn align_block_burst_is_trimmed_so_it_lands_100ms_ahead() {
    // The late-burst case that used to accumulate the ~70 s owner lag: the
    // feeder is on the wall and a 1 s catch-up burst arrives at once. The
    // OLDEST 900 ms are dropped so written ends at wall + 100 ms.
    let wall = 5_000 * F;
    let a = align_block(wall, wall, (1_000 * F) as usize);
    assert_eq!(
        a,
        AlignAction {
            pad_frames: 0,
            skip_frames: (900 * F) as usize
        }
    );
    let written = wall + a.pad_frames as u64 + (1_000 * F) - a.skip_frames as u64;
    assert_eq!(written, wall + 100 * F);
}

#[test]
fn align_block_skip_threshold_is_exclusive_at_300ms_ahead() {
    let wall = 5_000 * F;
    // A block that ends exactly 300 ms ahead is written verbatim.
    let a = align_block(wall, wall, (300 * F) as usize);
    assert_eq!(
        a,
        AlignAction {
            pad_frames: 0,
            skip_frames: 0
        }
    );
    // One frame more → trimmed back to 100 ms ahead: skip = 200 ms + 1 frame.
    let a = align_block(wall, wall, (300 * F + 1) as usize);
    assert_eq!(
        a,
        AlignAction {
            pad_frames: 0,
            skip_frames: (200 * F + 1) as usize
        }
    );
}

#[test]
fn align_block_skip_never_exceeds_the_block() {
    let wall = 5_000 * F;
    // Already 290 ms ahead, a 20 ms block would end 310 ms ahead: the target
    // (100 ms ahead) is BEHIND where we already are, so the whole block is
    // dropped — never more than the block, never a negative write.
    let a = align_block(wall, wall + 290 * F, (20 * F) as usize);
    assert_eq!(
        a,
        AlignAction {
            pad_frames: 0,
            skip_frames: (20 * F) as usize
        }
    );
    // An empty block never skips anything.
    let a = align_block(wall, wall + 400 * F, 0);
    assert_eq!(
        a,
        AlignAction {
            pad_frames: 0,
            skip_frames: 0
        }
    );
}

#[test]
fn align_block_pads_then_trims_a_late_burst() {
    // 1 s behind AND a 500 ms burst: pad 1 s up to the wall, then the burst
    // would end 500 ms ahead → drop its oldest 400 ms (lands 100 ms ahead).
    let wall = 5_000 * F;
    let a = align_block(wall, wall - 1_000 * F, (500 * F) as usize);
    assert_eq!(
        a,
        AlignAction {
            pad_frames: (1_000 * F) as usize,
            skip_frames: (400 * F) as usize
        }
    );
}

#[test]
fn align_timeout_pads_up_to_the_wall_when_no_block_arrived() {
    // #184 G2: with NO block for 200 ms the feeder still writes silence up to
    // the wall — the encoder (which interleaves by timestamp) is never starved.
    assert_eq!(align_timeout(1_000 * F, 800 * F), (200 * F) as usize);
    assert_eq!(align_timeout(60_000 * F, 0), (60_000 * F) as usize);
    // On the wall / ahead of it → nothing.
    assert_eq!(align_timeout(1_000 * F, 1_000 * F), 0);
    assert_eq!(align_timeout(1_000 * F, 1_100 * F), 0);
    assert_eq!(align_timeout(0, 0), 0);
    // Exclusive 150 ms threshold, same as a block.
    assert_eq!(align_timeout(1_000 * F, 850 * F), 0);
    assert_eq!(
        align_timeout(1_000 * F, 850 * F - 1),
        (150 * F + 1) as usize
    );
}

/// Tiny deterministic LCG (no rand dependency) for the property-style loop.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
}

#[test]
fn align_keeps_written_within_300ms_of_the_wall_over_1000_mixed_steps() {
    // Property-style: 1000 steps alternating on-time blocks, late blocks (the
    // source stalled), catch-up bursts and 200 ms timeouts. After EVERY step
    // |written − wall| ≤ 300 ms — the add-only gap fill let this grow by every
    // hiccup (~70 s on the box); the aligner bounds it forever.
    let mut rng = Lcg(0x5eed_0184);
    let mut wall: u64 = 0;
    let mut written: u64 = 0;
    let bound = (300 * F) as i64;
    let (mut padded, mut skipped) = (0u64, 0u64);
    for step in 0..1000u64 {
        let kind = (step + rng.next() % 2) % 4;
        match kind {
            // On-time: the wall advances by exactly the block's duration.
            0 => {
                let b = rng.range(10, 50) * F;
                wall += b;
                let a = align_block(wall, written, b as usize);
                assert!(a.skip_frames as u64 <= b);
                written += a.pad_frames as u64 + b - a.skip_frames as u64;
                padded += a.pad_frames as u64;
                skipped += a.skip_frames as u64;
            }
            // Late: the source stalled 160 ms–1.5 s, then one block.
            1 => {
                wall += rng.range(160, 1_500) * F;
                let b = rng.range(10, 50) * F;
                let a = align_block(wall, written, b as usize);
                assert!(a.skip_frames as u64 <= b);
                written += a.pad_frames as u64 + b - a.skip_frames as u64;
                padded += a.pad_frames as u64;
                skipped += a.skip_frames as u64;
            }
            // Burst: 0.4–3 s of backlogged audio arrives at (almost) once.
            2 => {
                wall += rng.range(0, 10) * F;
                let b = rng.range(400, 3_000) * F;
                let a = align_block(wall, written, b as usize);
                assert!(a.skip_frames as u64 <= b);
                written += a.pad_frames as u64 + b - a.skip_frames as u64;
                padded += a.pad_frames as u64;
                skipped += a.skip_frames as u64;
            }
            // Timeout: 200 ms with no block.
            _ => {
                wall += 200 * F;
                let p = align_timeout(wall, written);
                written += p as u64;
                padded += p as u64;
            }
        }
        let diff = written as i64 - wall as i64;
        assert!(
            diff.abs() <= bound,
            "step {step} (kind {kind}): written − wall = {} ms exceeds ±300 ms",
            diff / F as i64
        );
    }
    // The loop really exercised both directions.
    assert!(padded > 0, "no step padded");
    assert!(skipped > 0, "no step trimmed a burst");
}
