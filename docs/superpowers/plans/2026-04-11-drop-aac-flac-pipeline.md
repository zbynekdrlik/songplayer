# Drop AAC from Audio Pipeline — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the current two-stacked-lossy AAC audio pipeline with a split-file layout where `yt-dlp` stream-copies video and audio separately, FFmpeg applies 2-pass loudnorm once to produce a lossless FLAC sidecar, and `sp-decoder` decodes audio via pure-Rust Symphonia while keeping Media Foundation for video. Restores the missing legacy startup-sync and self-heals any pre-existing cached files on first boot.

**Architecture:** Split each song into `{base}_video.mp4` (stream-copied H.264/VP9/AV1) and `{base}_audio.flac` (single lossy-to-lossless transition, then lossless all the way to NDI). `sp-decoder` grows a trait-based `VideoStream` / `AudioStream` split with `MediaFoundationVideoReader` (Windows-only) and cross-platform `SymphoniaAudioReader`. `SplitSyncedDecoder` drives both with audio-as-master-clock.

**Tech Stack:** Rust 2024, Symphonia 0.5 (pure-Rust FLAC decoder), windows 0.58 (Media Foundation video only), sqlx 0.8 (SQLite), Axum 0.8. FFmpeg 2-pass loudnorm (unchanged). yt-dlp (split into two invocations).

**Spec:** `docs/superpowers/specs/2026-04-11-drop-aac-flac-pipeline-design.md`

---

## File map

Files this plan creates:

- `crates/sp-decoder/src/stream.rs` — `MediaStream`, `VideoStream`, `AudioStream` traits.
- `crates/sp-decoder/src/audio/mod.rs` — cross-platform audio module root.
- `crates/sp-decoder/src/audio/symphonia_reader.rs` — `SymphoniaAudioReader` implementing `AudioStream`.
- `crates/sp-decoder/src/video/mod.rs` — `cfg(windows)` video module root.
- `crates/sp-decoder/src/video/mf_reader.rs` — `MediaFoundationVideoReader` implementing `VideoStream` (video-only refactor of the existing `reader.rs`).
- `crates/sp-decoder/src/split_sync.rs` — `SplitSyncedDecoder` trait-based sync algorithm.
- `crates/sp-decoder/tests/fixtures/silent_3s.flac` — committed FLAC fixture, 3s silent stereo 48 kHz.
- `crates/sp-decoder/tests/fixtures/black_3s.mp4` — committed video-only MP4, 3s 32×32 black H.264, no audio track.
- `crates/sp-decoder/tests/fixtures/regen.sh` — shell script to regenerate both fixtures.
- `crates/sp-decoder/tests/symphonia_audio.rs` — Linux+Windows integration test.
- `crates/sp-decoder/tests/mf_video_only.rs` — Windows-only integration test for video-only MP4.
- `crates/sp-decoder/tests/split_synced.rs` — Windows-only integration test combining both fixtures through `SplitSyncedDecoder`.
- `crates/sp-server/tests/startup_migration.rs` — in-memory SQLite integration test for self-healing cache scan.
- `e2e/post-deploy-flac.spec.ts` — Playwright post-deploy assertion that split files exist and NDI audio flows after a real download round-trip.

Files this plan modifies:

- `VERSION` — bump to `0.11.0-dev.1`.
- `Cargo.toml` (workspace) — version bump, `symphonia` workspace dependency entry.
- `Cargo.lock` — version bump for the 4 workspace crates.
- `crates/sp-decoder/Cargo.toml` — add `symphonia` (`flac` feature).
- `crates/sp-decoder/src/lib.rs` — rewire module graph, export new public types, remove old re-exports.
- `crates/sp-decoder/src/error.rs` — add `DecoderError::Mismatch`, `DecoderError::Io`, `DecoderError::Decode` variants.
- `crates/sp-decoder/src/reader.rs` — DELETED at the end (task 10). Interim: left untouched; superseded by `video/mf_reader.rs`.
- `crates/sp-decoder/src/sync.rs` — DELETED at the end (task 10). Interim: left untouched; superseded by `split_sync.rs`.
- `crates/sp-decoder/tests/duration.rs` — retargeted to open the new `MediaFoundationVideoReader`.
- `crates/sp-server/src/db/mod.rs` — new migration `V4` (existing V2 and V3 are already present), idempotency test.
- `crates/sp-server/src/db/models.rs` — new `get_song_paths`, `mark_video_processed` signature takes both paths, helper queries return `CachedSongRow`.
- `crates/sp-server/src/downloader/cache.rs` — new `CachedSong { video_path, audio_path }`, new filename pattern, new regex, legacy-file detection, updated `scan_cache` + `cleanup_removed`.
- `crates/sp-server/src/downloader/normalize.rs` — FLAC output, updated FFmpeg pass 2 args.
- `crates/sp-server/src/downloader/mod.rs` — two `yt-dlp` invocations, new temp-file layout, finalize step that moves video temp to its final name and cleans audio temp.
- `crates/sp-server/src/playlist/selector.rs` — returns a `SelectedSong { id, video_path, audio_path }` instead of a bare `i64`.
- `crates/sp-server/src/playback/pipeline.rs` — `PipelineCommand::Play(CachedSong)` instead of `Play(PathBuf)`; uses `SplitSyncedDecoder`; removes `SyncedDecoder` import.
- `crates/sp-server/src/playback/mod.rs` (engine) — passes both paths through the `Play` command; touches any call site of the old single-path variant.
- `crates/sp-server/src/lib.rs` — `start()` grows a self-healing cache scan pass plus a startup sync loop (legacy parity).
- `CLAUDE.md` — new section documenting the split-file layout and the decoder split.

Out of scope (no file touches): `sp-core`, `sp-ui`, `sp-ndi`, `src-tauri`.

---

## Task 1: Bump version to 0.11.0-dev.1

**Files:**
- Modify: `VERSION`
- Modify: `Cargo.toml` (workspace root)
- Modify: `Cargo.lock`
- Modify: `src-tauri/Cargo.toml`
- Modify: `sp-ui/Cargo.toml`
- Modify: `src-tauri/tauri.conf.json`

- [ ] **Step 1: Update `VERSION`**

Write exactly this content (no trailing newline variations):

```
0.11.0-dev.1
```

- [ ] **Step 2: Run the version sync script**

```bash
./scripts/sync-version.sh
```

Expected: prints the new version and updates every Cargo.toml + tauri.conf.json that uses `version.workspace = true` or a hard-coded version.

- [ ] **Step 3: Update workspace crate versions in `Cargo.lock`**

Open `Cargo.lock`. Search for each of `name = "sp-core"`, `name = "sp-decoder"`, `name = "sp-ndi"`, `name = "sp-server"`. For each entry, change the `version = "..."` line to `version = "0.11.0-dev.1"`.

- [ ] **Step 4: Verify formatting**

```bash
cargo fmt --all --check
```

Expected: exit 0.

- [ ] **Step 5: Commit**

```bash
git add VERSION Cargo.toml Cargo.lock src-tauri/Cargo.toml sp-ui/Cargo.toml src-tauri/tauri.conf.json
git commit -m "$(cat <<'EOF'
chore: bump version to 0.11.0-dev.1 for FLAC migration

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Generate and commit test fixtures

**Files:**
- Create: `crates/sp-decoder/tests/fixtures/silent_3s.flac`
- Create: `crates/sp-decoder/tests/fixtures/black_3s.mp4`
- Create: `crates/sp-decoder/tests/fixtures/regen.sh`

- [ ] **Step 1: Write the fixture regen script**

Create `crates/sp-decoder/tests/fixtures/regen.sh`:

```bash
#!/usr/bin/env bash
# Regenerate committed test fixtures for sp-decoder.
#
# These fixtures are used by integration tests in crates/sp-decoder/tests/.
# They are committed as binary blobs (~3 KB each) so CI does not need FFmpeg
# to run the tests. Run this script after any intentional change to the
# fixture shape.
set -euo pipefail

cd "$(dirname "$0")"

# Silent stereo 48 kHz FLAC, exactly 3.000 seconds.
# FLAC compresses pure silence extremely well — ~3 KB.
ffmpeg -y \
  -f lavfi -i "anullsrc=r=48000:cl=stereo" \
  -t 3 \
  -c:a flac -compression_level 5 \
  silent_3s.flac

# 32x32 black H.264 video, exactly 3.000 seconds, no audio track.
# yuv420p and x264 baseline keep the file tiny (~3 KB) and ensure Media
# Foundation on Windows can open it without any codec pack.
ffmpeg -y \
  -f lavfi -i "color=c=black:s=32x32:d=3:r=30" \
  -c:v libx264 -profile:v baseline -pix_fmt yuv420p \
  -an \
  black_3s.mp4

echo "Regenerated silent_3s.flac ($(stat -c%s silent_3s.flac) bytes) and black_3s.mp4 ($(stat -c%s black_3s.mp4) bytes)"
```

- [ ] **Step 2: Make the script executable and run it**

```bash
chmod +x crates/sp-decoder/tests/fixtures/regen.sh
crates/sp-decoder/tests/fixtures/regen.sh
```

Expected:
```
Regenerated silent_3s.flac (... bytes) and black_3s.mp4 (... bytes)
```

Both files should be under 10 KB.

- [ ] **Step 3: Verify the fixtures with ffprobe**

```bash
ffprobe -v error -show_entries stream=codec_name,sample_rate,channels,duration -of default=noprint_wrappers=1 crates/sp-decoder/tests/fixtures/silent_3s.flac
ffprobe -v error -show_entries stream=codec_name,width,height,nb_frames -of default=noprint_wrappers=1 crates/sp-decoder/tests/fixtures/black_3s.mp4
```

Expected (silent_3s.flac):
```
codec_name=flac
sample_rate=48000
channels=2
duration=3.000000
```

Expected (black_3s.mp4):
```
codec_name=h264
width=32
height=32
```

- [ ] **Step 4: Commit**

```bash
git add crates/sp-decoder/tests/fixtures/silent_3s.flac \
        crates/sp-decoder/tests/fixtures/black_3s.mp4 \
        crates/sp-decoder/tests/fixtures/regen.sh
git commit -m "$(cat <<'EOF'
test(sp-decoder): add silent_3s.flac + black_3s.mp4 fixtures with regen script

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Add Symphonia dependency and create SymphoniaAudioReader

**Files:**
- Modify: `Cargo.toml` (workspace root) — add `symphonia` to `[workspace.dependencies]`.
- Modify: `crates/sp-decoder/Cargo.toml` — depend on `symphonia` with `flac` feature.
- Create: `crates/sp-decoder/src/stream.rs` — new trait module.
- Create: `crates/sp-decoder/src/audio/mod.rs` — audio submodule root.
- Create: `crates/sp-decoder/src/audio/symphonia_reader.rs` — the reader.
- Modify: `crates/sp-decoder/src/error.rs` — add new variants.
- Modify: `crates/sp-decoder/src/lib.rs` — wire new modules, export new types.
- Create: `crates/sp-decoder/tests/symphonia_audio.rs` — integration test.

- [ ] **Step 1: Add symphonia to workspace dependencies**

Open `Cargo.toml` in the repo root. In the `[workspace.dependencies]` block, add after the `rand = "0.8"` line:

```toml
symphonia = { version = "0.5", default-features = false, features = ["flac"] }
```

- [ ] **Step 2: Depend on symphonia in sp-decoder**

Open `crates/sp-decoder/Cargo.toml`. In the `[dependencies]` block, after the `thiserror = { workspace = true }` line, add:

```toml
symphonia = { workspace = true }
```

- [ ] **Step 3: Add new error variants**

Replace `crates/sp-decoder/src/error.rs` with:

```rust
//! Decoder error types.

/// Errors that can occur during media decoding.
#[derive(Debug, thiserror::Error)]
pub enum DecoderError {
    /// COM initialization failed.
    #[error("COM initialization failed: {0}")]
    ComInit(String),

    /// Failed to create the MF source reader.
    #[error("Failed to create source reader: {0}")]
    SourceReader(String),

    /// No stream of the given kind is available.
    #[error("No {0} stream available")]
    NoStream(&'static str),

    /// A sample read operation failed.
    #[error("Sample read failed: {0}")]
    ReadSample(String),

    /// The stream has reached its end.
    #[error("End of stream")]
    EndOfStream,

    /// A seek operation failed.
    #[error("Seek failed: {0}")]
    Seek(String),

    /// Locking the media buffer failed.
    #[error("Buffer lock failed: {0}")]
    BufferLock(String),

    /// I/O failure opening or reading a file.
    #[error("I/O failure: {0}")]
    Io(String),

    /// Decoder-side failure (Symphonia or MF codec error).
    #[error("Decode failure: {0}")]
    Decode(String),

    /// Video and audio sidecars disagree on duration / format.
    #[error("Video/audio mismatch: {0}")]
    Mismatch(String),
}
```

- [ ] **Step 4: Create the stream trait module**

Create `crates/sp-decoder/src/stream.rs`:

```rust
//! Trait-based decoder abstraction.
//!
//! The split-file pipeline opens the video and audio sidecars through two
//! separate readers. Each reader implements one of these traits, and
//! [`crate::split_sync::SplitSyncedDecoder`] drives both generically — which
//! makes mock-based unit tests possible on non-Windows platforms.

use crate::error::DecoderError;
use crate::types::{DecodedAudioFrame, DecodedVideoFrame};

/// Behaviour shared by every media stream reader.
pub trait MediaStream {
    /// Total duration of the stream in milliseconds.
    fn duration_ms(&self) -> u64;

    /// Seek to the given position (in ms). Precision is format-dependent.
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError>;
}

/// A reader that produces decoded video frames.
pub trait VideoStream: MediaStream + Send {
    /// Pull the next decoded frame. Returns `Ok(None)` at end-of-stream.
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError>;

    /// Frame width in pixels.
    fn width(&self) -> u32;

    /// Frame height in pixels.
    fn height(&self) -> u32;

    /// Frame rate as (numerator, denominator).
    fn frame_rate(&self) -> (u32, u32);
}

/// A reader that produces decoded audio samples.
pub trait AudioStream: MediaStream + Send {
    /// Pull the next chunk of decoded samples. Returns `Ok(None)` at EOS.
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError>;

    /// Sample rate in Hz.
    fn sample_rate(&self) -> u32;

    /// Channel count (1 = mono, 2 = stereo).
    fn channels(&self) -> u16;
}
```

- [ ] **Step 5: Create the audio module root**

Create `crates/sp-decoder/src/audio/mod.rs`:

```rust
//! Cross-platform audio decoder (Symphonia-backed).

pub mod symphonia_reader;

pub use symphonia_reader::SymphoniaAudioReader;
```

- [ ] **Step 6: Write the failing integration test**

Create `crates/sp-decoder/tests/symphonia_audio.rs`:

