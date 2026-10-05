//! #223 S3b: the reader's opt-in hardware decode on the H.264 fixture.
//!
//! `windows-latest` has no GPU, and its WARP refuses the video device the
//! reader asks for: `D3D11CreateDevice` on WARP with BGRA +
//! `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` at feature level 11.1 / 11.0 (the
//! call `sp_gpu::VideoDevice` makes) returns DXGI_ERROR_UNSUPPORTED
//! (0x887A0004, CI run 37293259981). Microsoft's `D3D11_CREATE_DEVICE_FLAG`
//! page says a WARP device with the flag succeeds; the same entry limits
//! video on a pre-WDDM-1.2 driver to feature levels 9.x, which may be why an
//! 11.x request is refused. On WARP (`open_hardware_on_warp`) a `Hardware`
//! open therefore falls back at open with exactly that reason
//! (`assert_fell_back_at_open_on_warp`): the CI test of a refused video
//! device. On the picked adapter the reader reports `hardware` or
//! `software` (`assert_reported`: whatever `pick_adapter` finds on the
//! runner, not pinned; with no GPU, an open fall back). Either way every
//! picture must be the SAME NV12 the software reader decodes (H.264 decoding
//! is bit-exact), compared pixel by pixel over the fixture's visible
//! 160×120. A failed open or decode FAILS here; nothing is skipped.
//!
//! So no WARP reader runs on the D3D path here. DXVA decode and the readback
//! of a decoder's texture are left to the main session's box bench
//! (`decode-bench` with `"hw": true` on the box's GPU: `decode_path:
//! "hardware"`, `path_changes: 0`). A seek and a decode error (the
//! mid-stream fall back) ON the D3D path are proven nowhere: the bench
//! neither seeks nor forces a fall back. Their code is shared with what CI
//! does run: the decisions in `hw_decode_tests.rs` (Linux), the seek parity
//! and the software reopen through one injected decode failure
//! (`fail_next_read_for_test`: the reader must go on where it stopped, no
//! picture lost or handed over twice), and the readback below.
//!
//! The readback of a GPU picture never runs on CI through a decoder, so
//! `a_dxgi_surface_is_read_back_into_the_software_layout` runs it on a real
//! DXGI surface: a WARP NV12 texture TALLER than the picture (decoders align
//! theirs), wrapped as a decoder wraps its output. The texture is made on a
//! plain WARP device with the compositor's flags (`plain_warp_device`, no
//! video flag): making an NV12 texture and mapping it should not need the
//! video device, and this test is what proves it on CI.

#![cfg(windows)]

