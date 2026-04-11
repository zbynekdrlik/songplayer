# NDI Full-Quality Rewrite Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make SongPlayer's NDI output audibly correct, frame-accurately paced, and CPU-efficient by fixing audio FLTP format, enabling NDI clock_video pacing, passing NV12 video through end-to-end, and adopting the async send_video_v2 double-buffer pattern.

**Architecture:** Three-crate change along the existing pipeline. `sp-ndi` gains the FLTP audio FourCC with planar deinterleaving, a native NV12 video FourCC, and an extended `NdiBackend` trait with `send_create_with_clocking`, `send_video_async`, and `send_video_flush`. `sp-decoder` drops its NV12→BGRA CPU conversion, returns raw NV12 bytes, and exposes the real frame rate parsed from Windows Media Foundation. `sp-server/playback/pipeline.rs` creates its NDI sender with `clock_video=true`, reads the real frame rate from the decoder, deletes the manual `thread::sleep` pacing, sends audio before video for each synced tuple, keeps the previous frame alive across iterations for the async buffer ownership rule, and flushes on every exit path.

**Tech Stack:** Rust 2024, Tauri 2, libloading 0.8 (NDI SDK runtime loader), windows 0.58 (Media Foundation), tokio 1, crossbeam-channel 0.5, cargo-mutants (zero-survivor gate).

**Spec:** `docs/superpowers/specs/2026-04-10-ndi-full-quality-rewrite-design.md`

---

## File structure

Files created:
- `crates/sp-ndi/src/deinterleave.rs` — pure function converting interleaved float audio to planar layout, with its own test module
- `crates/sp-server/src/playback/submitter.rs` — cross-platform `FrameSubmitter` helper that owns the NDI sender and the previous video frame's buffer, enforces the audio-before-video ordering, and handles flush on exit. Tested with the mock backend on any platform.

Files modified:
- `crates/sp-ndi/Cargo.toml` — adds a `test-util` feature and a dev-dep on `mutants`
- `crates/sp-ndi/src/types.rs` — `FourCCAudioType::FLTP` replaces `FltInterleaved`; `FourCCVideoType::NV12` added; `PixelFormat` enum added
- `crates/sp-ndi/src/ndi_sdk.rs` — resolves `NDIlib_send_send_video_async_v2`
- `crates/sp-ndi/src/sender.rs` — trait gains `send_create_with_clocking`, `send_video_async`, `send_video_flush`, `send_video` gains a `four_cc` parameter; `RealNdiBackend` implements them all; `MockNdiBackend` moves to a `test_util` submodule gated on `cfg(any(test, feature = "test-util"))`; `NdiSender` gains `new_with_clocking`, `send_video_async`, `send_video_flush`, and a scratch buffer; `VideoFrame` gains `pixel_format`; `Drop` flushes before destroy
- `crates/sp-ndi/src/lib.rs` — re-exports `PixelFormat`, exposes `test_util::MockNdiBackend` under the feature, updates the existing FourCC tests
- `crates/sp-decoder/Cargo.toml` — dev-dep on `mutants`
- `crates/sp-decoder/src/types.rs` — adds `PixelFormat` (one variant `Nv12`) and `VideoStreamInfo { width, height, pixel_format, frame_rate_num, frame_rate_den }`; `DecodedVideoFrame` gains `pixel_format`
- `crates/sp-decoder/src/reader.rs` — `MediaReader::open` parses `MF_MT_FRAME_RATE` and stores it; `next_video_frame` stops calling `nv12_to_bgra` and returns raw NV12 bytes with `stride = y_plane_stride`; `video_info()` method added; `nv12_to_bgra` function removed
- `crates/sp-decoder/src/sync.rs` — `SyncedDecoder::video_info()` forwards the reader's info
- `crates/sp-server/Cargo.toml` — dev-dep on `sp-ndi` gains the `test-util` feature
- `crates/sp-server/src/playback/mod.rs` — declares `mod submitter`
- `crates/sp-server/src/playback/pipeline.rs` — creates the sender via `new_with_clocking(_, true, false)`, builds a `FrameSubmitter` per Play, reads real frame rate from decoder, deletes manual pacing, uses `submitter.submit()` per synced tuple, calls `submitter.flush()` on every exit branch
- `VERSION` — `0.8.0-dev.1` → `0.8.0-dev.2`
- `Cargo.toml` (workspace root) — version bump
- `sp-ui/Cargo.toml` — version bump
- `src-tauri/Cargo.toml` — version bump
- `src-tauri/tauri.conf.json` — version bump
- `.github/workflows/ci.yml` — removes `sp-ndi/`, `sp-decoder/`, and `sp-server/src/playback/pipeline\.rs` from the mutation-testing exclude list

---

## Task 1: Version bump to 0.8.0-dev.2

**Files:**
- Modify: `VERSION`
- Modify: `Cargo.toml`, `sp-ui/Cargo.toml`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json` via the script

- [ ] **Step 1: Check current version on dev and main**

Run:
```bash
git fetch origin && cat VERSION && git show origin/main:VERSION
```
Expected: `dev` shows `0.8.0-dev.1`, `origin/main` shows `0.8.0` (or whatever the last released version is — the important thing is that dev is currently not strictly higher than main for the NEXT intended bump).

- [ ] **Step 2: Write new VERSION**

Overwrite `VERSION` with exactly:
```
0.8.0-dev.2
```
(one line, no extra whitespace)

- [ ] **Step 3: Sync version across the workspace**

Run:
```bash
./scripts/sync-version.sh
```
Expected output mentions `Cargo.toml`, `sp-ui/Cargo.toml`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json` being updated.

- [ ] **Step 4: Verify the version is consistent**

Run:
```bash
grep -E '^version' Cargo.toml sp-ui/Cargo.toml src-tauri/Cargo.toml
grep '"version"' src-tauri/tauri.conf.json
```
Expected: every line shows `0.8.0-dev.2`.

- [ ] **Step 5: Commit**

```bash
git add VERSION Cargo.toml sp-ui/Cargo.toml src-tauri/Cargo.toml src-tauri/tauri.conf.json
git commit -m "chore: bump version to 0.8.0-dev.2 for NDI rewrite"
```

---

## Task 2: sp-ndi FourCC values — FLTP audio, NV12 video, PixelFormat enum

**Files:**
- Modify: `crates/sp-ndi/src/types.rs`
- Modify: `crates/sp-ndi/src/lib.rs` (update existing FourCC tests)
- Test: inside the same files

- [ ] **Step 1: Replace the audio FourCC enum and add NV12 + PixelFormat**

Edit `crates/sp-ndi/src/types.rs`. Locate the existing `FourCCAudioType` and `FourCCVideoType` declarations. Replace them with:

```rust
/// Video FourCC pixel format identifiers.
///
/// Values are `NDI_LIB_FOURCC(ch0,ch1,ch2,ch3) = ch0 | (ch1<<8) | (ch2<<16) | (ch3<<24)`
/// so on little-endian the bytes in memory literally spell the ASCII chars.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FourCCVideoType {
    /// BGRA 8-bit per channel. `'B','G','R','A'`.
    BGRA = 0x4152_4742,
    /// NV12 planar luma + interleaved chroma (semi-planar). `'N','V','1','2'`.
    NV12 = 0x3231_564E,
}

/// Audio FourCC format identifiers.
///
/// Only FLTP is defined by the NDI SDK v6. See `NDIlib_FourCC_audio_type_e`
/// in `Processing.NDI.structs.h`.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FourCCAudioType {
    /// Planar 32-bit float. `'F','L','T','p'`.
    /// Must specify `channel_stride_in_bytes = no_samples * 4`.
    FLTP = 0x7054_4C46,
}

/// High-level pixel format tag on `sp_ndi::VideoFrame`.
///
/// Used by callers to tell the NDI sender which FourCC and stride semantics
/// to apply to the raw bytes in the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// BGRA 8-bit per channel, `stride = width * 4`.
    Bgra,
    /// NV12 semi-planar, `stride = y_plane_row_bytes` (usually `= width`).
    /// Data layout: Y plane of `height` rows, then UV plane of `height/2` rows.
    /// Total bytes: `width * height * 3 / 2`.
    Nv12,
}
```

- [ ] **Step 2: Update the FourCC tests in `crates/sp-ndi/src/lib.rs`**

Find `fourcc_audio_flt_interleaved_value` and the existing `fourcc_video_bgra_value`. Replace them with this block (delete the old ones):

```rust
#[test]
fn fourcc_video_bgra_value_and_bytes() {
    assert_eq!(FourCCVideoType::BGRA as u32, 0x4152_4742);
    let bytes = (FourCCVideoType::BGRA as u32).to_le_bytes();
    assert_eq!(&bytes, b"BGRA");
}

#[test]
fn fourcc_video_nv12_value_and_bytes() {
    assert_eq!(FourCCVideoType::NV12 as u32, 0x3231_564E);
    let bytes = (FourCCVideoType::NV12 as u32).to_le_bytes();
    assert_eq!(&bytes, b"NV12");
}

#[test]
fn fourcc_audio_fltp_value_and_bytes() {
    assert_eq!(FourCCAudioType::FLTP as u32, 0x7054_4C46);
    let bytes = (FourCCAudioType::FLTP as u32).to_le_bytes();
    assert_eq!(&bytes, b"FLTp");
}

#[test]
fn pixel_format_variants_are_distinct() {
    assert_ne!(types::PixelFormat::Bgra, types::PixelFormat::Nv12);
}
```

Also update the existing `use types::{...}` to include `PixelFormat` if it isn't already covered by a `use super::*`.

- [ ] **Step 3: Re-export `PixelFormat` from the crate root**

Edit `crates/sp-ndi/src/lib.rs`. Update the `types::*` re-export:

