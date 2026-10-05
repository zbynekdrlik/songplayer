//! #223 S3b: the hardware decode decisions (`hw_decode.rs`), on Linux.
//! Wired via `#[cfg(test)] #[path = "hw_decode_tests.rs"] mod tests;`.

use super::{
    DXGI_FORMAT_NV12, DecodeMode, DecodePath, FallbackStage, HwCounters, HwDecodeStats, HwFallback,
    OnDecodeError, Resume, SurfaceError, SurfaceLayout, hw_counters, mapped_from_scanline0,
    on_decode_error,
};
use crate::error::DecoderError;

// ---------------------------------------------------------------------------
// Mode and path
// ---------------------------------------------------------------------------

#[test]
fn the_mode_is_software_unless_hardware_is_asked_for() {
    assert_eq!(DecodeMode::default(), DecodeMode::Software);
    assert_eq!(DecodeMode::from_hw_flag(false), DecodeMode::Software);
    assert_eq!(DecodeMode::from_hw_flag(true), DecodeMode::Hardware);
}

#[test]
fn a_dxgi_surface_was_decoded_on_the_gpu_a_system_buffer_in_software() {
    assert_eq!(DecodePath::of_picture(true), DecodePath::Hardware);
    assert_eq!(DecodePath::of_picture(false), DecodePath::Software);
    assert_eq!(DecodePath::Hardware.as_str(), "hardware");
    assert_eq!(DecodePath::Software.as_str(), "software");
}

#[test]
fn a_fallback_names_its_stage_and_reason() {
    assert_eq!(FallbackStage::Open.as_str(), "open");
    assert_eq!(FallbackStage::MidStream.as_str(), "mid-stream");
    let fallback = HwFallback {
        stage: FallbackStage::MidStream,
        reason: "Sample read failed: device removed".to_string(),
    };
    assert_eq!(
        fallback.describe(),
        "mid-stream: Sample read failed: device removed"
    );
}

// ---------------------------------------------------------------------------
// A decode error
// ---------------------------------------------------------------------------

#[test]
fn a_decode_error_on_the_d3d_path_reopens_in_software() {
    for error in [
        DecoderError::ReadSample("device removed".into()),
        DecoderError::BufferLock("Lock2DSize failed".into()),
        DecoderError::Decode("bad bitstream".into()),
    ] {
        assert_eq!(
            on_decode_error(true, &error),
            OnDecodeError::ReopenSoftware,
            "{error}"
        );
    }
}

#[test]
fn a_reader_that_may_not_fall_back_hands_the_error_over() {
    // Software from the start, or already fallen back once.
    let error = DecoderError::ReadSample("device removed".into());
    assert_eq!(on_decode_error(false, &error), OnDecodeError::Propagate);
}

#[test]
fn a_host_out_of_memory_is_never_a_reason_to_leave_the_gpu() {
    // The pipeline drops that one picture; the GPU decoder is fine.
    let error = DecoderError::FrameAlloc(12_441_600);
    assert_eq!(on_decode_error(true, &error), OnDecodeError::Propagate);
    assert_eq!(on_decode_error(false, &error), OnDecodeError::Propagate);
}

// ---------------------------------------------------------------------------
// Where a software reopen goes on
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_reader_hands_every_picture_over_and_reopens_from_the_start() {
    let mut resume = Resume::default();
    assert_eq!(resume.reopen(), None, "nothing handed over, no seek");
    for ts in [0, 33, 66] {
        assert!(resume.take(ts), "{ts}");
    }
}

#[test]
fn a_reopen_seeks_to_the_last_picture_and_drops_the_ones_through_it() {
    let mut resume = Resume::default();
    for ts in [0, 33, 66, 100] {
        assert!(resume.take(ts));
    }
    assert_eq!(resume.reopen(), Some(100));
    // The seek lands on the keyframe before 100: decoded again, dropped.
    for ts in [0, 33, 66, 99, 100] {
        assert!(!resume.take(ts), "{ts} was handed over already");
    }
    assert!(resume.take(101), "the first new picture");
    assert!(resume.take(133));
}

#[test]
fn a_reopen_right_after_a_seek_goes_to_the_seek_target_and_drops_nothing() {
    let mut resume = Resume::default();
    assert!(resume.take(0));
    assert!(resume.take(33));
    resume.on_seek(5_000);
    assert_eq!(resume.reopen(), Some(5_000));
    // MF lands on the keyframe before 5 000; the caller asked for it.
    assert!(resume.take(4_800));
}

#[test]
fn a_seek_clears_a_pending_drop() {
    let mut resume = Resume::default();
    assert!(resume.take(2_000));
    assert_eq!(resume.reopen(), Some(2_000));
    assert!(!resume.take(1_900));
    // The caller seeks back: what comes now was asked for.
    resume.on_seek(0);
    assert!(resume.take(0));
    assert!(resume.take(33));
}

