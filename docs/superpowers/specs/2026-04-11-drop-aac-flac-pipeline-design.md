# Drop AAC from Audio Pipeline — SOTA Split-File FLAC Design

**Issue:** [#10 — Drop AAC from audio pipeline: keep Opus end-to-end or use FLAC intermediate](https://github.com/zbynekdrlik/songplayer/issues/10)

**Scope:** Full greenfield redesign of the audio pipeline from YouTube to NDI. Eliminate every unnecessary lossy audio re-encode, pick file layouts that are optimal for the target use case, add the missing legacy startup-sync feature, and make the migration self-healing on first boot.

**Status:** Approved design. Ready for implementation plan.

---

## 1. Problem

The current audio path applies **two stacked lossy AAC generations** per video:

1. `yt-dlp` downloads YouTube's Opus 160 kbps audio stream, then internally re-encodes it to AAC ~128 kbps because `--merge-output-format mp4` forces MP4, which historically does not carry Opus.
2. `normalize_audio` decodes that AAC, runs a 2-pass FFmpeg `loudnorm` filter at −14 LUFS, and re-encodes to AAC 192 kbps.

A YouTube song that left Google's servers as Opus 160 k at 48 kHz hits our disk as AAC 192 k after passing through **three lossy generations** (YouTube's own Opus encode + `yt-dlp`'s Opus→AAC + our loudnorm AAC→AAC).

The AAC-in-MP4 container choice exists purely because the legacy Python pipeline targeted OBS Media Source, which wants MP4. The new Rust pipeline does not use OBS Media Source: `sp-decoder` reads cached files directly, decodes to 32-bit float PCM, and pushes the samples over NDI as FLTP. AAC is no longer required for compatibility — it is pure quality loss with no remaining upside.

## 2. Design goals

1. Minimize the number of lossy re-encode generations on the audio signal. Target: exactly one — YouTube's own Opus encode — with every subsequent step preserving the signal losslessly.
2. Never re-encode the video stream. The video bytes yt-dlp receives are the bytes on disk.
3. Eliminate Windows Media Foundation codec gambles (MKV demux, FLAC-in-MP4, Opus-in-MP4) by choosing container/codec combinations that are either trivially supported or handled by a decoder we control.
4. Keep the pipeline serial and CPU-bounded on the production win-resolume machine. No new parallel workers, no new live filters during playback.
5. Make the filesystem layout forward-compatible with anticipated next-iteration features — stem separation for karaoke, bilingual lyrics overlays, audio-only playback — without any further refactors.
6. Make the migration from the legacy AAC cache fully automatic on first boot, with no manual steps.
7. Restore the missing legacy startup-sync behavior (fires one playlist sync per active playlist after tools are ready) that was not carried over from the Python version to the Rust rewrite.

## 3. Architecture

The pipeline from YouTube to NDI becomes:

```
YouTube ─┬─► yt-dlp (video stream-copy)  ─► {id}_video.mp4  ─┐
         │                                                    │
         └─► yt-dlp (audio stream-copy)  ─► {id}_audio.opus   │
                                                │              │
                                                ▼              │
                                  FFmpeg 2-pass loudnorm       │
                                  -14 LUFS, decode Opus once   │
                                                │              │
                                                ▼              │
                                       {id}_audio.flac ────────┤
                                       (delete temp opus)      │
                                                               │
                                                               ▼
                                           Media Foundation  ──► NV12 frames ─┐
                                                                              │
                                           Symphonia         ──► f32 PCM  ────┼─► SplitSyncedDecoder ─► NDI
                                                                              │
```

**Audio signal path:**
Opus (YouTube) → FFmpeg decode → loudnorm → FLAC → Symphonia decode → f32 PCM → NDI FLTP.
One lossy-to-lossless transition. All subsequent steps are lossless.

**Video signal path:**
H.264 / VP9 / AV1 (YouTube) → yt-dlp stream-copy → MP4 → Media Foundation decode → NV12 → NDI.
Zero re-encode transitions.

**Decoder split:**
- **Media Foundation** remains the video decoder, opening `{id}_video.mp4`. Hardware-accelerated H.264/VP9/AV1 → NV12 is MF's strongest path.
- **Symphonia** becomes the audio decoder, opening `{id}_audio.flac`. Pure Rust, decodes FLAC natively, produces interleaved f32 PCM which is what `sp-ndi` already expects.

**Rationale for the split-file layout:**
- Each stream uses its optimal container: MP4 for video (MF's strongest demux), FLAC standalone for audio (Symphonia's most mature format).
- The two streams are logically independent. Having them as separate files reflects the pipeline structure rather than hiding it inside a muxed container.
- Stem separation (#14) requires audio-only input to the Demucs worker. With a split file, the audio sidecar is the input — no extraction step. With a single muxed file, Demucs would need to extract audio first, producing the same split layout anyway but with extra I/O.
- Future audio-only playback mode is trivially implemented by not opening the video reader.
- Rewriting audio without touching video (e.g. re-running stem separation with a better model) does not rewrite the large video file.
- The `sp-decoder` crate's audio half becomes cross-platform, expanding mutation-test coverage on Linux CI.

## 4. Crate and module impact

### `sp-decoder`

Today the entire crate is `#[cfg(windows)]`. The new layout introduces a trait-based split:

```rust
pub trait MediaStream {
    fn duration_ms(&self) -> u64;
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError>;
}

pub trait VideoStream: MediaStream {
    fn next_frame(&mut self) -> Option<DecodedVideoFrame>; // NV12
    fn frame_rate(&self) -> (u32, u32);
    fn width(&self) -> u32;
    fn height(&self) -> u32;
}

pub trait AudioStream: MediaStream {
    fn next_samples(&mut self) -> Option<DecodedAudioFrame>; // f32 interleaved
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
}
```

Concrete implementations:
- `MediaFoundationVideoReader` — `#[cfg(windows)]`, wraps `IMFSourceReader` configured for video only, reads NV12 frames. This is the existing `MediaReader` stripped of audio logic.
- `SymphoniaAudioReader` — cross-platform, no `cfg` gate. Opens FLAC via Symphonia's `FormatReader`, decodes to interleaved f32.

`SyncedDecoder` is replaced by a new `SplitSyncedDecoder` that takes `Box<dyn VideoStream>` and `Box<dyn AudioStream>` and drives them with audio-as-master-clock (same algorithm as today, same 40 ms tolerance). Because both readers are behind traits, unit tests run on Linux with mock implementations.

Module layout inside `crates/sp-decoder/src/`:
- `src/video/mf.rs` — `#[cfg(windows)]` Media Foundation video reader.
- `src/audio/symphonia.rs` — cross-platform audio reader.
- `src/sync.rs` — cross-platform trait-based `SplitSyncedDecoder`.
- `src/lib.rs` — re-exports. Video module is `#[cfg(windows)] pub mod video;`; audio and sync are unconditional.

### `sp-server`

Changes are contained to `downloader/` and a small addition to `lib.rs`:

- `downloader/mod.rs` — `download_video` becomes `download_streams` and produces two temp files (`{id}_video_temp.mp4`, `{id}_audio_temp.*`) via two separate `yt-dlp` invocations.
- `downloader/normalize.rs` — `normalize_audio` takes the audio temp path and writes `{id}_audio.flac`. Two-pass FFmpeg loudnorm exactly as today; output codec and container change, no video touched.
- `downloader/cache.rs` — `CachedVideo` becomes `CachedSong` with `video_path` and `audio_path`. Scan logic groups pairs by base name; orphans are cleaned up.
- `db/mod.rs` — adds migration V2 (see §5).
- `lib.rs::start` — adds a one-shot startup sync loop over active playlists after tools setup (restores legacy `tools.py:194` behavior).

### `sp-core`

No changes. Shared types don't know about file paths.

### `sp-ui`, `sp-ndi`, `src-tauri`

No changes. The dashboard, NDI sink, and Tauri shell are agnostic to file layout.

## 5. Database migration V2

One idempotent additive migration, applied on startup by the existing manual migration runner in `crates/sp-server/src/db/mod.rs`:

```sql
-- V2: split audio/video file paths for FLAC migration.
ALTER TABLE videos ADD COLUMN audio_file_path TEXT;

-- Every existing row points at a legacy .mp4 file. Reset normalized=0 so
-- the download worker re-runs the full pipeline and produces split files.
UPDATE videos SET normalized = 0;
```

The existing `file_path` column is kept and repurposed as the video path in code. SQLite `ALTER TABLE RENAME COLUMN` support is version-dependent and not worth the risk; the name stays, the semantics change, and a code comment explains the mapping.

After V2 runs, every row has `normalized = 0` and `audio_file_path = NULL`. The subsequent startup cache scan (§6) reconciles disk state, and the download worker re-produces every song.

## 6. Startup sequence

Order of operations in `sp-server::start()`:

1. Open SQLite pool, run all migrations up to V2.
2. Set up the tools manager and wait for `yt-dlp` / `ffmpeg` to be available.
3. Run the **self-healing cache scan** (new):
   - Walk the cache directory.
   - Files matching `{song}_{artist}_{id}_normalized[_gf]_(video|audio).(mp4|flac)` are grouped by video ID. Complete pairs update the DB row (`video_file_path`, `audio_file_path`, `normalized = 1`).
   - Files matching the **legacy** pattern `{song}_{artist}_{id}_normalized[_gf].mp4` are deleted. Every deletion is logged at `info`.
   - Orphan files (video without matching audio, or vice versa) are deleted. DB row reset to `normalized = 0`.
4. Run the **startup sync** (new, restores legacy behavior): for each `is_active = 1` playlist, fire one `SyncRequest` via the existing sync channel. This replaces the missing call from legacy Python `tools.py:194`.
5. Start the download worker, OBS client, Resolume registry, playback engine, and Axum server exactly as today.

The download worker wakes up to find `normalized = 0` for every row and begins processing serially through the new pipeline. A few hours later, the full catalog is re-cached in the split layout.

## 7. Download worker — detailed flow

For each video with `normalized = 0`:

**Step 1 — Download video stream only:**
```
yt-dlp -f "bestvideo[height<=1440]"
       --no-part
       --remux-video mp4
       -o "{cache}/{id}_video_temp.mp4"
       https://www.youtube.com/watch?v={id}
```

`--remux-video mp4` repackages the selected video stream into an MP4 container without re-encoding. H.264 sources are stream-copied directly. VP9 sources are remuxed from WebM to MP4 (container change, byte-for-byte identical codec data). The output file has exactly one stream (video) and no audio track.

**Step 2 — Download audio stream only:**
```
yt-dlp -f "bestaudio"
       --no-part
       -o "{cache}/{id}_audio_temp.%(ext)s"
       https://www.youtube.com/watch?v={id}
```

The `%(ext)s` placeholder accepts whatever extension yt-dlp chooses for the native audio stream (`.opus`, `.webm`, or `.m4a`). No `--extract-audio`, no `--audio-format` — we want the raw stream bytes, whatever they are.

**Step 3 — Metadata extraction:**
Unchanged. Runs against the video title via the existing provider chain (Gemini → parser fallback).

**Step 4 — Normalize audio to FLAC:**
```
# Pass 1 — loudnorm analysis.
ffmpeg -i {id}_audio_temp.<ext>
       -af "loudnorm=I=-14:TP=-1:LRA=11:print_format=json"
       -f null -

# Pass 2 — apply measured values, write FLAC.
ffmpeg -i {id}_audio_temp.<ext>
       -af "loudnorm=I=-14:TP=-1:LRA=11:measured_I=...:..."
       -c:a flac
       -compression_level 5
       -y
       {cache}/{safe_song}_{safe_artist}_{id}_normalized[_gf]_audio.flac
```

No `-c:v` in either pass — the input has no video stream, so FFmpeg has nothing to copy or encode on the video side. `compression_level 5` is FFmpeg's FLAC default (~50% of uncompressed PCM size at trivial CPU cost).

**Step 5 — Finalize:**
- Rename `{id}_video_temp.mp4` → `{safe_song}_{safe_artist}_{id}_normalized[_gf]_video.mp4`.
- Delete `{id}_audio_temp.*` (audio was written directly to its final name in step 4).
- Update the DB row: `video_file_path`, `audio_file_path`, `normalized = 1`, metadata fields.

**Step 6 — Failure cleanup:**
If any step fails, delete whichever temp and final files exist for the current video so nothing is left half-processed. The existing failure-cleanup path extends to handle both file sets.

## 8. Decoder — detailed flow

`SplitSyncedDecoder::open` takes a `CachedSong` and opens both readers:

```rust
pub fn open(song: &CachedSong) -> Result<Self, DecoderError> {
    let video = MediaFoundationVideoReader::open(&song.video_path)?;
    let audio = SymphoniaAudioReader::open(&song.audio_path)?;

    let v_dur = video.duration_ms();
    let a_dur = audio.duration_ms();
    if v_dur.abs_diff(a_dur) > 100 {
        tracing::warn!(v_dur, a_dur, "video/audio duration mismatch");
    }

    // Audio duration is authoritative — FLAC reports exact sample count
    // in the STREAMINFO block, so duration is known immediately at open()
    // time without any frame-walking.
    Ok(Self {
        video: Box::new(video),
        audio: Box::new(audio),
        duration_ms: a_dur,
    })
}
```

This fixes the `duration_ms = 0` class of bugs from the previous release: Symphonia reads FLAC's `STREAMINFO` block during open and reports total sample count and sample rate, so duration is known immediately with no fallback paths.

**Sync algorithm.** Unchanged from today in spirit. Audio is the master clock. `next_synced()` decodes one audio chunk (~20 ms of samples), advances the internal audio position, then pulls video frames whose PTS ≤ current audio position. Tolerance is 40 ms. Late video frames are marked late; early frames accumulate in a bounded pending queue.

**Seek.** `SplitSyncedDecoder::seek(ms)` forwards to both readers:

```rust
pub fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
    self.audio.seek(position_ms)?;
    self.video.seek(position_ms)?;
    Ok(())
}
```

Symphonia FLAC seek is sample-accurate. Media Foundation video seek jumps to the nearest keyframe ≤ target. Net seek precision is bounded by video keyframe interval (≤5 s for H.264), identical to today.

**Validation on open.** After opening both readers, assert:
- `audio.channels() ∈ {1, 2}`.
- `audio.sample_rate() == 48000`.
- `video.width() > 0 && video.height() > 0`.
- `abs(v_dur - a_dur) < 100 ms` (warn but don't fail; the self-healing cache scan can rebuild broken pairs on the next cycle).

On validation failure, return `DecoderError::Mismatch` and let the playback engine skip the song rather than crash.

## 9. Testing strategy

### Unit tests (Linux CI, fast)

- **`downloader/cache.rs`**: new regex parses `{song}_{artist}_{id}_normalized[_gf]_(video|audio).(mp4|flac)`; pairing logic groups by video ID; orphan detection returns unpaired files; legacy pattern `{song}_{artist}_{id}_normalized[_gf].mp4` parsed as a distinct `LegacyFile` marker; `cleanup_removed` deletes both files in a pair, skips the currently-playing pair, removes orphans.
- **`downloader/normalize.rs`**: `extract_loudnorm_stats` parses audio-only FFmpeg output (different stderr format than today's video+audio path).
- **`sp-decoder::audio::symphonia`**: open committed FLAC fixture, decode first chunk, assert sample rate = 48 kHz, channels = 2, non-zero samples; assert `duration_ms()` matches fixture metadata within 1 ms; seek to 1500 ms, next chunk timestamp within 10 ms of 1500.
- **`sp-decoder::sync::SplitSyncedDecoder`**: constructed with mock `VideoStream` and `AudioStream` on Linux. Tests cover audio-master-clock drives video pull, late video frames marked late, bounded pending-video queue, seek propagates to both readers, duration mismatch warning fires above 100 ms and stays silent within. All previously untestable on Linux; the trait-based refactor makes them cross-platform.

### Integration tests

- **`sp-decoder/tests/split_symphonia.rs`** (Linux + Windows): opens committed FLAC fixture, decodes all samples, asserts total sample count matches fixture metadata.
- **`sp-decoder/tests/split_mf_video.rs`** (Windows only): opens committed video-only MP4 fixture, reads all NV12 frames, asserts duration and frame count.
- **`sp-decoder/tests/split_synced.rs`** (Windows only): opens both fixtures as a pair, runs real `SplitSyncedDecoder`, asserts audio and video come out synchronized within 40 ms.
- **`sp-server/tests/download_flow.rs`** (Windows, real FFmpeg): runs a reduced download worker loop against a recorded `yt-dlp` fixture and real FFmpeg. Verifies two files produced, legacy file deleted, DB rows updated correctly.
- **`sp-server/tests/startup_migration.rs`**: in-memory SQLite + temp cache dir. Seeds the DB with a mix of normalized legacy `.mp4` rows and places matching legacy files on disk. Runs the startup cache scan. Asserts all legacy files deleted, all DB rows have `normalized = 0`, no `CachedSong` entries exist. This is the single most important migration test — if it is weak or missing, a real deploy could strand files.

### Test fixtures

Committed under `crates/sp-decoder/tests/fixtures/`:

- `silent_3s.flac` — 3 seconds of silence, stereo, 48 kHz, FLAC compression level 5. File size ~3 KB. Replaces the existing `silent_3s.mp4` for the audio path.
- `black_3s.mp4` — 3 seconds of 32x32 black video, H.264 `yuv420p`, no audio track. File size ~3 KB.
- `regen.sh` — shell script with exact FFmpeg commands to regenerate both fixtures for auditability.

Both fixtures are under 10 KB; committed as raw binary, no git-lfs.

### Mutation testing

Today `cargo mutants --in-diff` excludes `sp-decoder` on Linux because it doesn't compile. After this change:
- `sp-decoder::audio::symphonia` compiles and mutates on Linux.
- `sp-decoder::sync` compiles and mutates on Linux.
- `sp-decoder::video::mf` stays `cfg(windows)`, excluded from Linux mutation runs.

The PR should measurably improve the mutation coverage score.

### Playwright E2E (post-deploy, real stack)

- `e2e/post-deploy-flac.spec.ts`: after CI deploy, trigger a playlist sync via the dashboard, wait for the download worker to finish at least one song, open the cache-listing endpoint, assert both `_video.mp4` and `_audio.flac` exist for a known video ID.
- Extend the existing OBS scene-switch post-deploy test: flip to an `sp-*` scene, assert NDI audio presence and that the dashboard playback position advances.
- All new tests enforce zero browser-console errors and warnings per the project rule.

## 10. Version bump and commit plan

`VERSION` is bumped from `0.10.0-dev.1` to `0.11.0-dev.1` as the first commit on `dev`, before any code change. `scripts/sync-version.sh` propagates it; the 4 workspace crate versions in `Cargo.lock` are updated manually.

**Commit sequence on `dev` (each commit compiles, tests, and stands alone):**

1. `chore: bump version to 0.11.0-dev.1 for FLAC migration`
2. `feat(db): add migration V2 — audio_file_path column, reset normalized`
3. `feat(decoder): introduce VideoStream/AudioStream traits and SplitSyncedDecoder`
4. `feat(decoder): add SymphoniaAudioReader with committed FLAC fixture`
5. `feat(decoder): split MediaReader into MediaFoundationVideoReader (video-only)`
6. `feat(decoder): wire SplitSyncedDecoder to real readers, remove old SyncedDecoder`
7. `feat(downloader): fetch video and audio streams separately via yt-dlp`
8. `feat(downloader): normalize audio to FLAC instead of AAC`
9. `feat(server): restore legacy startup playlist sync + self-healing cache scan`
10. `test(e2e): add post-deploy FLAC verification spec`
11. `docs: update CLAUDE.md with the new split-file layout and decoder split`

## 11. Out of scope (tracked in separate issues)

- **Stem separation for karaoke (#14)** — requires this PR's split layout to land first. A background Demucs worker produces `{id}_audio_vocals.flac` and `{id}_audio_instrumental.flac` sidecars. Playback engine grows karaoke modes that mix stems with per-stem gain.
- **Dashboard review + video preview (#15)** — full UX audit of every dashboard control with real-backend verification, plus a low-FPS JPEG preview of the playing video inside each playlist card.
- **Bilingual karaoke lyrics EN+SK (#16)** — timestamped lyrics via Whisper transcription, Gemini translation to Slovak, rendered over OBS/Resolume/dashboard synced to the playback clock.
- **Pure-Rust video decoder** — replacing Media Foundation with a cross-platform Rust decoder is much larger in scope and not justified by this PR's goals. Leaving MF in place for video keeps hardware acceleration and the already-debugged codepath.
- **Per-playlist karaoke mode settings** — introduced by #14.
- **Audio-only playback mode** (legacy `AUDIO_ONLY_MODE`) — to be re-added when stem separation lands; the split layout makes it trivial (don't open the video reader).
- **Manual migration of an already-deployed 0.10.x install** — unnecessary. The migration V2 plus self-healing cache scan handles the transition automatically on first boot.

## 12. Deploy expectations

After CI merges this PR and deploys to win-resolume, the first server startup will:

1. Run migration V2 — every video marked `normalized = 0`.
2. Run self-healing cache scan — every legacy AAC `.mp4` file deleted.
3. Run startup sync — fetches current state for every active playlist.
4. Download worker wakes up and starts serial re-download/re-normalize.

Expected re-download time at single-threaded serial speed: ~30–60 minutes for a full catalog of ~100–200 videos, depending on network and FFmpeg throughput. During this window, NDI has nothing to play — the scenes are idle until songs come back online one at a time. This is the known transitional cost of the clean migration, and it is the correct price: no stranded files, no version skew, no manual steps, no user intervention.

After the transition, every song is stored as two sidecar files with exactly one lossy audio generation (YouTube's own Opus) and zero video re-encodes. Audio quality is at the theoretical maximum achievable from a YouTube source. The file layout is ready for stem separation (#14), bilingual lyrics (#16), and audio-only playback without further refactors.