```rust
pub use types::{
    FRAME_FORMAT_PROGRESSIVE, FourCCAudioType, FourCCVideoType,
    NDI_SEND_TIMECODE_SYNTHESIZE, PixelFormat,
};
```

- [ ] **Step 4: Run the tests (they must compile and pass)**

Run:
```bash
cargo test -p sp-ndi --lib
```
Expected: all sp-ndi tests pass including the four new FourCC/PixelFormat tests. The older `fourcc_audio_flt_interleaved_value` test is gone — that is intended.

If anything referencing `FltInterleaved` fails to compile (it will, in `sender.rs:205`), leave it failing for now — Task 5 fixes it. Instead of running `cargo test`, run:

```bash
cargo test -p sp-ndi --lib --no-run 2>&1 | tail -20
```
Expected: compile errors referencing `FltInterleaved` in `sender.rs`. That's OK; it will be fixed in Task 5. Do NOT commit yet.

- [ ] **Step 5: Immediately apply the one-line fix in `sender.rs` so this task's commit compiles**

In `crates/sp-ndi/src/sender.rs`, find the line:
```rust
four_cc: FourCCAudioType::FltInterleaved,
```
Replace with:
```rust
four_cc: FourCCAudioType::FLTP,
```

This is the minimum change to keep the crate compiling. Task 5 will add the proper planar conversion around this line.

- [ ] **Step 6: Run the tests again — all must pass**

```bash
cargo test -p sp-ndi --lib
```
Expected: all sp-ndi lib tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/sp-ndi/src/types.rs crates/sp-ndi/src/lib.rs crates/sp-ndi/src/sender.rs
git commit -m "feat(sp-ndi): add FLTP audio + NV12 video FourCC + PixelFormat enum"
```

---

## Task 3: sp-ndi deinterleave helper

**Files:**
- Create: `crates/sp-ndi/src/deinterleave.rs`
- Modify: `crates/sp-ndi/src/lib.rs` (add `mod deinterleave;`)

- [ ] **Step 1: Create `crates/sp-ndi/src/deinterleave.rs` with the failing test first**

```rust
//! Interleaved → planar float audio conversion for NDI FLTP output.
//!
//! NDI's `NDIlib_FourCC_audio_type_FLTP` expects the audio buffer laid out
//! channel-by-channel, e.g. stereo:
//!
//!   `[L0 L1 L2 … L_{n-1}][R0 R1 R2 … R_{n-1}]`
//!
//! Windows Media Foundation delivers interleaved:
//!
//!   `[L0 R0 L1 R1 L2 R2 … L_{n-1} R_{n-1}]`
//!
//! This module provides a zero-allocation-in-steady-state conversion that
//! reuses a caller-owned scratch `Vec<f32>`.

