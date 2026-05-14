# Lyrics Source Probe + Reprocess Unblock — Design

**Date:** 2026-05-13
**Branch:** dev
**Pipeline version:** stays at 20 (no bump — `feedback_no_bump_until_proven.md`)

## Problem

Three coupled defects break the `/lyrics-verify` operator loop:

1. **No pre-flight visibility into text-source availability.** The operator picks a song and triggers reprocess; the worker runs Demucs + Gemini ASR; minutes later, no_source. There is no way to ask up-front "does this song have ANY text source at all (yt_subs / lyrics.ovh / genius / lrclib / spotify_proxy / description)?" Songs with zero text sources fall back to whisperx-only alignment, which the user has classified as weak STT and does not want to spend time on. The skill needs a pre-flight check that returns per-provider availability without invoking Claude cleanup or alignment.

2. **`POST /api/v1/lyrics/reprocess` is a silent no-op on `no_source@pv-current` rows.** The endpoint sets `lyrics_manual_priority = 1` and nothing else. The worker's `fetch_bucket_manual` SQL filters out rows where `lyrics_source IN ('failed','empty','no_source','asr_gap')` UNLESS `lyrics_pipeline_version < current`. So a row at the current pipeline version with `lyrics_source='no_source'` is never picked up despite `manual_priority=1`. The user expects reprocess to retry the song; current behavior silently does nothing.

3. **`GET /api/v1/lyrics/queue` count SQL has no source-state filter** and reports `bucket0_count` / `bucket1_count` that the worker can never actually consume. Dashboard lies. With 60 `no_source@pv20` rows, queue says `bucket1=59 bucket0=1` while worker pops 0.

## Goals

