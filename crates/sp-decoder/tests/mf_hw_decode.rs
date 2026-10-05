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
//!
//! The readback of a GPU picture never runs on CI through a decoder (WARP
//! decodes nothing), so `a_dxgi_surface_is_read_back_into_the_software_layout`
//! runs it on a real DXGI surface: a WARP NV12 texture TALLER than the
//! picture (decoders align theirs), wrapped as a decoder wraps its output.

#![cfg(windows)]

use sp_decoder::{
    DecodeMode, DecodePath, DecodedVideoFrame, FallbackStage, MediaFoundationVideoReader,
    MediaStream, VideoStream, hw_counters, read_texture_as_decoded_sample,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_DECODER, D3D11_FORMAT_SUPPORT_TEXTURE2D, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

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
    // The video device, the DXGI device manager and its reset were built on
    // WARP: either the source reader runs on them, or it refused them (the
    // only open fall back that comes after them).
    let on_d3d = reader.hw_adapter().is_some();
    let refused = reader
        .hw_fallback()
        .is_some_and(|f| f.reason.starts_with("the source reader refused"));
    assert!(
        on_d3d || refused,
        "the D3D11 path was not built on WARP: {:?}",
        reader.hw_fallback()
    );
    let first_pictures = |stats: sp_decoder::HwDecodeStats| stats.gpu_decodes + stats.mf_software;
    let counted_before = first_pictures(hw_counters().snapshot());
    let pictures = decode_all(&mut reader, "hardware (WARP)");
    assert_reported(&reader, "hardware (WARP)");
    if on_d3d {
        // A reader on the D3D path counts its first picture's path.
        assert!(
            first_pictures(hw_counters().snapshot()) > counted_before,
            "the first picture on the D3D path is counted"
        );
    }
    assert_same_pictures(&pictures, &software_pictures(), "hardware (WARP)");
}

/// The fixture's one keyframe is at 0, so a seek to the middle lands there.
/// Each reader first decodes 60 pictures (past 1 900 ms) and then seeks BACK
/// to the middle: a seek that did nothing would hand over the 61st picture
/// (~2 000 ms), a real one a picture at or before the target.
#[test]
fn a_hardware_reader_seeks_to_the_picture_software_seeks_to() {
    let first_after_seek = |mut reader: MediaFoundationVideoReader, what: &str| {
        for i in 0..60 {
            reader
                .next_frame()
                .unwrap_or_else(|e| panic!("{what}: decode {i}: {e}"))
                .unwrap_or_else(|| panic!("{what}: picture {i}"));
        }
        let target = reader.duration_ms() / 2;
        reader
            .seek(target)
            .unwrap_or_else(|e| panic!("{what}: seek: {e}"));
        let frame = reader
            .next_frame()
            .unwrap_or_else(|e| panic!("{what}: decode after the seek: {e}"))
            .unwrap_or_else(|| panic!("{what}: a picture after the seek"));
        assert!(
            frame.timestamp_ms <= target,
            "{what}: the seek moved back to {target} ms or before, got {} ms",
            frame.timestamp_ms
        );
        (frame.timestamp_ms, visible(&frame))
    };
    let hardware = MediaFoundationVideoReader::open_hardware_on_warp(&fixture())
        .expect("a Hardware open on WARP");
    let software = MediaFoundationVideoReader::open(&fixture()).expect("software open");
    assert_same_pictures(
        &[first_after_seek(hardware, "hardware")],
        &[first_after_seek(software, "software")],
        "the first picture after a seek",
    );
}

