//! #282: the reader's decode cap on the H.264 fixture (160×120 testsrc2,
//! 30 fps), on the software path.
//!
//! `windows-latest` has no GPU and its WARP refuses the video device, so no
//! reader here reaches the D3D11 path (`mf_hw_decode.rs`): the GPU video
//! processor's scale is proven on the box only. What runs here is the same
//! open — a first source reader read for the stream's own size, then a
//! second one with `MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING` and
//! the NV12 output type's `MF_MT_FRAME_SIZE` (+ the pixel aspect that keeps
//! the display aspect and the native colour description; no frame rate) —
//! with Media Foundation's video processor scaling on the CPU.
//!
//! The picture checks compare the scaled picture with the unscaled one by
//! regions, with tolerances measured on this fixture (ffmpeg's bilinear,
//! bicubic, area, lanczos and nearest scalers standing in for the video
//! processor, against a crop, a 1–2 px letterbox or pillarbox, a
//! limited→full range conversion, a BT.601→709 matrix change, 8 padding
//! rows scaled in and a 2-row shift; review round 1):
//!
//! - luma means of a 4×4 grid (tolerance 6): a correct scale ≤ 4.95
//!   (ffmpeg's fast_bilinear; the others ≤ 2.9), a defect ≥ 7.0 (a
//!   1-column pillarbox; a range conversion 10.8). Maxima over all 90
//!   pictures, re-measured in review round 2;
//! - the edge bands (2 rows / columns of the scaled picture against the
//!   matching rows / columns of the unscaled one): a correct scale ≤ 1.34
//!   at 80×60 (tolerance 6) and ≤ 7.2 at 66×50 (tolerance 10), a 1–2 px
//!   letterbox or pillarbox ≥ 25.
//!
//! Every test that changes or reads the process-wide `hw_counters()` (or
//! arms the refusal hook) holds `COUNTERS`, so a counter's change is that
//! test's own and is pinned exactly.
//!
//! #282 follow-up: each scaled open writes what it read of Media
//! Foundation to stderr (past libtest's capture, so every CI log shows the
//! runner's facts): the transform chain, the decoder's output at the open
//! and at the first picture, the native geometric aperture and the colour
//! descriptions. It then asserts the chain is the decoder followed by
//! another transform (the video processor), the decoder's output was read
//! and each colour value the native type states is on the negotiated
//! scaled type (`GetCurrentMediaType` keeps what was set: Wine's
//! Windows-run conformance tests; the fixture has no colour description,
//! no VUI signal type and no `colr` box, so on it this is most likely
//! vacuous until a CI log shows a stated `native_colour`). The tests that
//! decode from the first picture also assert the decoder's output was read
//! again there. Two runner premises only their first CI run proves sit in
//! their own tests, so a runner that disagrees fails there alone:
//! `a_scaled_open_lists_the_video_processor_in_its_chain` (the processor's
//! category) and `the_first_picture_shows_no_decoder_padding_in_reach`
//! (the decoder's crop shown as `MF_MT_MINIMUM_DISPLAY_APERTURE`).

#![cfg(windows)]

use std::io::Write;
use std::sync::{Mutex, MutexGuard};

use sp_decoder::video::ChainStage;
use sp_decoder::{
    DecodeMode, DecodedVideoFrame, MediaFoundationVideoReader, MediaStream, VideoStream,
    hw_counters,
};

/// The fixture's own picture.
const W: usize = 160;
const H: usize = 120;

/// One counter-touching test at a time (the module doc).
static COUNTERS: Mutex<()> = Mutex::new(());

fn counters_alone() -> MutexGuard<'static, ()> {
    COUNTERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("black_3s.mp4")
}

