//! Unit tests for the preview stream tap (#178) — geometry, letterbox blit,
//! viewer gating. All Linux-runnable (synthetic NV12, no ffmpeg child).

use super::*;

/// Solid NV12 frame of `sw×sh` (stride ≥ sw) with luma `y` and chroma `c`.
fn solid_nv12(sw: usize, sh: usize, stride: usize, y: u8, c: u8) -> Vec<u8> {
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
    let tap = StreamTap::new("t".into());
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
    let tap = StreamTap::new("t".into());
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
    let tap = StreamTap::new("t".into());
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
    let tap = StreamTap::new("t".into());
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
    let tap = StreamTap::new("t".into());
    assert!(tap.shared().try_claim_encoder(), "first claim wins");
    assert!(!tap.shared().try_claim_encoder(), "second claim blocked");
    tap.shared().release_encoder();
    assert!(
        tap.shared().try_claim_encoder(),
        "claimable again after release"
    );
}