#[test]
fn a_seek_then_pictures_reopen_at_the_last_picture() {
    let mut resume = Resume::default();
    resume.on_seek(5_000);
    assert!(resume.take(4_800));
    assert!(resume.take(4_833));
    assert_eq!(resume.reopen(), Some(4_833));
    assert!(!resume.take(4_833));
    assert!(resume.take(4_866));
}

// ---------------------------------------------------------------------------
// The mapped surface
// ---------------------------------------------------------------------------

/// A 6×3 picture on a surface of 4 rows, 8 bytes apart: the luma rows at 0,
/// 8, 16; the UV plane at 8 × 4 = 32, its ⌈3/2⌉ = 2 rows at 32 and 40.
const SMALL: SurfaceLayout = SurfaceLayout {
    pitch: 8,
    surface_rows: 4,
    width: 6,
    height: 3,
};

/// Bytes up to the last UV row's last byte: 32 + 8 + 6.
const SMALL_NEEDED: usize = 46;

/// Mapped bytes that each say where they are.
fn mapped(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[test]
fn a_surface_is_packed_row_by_row_from_both_planes() {
    let copy = SMALL
        .check(DXGI_FORMAT_NV12, SMALL_NEEDED)
        .expect("a whole NV12 surface");
    assert_eq!(copy.stride, 6);
    assert_eq!(copy.len, 30, "6 × (3 + 2)");
    let src = mapped(SMALL_NEEDED);
    let mut dst = Vec::new();
    copy.copy(&src, &mut dst);
    let mut want = Vec::new();
    for start in [0, 8, 16, 32, 40] {
        want.extend_from_slice(&src[start..start + 6]);
    }
    assert_eq!(dst, want);
    assert_eq!(dst.len(), copy.len);
}

#[test]
fn the_copy_appends_to_what_the_buffer_holds() {
    let copy = SMALL.check(DXGI_FORMAT_NV12, SMALL_NEEDED).unwrap();
    let mut dst = Vec::with_capacity(copy.len + 4);
    dst.extend_from_slice(&[9, 9]);
    copy.copy(&mapped(SMALL_NEEDED), &mut dst);
    assert_eq!(dst.len(), 2 + copy.len);
    assert_eq!(&dst[..3], &[9, 9, 0]);
}

#[test]
fn an_odd_width_packs_a_whole_chroma_row() {
    // 5 pixels = 3 U/V pairs = 6 bytes per row, as the software path's rows.
    let layout = SurfaceLayout { width: 5, ..SMALL };
    let copy = layout.check(DXGI_FORMAT_NV12, SMALL_NEEDED).unwrap();
    assert_eq!((copy.stride, copy.len), (6, 30));
    let too_narrow = SurfaceLayout { pitch: 5, ..layout };
    assert_eq!(
        too_narrow.check(DXGI_FORMAT_NV12, 1 << 20),
        Err(SurfaceError::Pitch { pitch: 5, row: 6 })
    );
}

#[test]
fn a_surface_as_tall_as_the_picture_has_its_uv_plane_right_after_it() {
    // 4×2 on a 2-row surface, pitch = row: the packed picture IS the mapping.
    let layout = SurfaceLayout {
        pitch: 4,
        surface_rows: 2,
        width: 4,
        height: 2,
    };
    let copy = layout.check(DXGI_FORMAT_NV12, 12).unwrap();
    assert_eq!((copy.stride, copy.len), (4, 12));
    let src = mapped(12);
    let mut dst = Vec::new();
    copy.copy(&src, &mut dst);
    assert_eq!(dst, src);
}

#[test]
fn only_nv12_surfaces_are_read() {
    // P010 (104) is what a 10-bit stream decodes into.
    assert_eq!(SMALL.check(104, 1 << 20), Err(SurfaceError::Format(104)));
    assert!(
        SMALL.check(103, 1 << 20).is_ok(),
        "DXGI_FORMAT_NV12 is 103 (dxgiformat.h)"
    );
}

#[test]
fn an_empty_or_oversized_picture_is_refused() {
    for (width, height) in [(0, 3), (6, 0), (16_385, 3), (6, 16_385)] {
        let layout = SurfaceLayout {
            pitch: 32_768,
            surface_rows: 16_385,
            width,
            height,
        };
        assert_eq!(
            layout.check(DXGI_FORMAT_NV12, usize::MAX),
            Err(SurfaceError::Size { width, height }),
            "{width}x{height}"
        );
    }
}

#[test]
fn a_picture_16384_on_a_side_is_read() {
    let wide = SurfaceLayout {
        pitch: 16_384,
        surface_rows: 2,
        width: 16_384,
        height: 2,
    };
    let copy = wide.check(DXGI_FORMAT_NV12, 16_384 * 3).unwrap();
    assert_eq!((copy.stride, copy.len), (16_384, 16_384 * 3));
    let tall = SurfaceLayout {
        pitch: 2,
        surface_rows: 16_384,
        width: 2,
        height: 16_384,
    };
    let copy = tall
        .check(DXGI_FORMAT_NV12, 2 * 16_384 + 2 * 8_192)
        .unwrap();
    assert_eq!((copy.stride, copy.len), (2, 2 * (16_384 + 8_192)));
}

#[test]
fn a_pitch_must_hold_a_row() {
    let exact = SurfaceLayout { pitch: 6, ..SMALL };
    assert!(exact.check(DXGI_FORMAT_NV12, 1 << 20).is_ok());
    let short = SurfaceLayout { pitch: 5, ..SMALL };
    assert_eq!(
        short.check(DXGI_FORMAT_NV12, 1 << 20),
        Err(SurfaceError::Pitch { pitch: 5, row: 6 })
    );
}

#[test]
fn the_surface_must_hold_the_pictures_rows() {
    let exact = SurfaceLayout {
        surface_rows: 3,
        ..SMALL
    };
    assert!(exact.check(DXGI_FORMAT_NV12, 1 << 20).is_ok());
    let short = SurfaceLayout {
        surface_rows: 2,
        ..SMALL
    };
    assert_eq!(
        short.check(DXGI_FORMAT_NV12, 1 << 20),
        Err(SurfaceError::Rows {
            surface_rows: 2,
            height: 3
        })
    );
}

#[test]
fn the_mapping_must_reach_the_last_uv_rows_last_byte() {
    assert!(SMALL.check(DXGI_FORMAT_NV12, SMALL_NEEDED).is_ok());
    assert_eq!(
        SMALL.check(DXGI_FORMAT_NV12, SMALL_NEEDED - 1),
        Err(SurfaceError::Short {
            len: SMALL_NEEDED - 1,
            needed: SMALL_NEEDED
        })
    );
}

#[test]
fn a_surface_no_mapping_could_hold_is_short() {
    // pitch × rows overflows.
    let huge = SurfaceLayout {
        pitch: usize::MAX,
        surface_rows: 3,
        ..SMALL
    };
    assert_eq!(
        huge.check(DXGI_FORMAT_NV12, usize::MAX),
        Err(SurfaceError::Short {
            len: usize::MAX,
            needed: usize::MAX
        })
    );
    // pitch × rows fits, the UV row after it does not.
    let edge = SurfaceLayout {
        pitch: usize::MAX / 2,
        surface_rows: 2,
        width: 6,
        height: 2,
    };
    assert_eq!(
        edge.check(DXGI_FORMAT_NV12, usize::MAX),
        Err(SurfaceError::Short {
            len: usize::MAX,
            needed: usize::MAX
        })
    );
}

#[test]
fn the_mapped_bytes_count_from_scanline_0_to_the_mappings_end() {
    assert_eq!(mapped_from_scanline0(1_000, 1_000, 46), Some(46));
    assert_eq!(mapped_from_scanline0(1_000, 1_010, 46), Some(36));
    assert_eq!(mapped_from_scanline0(1_000, 1_046, 46), Some(0));
    assert_eq!(
        mapped_from_scanline0(1_000, 1_047, 46),
        None,
        "past its end"
    );
    assert_eq!(mapped_from_scanline0(1_000, 999, 46), None, "before it");
}

// ---------------------------------------------------------------------------
// The counters
// ---------------------------------------------------------------------------

#[test]
fn the_counters_count_each_outcome_on_its_own() {
    let counters = HwCounters::zero();
    assert_eq!(counters.snapshot(), HwDecodeStats::default());
    counters.requested();
    counters.requested();
    counters.requested();
    counters.requested();
    counters.first_picture(DecodePath::Hardware);
    counters.first_picture(DecodePath::Hardware);
    counters.first_picture(DecodePath::Software);
    counters.fell_back(&HwFallback {
        stage: FallbackStage::Open,
        reason: "no GPU adapter".into(),
    });
    counters.fell_back(&HwFallback {
        stage: FallbackStage::MidStream,
        reason: "device removed".into(),
    });
    counters.fell_back(&HwFallback {
        stage: FallbackStage::MidStream,
        reason: "device hung".into(),
    });
    assert_eq!(
        counters.snapshot(),
        HwDecodeStats {
            requested: 4,
            hardware: 2,
            mf_software: 1,
            open_fallbacks: 1,
            mid_stream_fallbacks: 2,
            last_fallback: Some("mid-stream: device hung".into()),
        }
    );
}

#[test]
fn the_process_counters_are_one_instance() {
    assert!(std::ptr::eq(hw_counters(), hw_counters()));
    let before = hw_counters().snapshot().requested;
    hw_counters().requested();
    // Other tests never touch the process counters, but stay monotonic-safe.
    assert!(hw_counters().snapshot().requested > before);
}