/// Every picture of `reader`.
fn decode_all(reader: &mut MediaFoundationVideoReader, what: &str) -> Vec<DecodedVideoFrame> {
    let mut pictures = Vec::new();
    while let Some(frame) = reader
        .next_frame()
        .unwrap_or_else(|e| panic!("{what}: decode {}: {e}", pictures.len()))
    {
        pictures.push(frame);
    }
    assert!(pictures.len() > 30, "{what}: {} pictures", pictures.len());
    pictures
}

/// The top-left `w`×`h` of a picture, Y rows then UV rows, read through the
/// picture's own stride and height (the layout every consumer reads).
fn visible(frame: &DecodedVideoFrame, w: usize, h: usize) -> Vec<u8> {
    let stride = frame.stride as usize;
    let rows = frame.height as usize;
    assert!(
        stride >= w && rows >= h,
        "{}x{} stride {stride}",
        frame.width,
        rows
    );
    assert!(
        frame.data.len() >= stride * (rows + rows.div_ceil(2)),
        "a whole NV12 picture: {} bytes, stride {stride}, {rows} rows",
        frame.data.len()
    );
    let mut out = Vec::with_capacity(w * h * 3 / 2);
    for y in 0..h {
        out.extend_from_slice(&frame.data[y * stride..y * stride + w]);
    }
    let uv = stride * rows;
    for y in 0..h.div_ceil(2) {
        let row = uv + y * stride;
        out.extend_from_slice(&frame.data[row..row + w]);
    }
    out
}

/// The mean luma of the rows `y0..y1` and columns `x0..x1`.
fn luma_mean(frame: &DecodedVideoFrame, (x0, x1): (usize, usize), (y0, y1): (usize, usize)) -> f64 {
    let stride = frame.stride as usize;
    let mut sum = 0_u64;
    for y in y0..y1 {
        for &v in &frame.data[y * stride + x0..y * stride + x1] {
            sum += u64::from(v);
        }
    }
    sum as f64 / ((x1 - x0) * (y1 - y0)) as f64
}

/// The luma means of a 4×4 grid over the `w`×`h` picture (both divisible
/// by 4).
fn grid(frame: &DecodedVideoFrame, w: usize, h: usize) -> Vec<f64> {
    let (bw, bh) = (w / 4, h / 4);
    let mut out = Vec::new();
    for by in 0..4 {
        for bx in 0..4 {
            out.push(luma_mean(
                frame,
                (bx * bw, (bx + 1) * bw),
                (by * bh, (by + 1) * bh),
            ));
        }
    }
    out
}

/// The luma means of the top, bottom, left and right `edge` rows / columns
/// of the `w`×`h` picture.
fn bands(frame: &DecodedVideoFrame, w: usize, h: usize, edge: usize) -> Vec<f64> {
    vec![
        luma_mean(frame, (0, w), (0, edge)),
        luma_mean(frame, (0, w), (h - edge, h)),
        luma_mean(frame, (0, edge), (0, h)),
        luma_mean(frame, (w - edge, w), (0, h)),
    ]
}

/// The largest difference between two lists of means.
fn worst(a: &[f64], b: &[f64]) -> f64 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f64::max)
}

/// An uncapped reader on the fixture, its facts, and every picture.
fn unscaled() -> (MediaFoundationVideoReader, Vec<DecodedVideoFrame>) {
    let mut reader = MediaFoundationVideoReader::open(&fixture()).expect("software open");
    let pictures = decode_all(&mut reader, "unscaled");
    (reader, pictures)
}

