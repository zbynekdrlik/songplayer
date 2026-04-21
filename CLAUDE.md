<!-- Global rules inherited from ~/.claude/CLAUDE.md (managed by airuleset) -->
# CLAUDE.md

This file provides guidance to Claude Code when working with code in this repository.

## Project Overview

SongPlayer is a standalone Windows desktop application that plays YouTube playlists with loudness normalization and NDI output. Built with Rust using Tauri 2 (shell), Leptos 0.7 (WASM UI), and Axum 0.8 (embedded HTTP/WebSocket server). Videos are downloaded via yt-dlp, normalized to -14 LUFS with FFmpeg, and can be output via NDI.

## Workspace Structure

The Cargo workspace root manages 4 crates. Two additional crates are excluded from the workspace because they have different build toolchains.

```
songplayer/
├── Cargo.toml              # Workspace root (members: sp-core, sp-ndi, sp-decoder, sp-server)
├── VERSION                 # Single source of truth for version (e.g. 0.1.0-dev.1)
├── scripts/
│   └── sync-version.sh    # Reads VERSION, updates all Cargo.toml + tauri.conf.json
├── crates/
│   ├── sp-core/          # Shared types, database (SQLite/sqlx), domain logic — WASM-safe
│   ├── sp-ndi/           # NDI output via libloading (runtime-linked, no compile-time dep)
│   ├── sp-decoder/       # Windows Media Foundation decoder (cfg(windows) only)
│   └── sp-server/        # Axum HTTP + WebSocket server, yt-dlp/FFmpeg orchestration
├── sp-ui/                # Leptos 0.7 WASM frontend (excluded from workspace, built with Trunk)
└── src-tauri/             # Tauri 2 shell (excluded from workspace, built with cargo tauri)
```

### Crate Descriptions

| Crate | Purpose |
|-------|---------|
| `sp-core` | Shared types, SQLite database via sqlx, domain models. Must be WASM-safe (no tokio, no std-only I/O). |
| `sp-ndi` | NDI SDK integration via `libloading`. Loads the NDI shared library at runtime to avoid compile-time dependency. |
| `sp-decoder` | Windows Media Foundation video decoder. Entire crate is `cfg(windows)` — will not compile on Linux. |
| `sp-server` | Axum 0.8 server with HTTP REST + WebSocket. Runs yt-dlp and FFmpeg as subprocesses. Main async binary. |
| `sp-ui` | Leptos 0.7 CSR frontend compiled to WASM via Trunk. Communicates with sp-server via HTTP/WebSocket. |
| `src-tauri` | Tauri 2 application shell. Embeds `dist/` from sp-ui build and spawns sp-server in background. |

## Dev Commands

**Check the workspace (fast, no output):**
```bash
cargo check
```

**Run tests:**
```bash
cargo test
```

**Format code:**
```bash
cargo fmt --all
```

**Check formatting (for CI):**
```bash
cargo fmt --all --check
```

**Lint:**
```bash
cargo clippy -- -D warnings
```

**Build the WASM frontend (requires trunk):**
```bash
cd sp-ui && trunk build
```

**Build the full Tauri app (requires trunk output in dist/):**
```bash
cd src-tauri && cargo tauri build
```

**Sync version from VERSION file:**
```bash
./scripts/sync-version.sh
```

## Branch Strategy

Two branches: `dev` + `main`. After merge: recreate `dev` with next `-dev.N` version.

## Version Management

`VERSION` is the single source of truth. Run `./scripts/sync-version.sh` after changing it.

| Branch | VERSION format | Example |
|--------|---------------|---------|
| `dev`  | `X.Y.Z-dev.N` | `0.1.0-dev.1` |
| `main` | `X.Y.Z`       | `0.1.0` |

**Workflow:**
1. Start work on `dev` with VERSION like `0.1.0-dev.1`
2. Run `./scripts/sync-version.sh` to propagate to all Cargo.toml files
3. Before PR merge: change VERSION to `0.1.0`, run sync-version.sh
4. After merge: recreate dev with `0.2.0-dev.1`