/// A WARP NV12 texture of 160×128 (as a decoder aligns a 160×120 picture)
/// whose bytes each say where they are, read back as a 160×120 picture: the
/// first 120 luma rows, then the first 60 UV rows of the UV plane that
/// starts after ALL 128 luma rows, packed at a stride of 160. This is the
/// layout `Lock2DSize` maps (Direct3D's NV12), which no decoder reaches on
/// CI.
#[test]
fn a_dxgi_surface_is_read_back_into_the_software_layout() {
    // The pure crate's constants are the SDK's.
    assert_eq!(
        sp_decoder::hw_decode::DXGI_FORMAT_NV12,
        DXGI_FORMAT_NV12.0 as u32
    );
    assert_eq!(
        sp_decoder::hw_decode::D3D11_BIND_DECODER,
        D3D11_BIND_DECODER.0 as u32
    );
    const TEXTURE_W: usize = 160;
    const TEXTURE_H: usize = 128;
    let video = sp_gpu::VideoDevice::new_warp().expect("a video device on WARP");
    let device = video.device();
    let support =
        unsafe { device.CheckFormatSupport(DXGI_FORMAT_NV12) }.expect("WARP answers for NV12");
    assert_ne!(
        support & D3D11_FORMAT_SUPPORT_TEXTURE2D.0 as u32,
        0,
        "WARP must make NV12 textures ({support:#x})"
    );
    let surface: Vec<u8> = (0..TEXTURE_W * (TEXTURE_H + TEXTURE_H / 2))
        .map(|i| (i % 251) as u8)
        .collect();
    let desc = D3D11_TEXTURE2D_DESC {
        Width: TEXTURE_W as u32,
        Height: TEXTURE_H as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: 0,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    // NV12 initial data: the UV plane follows the luma plane at
    // SysMemPitch × Height.
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: surface.as_ptr().cast(),
        SysMemPitch: TEXTURE_W as u32,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut texture)) }
        .expect("a 160x128 NV12 texture on WARP");
    let texture = texture.expect("CreateTexture2D gave a texture");

    let picture = read_texture_as_decoded_sample(&texture, W as u32, H as u32)
        .expect("the DXGI surface reads back");

    let mut want = Vec::with_capacity(W * H * 3 / 2);
    for y in 0..H {
        want.extend_from_slice(&surface[y * TEXTURE_W..y * TEXTURE_W + W]);
    }
    let uv = TEXTURE_W * TEXTURE_H;
    for y in 0..H / 2 {
        let row = uv + y * TEXTURE_W;
        want.extend_from_slice(&surface[row..row + W]);
    }
    assert_eq!(picture.stride, W as u32);
    assert_eq!(picture.data.len(), want.len());
    if picture.data != want {
        let at = picture.data.iter().zip(&want).position(|(a, b)| a != b);
        panic!("the read-back picture differs from the texture at byte {at:?}");
    }
    assert_eq!(
        picture.path,
        DecodePath::Software,
        "a texture without D3D11_BIND_DECODER is no decoder's output"
    );
}

/// A reader decodes 10 pictures, one decode error is injected, and the rest
/// must be exactly the uninterrupted software sequence.
fn assert_goes_on_after_a_mid_stream_fallback(mut reader: MediaFoundationVideoReader, what: &str) {
    let want = software_pictures();
    let before = hw_counters().snapshot().mid_stream_fallbacks;
    let mut got = Vec::new();
    for _ in 0..10 {
        let frame = reader.next_frame().expect("decode").expect("a picture");
        got.push((frame.timestamp_ms, visible(&frame)));
    }
    reader.fail_next_read_for_test();
    // The fixture's one keyframe is at 0: the reopen decodes pictures 0..=9
    // again and must drop them.
    got.extend(decode_all(&mut reader, what));
    let fallback = reader.hw_fallback().expect("the reader fell back");
    assert_eq!(fallback.stage, FallbackStage::MidStream, "{what}");
    assert!(fallback.reason.contains("injected"), "{what}: {fallback:?}");
    assert_eq!(reader.decode_path(), Some(DecodePath::Software), "{what}");
    assert!(
        hw_counters().snapshot().mid_stream_fallbacks > before,
        "{what}: counted"
    );
    assert_same_pictures(&got, &want, what);
}

#[test]
fn a_software_reader_goes_on_where_it_stopped_after_a_mid_stream_fallback() {
    let reader = MediaFoundationVideoReader::open(&fixture()).expect("software open");
    assert_goes_on_after_a_mid_stream_fallback(reader, "software reader");
}

/// The fall back the feature exists for: a `Hardware` reader on WARP leaves
/// the D3D path for the system-memory path mid-file. Same contract as the
/// WARP open test: the reader runs on WARP's device manager, or the source
/// reader refused it (then this is the software reader's fall back again);
/// which one is logged.
#[test]
fn a_hardware_reader_goes_on_where_it_stopped_after_a_mid_stream_fallback() {
    let reader = MediaFoundationVideoReader::open_hardware_on_warp(&fixture())
        .expect("a Hardware open on WARP");
    let on_d3d = reader.hw_adapter().is_some();
    let refused = reader
        .hw_fallback()
        .is_some_and(|f| f.reason.starts_with("the source reader refused"));
    eprintln!(
        "hardware reader: on the D3D path={on_d3d} open fallback={:?}",
        reader.hw_fallback().map(|f| f.describe())
    );
    assert!(
        on_d3d || refused,
        "the D3D11 path was not built on WARP: {:?}",
        reader.hw_fallback()
    );
    assert_goes_on_after_a_mid_stream_fallback(reader, "hardware reader");
}
