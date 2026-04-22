# v0.22.0 — Youth Worship Training Bundle: Design

**Date:** 2026-04-22
**Target release:** v0.22.0 (single PR, ships before 2026-04-23 youth worship training)

## Goal

Deliver four features in one shipping train so tomorrow's worship training has every piece working end-to-end:

1. Four specific YouTube songs are in the catalog with clean Gemini v18+ lyrics, in sp-live, playable manually.
2. A low-latency Presenter API push so band singers see the current/next line on stage displays independently of the audience wall.
3. Resolume shows BOTH the current line (existing `#sp-subs`) and the NEXT line (new `#sp-subs-next`), so audience sees what's coming.
4. The `/live` dashboard page is phone-usable: responsive layout, scrubber, and tap-a-lyrics-line to seek.

Plus two enablers the above depend on:

5. Per-song `suppress_resolume_en` flag for songs with baked-in EN lyrics (so Resolume doesn't double-print).
6. `YtManualSubsProvider` — skip Gemini when a song already has high-quality manual YouTube captions with timing.

Plus a bootstrap endpoint:

7. `POST /api/v1/videos/import` for adding a bare YouTube URL when the song isn't part of any seeded YT playlist.

## Non-goals

- Group-colour population (`currentGroup`, `nextGroup` in Presenter API): the API is happy with empty fields; we leave them null. A follow-up can add per-song or per-line group metadata when bands start asking for it.
- Auto-enrolling the 4 songs into a permanent YouTube playlist: operator pastes URLs directly via the new import endpoint, no upstream playlist change.
- Presenter host CRUD UI: single settings-key for URL + enabled toggle is enough. If a second Presenter ever appears, add a registry then.
- Edit-lyrics-from-dashboard: out of scope.
- Changing the Resolume clip-discovery algorithm: `#sp-subs-next` piggybacks on the existing `#`-token scanner.

## Context: what's already in place

- Gemini v18 pipeline works (20 clean songs), with YT manual subs + YT description both already fed as `candidate_texts` for reference.
- `SplitSyncedDecoder::seek(ms)` already exists in `crates/sp-decoder/src/split_sync.rs:128`; just no pipeline or HTTP wiring.
- `NowPlaying` WebSocket event already carries `position_ms` on a 500 ms throttle.
- Resolume integration has `crossfade_title` + `show_subs` + `hide_subs` on `#sp-title` / `#sp-subs` / `#sp-subssk` clips.
- CI branch protection on main requires `Gate + Deploy + E2E Tests (win-resolume)`.
- Custom playlist 184 (`ytlive`, NDI `SP-live`) exists; items are added via `POST /api/v1/playlists/184/items { video_id }`.

## Architecture

### Component map

```
Playback engine (existing)
  └── on line-change event:
        ├── Resolume  ← existing show_subs; extended to push next line to #sp-subs-next
        │             ← NEW: skips EN clips when video.suppress_resolume_en = 1
        └── Presenter ← NEW: PUT /api/stage with {currentText, nextText, currentSong, nextSong}

Worker (existing)
  └── alignment provider chain:
        ├── NEW: YtManualSubsProvider (if manual subs w/ timing → short-circuit, ship as yt_subs)
        └── GeminiProvider (existing fallback)

HTTP API (new endpoints)
  ├── POST /api/v1/videos/import        → yt-dlp dump-json → insert video row → queue download
  ├── POST /api/v1/playlists/{id}/seek  → forward to engine → PipelineCommand::Seek
  └── PATCH /api/v1/videos/{id}         → toggle suppress_resolume_en (extend existing)

sp-ui /live page (rewritten)
  ├── Mobile-first responsive layout (≤768 px = single column)
  ├── Import URL input  ← POSTs to videos/import
  ├── Set-list with per-row EN-suppress toggle
  ├── NowPlayingCard with scrubber + play/pause/prev/skip
  ├── Lyrics scroller with tap-to-seek
  └── Seek button calls are fed through WS ClientMsg::Seek for lowest latency
```

### New modules / files

| Path | Purpose |
|------|---------|
| `crates/sp-server/src/presenter/mod.rs` | Module entry, `PresenterClient` constructor |
| `crates/sp-server/src/presenter/client.rs` | HTTP PUT wrapper, wiremock-tested |
| `crates/sp-server/src/presenter/payload.rs` | `PresenterPayload` struct + serde |
| `crates/sp-server/src/lyrics/yt_manual_subs_provider.rs` | New `AlignmentProvider` impl |
| `crates/sp-server/src/api/videos.rs` | Houses the new `POST /import` handler (may extend an existing file) |
| `sp-ui/src/components/now_playing_card.rs` | Responsive card with scrubber + transport controls |
| `sp-ui/src/components/lyrics_scroller.rs` | Tap-to-seek lyrics list |
| `sp-ui/src/components/import_url_box.rs` | URL paste input |
| `sp-ui/src/styles/live.css` (or extension to existing CSS) | Mobile-first media queries |
| `e2e/tests/live-mobile.spec.ts` | Playwright iPhone-SE viewport test |

### Touched files

- `crates/sp-server/src/playback/pipeline.rs` — add `PipelineCommand::Seek { position_ms }`
- `crates/sp-server/src/playback/mod.rs` — `PlaybackEngine::seek()` method
- `crates/sp-server/src/api/mod.rs` + routes — wire `/seek`, `/videos/import`
- `crates/sp-server/src/resolume/mod.rs` — add `SUBS_NEXT_TOKEN`
- `crates/sp-server/src/resolume/handlers.rs` — `show_subs(current, next)`, EN-suppress branch
- `crates/sp-server/src/db/` — migration for `videos.suppress_resolume_en`, model field plumbing
- `crates/sp-server/src/lyrics/mod.rs` — `LYRICS_PIPELINE_VERSION 18 → 19`
- `crates/sp-server/src/lyrics/worker.rs` — register `YtManualSubsProvider` ahead of Gemini
- `crates/sp-server/src/lib.rs` — build and store `PresenterClient` in `AppState`
- `crates/sp-core/src/ws.rs` — `ClientMsg::Seek`
- `sp-ui/src/pages/live.rs` — mount new components, responsive shell
- `sp-ui/src/store.rs` — seek action + position signal plumbing
- `.github/workflows/ci.yml` — new post-deploy gates (Presenter reachability, Resolume next-subs populated, mobile live-page clean)
- `CLAUDE.md` — history entry for `LYRICS_PIPELINE_VERSION = 19`

## Data flow: line-change event (the hottest path)

1. `PlaybackEngine::tick()` detects line transition (`position_ms` crossed line boundary).
2. Fetches current line, lookahead line (`next` = `lines[i+1]` or empty if last).
3. Parallel fire-and-forget pushes:
   - `resolume_registry.show_subs(current_en, next_en, current_sk, next_sk, suppress_en)` — fan-out to all enabled hosts, 2 s timeout per host.
   - `presenter_client.push(PresenterPayload { current_text: current_en, next_text: next_en, current_song, next_song })` — single host, 2 s timeout.
4. WebSocket `NowPlaying { position_ms, line_idx, line_text, ... }` broadcast to dashboard clients.
5. If the song ends in this tick: both Resolume subs and Presenter get empty strings; `next_song` becomes the next item in playlist 184's set-list, or empty if end of playlist.

All three pushes run concurrently via `tokio::spawn`; playback never blocks on network I/O.

## API shapes

### POST /api/v1/videos/import

```json
// request
{"youtube_url": "https://youtu.be/AvWOCj48pGw", "playlist_id": 184}

// response 201
{"video_id": 231, "youtube_id": "AvWOCj48pGw", "title": "..."}

// errors
// 400 invalid URL or yt-dlp failed
// 409 video_id already exists on that playlist
```

Implementation runs `yt-dlp --dump-json --skip-download --no-playlist <url>`, parses title/duration, inserts a row with `normalized=0`, and nudges the download worker. The video appears in the playlist's set-list within ~10 min (download → normalize → Gemini).

### POST /api/v1/playlists/{id}/seek

```json
// request
{"position_ms": 45000}

// response 204 No Content
// errors: 404 if playlist has no active pipeline, 400 if position_ms > duration_ms
```

Engine routes to the correct `PlaybackPipeline` and sends `PipelineCommand::Seek { position_ms }`. The pipeline thread picks it up next command drain, calls `decoder.seek(position_ms)`, flushes any pending audio/video, resumes.

### Presenter PUT (outbound from SongPlayer)

```json
PUT http://10.77.9.205/api/stage
Content-Type: application/json

{
  "currentText": "Allelujah, allelujah",
  "nextText": "We lift our hands",
  "currentSong": "Alleluia",
  "nextSong": "¡Alabo!"
}
```

`currentGroup` / `nextGroup` intentionally omitted (maps to empty on the Presenter side, no color bar). A 204 indicates success; anything else logs a warning but never blocks playback.

## Error handling

- Presenter/Resolume pushes are `tokio::spawn(async move { ... })` with a 2 s `reqwest` timeout; failures are logged at `warn!` and dropped.
- `YtManualSubsProvider` returning `Err` falls back to `GeminiProvider` (same pattern as existing gather-phase sources).
- `POST /videos/import` handles the yt-dlp subprocess-failure path: 400 with the stderr message so the operator can see why (bad URL, copyright region block, yt-dlp version drift).
- `POST /seek` before the pipeline has emitted `Started`: 409 `pipeline_not_ready`.
- Migration V-N adds `suppress_resolume_en` with a default of 0; rollback means dropping the column (Rust code tolerates the column's absence by matching against `Option<bool>` in row decoding → None = false).

## Testing strategy

### Unit (Rust)
- `PresenterClient` with wiremock: success 204, timeout behaviour, JSON field names correct.
- `YtManualSubsProvider`: returns `Some` on a track with `has_timing=true`, returns `None` on a track with no timing, mutants-checked.
- `PipelineCommand::Seek` handler: mock decoder's `seek_counter` increments; pending audio cleared.
- `PlaybackEngine::seek(id, ms)`: routes to the correct pipeline; no-op if that playlist has no pipeline yet.
- `show_subs(current, next, ...)`: pushes to both `#sp-subs` and `#sp-subs-next`; skips EN when `suppress_en=true`.
- Migration V-N: column added, default 0, idempotent on rerun.
- Import handler: mock yt-dlp succeeds → row inserted; mock fails → 400 returned.

### Frontend (Rust/WASM)
- `NowPlayingCard` component builds and renders for both mobile and desktop viewport signals.
- Scrubber `on:change` dispatches a store action that calls seek.

### Playwright (e2e)
- `live-mobile.spec.ts`: viewport 375×667, open `/live`, verify mobile layout (set-list stacks vertically, buttons ≥44 px touchable), tap a lyrics line, assert a seek request fires (intercept network) and position advances via WS broadcast.
- Post-deploy win-resolume E2E:
  1. **Presenter reachability** — PUT a probe payload to the configured `presenter_url`, expect 204.
  2. **Next-subs populated** — trigger playback on `sp-live`, query Resolume composition, assert any `#sp-subs-next` clip text ≠ any `#sp-subs` clip text (proves lookahead is wired).
  3. **Live-page mobile console-clean** — navigate to `/live` at mobile viewport, zero console errors/warnings.
  4. (Existing gates continue to run: AI proxy, Gemini line-level, deploy success.)

### Mutation testing
- Standard `--in-diff pr.diff` scope on the new code. `cargo-mutants` already has the 16 GB runner OOM mitigation (swap, `CARGO_BUILD_JOBS=2`) so the small diff won't trigger the issues that hit PR #48.

## CI gates added to branch protection

Via the push CI workflow, the `E2E Tests (win-resolume)` job gains three new steps (all failing = job fails = merge blocked):

- Presenter reachability (PUT probe → 204)
- `#sp-subs-next` populated after playback
- `/live` at 375 px wide has no console errors

The mobile Playwright test also runs in the PR-CI `Frontend E2E Tests` job so it's caught pre-deploy too.

## Implementation order

Four commits on dev, each self-contained and independently deployable. Every commit ends with green CI before the next lands so the dev branch is never broken.

**Commit 1 — Lyrics baseline (phase 1):**
- Migration for `videos.suppress_resolume_en` + plumbing into `Video` model + `/videos/{id}` PATCH.
- `POST /api/v1/videos/import` + unit tests.
- `/live`: add URL-paste component + per-row EN-suppress toggle.
- `YtManualSubsProvider` + unit tests.
- Worker registers the new provider ahead of Gemini.
- `LYRICS_PIPELINE_VERSION 18 → 19` + CLAUDE.md entry.
- Operationally after deploy: import the 3 missing songs, flag all 4 manual-priority, set `suppress_resolume_en=1` on song 3. Worker drains queue (~25 min).

**Commit 2 — Presenter integration (phase 2):**
- `crates/sp-server/src/presenter/` module with wiremock tests.
- Settings entries (`presenter_url`, `presenter_enabled`) with CI seed-if-empty behavior (same pattern as `gemini_api_key`).
- Playback engine hook at line-change.
- Post-deploy E2E gate for Presenter reachability.

**Commit 3 — Resolume next-line + EN-suppress enforcement (phase 3):**
- `SUBS_NEXT_TOKEN` constant + handler change.
- `suppress_resolume_en` branch in `show_subs`.
- Post-deploy E2E gate for `#sp-subs-next` populated.

**Commit 4 — Seek plumbing + mobile /live page (phase 4):**
- `PipelineCommand::Seek`, engine method, HTTP `POST /seek`, WS `ClientMsg::Seek`.
- `/live` responsive rewrite, scrubber, tap-line-to-seek.
- Playwright mobile test (pre-deploy in PR-CI + post-deploy in E2E job).

After commit 4 green on push CI, bump VERSION to `0.22.0`, commit, open PR → main.

## Risks and mitigations

- **Gemini quota:** 4 songs serial is manageable; multi-key rotation + 429 retry already handle it. If song 1 uses `YtManualSubsProvider` it's zero Gemini calls for that one.
- **Presenter server down at 10.77.9.205 during event:** new E2E gate catches it pre-deploy. Runtime failures are non-fatal (fire-and-forget, log-only).
- **Baked-in lyrics toggle set wrong:** the flag defaults to 0; if operator flips it by mistake, it only affects Resolume EN (SK and Presenter unaffected). Low-cost wrong-state.
- **Seek mid-song discontinuity:** pipeline clears pending audio on seek (already handled in `split_sync.rs::seek_clears_pending_and_forwards_to_both` test). Visual stutter is expected and acceptable.
- **Mobile CSS regressions on desktop:** Playwright desktop test already covers dashboard; the mobile viewport is additive not replacement.

## Success criteria (manual verification tomorrow)

1. All 4 songs appear in sp-live and play to NDI.
2. On each song, Resolume's `#sp-subs` shows current line, `#sp-subs-next` shows the next line, both in sync.
3. Song 3 (baked-in): Resolume `#sp-subs` stays empty, `#sp-subssk` (SK) still populates.
4. Presenter stage at 10.77.9.205/stage shows current+next text with <1 s latency after each line change.
5. Dashboard `/live` usable on an iPhone: tap a line jumps the song there; scrubber drags smoothly.
6. CI stays green throughout; merge to main lands with `mergeable_state: clean`.