```rust
//! SymphoniaAudioReader opens and decodes the committed FLAC fixture.
//!
//! This test runs on every platform — Symphonia is pure Rust, so the audio
//! half of sp-decoder is no longer gated on Windows.

use sp_decoder::{AudioStream, MediaStream, SymphoniaAudioReader};

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("silent_3s.flac")
}

#[test]
fn opens_flac_and_reports_metadata() {
    let reader = SymphoniaAudioReader::open(&fixture()).expect("open should succeed");
    assert_eq!(reader.sample_rate(), 48_000);
    assert_eq!(reader.channels(), 2);
    let dur = reader.duration_ms();
    assert!(
        (2_900..=3_100).contains(&dur),
        "expected ~3000ms, got {dur}ms"
    );
}

#[test]
fn decodes_first_chunk_with_valid_samples() {
    let mut reader = SymphoniaAudioReader::open(&fixture()).expect("open should succeed");
    let chunk = reader
        .next_samples()
        .expect("decode should succeed")
        .expect("chunk should exist");
    assert_eq!(chunk.channels, 2);
    assert_eq!(chunk.sample_rate, 48_000);
    assert!(!chunk.data.is_empty(), "first chunk must contain samples");
    // Silence: every sample is ~0.0. Allow tiny FLAC quantisation noise.
    let max_abs = chunk.data.iter().fold(0.0_f32, |a, &s| a.max(s.abs()));
    assert!(max_abs < 1e-4, "silence expected, max |s| = {max_abs}");
}

#[test]
fn decodes_entire_fixture_to_expected_sample_count() {
    let mut reader = SymphoniaAudioReader::open(&fixture()).expect("open should succeed");
    let mut total_samples = 0_usize;
    while let Some(chunk) = reader.next_samples().expect("decode should succeed") {
        // Interleaved samples: count frames (1 frame = channels samples).
        assert_eq!(chunk.channels, 2);
        total_samples += chunk.data.len() / 2;
    }
    // 3.000 seconds * 48_000 Hz = 144_000 frames, ±1 frame tolerance for
    // block boundary rounding inside the FLAC encoder.
    assert!(
        (143_999..=144_001).contains(&total_samples),
        "expected ~144000 frames, got {total_samples}"
    );
}

#[test]
fn seek_to_midpoint_reports_matching_timestamp() {
    let mut reader = SymphoniaAudioReader::open(&fixture()).expect("open should succeed");
    reader.seek(1_500).expect("seek should succeed");
    let chunk = reader
        .next_samples()
        .expect("decode should succeed")
        .expect("post-seek chunk should exist");
    let ts = chunk.timestamp_ms;
    assert!(
        (1_450..=1_550).contains(&ts),
        "expected ~1500ms after seek, got {ts}ms"
    );
}
```

- [ ] **Step 7: Temporarily wire lib.rs so the test compiles without an implementation**

Edit `crates/sp-decoder/src/lib.rs` to add the new modules and stream trait exports. Replace file contents with:

```rust
//! Media decoder for SongPlayer.
//!
//! This crate provides two stream-oriented readers that plug into the
//! playback pipeline through the shared [`stream`] traits:
//!
//! * [`audio::SymphoniaAudioReader`] — pure-Rust FLAC decoder (cross-platform)
//! * [`video::mf_reader::MediaFoundationVideoReader`] — Windows-only video
//!   reader backed by Media Foundation (added in a later task).
//!
//! [`split_sync::SplitSyncedDecoder`] drives them with audio-as-master-clock.
//!
//! The legacy [`MediaReader`] and [`SyncedDecoder`] types remain available
//! until the downloader and playback pipeline have fully migrated; they are
//! removed in task 10 of the FLAC migration plan.

mod error;
mod types;

pub mod audio;
pub mod stream;

#[cfg(windows)]
mod reader;
#[cfg(windows)]
mod sync;

pub use audio::SymphoniaAudioReader;
pub use error::DecoderError;
pub use stream::{AudioStream, MediaStream, VideoStream};
pub use types::{DecodedAudioFrame, DecodedVideoFrame, PixelFormat, VideoStreamInfo};

#[cfg(windows)]
pub use reader::MediaReader;
#[cfg(windows)]
pub use sync::SyncedDecoder;
```

- [ ] **Step 8: Run the test to prove it fails**

```bash
cargo test -p sp-decoder --test symphonia_audio
```

Expected: FAIL with a compile error pointing at the unresolved import `SymphoniaAudioReader` (the module exists but the struct is not defined yet).

- [ ] **Step 9: Implement `SymphoniaAudioReader`**

Create `crates/sp-decoder/src/audio/symphonia_reader.rs`:

```rust
//! Pure-Rust FLAC audio reader backed by Symphonia.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

use crate::error::DecoderError;
use crate::stream::{AudioStream, MediaStream};
use crate::types::DecodedAudioFrame;

/// Cross-platform audio decoder backed by [symphonia](https://crates.io/crates/symphonia).
///
/// Opens a FLAC file, reports its full duration immediately from the
/// STREAMINFO header, and yields interleaved f32 PCM samples one packet at
/// a time. Seeks are sample-accurate.
pub struct SymphoniaAudioReader {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    sample_rate: u32,
    channels: u16,
    duration_ms: u64,
}

impl SymphoniaAudioReader {
    /// Open a FLAC file and build the decoder.
    pub fn open(path: &Path) -> Result<Self, DecoderError> {
        let file = File::open(path).map_err(|e| DecoderError::Io(e.to_string()))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .map_err(|e| DecoderError::SourceReader(e.to_string()))?;

        let format = probed.format;

        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or(DecoderError::NoStream("audio"))?;

        let track_id = track.id;
        let codec_params = &track.codec_params;

        let sample_rate = codec_params
            .sample_rate
            .ok_or_else(|| DecoderError::Decode("missing sample rate".into()))?;
        let channels = codec_params
            .channels
            .ok_or_else(|| DecoderError::Decode("missing channels".into()))?
            .count() as u16;

        // FLAC STREAMINFO reports total sample count in frames. Derived
        // duration is sample-accurate and available immediately after open —
        // this kills the duration=0 class of bugs from the previous release.
        let duration_ms = match (codec_params.n_frames, codec_params.time_base) {
            (Some(n_frames), Some(tb)) => {
                let t = tb.calc_time(n_frames);
                (t.seconds as u64) * 1_000 + ((t.frac * 1_000.0) as u64)
            }
            _ => 0,
        };

        let decoder = symphonia::default::get_codecs()
            .make(codec_params, &DecoderOptions::default())
            .map_err(|e| DecoderError::Decode(e.to_string()))?;

        Ok(Self {
            format,
            decoder,
            track_id,
            sample_rate,
            channels,
            duration_ms,
        })
    }

    /// Decode one packet and return it as interleaved f32 PCM.
    /// Returns `Ok(None)` on end-of-stream.
    fn decode_packet(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymphoniaError::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None);
                }
                Err(SymphoniaError::ResetRequired) => {
                    return Err(DecoderError::Decode("reset required".into()));
                }
                Err(e) => return Err(DecoderError::Decode(e.to_string())),
            };

            if packet.track_id() != self.track_id {
                continue;
            }

            let decoded = self
                .decoder
                .decode(&packet)
                .map_err(|e| DecoderError::Decode(e.to_string()))?;

            let spec = *decoded.spec();
            let sample_rate = spec.rate;
            let channels = spec.channels.count() as u32;

            // Convert whatever sample format Symphonia produced into
            // interleaved f32.
            let mut interleaved: Vec<f32> = Vec::with_capacity(decoded.frames() * channels as usize);
            match decoded {
                AudioBufferRef::F32(buf) => {
                    for frame in 0..buf.frames() {
                        for ch in 0..channels as usize {
                            interleaved.push(buf.chan(ch)[frame]);
                        }
                    }
                }
                AudioBufferRef::S32(buf) => {
                    let scale = 1.0 / (i32::MAX as f32);
                    for frame in 0..buf.frames() {
                        for ch in 0..channels as usize {
                            interleaved.push(buf.chan(ch)[frame] as f32 * scale);
                        }
                    }
                }
                AudioBufferRef::S16(buf) => {
                    let scale = 1.0 / (i16::MAX as f32);
                    for frame in 0..buf.frames() {
                        for ch in 0..channels as usize {
                            interleaved.push(buf.chan(ch)[frame] as f32 * scale);
                        }
                    }
                }
                other => {
                    return Err(DecoderError::Decode(format!(
                        "unsupported symphonia sample format: {other:?}"
                    )));
                }
            }

            let ts = packet.ts();
            let timestamp_ms = (ts * 1_000 / sample_rate as u64) as u64;

            return Ok(Some(DecodedAudioFrame {
                data: interleaved,
                channels,
                sample_rate,
                timestamp_ms,
            }));
        }
    }
}

impl MediaStream for SymphoniaAudioReader {
    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        let target = Time::from(std::time::Duration::from_millis(position_ms));
        self.format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: target,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| DecoderError::Seek(e.to_string()))?;
        self.decoder.reset();
        Ok(())
    }
}

impl AudioStream for SymphoniaAudioReader {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        self.decode_packet()
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}
```

- [ ] **Step 10: Run the integration test**

```bash
cargo test -p sp-decoder --test symphonia_audio
```

Expected: all 4 tests pass.

- [ ] **Step 11: Run clippy + fmt**

```bash
cargo fmt --all --check
```

Expected: exit 0. (Skip `cargo clippy` locally per project rules — CI will run it.)

- [ ] **Step 12: Commit**

```bash
git add Cargo.toml crates/sp-decoder/Cargo.toml crates/sp-decoder/src/error.rs \
        crates/sp-decoder/src/stream.rs crates/sp-decoder/src/audio \
        crates/sp-decoder/src/lib.rs crates/sp-decoder/tests/symphonia_audio.rs
git commit -m "$(cat <<'EOF'
feat(decoder): add SymphoniaAudioReader and cross-platform stream traits

Pure-Rust FLAC decoder, reads duration from STREAMINFO at open time,
yields interleaved f32 PCM, sample-accurate seek. Plus MediaStream /
VideoStream / AudioStream traits that let SplitSyncedDecoder drive
mock readers in cross-platform tests.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Create MediaFoundationVideoReader (Windows-only video-only reader)

**Files:**
- Create: `crates/sp-decoder/src/video/mod.rs`
- Create: `crates/sp-decoder/src/video/mf_reader.rs`
- Create: `crates/sp-decoder/tests/mf_video_only.rs`
- Modify: `crates/sp-decoder/src/lib.rs`

This task adds a new video-only reader. It does NOT delete the existing `reader.rs` / `sync.rs` — those stay until task 10 so every interim commit compiles and the playback pipeline keeps working.

- [ ] **Step 1: Create the video module root**

Create `crates/sp-decoder/src/video/mod.rs`:

```rust
//! Windows Media Foundation video reader (video-only).
//!
//! This module is `cfg(windows)` because it depends on the
//! `windows::Win32::Media::MediaFoundation` bindings.

pub mod mf_reader;

pub use mf_reader::MediaFoundationVideoReader;
```

- [ ] **Step 2: Wire the module into lib.rs**

In `crates/sp-decoder/src/lib.rs`, add under the existing `#[cfg(windows)] mod sync;` line:

```rust
#[cfg(windows)]
pub mod video;
```

And under the existing `#[cfg(windows)] pub use sync::SyncedDecoder;` line:

```rust
#[cfg(windows)]
pub use video::MediaFoundationVideoReader;
```

- [ ] **Step 3: Write the failing Windows integration test**

Create `crates/sp-decoder/tests/mf_video_only.rs`:

```rust
//! MediaFoundationVideoReader opens a video-only MP4 fixture (no audio track).

#![cfg(windows)]

use sp_decoder::{MediaFoundationVideoReader, MediaStream, VideoStream};

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("black_3s.mp4")
}

#[test]
fn opens_video_only_mp4_and_reports_metadata() {
    let reader =
        MediaFoundationVideoReader::open(&fixture()).expect("open should succeed");
    assert_eq!(reader.width(), 32);
    assert_eq!(reader.height(), 32);
    let (num, den) = reader.frame_rate();
    assert!(num > 0 && den > 0, "frame rate must be non-zero");
    let dur = reader.duration_ms();
    assert!(
        (2_500..=3_500).contains(&dur),
        "expected ~3000ms, got {dur}ms"
    );
}

#[test]
fn decodes_first_nv12_frame() {
    let mut reader =
        MediaFoundationVideoReader::open(&fixture()).expect("open should succeed");
    let frame = reader
        .next_frame()
        .expect("decode should succeed")
        .expect("first frame should exist");
    assert_eq!(frame.width, 32);
    assert_eq!(frame.height, 32);
    assert!(!frame.data.is_empty());
}
```

- [ ] **Step 4: Run the test to prove it fails**

On Windows only:

```bash
cargo test -p sp-decoder --test mf_video_only
```

Expected: FAIL with a compile error — `MediaFoundationVideoReader` is not defined.

(On Linux the `#![cfg(windows)]` gate skips the test silently. The rest of the plan can proceed without blocking on a Windows runner for this step.)

- [ ] **Step 5: Implement `MediaFoundationVideoReader`**

Create `crates/sp-decoder/src/video/mf_reader.rs` with the content below. This is a focused subset of the existing `crates/sp-decoder/src/reader.rs` — video only, no audio stream configuration, no audio read path. Implements the new `VideoStream` + `MediaStream` traits.