/// A capped reader's open, its facts against the unscaled reader's, and
/// the counters' change; it writes what Media Foundation negotiated and
/// what the scaled open read (`ScaledOpenFacts`) straight to stderr (past
/// libtest's capture), so every CI log shows it, then asserts the chain is
/// the decoder and a transform after it, the decoder's output was read,
/// and each stated native colour value is on the negotiated type.
fn open_scaled(
    cap: u32,
    size: (u32, u32),
    plain: &MediaFoundationVideoReader,
) -> MediaFoundationVideoReader {
    let before = hw_counters().snapshot();
    let reader = MediaFoundationVideoReader::open_with(&fixture(), DecodeMode::Software, Some(cap))
        .expect("scaled open");
    let after = hw_counters().snapshot();
    let _ = writeln!(
        std::io::stderr(),
        "mf_decode_cap: cap {cap}: {}x{} at {:?} (known {}), duration {} ms, scaled from {:?}, \
         decoder output {:?}, scaled opens +{}, refusals +{}",
        reader.width(),
        reader.height(),
        reader.frame_rate(),
        reader.frame_rate_known(),
        reader.duration_ms(),
        reader.scaled_from(),
        reader.scaled_decoder_output(),
        after.scaled_opens - before.scaled_opens,
        after.scale_refusals - before.scale_refusals
    );
    let facts = reader.scaled_open_facts().cloned();
    let _ = writeln!(
        std::io::stderr(),
        "mf_decode_cap: cap {cap}: what the scaled open read: {facts:?}"
    );
    assert_eq!(
        after.scale_refusals, before.scale_refusals,
        "cap {cap}: Media Foundation took the scaled type"
    );
    assert_eq!(
        after.scaled_opens,
        before.scaled_opens + 1,
        "cap {cap}: the open was scaled"
    );
    assert_eq!((reader.width(), reader.height()), size, "cap {cap}");
    assert_eq!(
        reader.scaled_from(),
        Some((160, 120)),
        "cap {cap}: the reader says it scaled the stream's own size"
    );
    // The same rate, compared as a ratio (30/1 and 60/2 are one rate).
    let (scaled_rate, plain_rate) = (reader.frame_rate(), plain.frame_rate());
    assert_eq!(
        u64::from(scaled_rate.0) * u64::from(plain_rate.1),
        u64::from(plain_rate.0) * u64::from(scaled_rate.1),
        "cap {cap}: the stream's own rate, {scaled_rate:?} vs {plain_rate:?}"
    );
    assert!(reader.frame_rate_known(), "cap {cap}: not the 29.97 guess");
    let facts = facts.unwrap_or_else(|| panic!("cap {cap}: a scaled open keeps its facts"));
    // The decoder at index 0 (Microsoft) and a transform after it: the
    // processor the reader inserts (Wine's conformance tests, run against
    // Windows, read it at index 1). Its category is pinned in its own test.
    assert!(
        facts.chain.first() == Some(&ChainStage::Decoder)
            && facts
                .chain
                .get(1)
                .is_some_and(|stage| !matches!(stage, ChainStage::Error(_))),
        "cap {cap}: the decoder, then the video processor: {:?}",
        facts.chain
    );
    // `ScaledType::apply` puts each colour value the native type states on
    // the scaled type, so the processor converts no colour.
    let out = facts.out_colour;
    if let Some(matrix) = facts.native_colour.matrix {
        assert_eq!(
            out.and_then(|c| c.matrix),
            Some(matrix),
            "cap {cap}: the native YUV matrix on the scaled type"
        );
    }
    if let Some(range) = facts.native_colour.range {
        assert_eq!(
            out.and_then(|c| c.range),
            Some(range),
            "cap {cap}: the native nominal range on the scaled type"
        );
    }
    assert!(
        reader.scaled_decoder_output().is_some(),
        "cap {cap}: the decoder's output was read at the open: {:?}",
        facts.decoder
    );
    assert_eq!(
        facts.decoder_at_first_picture, None,
        "cap {cap}: the decoder's output is read again only at the first picture"
    );
    reader
}