/// Convert interleaved multi-channel audio into planar layout.
///
/// * `interleaved` — `[ch0_s0, ch1_s0, …, ch_{c-1}_s0, ch0_s1, …]`
/// * `channels` — number of channels (must be > 0 and must divide `interleaved.len()`)
/// * `out` — destination scratch buffer; cleared and resized to hold the output
///
/// If `channels == 0` or `interleaved` is empty, `out` is cleared and the
/// function returns.
pub fn deinterleave(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    if channels == 0 || interleaved.is_empty() {
        out.clear();
        return;
    }
    let samples_per_channel = interleaved.len() / channels;
    let total = channels * samples_per_channel;
    out.clear();
    out.resize(total, 0.0);
    for ch in 0..channels {
        for s in 0..samples_per_channel {
            out[ch * samples_per_channel + s] = interleaved[s * channels + ch];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_clears_output() {
        let mut out = vec![1.0, 2.0, 3.0];
        deinterleave(&[], 2, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn zero_channels_clears_output() {
        let mut out = vec![1.0];
        deinterleave(&[1.0, 2.0], 0, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn mono_is_passthrough() {
        let input = vec![1.0, 2.0, 3.0, 4.0];
        let mut out = Vec::new();
        deinterleave(&input, 1, &mut out);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn stereo_four_samples() {
        // Interleaved: L0 R0 L1 R1 L2 R2 L3 R3
        let input = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let mut out = Vec::new();
        deinterleave(&input, 2, &mut out);
        // Planar: L0 L1 L2 L3 R0 R1 R2 R3
        assert_eq!(out, vec![1.0, 3.0, 5.0, 7.0, 2.0, 4.0, 6.0, 8.0]);
    }

    #[test]
    fn six_channel_preserves_sample_count_and_order() {
        // 2 samples per channel, 6 channels = 5.1 layout
        // Interleaved: s0(c0..c5) s1(c0..c5)
        let input: Vec<f32> = (0..12).map(|x| x as f32).collect();
        let mut out = Vec::new();
        deinterleave(&input, 6, &mut out);
        // Planar: c0_s0 c0_s1 c1_s0 c1_s1 … c5_s0 c5_s1
        // c0: input[0], input[6]
        // c1: input[1], input[7]
        // …
        assert_eq!(out, vec![
            0.0, 6.0,   // c0
            1.0, 7.0,   // c1
            2.0, 8.0,   // c2
            3.0, 9.0,   // c3
            4.0, 10.0,  // c4
            5.0, 11.0,  // c5
        ]);
    }

    #[test]
    fn preserves_exact_float_bits() {
        // Use non-round bit patterns to catch any accidental arithmetic.
        let a = f32::from_bits(0x3E8A_3D71); // ~0.27
        let b = f32::from_bits(0xBF19_999A); // ~-0.6
        let c = f32::from_bits(0x4049_0FDB); // ~3.1416
        let d = f32::from_bits(0xC0A0_0000); // -5.0
        let input = vec![a, b, c, d];
        let mut out = Vec::new();
        deinterleave(&input, 2, &mut out);
        // Stereo: L0=a L1=c R0=b R1=d
        assert_eq!(out[0].to_bits(), a.to_bits());
        assert_eq!(out[1].to_bits(), c.to_bits());
        assert_eq!(out[2].to_bits(), b.to_bits());
        assert_eq!(out[3].to_bits(), d.to_bits());
    }

    #[test]
    fn reuses_scratch_buffer_capacity_on_second_call() {
        let mut out = Vec::with_capacity(16);
        let cap_before = out.capacity();
        // First call: 4 samples × 2ch = 8 floats
        deinterleave(&[1.0; 8], 2, &mut out);
        assert_eq!(out.len(), 8);
        // Second call: 2 samples × 2ch = 4 floats (smaller, must not realloc)
        deinterleave(&[2.0; 4], 2, &mut out);
        assert_eq!(out.len(), 4);
        assert!(out.capacity() >= cap_before);
        // Third call: grow to 16 — may or may not realloc, both are fine.
        deinterleave(&[3.0; 16], 2, &mut out);
        assert_eq!(out.len(), 16);
    }
}
```

- [ ] **Step 2: Add `mod deinterleave;` to `crates/sp-ndi/src/lib.rs`**

Insert near the other `pub mod` declarations, e.g.:
```rust
pub mod deinterleave;
pub mod error;
pub mod ndi_sdk;
pub mod sender;
pub mod types;
```

- [ ] **Step 3: Run the tests**

```bash
cargo test -p sp-ndi --lib deinterleave
```
Expected: 7 passed, 0 failed.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-ndi/src/deinterleave.rs crates/sp-ndi/src/lib.rs
git commit -m "feat(sp-ndi): add interleaved→planar float audio helper"
```

---

## Task 4: sp-ndi ndi_sdk — resolve NDIlib_send_send_video_async_v2

**Files:**
- Modify: `crates/sp-ndi/src/ndi_sdk.rs`

- [ ] **Step 1: Add the function pointer type and resolved field**

Edit `crates/sp-ndi/src/ndi_sdk.rs`. In the "Function-pointer type aliases" block near the top, add after `FnSendVideoV2`:

```rust
type FnSendVideoAsyncV2 =
    unsafe extern "C" fn(*mut NDIlib_send_instance_t, *const NDIlib_video_frame_v2_t);
```

In the `NdiLib` struct, add below `pub(crate) send_send_video_v2: FnSendVideoV2,`:

```rust
pub(crate) send_send_video_async_v2: FnSendVideoAsyncV2,
```

In `NdiLib::load()` unsafe block, below the `send_send_video_v2` resolution, add:

```rust
let send_send_video_async_v2 = Self::resolve::<FnSendVideoAsyncV2>(
    &library,
    b"NDIlib_send_send_video_async_v2\0",
)?;
```

And include it in the `Ok(Self { … })` struct literal:

```rust
send_send_video_async_v2,
```

- [ ] **Step 2: Run the sp-ndi tests**

```bash
cargo test -p sp-ndi
```
Expected: all tests pass. The `ndi_lib_load_fails_gracefully_without_sdk` test still returns `LibraryNotFound` because no NDI SDK is installed on the CI runner.

- [ ] **Step 3: Commit**

```bash
git add crates/sp-ndi/src/ndi_sdk.rs
git commit -m "feat(sp-ndi): resolve NDIlib_send_send_video_async_v2 symbol"
```

---

## Task 5: sp-ndi — extend NdiBackend trait with FLTP audio + async video + clocking, implement in Mock and Real backends, expose MockNdiBackend via test-util feature

**Files:**
- Modify: `crates/sp-ndi/Cargo.toml` (add `test-util` feature + dev-dep on `mutants`)
- Modify: `crates/sp-ndi/src/sender.rs` (trait + backends + move Mock to `test_util`)
- Modify: `crates/sp-ndi/src/lib.rs` (pub mod test_util)

- [ ] **Step 1: Update `crates/sp-ndi/Cargo.toml`**

Replace the whole file with:

```toml
[package]
name = "sp-ndi"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[features]
# Exposes the in-process `MockNdiBackend` for downstream test crates.
test-util = []

[dependencies]
libloading = { workspace = true }
tracing = { workspace = true }
thiserror = { workspace = true }

[dev-dependencies]
# Placeholder crate that defines the `mutants::skip` attribute as a no-op,
# letting cargo-mutants exclude specific functions from mutation testing.
mutants = "0.0.3"
```

- [ ] **Step 2: Rewrite `crates/sp-ndi/src/sender.rs`**

This is the biggest change in the plan. Replace the entire file contents with the block below. Read each section; the key points are:

- `VideoFrame` gains `pixel_format: PixelFormat`
- `NdiBackend` trait gains `send_create_with_clocking`, `send_video_async`, `send_video_flush`, and the video methods gain a `four_cc: FourCCVideoType` parameter
- `RealNdiBackend` owns a per-sender `Mutex<Vec<f32>>` deinterleave scratch, keyed by handle id, and dispatches audio via `FLTP` planar + correct stride
- `RealNdiBackend::send_video_async` calls `NDIlib_send_send_video_async_v2`
- `RealNdiBackend::send_video_flush` calls `NDIlib_send_send_video_async_v2` with `std::ptr::null()`
- `NdiSender::new_with_clocking` is the new constructor; `NdiSender::new` stays as a thin wrapper around `new_with_clocking(backend, name, false, false)`
- `NdiSender::send_video_async` + `send_video_flush` public API
- `NdiSender::Drop` calls `send_video_flush` before `send_destroy`
- `MockNdiBackend` moves into a `pub mod test_util { … }` submodule gated on `#[cfg(any(test, feature = "test-util"))]` and records every call including the new ones

```rust
//! High-level NDI sender with a mockable backend trait.

use std::collections::HashMap;
use std::ffi::CString;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tracing::{debug, info};

use crate::deinterleave::deinterleave;
use crate::error::NdiError;
use crate::ndi_sdk::NdiLib;
use crate::types::{
    FRAME_FORMAT_PROGRESSIVE, FourCCAudioType, FourCCVideoType, NDI_SEND_TIMECODE_SYNTHESIZE,
    NDIlib_audio_frame_v3_t, NDIlib_send_create_t, NDIlib_send_instance_t, NDIlib_tally_t,
    NDIlib_video_frame_v2_t, PixelFormat,
};

// ---------------------------------------------------------------------------
// Safe public frame types
// ---------------------------------------------------------------------------

/// A video frame ready to send over NDI.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Raw pixel data in the layout required by `pixel_format`.
    pub data: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per scan line. For BGRA: `width * 4`. For NV12: Y-plane row bytes
    /// (usually `width`).
    pub stride: u32,
    /// Frame rate numerator (e.g. 30 for 30 fps, 30000 for 29.97).
    pub frame_rate_n: i32,
    /// Frame rate denominator (e.g. 1 for 30 fps, 1001 for 29.97).
    pub frame_rate_d: i32,
    /// Pixel format. Determines the FourCC sent to NDI and the stride semantic.
    pub pixel_format: PixelFormat,
}

/// An audio frame ready to send over NDI. Data is interleaved f32 PCM — the
/// sender converts to planar FLTP internally.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Interleaved float samples `[c0_s0, c1_s0, …, c0_s1, c1_s1, …]`.
    pub data: Vec<f32>,
    /// Number of audio channels.
    pub channels: u32,
    /// Sample rate in Hz.
    pub sample_rate: u32,
}

/// Tally state — whether this source is on program / preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub on_program: bool,
    pub on_preview: bool,
}

// ---------------------------------------------------------------------------
// Backend trait (for mockability)
// ---------------------------------------------------------------------------

/// Abstraction over the NDI SDK for testing.
pub trait NdiBackend: Send + Sync {
    /// Create a sender with the default (no clocking) flags. Convenience wrapper.
    fn send_create(&self, name: &str) -> Result<usize, NdiError> {
        self.send_create_with_clocking(name, false, false)
    }

    /// Create a sender with explicit `clock_video` / `clock_audio` flags.
    fn send_create_with_clocking(
        &self,
        name: &str,
        clock_video: bool,
        clock_audio: bool,
    ) -> Result<usize, NdiError>;

    /// Destroy a sender instance.
    fn send_destroy(&self, handle: usize);

    /// Send a video frame synchronously (blocks if the sender was created with
    /// `clock_video = true`).
    #[allow(clippy::too_many_arguments)]
    fn send_video(
        &self,
        handle: usize,
        four_cc: FourCCVideoType,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        data: &[u8],
    );

    /// Schedule a video frame for asynchronous send. The caller MUST keep the
    /// `data` buffer alive until the next call to `send_video_async`,
    /// `send_video`, `send_video_flush`, or `send_destroy`.
    #[allow(clippy::too_many_arguments)]
    fn send_video_async(
        &self,
        handle: usize,
        four_cc: FourCCVideoType,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        data: &[u8],
    );

    /// Flush the last async frame by calling `send_send_video_async_v2(NULL)`.
    /// After this returns, the previous frame's buffer may be freed.
    fn send_video_flush(&self, handle: usize);

    /// Send an audio frame. The backend is responsible for converting the
    /// interleaved float input into the planar FLTP layout NDI requires.
    fn send_audio(
        &self,
        handle: usize,
        sample_rate: i32,
        channels: i32,
        samples_per_channel: i32,
        interleaved: &[f32],
    );

    /// Query tally state. Returns `None` if the timeout expired with no change.
    fn send_get_tally(&self, handle: usize, timeout_ms: u32) -> Option<(bool, bool)>;
}

// ---------------------------------------------------------------------------
// Real backend (wraps NdiLib)
// ---------------------------------------------------------------------------

/// Per-handle state kept by the real backend.
struct RealHandleState {
    ptr: *mut NDIlib_send_instance_t,
    /// Planar audio scratch buffer — reused to avoid per-frame allocation.
    audio_scratch: Vec<f32>,
}

// SAFETY: the raw NDI pointer is only touched through NDI SDK calls which are
// thread-safe per sender instance. The scratch Vec is a plain owned buffer.
unsafe impl Send for RealHandleState {}

/// Production [`NdiBackend`] backed by the real NDI SDK via [`NdiLib`].
pub struct RealNdiBackend {
    lib: Arc<NdiLib>,
    next_id: AtomicUsize,
    handles: Mutex<HashMap<usize, RealHandleState>>,
}

unsafe impl Send for RealNdiBackend {}
unsafe impl Sync for RealNdiBackend {}

impl RealNdiBackend {
    /// Create a new backend from an already-loaded NDI SDK.
    pub fn new(lib: Arc<NdiLib>) -> Self {
        Self {
            lib,
            next_id: AtomicUsize::new(1),
            handles: Mutex::new(HashMap::new()),
        }
    }

    fn build_video_frame(
        four_cc: FourCCVideoType,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        data: *const u8,
    ) -> NDIlib_video_frame_v2_t {
        NDIlib_video_frame_v2_t {
            xres: width,
            yres: height,
            four_cc,
            frame_rate_n,
            frame_rate_d,
            picture_aspect_ratio: 0.0,
            frame_format_type: FRAME_FORMAT_PROGRESSIVE,
            timecode: NDI_SEND_TIMECODE_SYNTHESIZE,
            p_data: data,
            line_stride_in_bytes: stride,
            p_metadata: ptr::null(),
            timestamp: 0,
        }
    }
}

impl NdiBackend for RealNdiBackend {
    fn send_create_with_clocking(
        &self,
        name: &str,
        clock_video: bool,
        clock_audio: bool,
    ) -> Result<usize, NdiError> {
        let c_name = CString::new(name).map_err(|_| NdiError::InitFailed)?;

        let create_desc = NDIlib_send_create_t {
            p_ndi_name: c_name.as_ptr(),
            p_groups: ptr::null(),
            clock_video,
            clock_audio,
        };

        let ptr = unsafe { (self.lib.send_create)(&create_desc) };
        if ptr.is_null() {
            return Err(NdiError::InitFailed);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().unwrap().insert(
            id,
            RealHandleState {
                ptr,
                audio_scratch: Vec::new(),
            },
        );
        info!(
            "Created NDI sender '{name}' handle={id} clock_video={clock_video} clock_audio={clock_audio}"
        );
        Ok(id)
    }

    fn send_destroy(&self, handle: usize) {
        if let Some(state) = self.handles.lock().unwrap().remove(&handle) {
            debug!("Destroying NDI sender handle {handle}");
            unsafe {
                (self.lib.send_destroy)(state.ptr);
            }
        }
    }

    fn send_video(
        &self,
        handle: usize,
        four_cc: FourCCVideoType,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        data: &[u8],
    ) {
        let handles = self.handles.lock().unwrap();
        let Some(state) = handles.get(&handle) else {
            return;
        };
        let frame = Self::build_video_frame(
            four_cc, width, height, stride, frame_rate_n, frame_rate_d, data.as_ptr(),
        );
        unsafe {
            (self.lib.send_send_video_v2)(state.ptr, &frame);
        }
    }

    fn send_video_async(
        &self,
        handle: usize,
        four_cc: FourCCVideoType,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        data: &[u8],
    ) {
        let handles = self.handles.lock().unwrap();
        let Some(state) = handles.get(&handle) else {
            return;
        };
        let frame = Self::build_video_frame(
            four_cc, width, height, stride, frame_rate_n, frame_rate_d, data.as_ptr(),
        );
        unsafe {
            (self.lib.send_send_video_async_v2)(state.ptr, &frame);
        }
    }

    fn send_video_flush(&self, handle: usize) {
        let handles = self.handles.lock().unwrap();
        let Some(state) = handles.get(&handle) else {
            return;
        };
        unsafe {
            (self.lib.send_send_video_async_v2)(state.ptr, ptr::null());
        }
    }

    fn send_audio(
        &self,
        handle: usize,
        sample_rate: i32,
        channels: i32,
        samples_per_channel: i32,
        interleaved: &[f32],
    ) {
        if channels <= 0 || samples_per_channel <= 0 || interleaved.is_empty() {
            return;
        }
        let mut handles = self.handles.lock().unwrap();
        let Some(state) = handles.get_mut(&handle) else {
            return;
        };

        // Deinterleave into the per-sender scratch buffer.
        deinterleave(interleaved, channels as usize, &mut state.audio_scratch);

        let frame = NDIlib_audio_frame_v3_t {
            sample_rate,
            no_channels: channels,
            no_samples: samples_per_channel,
            timecode: NDI_SEND_TIMECODE_SYNTHESIZE,
            four_cc: FourCCAudioType::FLTP,
            p_data: state.audio_scratch.as_ptr(),
            channel_stride_in_bytes: samples_per_channel * 4,
            p_metadata: ptr::null(),
            timestamp: 0,
        };

        unsafe {
            (self.lib.send_send_audio_v3)(state.ptr, &frame);
        }
    }

    fn send_get_tally(&self, handle: usize, timeout_ms: u32) -> Option<(bool, bool)> {
        let handles = self.handles.lock().unwrap();
        let state = handles.get(&handle)?;

        let mut tally = NDIlib_tally_t::default();
        let changed = unsafe { (self.lib.send_get_tally)(state.ptr, &mut tally, timeout_ms) };
        if changed {
            Some((tally.on_program, tally.on_preview))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// NdiSender — high-level sender that wraps a backend
// ---------------------------------------------------------------------------

/// High-level NDI sender.
///
/// Generic over `B: NdiBackend` so tests can inject a mock.
/// On [`Drop`], the sender flushes any pending async frame and destroys the
/// underlying NDI instance.
pub struct NdiSender<B: NdiBackend> {
    backend: Arc<B>,
    handle: usize,
}

impl<B: NdiBackend> NdiSender<B> {
    /// Create a sender with no clocking (backwards-compatible default).
    pub fn new(backend: Arc<B>, name: &str) -> Result<Self, NdiError> {
        Self::new_with_clocking(backend, name, false, false)
    }

    /// Create a sender with explicit clocking flags. For single-threaded
    /// video+audio submission, `clock_video=true, clock_audio=false` is the
    /// SDK-recommended configuration.
    pub fn new_with_clocking(
        backend: Arc<B>,
        name: &str,
        clock_video: bool,
        clock_audio: bool,
    ) -> Result<Self, NdiError> {
        let handle = backend.send_create_with_clocking(name, clock_video, clock_audio)?;
        Ok(Self { backend, handle })
    }

    /// Send a video frame synchronously.
    pub fn send_video(&self, frame: &VideoFrame) {
        let four_cc = match frame.pixel_format {
            PixelFormat::Bgra => FourCCVideoType::BGRA,
            PixelFormat::Nv12 => FourCCVideoType::NV12,
        };
        self.backend.send_video(
            self.handle,
            four_cc,
            frame.width as i32,
            frame.height as i32,
            frame.stride as i32,
            frame.frame_rate_n,
            frame.frame_rate_d,
            &frame.data,
        );
    }

    /// Schedule a video frame for async send. The caller must keep the frame
    /// buffer alive until the next async/sync/flush/drop call.
    pub fn send_video_async(&self, frame: &VideoFrame) {
        let four_cc = match frame.pixel_format {
            PixelFormat::Bgra => FourCCVideoType::BGRA,
            PixelFormat::Nv12 => FourCCVideoType::NV12,
        };
        self.backend.send_video_async(
            self.handle,
            four_cc,
            frame.width as i32,
            frame.height as i32,
            frame.stride as i32,
            frame.frame_rate_n,
            frame.frame_rate_d,
            &frame.data,
        );
    }

    /// Release any pending async frame. Must be called before dropping the
    /// buffer of the last async frame.
    pub fn send_video_flush(&self) {
        self.backend.send_video_flush(self.handle);
    }

    /// Send an audio frame.
    pub fn send_audio(&self, frame: &AudioFrame) {
        if frame.channels == 0 {
            return;
        }
        let samples_per_channel = frame.data.len() as i32 / frame.channels as i32;
        self.backend.send_audio(
            self.handle,
            frame.sample_rate as i32,
            frame.channels as i32,
            samples_per_channel,
            &frame.data,
        );
    }

    /// Query the tally state (program / preview) with a timeout in milliseconds.
    pub fn get_tally(&self, timeout_ms: u32) -> Option<Tally> {
        self.backend
            .send_get_tally(self.handle, timeout_ms)
            .map(|(on_program, on_preview)| Tally {
                on_program,
                on_preview,
            })
    }

    /// Return the internal handle ID (useful for tests).
    pub fn handle(&self) -> usize {
        self.handle
    }
}

impl<B: NdiBackend> Drop for NdiSender<B> {
    fn drop(&mut self) {
        // Flush any pending async frame before destroying — guarantees the SDK
        // has released its pointer to our last buffer.
        self.backend.send_video_flush(self.handle);
        self.backend.send_destroy(self.handle);
    }
}

// ---------------------------------------------------------------------------
// Mock backend — exposed under the `test-util` feature for downstream tests.
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "test-util"))]
pub mod test_util {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// A mock backend that records every call for assertion.
    #[derive(Default)]
    pub struct MockNdiBackend {
        calls: StdMutex<Vec<String>>,
        tally_response: StdMutex<Option<(bool, bool)>>,
        last_audio_planar: StdMutex<Vec<f32>>,
    }

    impl MockNdiBackend {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        pub fn last_audio_planar(&self) -> Vec<f32> {
            self.last_audio_planar.lock().unwrap().clone()
        }

        pub fn set_tally(&self, on_program: bool, on_preview: bool) {
            *self.tally_response.lock().unwrap() = Some((on_program, on_preview));
        }
    }

    impl NdiBackend for MockNdiBackend {
        fn send_create_with_clocking(
            &self,
            name: &str,
            clock_video: bool,
            clock_audio: bool,
        ) -> Result<usize, NdiError> {
            self.calls.lock().unwrap().push(format!(
                "send_create_with_clocking({name},{clock_video},{clock_audio})"
            ));
            Ok(42)
        }

        fn send_destroy(&self, handle: usize) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("send_destroy({handle})"));
        }

        fn send_video(
            &self,
            handle: usize,
            four_cc: FourCCVideoType,
            width: i32,
            height: i32,
            stride: i32,
            frame_rate_n: i32,
            frame_rate_d: i32,
            _data: &[u8],
        ) {
            self.calls.lock().unwrap().push(format!(
                "send_video({handle},{four_cc:?},{width}x{height},stride={stride},{frame_rate_n}/{frame_rate_d})"
            ));
        }

        fn send_video_async(
            &self,
            handle: usize,
            four_cc: FourCCVideoType,
            width: i32,
            height: i32,
            stride: i32,
            frame_rate_n: i32,
            frame_rate_d: i32,
            _data: &[u8],
        ) {
            self.calls.lock().unwrap().push(format!(
                "send_video_async({handle},{four_cc:?},{width}x{height},stride={stride},{frame_rate_n}/{frame_rate_d})"
            ));
        }

        fn send_video_flush(&self, handle: usize) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("send_video_flush({handle})"));
        }

        fn send_audio(
            &self,
            handle: usize,
            sample_rate: i32,
            channels: i32,
            samples_per_channel: i32,
            interleaved: &[f32],
        ) {
            self.calls.lock().unwrap().push(format!(
                "send_audio({handle},sr={sample_rate},ch={channels},spc={samples_per_channel})"
            ));
            // Record the planar form for tests that want to verify layout.
            let mut scratch = Vec::new();
            crate::deinterleave::deinterleave(interleaved, channels as usize, &mut scratch);
            *self.last_audio_planar.lock().unwrap() = scratch;
        }

        fn send_get_tally(&self, handle: usize, timeout_ms: u32) -> Option<(bool, bool)> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("send_get_tally({handle},{timeout_ms})"));
            *self.tally_response.lock().unwrap()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use test_util::MockNdiBackend;

    #[test]
    fn new_uses_no_clocking_by_default() {
        let backend = Arc::new(MockNdiBackend::new());
        let _s = NdiSender::new(backend.clone(), "X").unwrap();
        let calls = backend.calls();
        assert_eq!(calls[0], "send_create_with_clocking(X,false,false)");
    }

    #[test]
    fn new_with_clocking_forwards_flags() {
        let backend = Arc::new(MockNdiBackend::new());
        let _s = NdiSender::new_with_clocking(backend.clone(), "Y", true, false).unwrap();
        let calls = backend.calls();
        assert_eq!(calls[0], "send_create_with_clocking(Y,true,false)");
    }

    #[test]
    fn send_video_async_records_nv12_fourcc_and_size() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new(backend.clone(), "V").unwrap();

        let frame = VideoFrame {
            data: vec![0u8; 1920 * 1080 * 3 / 2],
            width: 1920,
            height: 1080,
            stride: 1920,
            frame_rate_n: 30,
            frame_rate_d: 1,
            pixel_format: PixelFormat::Nv12,
        };
        sender.send_video_async(&frame);
        let calls = backend.calls();
        assert_eq!(
            calls[1],
            "send_video_async(42,NV12,1920x1080,stride=1920,30/1)"
        );
    }

    #[test]
    fn send_video_records_bgra_fourcc() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new(backend.clone(), "B").unwrap();
        let frame = VideoFrame {
            data: vec![0u8; 4],
            width: 1,
            height: 1,
            stride: 4,
            frame_rate_n: 30,
            frame_rate_d: 1,
            pixel_format: PixelFormat::Bgra,
        };
        sender.send_video(&frame);
        let calls = backend.calls();
        assert_eq!(calls[1], "send_video(42,BGRA,1x1,stride=4,30/1)");
    }

    #[test]
    fn send_audio_records_samples_and_planarises() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new(backend.clone(), "A").unwrap();

        // 2ch interleaved, 4 samples/ch → 8 floats
        let frame = AudioFrame {
            data: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0],
            channels: 2,
            sample_rate: 48000,
        };
        sender.send_audio(&frame);

        let calls = backend.calls();
        assert_eq!(calls[1], "send_audio(42,sr=48000,ch=2,spc=4)");

        // Planar: [1,3,5,7, 2,4,6,8]
        assert_eq!(
            backend.last_audio_planar(),
            vec![1.0, 3.0, 5.0, 7.0, 2.0, 4.0, 6.0, 8.0]
        );
    }

    #[test]
    fn send_video_flush_is_recorded() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new(backend.clone(), "F").unwrap();
        sender.send_video_flush();
        let calls = backend.calls();
        assert_eq!(calls.last().unwrap(), "send_video_flush(42)");
    }

    #[test]
    fn sender_drop_flushes_then_destroys() {
        let backend = Arc::new(MockNdiBackend::new());
        {
            let _s = NdiSender::new(backend.clone(), "D").unwrap();
        }
        let calls = backend.calls();
        // create, then on drop: flush, then destroy
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], "send_create_with_clocking(D,false,false)");
        assert_eq!(calls[1], "send_video_flush(42)");
        assert_eq!(calls[2], "send_destroy(42)");
    }

    #[test]
    fn send_audio_zero_channels_is_noop() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new(backend.clone(), "Z").unwrap();
        let frame = AudioFrame {
            data: vec![1.0, 2.0],
            channels: 0,
            sample_rate: 48000,
        };
        sender.send_audio(&frame);
        // Only create + drop-flush + destroy — no send_audio recorded.
        let calls = backend.calls();
        assert!(calls.iter().all(|c| !c.starts_with("send_audio")));
    }

    #[test]
    fn get_tally_returns_recorded_value() {
        let backend = Arc::new(MockNdiBackend::new());
        backend.set_tally(true, false);
        let sender = NdiSender::new(backend.clone(), "T").unwrap();
        let tally = sender.get_tally(100).unwrap();
        assert!(tally.on_program);
        assert!(!tally.on_preview);
    }

    #[test]
    fn get_tally_returns_none_by_default() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new(backend.clone(), "T").unwrap();
        assert!(sender.get_tally(0).is_none());
    }
}
```

- [ ] **Step 3: Update `crates/sp-ndi/src/lib.rs` re-exports**

Replace the existing `pub use sender::{...}` line with:

```rust
pub use sender::{AudioFrame, NdiBackend, NdiSender, RealNdiBackend, Tally, VideoFrame};
#[cfg(any(test, feature = "test-util"))]
pub use sender::test_util;
```

Delete the old `fn sender_new_calls_backend_create`, `sender_send_video_forwards_dimensions`, `sender_send_audio_computes_samples_per_channel`, `sender_drop_calls_destroy`, and `sender_get_tally_*` tests inside `crates/sp-ndi/src/sender.rs` only if they remain after the rewrite (the full file replacement in Step 2 already drops them in favour of the new set — this note is a reminder to verify).

- [ ] **Step 4: Format and run the tests**

```bash
cargo fmt -p sp-ndi
cargo test -p sp-ndi
```
Expected: all tests pass. New tests in `sender::tests` cover clocking, async send, NV12 fourcc, audio planarisation, flush, and the drop order.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-ndi/Cargo.toml crates/sp-ndi/src/sender.rs crates/sp-ndi/src/lib.rs
git commit -m "feat(sp-ndi): add clocking, async send, FLTP audio, NV12 video to backend"
```

---

## Task 6: sp-decoder types — PixelFormat, VideoStreamInfo, DecodedVideoFrame.pixel_format

**Files:**
- Modify: `crates/sp-decoder/src/types.rs`
- Modify: `crates/sp-decoder/Cargo.toml` (add mutants dev-dep)

- [ ] **Step 1: Update `crates/sp-decoder/Cargo.toml`** — append to `[dev-dependencies]` (create the section if missing):

```toml
[dev-dependencies]
mutants = "0.0.3"
```

- [ ] **Step 2: Replace `crates/sp-decoder/src/types.rs` with:**

```rust
//! Public frame types used by other crates.
//!
//! These types are **not** behind `cfg(windows)` so that crates consuming
//! decoded frames can compile on any platform.

/// Pixel format produced by the decoder.
///
/// Today only NV12 is produced because Windows Media Foundation's hardware
/// path negotiates NV12 natively and NDI accepts NV12 FourCC directly — no
/// intermediate BGRA conversion is performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// NV12 semi-planar: Y plane (height rows of `stride` bytes),
    /// immediately followed by interleaved UV plane (height/2 rows).
    Nv12,
}

/// Metadata describing the video stream of an opened media file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoStreamInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel format.
    pub pixel_format: PixelFormat,
    /// Frame rate numerator.
    pub frame_rate_num: u32,
    /// Frame rate denominator.
    pub frame_rate_den: u32,
}

/// A single decoded video frame.
#[derive(Debug, Clone)]
pub struct DecodedVideoFrame {
    /// Raw pixel data in the layout required by `pixel_format`.
    pub data: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Row stride in bytes for the primary plane (Y plane for NV12).
    pub stride: u32,
    /// Presentation timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Pixel format of `data`.
    pub pixel_format: PixelFormat,
}

/// A chunk of decoded audio as interleaved f32 PCM samples.
#[derive(Debug, Clone)]
pub struct DecodedAudioFrame {
    /// Interleaved f32 PCM sample data.
    pub data: Vec<f32>,
    /// Number of audio channels.
    pub channels: u32,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Presentation timestamp in milliseconds.
    pub timestamp_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_format_nv12_is_unique() {
        assert_eq!(PixelFormat::Nv12, PixelFormat::Nv12);
    }

    #[test]
    fn video_stream_info_round_trip() {
        let info = VideoStreamInfo {
            width: 1920,
            height: 1080,
            pixel_format: PixelFormat::Nv12,
            frame_rate_num: 30000,
            frame_rate_den: 1001,
        };
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        assert_eq!(info.frame_rate_num, 30000);
        assert_eq!(info.frame_rate_den, 1001);
        assert_eq!(info.pixel_format, PixelFormat::Nv12);
    }

    #[test]
    fn decoded_video_frame_defaults_to_nv12() {
        let f = DecodedVideoFrame {
            data: vec![0u8; 6],
            width: 2,
            height: 2,
            stride: 2,
            timestamp_ms: 0,
            pixel_format: PixelFormat::Nv12,
        };
        assert_eq!(f.pixel_format, PixelFormat::Nv12);
        // NV12 size: w * h * 3 / 2 = 2 * 2 * 3 / 2 = 6
        assert_eq!(f.data.len(), 6);
    }
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p sp-decoder --lib types
```
Expected: all tests pass. The compile may fail in `reader.rs` because `DecodedVideoFrame` gained a required field — this will be fixed in the next task. To limit to the types module only:

```bash
cargo test -p sp-decoder --lib --no-run 2>&1 | tail -20
```
If `reader.rs` fails with missing `pixel_format` field, proceed to Task 7 without committing.

- [ ] **Step 4: Quick-fix the field in `reader.rs` so this commit compiles**

In `crates/sp-decoder/src/reader.rs`, find the `Ok(Some(DecodedVideoFrame { ... }))` return and add `pixel_format: crate::types::PixelFormat::Nv12,` as the last field. (This matches what Task 7 does more thoroughly; the minimum here is to keep the crate compiling.)

- [ ] **Step 5: Run all sp-decoder tests**

```bash
cargo test -p sp-decoder
```
Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/sp-decoder/Cargo.toml crates/sp-decoder/src/types.rs crates/sp-decoder/src/reader.rs
git commit -m "feat(sp-decoder): add PixelFormat, VideoStreamInfo, and pixel_format field"
```

---

## Task 7: sp-decoder reader — parse MF_MT_FRAME_RATE and expose video_info()

**Files:**
- Modify: `crates/sp-decoder/src/reader.rs`

- [ ] **Step 1: Extend `MediaReader` to store frame rate**

In `crates/sp-decoder/src/reader.rs`:

1. Add to the `use windows::Win32::Media::MediaFoundation::{...}` import block:
   ```rust
   MF_MT_FRAME_RATE,
   ```
   (Verify the name via `rustc --explain` or the `windows` crate docs; it is in the same module as `MF_MT_FRAME_SIZE`.)

2. Add new fields to `MediaReader`:
   ```rust
   pub struct MediaReader {
       reader: IMFSourceReader,
       duration_ms: u64,
       video_width: u32,
       video_height: u32,
       frame_rate_num: u32,
       frame_rate_den: u32,
   }
   ```

3. After `Self::make_video_output_type` and `SetCurrentMediaType` succeed in `open()`, but BEFORE returning `Ok(Self { … })`, read the negotiated media type and pull the frame rate + frame size:
   ```rust
   // Read back the negotiated video type so we know exactly what we're getting.
   let negotiated_video: IMFMediaType = unsafe {
       reader
           .GetCurrentMediaType(VIDEO_STREAM)
           .map_err(|e| DecoderError::ReadSample(format!("GetCurrentMediaType video: {e}")))?
   };
   let (video_width, video_height) = unsafe {
       let size = negotiated_video.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
       ((size >> 32) as u32, size as u32)
   };
   let (frame_rate_num, frame_rate_den) = unsafe {
       match negotiated_video.GetUINT64(&MF_MT_FRAME_RATE) {
           Ok(packed) => ((packed >> 32) as u32, packed as u32),
           Err(_) => {
               tracing::warn!("MF_MT_FRAME_RATE unavailable; falling back to 30000/1001");
               (30000, 1001)
           }
       }
   };
   debug!(
       video_width,
       video_height,
       frame_rate_num,
       frame_rate_den,
       "negotiated video media type"
   );
   ```

4. Change the return to:
   ```rust
   Ok(Self {
       reader,
       duration_ms: 0,
       video_width,
       video_height,
       frame_rate_num,
       frame_rate_den,
   })
   ```

5. Add the `video_info` method after `duration_ms()`:
   ```rust
   /// Video stream metadata from the negotiated media type.
   pub fn video_info(&self) -> crate::types::VideoStreamInfo {
       crate::types::VideoStreamInfo {
           width: self.video_width,
           height: self.video_height,
           pixel_format: crate::types::PixelFormat::Nv12,
           frame_rate_num: self.frame_rate_num,
           frame_rate_den: self.frame_rate_den,
       }
   }
   ```

- [ ] **Step 2: Build (Windows-only code path, so just `cargo check` on Linux)**

```bash
cargo check -p sp-decoder
```
Expected: compiles on Linux (reader.rs is `cfg(windows)`, but types and sync must still build). On a Windows CI run the full compile will be exercised; this is acceptable because the integration tests for reader live on the Windows runner.

- [ ] **Step 3: Commit**

```bash
git add crates/sp-decoder/src/reader.rs
git commit -m "feat(sp-decoder): parse MF_MT_FRAME_RATE and expose video_info()"
```

---

## Task 8: sp-decoder reader — stop converting NV12→BGRA, return raw NV12 bytes

**Files:**
- Modify: `crates/sp-decoder/src/reader.rs`

- [ ] **Step 1: Delete the `nv12_to_bgra` function and its call site**

In `crates/sp-decoder/src/reader.rs`:

1. Delete the entire `fn nv12_to_bgra(nv12: &[u8], width: u32, height: u32) -> Vec<u8>` at the bottom of the file.

2. Inside `next_video_frame`, find:
   ```rust
   // Convert NV12 → BGRA for NDI output.
   let bgra = nv12_to_bgra(&nv12_data, width, height);
   let stride = width * 4;

   Ok(Some(DecodedVideoFrame {
       data: bgra,
       width,
       height,
       stride,
       timestamp_ms,
       pixel_format: crate::types::PixelFormat::Nv12,
   }))
   ```

3. Replace with:
   ```rust
   // NV12 passthrough: the raw buffer is Y plane (height × width bytes) plus
   // interleaved UV plane (height/2 × width bytes). NDI accepts NV12 natively
   // via NDIlib_FourCC_video_type_NV12. Stride = Y-plane row bytes = width.
   let stride = width;

   Ok(Some(DecodedVideoFrame {
       data: nv12_data,
       width,
       height,
       stride,
       timestamp_ms,
       pixel_format: crate::types::PixelFormat::Nv12,
   }))
   ```

- [ ] **Step 2: Run the workspace check and sp-decoder tests**

```bash
cargo check
cargo test -p sp-decoder
```
Expected: clean compile and the existing tests (which are minimal on Linux) still pass. The real reader tests run on Windows CI.

- [ ] **Step 3: Commit**

```bash
git add crates/sp-decoder/src/reader.rs
git commit -m "perf(sp-decoder): pass NV12 through to NDI, drop CPU BGRA conversion"
```

---

## Task 9: sp-decoder sync — forward video_info()

**Files:**
- Modify: `crates/sp-decoder/src/sync.rs`

- [ ] **Step 1: Add `video_info` to `SyncedDecoder`**

In `crates/sp-decoder/src/sync.rs`, add below the `duration_ms` method:

```rust
/// Video stream metadata forwarded from the underlying reader.
pub fn video_info(&self) -> crate::types::VideoStreamInfo {
    self.reader.video_info()
}
```

- [ ] **Step 2: Run sp-decoder tests**

```bash
cargo check
cargo test -p sp-decoder
```
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/sp-decoder/src/sync.rs
git commit -m "feat(sp-decoder): expose video_info() on SyncedDecoder"
```

---

## Task 10: sp-server FrameSubmitter helper (cross-platform, tested with mock backend)

**Files:**
- Create: `crates/sp-server/src/playback/submitter.rs`
- Modify: `crates/sp-server/src/playback/mod.rs` (add `mod submitter;`)
- Modify: `crates/sp-server/Cargo.toml` (enable sp-ndi `test-util` dev-feature)

- [ ] **Step 1: Update `crates/sp-server/Cargo.toml`**

Find the `[dev-dependencies]` section and add, after the existing dev deps:

```toml
sp-ndi = { path = "../sp-ndi", features = ["test-util"] }
```

This makes `sp_ndi::test_util::MockNdiBackend` available in `cargo test` builds.

- [ ] **Step 2: Create `crates/sp-server/src/playback/submitter.rs`**

```rust
//! Frame submission helper for the playback pipeline.
//!
//! Owns an `NdiSender` and enforces the rules required for correct NDI output:
//!
//! 1. For each synced tuple, audio chunks are submitted BEFORE the video frame.
//!    This keeps audio buffered in NDI's internal queue when `clock_video=true`
//!    blocks the calling thread for frame pacing.
//!
//! 2. The previous video frame's `Vec<u8>` buffer is kept alive until the next
//!    `submit` or `flush` call. `NDIlib_send_send_video_async_v2` retains a
//!    pointer to our bytes and only releases it when the next async/sync/flush
//!    call arrives.
//!
//! 3. `flush` is called on every playback exit path (Ended / Stopped /
//!    Shutdown / NewPlay / Error / Pause). Flush itself is a sync point that
//!    releases the previous frame, after which the buffer may be dropped.

use std::sync::Arc;

use sp_ndi::{AudioFrame, NdiBackend, NdiSender, PixelFormat, VideoFrame};

/// Owns an `NdiSender` plus the previous frame's buffer for the async
/// double-buffer pattern.
pub struct FrameSubmitter<B: NdiBackend> {
    sender: NdiSender<B>,
    /// Keeps the previous async frame's `Vec<u8>` alive until NDI releases
    /// its pointer (which happens when the next submit / flush call fires).
    prev_frame: Option<Vec<u8>>,
    frame_rate_n: i32,
    frame_rate_d: i32,
}

impl<B: NdiBackend> FrameSubmitter<B> {
    /// Create a submitter owning an already-constructed sender.
    pub fn new(sender: NdiSender<B>, frame_rate_n: i32, frame_rate_d: i32) -> Self {
        Self {
            sender,
            prev_frame: None,
            frame_rate_n,
            frame_rate_d,
        }
    }

    /// Submit one decoded frame tuple: all audio chunks first, then video
    /// asynchronously. Video buffer ownership transfers to the submitter for
    /// the double-buffer holdover.
    pub fn submit_nv12(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        video_data: Vec<u8>,
        audio: &[AudioFrame],
    ) {
        // 1. Audio first — fast, non-blocking, goes straight into NDI's queue.
        for af in audio {
            self.sender.send_audio(af);
        }

        // 2. Video async — may block on clock_video pacing, returns once NDI
        //    has taken ownership of our pointer.
        let frame = VideoFrame {
            data: video_data,
            width,
            height,
            stride,
            frame_rate_n: self.frame_rate_n,
            frame_rate_d: self.frame_rate_d,
            pixel_format: PixelFormat::Nv12,
        };
        self.sender.send_video_async(&frame);

        // The async call above is itself the sync point that releases the
        // PREVIOUS prev_frame. Safe to drop it now and install the new one.
        self.prev_frame = Some(frame.data);
    }

    /// Release any pending async frame. Call this on every playback exit path
    /// before allowing the previous frame's Vec to drop.
    pub fn flush(&mut self) {
        self.sender.send_video_flush();
        self.prev_frame = None;
    }

    /// Send a solid-colour BGRA frame synchronously — used for idle /
    /// paused states. Internally flushes any pending async frame first.
    pub fn send_black_bgra(&mut self, width: u32, height: u32) {
        self.flush();
        let data = vec![0u8; (width * height * 4) as usize];
        let frame = VideoFrame {
            data,
            width,
            height,
            stride: width * 4,
            frame_rate_n: self.frame_rate_n,
            frame_rate_d: self.frame_rate_d,
            pixel_format: PixelFormat::Bgra,
        };
        self.sender.send_video(&frame);
    }

    /// Borrow the underlying sender (mainly for tests).
    pub fn sender(&self) -> &NdiSender<B> {
        &self.sender
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_ndi::test_util::MockNdiBackend;

    fn mk_audio(interleaved: Vec<f32>, channels: u32) -> AudioFrame {
        AudioFrame {
            data: interleaved,
            channels,
            sample_rate: 48000,
        }
    }

    #[test]
    fn submit_sends_audio_before_video_async() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "S", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        let audio = vec![mk_audio(vec![0.1, 0.2, 0.3, 0.4], 2)];
        sub.submit_nv12(4, 2, 4, vec![0u8; 4 * 2 * 3 / 2], &audio);

        let calls = backend.calls();
        // Expect: create (with clocking), send_audio, send_video_async
        assert_eq!(
            calls[0],
            "send_create_with_clocking(S,true,false)"
        );
        assert_eq!(calls[1], "send_audio(42,sr=48000,ch=2,spc=2)");
        assert_eq!(
            calls[2],
            "send_video_async(42,NV12,4x2,stride=4,30/1)"
        );
    }

    #[test]
    fn submit_handles_multiple_audio_chunks_in_order() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "M", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        let audio = vec![
            mk_audio(vec![1.0, 2.0], 2),
            mk_audio(vec![3.0, 4.0, 5.0, 6.0], 2),
        ];
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &audio);

        let calls = backend.calls();
        // create, audio chunk 1, audio chunk 2, video
        assert!(calls[1].starts_with("send_audio(42,sr=48000,ch=2,spc=1)"));
        assert!(calls[2].starts_with("send_audio(42,sr=48000,ch=2,spc=2)"));
        assert!(calls[3].starts_with("send_video_async"));
    }

    #[test]
    fn flush_is_recorded_and_clears_prev_frame() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "F", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        sub.flush();

        let calls = backend.calls();
        assert!(calls.iter().any(|c| c == "send_video_flush(42)"));
        assert!(sub.prev_frame.is_none());
    }

    #[test]
    fn send_black_bgra_flushes_first() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "K", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        sub.send_black_bgra(1920, 1080);

        let calls = backend.calls();
        // Must see: create, send_video_async (NV12), send_video_flush, send_video (BGRA)
        let idx_async = calls
            .iter()
            .position(|c| c.starts_with("send_video_async"))
            .unwrap();
        let idx_flush = calls
            .iter()
            .position(|c| c == "send_video_flush(42)")
            .unwrap();
        let idx_black = calls
            .iter()
            .position(|c| c.starts_with("send_video(42,BGRA,1920x1080"))
            .unwrap();
        assert!(idx_async < idx_flush);
        assert!(idx_flush < idx_black);
    }

    #[test]
    fn drop_flushes_before_destroy_via_sender() {
        let backend = Arc::new(MockNdiBackend::new());
        {
            let sender = NdiSender::new_with_clocking(backend.clone(), "D", true, false).unwrap();
            let mut sub = FrameSubmitter::new(sender, 30, 1);
            sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
            // sub drops here → sender drops → flush + destroy
        }
        let calls = backend.calls();
        // The last two calls must be flush then destroy (flush on drop + destroy).
        // Allow one flush we called via `submit_nv12` ordering (none here).
        // But prev_frame means no extra flush was called before drop.
        let last_two = &calls[calls.len() - 2..];
        assert_eq!(last_two[0], "send_video_flush(42)");
        assert_eq!(last_two[1], "send_destroy(42)");
    }

    #[test]
    fn frame_rate_is_forwarded_to_video_frame() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "R", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 60000, 1001);
        sub.submit_nv12(1920, 1080, 1920, vec![0u8; 1920 * 1080 * 3 / 2], &[]);
        let calls = backend.calls();
        assert!(
            calls.iter().any(|c| c.contains("60000/1001")),
            "expected 60000/1001 in one of the calls: {calls:#?}"
        );
    }
}
```

- [ ] **Step 3: Declare the module**

Edit `crates/sp-server/src/playback/mod.rs`. After the existing `pub mod pipeline;` and `pub mod state;` declarations, add:

```rust
pub mod submitter;
```

- [ ] **Step 4: Run sp-server tests**

```bash
cargo test -p sp-server --lib playback::submitter
```
Expected: all 6 submitter tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/Cargo.toml crates/sp-server/src/playback/submitter.rs crates/sp-server/src/playback/mod.rs
git commit -m "feat(sp-server): add FrameSubmitter helper with audio-first ordering"
```