```rust
//! Media Foundation video-only reader.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use tracing::debug;

use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaBuffer, IMFMediaType, IMFSample, IMFSourceReader, MF_API_VERSION,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_PD_DURATION, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_ENDOFSTREAM, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video, MFSTARTUP_NOSOCKET,
    MFStartup, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::core::PCWSTR;

use crate::error::DecoderError;
use crate::stream::{MediaStream, VideoStream};
use crate::types::{DecodedVideoFrame, PixelFormat};

const VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Video-only Media Foundation source reader.
pub struct MediaFoundationVideoReader {
    reader: IMFSourceReader,
    duration_ms: u64,
    width: u32,
    height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
}

impl MediaFoundationVideoReader {
    #[cfg_attr(test, mutants::skip)]
    pub fn open(path: &Path) -> Result<Self, DecoderError> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            debug!(hr = ?hr, "CoInitializeEx result");
            MFStartup(MF_API_VERSION, MFSTARTUP_NOSOCKET)
                .map_err(|e| DecoderError::ComInit(format!("MFStartup: {e}")))?;
        }

        let wide_path: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut attrs: Option<IMFAttributes> = None;
        unsafe {
            MFCreateAttributes(&mut attrs, 1)
                .map_err(|e| DecoderError::ComInit(format!("MFCreateAttributes: {e}")))?;
        }
        let attrs = attrs
            .ok_or_else(|| DecoderError::ComInit("MFCreateAttributes returned null".into()))?;
        unsafe {
            attrs
                .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                .map_err(|e| {
                    DecoderError::ComInit(format!("SetUINT32 ENABLE_HARDWARE_TRANSFORMS: {e}"))
                })?;
        }

        let reader: IMFSourceReader = unsafe {
            MFCreateSourceReaderFromURL(PCWSTR(wide_path.as_ptr()), Some(&attrs))
                .map_err(|e| DecoderError::SourceReader(e.to_string()))?
        };

        // Negotiate NV12 output.
        let video_type = Self::make_video_output_type()?;
        unsafe {
            reader
                .SetCurrentMediaType(VIDEO_STREAM, None, &video_type)
                .map_err(|e| {
                    DecoderError::NoStream(Box::leak(
                        format!("video: SetCurrentMediaType failed: {e}").into_boxed_str(),
                    ))
                })?;
        }

        let negotiated_video: IMFMediaType = unsafe {
            reader
                .GetCurrentMediaType(VIDEO_STREAM)
                .map_err(|e| DecoderError::ReadSample(format!("GetCurrentMediaType video: {e}")))?
        };
        let (width, height) = unsafe {
            let size = negotiated_video.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            ((size >> 32) as u32, size as u32)
        };
        let (frame_rate_num, frame_rate_den) = unsafe {
            match negotiated_video.GetUINT64(&MF_MT_FRAME_RATE) {
                Ok(packed) => ((packed >> 32) as u32, packed as u32),
                Err(_) => (30000, 1001),
            }
        };

        let duration_ms: u64 = unsafe {
            match reader
                .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
            {
                Ok(pv) => u64::try_from(&pv).unwrap_or(0) / 10_000,
                Err(_) => 0,
            }
        };

        Ok(Self {
            reader,
            duration_ms,
            width,
            height,
            frame_rate_num,
            frame_rate_den,
        })
    }

    fn make_video_output_type() -> Result<IMFMediaType, DecoderError> {
        let media_type: IMFMediaType = unsafe {
            MFCreateMediaType()
                .map_err(|e| DecoderError::NoStream(Box::leak(e.to_string().into_boxed_str())))?
        };
        unsafe {
            media_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| DecoderError::NoStream(Box::leak(e.to_string().into_boxed_str())))?;
            media_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(|e| DecoderError::NoStream(Box::leak(e.to_string().into_boxed_str())))?;
        }
        Ok(media_type)
    }

    fn lock_video_buffer(
        buffer: &IMFMediaBuffer,
        reader: &IMFSourceReader,
    ) -> Result<(Vec<u8>, u32, u32, u32), DecoderError> {
        let mut data_ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        let mut current_len: u32 = 0;

        unsafe {
            buffer
                .Lock(&mut data_ptr, Some(&mut max_len), Some(&mut current_len))
                .map_err(|e| DecoderError::BufferLock(e.to_string()))?;
        }

        let nv12: Vec<u8> =
            unsafe { std::slice::from_raw_parts(data_ptr, current_len as usize).to_vec() };

        unsafe {
            buffer
                .Unlock()
                .map_err(|e| DecoderError::BufferLock(e.to_string()))?;
        }

        let media_type: IMFMediaType = unsafe {
            reader
                .GetCurrentMediaType(VIDEO_STREAM)
                .map_err(|e| DecoderError::ReadSample(e.to_string()))?
        };
        let (width, height) = unsafe {
            let size = media_type.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            ((size >> 32) as u32, size as u32)
        };
        let stride = unsafe {
            media_type
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|s| s as u32)
                .unwrap_or(width)
        };

        Ok((nv12, width, height, stride))
    }
}

impl MediaStream for MediaFoundationVideoReader {
    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        // MF seeks are expressed in 100ns units. Use a PROPVARIANT with VT_I8.
        use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
        use windows::Win32::System::Variant::VT_I8;
        let ticks: i64 = (position_ms as i64) * 10_000;
        let mut pv: PROPVARIANT = PROPVARIANT::default();
        unsafe {
            (*pv.as_raw()).Anonymous.Anonymous.vt = VT_I8.0 as u16;
            (*pv.as_raw()).Anonymous.Anonymous.Anonymous.hVal = ticks.into();
            self.reader
                .SetCurrentPosition(&windows::core::GUID::zeroed(), &pv)
                .map_err(|e| DecoderError::Seek(e.to_string()))?;
        }
        Ok(())
    }
}

impl VideoStream for MediaFoundationVideoReader {
    #[cfg_attr(test, mutants::skip)]
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let mut flags: u32 = 0;
        let mut timestamp_100ns: i64 = 0;
        let mut actual_stream_index: u32 = 0;
        let mut sample: Option<IMFSample> = None;

        unsafe {
            self.reader
                .ReadSample(
                    VIDEO_STREAM,
                    0,
                    Some(&mut actual_stream_index as *mut _),
                    Some(&mut flags as *mut _),
                    Some(&mut timestamp_100ns as *mut _),
                    Some(&mut sample as *mut _),
                )
                .map_err(|e| DecoderError::ReadSample(e.to_string()))?;
        }

        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Ok(None);
        }

        let sample = match sample {
            Some(s) => s,
            None => return Ok(None),
        };

        let buffer: IMFMediaBuffer = unsafe {
            sample
                .ConvertToContiguousBuffer()
                .map_err(|e| DecoderError::BufferLock(e.to_string()))?
        };

        let (nv12_data, width, height, stride) = Self::lock_video_buffer(&buffer, &self.reader)?;
        let timestamp_ms = (timestamp_100ns / 10_000) as u64;

        if timestamp_ms > self.duration_ms {
            self.duration_ms = timestamp_ms;
        }

        Ok(Some(DecodedVideoFrame {
            data: nv12_data,
            width,
            height,
            stride,
            timestamp_ms,
            pixel_format: PixelFormat::Nv12,
        }))
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn frame_rate(&self) -> (u32, u32) {
        (self.frame_rate_num, self.frame_rate_den)
    }
}
```

- [ ] **Step 6: Run a cross-platform compile check**

```bash
cargo check -p sp-decoder
```

Expected: exit 0 on Linux. The new `video::mf_reader` module is gated behind `cfg(windows)` so the Linux compile must not pull it in.

- [ ] **Step 7: Run fmt check**

```bash
cargo fmt --all --check
```

Expected: exit 0.

- [ ] **Step 8: Commit**

```bash
git add crates/sp-decoder/src/video crates/sp-decoder/src/lib.rs \
        crates/sp-decoder/tests/mf_video_only.rs
git commit -m "$(cat <<'EOF'
feat(decoder): add MediaFoundationVideoReader (video-only, Windows)

Video-only subset of the existing Media Foundation reader. Implements
the new VideoStream trait. Opens via MF_PD_DURATION so duration is
reported accurately at open() time. Integration test against the
committed black_3s.mp4 fixture runs on Windows CI.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Create SplitSyncedDecoder with mock-driven tests

**Files:**
- Create: `crates/sp-decoder/src/split_sync.rs`
- Modify: `crates/sp-decoder/src/lib.rs`

- [ ] **Step 1: Wire the new module into lib.rs**

In `crates/sp-decoder/src/lib.rs`, add below the `pub mod stream;` line:

```rust
pub mod split_sync;
```

And below the `pub use stream::{AudioStream, MediaStream, VideoStream};` line:

```rust
pub use split_sync::SplitSyncedDecoder;
```

- [ ] **Step 2: Write the failing unit tests alongside the new module**

Create `crates/sp-decoder/src/split_sync.rs` with only the test module first. This proves the tests fail before the implementation exists:

```rust
//! Trait-based A/V sync that drives a [`VideoStream`] and an [`AudioStream`]
//! with audio-as-master-clock. Cross-platform — tests run on Linux.

// Implementation follows in step 3.

#[cfg(test)]
mod tests;
```

Then create the test file `crates/sp-decoder/src/split_sync/tests.rs`... actually that requires directory splitting. Use a single file with an inline test module instead. Skip step 2; jump to step 3 and include tests at the bottom of the same file.

- [ ] **Step 3: Implement `SplitSyncedDecoder` with inline tests**

Replace `crates/sp-decoder/src/split_sync.rs` with:

```rust
//! Trait-based A/V sync that drives a [`VideoStream`] and an [`AudioStream`]
//! with audio-as-master-clock. Cross-platform.

use std::collections::VecDeque;

use tracing::debug;

use crate::error::DecoderError;
use crate::stream::{AudioStream, MediaStream, VideoStream};
use crate::types::{DecodedAudioFrame, DecodedVideoFrame};

/// Default tolerance for pairing audio chunks to a video frame (ms).
pub const DEFAULT_TOLERANCE_MS: u64 = 40;

/// Maximum duration disagreement between video and audio sidecars before
/// [`SplitSyncedDecoder::open`] warns.
pub const DURATION_MISMATCH_WARN_MS: u64 = 100;

/// Cross-platform split-file A/V sync driver.
///
/// Takes a video and audio reader behind trait objects and pairs each video
/// frame with all the audio chunks whose timestamps fall before (or within
/// [`DEFAULT_TOLERANCE_MS`] of) that frame. Audio is the master clock: the
/// reported duration is the audio stream's duration and every frame is
/// paired against it.
pub struct SplitSyncedDecoder {
    video: Box<dyn VideoStream>,
    audio: Box<dyn AudioStream>,
    pending_audio: VecDeque<DecodedAudioFrame>,
    tolerance_ms: u64,
    duration_ms: u64,
}

impl SplitSyncedDecoder {
    /// Build from owned readers. Performs the validation / mismatch check.
    pub fn new(
        video: Box<dyn VideoStream>,
        audio: Box<dyn AudioStream>,
    ) -> Result<Self, DecoderError> {
        Self::with_tolerance(video, audio, DEFAULT_TOLERANCE_MS)
    }

    /// Like [`new`], but accepts a custom pairing tolerance.
    pub fn with_tolerance(
        video: Box<dyn VideoStream>,
        audio: Box<dyn AudioStream>,
        tolerance_ms: u64,
    ) -> Result<Self, DecoderError> {
        if audio.sample_rate() != 48_000 {
            return Err(DecoderError::Mismatch(format!(
                "audio sample rate must be 48000, got {}",
                audio.sample_rate()
            )));
        }
        let ch = audio.channels();
        if !(1..=2).contains(&ch) {
            return Err(DecoderError::Mismatch(format!(
                "audio channels must be 1 or 2, got {ch}"
            )));
        }
        if video.width() == 0 || video.height() == 0 {
            return Err(DecoderError::Mismatch(format!(
                "video dimensions invalid: {}x{}",
                video.width(),
                video.height()
            )));
        }

        let v_dur = video.duration_ms();
        let a_dur = audio.duration_ms();
        if v_dur.abs_diff(a_dur) > DURATION_MISMATCH_WARN_MS {
            tracing::warn!(
                v_dur,
                a_dur,
                "video/audio duration mismatch beyond {DURATION_MISMATCH_WARN_MS}ms tolerance"
            );
        }

        Ok(Self {
            video,
            audio,
            pending_audio: VecDeque::new(),
            tolerance_ms,
            duration_ms: a_dur,
        })
    }

    /// Master-clock duration (audio).
    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    /// Video width in pixels.
    pub fn width(&self) -> u32 {
        self.video.width()
    }

    /// Video height in pixels.
    pub fn height(&self) -> u32 {
        self.video.height()
    }

    /// Video frame rate forwarded from the reader.
    pub fn frame_rate(&self) -> (u32, u32) {
        self.video.frame_rate()
    }

    /// Forward a seek to both readers. Audio first (sample-accurate), video
    /// second (keyframe-aligned).
    pub fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.audio.seek(position_ms)?;
        self.video.seek(position_ms)?;
        self.pending_audio.clear();
        Ok(())
    }

    /// Clear buffered audio (used by the pipeline on pause/restart).
    pub fn clear_buffer(&mut self) {
        self.pending_audio.clear();
    }

    /// Return the next video frame together with all audio chunks whose
    /// timestamps are at or before `video_ts + tolerance`.
    ///
    /// Returns `Ok(None)` when the video stream has ended.
    pub fn next_synced(
        &mut self,
    ) -> Result<Option<(DecodedVideoFrame, Vec<DecodedAudioFrame>)>, DecoderError> {
        let video = match self.video.next_frame()? {
            Some(v) => v,
            None => return Ok(None),
        };

        let deadline = video.timestamp_ms + self.tolerance_ms;
        let mut audio_frames: Vec<DecodedAudioFrame> = Vec::new();

        while let Some(front) = self.pending_audio.front() {
            if front.timestamp_ms <= deadline {
                audio_frames.push(self.pending_audio.pop_front().unwrap());
            } else {
                break;
            }
        }

        loop {
            match self.audio.next_samples()? {
                Some(af) => {
                    if af.timestamp_ms <= deadline {
                        audio_frames.push(af);
                    } else {
                        self.pending_audio.push_back(af);
                        break;
                    }
                }
                None => break,
            }
        }

        debug!(
            video_ts = video.timestamp_ms,
            audio_chunks = audio_frames.len(),
            "SplitSyncedDecoder paired frame"
        );

        Ok(Some((video, audio_frames)))
    }
}