/// #282 follow-up: after a scaled reader's first picture, the decoder's
/// output was read again, once; written to stderr (the CI log's fact for
/// the software decoder after its first frame).
fn assert_reread_at_first_picture(reader: &MediaFoundationVideoReader, what: &str) {
    let facts = reader
        .scaled_open_facts()
        .unwrap_or_else(|| panic!("{what}: a scaled reader keeps its facts"));
    let _ = writeln!(
        std::io::stderr(),
        "mf_decode_cap: {what}: the decoder's output at the open {:?}, at the first picture {:?}",
        facts.decoder,
        facts.decoder_at_first_picture
    );
    assert!(
        matches!(facts.decoder_at_first_picture, Some(Ok(_))),
        "{what}: the decoder's output was read again at the first picture: {:?}",
        facts.decoder_at_first_picture
    );
    assert!(
        facts.first_picture_padding_in_reach.is_some(),
        "{what}: the padding check ran with the re-read"
    );
}

/// The pictures handed over after the reader decoded 60 (past 1 900 ms)
/// and then sought BACK to the middle, with the seek's target. The
/// fixture's one keyframe is at 0, so a real seek hands over a picture at
/// or before the target; a seek that did nothing would hand over the 61st
/// picture (~2 000 ms).
fn after_a_seek(
    reader: &mut MediaFoundationVideoReader,
    what: &str,
) -> (u64, Vec<DecodedVideoFrame>) {
    for i in 0..60 {
        reader
            .next_frame()
            .unwrap_or_else(|e| panic!("{what}: decode {i}: {e}"))
            .unwrap_or_else(|| panic!("{what}: picture {i}"));
    }
    let target = reader.duration_ms() / 2;
    reader
        .seek(target)
        .unwrap_or_else(|e| panic!("{what}: seek to {target} ms: {e}"));
    let mut rest = Vec::new();
    while let Some(frame) = reader
        .next_frame()
        .unwrap_or_else(|e| panic!("{what}: decode {} after the seek: {e}", rest.len()))
    {
        rest.push(frame);
    }
    assert!(!rest.is_empty(), "{what}: pictures after the seek");
    assert!(
        rest[0].timestamp_ms <= target,
        "{what}: the seek moved back to {target} ms or before, got {} ms",
        rest[0].timestamp_ms
    );
    (target, rest)
}

/// Every `scaled` picture is 80×60 (stride 80) and matches the `plain`
/// picture at the same place by regions (the module doc's tolerances).
fn assert_scaled_like(scaled: &[DecodedVideoFrame], plain: &[DecodedVideoFrame], what: &str) {
    assert_eq!(scaled.len(), plain.len(), "{what}: one picture each");
    for (i, (s, p)) in scaled.iter().zip(plain).enumerate() {
        assert_eq!(
            s.timestamp_ms, p.timestamp_ms,
            "{what}, picture {i}: its time"
        );
        assert_eq!(
            (s.width, s.height, s.stride),
            (80, 60, 80),
            "{what}, picture {i}: the scaled size"
        );
        let _ = visible(s, 80, 60);
        let grid_diff = worst(&grid(s, 80, 60), &grid(p, W, H));
        assert!(
            grid_diff < 6.0,
            "{what}, picture {i}: a 4x4 luma block off by {grid_diff:.1}"
        );
    }
}

/// A cap of 60 rows: the box is 106×60 (60 × 16/9 = 106.7 → 106), the
/// 160×120 picture's height binds, so it is decoded at 80×60 (pixel aspect
/// 1:1).
#[test]
fn a_cap_smaller_than_the_picture_hands_every_picture_over_scaled() {
    let _alone = counters_alone();
    let (plain_reader, plain) = unscaled();
    let mut reader = open_scaled(60, (80, 60), &plain_reader);
    let scaled = decode_all(&mut reader, "scaled");
    assert_reread_at_first_picture(&reader, "cap 60");
    assert_eq!(
        scaled.len(),
        plain.len(),
        "one scaled picture per decoded picture (no frame-rate conversion)"
    );
    let _ = writeln!(
        std::io::stderr(),
        "mf_decode_cap: cap 60: first picture {}x{} stride {}",
        scaled[0].width,
        scaled[0].height,
        scaled[0].stride
    );
    for (i, (s, p)) in scaled.iter().zip(&plain).enumerate() {
        assert_eq!(s.timestamp_ms, p.timestamp_ms, "picture {i}: its time");
        assert_eq!(
            (s.width, s.height, s.stride),
            (80, 60, 80),
            "picture {i}: the scaled size, packed (stride = one chroma row)"
        );
        // A whole NV12 picture, every byte of both planes readable.
        let _ = visible(s, 80, 60);
        let grid_diff = worst(&grid(s, 80, 60), &grid(p, W, H));
        assert!(
            grid_diff < 6.0,
            "picture {i}: a 4x4 luma block off by {grid_diff:.1}"
        );
        let edge_diff = worst(&bands(s, 80, 60, 2), &bands(p, W, H, 4));
        assert!(
            edge_diff < 6.0,
            "picture {i}: an edge band off by {edge_diff:.1} (a letterbox, a crop or padding)"
        );
    }
}

