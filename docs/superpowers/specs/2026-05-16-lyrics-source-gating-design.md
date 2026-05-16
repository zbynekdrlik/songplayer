# Lyrics Source Gating + Processing Metadata — Design Spec

**Date:** 2026-05-16
**Status:** Draft
**Goal:** Lock the current whisperx pipeline to songs whose text source is line-timed (yt_subs / lrclib / spotify) or curated (description). Block all other text-source paths via a new `unsupported_source` sentinel until a future-model PR delivers a better alignment strategy. Add `lyrics_processed_at` and `lyrics_alignment_model` columns so future audit and reprocess decisions can be made from SQL without parsing source-label strings.

**Architecture:** Single-PR change. One DB migration (two new columns), one new orchestrator gate function, two SQL skip-list extensions, one new admin endpoint, write-path threading through the worker. No `LYRICS_PIPELINE_VERSION` bump.

**Tech Stack:** Rust 2024, sqlx 0.8 (SQLite), tokio, tracing. No new dependencies.

---

## Background

Weeks of song-by-song wall verification produced a clear pattern: the current whisperx-based alignment pipeline yields acceptable wall output only when the text reference comes from one of two source families:

- **Line-timed text** — `yt_subs` (line-timed YouTube subtitles), `lrclib` (line-timed LRC), `spotify` (Spotify line-synced lyrics), `lrclib+timed-merge`. Whisperx only needs to assist with long-line splitting; line-level timing comes from the provider.
- **Curated text** — `description` (YouTube description block extracted by Claude). Whisperx performs full alignment but against high-quality, formatted text.

Everything else (genius scrapes, lrclib plain-text, ensemble:gemini ASR alone, raw whisperx without text reference) has consistently failed wall verification. These songs need a different alignment strategy entirely — a higher-quality ASR model, a multi-model ensemble, or a redesigned text-reconciliation flow — which is out of scope for this PR.

### Current source distribution (246 songs, 2026-05-16)

| `lyrics_source` | v20 | v21 | Total | Allowed under new gate? |
|---|---|---|---|---|
| `ensemble:gemini` | 70 | — | 70 | ❌ no text source |
| `no_source` | 55 | — | 55 | ❌ no text found |
| `whisperx-large-v3@rev1+claude-merge` | 4 | 55 | 59 | ❌ text=noisy (genius / lrclib-plain) |
| `yt_subs` | 2 | 22 | 24 | ✅ line-timed |
| `lrclib` | 21 | — | 21 | ✅ line-timed |
| `description+whisperx-large-v3@rev1` | 8 | — | 8 | ✅ curated text + whisperx |
| `asr_gap` | 4 | — | 4 | quarantined (separate sentinel) |
| `genius+whisperx-large-v3@rev1` | 1 | — | 1 | ❌ text=noisy |
| `lrclib+timed-merge` | 1 | — | 1 | ✅ line-timed |
| `spotify` | 1 | — | 1 | ✅ line-timed |
| `whisperx-large-v3@rev1` | 1 | — | 1 | ❌ no text reference |
| `yt_subs+whisperx-large-v3@rev1` | 1 | — | 1 | ✅ yt_subs path |

Allowed set: ~56 songs. Blocked set: ~190 songs (excluding 4 already at `asr_gap`).

### v21 anomaly

77 rows are stamped `pipeline_version=21`, but the constant in code is `LYRICS_PIPELINE_VERSION = 20`. Git log shows three reverts (`32f83b8`, `4a342c6`, `e16770f`) of a prior code path that bumped the version. The stale-bucket re-queue clause (`OR lyrics_pipeline_version < current`) cannot see these rows because they are ahead of current. This PR includes a one-shot SQL restamp that drops `version > 20` rows to `version = 19`, making them visible to the stale bucket again.

---

## Allowlist (locked)

The new gate function accepts a song if `gather_sources_impl` returns at least one `CandidateText` matching:

| `source` field | Accepted when |
|---|---|
| `yt_subs` | `has_timing == true` |
| `lrclib` | `has_timing == true` |
| `spotify` | `has_timing == true` |
| `description` | `lines.is_empty() == false` (no timing requirement — whisperx aligns it) |

Any other `source` value, or any of the above with the wrong timing/empty-lines state, fails the gate.

---

## Block mechanic

