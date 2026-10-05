//! #223 S3b: the reader's opt-in hardware decode on the H.264 fixture.
//!
//! `windows-latest` has no GPU. A `Hardware` open there either finds no
//! hardware adapter and opens the file in software (an open fall back), or,
//! on WARP (`open_hardware_on_warp`), gives Media Foundation a WARP device
//! whose decoder then decodes in software (WARP has no decoder profiles) or
//! hands DXGI surfaces over. Whatever the path, the reported one must be
//! `hardware` or `software`, and every picture must be the SAME NV12 the
//! software reader decodes (H.264 decoding is bit-exact), compared pixel by
//! pixel over the fixture's visible 160×120. A failed open or decode FAILS
//! here; nothing is skipped.
//!
//! The mid-stream fall back cannot be caused on CI (no device to lose), so a
//! test injects one decode failure (`fail_next_read_for_test`): the reader
//! must reopen the file in software and go on where it stopped, with no
//! picture lost or handed over twice.

#![cfg(windows)]

use sp_decoder::{
    DecodeMode, DecodePath, DecodedVideoFrame, FallbackStage, MediaFoundationVideoReader,
    MediaStream, VideoStream, hw_counters,
};

/// The fixture's own picture: 160×120 (MF may pad the height to 128).
const W: usize = 160;
const H: usize = 120;

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("black_3s.mp4")
}

/// The visible 160×120 of a picture, Y rows then UV rows, read through the
/// picture's own stride and height (the layout every consumer reads).
fn visible(frame: &DecodedVideoFrame) -> Vec<u8> {
    let stride = frame.stride as usize;
    let rows = frame.height as usize;
    assert!(
        stride >= W && rows >= H,
        "{}x{} stride {stride}",
        frame.width,
        rows
    );
    assert!(
        frame.data.len() >= stride * (rows + rows.div_ceil(2)),
        "a whole NV12 picture: {} bytes, stride {stride}, {rows} rows",
        frame.data.len()
    );
    let mut out = Vec::with_capacity(W * H * 3 / 2);
    for y in 0..H {
        out.extend_from_slice(&frame.data[y * stride..y * stride + W]);
    }
    let uv = stride * rows;
    for y in 0..H.div_ceil(2) {
        let row = uv + y * stride;
        out.extend_from_slice(&frame.data[row..row + W]);
    }
    out
}

/// Every picture of `reader`: its timestamp and its visible bytes. After
/// each one the reader names a real path.
fn decode_all(reader: &mut MediaFoundationVideoReader, what: &str) -> Vec<(u64, Vec<u8>)> {
    let mut pictures = Vec::new();
    while let Some(frame) = reader
        .next_frame()
        .unwrap_or_else(|e| panic!("{what}: decode {}: {e}", pictures.len()))
    {
        assert!(
            matches!(
                reader.decode_path(),
                Some(DecodePath::Hardware | DecodePath::Software)
            ),
            "{what}: picture {} has no path",
            pictures.len()
        );
        pictures.push((frame.timestamp_ms, visible(&frame)));
    }
    assert!(pictures.len() > 30, "{what}: {} pictures", pictures.len());
    pictures
}

fn software_pictures() -> Vec<(u64, Vec<u8>)> {
    let mut reader = MediaFoundationVideoReader::open(&fixture()).expect("software open");
    assert_eq!(reader.decode_mode(), DecodeMode::Software);
    let pictures = decode_all(&mut reader, "software");
    assert_eq!(reader.decode_path(), Some(DecodePath::Software));
    assert_eq!(reader.hw_adapter(), None);
    assert!(reader.hw_fallback().is_none());
    pictures
}

/// The same pictures, at the same timestamps, byte for byte.
fn assert_same_pictures(got: &[(u64, Vec<u8>)], want: &[(u64, Vec<u8>)], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: picture count");
    for (i, ((got_ts, got_px), (want_ts, want_px))) in got.iter().zip(want).enumerate() {
        assert_eq!(got_ts, want_ts, "{what}: picture {i}'s timestamp");
        if got_px != want_px {
            let at = got_px.iter().zip(want_px).position(|(a, b)| a != b);
            panic!("{what}: picture {i} (ts {got_ts}) differs from software at byte {at:?}");
        }
    }
}

