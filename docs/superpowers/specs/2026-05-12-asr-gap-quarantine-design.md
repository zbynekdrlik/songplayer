# ASR-Gap Quarantine — Design

**Status:** Draft (2026-05-12)
**Branch:** dev
**Pipeline version:** stays at current `LYRICS_PIPELINE_VERSION` (no bump per `feedback_pipeline_version_approval.md` and `feedback_no_bump_until_proven.md`)

---

## Problem

Songs whose ASR (today WhisperX via Replicate; future Gemini-3.1-Pro / Whisper-large-v3 / multi-model merge) produces unrecoverable transcription waste investigation time. Two recent cases (Saints id=233, Praise God earlier) burned hours grinding `text_reference_merge` heuristics that cannot recover from ASR-missed regions.

User stated requirement (verbatim):

> we need find approach to not solve now any whisperx issues songs to not waste time, there is other possibilites how to solve it, eg by better model, multiple models where problematic part wil be merged from different one, use gemini 3.1 pro etc but as i many times asked you it is important imediatelly when you find that issue is with whisperx results that song needs be marked as whisperx problem and needs be postponed.

## Goal

Provide a one-call mechanism to mark a song as ASR-broken so:

1. The lyrics worker stops picking it up under current pipeline.
2. The wall stops showing the broken karaoke output that was previously shipped.
3. A future ASR upgrade (model swap, multi-model merge, Gemini-3.1-Pro) retries it automatically without any per-song bookkeeping.

## Non-goals

- Auto-detection of ASR gaps inside the worker. Manual flag only, per user Q1 answer.
- Dashboard UI button. API endpoint only.
- Reverse endpoint (un-quarantine). SQL one-liner is sufficient today.
- Separate `lyrics_asr_version` column. The existing `lyrics_pipeline_version < ?` exception in the bucket SQL already covers future-retry semantics.
- Bumping `LYRICS_PIPELINE_VERSION`. Stays as-is.

## Design

### Surface

New endpoint: `POST /api/v1/lyrics/quarantine`

Request body:
```json
{ "video_id": 233, "reason": "ASR missed bridge 1:54-2:10" }
```

`reason` is optional. Empty or omitted reason is allowed (logged as empty string).

Response (200):
```json
{
  "video_id": 233,
  "youtube_id": "BW_vUblj_RA",
  "previous_source": "ensemble:gemini",
  "deleted_cache_file": true
}
```

`previous_source` is JSON `null` when the row had `lyrics_source IS NULL` before the call.

Response (404) when `video_id` does not exist.

`reason` is free-text, persisted only in the `tracing::warn!` audit log; it does not enter the DB. Keeping it out of the schema avoids a column migration and matches existing diagnostic conventions in the codebase.

### Effect (single DB transaction + filesystem cleanup)

1. `UPDATE videos SET has_lyrics = 0, lyrics_source = 'asr_gap', lyrics_pipeline_version = <current LYRICS_PIPELINE_VERSION>, lyrics_manual_priority = 0 WHERE id = ?`
2. Delete `cache_dir/{youtube_id}_lyrics.json` if present. Wall stops rendering broken karaoke immediately (playback reads this file directly at `playback/lyrics_loader.rs:72` and `api/lyrics.rs:198`).
3. `tracing::warn!(video_id, youtube_id, reason, previous_source, "lyrics quarantined as asr_gap")` — single structured log line for audit.

The cache delete is best-effort: if the file is missing or unreadable, the endpoint still returns 200 with `deleted_cache_file: false`. The DB update is the authoritative state change.

### Queue plumbing — `reprocess.rs`

Add `'asr_gap'` to the existing skip-list in two buckets (the third is implicitly covered):

- `fetch_bucket_manual` (line 48): `NOT IN ('failed', 'empty', 'no_source', 'asr_gap')`
- `fetch_bucket_null` (line 94): `NOT IN ('failed', 'empty', 'no_source', 'asr_gap')`
- `fetch_bucket_stale`: no change. Stale only fires on `has_lyrics = 1`. The quarantine endpoint sets `has_lyrics = 0`, so `asr_gap` rows are excluded from stale by the existing predicate.

### Future ASR-upgrade retry

The existing exception in both bucket WHERE clauses:

```sql
OR v.lyrics_pipeline_version < ?
```

automatically re-picks every `asr_gap` row on the next `LYRICS_PIPELINE_VERSION` bump. When a future ASR-related upgrade lands and the user approves a version bump, quarantined rows flow back through the null bucket and retry under the new ASR. If they still fail, the worker will re-quarantine them. No new infrastructure is needed.

Note on cost: this also means non-ASR pipeline bumps will retry `asr_gap` rows. That is acceptable because each retry is one worker tick per song (cheap on the queue, expensive on the Gemini side only if alignment runs). A future optimisation could split sentinels by failure category, but YAGNI for now.

### Reverse op (un-quarantine)

Not in scope. SQL one-liner when needed:

```sql
UPDATE videos SET lyrics_source = NULL, lyrics_pipeline_version = 0 WHERE id = ?;
```

If reverse ops become frequent, add a `POST /api/v1/lyrics/release-quarantine` endpoint in a follow-up.

### Race with in-flight worker

If the worker is mid-`process_song` for the row being quarantined, two cases:

- **Worker fails after quarantine fires** — Worker writes `lyrics_source = 'no_source'` on failure path (`worker.rs:263`). This overwrites `'asr_gap'`. The row is still parked (both are in the skip-list), but the audit trail loses the `asr_gap` signal. Acceptable: log still shows the original quarantine warn line.
- **Worker succeeds after quarantine fires** — Worker writes new lyrics output, `has_lyrics = 1`, `lyrics_source = '<provider>'`. Overwrites quarantine. Row resumes normal life. If the new output is also broken, we re-quarantine.