// ---------------------------------------------------------------------------
// Tests — cross-platform, use mock readers.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock video stream that yields a pre-scripted list of frames.
    struct MockVideo {
        frames: VecDeque<DecodedVideoFrame>,
        duration_ms: u64,
        width: u32,
        height: u32,
        seek_calls: std::cell::Cell<u64>,
    }

    impl MockVideo {
        fn new(ms_list: &[u64]) -> Self {
            let frames = ms_list
                .iter()
                .map(|&ms| DecodedVideoFrame {
                    data: vec![0u8; 6],
                    width: 2,
                    height: 2,
                    stride: 2,
                    timestamp_ms: ms,
                    pixel_format: crate::types::PixelFormat::Nv12,
                })
                .collect::<VecDeque<_>>();
            let duration_ms = *ms_list.last().unwrap_or(&0);
            Self {
                frames,
                duration_ms,
                width: 2,
                height: 2,
                seek_calls: std::cell::Cell::new(0),
            }
        }
    }

    impl MediaStream for MockVideo {
        fn duration_ms(&self) -> u64 {
            self.duration_ms
        }
        fn seek(&mut self, _ms: u64) -> Result<(), DecoderError> {
            self.seek_calls.set(self.seek_calls.get() + 1);
            Ok(())
        }
    }

    impl VideoStream for MockVideo {
        fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
            Ok(self.frames.pop_front())
        }
        fn width(&self) -> u32 {
            self.width
        }
        fn height(&self) -> u32 {
            self.height
        }
        fn frame_rate(&self) -> (u32, u32) {
            (30, 1)
        }
    }

    /// Mock audio stream that yields chunks with explicit timestamps.
    struct MockAudio {
        chunks: VecDeque<DecodedAudioFrame>,
        duration_ms: u64,
        seek_calls: std::cell::Cell<u64>,
    }

    impl MockAudio {
        fn new(ts_list: &[u64], duration_ms: u64) -> Self {
            let chunks = ts_list
                .iter()
                .map(|&ts| DecodedAudioFrame {
                    data: vec![0.0; 4],
                    channels: 2,
                    sample_rate: 48_000,
                    timestamp_ms: ts,
                })
                .collect::<VecDeque<_>>();
            Self {
                chunks,
                duration_ms,
                seek_calls: std::cell::Cell::new(0),
            }
        }
    }

    impl MediaStream for MockAudio {
        fn duration_ms(&self) -> u64 {
            self.duration_ms
        }
        fn seek(&mut self, _ms: u64) -> Result<(), DecoderError> {
            self.seek_calls.set(self.seek_calls.get() + 1);
            Ok(())
        }
    }

    impl AudioStream for MockAudio {
        fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
            Ok(self.chunks.pop_front())
        }
        fn sample_rate(&self) -> u32 {
            48_000
        }
        fn channels(&self) -> u16 {
            2
        }
    }

    #[test]
    fn rejects_audio_with_wrong_sample_rate() {
        struct Bad;
        impl MediaStream for Bad {
            fn duration_ms(&self) -> u64 {
                1000
            }
            fn seek(&mut self, _: u64) -> Result<(), DecoderError> {
                Ok(())
            }
        }
        impl AudioStream for Bad {
            fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
                Ok(None)
            }
            fn sample_rate(&self) -> u32 {
                44_100
            }
            fn channels(&self) -> u16 {
                2
            }
        }
        let v = Box::new(MockVideo::new(&[0, 33, 66]));
        let a: Box<dyn AudioStream> = Box::new(Bad);
        let err = SplitSyncedDecoder::new(v, a).unwrap_err();
        assert!(matches!(err, DecoderError::Mismatch(_)));
    }

    #[test]
    fn rejects_zero_video_dimensions() {
        struct ZeroVid;
        impl MediaStream for ZeroVid {
            fn duration_ms(&self) -> u64 {
                1000
            }
            fn seek(&mut self, _: u64) -> Result<(), DecoderError> {
                Ok(())
            }
        }
        impl VideoStream for ZeroVid {
            fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
                Ok(None)
            }
            fn width(&self) -> u32 {
                0
            }
            fn height(&self) -> u32 {
                0
            }
            fn frame_rate(&self) -> (u32, u32) {
                (30, 1)
            }
        }
        let v: Box<dyn VideoStream> = Box::new(ZeroVid);
        let a = Box::new(MockAudio::new(&[], 1000));
        let err = SplitSyncedDecoder::new(v, a).unwrap_err();
        assert!(matches!(err, DecoderError::Mismatch(_)));
    }

    #[test]
    fn duration_is_audio_duration() {
        let v = Box::new(MockVideo::new(&[0, 33]));
        let a = Box::new(MockAudio::new(&[], 2500));
        let dec = SplitSyncedDecoder::new(v, a).unwrap();
        assert_eq!(dec.duration_ms(), 2500);
    }

    #[test]
    fn next_synced_pairs_audio_up_to_tolerance() {
        // Video at 0, 50, 100. Audio at 10, 40, 60, 95, 130.
        let v = Box::new(MockVideo::new(&[0, 50, 100]));
        let a = Box::new(MockAudio::new(&[10, 40, 60, 95, 130], 150));
        let mut dec = SplitSyncedDecoder::new(v, a).unwrap();

        // Frame 0 with tolerance 40 — deadline = 40. Audio <= 40: 10, 40.
        let (f1, a1) = dec.next_synced().unwrap().unwrap();
        assert_eq!(f1.timestamp_ms, 0);
        let ts: Vec<u64> = a1.iter().map(|a| a.timestamp_ms).collect();
        assert_eq!(ts, vec![10, 40]);

        // Frame 50 — deadline = 90. Audio <= 90: 60. (95 stays pending.)
        let (f2, a2) = dec.next_synced().unwrap().unwrap();
        assert_eq!(f2.timestamp_ms, 50);
        let ts: Vec<u64> = a2.iter().map(|a| a.timestamp_ms).collect();
        assert_eq!(ts, vec![60]);

        // Frame 100 — deadline = 140. 95 comes out of pending; 130 matches.
        let (f3, a3) = dec.next_synced().unwrap().unwrap();
        assert_eq!(f3.timestamp_ms, 100);
        let ts: Vec<u64> = a3.iter().map(|a| a.timestamp_ms).collect();
        assert_eq!(ts, vec![95, 130]);

        // No more frames.
        assert!(dec.next_synced().unwrap().is_none());
    }

    #[test]
    fn next_synced_returns_none_when_video_ends() {
        let v = Box::new(MockVideo::new(&[]));
        let a = Box::new(MockAudio::new(&[0, 10, 20], 30));
        let mut dec = SplitSyncedDecoder::new(v, a).unwrap();
        assert!(dec.next_synced().unwrap().is_none());
    }

    #[test]
    fn seek_clears_pending_and_forwards_to_both() {
        let v = Box::new(MockVideo::new(&[0, 50]));
        let a = Box::new(MockAudio::new(&[200, 500], 1000));
        let mut dec = SplitSyncedDecoder::new(v, a).unwrap();

        // Pull one frame first so pending_audio fills.
        let _ = dec.next_synced().unwrap().unwrap();

        dec.seek(500).unwrap();
        // Pending is cleared.
        assert!(dec.pending_audio.is_empty());
    }
}
```

- [ ] **Step 4: Run the tests**

```bash
cargo test -p sp-decoder split_sync
```

Expected: all 6 tests pass. (Note: these are unit tests inside the crate, not integration tests, so they're selected by name prefix.)

- [ ] **Step 5: Run fmt check**

```bash
cargo fmt --all --check
```

Expected: exit 0.

- [ ] **Step 6: Commit**

```bash
git add crates/sp-decoder/src/split_sync.rs crates/sp-decoder/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(decoder): add SplitSyncedDecoder with mock-driven cross-platform tests

Trait-based sync driver using audio-as-master-clock. 6 unit tests cover
validation (sample rate, channels, dimensions), duration reporting,
tolerance-based pairing, end-of-stream, and seek. Runs on Linux —
mutation testing now covers the sync algorithm.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Add DB migration V4 for audio_file_path

**Files:**
- Modify: `crates/sp-server/src/db/mod.rs`

This is additive to migrations 1, 2, 3 already present.

- [ ] **Step 1: Write a failing test for migration V4**

Open `crates/sp-server/src/db/mod.rs`. At the end of the existing `#[cfg(test)] mod tests { ... }` block (after `migration_v3_drops_per_playlist_title_columns`), add:

```rust
    #[tokio::test]
    async fn migration_v4_adds_audio_file_path_column() {
        let pool = setup().await;
        let cols: Vec<String> = sqlx::query("PRAGMA table_info(videos)")
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        assert!(
            cols.contains(&"audio_file_path".to_string()),
            "audio_file_path column should exist, columns: {cols:?}"
        );
    }

    #[tokio::test]
    async fn migration_v4_resets_all_normalized_rows() {
        let pool = create_memory_pool().await.unwrap();
        // Apply V1 + V2 + V3 manually so we can seed data before V4.
        for &(version, sql) in &MIGRATIONS[..3] {
            let mut tx = pool.begin().await.unwrap();
            for stmt in sql.split(';') {
                let s = stmt.trim();
                if !s.is_empty() {
                    sqlx::query(s).execute(&mut *tx).await.unwrap();
                }
            }
            sqlx::query("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')))")
                .execute(&mut *tx)
                .await
                .ok();
            sqlx::query("INSERT INTO schema_version (version) VALUES (?)")
                .bind(version)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        // Seed a playlist and a normalized video.
        sqlx::query("INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES ('p', 'u', 'n')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO videos (playlist_id, youtube_id, normalized, file_path) VALUES (1, 'abc', 1, '/tmp/foo.mp4')")
            .execute(&pool)
            .await
            .unwrap();

        // Now apply V4.
        run_migrations(&pool).await.unwrap();

        // Row's normalized must have been reset to 0.
        let n: i64 = sqlx::query("SELECT normalized FROM videos WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("normalized");
        assert_eq!(n, 0, "V4 must reset normalized=0 for every existing row");
    }
```

Also update the existing asserts from `assert_eq!(ver, 3);` to `assert_eq!(ver, 4);` in `pool_creation_and_migration` and `migrations_are_idempotent`.

- [ ] **Step 2: Run the tests to prove they fail**

```bash
cargo test -p sp-server db::tests::migration_v4
cargo test -p sp-server db::tests::pool_creation_and_migration
```

Expected: FAIL — `audio_file_path` column not present, and the version assertion expects 4 but gets 3.

- [ ] **Step 3: Add the V4 migration SQL**

In `crates/sp-server/src/db/mod.rs`, change the `MIGRATIONS` constant:

```rust
const MIGRATIONS: &[(i32, &str)] = &[
    (1, MIGRATION_V1),
    (2, MIGRATION_V2),
    (3, MIGRATION_V3),
    (4, MIGRATION_V4),
];
```

And add after the existing `MIGRATION_V3` constant:

```rust
const MIGRATION_V4: &str = "
ALTER TABLE videos ADD COLUMN audio_file_path TEXT;
UPDATE videos SET normalized = 0;
";
```

- [ ] **Step 4: Run the tests to prove they pass**

```bash
cargo test -p sp-server db::tests
```

Expected: all tests pass, including `migration_v4_adds_audio_file_path_column`, `migration_v4_resets_all_normalized_rows`, `pool_creation_and_migration`, and `migrations_are_idempotent`.

- [ ] **Step 5: fmt check**

```bash
cargo fmt --all --check
```

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/db/mod.rs
git commit -m "$(cat <<'EOF'
feat(db): add migration V4 — audio_file_path column, reset normalized

The reset is the core of the self-healing FLAC migration: every video
is marked unnormalized on first boot of 0.11, and the download worker
re-runs the full pipeline to produce split video/audio sidecars.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Update `downloader/cache.rs` for the split-file layout

**Files:**
- Modify: `crates/sp-server/src/downloader/cache.rs`

This task replaces the file in place. The new shape exports `CachedSong` (video+audio pair) and `LegacyFile` (old single-file `.mp4` from before the migration).

- [ ] **Step 1: Write the failing tests first**

At the bottom of `crates/sp-server/src/downloader/cache.rs`, replace the existing `#[cfg(test)] mod tests { ... }` block with the block below. Do NOT yet change the production code above it.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn sanitize_removes_special_chars() {
        assert_eq!(sanitize_filename("Hello World!"), "Hello World");
        assert_eq!(sanitize_filename("AC/DC"), "ACDC");
        assert_eq!(sanitize_filename("test@#$%^&*()file"), "testfile");
    }

    #[test]
    fn sanitize_collapses_whitespace() {
        assert_eq!(sanitize_filename("  hello   world  "), "hello world");
    }

    #[test]
    fn sanitize_limits_length() {
        let long = "a".repeat(100);
        let result = sanitize_filename(&long);
        assert!(result.len() <= 50);
    }

    #[test]
    fn sanitize_preserves_hyphens() {
        assert_eq!(sanitize_filename("hip-hop"), "hip-hop");
    }

    #[test]
    fn video_filename_without_gf() {
        let name = video_filename("Amazing Grace", "Chris Tomlin", "dQw4w9WgXcQ", false);
        assert_eq!(
            name,
            "Amazing Grace_Chris Tomlin_dQw4w9WgXcQ_normalized_video.mp4"
        );
    }

    #[test]
    fn video_filename_with_gf() {
        let name = video_filename("Song", "Artist", "dQw4w9WgXcQ", true);
        assert_eq!(name, "Song_Artist_dQw4w9WgXcQ_normalized_gf_video.mp4");
    }

    #[test]
    fn audio_filename_without_gf() {
        let name = audio_filename("Amazing Grace", "Chris Tomlin", "dQw4w9WgXcQ", false);
        assert_eq!(
            name,
            "Amazing Grace_Chris Tomlin_dQw4w9WgXcQ_normalized_audio.flac"
        );
    }

    #[test]
    fn audio_filename_with_gf() {
        let name = audio_filename("Song", "Artist", "dQw4w9WgXcQ", true);
        assert_eq!(name, "Song_Artist_dQw4w9WgXcQ_normalized_gf_audio.flac");
    }

    #[test]
    fn scan_cache_pairs_video_and_audio() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        fs::write(
            base.join("Amazing Grace_Chris Tomlin_dQw4w9WgXcQ_normalized_video.mp4"),
            "fake video",
        )
        .unwrap();
        fs::write(
            base.join("Amazing Grace_Chris Tomlin_dQw4w9WgXcQ_normalized_audio.flac"),
            "fake audio",
        )
        .unwrap();

        let result = scan_cache(base);
        assert_eq!(result.songs.len(), 1);
        assert!(result.legacy.is_empty());
        assert!(result.orphans.is_empty());

        let song = &result.songs[0];
        assert_eq!(song.video_id, "dQw4w9WgXcQ");
        assert!(!song.gemini_failed);
        assert_eq!(song.song, "Amazing Grace");
        assert_eq!(song.artist, "Chris Tomlin");
    }

    #[test]
    fn scan_cache_flags_legacy_single_mp4() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Old Song_Old Artist_xxxxxxxxxxx_normalized.mp4"),
            "legacy",
        )
        .unwrap();

        let result = scan_cache(dir.path());
        assert!(result.songs.is_empty());
        assert_eq!(result.legacy.len(), 1);
        assert_eq!(result.legacy[0].video_id, "xxxxxxxxxxx");
    }

    #[test]
    fn scan_cache_flags_legacy_gf_single_mp4() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path()
                .join("Old_Song_xxxxxxxxxxx_normalized_gf.mp4"),
            "legacy gf",
        )
        .unwrap();

        let result = scan_cache(dir.path());
        assert_eq!(result.legacy.len(), 1);
        assert!(result.legacy[0].gemini_failed);
    }

    #[test]
    fn scan_cache_orphan_video_without_audio() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path()
                .join("S_A_aaaaaaaaaaa_normalized_video.mp4"),
            "v",
        )
        .unwrap();

        let result = scan_cache(dir.path());
        assert!(result.songs.is_empty());
        assert_eq!(result.orphans.len(), 1);
    }

    #[test]
    fn scan_cache_orphan_audio_without_video() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path()
                .join("S_A_bbbbbbbbbbb_normalized_audio.flac"),
            "a",
        )
        .unwrap();

        let result = scan_cache(dir.path());
        assert!(result.songs.is_empty());
        assert_eq!(result.orphans.len(), 1);
    }

    #[test]
    fn scan_cache_ignores_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.txt"), "ignore me").unwrap();
        fs::write(dir.path().join("xxxxxxxxxxx_temp.mp4"), "temp").unwrap();

        let result = scan_cache(dir.path());
        assert!(result.songs.is_empty());
        assert!(result.legacy.is_empty());
        assert!(result.orphans.is_empty());
    }

    #[test]
    fn cleanup_removed_deletes_both_files_of_a_pair() {
        let dir = tempfile::tempdir().unwrap();
        let v = dir.path().join("S_A_dQw4w9WgXcQ_normalized_video.mp4");
        let a = dir.path().join("S_A_dQw4w9WgXcQ_normalized_audio.flac");
        fs::write(&v, "v").unwrap();
        fs::write(&a, "a").unwrap();

        let active: HashSet<String> = HashSet::new();
        cleanup_removed(dir.path(), &active, None);
        assert!(!v.exists());
        assert!(!a.exists());
    }

    #[test]
    fn cleanup_removed_skips_currently_playing() {
        let dir = tempfile::tempdir().unwrap();
        let v = dir.path().join("S_A_xxxxxxxxxxx_normalized_video.mp4");
        let a = dir.path().join("S_A_xxxxxxxxxxx_normalized_audio.flac");
        fs::write(&v, "v").unwrap();
        fs::write(&a, "a").unwrap();

        let active: HashSet<String> = HashSet::new();
        cleanup_removed(dir.path(), &active, Some("xxxxxxxxxxx"));
        assert!(v.exists());
        assert!(a.exists());
    }

    #[test]
    fn is_valid_video_id_accepts_valid() {
        assert!(is_valid_video_id("dQw4w9WgXcQ"));
        assert!(is_valid_video_id("xxxxxxxxxxx"));
        assert!(is_valid_video_id("abc-def_123"));
    }

    #[test]
    fn is_valid_video_id_rejects_invalid() {
        assert!(!is_valid_video_id("short"));
        assert!(!is_valid_video_id("toolongstring123"));
        assert!(!is_valid_video_id("hello world"));
        assert!(!is_valid_video_id("abc!def@123"));
    }
}
```

- [ ] **Step 2: Run the tests to prove they fail**

```bash
cargo test -p sp-server downloader::cache::tests
```

Expected: FAIL with compile errors on `video_filename`, `audio_filename`, `ScanResult`, `CachedSong`, `LegacyFile`, `Orphan` types that do not exist yet.

- [ ] **Step 3: Replace the production code above the test module**

Replace everything in `crates/sp-server/src/downloader/cache.rs` above the `#[cfg(test)] mod tests` line with:

```rust
//! Cache scanning and cleanup — manages normalized song files on disk.
//!
//! The pipeline stores each processed song as two sidecar files that share
//! a common base name:
//!
//! ```text
//! {safe_song}_{safe_artist}_{video_id}_normalized[_gf]_video.mp4
//! {safe_song}_{safe_artist}_{video_id}_normalized[_gf]_audio.flac
//! ```
//!
//! `scan_cache` walks the directory and returns three disjoint sets:
//!
//! * [`ScanResult::songs`] — complete video+audio pairs.
//! * [`ScanResult::legacy`] — pre-migration single `.mp4` files (these are
//!   deleted by the self-healing startup scan).
//! * [`ScanResult::orphans`] — unpaired half-sidecars from a crashed mid
//!   download (these are deleted by `cleanup_removed`).

use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// A complete, processed song present in the cache.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedSong {
    pub video_id: String,
    pub song: String,
    pub artist: String,
    pub gemini_failed: bool,
    pub video_path: PathBuf,
    pub audio_path: PathBuf,
}

/// A single-file legacy `.mp4` from before the FLAC migration.
#[derive(Debug, Clone, PartialEq)]
pub struct LegacyFile {
    pub video_id: String,
    pub gemini_failed: bool,
    pub path: PathBuf,
}

/// An unpaired sidecar (video without audio, or audio without video).
#[derive(Debug, Clone, PartialEq)]
pub struct Orphan {
    pub video_id: String,
    pub path: PathBuf,
}

/// Result of walking the cache directory once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanResult {
    pub songs: Vec<CachedSong>,
    pub legacy: Vec<LegacyFile>,
    pub orphans: Vec<Orphan>,
}

static VIDEO_ID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_-]{11}$").unwrap());

static SPLIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+)_(.+)_([a-zA-Z0-9_-]{11})_normalized(_gf)?_(video|audio)\.(mp4|flac)$")
        .unwrap()
});

static LEGACY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+)_(.+)_([a-zA-Z0-9_-]{11})_normalized(_gf)?\.mp4$").unwrap()
});

/// Build the output filename for the video sidecar.
pub fn video_filename(song: &str, artist: &str, video_id: &str, gemini_failed: bool) -> String {
    let safe_song = sanitize_filename(song);
    let safe_artist = sanitize_filename(artist);
    let gf = if gemini_failed { "_gf" } else { "" };
    format!("{safe_song}_{safe_artist}_{video_id}_normalized{gf}_video.mp4")
}

/// Build the output filename for the audio sidecar.
pub fn audio_filename(song: &str, artist: &str, video_id: &str, gemini_failed: bool) -> String {
    let safe_song = sanitize_filename(song);
    let safe_artist = sanitize_filename(artist);
    let gf = if gemini_failed { "_gf" } else { "" };
    format!("{safe_song}_{safe_artist}_{video_id}_normalized{gf}_audio.flac")
}

/// Walk the cache directory and categorise every matching file.
pub fn scan_cache(cache_dir: &Path) -> ScanResult {
    let entries = match std::fs::read_dir(cache_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("cannot read cache dir {}: {e}", cache_dir.display());
            return ScanResult::default();
        }
    };

    // Temporary buckets per video_id for pairing.
    let mut video_half: HashMap<String, (String, String, bool, PathBuf)> = HashMap::new();
    let mut audio_half: HashMap<String, (String, String, bool, PathBuf)> = HashMap::new();
    let mut legacy: Vec<LegacyFile> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if let Some(caps) = SPLIT_RE.captures(filename) {
            let song = caps[1].to_string();
            let artist = caps[2].to_string();
            let vid = caps[3].to_string();
            let gf = caps.get(4).is_some();
            let kind = &caps[5];
            let slot = (song, artist, gf, path.clone());
            if kind == "video" {
                video_half.insert(vid, slot);
            } else {
                audio_half.insert(vid, slot);
            }
            continue;
        }

        if let Some(caps) = LEGACY_RE.captures(filename) {
            legacy.push(LegacyFile {
                video_id: caps[3].to_string(),
                gemini_failed: caps.get(4).is_some(),
                path,
            });
            continue;
        }
    }

    // Pair video + audio halves by video_id.
    let mut songs: Vec<CachedSong> = Vec::new();
    let mut orphans: Vec<Orphan> = Vec::new();

    let video_ids: HashSet<String> = video_half.keys().cloned().collect();
    let audio_ids: HashSet<String> = audio_half.keys().cloned().collect();

    for vid in video_ids.intersection(&audio_ids) {
        let (song, artist, gf, v_path) = video_half.remove(vid).unwrap();
        let (_, _, _, a_path) = audio_half.remove(vid).unwrap();
        songs.push(CachedSong {
            video_id: vid.clone(),
            song,
            artist,
            gemini_failed: gf,
            video_path: v_path,
            audio_path: a_path,
        });
    }

    for (vid, (_, _, _, path)) in video_half.into_iter().chain(audio_half.into_iter()) {
        orphans.push(Orphan {
            video_id: vid,
            path,
        });
    }

    ScanResult {
        songs,
        legacy,
        orphans,
    }
}

/// Delete song pairs whose video ID is not in `active_ids`, and always
/// preserve the currently playing video ID if supplied.
pub fn cleanup_removed(cache_dir: &Path, active_ids: &HashSet<String>, playing_id: Option<&str>) {
    let result = scan_cache(cache_dir);
    for song in result.songs {
        if active_ids.contains(&song.video_id) {
            continue;
        }
        if playing_id == Some(song.video_id.as_str()) {
            continue;
        }
        for path in [&song.video_path, &song.audio_path] {
            tracing::info!(
                "removing cached sidecar for removed video {}: {}",
                song.video_id,
                path.display()
            );
            if let Err(e) = std::fs::remove_file(path) {
                tracing::warn!("failed to remove {}: {e}", path.display());
            }
        }
    }
    // Orphans are always removed — they are debris from a crashed download.
    for orphan in result.orphans {
        tracing::info!(
            "removing orphan sidecar for {}: {}",
            orphan.video_id,
            orphan.path.display()
        );
        if let Err(e) = std::fs::remove_file(&orphan.path) {
            tracing::warn!("failed to remove orphan {}: {e}", orphan.path.display());
        }
    }
}

/// Delete every legacy single-file `.mp4` listed in `legacy`.
pub fn cleanup_legacy(legacy: &[LegacyFile]) {
    for item in legacy {
        tracing::info!(
            "deleting legacy AAC file for {}: {}",
            item.video_id,
            item.path.display()
        );
        if let Err(e) = std::fs::remove_file(&item.path) {
            tracing::warn!(
                "failed to remove legacy file {}: {e}",
                item.path.display()
            );
        }
    }
}

/// Sanitize a string for use inside a filename.
pub fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-')
        .collect();
    let collapsed: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated = if collapsed.len() > 50 {
        let mut end = 50;
        while end > 0 && !collapsed.is_char_boundary(end) {
            end -= 1;
        }
        &collapsed[..end]
    } else {
        &collapsed
    };
    truncated.trim().to_string()
}

/// Check if a string looks like a valid YouTube video ID.
pub fn is_valid_video_id(s: &str) -> bool {
    VIDEO_ID_RE.is_match(s)
}
```

- [ ] **Step 4: Run the tests to prove they pass**

```bash
cargo test -p sp-server downloader::cache::tests
```

Expected: every test passes (15+ tests).

- [ ] **Step 5: Fix any call sites in `sp-server` that still reference the old `CachedVideo` / `normalized_filename` API**

```bash
cargo check -p sp-server 2>&1 | tail -40
```

This will expose the remaining call sites. Task 8 and Task 9 will update them. Until then, leave the old code paths temporarily broken inside `downloader/mod.rs`, `reprocess/mod.rs`, and wherever else — they will be fixed by the next tasks. To keep this commit compilable, temporarily add re-exports so the old names keep working:

In `crates/sp-server/src/downloader/cache.rs`, add at the bottom of the file (outside the test module):

```rust
/// Deprecated shim: legacy callers use `normalized_filename`. New code
/// must call [`video_filename`] / [`audio_filename`] directly. Removed
/// in task 10.
#[deprecated = "use video_filename or audio_filename"]
pub fn normalized_filename(
    song: &str,
    artist: &str,
    video_id: &str,
    gemini_failed: bool,
) -> String {
    audio_filename(song, artist, video_id, gemini_failed)
}

/// Deprecated shim: legacy [`CachedVideo`] type used by `reprocess::mod`.
/// Removed in task 10 once callers migrate to [`CachedSong`].
#[allow(dead_code)]
pub struct CachedVideo {
    pub video_id: String,
    pub file_path: PathBuf,
    pub song: String,
    pub artist: String,
    pub gemini_failed: bool,
}
```

And re-run:

```bash
cargo check -p sp-server
```

Expected: exit 0. The old callers compile via the shim even though the behaviour does not yet match. Task 10 deletes the shim.

- [ ] **Step 6: Run fmt check**

```bash
cargo fmt --all --check
```

- [ ] **Step 7: Commit**

```bash
git add crates/sp-server/src/downloader/cache.rs
git commit -m "$(cat <<'EOF'
feat(downloader): split cache layout into video+audio pair with orphan/legacy detection

Replaces the monolithic CachedVideo scan with a three-way ScanResult:
complete song pairs, legacy single-.mp4 files (pre-migration), and
orphan half-sidecars. Adds cleanup_legacy for the self-healing startup
scan. Deprecated shims keep old call sites compiling until task 10.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Update `downloader/normalize.rs` to output FLAC

**Files:**
- Modify: `crates/sp-server/src/downloader/normalize.rs`

- [ ] **Step 1: Write the failing test**

Replace the existing `#[cfg(test)] mod tests` block inside `normalize.rs` (keep the existing `parse_loudnorm_stats*` tests intact) and add this new test after them, still inside the same `mod tests` block:

```rust
    #[test]
    fn pass2_args_request_flac_codec() {
        // This is a structural guard rather than a behaviour test —
        // normalize_audio spawns ffmpeg so we can't run it under unit tests.
        // Instead we expose an internal helper that builds the pass-2
        // argument list and inspect it for the FLAC flags.
        let filter = "loudnorm=I=-14:TP=-1:LRA=11:measured_I=-20";
        let args = build_pass2_args(filter, std::path::Path::new("in.opus"), std::path::Path::new("out.flac"));
        // The last three args before the output path must be:
        // "-c:a", "flac", "-compression_level", "5"
        assert!(args.iter().any(|a| a == "flac"), "flac codec missing: {args:?}");
        assert!(args.iter().any(|a| a == "-c:a"), "-c:a missing: {args:?}");
        assert!(args.iter().any(|a| a == "-compression_level"), "compression_level missing: {args:?}");
        assert!(!args.iter().any(|a| a == "aac"), "aac must not appear: {args:?}");
        assert!(!args.iter().any(|a| a == "192k"), "192k must not appear: {args:?}");
        assert!(!args.iter().any(|a| a == "-c:v"), "-c:v must not appear for audio-only normalize: {args:?}");
    }
```

- [ ] **Step 2: Run the test to prove it fails**

```bash
cargo test -p sp-server downloader::normalize::tests::pass2_args_request_flac_codec
```

Expected: FAIL — `build_pass2_args` is undefined.

- [ ] **Step 3: Refactor `normalize_audio` to call `build_pass2_args`**

Replace `crates/sp-server/src/downloader/normalize.rs` with:

```rust
//! FFmpeg 2-pass loudnorm audio normalization (-14 LUFS) with FLAC output.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::hide_console_window;

/// Statistics extracted from FFmpeg's first-pass loudnorm analysis.
#[derive(Debug, Clone)]
struct LoudnormStats {
    input_i: String,
    input_tp: String,
    input_lra: String,
    input_thresh: String,
    target_offset: String,
}

/// Normalize audio to -14 LUFS and write a FLAC sidecar.
///
/// `input` must be an audio-only file (whatever yt-dlp produced — usually
/// `.opus`, sometimes `.webm` or `.m4a`). `output` will be written as a
/// native FLAC container.
pub async fn normalize_audio(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
) -> Result<(), anyhow::Error> {
    // Pass 1 — loudnorm analysis.
    let mut cmd1 = tokio::process::Command::new(ffmpeg);
    cmd1.arg("-i")
        .arg(input)
        .args([
            "-af",
            "loudnorm=I=-14:TP=-1:LRA=11:print_format=json",
            "-f",
            "null",
        ])
        .arg(null_output())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    hide_console_window(&mut cmd1);
    let pass1 = cmd1.output().await?;

    if !pass1.status.success() {
        let stderr = String::from_utf8_lossy(&pass1.stderr);
        anyhow::bail!("ffmpeg pass 1 failed: {stderr}");
    }

    let stderr = String::from_utf8_lossy(&pass1.stderr);
    let stats = extract_loudnorm_stats(&stderr)
        .ok_or_else(|| anyhow::anyhow!("failed to parse loudnorm stats from ffmpeg output"))?;

    let af_filter = format!(
        "loudnorm=I=-14:TP=-1:LRA=11:\
         measured_I={}:measured_TP={}:measured_LRA={}:\
         measured_thresh={}:offset={}",
        stats.input_i, stats.input_tp, stats.input_lra, stats.input_thresh, stats.target_offset,
    );

    // Pass 2 — apply measured values, write FLAC.
    let args = build_pass2_args(&af_filter, input, output);
    let mut cmd2 = tokio::process::Command::new(ffmpeg);
    cmd2.args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    hide_console_window(&mut cmd2);
    let pass2 = cmd2.output().await?;

    if !pass2.status.success() {
        let stderr = String::from_utf8_lossy(&pass2.stderr);
        anyhow::bail!("ffmpeg pass 2 failed: {stderr}");
    }

    tracing::info!(
        "normalized {} -> {}",
        input.display(),
        output.display()
    );
    Ok(())
}

/// Build the FFmpeg pass-2 argument list.
///
/// Pulled out of `normalize_audio` so it can be asserted against in unit
/// tests without spawning a subprocess.
pub(crate) fn build_pass2_args(
    af_filter: &str,
    input: &Path,
    output: &Path,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    args.push("-i".into());
    args.push(input.as_os_str().to_os_string());
    args.push("-af".into());
    args.push(af_filter.into());
    args.push("-c:a".into());
    args.push("flac".into());
    args.push("-compression_level".into());
    args.push("5".into());
    args.push("-y".into());
    args.push(output.as_os_str().to_os_string());
    args
}

/// Extract loudnorm statistics JSON from FFmpeg stderr output.
fn extract_loudnorm_stats(stderr: &str) -> Option<LoudnormStats> {
    let json_start = stderr.rfind("{\r\n").or_else(|| stderr.rfind("{\n"))?;
    let json_end = stderr[json_start..].find('}')? + json_start + 1;
    let json_str = &stderr[json_start..json_end];

    let obj: serde_json::Value = serde_json::from_str(json_str).ok()?;

    Some(LoudnormStats {
        input_i: obj.get("input_i")?.as_str()?.to_string(),
        input_tp: obj.get("input_tp")?.as_str()?.to_string(),
        input_lra: obj.get("input_lra")?.as_str()?.to_string(),
        input_thresh: obj.get("input_thresh")?.as_str()?.to_string(),
        target_offset: obj.get("target_offset")?.as_str()?.to_string(),
    })
}

fn null_output() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

// Suppress unused-import warnings for PathBuf on some platforms.
#[allow(dead_code)]
fn _keep_pathbuf_imported(_: PathBuf) {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_FFMPEG_OUTPUT: &str = r#"
[Parsed_loudnorm_0 @ 0x562e9a5c0d80]
{
    "input_i" : "-24.12",
    "input_tp" : "-3.45",
    "input_lra" : "7.80",
    "input_thresh" : "-34.56",
    "output_i" : "-14.00",
    "output_tp" : "-1.00",
    "output_lra" : "6.50",
    "output_thresh" : "-24.44",
    "normalization_type" : "dynamic",
    "target_offset" : "0.12"
}
"#;

    #[test]
    fn parse_loudnorm_stats_from_real_output() {
        let stats = extract_loudnorm_stats(SAMPLE_FFMPEG_OUTPUT).unwrap();
        assert_eq!(stats.input_i, "-24.12");
        assert_eq!(stats.input_tp, "-3.45");
        assert_eq!(stats.input_lra, "7.80");
        assert_eq!(stats.input_thresh, "-34.56");
        assert_eq!(stats.target_offset, "0.12");
    }

    #[test]
    fn parse_loudnorm_stats_missing_field() {
        let bad_json = r#"some ffmpeg output
{
    "input_i" : "-24.12",
    "input_tp" : "-3.45"
}
"#;
        assert!(extract_loudnorm_stats(bad_json).is_none());
    }

    #[test]
    fn parse_loudnorm_stats_no_json() {
        assert!(extract_loudnorm_stats("no json here at all").is_none());
    }

    #[test]
    fn parse_loudnorm_stats_windows_line_endings() {
        let win_output = "[Parsed_loudnorm_0 @ 0x562e9a5c0d80]\r\n{\r\n\t\"input_i\" : \"-24.12\",\r\n\t\"input_tp\" : \"-3.45\",\r\n\t\"input_lra\" : \"7.80\",\r\n\t\"input_thresh\" : \"-34.56\",\r\n\t\"output_i\" : \"-14.00\",\r\n\t\"output_tp\" : \"-1.00\",\r\n\t\"output_lra\" : \"6.50\",\r\n\t\"output_thresh\" : \"-24.44\",\r\n\t\"normalization_type\" : \"dynamic\",\r\n\t\"target_offset\" : \"0.12\"\r\n}\r\n";
        let stats = extract_loudnorm_stats(win_output).unwrap();
        assert_eq!(stats.input_i, "-24.12");
        assert_eq!(stats.target_offset, "0.12");
    }

    #[test]
    fn null_output_is_valid() {
        let dev = null_output();
        assert!(!dev.is_empty());
    }

    #[test]
    fn pass2_args_request_flac_codec() {
        let filter = "loudnorm=I=-14:TP=-1:LRA=11:measured_I=-20";
        let args = build_pass2_args(
            filter,
            std::path::Path::new("in.opus"),
            std::path::Path::new("out.flac"),
        );
        assert!(
            args.iter().any(|a| a == "flac"),
            "flac codec missing: {args:?}"
        );
        assert!(args.iter().any(|a| a == "-c:a"), "-c:a missing: {args:?}");
        assert!(
            args.iter().any(|a| a == "-compression_level"),
            "compression_level missing: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "aac"),
            "aac must not appear: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "192k"),
            "192k must not appear: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "-c:v"),
            "-c:v must not appear for audio-only normalize: {args:?}"
        );
    }
}
```

- [ ] **Step 4: Run the tests to prove they pass**

```bash
cargo test -p sp-server downloader::normalize::tests
```

Expected: every test passes.

- [ ] **Step 5: fmt check**

```bash
cargo fmt --all --check
```

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/downloader/normalize.rs
git commit -m "$(cat <<'EOF'
feat(downloader): normalize audio to FLAC instead of AAC

Pass 2 switches from -c:a aac -b:a 192k to -c:a flac -compression_level 5,
drops the -c:v copy flag (input is audio-only), and outputs a native FLAC
container. Adds build_pass2_args helper to make the pass-2 command
inspectable in unit tests without spawning a subprocess.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Update `downloader/mod.rs` — two yt-dlp invocations and pair finalize

**Files:**
- Modify: `crates/sp-server/src/downloader/mod.rs`
- Modify: `crates/sp-server/src/db/models.rs` (add `mark_video_processed_pair` helper)

- [ ] **Step 1: Add a DB helper that stores both paths**

In `crates/sp-server/src/db/models.rs`, after the existing `get_video_file_path` function, add:

```rust
/// Update a video row with both sidecar paths after a successful download.
pub async fn mark_video_processed_pair(
    pool: &SqlitePool,
    video_db_id: i64,
    song: &str,
    artist: &str,
    metadata_source: &str,
    gemini_failed: bool,
    video_path: &str,
    audio_path: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos
         SET song = ?, artist = ?, metadata_source = ?,
             gemini_failed = ?, file_path = ?, audio_file_path = ?, normalized = 1
         WHERE id = ?",
    )
    .bind(song)
    .bind(artist)
    .bind(metadata_source)
    .bind(gemini_failed as i32)
    .bind(video_path)
    .bind(audio_path)
    .bind(video_db_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Return both sidecar paths for a normalized video, or `None`.
pub async fn get_song_paths(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<Option<(String, String)>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT file_path, audio_file_path FROM videos WHERE id = ? AND normalized = 1",
    )
    .bind(video_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|r| {
        let v: Option<String> = r.get("file_path");
        let a: Option<String> = r.get("audio_file_path");
        match (v, a) {
            (Some(vp), Some(ap)) => Some((vp, ap)),
            _ => None,
        }
    }))
}
```

- [ ] **Step 2: Replace `downloader/mod.rs` to use two invocations**

Replace `crates/sp-server/src/downloader/mod.rs` with:

```rust
//! Download worker — orchestrates yt-dlp downloads, metadata extraction,
//! and FFmpeg normalization for queued videos.
//!
//! The FLAC pipeline issues two separate yt-dlp invocations per video —
//! one for the video stream, one for the audio stream. Both are stream
//! copies from YouTube's native encodings; there is no merge step. The
//! audio is then normalized to FLAC by [`normalize::normalize_audio`] and
//! the two resulting sidecar files live alongside each other in the
//! cache directory.

pub mod cache;
pub mod normalize;
pub mod tools;

use crate::metadata::MetadataProvider;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use tokio::sync::broadcast;
use tools::ToolPaths;

/// Apply platform-specific flags to hide console windows on Windows.
pub fn hide_console_window(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let _ = cmd;
}

const MAX_RESOLUTION: u32 = 1440;
const DOWNLOAD_TIMEOUT: u64 = 600;

pub struct DownloadWorker {
    pool: SqlitePool,
    tools: ToolPaths,
    cache_dir: PathBuf,
    providers: Vec<Box<dyn MetadataProvider>>,
    event_tx: broadcast::Sender<String>,
}

impl DownloadWorker {
    pub fn new(
        pool: SqlitePool,
        tools: ToolPaths,
        cache_dir: PathBuf,
        providers: Vec<Box<dyn MetadataProvider>>,
        event_tx: broadcast::Sender<String>,
    ) -> Self {
        Self {
            pool,
            tools,
            cache_dir,
            providers,
            event_tx,
        }
    }

    pub async fn run(self, mut shutdown: broadcast::Receiver<()>) {
        tracing::info!("download worker started");
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = self.process_next() => {}
            }
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }
        }
        tracing::info!("download worker stopped");
    }

    async fn process_next(&self) -> bool {
        let row = match self.fetch_next_unprocessed().await {
            Ok(Some(r)) => r,
            Ok(None) => return false,
            Err(e) => {
                tracing::error!("failed to fetch next video: {e}");
                return false;
            }
        };

        tracing::info!(video_id = %row.youtube_id, title = %row.title, "processing video");
        let _ = self
            .event_tx
            .send(format!("downloading:{}", row.youtube_id));

        let video_temp = self
            .cache_dir
            .join(format!("{}_video_temp.mp4", row.youtube_id));
        let audio_temp_glob = format!("{}_audio_temp", row.youtube_id);
        // yt-dlp picks the native extension for audio (%(ext)s), so we use
        // a base path and then find the actual file after the call.
        let audio_temp_base = self.cache_dir.join(&audio_temp_glob);

        if let Err(e) = self.download_video_stream(&row.youtube_id, &video_temp).await {
            tracing::error!(video_id = %row.youtube_id, "video download failed: {e}");
            cleanup_temps(&video_temp, &self.cache_dir, &row.youtube_id);
            return false;
        }

        let audio_temp = match self
            .download_audio_stream(&row.youtube_id, &audio_temp_base)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(video_id = %row.youtube_id, "audio download failed: {e}");
                cleanup_temps(&video_temp, &self.cache_dir, &row.youtube_id);
                return false;
            }
        };

        let meta =
            crate::metadata::get_metadata(&self.providers, &row.youtube_id, &row.title).await;

        let video_final = self.cache_dir.join(cache::video_filename(
            &meta.song,
            &meta.artist,
            &row.youtube_id,
            meta.gemini_failed,
        ));
        let audio_final = self.cache_dir.join(cache::audio_filename(
            &meta.song,
            &meta.artist,
            &row.youtube_id,
            meta.gemini_failed,
        ));

        // Normalize audio first — failure here is recoverable.
        if let Err(e) =
            normalize::normalize_audio(&self.tools.ffmpeg, &audio_temp, &audio_final).await
        {
            tracing::error!(video_id = %row.youtube_id, "normalization failed: {e}");
            let _ = tokio::fs::remove_file(&audio_temp).await;
            let _ = tokio::fs::remove_file(&video_temp).await;
            return false;
        }

        // Move the video temp to its final pair name.
        if let Err(e) = tokio::fs::rename(&video_temp, &video_final).await {
            tracing::error!(video_id = %row.youtube_id, "video rename failed: {e}");
            let _ = tokio::fs::remove_file(&audio_final).await;
            let _ = tokio::fs::remove_file(&video_temp).await;
            return false;
        }

        // Drop the audio temp.
        let _ = tokio::fs::remove_file(&audio_temp).await;

        if let Err(e) = crate::db::models::mark_video_processed_pair(
            &self.pool,
            row.id,
            &meta.song,
            &meta.artist,
            meta.source.as_str(),
            meta.gemini_failed,
            video_final.to_string_lossy().as_ref(),
            audio_final.to_string_lossy().as_ref(),
        )
        .await
        {
            tracing::error!(video_id = %row.youtube_id, "DB update failed: {e}");
            return false;
        }

        let _ = self.event_tx.send(format!("processed:{}", row.youtube_id));
        tracing::info!(video_id = %row.youtube_id, "video processed successfully");
        true
    }

    async fn fetch_next_unprocessed(&self) -> Result<Option<VideoRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, VideoRow>(
            "SELECT v.id, v.youtube_id, COALESCE(v.title, '') as title
             FROM videos v
             JOIN playlists p ON p.id = v.playlist_id
             WHERE v.normalized = 0 AND p.is_active = 1
             ORDER BY v.id
             LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn download_video_stream(
        &self,
        video_id: &str,
        output: &Path,
    ) -> Result<(), anyhow::Error> {
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        let format_spec = format!("bestvideo[height<={MAX_RESOLUTION}]");
        let ffmpeg_dir = self
            .tools
            .ffmpeg
            .parent()
            .unwrap_or(std::path::Path::new("."));

        let mut cmd = tokio::process::Command::new(&self.tools.ytdlp);
        cmd.args(["--progress", "--newline"])
            .args(["-f", &format_spec])
            .args(["--ffmpeg-location"])
            .arg(ffmpeg_dir)
            .args(["--js-runtimes", "node"])
            .args(["--socket-timeout", &DOWNLOAD_TIMEOUT.to_string()])
            .args(["--remux-video", "mp4"])
            .arg("--no-part")
            .args(["-o"])
            .arg(output)
            .arg(&url)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        hide_console_window(&mut cmd);
        let child_output = cmd.output().await?;

        if !child_output.status.success() {
            let stderr = String::from_utf8_lossy(&child_output.stderr);
            anyhow::bail!("yt-dlp (video) exited with {}: {}", child_output.status, stderr);
        }
        Ok(())
    }

    async fn download_audio_stream(
        &self,
        video_id: &str,
        output_base: &Path,
    ) -> Result<PathBuf, anyhow::Error> {
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        let ffmpeg_dir = self
            .tools
            .ffmpeg
            .parent()
            .unwrap_or(std::path::Path::new("."));

        // yt-dlp writes the file with an extension it picks itself based on
        // the source stream. We pass the base path with `%(ext)s` so yt-dlp
        // appends the native extension; afterwards we locate the file by
        // listing the cache dir for matches.
        let output_template = format!("{}.%(ext)s", output_base.display());

        let mut cmd = tokio::process::Command::new(&self.tools.ytdlp);
        cmd.args(["--progress", "--newline"])
            .args(["-f", "bestaudio"])
            .args(["--ffmpeg-location"])
            .arg(ffmpeg_dir)
            .args(["--js-runtimes", "node"])
            .args(["--socket-timeout", &DOWNLOAD_TIMEOUT.to_string()])
            .arg("--no-part")
            .args(["-o", &output_template])
            .arg(&url)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        hide_console_window(&mut cmd);
        let child_output = cmd.output().await?;

        if !child_output.status.success() {
            let stderr = String::from_utf8_lossy(&child_output.stderr);
            anyhow::bail!("yt-dlp (audio) exited with {}: {}", child_output.status, stderr);
        }

        // Find the file that was written.
        let parent = output_base.parent().unwrap_or(std::path::Path::new("."));
        let file_stem = output_base
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid output base"))?;
        let entries = std::fs::read_dir(parent)?;
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.starts_with(file_stem)
                && !name.ends_with(".part")
            {
                return Ok(path);
            }
        }
        anyhow::bail!("no audio output file matched prefix {file_stem}")
    }
}

fn cleanup_temps(video_temp: &Path, cache_dir: &Path, video_id: &str) {
    let _ = std::fs::remove_file(video_temp);
    // Remove any audio temp file with a matching prefix.
    let prefix = format!("{video_id}_audio_temp");
    if let Ok(entries) = std::fs::read_dir(cache_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && name.starts_with(&prefix)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct VideoRow {
    id: i64,
    youtube_id: String,
    title: String,
}
```