Note: The 4 workspace crates use `version.workspace = true` — only the root `Cargo.toml`, `src-tauri/Cargo.toml`, `sp-ui/Cargo.toml`, and `src-tauri/tauri.conf.json` need updating.

## Database

SQLite via sqlx with manual migrations. Migration logic lives in `crates/sp-server/src/db/mod.rs`. No external migration files — schema is applied programmatically at startup.

- Database file: configurable path, defaults to `songplayer.db` in app data dir
- Connection type: `SqlitePool` with `sqlx::sqlite`
- No compile-time query checking (`query!` macro requires DATABASE_URL) — use `query_as` with runtime checking

## Key Patterns

**sp-core must be WASM-safe:**
- No `tokio` in sp-core (use `futures` traits only)
- No `std::fs` or OS-specific I/O
- No `cfg(windows)` — platform code belongs in sp-decoder or sp-server

**sp-decoder is Windows-only:**
- All code wrapped in `#[cfg(target_os = "windows")]` or `#[cfg(windows)]`
- Uses the `windows` crate for WMF (Windows Media Foundation) APIs
- Not compiled on Linux/macOS CI — use feature flags if needed for cross-platform CI

**NDI via libloading (runtime linking):**
- NDI SDK not required at compile time
- `sp-ndi` loads `Processing.NDI.Lib.x64.dll` at runtime via `libloading`
- Gracefully degrades if NDI is not installed

**NDI network name format (scene detection):**
NDI sources on the network are advertised as `"MACHINE (stream)"` — the machine hostname that owns the sender, a space, then the stream name in parentheses. When OBS adds an NDI source, its `ndi_source_name` input setting stores this full string (e.g. `"RESOLUME-SNV (SP-fast)"`). SongPlayer's playlist `ndi_output_name` is just the bare stream part (`"SP-fast"`), so `crates/sp-server/src/obs/ndi_discovery.rs::extract_ndi_stream_name` strips the `MACHINE ` prefix before matching. Anyone touching the scene-detection path must preserve this split — otherwise the map built in `rebuild_ndi_source_map` will never match real OBS inputs.

**Split-file audio layout (FLAC pipeline):**
Each cached song is stored as two sidecar files sharing a common base name:

- `{safe_song}_{safe_artist}_{video_id}_normalized[_gf]_video.mp4` — H.264/VP9/AV1 stream-copied from YouTube, zero re-encodes.
- `{safe_song}_{safe_artist}_{video_id}_normalized[_gf]_audio.flac` — decoded from YouTube's Opus stream, 2-pass FFmpeg loudnorm at -14 LUFS, re-encoded to FLAC exactly once. Signal is lossless from this point to NDI.

The decoder split follows the file layout: `sp_decoder::MediaFoundationVideoReader` (Windows-only, hardware-accelerated MF) reads the video sidecar, and `sp_decoder::SymphoniaAudioReader` (pure Rust, cross-platform) reads the FLAC sidecar. `SplitSyncedDecoder` drives both with audio-as-master-clock at 40 ms tolerance. The `VideoStream` / `AudioStream` / `MediaStream` traits in `sp_decoder::stream` let unit tests drive the sync algorithm with mock readers on Linux.

On first boot of a new version, `sp_server::startup::self_heal_cache` walks the cache directory: any legacy single-file `.mp4` from before the FLAC migration is deleted, any orphan half-sidecars (video without audio or vice versa) are deleted, and every complete video+audio pair is re-linked to its DB row. Migration V4 resets `normalized = 0` for every existing row so the download worker re-processes everything under the new layout.

A one-shot startup sync (`sp_server::startup::startup_sync_active_playlists`, matching legacy Python `tools.py::trigger_startup_sync`) runs for every `is_active = 1` playlist once tools are ready — this was missing from the initial Rust port and is restored alongside the FLAC migration.