---

## Task 11: sp-server pipeline.rs — use FrameSubmitter + clock_video + real frame rate + remove manual pacing

**Files:**
- Modify: `crates/sp-server/src/playback/pipeline.rs`

- [ ] **Step 1: Add `use` and switch to the new helper**

At the top of `crates/sp-server/src/playback/pipeline.rs`, below the existing imports, add:

```rust
use crate::playback::submitter::FrameSubmitter;
```

- [ ] **Step 2: Rewrite the Windows `run_loop_windows` function**

Replace the body of `run_loop_windows` from the `let backend = match ndi_backend { … }` line through the end of the function with this version. The key differences from the current code are:

- `NdiSender::new_with_clocking(backend, ndi_name, true, false)` instead of `NdiSender::new`
- A `FrameSubmitter` is built when a video starts playing (not at loop init) because the frame rate comes from the decoder and differs per file
- Manual `playback_start` / `pause_offset` / `thread::sleep` pacing is deleted — NDI now paces `send_video_async`
- Audio is submitted via `FrameSubmitter::submit_nv12` together with video in one call
- `submitter.flush()` is called on every exit branch before sending a black frame

Paste the block below verbatim into the position that the old Windows code occupied. Read both the old and new carefully before pasting; adjust imports at the top of the file as needed.