/// A cap of 50 rows: the box is 88×50, the picture is decoded at 66×50, a
/// fraction of a pixel off the native aspect (pixel aspect 100:99): the
/// picture must still fill the frame (no letterboxed edge).
#[test]
fn an_inexact_ratio_fills_the_whole_frame() {
    let _alone = counters_alone();
    let (plain_reader, plain) = unscaled();
    let mut reader = open_scaled(50, (66, 50), &plain_reader);
    let scaled = decode_all(&mut reader, "scaled at 66x50");
    assert_reread_at_first_picture(&reader, "cap 50");
    assert_eq!(scaled.len(), plain.len());
    // 66 is 2 mod 4: where a stride the type does not state would shear.
    let _ = writeln!(
        std::io::stderr(),
        "mf_decode_cap: cap 50: first picture {}x{} stride {}",
        scaled[0].width,
        scaled[0].height,
        scaled[0].stride
    );
    for (i, (s, p)) in scaled.iter().zip(&plain).enumerate() {
        assert_eq!(s.timestamp_ms, p.timestamp_ms, "picture {i}: its time");
        assert_eq!((s.width, s.height), (66, 50), "picture {i}");
        let _ = visible(s, 66, 50);
        // 2 scaled rows / columns ≈ 5 unscaled ones (×2.4).
        let edge_diff = worst(&bands(s, 66, 50, 2), &bands(p, W, H, 5));
        assert!(
            edge_diff < 10.0,
            "picture {i}: an edge band off by {edge_diff:.1} (a letterboxed edge)"
        );
    }
}

/// A cap whose box holds the picture opens it exactly as before: no
/// scaled open, the same pictures as an uncapped reader.
#[test]
fn a_cap_at_or_above_the_picture_changes_nothing() {
    let _alone = counters_alone();
    let (_, plain) = unscaled();
    let before = hw_counters().snapshot();
    for cap in [120, 2160] {
        let mut reader =
            MediaFoundationVideoReader::open_with(&fixture(), DecodeMode::Software, Some(cap))
                .expect("open");
        assert_eq!((reader.width(), reader.height()), (160, 120), "cap {cap}");
        assert_eq!(reader.scaled_from(), None, "cap {cap}: not scaled");
        assert!(
            reader.scaled_open_facts().is_none(),
            "cap {cap}: no scaled open, no facts"
        );
        let pictures = decode_all(&mut reader, "capped, inside the box");
        assert_eq!(pictures.len(), plain.len(), "cap {cap}");
        for (i, (c, p)) in pictures.iter().zip(&plain).enumerate() {
            assert_eq!(
                (c.timestamp_ms, c.width, c.height, c.stride),
                (p.timestamp_ms, p.width, p.height, p.stride),
                "cap {cap}, picture {i}"
            );
            assert!(
                visible(c, W, H) == visible(p, W, H),
                "cap {cap}, picture {i}: the same bytes"
            );
        }
    }
    let after = hw_counters().snapshot();
    assert_eq!(after.scaled_opens, before.scaled_opens, "nothing scaled");
    assert_eq!(after.scale_refusals, before.scale_refusals);
}