**Circular import avoidance:**
Use local imports inside functions when needed to break cycles:
```rust
fn some_fn() {
    use crate::other_module::Thing;
    // ...
}
```

**Windows subprocess (hide console window):**
All `std::process::Command` calls for yt-dlp/FFmpeg must use `CREATE_NO_WINDOW`:
```rust
use std::os::windows::process::CommandExt;
command.creation_flags(0x08000000); // CREATE_NO_WINDOW
```

**Server orchestration (`sp-server/src/lib.rs`):**
The `start()` function wires all subsystems: DB, tools manager, playlist sync handler, download worker, OBS WebSocket client, playback engine, Resolume workers, reprocess worker, and Axum HTTP server. All workers receive a shutdown broadcast for graceful termination.

**API routes are under `/api/v1/`** (not `/api/`). The WASM dashboard uses relative URLs.

**Deployment target:** Windows machine `win-resolume` (10.77.9.201) running OBS Studio with NDI plugin. Installed via NSIS installer from CI artifacts. Data directory: `C:\ProgramData\SongPlayer\`.

**Follow existing patterns** from similar projects (restreamer, iem-mixer) for consistency in error handling, logging (tracing), and state management.

## Pipeline versioning (lyrics)

`crates/sp-server/src/lyrics/mod.rs::LYRICS_PIPELINE_VERSION` is a monotonic integer identifying the lyrics processing output format. Every song's lyrics JSON + DB row records the version it was produced under. On worker startup, songs with `lyrics_pipeline_version < LYRICS_PIPELINE_VERSION` are re-queued for reprocessing (stale bucket, worst-quality-first).

**Bump the constant when:**
- Adding or removing an `AlignmentProvider` from the worker registration
- Changing a provider's algorithm (chunking, matcher, density gate thresholds)
- Changing either Claude merge prompt (text reconciliation or timing merge)
- Changing the reference-text-selection algorithm
- Switching alignment-provider registration (e.g. toggling `LYRICS_GEMINI_ENABLED` or `LYRICS_QWEN3_ENABLED` in `mod.rs`)

**Do NOT bump for:**
- Bug fixes that produce identical output
- Refactoring, renaming, logging changes
- UI/dashboard-only changes
- Performance optimizations with identical output

**History:**
- v1 (pre-#33): single-path yt_subs→Qwen3 or lrclib-line-level
- v2 (#34/#35): ensemble orchestrator + AutoSubProvider + Claude text-merge
- v3 (#34/#35): merge prompt reworked — confidence-weighted, disagreement rule, compact output schema
- v4 (#42): description provider added as 4th text candidate (raw YouTube description → Claude extraction → candidate_texts)
- v5 (#42): description prompt reframed to software-engineering task (empty system, karaoke-app framing) — v4's direct-instruction prompt yielded 0% extraction on production because Claude via CLIProxyAPI OAuth returned conversational preamble instead of JSON
- v6 (#42): merge-layer fallback when Claude miscounts per-word timings (typically off by 1-6 on contractions/possessives) — returns the highest-base-confidence provider's per-word timings tagged `ensemble:fallback_to_<provider>` instead of dropping the song. Fixes ~40% production song-loss observed post-v5 deploy.
- v7 (#42): merge layer rewritten as pure Rust, Claude call dropped entirely. LLMs cannot reliably emit exact-length arrays (the v5/v6 root cause); the merge rules (base_confidence^2 weighting, disagreement handling, outlier rejection) are all deterministic math. Highest-base-confidence provider is primary; other providers' timestamps within 500ms boost confidence to min(1.0, base * 1.2); otherwise pass-through at base * 0.7.
- v8 (post-event): sanitize word timings in the merge layer — enforce monotonic start_ms, minimum 80ms per-word duration, no overlap with next word. Fixes blinking / stuck / out-of-sync karaoke observed during 2026-04-19 event. Primary provider (qwen3) sometimes emits zero-duration words, backward-in-time starts, and duplicate-start clusters; the sanitizer clamps these into well-formed timings before output.
- v9 (post-event fixup): extend the sanitizer to the single-provider pass-through in the orchestrator. v8 only sanitized the multi-provider merge path, so `ensemble:qwen3` songs (autosub dropped) still shipped raw duplicate-start / zero-duration words. v9 calls `sanitize_word_timings` on both paths; measured post-v9, `duplicate_start_pct` converges to 0% across the whole catalog.
- v10 (post-event fixup 2): thread `floor_start_ms` across line boundaries when sanitizing. v9 sanitized per-line but reset the start floor to 0 for each line, so two consecutive lines could share a word start_ms at their boundary. Since `compute_duplicate_start_pct` sorts word starts globally then counts ties, v9 audit logs reported 91% duplicates even though each line's output was individually clean. v10 makes cross-line boundaries strictly increasing.
- v11 (#TBD): Gemini 3 Pro chunked transcription replaces qwen3 forced alignment
  for line-level timing. Demucs-dereverbed vocal WAV is sliced into 60 s chunks
  with 10 s overlap, each chunk transcribed independently via Google's
  generativelanguage.googleapis.com API using the existing gemini_api_key.
  `thinkingConfig.thinkingBudget = 2048` limits Gemini's reasoning budget to
  avoid hallucinated-duplicate loops + timeouts observed on dense chorus audio.
  Overlapping regions deduplicated by normalized-text match + 1.5 s start-time
  agreement. Word-level timings deferred; qwen3 parked behind
  `LYRICS_QWEN3_ENABLED=false`. Addresses the song-230 collapse from v10 where
  untimed reference text caused qwen3 to cram an 11-min song into 10 s.
- v12 (#TBD): Gemini provider gains HTTP 429/500/503 retry with exponential
  backoff (base 10 s, cap 60 s, max 4 attempts, honors Retry-After header)
  plus 1 s inter-chunk pacing. v11 silently dropped chunks on first 429
  (Google's bulk-reprocess quota), causing ~18 songs to end up with
  `source="ensemble:autosub"` instead of `ensemble:gemini` and ~95 with
  `no_source`. v12 re-queues the entire catalog for a clean Gemini pass.
- v13 (#TBD): Gemini alignment routes through the local CLIProxyAPI
  (`http://127.0.0.1:18787`) instead of the direct Gemini API. CLIProxy
  carries an AI-Pro OAuth login so quota is the paid subscription tier
  instead of the €10 API cap that capped v12 mid-catalog. Output format
  is byte-identical to v12, so the stale bucket in
  `reprocess.rs::fetch_bucket_stale` skips any row where
  `lyrics_source LIKE '%gemini%' AND lyrics_pipeline_version >= 12` —
  songs Gemini already produced correctly under v12 stay untouched, only
  autosub-fallback and `no_source` failures from v12 are retried under
  v13. Override: `GEMINI_PROXY_URL` env var.