```rust
    let backend = match ndi_backend {
        Some(b) => b,
        None => {
            error!("no NDI backend provided");
            let _ = event_tx.send((
                playlist_id,
                PipelineEvent::Error("NDI SDK not available".into()),
            ));
            wait_for_shutdown(&cmd_rx, playlist_id);
            return;
        }
    };

    // clock_video = true lets NDI pace `send_video_async` on its internal
    // high-resolution clock. clock_audio stays false because we submit both
    // streams from a single thread; clocking both would deadlock on startup.
    let sender = match sp_ndi::NdiSender::new_with_clocking(backend, ndi_name, true, false) {
        Ok(s) => s,
        Err(e) => {
            error!(%e, "failed to create NDI sender");
            let _ = event_tx.send((
                playlist_id,
                PipelineEvent::Error(format!("Failed to create NDI sender: {e}")),
            ));
            wait_for_shutdown(&cmd_rx, playlist_id);
            return;
        }
    };

    info!(ndi_name, "NDI sender created with clock_video=true");

    // Initial black frame so the NDI source is visible immediately with a
    // sane default frame rate. The FrameSubmitter is rebuilt when real
    // playback starts with the actual frame rate.
    let mut idle_submitter = FrameSubmitter::new(sender, 30, 1);
    idle_submitter.send_black_bgra(1920, 1080);

    let mut paused = false;

    loop {
        match cmd_rx.recv() {
            Ok(PipelineCommand::Shutdown) | Err(_) => {
                info!(playlist_id, "pipeline thread shutting down");
                idle_submitter.flush();
                break;
            }

            Ok(PipelineCommand::Play(path)) => {
                let mut current_path = path;
                let shutdown_requested = loop {
                    info!(?current_path, playlist_id, "starting playback");
                    paused = false;

                    match decode_and_send(
                        &cmd_rx,
                        &mut idle_submitter,
                        &current_path,
                        &event_tx,
                        playlist_id,
                        &mut paused,
                    ) {
                        DecodeResult::Ended => {
                            info!(playlist_id, "video ended naturally");
                            idle_submitter.send_black_bgra(1920, 1080);
                            let _ = event_tx.send((playlist_id, PipelineEvent::Ended));
                            break false;
                        }
                        DecodeResult::Stopped => {
                            info!(playlist_id, "playback stopped");
                            idle_submitter.send_black_bgra(1920, 1080);
                            break false;
                        }
                        DecodeResult::Shutdown => {
                            info!(playlist_id, "shutdown during playback");
                            idle_submitter.flush();
                            break true;
                        }
                        DecodeResult::NewPlay(new_path) => {
                            info!(?new_path, playlist_id, "switching to new video");
                            current_path = new_path;
                            continue;
                        }
                        DecodeResult::Error(msg) => {
                            error!(playlist_id, %msg, "decode error");
                            idle_submitter.send_black_bgra(1920, 1080);
                            let _ = event_tx.send((playlist_id, PipelineEvent::Error(msg)));
                            break false;
                        }
                    }
                };

                if shutdown_requested {
                    break;
                }
            }

            Ok(PipelineCommand::Pause) => {
                paused = true;
                debug!(playlist_id, "paused (no active playback)");
            }
            Ok(PipelineCommand::Resume) => {
                paused = false;
                debug!(playlist_id, "resumed (no active playback)");
            }
            Ok(PipelineCommand::Stop) => {
                idle_submitter.send_black_bgra(1920, 1080);
                debug!(playlist_id, "stopped (no active playback)");
            }
        }
    }
}
```