/// Review round 1: a scaled open Media Foundation refuses plays the file at
/// its own size (never a dead song), counted in `scale_refusals`. CI's
/// Media Foundation takes the scaled type, so the refusal is injected.
#[test]
fn a_refused_scaled_open_plays_the_file_at_its_own_size() {
    let _alone = counters_alone();
    let (_, plain) = unscaled();
    let before = hw_counters().snapshot();
    let refusal = MediaFoundationVideoReader::refuse_next_scaled_open_for_test();
    let mut reader =
        MediaFoundationVideoReader::open_with(&fixture(), DecodeMode::Software, Some(60))
            .expect("a refused scaled open still opens the file");
    drop(refusal);
    let after = hw_counters().snapshot();
    assert_eq!(after.scale_refusals, before.scale_refusals + 1, "counted");
    assert_eq!(after.scaled_opens, before.scaled_opens, "not scaled");
    assert_eq!((reader.width(), reader.height()), (160, 120));
    assert_eq!(reader.scaled_from(), None, "a refused open is not scaled");
    assert!(
        reader.scaled_open_facts().is_none(),
        "a refused open has no facts"
    );
    let pictures = decode_all(&mut reader, "refused, unscaled");
    assert_eq!(pictures.len(), plain.len());
    for (i, (c, p)) in pictures.iter().zip(&plain).enumerate() {
        assert_eq!(
            (c.timestamp_ms, c.width, c.height),
            (p.timestamp_ms, p.width, p.height),
            "picture {i}"
        );
        assert!(
            visible(c, W, H) == visible(p, W, H),
            "picture {i}: the same bytes as an uncapped reader"
        );
    }
}

/// #282 follow-up: the transform the source reader inserts after the
/// decoder for a scaled open is listed in `MFT_CATEGORY_VIDEO_PROCESSOR`
/// (Microsoft: the reader looks for its processor in that category; Wine's
/// reader reports that category for it). Its own test, so a runner that
/// reports another category fails here alone; the chain is in the CI log.
#[test]
fn a_scaled_open_lists_the_video_processor_in_its_chain() {
    let _alone = counters_alone();
    let (plain_reader, _) = unscaled();
    let reader = open_scaled(60, (80, 60), &plain_reader);
    let facts = reader.scaled_open_facts().expect("scaled facts");
    assert!(
        facts.has_video_processor(),
        "a video processor in the chain: {:?}",
        facts.chain
    );
}

/// #282 follow-up: at the first picture the decoder's padding does not
/// reach the processor (`first_picture_padding_in_reach` `Some(false)`).
/// The fixture's pictures fill the frame (the edge bands of the tests
/// above), so this pins that the check reads the stream's own size
/// (160x120), never the scaled one (80x60: "padded" on every file). Its
/// premise, that the decoder shows its crop (coded 160x128) as
/// `MF_MT_MINIMUM_DISPLAY_APERTURE` (Wine's H.264 decoder tests, run
/// against Windows) or shows no padding at all, is proven by its first CI
/// run; a runner that shows the crop another way fails here alone.
#[test]
fn the_first_picture_shows_no_decoder_padding_in_reach() {
    let _alone = counters_alone();
    let (plain_reader, _) = unscaled();
    let mut reader = open_scaled(60, (80, 60), &plain_reader);
    reader
        .next_frame()
        .expect("decode the first picture")
        .expect("a first picture");
    assert_reread_at_first_picture(&reader, "the padding check");
    let facts = reader.scaled_open_facts().expect("scaled facts");
    assert_eq!(
        facts.first_picture_padding_in_reach,
        Some(false),
        "no decoder padding in reach of the processor at the first picture: {:?}",
        facts.decoder_at_first_picture
    );
}

