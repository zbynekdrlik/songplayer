<!-- Global rules inherited from ~/.claude/CLAUDE.md (managed by airuleset) -->
# CLAUDE.md

This file provides guidance to Claude Code when working with code in this repository.

## Playbook router

Path-scoped rules in `.claude/rules/` auto-load on their `paths:`; skills in
`.claude/skills/` load on demand.

- rust workspace (1000-line cap, cargo-fmt reorder trap) → `.claude/rules/rust-workspace.md` (auto-loads on `crates/**/*.rs`)
- lyrics-eval backends → `.claude/rules/lyrics-eval-backends.md` (auto-loads on `eval/lyrics/**`)
- sp-ui / e2e mock gotchas → `.claude/rules/sp-ui-frontend.md` (auto-loads on `sp-ui/**`, `e2e/mock-api.mjs`)
- pipeline.rs testability → `.claude/rules/pipeline-testability.md` (auto-loads on `playback/pipeline*.rs`, `submitter.rs`)
- YouTube cookie file / bot-check → `.claude/rules/youtube-cookies.md` (auto-loads on `downloader/**`, `playlist/**`)
- yt-dlp spawn env (UTF-8 titles + hide console) → `.claude/rules/yt-dlp-spawn-env.md` (auto-loads on `downloader/**`, `playlist/**`)
- genlock / NDI timecodes / dantesync → `.claude/rules/genlock.md` (auto-loads on `sp-core genlock*`, `playback/{wallclock,clock_health,pacer,submitter}*`, `sp-ndi/**`)
- live video preview (#15 part 2, never touch NDI submit path) → `.claude/rules/preview.md` (auto-loads on `playback/{preview,pipeline,pipeline_paced}*`, `sp-ui/.../playlist_card.rs`)
- karaoke stem separation → `.claude/rules/karaoke-stems.md` (auto-loads on `stems/**`, `audio/karaoke*.rs`, `scripts/stem_worker.py`)
- OBS↔NDI health / dark-wall receiver recovery → `.claude/rules/obs-ndi-health.md` (auto-loads on `obs/**`, `playback/ndi_health.rs`, `e2e/post-deploy*`)
- obs-mcp gateway watchdog → `.claude/rules/obs-mcp-gateway.md` (auto-loads on `scripts/obs-mcp/**`)
- CLIProxyAPI / DEFAULT_AI_MODEL → `.claude/rules/ai-proxy.md` (auto-loads on `crates/sp-server/src/ai/**`, `config.rs`)
- crash diagnostics / panic hook → `.claude/rules/crash-diagnostics.md` (auto-loads on `panic_hook.rs`, `src-tauri/src/lib.rs`, `build.rs`)
- LAN sp.local mDNS advertisement → `.claude/rules/lan-mdns.md` (auto-loads on `crates/sp-server/src/mdns.rs`)
- CI workflows: runner shell traps / mutation gate / push+PR de-dup → `.claude/rules/ci-workflows.md` (auto-loads on `.github/workflows/**`, `.cargo/mutants.toml`)

| Area | Skill | Load when |
|------|-------|-----------|
| Lyrics pipeline | `lyrics-pipeline` | lyrics processing, alignment providers, pipeline versioning, translation, Gemini/CLIProxy, reprocess |
| Wall verification | `lyrics-verify` | /lyrics-verify, wall-verify loop, catalog songs, sp-live setlist, quarantine |
| Lyrics eval | `lyrics-eval` | evaluating ASR/alignment backends, /lyrics-eval command, eval harness |
| win-resolume ops | `win-resolume-ops` | deployments, CI monitoring, Resolume diagnostics, OBS, runner health |
| CI quality | `ci-discipline` | writing CI jobs, reviewing PRs, test design, quality gates |

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
- Adding or removing a route / alignment stage from the worker (the #159
  one-regime cut that dropped WhisperX + asr_path was a 21→22 bump)
- Changing the mtl align invocation, the `reference_gate` thresholds, or the
  g35t base-tier grouping (gap/coalesce/sanitize) in a way that alters output
- Changing the reference-text-selection algorithm (`best_authoritative_candidate`)

**Do NOT bump for:**
- Bug fixes that produce identical output
- Refactoring, renaming, logging changes
- UI/dashboard-only changes
- Performance optimizations with identical output

**History (condensed — regimes below v22 are DELETED, kept as a one-line trail):**
- v1–v10: qwen3/autosub ensemble + Claude/Rust merge + word-timing sanitizers
  (blinking-karaoke fixes). Ensemble deleted.
- v11–v18: Gemini chunked forced-alignment era — multi-key rotation,
  CLIProxy↔direct-API flip-flops, the `lines: []` data-loss fix (v15), AutoSub
  unregistered (v16), line-level-only timing (v18, `words: None`, no synthesized
  word timings — still enforced). Gemini chunked regime deleted.
- v19: manual yt_subs short-circuit. v20: Genius text source +
  `lyrics_override_text`; aligner became WhisperX-on-Replicate with an
  AssemblyAI-U3-Pro `asr_path` fallback for no-text songs.
- v21 (#143): Lever-2 reference stage added — `mtl_aligner` force-align verified
  by a Gemini-3.5-Transcribe transcript (`reference_gate`), ship ★ on PASS, else
  fall through to the v20 routes.
- v22 (#159): **one regime.** The v20 WhisperX-on-Replicate route and the
  AssemblyAI `asr_path` route are DELETED (owner directive 2026-09-14, not "keep
  as fallback"). Two tiers now — (1) ★ tier: text + mtl force-align + g35t gate
  → mtl line timings (unchanged from v21); (2) base tier: everything else (no
  usable text, gate fail, mtl skip/error) → a Gemini-3.5-Transcribe transcript
  grouped into lines (`g35t_transcript`, source `gemini-3-5-transcribe`). One
  forced aligner (mtl), one ASR vendor (Gemini). Measured no-text quality g35t
  19.7% gold-norm ≤400ms vs the retired AssemblyAI 3.8%
  (`eval/lyrics/reports/2026-09-12-gemini-3-5-transcribe.md`). Every pre-v22 row
  re-queues (no smart-skip — that was the v18 trap); v21 mtl rows re-run to
  identical output and re-★. Full route map: the `lyrics-pipeline` skill.

## Disabled subsystems (do not re-enable without redesign)

### NDI auto-recovery (per-sender RecreateSender) — disabled v0.26.0

PR #58 shipped a Tier-2 auto-recovery that, on prolonged `connections=0`, sent a `PipelineCommand::RecreateSender` to the affected pipeline thread, which destroyed the old NDI sender and created a new one with the same name. **It cannot work and was ripped out in v0.26.0.** Two structural reasons:

1. **NDI runtime mDNS binding is process-global.** The 2026-04-27 production failure cause was the NDI SDK binding its mDNS announce socket to a stale APIPA address (`169.254.144.214` — no longer present on any current adapter). Per-sender create/destroy keeps the same global socket; the new sender is also dark.
2. **Real NDI rejects two senders with the same name in one process.** Our recreate created the new sender BEFORE the old one's destroy ran (Drop is deferred to end of arm), so `send_create` returned null on the same-name conflict and the loop spammed `RecreateSender mid-decode: failed; keeping existing` every 30 s. `MockNdiBackend` did not enforce same-name uniqueness so unit tests never caught it.

Recovery from that state requires either:
- Process restart (works today; operator-only — there is no in-app /api/v1/admin/restart endpoint)
- Full NDI runtime re-init: `NDIlib_destroy()` + `NDIlib_initialize()` + recreate every sender + reconnect every receiver subscription. Coordinated, multi-pipeline, and risky enough that it needs its own design and integration test against a real Windows NDI runtime — tracked in #60.

If you find yourself adding `PipelineCommand::RecreateSender` back: stop. Check #60 first. Any per-sender recreate is structurally unable to fix the root cause.

The `degraded_reason` field on `PipelineHealthSnapshot` is preserved as Tier-1 visibility: when `connections=0` for ≥2 consecutive 5-second polls while Playing, the dashboard / log gets `"no NDI receiver — wall is dark"`. That's the entire current response.

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

## CI architecture

The CI pipeline (`.github/workflows/ci.yml`) gates every push to `dev`
and `main`, plus every PR to `main`. Steady-state runtime on cache hit
is ~17 minutes for a dev push (after the 2026-04-25 lyrics-quality
removal).

**Critical path (a dev push):**

```
parallel ubuntu checks (≤56s)
  → Build WASM (~1:40)
    → Build Tauri (~9:50)             ← release compile of sp-server
      → Gate (2s)
        → Deploy win-resolume (~2:15)
          → E2E win-resolume (~3:00)
TOTAL ≈ 17 min
```

`Build Tauri` dominates because `src-tauri` is excluded from the
workspace and uses cargo's default release profile (codegen-units=16,
no LTO) — already maximally parallel for the Windows runner. Cache
(`Swatinem/rust-cache@v2`) is in place and hits reliably; the time is
genuine release optimization of sp-server's dependency graph.

**Hard rules:**

- **No sleep-based CI jobs.** Any job whose runtime is dominated by
  `sleep` / `Start-Sleep` / `time.sleep` is forbidden. If a soak window
  is needed for trend analysis, it goes into a scheduled workflow
  (cron), not the post-deploy critical path. Cron-scheduled jobs do
  not gate dev pushes.

- **Self-reported metrics are not quality gates.** A pipeline reporting
  its own `confidence` is not a quality signal. Real quality gates
  compare against ground truth (hand-verified fixtures, known-correct
  reference data, or human-perceptible behaviour exercised end-to-end
  via Playwright on the deployed target).

- **`measure_lyrics_quality.py`** stays installed on win-resolume at
  `C:\ProgramData\SongPlayer\cache\tools\measure_lyrics_quality.py` as
  an ad-hoc trend tool. It is no longer wired into CI.

- **E2E must not switch to disruptive OBS scenes.** The post-deploy
  Playwright suite shares win-resolume's OBS with the LED wall and live
  audio. The "off-program baseline" picker (`pickBaselineScene` in
  `e2e/post-deploy.spec.ts`) prefers `sp-slow` and falls back to any
  sp-* scene that isn't `sp-fast` (under test) or `sp-warmup` (sync
  tone). Non-sp scenes (such as a QR-code/sync-tone test scene) are a
  last resort only. The suite captures the program scene at start
  (`beforeAll`) and restores it at end (`afterAll`); never leave the
  wall on whatever scene the last test happened to switch to.