Sentinel pattern parallel to `asr_gap`:

```text
lyrics_source             = 'unsupported_source'
has_lyrics                = 0
lyrics_pipeline_version   = LYRICS_PIPELINE_VERSION   (20)
lyrics_processed_at       = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
lyrics_alignment_model    = NULL
lyrics_manual_priority    = 0
```

The stale-bucket selector in `crates/sp-server/src/lyrics/reprocess.rs` extends its existing skip-list from `NOT IN ('failed', 'empty', 'no_source', 'asr_gap')` to `NOT IN ('failed', 'empty', 'no_source', 'asr_gap', 'unsupported_source')` in both SQL spots (mirroring the asr_gap quarantine pattern).

A future-model PR retires the sentinel via a one-shot admin endpoint (parallel to whatever asr_gap-retire endpoint ships next).

---

## Schema migration

Single new migration block in `crates/sp-server/src/db/mod.rs::run_migrations`:

```sql
ALTER TABLE videos ADD COLUMN lyrics_processed_at    TEXT;
ALTER TABLE videos ADD COLUMN lyrics_alignment_model TEXT;
```

Both columns NULLABLE. Existing rows stay NULL — honest signal that we do not know when those legacy rows were processed or with which model.

The migration runner already enforces idempotency via the `schema_version` table. Pattern matches existing migrations exactly.

---

## Gate placement

Gate function lives in `crates/sp-server/src/lyrics/orchestrator.rs`:

```rust
pub(crate) fn is_allowed_text_source(candidates: &[CandidateText]) -> bool {
    candidates.iter().any(|c| match c.source.as_str() {
        "yt_subs" | "lrclib" | "spotify" => c.has_timing,
        "description" => !c.lines.is_empty(),
        _ => false,
    })
}
```

Called from `crates/sp-server/src/lyrics/worker.rs::process_song` immediately after `gather_sources` returns the `ctx`. Current location: between line 429 (`};` ending the `let ctx = match ...`) and line 431 (the `broadcast_stage("preprocessing", ...)` call). The insertion is:

```rust
// existing: let ctx = match self.gather_sources(&row).await { ... };
if !crate::lyrics::orchestrator::is_allowed_text_source(&ctx.candidate_texts) {
    let names: Vec<&str> = ctx.candidate_texts.iter().map(|c| c.source.as_str()).collect();
    tracing::warn!(
        video_id, youtube_id = %youtube_id,
        candidate_sources = ?names,
        "lyrics: no allowed text source — marking unsupported_source"
    );
    crate::db::models::mark_unsupported_source(
        &self.pool, video_id, LYRICS_PIPELINE_VERSION,
    ).await?;
    self.clear_processing().await;
    return Ok(());
}
// existing: self.broadcast_stage(... "preprocessing" ...)
```

The gate sits AFTER `gather_sources` (cheap HTTP / yt-dlp text fetches, ~5-15 s per song) but BEFORE `preprocess_vocals` (Mel-Roformer + anvuew dereverb, ~30-90 s) and whisperx alignment (~2-3 min). Blocked songs cost only the gather pass.

---

## Reprocess trigger — admin endpoint

New endpoint `POST /api/v1/lyrics/reprocess-catalog-with-new-gate`:

1. **Restamp v21 anomaly:**
   ```sql
   UPDATE videos
      SET lyrics_pipeline_version = 19
    WHERE lyrics_pipeline_version > 20;
   ```
2. **Queue all candidate rows:**
   ```sql
   UPDATE videos
      SET lyrics_manual_priority = 1
    WHERE lyrics_pipeline_version < 20
      AND (lyrics_source IS NULL
           OR lyrics_source NOT IN ('asr_gap', 'unsupported_source'));
   ```
3. **Return JSON:** `{ "restamped": N, "queued": M }`.

Endpoint is idempotent — calling it twice has no additional effect after the first run.

Worker picks rows serially. Cheap path (gather → gate → `unsupported_source`) takes ~1-3 s per blocked song. Allowed path runs full alignment ~3-5 min per allowed song. Catalog pass = ~190 × 2 s (~6 min for blocked) + ~56 × 4 min (~3.5 h for allowed). Total ~4 hours.

Endpoint extracted to a new file `crates/sp-server/src/api/lyrics_catalog.rs` because `api/lyrics.rs` is already 915 lines (cap is 1000) and a new handler + integration tests would push it over.