use sp_decoder::{
    DecodeMode, DecodePath, DecodedVideoFrame, FallbackStage, MediaFoundationVideoReader,
    MediaStream, VideoStream, hw_counters, read_texture_as_decoded_sample,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_DECODER, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FORMAT_SUPPORT_TEXTURE2D,
    D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_UNSUPPORTED;

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

/// What a `Hardware` reader on WARP is on `windows-latest`: its video device
/// was refused (`D3D11CreateDevice` with the video flag at feature level 11.x,
/// DXGI_ERROR_UNSUPPORTED, which the reader's session reports as
/// `no video device: <sp_gpu error>`), so it has no device manager, never
/// ran on the D3D path and fell back at open. A WARP that one day makes that
/// device fails this, and the WARP tests must then be written for the D3D
/// path they reach.
fn assert_fell_back_at_open_on_warp(reader: &MediaFoundationVideoReader, what: &str) {
    assert_eq!(reader.decode_mode(), DecodeMode::Hardware, "{what}");
    assert_eq!(reader.hw_adapter(), None, "{what}: never on the D3D path");
    let refused = sp_gpu::GpuError::Api {
        call: "D3D11CreateDevice",
        hresult: DXGI_ERROR_UNSUPPORTED.0 as u32,
    };
    let fallback = reader
        .hw_fallback()
        .unwrap_or_else(|| panic!("{what}: WARP refuses the video device, so the open falls back"));
    assert_eq!(fallback.stage, FallbackStage::Open, "{what}: {fallback:?}");
    assert_eq!(
        fallback.reason,
        format!("no video device: {refused}"),
        "{what}"
    );
}

/// A `Hardware` open on WARP, the CI test of a refused video device: it
/// falls back at open (counted), and every picture is decoded in software,
/// on one path, and is the software reader's.
#[test]
fn hardware_mode_on_warp_falls_back_at_open_and_decodes_what_software_decodes() {
    let before = hw_counters().snapshot();
    let mut reader = MediaFoundationVideoReader::open_hardware_on_warp(&fixture())
        .expect("a Hardware open on WARP never fails on a file software opens");
    assert_fell_back_at_open_on_warp(&reader, "hardware (WARP)");
    let after = hw_counters().snapshot();
    assert!(after.requested > before.requested, "the request is counted");
    assert!(
        after.open_fallbacks > before.open_fallbacks,
        "the open fall back is counted"
    );
    let pictures = decode_all(&mut reader, "hardware (WARP)");
    assert_eq!(reader.decode_path(), Some(DecodePath::Software));
    assert_eq!(reader.path_changes(), 0, "one path for the whole file");
    assert_same_pictures(&pictures, &software_pictures(), "hardware (WARP)");
}

/// The fixture's one keyframe is at 0, so a seek to the middle lands there.
/// Each reader first decodes 60 pictures (past 1 900 ms) and then seeks BACK
/// to the middle: a seek that did nothing would hand over the 61st picture
/// (~2 000 ms), a real one a picture at or before the target. On CI the
/// `Hardware` reader fell back at open (asserted), so this pins the seek of a
/// `Hardware` reader on the software path (`Resume::on_seek` +
/// `SetCurrentPosition`). A seek on the D3D path is proven nowhere: the box
/// bench does not seek.
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
    assert_fell_back_at_open_on_warp(&hardware, "hardware");
    let software = MediaFoundationVideoReader::open(&fixture()).expect("software open");
    assert_same_pictures(
        &[first_after_seek(hardware, "hardware")],
        &[first_after_seek(software, "software")],
        "the first picture after a seek",
    );
}

/// A WARP device WITHOUT the video flag: BGRA support at feature level 11.1
/// or 11.0, the call CI already makes for the compositor's WARP device
/// (`sp_gpu`'s `DeviceUse::Compose`). It is the device of the readback
/// test's texture, not a decode device.
fn plain_warp_device() -> ID3D11Device {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_WARP,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0][..]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .expect("a WARP device without the video flag");
    device.expect("D3D11CreateDevice gave a device")
}

/// A WARP NV12 texture of 160×128 (as a decoder aligns a 160×120 picture)
/// whose bytes each say where they are, read back as a 160×120 picture: the
/// first 120 luma rows, then the first 60 UV rows of the UV plane that
/// starts after ALL 128 luma rows, packed at a stride of 160. This is the
/// layout `Lock2DSize` maps (Direct3D's NV12), which no decoder reaches on
/// CI. The texture's device has no video flag (`plain_warp_device`); if WARP
/// cannot make NV12 textures, the test FAILS on that, it never skips.
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
    let device = plain_warp_device();
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

/// The fall back the feature exists for, a decode error on the D3D path,
/// cannot be reached on CI: WARP refuses the video device, so a `Hardware`
/// reader on WARP falls back at OPEN (asserted first) and never runs on the
/// D3D path. Nor does the box bench force one, so that path's fall back is
/// proven nowhere; its code is what CI covers: the gate, `Resume` and the
/// software reopen are the code the software-reader variant above runs, and
/// `hw_decode_tests.rs` pins every decision on Linux. What this test adds: a
/// `Hardware` reader that fell back at open, given one injected decode error
/// (the hook arms the once-per-file gate, which such a reader never arms
/// itself), goes on exactly as software does, and its fall back is then the
/// mid-stream one.
#[test]
fn a_hardware_reader_that_fell_back_at_open_goes_on_after_an_injected_decode_error() {
    let reader = MediaFoundationVideoReader::open_hardware_on_warp(&fixture())
        .expect("a Hardware open on WARP");
    assert_fell_back_at_open_on_warp(&reader, "hardware reader");
    assert_goes_on_after_a_mid_stream_fallback(reader, "hardware reader");
}