- v14 (#TBD): Reverts alignment transport from CLIProxyAPI OAuth back to
  the direct `generativelanguage.googleapis.com` API — the OAuth path
  turned out globally capped by Google (`MODEL_CAPACITY_EXHAUSTED` on
  `cloudcode-pa.googleapis.com` for 3.x Pro preview models; public issue
  in google-gemini/gemini-cli #24004 and #24159). Adds multi-key Gemini
  rotation: `gemini_api_key` is now a comma-separated list of direct-API
  keys, and `transcribe_rotating` in `gemini_provider.rs` advances to
  the next key on HTTP 429 — starts at a sticky index so subsequent
  chunks skip already-exhausted keys. Moves EN→SK translation from
  Gemini to Claude (CLIProxyAPI) with a short neutral prompt
  (`translator.rs::build_prompt`).
- v15 (#TBD): **Critical data-loss fix for Gemini output.**
  `merge.rs::sanitize_track` had a `continue` branch that silently
  dropped every `LineTiming` with `words: []`, and `GeminiProvider`
  produces wordless lines (line-level only — word-level is deferred).
  Every v11-v14 Gemini-only song shipped with `lines: []` despite
  being marked `has_lyrics=1` in the DB. 17 of 31 Gemini rows on
  win-resolume were empty JSONs at deploy time; the other 14 only had
  content because autosub contributed words via the multi-provider
  merge path. v15 emits wordless lines with their line-level timing
  (floor-clamped for the strict-increasing invariant). The smart-skip
  clause in `reprocess.rs::fetch_bucket_stale` is tightened from
  `version >= 12` to `version >= 15` so every pre-v15 Gemini row is
  retried regardless of line count. Regression tests assert wordless
  lines now pass through (`sanitize_track_emits_wordless_lines_with_
  line_level_timing`, `sanitize_track_all_wordless_lines_all_emitted`,
  `sanitize_track_wordless_line_clamps_start_to_floor`). Root cause:
  sanitize_track was written for qwen3 (word-level) and its wordless-
  skip guard was never updated when Gemini (line-level) was added in
  v11. Verification gap: no integration test compared the final
  `_lyrics.json` on disk against Python-prototype output for the same
  song — a single file diff would have caught the empty-lines bug in
  v11.

## Legacy OBS YouTube Player (obsytplayer)

SongPlayer is the Rust replacement for the legacy Python OBS YouTube Player at `/home/newlevel/devel/obsytplayer/`. **Always reference the legacy code when implementing features** — it contains battle-tested logic for:

- **Metadata extraction:** `yt-player-main/metadata.py` + `yt-player-main/gemini_metadata.py` — title parsing regexes, Gemini prompt, featuring cleanup
- **Download/normalize pipeline:** `yt-player-main/playlist_manager.py` — yt-dlp format selection, FFmpeg 2-pass loudnorm, file naming conventions
- **Playback engine:** `yt-player-main/player.py` — scene detection, play/pause/skip logic, title display timing (show 1.5s after start, hide 3.5s before end)
- **Playlist sync:** `yt-player-main/playlist_manager.py` — YouTube playlist flat-download, video dedup, unplayed tracking
- **OBS integration:** `yt-player-main/obs_controller.py` — text source updates, media source path changes
- **Resolume title delivery:** `yt-player-main/resolume_controller.py` — A/B lane crossfade, clip mapping via #token tags
- **Configuration:** Each instance had its own `config.json` with playlist URL, OBS source names, Gemini API key

**Key design decisions from legacy code to preserve:**
- Loudness normalization target: -14 LUFS (FFmpeg loudnorm filter)
- yt-dlp format: `bestvideo[height<=1440]+bestaudio/best[height<=1440]`
- Title display: show artist + song 1.5s after video starts, hide 3.5s before end
- Gemini prompt asks for `{song, artist, source}` JSON; falls back to regex parser
- File naming: `{song}_{artist}_{youtube_id}_normalized.mp4` (with `_gf` suffix if Gemini failed)

**6 playlist instances being migrated:**
| Name | YouTube Playlist | OBS Scene | NDI Output |
|------|-----------------|-----------|------------|
| ytwarmup | PLFdHTR758BvcHRX3nVKMEPHuBdU75dBVE | ytwarmup | SP-warmup |
| ytpresence | PLFdHTR758BveAZ9YDY4ALy9iGxQVrkGRl | ytpresence | SP-presence |
| ytslow | PLFdHTR758Bvd9c7dKV-ZZFQ1jg30ahHFq | ytslow | SP-slow |
| yt90s | PLFdHTR758BvfM0XYF6Q2nEDnW0CqHXI17 | yt90s | SP-90s |
| ytworship | PLFdHTR758BveEaqE5BWIQI7ukkijjdbbG | ytworship | SP-worship |
| ytfast | PLFdHTR758BvdEXF1tZ_3g8glRuev6EC6U | ytfast | SP-fast |

**Coexistence strategy:** Create NEW `sp-*` scenes in OBS with NDI sources from SongPlayer. Do NOT modify existing `yt*` scenes — legacy scripts remain active until SongPlayer is verified working identically.