- [ ] **Step 3: Rewrite `decode_and_send`**

Replace the entire `fn decode_and_send` with:

```rust
#[cfg(windows)]
fn decode_and_send(
    cmd_rx: &Receiver<PipelineCommand>,
    submitter: &mut FrameSubmitter<sp_ndi::RealNdiBackend>,
    path: &std::path::Path,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    paused: &mut bool,
) -> DecodeResult {
    use sp_decoder::SyncedDecoder;

    let mut decoder = match SyncedDecoder::open(path) {
        Ok(d) => d,
        Err(e) => {
            return DecodeResult::Error(format!("Failed to open {}: {e}", path.display()));
        }
    };

    // Apply the file's real frame rate to the submitter so NDI paces correctly.
    let info = decoder.video_info();
    submitter.set_frame_rate(info.frame_rate_num as i32, info.frame_rate_den as i32);

    // Report start.
    let _ = event_tx.send((
        playlist_id,
        PipelineEvent::Started {
            duration_ms: decoder.duration_ms(),
        },
    ));

    let mut last_position_report = Instant::now();
    let mut frame_count: u64 = 0;

    loop {
        // Check for commands between frames (non-blocking).
        match cmd_rx.try_recv() {
            Ok(PipelineCommand::Shutdown) => {
                submitter.flush();
                return DecodeResult::Shutdown;
            }
            Ok(PipelineCommand::Stop) => {
                submitter.flush();
                return DecodeResult::Stopped;
            }
            Ok(PipelineCommand::Play(new_path)) => {
                submitter.flush();
                return DecodeResult::NewPlay(new_path);
            }
            Ok(PipelineCommand::Pause) => {
                *paused = true;
                debug!(playlist_id, "paused during playback");
            }
            Ok(PipelineCommand::Resume) => {
                *paused = false;
                debug!(playlist_id, "resumed playback");
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                submitter.flush();
                return DecodeResult::Shutdown;
            }
        }

        // If paused, send BGRA black at ~10 fps via the sync path.
        if *paused {
            submitter.send_black_bgra(1920, 1080);
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }

        // Decode the next synced frame.
        match decoder.next_synced() {
            Ok(Some((video_frame, audio_frames))) => {
                // Convert audio chunks to sp_ndi::AudioFrame.
                let ndi_audio: Vec<sp_ndi::AudioFrame> = audio_frames
                    .into_iter()
                    .map(|af| sp_ndi::AudioFrame {
                        data: af.data,
                        channels: af.channels,
                        sample_rate: af.sample_rate,
                    })
                    .collect();

                let timestamp_ms = video_frame.timestamp_ms;
                submitter.submit_nv12(
                    video_frame.width,
                    video_frame.height,
                    video_frame.stride,
                    video_frame.data,
                    &ndi_audio,
                );

                frame_count += 1;

                // Report position every 500 ms.
                if last_position_report.elapsed() >= std::time::Duration::from_millis(500) {
                    let _ = event_tx.send((
                        playlist_id,
                        PipelineEvent::Position {
                            position_ms: timestamp_ms,
                            duration_ms: decoder.duration_ms(),
                        },
                    ));
                    last_position_report = Instant::now();
                }
            }
            Ok(None) => {
                info!(playlist_id, frame_count, "video decode complete");
                submitter.flush();
                return DecodeResult::Ended;
            }
            Err(e) => {
                submitter.flush();
                return DecodeResult::Error(format!(
                    "Decode error at frame {frame_count}: {e}"
                ));
            }
        }
    }
}
```