/// What a `Hardware` reader reports: a hardware path names its adapter; a
/// software one either fell back at open or runs on the D3D path with Media
/// Foundation's software decoder.
fn assert_reported(reader: &MediaFoundationVideoReader, what: &str) {
    assert_eq!(reader.decode_mode(), DecodeMode::Hardware, "{what}");
    let path = reader.decode_path();
    let fallback = reader.hw_fallback();
    eprintln!(
        "{what}: decode_path={path:?} adapter={:?} fallback={:?}",
        reader.hw_adapter(),
        fallback.map(|f| f.describe())
    );
    match path {
        Some(DecodePath::Hardware) => {
            assert!(
                reader.hw_adapter().is_some(),
                "{what}: a GPU path names its adapter"
            );
            assert!(fallback.is_none(), "{what}: {fallback:?}");
        }
        Some(DecodePath::Software) => {
            if let Some(f) = fallback {
                assert_eq!(f.stage, FallbackStage::Open, "{what}: {f:?}");
                assert!(!f.reason.is_empty(), "{what}");
            }
        }
        None => panic!("{what}: no picture decoded"),
    }
}

#[test]
fn hardware_mode_opens_the_fixture_and_decodes_what_software_decodes() {
    let before = hw_counters().snapshot().requested;
    let mut reader = MediaFoundationVideoReader::open_with(&fixture(), DecodeMode::Hardware)
        .expect("a Hardware open never fails on a file software opens");
    assert!(hw_counters().snapshot().requested > before, "counted");
    let pictures = decode_all(&mut reader, "hardware (picked adapter)");
    assert_reported(&reader, "hardware (picked adapter)");
    assert_same_pictures(&pictures, &software_pictures(), "hardware (picked adapter)");
}

#[test]
fn hardware_mode_on_warp_decodes_what_software_decodes() {
    let mut reader = MediaFoundationVideoReader::open_hardware_on_warp(&fixture())
        .expect("a Hardware open on WARP never fails on a file software opens");
    let pictures = decode_all(&mut reader, "hardware (WARP)");
    assert_reported(&reader, "hardware (WARP)");
    assert_same_pictures(&pictures, &software_pictures(), "hardware (WARP)");
}

#[test]
fn a_hardware_reader_seeks_and_decodes() {
    let mut reader = MediaFoundationVideoReader::open_hardware_on_warp(&fixture())
        .expect("a Hardware open on WARP");
    let target = reader.duration_ms() / 2;
    reader.seek(target).expect("seek on the hardware reader");
    let frame = reader
        .next_frame()
        .expect("decode after the seek")
        .expect("a picture after the seek");
    assert_eq!(visible(&frame).len(), W * H * 3 / 2);
}

#[test]
fn a_mid_stream_fallback_goes_on_where_it_stopped() {
    let want = software_pictures();
    let before = hw_counters().snapshot().mid_stream_fallbacks;
    let mut reader = MediaFoundationVideoReader::open(&fixture()).expect("software open");
    let mut got = Vec::new();
    for _ in 0..10 {
        let frame = reader.next_frame().expect("decode").expect("a picture");
        got.push((frame.timestamp_ms, visible(&frame)));
    }
    reader.fail_next_read_for_test();
    // The fixture's one keyframe is at 0: the reopen decodes pictures 0..=9
    // again and must drop them.
    got.extend(decode_all(&mut reader, "after the injected failure"));
    let fallback = reader.hw_fallback().expect("the reader fell back");
    assert_eq!(fallback.stage, FallbackStage::MidStream);
    assert!(fallback.reason.contains("injected"), "{fallback:?}");
    assert_eq!(reader.decode_path(), Some(DecodePath::Software));
    assert!(
        hw_counters().snapshot().mid_stream_fallbacks > before,
        "counted"
    );
    assert_same_pictures(&got, &want, "mid-stream fall back");
}