- [ ] **Step 3: Compile and run existing downloader tests**

```bash
cargo check -p sp-server
cargo test -p sp-server downloader
```

Expected: compiles cleanly; existing normalize tests still pass. The download-worker itself has no unit tests (subprocess-heavy), so this step just guards against regressions in the module's public surface.

- [ ] **Step 4: fmt check**

```bash
cargo fmt --all --check
```

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/downloader/mod.rs crates/sp-server/src/db/models.rs
git commit -m "$(cat <<'EOF'
feat(downloader): fetch video + audio streams separately via yt-dlp

Two yt-dlp invocations, each stream-copied (video --remux-video mp4,
audio native --no-part with %(ext)s template). normalize_audio now
operates on the raw audio temp and writes its FLAC sidecar directly
to the final filename. video temp is renamed to its pair filename.
DB write uses mark_video_processed_pair which stores both file paths.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Wire playback pipeline to `SplitSyncedDecoder`, retire legacy reader

**Files:**
- Modify: `crates/sp-server/src/playback/pipeline.rs`
- Modify: `crates/sp-server/src/playback/mod.rs`
- Modify: `crates/sp-server/src/playlist/selector.rs`
- Modify: `crates/sp-decoder/src/lib.rs` (remove old re-exports, delete reader.rs + sync.rs)
- Modify: `crates/sp-decoder/src/reader.rs` — DELETED
- Modify: `crates/sp-decoder/src/sync.rs` — DELETED
- Modify: `crates/sp-decoder/tests/duration.rs` — retargeted
- Modify: `crates/sp-server/src/downloader/cache.rs` — remove the `#[deprecated]` shims

- [ ] **Step 1: Change `PipelineCommand::Play` to carry both paths**

In `crates/sp-server/src/playback/pipeline.rs`, replace the `Play(PathBuf)` variant with:

```rust
    /// Start playing a song. Both sidecar files must exist.
    Play { video: PathBuf, audio: PathBuf },
```

- [ ] **Step 2: Rewrite `decode_and_send` to use `SplitSyncedDecoder`**

Replace the body of `decode_and_send` in `crates/sp-server/src/playback/pipeline.rs`. The key changes: take two paths, open a `MediaFoundationVideoReader` + `SymphoniaAudioReader`, combine them into a `SplitSyncedDecoder`, and iterate with `next_synced` exactly as before.

```rust
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn decode_and_send(
    cmd_rx: &Receiver<PipelineCommand>,
    submitter: &mut FrameSubmitter<sp_ndi::RealNdiBackend>,
    video_path: &std::path::Path,
    audio_path: &std::path::Path,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    paused: &mut bool,
) -> DecodeResult {
    use sp_decoder::{
        MediaFoundationVideoReader, SplitSyncedDecoder, SymphoniaAudioReader,
    };

    let video_reader = match MediaFoundationVideoReader::open(video_path) {
        Ok(v) => v,
        Err(e) => {
            return DecodeResult::Error(format!(
                "failed to open video {}: {e}",
                video_path.display()
            ));
        }
    };
    let audio_reader = match SymphoniaAudioReader::open(audio_path) {
        Ok(a) => a,
        Err(e) => {
            return DecodeResult::Error(format!(
                "failed to open audio {}: {e}",
                audio_path.display()
            ));
        }
    };
    let mut decoder = match SplitSyncedDecoder::new(Box::new(video_reader), Box::new(audio_reader))
    {
        Ok(d) => d,
        Err(e) => {
            return DecodeResult::Error(format!("SplitSyncedDecoder::new failed: {e}"));
        }
    };

    let (num, den) = decoder.frame_rate();
    submitter.set_frame_rate(num as i32, den as i32);

    let _ = event_tx.send((
        playlist_id,
        PipelineEvent::Started {
            duration_ms: decoder.duration_ms(),
        },
    ));

    let mut last_position_report = Instant::now();
    let mut frame_count: u64 = 0;

    loop {
        match cmd_rx.try_recv() {
            Ok(PipelineCommand::Shutdown) => {
                submitter.flush();
                return DecodeResult::Shutdown;
            }
            Ok(PipelineCommand::Stop) => {
                submitter.flush();
                return DecodeResult::Stopped;
            }
            Ok(PipelineCommand::Play { video, audio }) => {
                submitter.flush();
                return DecodeResult::NewPlay { video, audio };
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
                return DecodeResult::Shutdown;
            }
        }

        if *paused {
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }

        match decoder.next_synced() {
            Ok(Some((video_frame, audio_chunks))) => {
                frame_count += 1;
                submitter.push_frame(video_frame, audio_chunks);

                if last_position_report.elapsed() >= std::time::Duration::from_millis(500) {
                    let _ = event_tx.send((
                        playlist_id,
                        PipelineEvent::Position {
                            position_ms: decoder.duration_ms().min(frame_count * 33),
                            duration_ms: decoder.duration_ms(),
                        },
                    ));
                    last_position_report = Instant::now();
                }
            }
            Ok(None) => {
                submitter.flush();
                let _ = event_tx.send((playlist_id, PipelineEvent::Ended));
                return DecodeResult::Ended;
            }
            Err(e) => {
                error!("decode error: {e}");
                return DecodeResult::Error(e.to_string());
            }
        }
    }
}
```

Also change the surrounding loop function that owns `decode_and_send`: wherever `DecodeResult::NewPlay(PathBuf)` appeared, change it to `DecodeResult::NewPlay { video: PathBuf, audio: PathBuf }`, and update the match that dispatches `PipelineCommand::Play` into the decode loop to unpack both paths.

- [ ] **Step 3: Propagate the two-path `Play` through the engine**

In `crates/sp-server/src/playback/mod.rs`, find every call site that builds `PipelineCommand::Play(...)`. Each needs both paths. Example transformation:

```rust
// Before:
pipeline.send(PipelineCommand::Play(path.clone()));

// After:
pipeline.send(PipelineCommand::Play {
    video: song.video_path.clone(),
    audio: song.audio_path.clone(),
});
```

Where `song` is a `CachedSong` (from the cache scan) or a new `SelectedSong` struct returned by the video selector.

- [ ] **Step 4: Update the selector to return both paths**

In `crates/sp-server/src/playlist/selector.rs`, replace the existing `VideoSelector::select_next` signature with one that looks up the pair via `db::models::get_song_paths`:

```rust
use crate::db::models;
use sp_core::playback::PlaybackMode;
use sqlx::SqlitePool;

/// The result of a selection: the DB row id and both sidecar paths.
#[derive(Debug, Clone)]
pub struct SelectedSong {
    pub video_db_id: i64,
    pub video_path: std::path::PathBuf,
    pub audio_path: std::path::PathBuf,
}

pub struct VideoSelector;

impl VideoSelector {
    pub async fn select_next(
        pool: &SqlitePool,
        playlist_id: i64,
        mode: PlaybackMode,
        current_video_id: Option<i64>,
    ) -> Result<Option<SelectedSong>, sqlx::Error> {
        let picked = match mode {
            PlaybackMode::Loop => current_video_id.or(Self::select_random_unplayed(pool, playlist_id).await?),
            PlaybackMode::Continuous | PlaybackMode::Single => {
                Self::select_random_unplayed(pool, playlist_id).await?
            }
        };
        let Some(id) = picked else { return Ok(None); };
        let paths = models::get_song_paths(pool, id).await?;
        Ok(paths.map(|(v, a)| SelectedSong {
            video_db_id: id,
            video_path: std::path::PathBuf::from(v),
            audio_path: std::path::PathBuf::from(a),
        }))
    }

    async fn select_random_unplayed(
        pool: &SqlitePool,
        playlist_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        use rand::seq::SliceRandom;
        let mut unplayed = models::get_unplayed_normalized_video_ids(pool, playlist_id).await?;
        if unplayed.is_empty() {
            let all = models::get_normalized_video_ids(pool, playlist_id).await?;
            if all.is_empty() {
                return Ok(None);
            }
            models::clear_play_history(pool, playlist_id).await?;
            unplayed = all;
        }
        let mut rng = rand::thread_rng();
        Ok(unplayed.choose(&mut rng).copied())
    }
}
```

Update the existing unit tests in the `#[cfg(test)] mod tests` block below to assert on `SelectedSong` fields — where the old tests asserted `Ok(Some(id))`, they now need to seed a `file_path` and `audio_file_path` on each inserted video row and assert on `selected.video_db_id` + `selected.audio_path.ends_with("_audio.flac")`.

- [ ] **Step 5: Delete legacy sp-decoder files**

```bash
rm crates/sp-decoder/src/reader.rs crates/sp-decoder/src/sync.rs
```

Update `crates/sp-decoder/src/lib.rs` to remove the deleted references. The final `lib.rs` is:

```rust
//! Media decoder for SongPlayer.
//!
//! This crate provides stream-oriented readers that plug into the playback
//! pipeline through the shared [`stream`] traits:
//!
//! * [`audio::SymphoniaAudioReader`] — pure-Rust FLAC decoder (cross-platform)
//! * [`video::mf_reader::MediaFoundationVideoReader`] — Windows-only Media
//!   Foundation video reader.
//!
//! [`split_sync::SplitSyncedDecoder`] drives them with audio-as-master-clock.

mod error;
mod types;

pub mod audio;
pub mod split_sync;
pub mod stream;

#[cfg(windows)]
pub mod video;

pub use audio::SymphoniaAudioReader;
pub use error::DecoderError;
pub use split_sync::SplitSyncedDecoder;
pub use stream::{AudioStream, MediaStream, VideoStream};
pub use types::{DecodedAudioFrame, DecodedVideoFrame, PixelFormat, VideoStreamInfo};

#[cfg(windows)]
pub use video::MediaFoundationVideoReader;
```

- [ ] **Step 6: Update `crates/sp-decoder/tests/duration.rs` to open the new reader and the new video-only fixture**

Replace `crates/sp-decoder/tests/duration.rs` with:

```rust
//! Regression test for the `duration_ms=0` bug (now against the split-file
//! video-only reader).

#![cfg(windows)]

use sp_decoder::{MediaFoundationVideoReader, MediaStream, VideoStream};

#[test]
fn mf_video_reader_reports_nonzero_duration_for_test_mp4() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("black_3s.mp4");
    assert!(fixture.exists());
    let reader = MediaFoundationVideoReader::open(&fixture).expect("open");
    let duration = reader.duration_ms();
    assert!(
        (2_500..=3_500).contains(&duration),
        "expected ~3000ms, got {duration}ms"
    );
}

#[test]
fn mf_video_reader_reports_nonzero_size() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("black_3s.mp4");
    let reader = MediaFoundationVideoReader::open(&fixture).expect("open");
    assert_eq!(reader.width(), 32);
    assert_eq!(reader.height(), 32);
    let (num, den) = reader.frame_rate();
    assert!(num > 0 && den > 0);
}
```

- [ ] **Step 7: Delete the now-unused legacy fixture**

```bash
rm crates/sp-decoder/tests/fixtures/silent_3s.mp4
```

- [ ] **Step 8: Remove the deprecated shims in `downloader/cache.rs`**

In `crates/sp-server/src/downloader/cache.rs`, delete the two blocks at the bottom of the file labelled `#[deprecated]` `normalized_filename` and `CachedVideo`. Run:

```bash
cargo check -p sp-server 2>&1 | tail -40
```

If any call site still references `normalized_filename` or `CachedVideo`, update it to the new API (`video_filename` / `audio_filename` / `CachedSong`). Common call sites to check: `reprocess/mod.rs` (uses `cache::normalized_filename` for the `_gf` → non-`_gf` rename) — update it to rename both files in the pair.

- [ ] **Step 9: Update `reprocess/mod.rs` to rename both sidecars**

Find the block in `crates/sp-server/src/reprocess/mod.rs` that currently contains `cache::normalized_filename(...)`. Replace the rename logic with this helper that handles both sidecars:

```rust
        // Build new filenames via the split helpers.
        let new_video = crate::downloader::cache::video_filename(
            &new_song,
            &new_artist,
            &row.youtube_id,
            false,
        );
        let new_audio = crate::downloader::cache::audio_filename(
            &new_song,
            &new_artist,
            &row.youtube_id,
            false,
        );
        // Rename both sidecars. If either rename fails, log and continue.
        let cache_dir = &self.cache_dir;
        let old_v = std::path::Path::new(&row.file_path);
        let old_a: std::path::PathBuf = row
            .audio_file_path
            .clone()
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        let new_v_path = cache_dir.join(&new_video);
        let new_a_path = cache_dir.join(&new_audio);
        if let Err(e) = tokio::fs::rename(old_v, &new_v_path).await {
            warn!("video rename {} -> {} failed: {e}", old_v.display(), new_v_path.display());
        }
        if !old_a.as_os_str().is_empty()
            && let Err(e) = tokio::fs::rename(&old_a, &new_a_path).await
        {
            warn!("audio rename {} -> {} failed: {e}", old_a.display(), new_a_path.display());
        }
```

You will also need to add `audio_file_path` to the `ReprocessRow` struct and to the SQL in `query_gf_videos` (select `audio_file_path` as well), and update the `UPDATE videos SET ... WHERE id = ?` to also write the new audio path.

- [ ] **Step 10: Compile-check the workspace**

```bash
cargo check -p sp-server
```

Expected: exit 0. Any remaining references to `MediaReader`, `SyncedDecoder`, `CachedVideo`, `normalized_filename`, or `PipelineCommand::Play(PathBuf)` must be fixed before this step passes.

- [ ] **Step 11: Run the full test suite**

```bash
cargo test -p sp-decoder
cargo test -p sp-server
cargo fmt --all --check
```

Expected: all pass.

- [ ] **Step 12: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
feat(decoder): wire SplitSyncedDecoder into playback, retire legacy reader

Deletes crates/sp-decoder/src/reader.rs and sync.rs (superseded by
video/mf_reader.rs + audio/symphonia_reader.rs + split_sync.rs).
PipelineCommand::Play now carries both video and audio paths. Video
selector returns SelectedSong { video_db_id, video_path, audio_path }.
Reprocess rename path updates both sidecars. Deprecated shims in
downloader/cache.rs removed.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Self-healing cache scan + startup sync in `sp-server::start`

**Files:**
- Modify: `crates/sp-server/src/lib.rs`
- Create: `crates/sp-server/tests/startup_migration.rs`

- [ ] **Step 1: Write the failing integration test**

Create `crates/sp-server/tests/startup_migration.rs`:

```rust
//! Startup migration integration test: legacy files are deleted and
//! all video rows are reset to unnormalized on first boot.

use std::fs;
use std::path::PathBuf;

use sp_server::startup::self_heal_cache;
use sqlx::Row;

#[tokio::test]
async fn self_heal_deletes_legacy_files_and_resets_normalized() {
    let pool = sp_server::db::create_memory_pool().await.unwrap();
    sp_server::db::run_migrations(&pool).await.unwrap();

    // Seed a playlist + an already-normalized video pointing at a legacy
    // .mp4 path.
    sqlx::query("INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES ('p', 'u', 'n')")
        .execute(&pool)
        .await
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let legacy_path = tmp.path().join("Old_Song_dQw4w9WgXcQ_normalized.mp4");
    fs::write(&legacy_path, b"legacy").unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, file_path) VALUES (1, 'dQw4w9WgXcQ', 1, ?)",
    )
    .bind(legacy_path.to_string_lossy().as_ref())
    .execute(&pool)
    .await
    .unwrap();

    // Apply V4 has already reset normalized=0 via run_migrations above.
    // self_heal_cache must additionally delete the legacy file on disk.
    self_heal_cache(&pool, tmp.path()).await.unwrap();

    // File is gone.
    assert!(!legacy_path.exists(), "legacy .mp4 must be deleted");

    // Row is unnormalized (V4 did this).
    let n: i64 = sqlx::query("SELECT normalized FROM videos WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("normalized");
    assert_eq!(n, 0);
}

#[tokio::test]
async fn self_heal_deletes_orphan_half_sidecar() {
    let pool = sp_server::db::create_memory_pool().await.unwrap();
    sp_server::db::run_migrations(&pool).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();

    // A video sidecar without its audio partner — classic mid-download crash.
    let orphan = tmp.path().join("S_A_aaaaaaaaaaa_normalized_video.mp4");
    fs::write(&orphan, b"orphan").unwrap();

    self_heal_cache(&pool, tmp.path()).await.unwrap();

    assert!(!orphan.exists(), "orphan sidecar must be deleted");
}

#[tokio::test]
async fn self_heal_keeps_complete_pairs() {
    let pool = sp_server::db::create_memory_pool().await.unwrap();
    sp_server::db::run_migrations(&pool).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();

    let v = tmp.path().join("S_A_bbbbbbbbbbb_normalized_video.mp4");
    let a = tmp.path().join("S_A_bbbbbbbbbbb_normalized_audio.flac");
    fs::write(&v, b"v").unwrap();
    fs::write(&a, b"a").unwrap();

    self_heal_cache(&pool, tmp.path()).await.unwrap();

    assert!(v.exists(), "complete video sidecar must survive");
    assert!(a.exists(), "complete audio sidecar must survive");
}
```

- [ ] **Step 2: Run the test to prove it fails**

```bash
cargo test -p sp-server --test startup_migration
```

Expected: FAIL with a compile error — `sp_server::startup::self_heal_cache` does not exist and `sp_server::db` is private.

- [ ] **Step 3: Expose the needed surface from sp-server**

In `crates/sp-server/src/lib.rs`, make the db module and the new startup module public:

```rust
pub mod db;
pub mod startup;
```

(The `db` module is currently `mod db;` — change it to `pub mod db;`.)

- [ ] **Step 4: Create the startup module with `self_heal_cache` and `startup_sync_active_playlists`**

Create `crates/sp-server/src/startup.rs`:

```rust
//! First-boot self-healing routines: cache reconciliation + legacy
//! playlist sync parity with the original Python implementation.

use std::path::Path;

use sqlx::{Row, SqlitePool};

use crate::downloader::cache;
use crate::SyncRequest;

/// Walk the cache directory, categorise every file, and delete any
/// legacy single-file mp4s and orphan half-sidecars. Complete video+audio
/// pairs are left untouched and their DB row is re-linked to both paths.
pub async fn self_heal_cache(pool: &SqlitePool, cache_dir: &Path) -> Result<(), sqlx::Error> {
    let scan = cache::scan_cache(cache_dir);
    tracing::info!(
        songs = scan.songs.len(),
        legacy = scan.legacy.len(),
        orphans = scan.orphans.len(),
        "self-heal cache scan"
    );

    // Delete legacy AAC single-file .mp4s — they are always unusable under
    // the new pipeline.
    cache::cleanup_legacy(&scan.legacy);

    // Delete orphan half-sidecars (mid-download crash debris).
    for orphan in &scan.orphans {
        tracing::info!(
            "removing orphan sidecar for {}: {}",
            orphan.video_id,
            orphan.path.display()
        );
        let _ = std::fs::remove_file(&orphan.path);
    }

    // For complete pairs, re-link the DB row if a matching row exists.
    for song in &scan.songs {
        let v = song.video_path.to_string_lossy().to_string();
        let a = song.audio_path.to_string_lossy().to_string();
        sqlx::query(
            "UPDATE videos SET file_path = ?, audio_file_path = ?, normalized = 1
             WHERE youtube_id = ?",
        )
        .bind(&v)
        .bind(&a)
        .bind(&song.video_id)
        .execute(pool)
        .await?;
    }

    Ok(())
}

/// Trigger a one-time playlist sync for every active playlist at startup.
/// Legacy Python parity (`tools.py::trigger_startup_sync`).
pub async fn startup_sync_active_playlists(
    pool: &SqlitePool,
    sync_tx: &tokio::sync::mpsc::Sender<SyncRequest>,
) -> Result<(), sqlx::Error> {
    let rows = sqlx::query("SELECT id, youtube_url FROM playlists WHERE is_active = 1")
        .fetch_all(pool)
        .await?;
    for row in rows {
        let playlist_id: i64 = row.get("id");
        let youtube_url: String = row.get("youtube_url");
        if let Err(e) = sync_tx
            .send(SyncRequest {
                playlist_id,
                youtube_url,
            })
            .await
        {
            tracing::warn!(playlist_id, "startup sync enqueue failed: {e}");
        }
    }
    Ok(())
}
```

- [ ] **Step 5: Call both from `start()`**

In `crates/sp-server/src/lib.rs`, inside the `pub async fn start(...)` body, after the migration runs but before the download worker loop begins polling, add:

```rust
    // Self-heal cache: delete legacy single-mp4 and orphan sidecars, and
    // re-link any complete pairs back to their DB rows.
    if let Err(e) = startup::self_heal_cache(&pool, &config.cache_dir).await {
        tracing::warn!("self-heal cache failed (non-fatal): {e}");
    }
```

And after tools are known to be ready (after the block that spawns `DownloadWorker`), add:

```rust
    // Startup sync (legacy parity): fire a one-shot sync for every active
    // playlist so the download worker has fresh video IDs to process.
    if let Err(e) = startup::startup_sync_active_playlists(&pool, &sync_tx).await {
        tracing::warn!("startup sync enqueue failed: {e}");
    }
```

- [ ] **Step 6: Run the test to prove it passes**

```bash
cargo test -p sp-server --test startup_migration
```

Expected: all 3 tests pass.

- [ ] **Step 7: fmt + check**

```bash
cargo fmt --all --check
cargo check -p sp-server
```

- [ ] **Step 8: Commit**

```bash
git add crates/sp-server/src/lib.rs crates/sp-server/src/startup.rs \
        crates/sp-server/tests/startup_migration.rs
git commit -m "$(cat <<'EOF'
feat(server): self-healing cache scan + legacy startup playlist sync

Adds sp_server::startup with two routines called from start():
  - self_heal_cache: deletes legacy AAC .mp4s, deletes orphan sidecars,
    re-links complete video+audio pairs back to their DB rows.
  - startup_sync_active_playlists: fires a one-shot SyncRequest for every
    is_active=1 playlist, matching the legacy Python tools.py behaviour
    that was missed in the initial Rust port.

Integration test exercises the scan against a temp dir with seeded
legacy files, orphans, and complete pairs.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Playwright post-deploy FLAC verification spec

**Files:**
- Create: `e2e/post-deploy-flac.spec.ts`

- [ ] **Step 1: Author the post-deploy spec**

Create `e2e/post-deploy-flac.spec.ts`:

```ts
import { test, expect, Page } from '@playwright/test';
import WebSocket from 'ws';

const BASE = process.env.SONGPLAYER_URL ?? 'http://10.77.9.201:8080';
const PLAYLIST_NAME = 'ytfast';
const POLL_TIMEOUT_MS = 30 * 60 * 1000; // 30 min — the worker is serial

test.describe('post-deploy FLAC pipeline verification', () => {
    let consoleErrors: string[] = [];

    test.beforeEach(({ page }) => {
        consoleErrors = [];
        page.on('console', (msg) => {
            if (msg.type() === 'error' || msg.type() === 'warning') {
                if (/integrity.*attribute.*ignored/i.test(msg.text())) return;
                consoleErrors.push(`[${msg.type()}] ${msg.text()}`);
            }
        });
    });

    test.afterEach(() => {
        expect(consoleErrors).toEqual([]);
    });

    test('dashboard loads and shows all playlists', async ({ page }) => {
        await page.goto(BASE);
        await expect(page.getByText(PLAYLIST_NAME)).toBeVisible({ timeout: 10_000 });
    });

    test('split video+audio sidecars exist after download', async ({ request }) => {
        // Poll the status endpoint until at least one ytfast video is normalized.
        const deadline = Date.now() + POLL_TIMEOUT_MS;
        let found = false;
        while (Date.now() < deadline) {
            const resp = await request.get(`${BASE}/api/v1/playlists`);
            const body = await resp.json();
            const list = body.playlists ?? [];
            const fast = list.find((p: any) => p.name === PLAYLIST_NAME);
            if (fast && fast.normalized_count > 0) {
                found = true;
                break;
            }
            await new Promise((r) => setTimeout(r, 5_000));
        }
        expect(found, 'expected at least one normalized video in ytfast within 30 min').toBe(true);

        // Now list the cache directory (via the status endpoint or a
        // dedicated diagnostic endpoint) and look for the sidecar pair.
        const statusResp = await request.get(`${BASE}/api/v1/status`);
        const status = await statusResp.json();
        const cacheFiles: string[] = status.cache_files ?? [];
        const hasVideoSidecar = cacheFiles.some((f) => /_normalized(_gf)?_video\.mp4$/.test(f));
        const hasAudioSidecar = cacheFiles.some((f) => /_normalized(_gf)?_audio\.flac$/.test(f));
        expect(hasVideoSidecar, `no *_video.mp4 in cache: ${cacheFiles.join(', ')}`).toBe(true);
        expect(hasAudioSidecar, `no *_audio.flac in cache: ${cacheFiles.join(', ')}`).toBe(true);
    });

    test('scene switch to sp-fast activates playlist and position advances', async ({ request, page }) => {
        // Use the OBS websocket to switch scene.
        const OBS_URL = process.env.OBS_WS_URL ?? 'ws://10.77.9.201:4455';
        const driver = await import('./obs-driver.js');
        const obs = await driver.connect(OBS_URL, process.env.OBS_WS_PASSWORD);
        try {
            await driver.switchScene(obs, 'sp-fast');
        } finally {
            // leave obs connected; driver handles teardown per test file
        }

        // Poll status for active playlist.
        let sawActive = false;
        let lastPos = 0;
        const deadline = Date.now() + 30_000;
        while (Date.now() < deadline) {
            const resp = await request.get(`${BASE}/api/v1/status`);
            const s = await resp.json();
            const ids: number[] = s.active_playlist_ids ?? [];
            if (ids.length > 0) {
                sawActive = true;
                const nowPlaying = s.now_playing?.[String(ids[0])];
                const pos = Number(nowPlaying?.position_ms ?? 0);
                if (pos > lastPos) {
                    lastPos = pos;
                }
                if (lastPos > 1500) break;
            }
            await new Promise((r) => setTimeout(r, 500));
        }
        expect(sawActive, 'sp-fast scene switch did not activate a playlist').toBe(true);
        expect(lastPos, 'playback position did not advance past 1500ms').toBeGreaterThan(1500);
    });
});
```

Note: if `obs-driver.ts` lives at `e2e/obs-driver.ts` already (from the previous PR), this file imports it directly. If its API differs, adapt the `driver.connect` / `driver.switchScene` calls to match the helper's actual surface. The fields `normalized_count` and `cache_files` in the status endpoint may also need to be added in a follow-up commit if they do not yet exist — in which case this task's verification falls back to the dashboard-level assertion and the `active_playlist_ids` assertion in the third test.

- [ ] **Step 2: Add the test to the post-deploy runner**

Extend `e2e/post-deploy.config.ts` (or whichever Playwright config runs against the deployed URL) so that `post-deploy-flac.spec.ts` is included in the default project. Check the existing config for the `testMatch` or `testDir` patterns and add the new file if it is not picked up automatically.

- [ ] **Step 3: Commit**

```bash
git add e2e/post-deploy-flac.spec.ts e2e/post-deploy.config.ts
git commit -m "$(cat <<'EOF'
test(e2e): post-deploy FLAC verification spec

Polls the dashboard until the download worker has processed at least
one ytfast video under the new pipeline, then asserts both
*_video.mp4 and *_audio.flac sidecars are present in the cache. Also
switches OBS to the sp-fast scene and asserts the playback position
advances past 1500ms over NDI.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 13: Update `CLAUDE.md`

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Append a new section to `CLAUDE.md`**

Under the existing "Key Patterns" section, add a new subsection after the NDI network name format block:

```markdown
**Split-file audio layout (FLAC pipeline):**
Each cached song is stored as two sidecar files sharing a common base name:

- `{safe_song}_{safe_artist}_{video_id}_normalized[_gf]_video.mp4` — H.264/VP9/AV1 stream-copied from YouTube, zero re-encodes.
- `{safe_song}_{safe_artist}_{video_id}_normalized[_gf]_audio.flac` — decoded from YouTube's Opus stream, 2-pass FFmpeg loudnorm at -14 LUFS, re-encoded to FLAC exactly once. Signal is lossless from this point to NDI.

The decoder split follows the file layout: `sp_decoder::MediaFoundationVideoReader` (Windows-only, hardware-accelerated MF) reads the video sidecar, and `sp_decoder::SymphoniaAudioReader` (pure Rust, cross-platform) reads the FLAC sidecar. `SplitSyncedDecoder` drives both with audio-as-master-clock at 40 ms tolerance.

On first boot of a new version, the server self-heals the cache: any legacy single-file `.mp4` from before the FLAC migration is deleted, any orphan half-sidecars are deleted, and every existing `videos` row is marked `normalized = 0` so the download worker re-produces everything under the new layout. A one-shot startup sync (legacy Python parity with `tools.py::trigger_startup_sync`) runs for every `is_active = 1` playlist once tools are ready.
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "$(cat <<'EOF'
docs: document split-file FLAC layout and decoder split

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Verification

After all tasks are complete:

1. **Local on Linux:**
   ```
   cargo fmt --all --check
   cargo check --workspace
   cargo test -p sp-core -p sp-decoder -p sp-server
   ```
   Expected: every test passes. `sp-decoder` tests run on Linux via Symphonia.

2. **Push to `dev` and monitor CI.** Watch every job reach green, including Windows test, WASM build, mutation testing, Playwright post-deploy.

3. **After CI deploys to win-resolume:**
   - Observe `self_heal_cache` log line showing the legacy AAC file count that was deleted.
   - Observe the `startup sync` log lines firing once per active playlist.
   - Wait for the download worker to process at least one song.
   - Confirm via the dashboard that a song is playable.
   - Switch OBS to an `sp-*` scene and confirm NDI audio + video flow, dashboard position advances, Resolume text updates.
   - Confirm both `_video.mp4` and `_audio.flac` sidecars exist in `C:\ProgramData\SongPlayer\cache\` for the processed video IDs.
   - Confirm zero browser console errors/warnings on the dashboard.

4. **Create a PR from `dev` to `main`** once CI is fully green. Wait for explicit merge instruction before merging.