- [ ] **Step 4: Add `set_frame_rate` to `FrameSubmitter`**

In `crates/sp-server/src/playback/submitter.rs`, add a method on `FrameSubmitter`:

```rust
/// Update the frame rate used for subsequent submissions. Call this when
/// a new file is opened and its real frame rate is known.
pub fn set_frame_rate(&mut self, num: i32, den: i32) {
    self.frame_rate_n = num;
    self.frame_rate_d = den;
}
```

And add a unit test right below the existing ones in the `tests` module:

```rust
#[test]
fn set_frame_rate_updates_subsequent_frames() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "U", true, false).unwrap();
    let mut sub = FrameSubmitter::new(sender, 30, 1);
    sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
    sub.set_frame_rate(60, 1);
    sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
    let calls = backend.calls();
    let async_calls: Vec<_> = calls
        .iter()
        .filter(|c| c.starts_with("send_video_async"))
        .collect();
    assert_eq!(async_calls.len(), 2);
    assert!(async_calls[0].contains("30/1"));
    assert!(async_calls[1].contains("60/1"));
}
```

- [ ] **Step 5: Delete the old `send_black_frame` free function in `pipeline.rs`**

In `crates/sp-server/src/playback/pipeline.rs`, find:

```rust
/// Send a black BGRA frame to keep the NDI source visible.
#[cfg(windows)]
fn send_black_frame(sender: &sp_ndi::NdiSender<sp_ndi::RealNdiBackend>, width: u32, height: u32) {
    ...
}
```