- Operator can ask "is this song workable?" before burning Gemini/Demucs cycles
- Manual reprocess actually retries `no_source` / `failed` / `empty` rows (but NOT `asr_gap` — that's the operator quarantine sentinel)
- Queue counts match worker-pop eligibility

## Non-goals

- Bumping `LYRICS_PIPELINE_VERSION` (banned per `feedback_no_bump_until_proven.md`)
- Triggering Claude cleanup or alignment from the probe endpoint (probe stays cheap)
- Dequarantining `asr_gap` rows (separate concern, deferred)
- Auto-skipping no-text songs without operator review (operator runs the loop)

## Design

### Endpoint: `POST /api/v1/lyrics/probe-sources`

**Request body:**

```json
{ "video_id": 81 }
```

**Response 200:**

```json
{
  "video_id": 81,
  "youtube_id": "gBlykZGx24w",
  "song": "Jireh",
  "artist": "New Heights Worship",
  "probes": {
    "yt_subs":     { "available": false, "line_count": 0,  "note": "no manual captions" },
    "description": { "available": true,  "line_count": 42, "note": "description has 42 candidate lines" },
    "lyrics_ovh":  { "available": false, "line_count": 0,  "note": "404 no lyrics found" },
    "genius":      { "available": false, "line_count": 0,  "note": "404 not found" },
    "lrclib":      { "available": false, "line_count": 0,  "note": "TrackNotFound" },
    "spotify":     { "available": false, "line_count": 0,  "note": "no spotify_track_id on row" }
  },
  "any_text_source": true,
  "recommendation": "proceed"
}
```

**Response 404:** unknown `video_id`.

**Response 500:** unhandled errors. (Per-probe errors surface in the `note` string with `available=false`.)

### Per-probe semantics

The probe MUST NOT invoke Claude cleanup or alignment. Each provider has an existing cheap raw-fetch function used by `gather_sources_impl`; the probe reuses those:

| Provider     | Existing fn                                  | Probe behavior                                                                                                               |
|--------------|----------------------------------------------|------------------------------------------------------------------------------------------------------------------------------|
| yt_subs      | `youtube_subs::fetch_subtitles`              | Available if `Ok(Some(_))`. `line_count` from track. Manual-only — autosub is banned per `feedback_no_autosub.md`.            |
| description  | `youtube_subs::fetch_description` (new)      | yt-dlp `--get-description`. Available if non-empty after trim. `line_count` = newline-separated lines >= 1 char.              |
| lyrics_ovh   | `lyrics_ovh::fetch_lyrics`                   | Available if `Ok(Some(lines))`. `line_count` = lines.len(). Skipped if `song.is_empty() || artist.is_empty()`.                |
| genius       | `genius::fetch_lyrics`                       | Available if `Ok(Some(track))`. `line_count` = track.lines.len(). Skipped if no genius_access_token, song/artist empty.       |
| lrclib       | `lrclib::fetch_lyrics`                       | Available if `Ok(Some(track))`. `line_count` = track.lines.len(). Note tells whether timing is real or all-zero plain.        |
| spotify      | `SpotifyLyricsFetcher::fetch`                | Skipped (`available=false note=no spotify_track_id`) if row has no `spotify_track_id`. Else fetches and reports.              |

If `description` does not exist as a standalone fetch in the current codebase, the probe inlines a `yt-dlp --get-description` call (no Claude, no further processing — raw text only).

### `any_text_source` + recommendation

```rust
let any_text_source = probes.iter().any(|p| p.available);
let recommendation = if any_text_source { "proceed" } else { "skip_no_text_source" };
```

### Fix `POST /api/v1/lyrics/reprocess`

Current SQL:

```sql
UPDATE videos SET lyrics_manual_priority = 1 WHERE id IN (...)
```

New SQL (per video_ids and per playlist_id variants both updated):

```sql
UPDATE videos
SET lyrics_manual_priority = 1,
    lyrics_source = CASE
        WHEN lyrics_source IN ('failed', 'empty', 'no_source') THEN NULL
        ELSE lyrics_source
    END
WHERE id IN (...)
```

- Clears `lyrics_source` to NULL for the three auto-failure states so the worker's `fetch_bucket_manual` SQL re-includes the row via the `lyrics_source IS NULL` OR-branch.
- LEAVES `asr_gap` untouched. Per `feedback_no_bump_until_proven.md` + the quarantine design, `asr_gap` rows are operator-parked and should only retry on pipeline version bump (Iron Rule #1). Dequarantining requires an explicit separate endpoint or a version bump.
- LEAVES already-successful sources (`yt_subs`, `genius`, `description`, etc.) untouched so reprocess of a healthy row doesn't accidentally erase its source label.

### Fix `GET /api/v1/lyrics/queue` count SQL

Align the bucket0 and bucket1 count SQL with the worker pop SQL so the dashboard reports rows the worker can actually consume.

Bucket0 (manual queue) new SQL:

```sql
SELECT COUNT(*) FROM videos v JOIN playlists p ON p.id = v.playlist_id
WHERE v.lyrics_manual_priority = 1
  AND (v.lyrics_source IS NULL
       OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source', 'asr_gap')
       OR v.lyrics_pipeline_version < ?)
  AND p.is_active = 1 AND v.normalized = 1
```

Bucket1 (null bucket) new SQL:

```sql
SELECT COUNT(*) FROM videos v JOIN playlists p ON p.id = v.playlist_id
WHERE (v.has_lyrics IS NULL OR v.has_lyrics = 0)
  AND (v.lyrics_source IS NULL
       OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source', 'asr_gap')
       OR v.lyrics_pipeline_version < ?)
  AND v.lyrics_manual_priority = 0
  AND p.is_active = 1 AND v.normalized = 1
```

Bucket2 (stale) SQL is unchanged — already aligned with worker.

`fetch_queue_counts` signature gains `current_version: u32` parameter (bucket2 already had it; consistent across all three now).

## File changes

- **New:** `crates/sp-server/src/lyrics/probe.rs` (~120 LoC) — `probe_sources_impl` async fn + `ProbeReport` / `ProbeResult` types.
- **New:** `crates/sp-server/src/lyrics/probe_tests.rs` (sibling test file, ~150 LoC).
- **Modified:** `crates/sp-server/src/lyrics/mod.rs` — `pub mod probe;` + `#[cfg(test)] mod probe_tests;`.
- **Modified:** `crates/sp-server/src/lyrics/youtube_subs.rs` — add `fetch_description` helper if not already present (~30 LoC). If a helper for raw description exists elsewhere, reuse it.
- **Modified:** `crates/sp-server/src/api/lyrics.rs` (~750 LoC currently — add ~80 LoC for handler + types; stays under 1000-line cap):
  - `ProbeRequest`, `ProbeResponse`, `ProbeReport`, `ProbeResult` types
  - `pub async fn post_probe_sources` handler
  - Modify `post_reprocess` SQL (both variants — video_ids + playlist_id)
  - Modify `fetch_queue_counts` SQL (bucket0 + bucket1 + current_version param threading)
  - New tests: probe handler integration tests, reprocess SQL behavior tests (clears auto-failure, leaves asr_gap), queue counts alignment tests
- **Modified:** `crates/sp-server/src/api/mod.rs` — wire `/api/v1/lyrics/probe-sources` route (~4 LoC).
- **Skill:** `~/.claude/skills/lyrics-verify/SKILL.md` — Phase 0.5 added between Phase 1 and Phase 2 + Iron Rule #21.
- **Memory:** `~/.claude/projects/-home-newlevel-devel-songplayer/memory/feedback_preflight_text_source.md` — new feedback memory + MEMORY.md index entry.

Estimated total: ~400 LoC source + ~250 LoC tests. Bundled because all three changes serve the same operator loop and cannot ship independently without leaving the loop broken.

## TDD strategy

Each commit is RED-first per `tdd-workflow.md`:

1. Test asserts new behavior, run, confirm FAIL
2. Implement, run, confirm PASS
3. Commit

Tests use:
- `db::create_memory_pool` + `run_migrations` for SQL fixtures
- `axum::body::Body` + `tower::ServiceExt` for endpoint integration
- `wiremock` (or existing patterns) for HTTP-mocked provider probes

## Risks

- **Description fetch via yt-dlp can be slow on cold cache** (5-10s). Mitigate by caching the raw description in `{youtube_id}_description_raw.txt` so repeat probes are instant. (If a cache already exists in `description_provider`, reuse it.)
- **Concurrent probe calls** for the same `video_id` could double-fetch yt-dlp. Acceptable — probes are operator-driven, not hot-path.
- **Clearing `lyrics_source` to NULL** on reprocess changes audit-trail semantics. Mitigated by emitting a `tracing::info!` log line on the clear so the prior label is recorded.

## Rollout

- Single PR `dev` → `main`. After CI green + deploy, probe-sources is callable and reprocess actually works.
- After deploy: operator re-triggers `/lyrics-verify` on null-bucket songs. Probe filters out no-text songs; workable ones get reprocessed; cycle continues.
- Pre-existing `manual_priority=1` rows like `id=81` Jireh: after deploy, the new reprocess SQL will not retroactively clear their `lyrics_source` (only future reprocess calls clear). Operator must re-POST reprocess on those rows once, or just let the probe tell them to skip Jireh entirely.

## Out of scope

- Dequarantine endpoint for `asr_gap` rows (separate concern).
- Adding new text-source providers (e.g. Musixmatch, AZLyrics).
- Auto-running probes for all null-bucket songs (skill responsibility; operator triggers per-song).