---

## Per-song processing metadata

Every `UPDATE videos SET ... lyrics_source = ...` site in `crates/sp-server/src/db/models.rs` is extended to also write `lyrics_processed_at` and `lyrics_alignment_model`. There are exactly four sites today, identified by line number in the current main branch:

| Line | Function context | `lyrics_processed_at` | `lyrics_alignment_model` |
|---|---|---|---|
| 426 | `mark_lyrics_failed` (or equivalent failure path) — `has_lyrics=?, lyrics_source=?, version=?` | `strftime(...)` | `NULL` |
| 453 | success finalize — `has_lyrics=1, source=?, version=?, quality_score=?` | `strftime(...)` | model derived in worker (see below) |
| 521 | clear-lyrics path — `has_lyrics=0, lyrics_source=NULL` | `NULL` (clearing = forgetting) | `NULL` |
| 805 | `quarantine_video_lyrics` (asr_gap) | `strftime(...)` | `NULL` |
| NEW  | `mark_unsupported_source` | `strftime(...)` | `NULL` |

For the success finalize at line 453, the model identifier is computed at the worker call site based on which alignment ran:

| Successful path | `alignment_model` literal |
|---|---|
| `yt_subs` raw ship-through (no whisperx) | `"none"` |
| `lrclib` raw ship-through (line-timed, no whisperx) | `"none"` |
| `spotify` raw ship-through (line-timed, no whisperx) | `"none"` |
| `lrclib+timed-merge` (line-timed + post-merge module) | `"timed-merge"` |
| `description` (curated text + whisperx alignment + claude-merge) | `"whisperx-large-v3@rev1"` |
| `yt_subs+whisperx-large-v3@rev1` (yt_subs with whisperx assist) | `"whisperx-large-v3@rev1"` |

These literals are defined as `pub const` strings in `crates/sp-server/src/lyrics/mod.rs` (or a sibling) so the worker and the tests reference the same identifiers — no magic strings scattered.

`lyrics_processed_at` is always written as `strftime('%Y-%m-%dT%H:%M:%fZ', 'now')` so the timestamp comes from SQLite, not the worker process — no timezone or clock-skew concerns.