Delete the entire function — the `FrameSubmitter::send_black_bgra` replaces it.

- [ ] **Step 6: Format and run everything**

```bash
cargo fmt --all
cargo check
cargo test -p sp-server --lib playback
```
Expected: every sp-server playback test passes, including the new `set_frame_rate_updates_subsequent_frames` test. On Linux the Windows-only decode path isn't exercised but the code compiles.

- [ ] **Step 7: Commit**

```bash
git add crates/sp-server/src/playback/pipeline.rs crates/sp-server/src/playback/submitter.rs
git commit -m "feat(sp-server): clock NDI video, use FrameSubmitter, drop manual pacing"
```

---

## Task 12: CI — re-enable mutation testing on touched files

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Remove the exclusions for sp-ndi, sp-decoder, and pipeline.rs**

In `.github/workflows/ci.yml`, find the `cargo mutants --in-diff pr.diff` invocation around line 372. Delete these three `--exclude-re` lines:

```
            --exclude-re 'sp-decoder/' \
            --exclude-re 'sp-ndi/' \
            --exclude-re 'sp-server/src/playback/pipeline\.rs' \
```

Leave the remaining exclusions (`sp-server/src/lib.rs`, `sp-server/src/obs/`, `sp-server/src/downloader/`, `sp-server/src/reprocess/`, `sp-server/src/api/websocket`, `sp-server/src/playlist/`) in place.

Also update the comment block directly above the command from:

```yaml
          # - sp-server/src/playlist: shells out to yt-dlp
          # NOTE: sp-server/src/resolume/ is NOT excluded — handlers/driver have wiremock tests.
          # NOTE: sp-server/src/playback/mod.rs is NOT excluded — title timer logic is unit-tested.
```

to:

```yaml
          # - sp-server/src/playlist: shells out to yt-dlp
          # NOTE: sp-server/src/resolume/ is NOT excluded — handlers/driver have wiremock tests.
          # NOTE: sp-server/src/playback/mod.rs is NOT excluded — title timer logic is unit-tested.
          # NOTE: sp-ndi/, sp-decoder/, and sp-server/src/playback/pipeline.rs
          # are NOT excluded — covered by unit tests via MockNdiBackend / FrameSubmitter.
```

- [ ] **Step 2: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: require zero mutation survivors in sp-ndi, sp-decoder, pipeline"
```

---

## Task 13: Push, monitor CI, fix any surviving mutants, verify on win-resolume

**Files:** none (process task)

- [ ] **Step 1: Push all commits**

```bash
git push origin dev
```

- [ ] **Step 2: Monitor the CI run**

```bash
gh run list --branch dev --limit 3
# Pick the latest run id:
gh run view <run-id>
```

Wait for ALL jobs to reach terminal state. Expected terminal state: all green. Non-green outcomes:

- **Format / lint failure:** run `cargo fmt --all` locally, commit, push, monitor again.
- **Unit test failure on Linux or Windows:** `gh run view <run-id> --log-failed`, inspect, fix, commit, push, monitor.
- **Mutation surviving:** fetch the `mutants-out` artifact, identify the surviving mutants, add targeted tests or — only where the mutant truly can't be observed without real-time sleeps — an `#[cfg_attr(test, mutants::skip)]` attribute with a comment explaining why. Commit, push, monitor.
- **E2E Playwright failure:** the dashboard should not be affected by media pipeline changes. If it fails, inspect `playwright-report` artifact and fix.
- **Build-tauri failure:** usually a version bump or artifact issue. Fix and push.

Do NOT merge or proceed until every job is green.

- [ ] **Step 3: Post-deploy verification on `win-resolume`**

After CI runs Deploy-to-win-resolume and E2E-Tests (win-resolume), perform the manual/MCP verification from the spec:

1. Via `mcp__win-resolume__Shell`: run `Get-Process sp-server` and confirm it is running. Expected non-empty output.
2. Open the dashboard in Playwright (`http://10.77.9.201:8920/` — use the dev machine IP, not localhost) via `mcp__plugin_playwright_playwright__browser_navigate`.
3. Click Play on a playlist with a known song — `mcp__plugin_playwright_playwright__browser_click`. Use `browser_snapshot` first to find the selector.
4. Wait 3 seconds, assert the dashboard now-playing row shows a song title and elapsed time > 0 via `browser_snapshot`.
5. Via `mcp__obs-resolume__obs-get-scene-item-list` on the `sp-fast` (or whichever matching) scene: confirm the NDI source is in the list and enabled.
6. Via `mcp__obs-resolume__obs-get-source-active` on the NDI source: assert `videoActive == true`.
7. Via `mcp__obs-resolume__obs-get-source-screenshot` of the NDI source with `imageFormat: "png"`, `imageWidth: 320`: decode the base64 and assert the buffer is > 1 KB and is not all-identical bytes (no static pattern). This proves decoded pixels are flowing.
8. Via `mcp__win-resolume__Snapshot` of the OBS main window: visually inspect the audio mixer strip of the SongPlayer NDI source. The dB meter must be active (not floor / not `-inf`).
9. **Manual listen** on the win-resolume audio monitor (document which clip was played and the subjective sync observation in the completion report).

If any verification step fails, stop, investigate the root cause, fix, and repeat the full CI cycle.

- [ ] **Step 4: Open the PR from dev to main**

Follow the standard PR flow (`gh pr create --base main --head dev …`). Include in the PR body:

- Summary bullet: fixed silent NDI audio via FLTP + deinterleave; enabled clock_video pacing; NV12 passthrough; async double-buffer send; real frame rate from MF.
- Test plan bullets: unit tests (`sp-ndi` new tests, `sp-decoder` new tests, `sp-server playback::submitter` new tests), mutation testing zero survivors on all touched files, post-deploy Playwright + OBS MCP verification on win-resolume, manual listen confirmation with clip name.

Wait for user merge approval before merging.

---

## Verification checklist (end-of-plan)

After all tasks complete:

1. `cargo fmt --all --check` — clean
2. `cargo check` on Linux — clean
3. CI: all jobs green on the latest `dev` push
4. Mutation testing: 0 surviving mutants on all touched files (`sp-ndi/`, `sp-decoder/`, `sp-server/src/playback/pipeline.rs`, `sp-server/src/playback/submitter.rs`)
5. Deploy to `win-resolume`: succeeds, process running
6. Playwright verification of dashboard playback: song name + elapsed time update
7. OBS MCP verification: NDI source active, screenshot non-static, audio mixer meter active
8. Manual listen: audio audible and A/V perceived in sync
9. PR opened from `dev` to `main`, mergeable, clean, all checks green