In practice the developer quarantines AFTER observing broken wall output, when the worker is idle on that row. Mid-flight races are rare and self-healing on re-observation. No locking needed.

### Workflow once shipped

1. Reprocess id=N (manual_priority or natural pickup).
2. Wall-verify the result.
3. If broken karaoke is caused by ASR transcription (not by text-source mismatch, not by pipeline bug):
   `curl -X POST http://10.77.9.201:8920/api/v1/lyrics/quarantine -d '{"video_id":N,"reason":"<short note>"}'`
4. Row parked, cache file deleted, wall clears.
5. Move to next song. No further investigation time on N.
6. Future: ASR upgrade lands → user approves `LYRICS_PIPELINE_VERSION` bump → quarantined rows auto-retry.

## Files touched

- `crates/sp-server/src/db/models.rs` — new `pub async fn quarantine_video_lyrics(pool, video_id, cache_dir, reason) -> Result<QuarantineOutcome>` that runs the UPDATE, deletes the cache file, emits the tracing line, returns `QuarantineOutcome { youtube_id, previous_source, deleted_cache_file }`. Returns `Err(NotFound)` when the video_id does not exist.
- `crates/sp-server/src/api/lyrics.rs` — new handler `quarantine_lyrics(State<AppState>, Json<QuarantineRequest>) -> ApiResult<Json<QuarantineResponse>>`. Maps `NotFound` → 404, other errors → 500.
- `crates/sp-server/src/api/routes.rs` — `.route("/api/v1/lyrics/quarantine", post(quarantine_lyrics))`.
- `crates/sp-server/src/lyrics/reprocess.rs` — extend skip-list in `fetch_bucket_manual` (line 48) and `fetch_bucket_null` (line 94).

No schema migration. No new columns. No new constants.

## Tests

Per `feedback_chunk_success_required.md`, `feedback_ci_production_behavior.md`, and standard TDD:

1. `db::models::quarantine_video_lyrics_sets_sentinel_and_deletes_cache_file`
   In-memory SQLite, fixture row with `has_lyrics=1, lyrics_source='ensemble:gemini'`, fixture `_lyrics.json` in tmpdir. After call: row shows `has_lyrics=0, lyrics_source='asr_gap', lyrics_pipeline_version=<current>`. Cache file gone. Outcome reports `deleted_cache_file: true, previous_source: 'ensemble:gemini'`.

2. `db::models::quarantine_video_lyrics_handles_missing_cache_file`
   Same as (1) but no cache file on disk. Outcome reports `deleted_cache_file: false`. DB still updated.

3. `db::models::quarantine_video_lyrics_returns_not_found_for_missing_id`
   Empty DB, call with `video_id=999`. Returns `Err` whose variant the handler maps to 404.

4. `reprocess::null_bucket_skips_asr_gap_at_current_version`
   Fixture rows: one with `lyrics_source='asr_gap', lyrics_pipeline_version=<current>`, one normal NULL row. Null bucket returns the NULL row, never the asr_gap row.

5. `reprocess::null_bucket_picks_asr_gap_when_version_older`
   Fixture row: `lyrics_source='asr_gap', lyrics_pipeline_version=<current-1>`. Null bucket picks it up (the existing `OR pipeline_version < ?` exception). Proves future-bump retry path is unbroken by the new sentinel.

6. `reprocess::manual_bucket_skips_asr_gap_at_current_version`
   Fixture: `lyrics_manual_priority=1, lyrics_source='asr_gap', lyrics_pipeline_version=<current>`. Manual bucket returns None. Prevents accidental manual-priority loops on quarantined songs.

7. `api::quarantine_endpoint_marks_row_and_returns_outcome`
   Integration test with `axum_test` (matching existing pattern in `api/lyrics.rs` tests if present, else use `tower::ServiceExt`). POST endpoint, assert 200, assert DB row updated, assert cache file deleted.

8. `api::quarantine_endpoint_returns_404_for_missing_video`
   POST with bogus `video_id`. Asserts 404.

## Out of scope

- Auto-detection metric (`asr_coverage_ratio`, `max_untranscribed_sung_window_ms`, etc.). Future work if manual-flagging proves insufficient.
- Dashboard UI button.
- Reverse endpoint.
- ASR-version-aware retry (separate constant from `LYRICS_PIPELINE_VERSION`).
- Multi-model merge logic. The whole point is to PARK these songs so we don't write workarounds in the current pipeline.

## Risk

- **Quarantine fires mid-worker.** Race documented above. Self-healing on re-observation.
- **Wall caching.** Renderer reads `_lyrics.json` per playback request, no in-memory cache survives the delete. Confirmed at `playback/lyrics_loader.rs:72-76` (reads file fresh) and `api/lyrics.rs:198-202` (reads file fresh).
- **Developer mis-quarantine.** Marking a song that is actually fixable (text-source bug, not ASR) wastes the future-retry slot. Mitigated by SQL un-quarantine being trivial.

## Acceptance

- Endpoint exists, returns 200 with expected JSON, returns 404 for missing IDs.
- DB row updated as specified.
- Cache file deleted when present.
- Queue buckets skip `asr_gap` at current pipeline version, pick it up at older versions.
- CI green (all jobs, including E2E).
- Wall verification: after curl, broken karaoke disappears from the wall on next playback of that song (renderer falls back to no-lyrics).