API responses (the dashboard's lyrics songs list) expose both new fields. No new dashboard UI is added — the fields are available for future tooling and for ad-hoc SQL audit.

---

## File-level decomposition

| File | Change | Estimated LoC delta |
|---|---|---|
| `crates/sp-server/src/db/mod.rs` | New migration block | +20 |
| `crates/sp-server/src/db/models.rs` | Extend `VideoLyricsRow`, `finalize_track_for_video`, `mark_lyrics_failed`; new `mark_unsupported_source` | +60 |
| `crates/sp-server/src/lyrics/orchestrator.rs` | New `is_allowed_text_source` + call site | +40 |
| `crates/sp-server/src/lyrics/reprocess.rs` | Extend two SQL `NOT IN` lists | +4 |
| `crates/sp-server/src/lyrics/worker.rs` | Thread `alignment_model` through finalize / fail call sites | +30 |
| `crates/sp-server/src/api/lyrics_catalog.rs` | **NEW FILE** — admin endpoint handler + tests | +180 |
| `crates/sp-server/src/api/mod.rs` | Wire new module | +1 |
| `crates/sp-server/src/api/routes.rs` | Wire new route | +1 |

All touched files stay under the 1000-line cap. Total diff ~340 LoC — fits the bundling gate.

---

## Testing

### Migration tests
- Existing schema_version test pattern: assert version advances; assert new columns exist; assert existing rows preserved with NULL for new columns.

### Gate logic tests (`is_allowed_text_source` — sibling test module in `orchestrator.rs`)
- Empty candidates → false.
- Only `yt_subs` with `has_timing=true` → true.
- Only `yt_subs` with `has_timing=false` → false.
- Only `lrclib` with `has_timing=true` → true.
- Only `lrclib` with `has_timing=false` → false.
- Only `spotify` with `has_timing=true` → true.
- Only `description` with non-empty lines → true.
- Only `description` with empty lines → false.
- Only `genius` (any state) → false.
- Mixed: `genius` + `yt_subs` with timing → true (passes because of yt_subs).

### `mark_unsupported_source` tests (sibling in `models.rs`)
- Writes all five expected fields with correct values.
- `lyrics_manual_priority` cleared to 0.
- Round-trip via `SELECT` confirms persistence.

### Skip-list tests (sibling in `reprocess.rs`)
- Video at `lyrics_source='unsupported_source'` not picked by `get_next_video_for_lyrics`.
- Video at `lyrics_source='unsupported_source'` not picked by the stale-bucket query.

### Endpoint integration tests (sibling in `api/lyrics_catalog.rs`)
- Endpoint downgrades all `version > 20` rows to `version = 19`.
- Endpoint sets `manual_priority = 1` only on rows that pass the `NOT IN` filter.
- Endpoint returns correct `{ restamped, queued }` counts.
- Endpoint is idempotent — second call produces `{ restamped: 0, queued: 0 }`.

### Worker write-path tests (sibling in `models.rs` test module)
- Success finalize (line 453) writes non-NULL timestamp and the expected model literal.
- Failure path (line 426) writes non-NULL timestamp and NULL model.
- Clear path (line 521) writes NULL timestamp and NULL model.
- Quarantine path (line 805, asr_gap) writes non-NULL timestamp and NULL model.

---

## Wall verification workflow (post-deploy, user-driven)

This section describes the manual workflow after the PR ships. Not part of the code work but recorded for completeness.

1. Deploy lands on `win-resolume`. CI green.
2. User hits `POST /api/v1/lyrics/reprocess-catalog-with-new-gate` from the dashboard or via curl.
3. Worker processes serially. As each allowed song finalizes, the agent (per `feedback_auto_play_and_ci_monitor.md`) auto-queues it to sp-live playlist 184.
4. User wall-verifies song-by-song per the `lyrics-verify` skill. Acceptable songs stay. Broken songs → fix code → reprocess that single song → re-verify (per `feedback_song_by_song_iteration.md`).
5. Once the full allowed set is wall-acceptable, user says "merge it" → PR ships.
6. Future-model PR handles the 190 blocked `unsupported_source` rows.

---

## Out of scope (explicit non-goals)

- `LYRICS_PIPELINE_VERSION` bump. Constant stays at 20. Per `feedback_no_bump_until_proven.md` + `feedback_pipeline_version_approval.md`.
- Future-model PR (Gemini 3.x alignment, multi-model ensemble, redesigned merge). Separate work.
- Dashboard UI controls for un-sentinel-ing `unsupported_source` rows. Admin endpoint suffices.
- Backfill of `lyrics_processed_at` / `lyrics_alignment_model` for existing rows. NULL is honest — we do not know.
- A dedicated `lyrics_text_source` column. Text source remains parseable from the `lyrics_source` string prefix (`yt_subs`, `description`, `lrclib`, `spotify`). If a future PR demands SQL-level querying on text source, that migration is trivial — add the column then.
- Resetting the asr_gap quarantine. Untouched.

---

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| Migration runs but new columns fail to write because a write-path was missed | Audit every `UPDATE videos` statement in `db/models.rs` during implementation. Sibling tests cover each updated path. |
| Gate function gets the source-name match wrong (typo) and excludes valid songs | Allowlist constants are `match` arms with `&str` literals — compile-time correctness. Tests exercise each allowed source individually. |
| v21 restamp catches rows that should not be restamped | Restamp condition is `pipeline_version > 20` — current is exactly 20. No future version can be ahead until the constant bumps. Endpoint is admin-only, idempotent. |
| Endpoint queues too many songs and worker backlogs | Reprocess is serial. Worst case: ~4 h to drain. User can stop the worker or call `POST /api/v1/lyrics/clear-manual-queue` to bail. |
| Dashboard becomes confused by `unsupported_source` rows | Dashboard already handles `asr_gap` and `no_source` as "no lyrics" rows — same UI path. New sentinel renders identically. |

---

## Acceptance criteria

The PR is mergeable when:

1. CI is green (all jobs including deploy + E2E on win-resolume).
2. The new admin endpoint has been called on production at least once.
3. The catalog distribution shows: ~56 allowed songs at `pipeline_version=20` with non-NULL `lyrics_processed_at` and `lyrics_alignment_model`; ~190 blocked songs at `lyrics_source='unsupported_source'`.
4. User has wall-verified the allowed set (or a representative sample) and explicitly says "merge it".