/// #282 follow-up: the refusal hook is disarmed when its guard drops, so a
/// test that panics between arming it and its open (the unwind drops the
/// guard) can never refuse a later test's scaled open.
#[test]
fn a_dropped_refusal_guard_refuses_no_later_scaled_open() {
    let _alone = counters_alone();
    let (plain_reader, _) = unscaled();
    let refusal = MediaFoundationVideoReader::refuse_next_scaled_open_for_test();
    drop(refusal);
    // `open_scaled` asserts the open was scaled and nothing was refused.
    let reader = open_scaled(60, (80, 60), &plain_reader);
    assert_eq!(reader.scaled_from(), Some((160, 120)));
}

/// #282 follow-up: a seek on a scaled reader lands where the unscaled
/// reader's lands, and every picture after it keeps the scaled size (the
/// video processor follows the seek; nothing reopens).
#[test]
fn a_seek_on_a_scaled_reader_lands_and_keeps_the_scaled_size() {
    let _alone = counters_alone();
    let (plain_reader, _) = unscaled();
    let mut scaled = open_scaled(60, (80, 60), &plain_reader);
    let before = hw_counters().snapshot();
    let (target, scaled_rest) = after_a_seek(&mut scaled, "scaled");
    assert_reread_at_first_picture(&scaled, "the seek's reader");
    let after = hw_counters().snapshot();
    assert_eq!(
        (after.scaled_opens, after.scale_refusals),
        (before.scaled_opens, before.scale_refusals),
        "a seek reopens nothing"
    );
    let mut plain = MediaFoundationVideoReader::open(&fixture()).expect("software open");
    let (plain_target, plain_rest) = after_a_seek(&mut plain, "unscaled");
    assert_eq!(target, plain_target, "the same target");
    let _ = writeln!(
        std::io::stderr(),
        "mf_decode_cap: a seek to {target} ms: the first scaled picture at {} ms ({}x{}), \
         the unscaled at {} ms, {} pictures after it",
        scaled_rest[0].timestamp_ms,
        scaled_rest[0].width,
        scaled_rest[0].height,
        plain_rest[0].timestamp_ms,
        scaled_rest.len()
    );
    assert_scaled_like(&scaled_rest, &plain_rest, "after the seek");
    assert_eq!(scaled.scaled_from(), Some((160, 120)), "still scaled");
}

/// #282: a software reopen after a decode error keeps the decode cap: the
/// rest of the song comes scaled too (a second scaled open, counted, whose
/// decoder output is read again at its first picture), with no picture lost
/// or handed over twice. CI injects the decode error (the D3D path's own
/// cannot be reached, `mf_hw_decode.rs`).
#[test]
fn a_reopen_after_a_decode_error_keeps_the_decode_cap() {
    let _alone = counters_alone();
    let (plain_reader, plain) = unscaled();
    let mut reader = open_scaled(60, (80, 60), &plain_reader);
    let mut got = Vec::new();
    for i in 0..10 {
        got.push(
            reader
                .next_frame()
                .unwrap_or_else(|e| panic!("decode {i}: {e}"))
                .unwrap_or_else(|| panic!("picture {i}")),
        );
    }
    let before = hw_counters().snapshot();
    reader.fail_next_read_for_test();
    // The fixture's one keyframe is at 0: the reopen decodes pictures 0..=9
    // again and must drop them.
    got.extend(decode_all(&mut reader, "after the reopen"));
    let after = hw_counters().snapshot();
    assert_eq!(
        after.mid_stream_fallbacks,
        before.mid_stream_fallbacks + 1,
        "the injected error reopened the file"
    );
    assert_eq!(
        (after.scaled_opens, after.scale_refusals),
        (before.scaled_opens + 1, before.scale_refusals),
        "the reopen opened scaled too"
    );
    assert_eq!(reader.scaled_from(), Some((160, 120)));
    assert_reread_at_first_picture(&reader, "the reopen");
    assert_scaled_like(&got, &plain, "across the reopen");
}
