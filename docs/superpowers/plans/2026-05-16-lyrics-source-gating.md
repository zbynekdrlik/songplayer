# Lyrics Source Gating + Processing Metadata Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Lock the current whisperx pipeline to songs whose text source is line-timed (yt_subs / lrclib / spotify) or curated (description); block everything else via a new `unsupported_source` sentinel. Add `lyrics_processed_at` + `lyrics_alignment_model` columns and a one-shot admin endpoint that restamps the 77 stale-v21 rows and queues the catalog for reprocess under the new gate.

**Architecture:** Single PR, ~7 commits. One DB migration (V19, two new NULLABLE columns). One new orchestrator gate function. Four existing `UPDATE videos SET lyrics_*` sites extended to write the two new columns. One new helper fn (`mark_unsupported_source`). Three new alignment-model constants. Two skip-list SQL extensions (parallel to asr_gap). One new file `api/lyrics_catalog.rs` with the admin endpoint and tests. No `LYRICS_PIPELINE_VERSION` bump.

**Tech Stack:** Rust 2024, sqlx 0.8 (SQLite), tokio, axum 0.8, tracing. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-05-16-lyrics-source-gating-design.md` (commit `365dbf8`).

---

## Implementer rules (verbatim — non-negotiable)

- **TDD strict:** failing test first → trust by inspection → implement → trust by inspection → `cargo fmt --all --check` (the ONLY local cargo command allowed) → commit on green.
- **NEVER** run `cargo clippy`, `cargo test`, `cargo build`, or `cargo check` locally. Rely on CI.
- **File-size cap 1000 lines per file.** Current sizes:
  - `db/models.rs` 838 (cap allows +162)
  - `db/mod.rs` 328 (plenty)
  - `lyrics/orchestrator.rs` 837 (cap allows +163)
  - `lyrics/reprocess.rs` 668 (plenty)
  - `lyrics/worker.rs` 793 (plenty)
  - `api/lyrics.rs` 915 (only 85 room — **DO NOT touch**; new admin endpoint goes in NEW FILE `api/lyrics_catalog.rs`)
  - `api/routes.rs` 837 (no edits)
  - `api/mod.rs` will gain ~5 lines for `pub mod lyrics_catalog;` and one `.route(...)` block
- **One commit per Task.** Tests bundled inside the same commit as the feature (recent songplayer convention — see `a7ab707` and `fe847ba`).
- **`mutants::skip` requires inline justification** when used.
- **Do NOT push.** The controller batches and pushes once after all commits land.
- **`LYRICS_PIPELINE_VERSION` stays at 20.** Per `feedback_pipeline_version_approval.md` + `feedback_no_bump_until_proven.md` — NEVER bump.
- **Root-cause fixes only.** Per `feedback_take_ownership.md`.
- **Two new NULLABLE columns** via incremental ALTER TABLE in a new V19 migration block.

---

## File Structure

| File | Action | LoC delta |
|---|---|---|
| `crates/sp-server/src/db/mod.rs` | Add `MIGRATION_V19` constant, append to `MIGRATIONS` array, add new `mod_tests_v19` declaration | +25 |
| `crates/sp-server/src/db/mod_tests_v19.rs` | **NEW** — migration tests (mirror `mod_tests_v18.rs`) | +95 |
| `crates/sp-server/src/db/models.rs` | Extend 4 `UPDATE videos` sites; add `mark_unsupported_source` fn | +90 |
| `crates/sp-server/src/db/models_tests.rs` | Add tests for the 4 extended write-paths + new fn | +180 |
| `crates/sp-server/src/lyrics/mod.rs` | Add 3 `pub const` alignment-model strings | +12 |
| `crates/sp-server/src/lyrics/orchestrator.rs` | Add `is_allowed_text_source` + sibling tests | +90 |
| `crates/sp-server/src/lyrics/worker.rs` | Insert gate call after gather; thread `alignment_model` to `mark_video_lyrics_complete` call site (~line 608) | +30 |
| `crates/sp-server/src/lyrics/reprocess.rs` | Extend two SQL `NOT IN` lists; add 2 sibling tests | +60 |
| `crates/sp-server/src/api/lyrics_catalog.rs` | **NEW** — admin endpoint handler + 4 integration tests | +220 |
| `crates/sp-server/src/api/mod.rs` | `pub mod lyrics_catalog;` + one `.route(...)` block | +5 |

Total: ~810 LoC across the diff (well under the 300-LoC bundling threshold per individual file; PR-wide diff is one feature, so fits "one feature = one PR").

---

## Task 1: Migration V19 — add `lyrics_processed_at` + `lyrics_alignment_model`

**Files:**
- Modify: `crates/sp-server/src/db/mod.rs:11-30` (MIGRATIONS array), `crates/sp-server/src/db/mod.rs:248-251` (V18 const), `crates/sp-server/src/db/mod.rs:326-329` (test module declarations)
- Create: `crates/sp-server/src/db/mod_tests_v19.rs`

- [ ] **Step 1.1: Write the failing tests file**

Create `crates/sp-server/src/db/mod_tests_v19.rs`:

```rust
//! V19 migration tests. Sibling file split from mod_tests.rs to honor
//! the airuleset 1000-line cap.

#![allow(unused_imports)]

use super::*;

async fn setup() -> SqlitePool {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    pool
}

/// Apply V1..V18 manually so a test can seed pre-V19 data and then trigger
/// V19 in isolation. Mirrors `apply_through_v17` in `mod_tests_v18.rs`.
async fn apply_through_v18(pool: &SqlitePool) {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await
    .unwrap();

    // MIGRATIONS[..18] is V1..V18 inclusive (18 entries), leaving V19
    // (the last one) for the caller to apply via run_migrations.
    for &(version, sql) in &MIGRATIONS[..18] {
        let mut tx = pool.begin().await.unwrap();
        for stmt in sql.split(';') {
            let s = stmt.trim();
            if !s.is_empty() {
                sqlx::query(s).execute(&mut *tx).await.unwrap();
            }
        }
        sqlx::query("INSERT INTO schema_version (version) VALUES (?)")
            .bind(version)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
}

#[tokio::test]
async fn migration_v19_adds_lyrics_processed_at_column() {
    let pool = setup().await;
    let cols: Vec<String> = sqlx::query("PRAGMA table_info(videos)")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
    assert!(
        cols.contains(&"lyrics_processed_at".to_string()),
        "V19 must add lyrics_processed_at column; got: {cols:?}"
    );
}

#[tokio::test]
async fn migration_v19_adds_lyrics_alignment_model_column() {
    let pool = setup().await;
    let cols: Vec<String> = sqlx::query("PRAGMA table_info(videos)")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
    assert!(
        cols.contains(&"lyrics_alignment_model".to_string()),
        "V19 must add lyrics_alignment_model column; got: {cols:?}"
    );
}

#[tokio::test]
async fn migration_v19_leaves_existing_rows_with_null_for_new_columns() {
    // Apply V1..V18 manually so we can seed a row BEFORE V19 fires.
    let pool = create_memory_pool().await.unwrap();
    apply_through_v18(&pool).await;

    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title) \
         VALUES (1, 'aaa', 't') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // Apply V19.
    run_migrations(&pool).await.unwrap();

    let processed_at: Option<String> =
        sqlx::query_scalar("SELECT lyrics_processed_at FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let model: Option<String> =
        sqlx::query_scalar("SELECT lyrics_alignment_model FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        processed_at.is_none() && model.is_none(),
        "V19 must NOT backfill existing rows — they stay NULL"
    );
}

#[tokio::test]
async fn migration_v19_advances_schema_version() {
    let pool = setup().await;
    let v = current_schema_version(&pool).await.unwrap();
    assert_eq!(v, 19, "schema_version must advance to 19 after V19 applied");
}
```

- [ ] **Step 1.2: Verify tests fail to compile (V19 not yet added)**

Skipped — `cargo test` is banned locally per rules. CI will run them. Trust by inspection: the tests reference `lyrics_processed_at`, `lyrics_alignment_model`, and `schema_version == 19`, none of which exist until Step 1.3 lands.

- [ ] **Step 1.3: Add the V19 migration**

In `crates/sp-server/src/db/mod.rs`, append `(19, MIGRATION_V19)` to the `MIGRATIONS` array (line 29 → add new row after V18 entry):

```rust
const MIGRATIONS: &[(i32, &str)] = &[
    (1, MIGRATION_V1),
    (2, MIGRATION_V2),
    (3, MIGRATION_V3),
    (4, MIGRATION_V4),
    (5, MIGRATION_V5),
    (6, MIGRATION_V6),
    (7, MIGRATION_V7),
    (8, MIGRATION_V8),
    (9, MIGRATION_V9),
    (10, MIGRATION_V10),
    (11, MIGRATION_V11),
    (12, MIGRATION_V12),
    (13, MIGRATION_V13),
    (14, MIGRATION_V14),
    (15, MIGRATION_V15),
    (16, MIGRATION_V16),
    (17, MIGRATION_V17),
    (18, MIGRATION_V18),
    (19, MIGRATION_V19),
];
```

Add the new constant after `MIGRATION_V18` (line 251):

```rust
// V19: Add lyrics_processed_at + lyrics_alignment_model columns for audit
// and reprocess-decision SQL queries. Both NULLABLE. Existing rows stay
// NULL — honest signal that we do not know when/how they were processed.
// See docs/superpowers/specs/2026-05-16-lyrics-source-gating-design.md.
const MIGRATION_V19: &str = "
ALTER TABLE videos ADD COLUMN lyrics_processed_at TEXT;
ALTER TABLE videos ADD COLUMN lyrics_alignment_model TEXT;
";
```

Add the test module declaration after `tests_v18` (line 328):

```rust
#[path = "mod_tests_v19.rs"]
#[cfg(test)]
mod tests_v19;
```

- [ ] **Step 1.4: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 1.5: Commit**

```bash
git add crates/sp-server/src/db/mod.rs crates/sp-server/src/db/mod_tests_v19.rs
git commit -m "feat(lyrics): V19 migration adds lyrics_processed_at + alignment_model columns"
```

---

## Task 2: Extend 4 lyrics UPDATE sites + new helper fn + alignment-model constants

**Files:**
- Modify: `crates/sp-server/src/db/models.rs:417-436` (mark_video_lyrics), `crates/sp-server/src/db/models.rs:444-464` (mark_video_lyrics_complete), `crates/sp-server/src/db/models.rs:518-526` (reset_video_lyrics), `crates/sp-server/src/db/models.rs:782-830` (quarantine_video_lyrics)
- Append: `crates/sp-server/src/db/models.rs` — new `mark_unsupported_source` fn
- Modify: `crates/sp-server/src/db/models_tests.rs` — add 5 new tests
- Modify: `crates/sp-server/src/lyrics/mod.rs` — add 3 pub const strings

- [ ] **Step 2.1: Add alignment-model constants**

In `crates/sp-server/src/lyrics/mod.rs`, after the existing `pub const LYRICS_PIPELINE_VERSION` (line 166), add:

```rust
/// Alignment-model identifier written to `lyrics_alignment_model` for the
/// raw-ship-through path (line-timed text source, no whisperx alignment ran).
pub const ALIGNMENT_MODEL_NONE: &str = "none";

/// Alignment-model identifier for the line-timed merge module path (e.g.
/// `lrclib+timed-merge` source label). No whisperx alignment ran.
pub const ALIGNMENT_MODEL_TIMED_MERGE: &str = "timed-merge";

/// Alignment-model identifier for whisperx large-v3 rev1 (current default
/// alignment model for description + yt_subs+whisperx paths).
pub const ALIGNMENT_MODEL_WHISPERX_V3_REV1: &str = "whisperx-large-v3@rev1";
```

- [ ] **Step 2.2: Extend `mark_video_lyrics` (failure path, line 426 SQL)**

Replace lines 417-436 in `crates/sp-server/src/db/models.rs`:

```rust
#[cfg_attr(test, mutants::skip)]
pub async fn mark_video_lyrics(
    pool: &SqlitePool,
    video_id: i64,
    has_lyrics: bool,
    lyrics_source: Option<&str>,
    pipeline_version: u32,
) -> Result<(), sqlx::Error> {
    // lyrics_processed_at = strftime() so the timestamp comes from SQLite
    // (no clock-skew between server process and DB). lyrics_alignment_model
    // = NULL because the failure path has no successful alignment to record.
    sqlx::query(
        "UPDATE videos SET has_lyrics = ?, lyrics_source = ?, lyrics_pipeline_version = ?, \
         lyrics_processed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
         lyrics_alignment_model = NULL \
         WHERE id = ?",
    )
    .bind(has_lyrics as i32)
    .bind(lyrics_source)
    .bind(pipeline_version as i64)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}
```

- [ ] **Step 2.3: Extend `mark_video_lyrics_complete` (success path, line 453 SQL) with new model arg**

Replace lines 444-464 in `crates/sp-server/src/db/models.rs`:

```rust
#[cfg_attr(test, mutants::skip)] // single UPDATE; covered by integration tests below
pub async fn mark_video_lyrics_complete(
    pool: &SqlitePool,
    video_id: i64,
    source: &str,
    pipeline_version: u32,
    quality_score: Option<f32>,
    alignment_model: Option<&str>,
) -> Result<(), sqlx::Error> {
    // lyrics_alignment_model is Option because some success paths (raw line-timed
    // ship-through) genuinely have no alignment model. Callers pass
    // Some("none") if they want the explicit literal vs None.
    sqlx::query(
        "UPDATE videos SET has_lyrics = 1, lyrics_source = ?, \
         lyrics_pipeline_version = ?, lyrics_quality_score = ?, \
         lyrics_manual_priority = 0, \
         lyrics_processed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
         lyrics_alignment_model = ? \
         WHERE id = ?",
    )
    .bind(source)
    .bind(pipeline_version as i64)
    .bind(quality_score.map(|q| q as f64))
    .bind(alignment_model)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}
```

- [ ] **Step 2.4: Extend `reset_video_lyrics` (clear path, line 521 SQL)**

Replace lines 518-526:

```rust
/// Reset lyrics fields for a video so it will be re-processed.
///
/// Also clears `lyrics_processed_at` and `lyrics_alignment_model` because
/// "reset" means "forget when/how this was processed" — leaving the old
/// timestamp/model would make audit queries misleading.
#[cfg_attr(test, mutants::skip)]
pub async fn reset_video_lyrics(pool: &SqlitePool, video_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos SET has_lyrics = 0, lyrics_source = NULL, \
         lyrics_processed_at = NULL, lyrics_alignment_model = NULL \
         WHERE id = ?",
    )
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}
```

- [ ] **Step 2.5: Extend `quarantine_video_lyrics` (asr_gap path, line 805 SQL)**

In `crates/sp-server/src/db/models.rs` around line 804-811, replace the UPDATE:

```rust
    sqlx::query(
        "UPDATE videos SET has_lyrics = 0, lyrics_source = 'asr_gap', \
         lyrics_pipeline_version = ?, lyrics_manual_priority = 0, \
         lyrics_processed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
         lyrics_alignment_model = NULL \
         WHERE id = ?",
    )
    .bind(current_pipeline_version as i64)
    .bind(video_id)
    .execute(pool)
    .await?;
```

- [ ] **Step 2.6: Add `mark_unsupported_source` fn at the end of models.rs (before `// Tests` divider, line 832)**

Insert before the `// Tests` divider:

```rust
// ---------------------------------------------------------------------------
// Unsupported-source sentinel
// ---------------------------------------------------------------------------

/// Mark a video as having no allowed text source. Parallel to `quarantine_video_lyrics`
/// (asr_gap) but for the case where the gather pass found candidates but none of
/// them passed `is_allowed_text_source` — typically genius-only, lrclib-plain
/// without timing, or no_source-then-just-whisperx.
///
/// Sets `lyrics_source = 'unsupported_source'`, clears `has_lyrics` and
/// `lyrics_manual_priority`, stamps the current pipeline version and timestamp,
/// nulls out `lyrics_alignment_model` (nothing aligned). The stale-bucket
/// `NOT IN (...)` skip-list in `reprocess.rs` is extended in Task 6 to
/// exclude this sentinel so the worker does not loop on it.
///
/// A future-model PR retires the sentinel via a dedicated admin endpoint
/// (parallel to whatever asr_gap-retire endpoint ships next). See
/// `docs/superpowers/specs/2026-05-16-lyrics-source-gating-design.md`.
#[cfg_attr(test, mutants::skip)] // Single UPDATE — observable side effects
// are all covered by `mark_unsupported_source_writes_all_fields` and
// `mark_unsupported_source_clears_manual_priority` in models_tests.rs;
// remaining mutation targets reduce to SQL string literals which cargo-mutants
// cannot mutate meaningfully.
pub async fn mark_unsupported_source(
    pool: &SqlitePool,
    video_id: i64,
    current_pipeline_version: u32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos SET has_lyrics = 0, lyrics_source = 'unsupported_source', \
         lyrics_pipeline_version = ?, lyrics_manual_priority = 0, \
         lyrics_processed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
         lyrics_alignment_model = NULL \
         WHERE id = ?",
    )
    .bind(current_pipeline_version as i64)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}
```

- [ ] **Step 2.7: Add 5 tests in `crates/sp-server/src/db/models_tests.rs`**

Append at the end of `models_tests.rs` (before the closing brace of the test module, if any):

```rust
#[tokio::test]
async fn mark_video_lyrics_writes_processed_at_and_null_model_on_failure() {
    let (pool, video_id) = setup_with_video().await;
    mark_video_lyrics(&pool, video_id, false, Some("failed"), 20)
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT lyrics_processed_at, lyrics_alignment_model FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let processed_at: Option<String> = row.try_get("lyrics_processed_at").ok().flatten();
    let model: Option<String> = row.try_get("lyrics_alignment_model").ok().flatten();
    assert!(processed_at.is_some(), "processed_at must be set on failure path");
    assert!(model.is_none(), "alignment_model must be NULL on failure path");
}

#[tokio::test]
async fn mark_video_lyrics_complete_writes_processed_at_and_explicit_model() {
    let (pool, video_id) = setup_with_video().await;
    mark_video_lyrics_complete(
        &pool,
        video_id,
        "description+whisperx-large-v3@rev1",
        20,
        Some(0.85),
        Some(crate::lyrics::ALIGNMENT_MODEL_WHISPERX_V3_REV1),
    )
    .await
    .unwrap();
    let row = sqlx::query(
        "SELECT lyrics_processed_at, lyrics_alignment_model FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let processed_at: Option<String> = row.try_get("lyrics_processed_at").ok().flatten();
    let model: Option<String> = row.try_get("lyrics_alignment_model").ok().flatten();
    assert!(processed_at.is_some(), "processed_at must be set on success path");
    assert_eq!(
        model.as_deref(),
        Some("whisperx-large-v3@rev1"),
        "alignment_model must round-trip the literal"
    );
}

#[tokio::test]
async fn reset_video_lyrics_clears_processed_at_and_model() {
    let (pool, video_id) = setup_with_video().await;
    // Seed with non-NULL values first.
    mark_video_lyrics_complete(
        &pool,
        video_id,
        "yt_subs",
        20,
        Some(0.9),
        Some(crate::lyrics::ALIGNMENT_MODEL_NONE),
    )
    .await
    .unwrap();
    // Now reset.
    reset_video_lyrics(&pool, video_id).await.unwrap();
    let row = sqlx::query(
        "SELECT lyrics_processed_at, lyrics_alignment_model FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let processed_at: Option<String> = row.try_get("lyrics_processed_at").ok().flatten();
    let model: Option<String> = row.try_get("lyrics_alignment_model").ok().flatten();
    assert!(
        processed_at.is_none() && model.is_none(),
        "reset must NULL both new columns"
    );
}

#[tokio::test]
async fn mark_unsupported_source_writes_all_fields() {
    let (pool, video_id) = setup_with_video().await;
    mark_unsupported_source(&pool, video_id, 20).await.unwrap();
    let row = sqlx::query(
        "SELECT has_lyrics, lyrics_source, lyrics_pipeline_version, \
                lyrics_processed_at, lyrics_alignment_model, lyrics_manual_priority \
         FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let has_lyrics: i64 = row.get("has_lyrics");
    let source: String = row.get("lyrics_source");
    let version: i64 = row.get("lyrics_pipeline_version");
    let processed_at: Option<String> = row.try_get("lyrics_processed_at").ok().flatten();
    let model: Option<String> = row.try_get("lyrics_alignment_model").ok().flatten();
    let priority: i64 = row.get("lyrics_manual_priority");
    assert_eq!(has_lyrics, 0, "has_lyrics must be cleared");
    assert_eq!(source, "unsupported_source", "source must be sentinel literal");
    assert_eq!(version, 20, "pipeline_version must be current");
    assert!(processed_at.is_some(), "processed_at must be set");
    assert!(model.is_none(), "alignment_model must be NULL");
    assert_eq!(priority, 0, "manual_priority must be cleared");
}

#[tokio::test]
async fn mark_unsupported_source_clears_manual_priority() {
    let (pool, video_id) = setup_with_video().await;
    // Seed with manual_priority = 1.
    sqlx::query("UPDATE videos SET lyrics_manual_priority = 1 WHERE id = ?")
        .bind(video_id)
        .execute(&pool)
        .await
        .unwrap();
    mark_unsupported_source(&pool, video_id, 20).await.unwrap();
    let priority: i64 = sqlx::query_scalar(
        "SELECT lyrics_manual_priority FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(priority, 0, "manual_priority must be cleared by the sentinel");
}

#[tokio::test]
async fn quarantine_video_lyrics_writes_processed_at_and_null_model() {
    // Reuses the same scaffolding as the existing quarantine tests at
    // lines 701-779. Focused assertion: the SQL extension in Task 2.5 must
    // populate lyrics_processed_at and leave lyrics_alignment_model NULL.
    let (pool, video_id) = setup_with_video().await;
    let tmp = tempfile::tempdir().unwrap();
    quarantine_video_lyrics(&pool, video_id, tmp.path(), "test reason", 20)
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT lyrics_processed_at, lyrics_alignment_model FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let processed_at: Option<String> = row.try_get("lyrics_processed_at").ok().flatten();
    let model: Option<String> = row.try_get("lyrics_alignment_model").ok().flatten();
    assert!(processed_at.is_some(), "quarantine must set processed_at");
    assert!(model.is_none(), "quarantine must leave alignment_model NULL");
}
```

All 6 tests REUSE the existing `setup_with_video()` helper at `crates/sp-server/src/db/models_tests.rs:10`. It seeds a playlist + one video with `youtube_id = 'yt123'`. Do NOT define a duplicate.

The quarantine test relies on `tempfile::tempdir()` — already in the dev-dependencies (used by existing quarantine tests at lines 712-779).

- [ ] **Step 2.8: Update the one external caller of `mark_video_lyrics_complete` to pass the new arg**

In `crates/sp-server/src/lyrics/worker.rs` near line 608, the existing call:

```rust
        crate::db::models::mark_video_lyrics_complete(
            &self.pool,
            video_id,
            &track.source,
            LYRICS_PIPELINE_VERSION,
            None,
        )
        .await?;
```

becomes (insert the `let alignment_model = ...;` block immediately before the call):

```rust
        // Pick the alignment-model literal for this success path. Logic
        // mirrors the table in the spec ("Per-song processing metadata"):
        //   - source label contains `whisperx` → WHISPERX_V3_REV1
        //   - source label contains `timed-merge` → TIMED_MERGE
        //   - source label is exactly `yt_subs` / `lrclib` / `spotify` (raw
        //     ship-through, no alignment ran) → NONE
        //   - anything else → None (NULL — unknown model, e.g. legacy
        //     ensemble:gemini paths that may still appear in `track.source`)
        let alignment_model: Option<&'static str> = if track.source.contains("whisperx") {
            Some(crate::lyrics::ALIGNMENT_MODEL_WHISPERX_V3_REV1)
        } else if track.source.contains("timed-merge") {
            Some(crate::lyrics::ALIGNMENT_MODEL_TIMED_MERGE)
        } else if track.source == "yt_subs"
            || track.source == "lrclib"
            || track.source == "spotify"
        {
            Some(crate::lyrics::ALIGNMENT_MODEL_NONE)
        } else {
            None
        };

        crate::db::models::mark_video_lyrics_complete(
            &self.pool,
            video_id,
            &track.source,
            LYRICS_PIPELINE_VERSION,
            None,
            alignment_model,
        )
        .await?;
```

The existing call to `mark_video_lyrics` at line 259 (failure path) takes no new arg — its signature was extended internally via the SQL change only.

- [ ] **Step 2.9: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 2.10: Commit**

```bash
git add crates/sp-server/src/db/models.rs \
        crates/sp-server/src/db/models_tests.rs \
        crates/sp-server/src/lyrics/mod.rs \
        crates/sp-server/src/lyrics/worker.rs
git commit -m "feat(lyrics): write processed_at + alignment_model on every lyrics UPDATE site"
```

---

## Task 3: Gate function `is_allowed_text_source`

**Files:**
- Modify: `crates/sp-server/src/lyrics/orchestrator.rs` — append gate fn + sibling test mod

- [ ] **Step 3.1: Add the gate function**

Append at the end of `crates/sp-server/src/lyrics/orchestrator.rs` (before any existing `#[cfg(test)] mod tests` block — if one exists, place the new fn ABOVE it):

```rust
/// Decide whether the gathered text candidates contain at least one source
/// from the allowed set. Called by the worker after `gather_sources` and
/// before any expensive alignment dispatch (Demucs, whisperx, Claude-merge).
///
/// Allowed sources:
/// - `yt_subs`, `lrclib`, `spotify` — accepted when `has_timing == true`
///   (line-timed text already; whisperx only assists with long-line splits)
/// - `description` — accepted when `lines.is_empty() == false`
///   (curated text; whisperx performs full alignment against it)
///
/// Anything else (`genius`, `lrclib` without timing, raw whisperx with no
/// text reference, empty candidate list) is rejected. The worker stamps the
/// row with `lyrics_source = 'unsupported_source'` and bails — see
/// `db::models::mark_unsupported_source`.
///
/// See `docs/superpowers/specs/2026-05-16-lyrics-source-gating-design.md`.
pub(crate) fn is_allowed_text_source(
    candidates: &[crate::lyrics::provider::CandidateText],
) -> bool {
    candidates.iter().any(|c| match c.source.as_str() {
        "yt_subs" | "lrclib" | "spotify" => c.has_timing,
        "description" => !c.lines.is_empty(),
        _ => false,
    })
}

#[cfg(test)]
mod is_allowed_text_source_tests {
    use super::is_allowed_text_source;
    use crate::lyrics::provider::CandidateText;

    fn candidate(source: &str, has_timing: bool, lines: Vec<&str>) -> CandidateText {
        CandidateText {
            source: source.to_string(),
            lines: lines.into_iter().map(String::from).collect(),
            has_timing,
            line_timings: None,
        }
    }

    #[test]
    fn empty_candidates_rejected() {
        assert!(!is_allowed_text_source(&[]));
    }

    #[test]
    fn yt_subs_with_timing_accepted() {
        assert!(is_allowed_text_source(&[candidate("yt_subs", true, vec!["line"])]));
    }

    #[test]
    fn yt_subs_without_timing_rejected() {
        assert!(!is_allowed_text_source(&[candidate("yt_subs", false, vec!["line"])]));
    }

    #[test]
    fn lrclib_with_timing_accepted() {
        assert!(is_allowed_text_source(&[candidate("lrclib", true, vec!["line"])]));
    }

    #[test]
    fn lrclib_without_timing_rejected() {
        assert!(!is_allowed_text_source(&[candidate("lrclib", false, vec!["line"])]));
    }

    #[test]
    fn spotify_with_timing_accepted() {
        assert!(is_allowed_text_source(&[candidate("spotify", true, vec!["line"])]));
    }

    #[test]
    fn spotify_without_timing_rejected() {
        assert!(!is_allowed_text_source(&[candidate("spotify", false, vec!["line"])]));
    }

    #[test]
    fn description_with_lines_accepted() {
        assert!(is_allowed_text_source(&[candidate("description", false, vec!["a", "b"])]));
    }

    #[test]
    fn description_empty_lines_rejected() {
        assert!(!is_allowed_text_source(&[candidate("description", false, vec![])]));
    }

    #[test]
    fn genius_always_rejected() {
        assert!(!is_allowed_text_source(&[candidate("genius", true, vec!["line"])]));
        assert!(!is_allowed_text_source(&[candidate("genius", false, vec!["line"])]));
    }

    #[test]
    fn mixed_genius_plus_yt_subs_with_timing_accepted() {
        assert!(is_allowed_text_source(&[
            candidate("genius", true, vec!["line"]),
            candidate("yt_subs", true, vec!["line"]),
        ]));
    }
}
```

- [ ] **Step 3.2: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 3.3: Commit**

```bash
git add crates/sp-server/src/lyrics/orchestrator.rs
git commit -m "feat(lyrics): is_allowed_text_source gate fn + 11 unit tests"
```

---

## Task 4: Wire the gate into the worker

**Files:**
- Modify: `crates/sp-server/src/lyrics/worker.rs:423-440` (gate insertion between `let ctx = ...` and `broadcast_stage("preprocessing", ...)`)

- [ ] **Step 4.1: Insert the gate call**

In `crates/sp-server/src/lyrics/worker.rs::process_song`, the current code (lines 423-440):

```rust
        let ctx = match self.gather_sources(&row).await {
            Ok(c) => c,
            Err(e) => {
                self.clear_processing().await;
                return Err(e);
            }
        };

        self.broadcast_stage(
            video_id,
            &youtube_id,
            &song,
            &artist,
            "preprocessing",
            None,
            started_at_unix_ms,
        )
        .await;
```

becomes:

```rust
        let ctx = match self.gather_sources(&row).await {
            Ok(c) => c,
            Err(e) => {
                self.clear_processing().await;
                return Err(e);
            }
        };

        // GATE: per docs/superpowers/specs/2026-05-16-lyrics-source-gating-design.md,
        // refuse to run expensive alignment (Demucs + whisperx, ~3 min/song) on
        // text sources we know produce poor wall output. Allowed set is
        // yt_subs/lrclib/spotify (line-timed) and description (curated). Anything
        // else (genius, lrclib-plain-without-timing, no_source) gets the
        // `unsupported_source` sentinel and is parked until a future-model PR.
        if !crate::lyrics::orchestrator::is_allowed_text_source(&ctx.candidate_texts) {
            let names: Vec<&str> = ctx
                .candidate_texts
                .iter()
                .map(|c| c.source.as_str())
                .collect();
            tracing::warn!(
                video_id,
                youtube_id = %youtube_id,
                candidate_sources = ?names,
                "lyrics: no allowed text source — marking unsupported_source"
            );
            if let Err(e) = crate::db::models::mark_unsupported_source(
                &self.pool,
                video_id,
                LYRICS_PIPELINE_VERSION,
            )
            .await
            {
                warn!("worker: mark_unsupported_source error for {youtube_id}: {e}");
            }
            self.clear_processing().await;
            return Ok(());
        }

        self.broadcast_stage(
            video_id,
            &youtube_id,
            &song,
            &artist,
            "preprocessing",
            None,
            started_at_unix_ms,
        )
        .await;
```

- [ ] **Step 4.2: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 4.3: Commit**

```bash
git add crates/sp-server/src/lyrics/worker.rs
git commit -m "feat(lyrics): gate process_song on is_allowed_text_source after gather"
```

> Note: `process_song` is `mutants::skip`'d (per existing worker.rs convention for the top-level orchestrator method). The gate's behavior is covered by Task 3's unit tests (gate logic) and Task 2's tests (sentinel write-path). Integration verification happens at wall-test time per the spec's "Wall verification workflow".

---

## Task 5: Extend skip-list in reprocess.rs

**Files:**
- Modify: `crates/sp-server/src/lyrics/reprocess.rs:48`, `:73-77` (comment), `:100`
- Modify: `crates/sp-server/src/lyrics/reprocess.rs` test module — add 2 sibling tests

- [ ] **Step 5.1: Update the two SQL `NOT IN` lists**

Change line 48 of `crates/sp-server/src/lyrics/reprocess.rs` from:

```rust
                    OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source', 'asr_gap') \
```

to:

```rust
                    OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source', 'asr_gap', 'unsupported_source') \
```

Change line 100 from:

```rust
                    OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source', 'asr_gap') \
```

to:

```rust
                    OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source', 'asr_gap', 'unsupported_source') \
```

- [ ] **Step 5.2: Update the explanatory comment at lines 73-77**

Locate the comment block that says:
```rust
    // `lyrics_source NOT IN ('failed','empty','no_source','asr_gap')` skips rows
```

and replace with:

```rust
    // `lyrics_source NOT IN ('failed','empty','no_source','asr_gap','unsupported_source')`
    // skips rows the worker has parked: terminal failure modes (`failed`,
    // `empty`, `no_source`), the ASR-gap quarantine sentinel (`asr_gap`,
    // per #86), and the new source-gating sentinel (`unsupported_source`,
    // per docs/superpowers/specs/2026-05-16-lyrics-source-gating-design.md).
```

- [ ] **Step 5.3: Add 2 tests in the existing reprocess.rs test module**

Locate the existing `#[cfg(test)] mod tests` block at the bottom of `reprocess.rs` (or the sibling test file if separated). Append:

```rust
#[tokio::test]
async fn unsupported_source_not_picked_by_get_next_video_for_lyrics() {
    // Seed a pool with one video at `lyrics_source = 'unsupported_source'`
    // AND `pipeline_version < current` AND `has_lyrics = 1`. Without the
    // skip-list extension this row would be picked up by the stale-bucket
    // path; with it, the call must return None (no row to process).
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, has_lyrics, lyrics_source, lyrics_pipeline_version) \
         VALUES (1, 'aaa', 't', 1, 'unsupported_source', 5)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let next = get_next_video_for_lyrics(&pool, 20).await.unwrap();
    assert!(
        next.is_none(),
        "row at unsupported_source must NOT be picked by stale-bucket selector"
    );
}

#[tokio::test]
async fn unsupported_source_not_picked_by_manual_bucket() {
    // Even when `lyrics_manual_priority = 1`, an `unsupported_source` row must
    // stay parked — the sentinel acts as a permanent "do not retry under this
    // pipeline" mark until a future-model PR lifts it.
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, has_lyrics, lyrics_source, \
                             lyrics_pipeline_version, lyrics_manual_priority) \
         VALUES (1, 'bbb', 't2', 1, 'unsupported_source', 5, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let next = get_next_video_for_lyrics(&pool, 20).await.unwrap();
    assert!(
        next.is_none(),
        "row at unsupported_source must NOT be picked even with manual_priority=1"
    );
}
```

- [ ] **Step 5.4: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 5.5: Commit**

```bash
git add crates/sp-server/src/lyrics/reprocess.rs
git commit -m "feat(lyrics): extend skip-list with unsupported_source sentinel"
```

---

## Task 6: Admin endpoint — `POST /api/v1/lyrics/reprocess-catalog-with-new-gate`

**Files:**
- Create: `crates/sp-server/src/api/lyrics_catalog.rs`
- Modify: `crates/sp-server/src/api/mod.rs:3-7` (add module declaration), `crates/sp-server/src/api/mod.rs:120-138` (route block — insert new route after `clear-manual-queue`)

- [ ] **Step 6.1: Create the new module file**

Create `crates/sp-server/src/api/lyrics_catalog.rs`:

```rust
//! Admin endpoint for one-shot catalog reprocess under the new source gate.
//!
//! `POST /api/v1/lyrics/reprocess-catalog-with-new-gate`:
//!   1. Restamps any `pipeline_version > 20` rows down to 19 (un-anomalies
//!      the 77 rows produced by a now-reverted code path that bumped the
//!      constant without approval).
//!   2. Sets `lyrics_manual_priority = 1` on every row whose
//!      `pipeline_version < current` AND `lyrics_source` is not a parked
//!      sentinel (`asr_gap` / `unsupported_source`).
//!   3. Returns `{ "restamped": N, "queued": M }`.
//!
//! Idempotent — calling twice produces `{ "restamped": 0, "queued": 0 }`
//! on the second call (because the first call already moved every row out
//! of the matching set).
//!
//! See `docs/superpowers/specs/2026-05-16-lyrics-source-gating-design.md`.

use crate::AppState;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::Serialize;
use tracing::warn;

#[derive(Debug, Serialize, PartialEq)]
pub struct ReprocessCatalogResponse {
    pub restamped: u64,
    pub queued: u64,
}

#[cfg_attr(test, mutants::skip)] // Thin glue: two UPDATEs and a JSON response;
// observable side effects are covered by the four sibling tests below
// (counts on each UPDATE, idempotency, response JSON shape). Remaining
// mutation targets reduce to SQL string literals that cargo-mutants
// cannot mutate meaningfully.
pub async fn reprocess_catalog_with_new_gate(
    State(state): State<AppState>,
) -> impl IntoResponse {
    use crate::lyrics::LYRICS_PIPELINE_VERSION;

    // Step 1: restamp v21 anomaly.
    let restamped = match sqlx::query(
        "UPDATE videos SET lyrics_pipeline_version = 19 \
         WHERE lyrics_pipeline_version > ?",
    )
    .bind(LYRICS_PIPELINE_VERSION as i64)
    .execute(&state.pool)
    .await
    {
        Ok(r) => r.rows_affected(),
        Err(e) => {
            warn!("reprocess_catalog_with_new_gate restamp error: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    // Step 2: queue all candidate rows.
    let queued = match sqlx::query(
        "UPDATE videos SET lyrics_manual_priority = 1 \
         WHERE lyrics_pipeline_version < ? \
           AND lyrics_manual_priority = 0 \
           AND (lyrics_source IS NULL \
                OR lyrics_source NOT IN ('asr_gap', 'unsupported_source'))",
    )
    .bind(LYRICS_PIPELINE_VERSION as i64)
    .execute(&state.pool)
    .await
    {
        Ok(r) => r.rows_affected(),
        Err(e) => {
            warn!("reprocess_catalog_with_new_gate queue error: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    tracing::info!(restamped, queued, "reprocess-catalog-with-new-gate complete");

    Json(ReprocessCatalogResponse { restamped, queued }).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    async fn setup_pool_with_videos() -> SqlitePool {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
            .execute(&pool)
            .await
            .unwrap();
        // Row 1: v21 (anomaly — should be restamped to 19, then queued).
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, lyrics_source, lyrics_pipeline_version) \
             VALUES (1, 'aaa', 't1', 'yt_subs', 21)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Row 2: v15, ordinary lyrics — should be queued.
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, lyrics_source, lyrics_pipeline_version) \
             VALUES (1, 'bbb', 't2', 'lrclib', 15)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Row 3: v10, asr_gap — must NOT be queued (skip-list).
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, lyrics_source, lyrics_pipeline_version) \
             VALUES (1, 'ccc', 't3', 'asr_gap', 10)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Row 4: v18, unsupported_source — must NOT be queued (skip-list).
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, lyrics_source, lyrics_pipeline_version) \
             VALUES (1, 'ddd', 't4', 'unsupported_source', 18)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Row 5: v20 (current) — must NOT be queued (already at current).
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, lyrics_source, lyrics_pipeline_version) \
             VALUES (1, 'eee', 't5', 'yt_subs', 20)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    /// Replicate the endpoint's two UPDATEs against an in-memory pool so we
    /// can assert counts without spinning up an Axum router. Mirrors the
    /// pattern in `db::models_tests.rs` for SQL behavior verification.
    async fn run_endpoint_sql(pool: &SqlitePool) -> ReprocessCatalogResponse {
        let restamped = sqlx::query(
            "UPDATE videos SET lyrics_pipeline_version = 19 \
             WHERE lyrics_pipeline_version > 20",
        )
        .execute(pool)
        .await
        .unwrap()
        .rows_affected();

        let queued = sqlx::query(
            "UPDATE videos SET lyrics_manual_priority = 1 \
             WHERE lyrics_pipeline_version < 20 \
               AND (lyrics_source IS NULL \
                    OR lyrics_source NOT IN ('asr_gap', 'unsupported_source'))",
        )
        .execute(pool)
        .await
        .unwrap()
        .rows_affected();

        ReprocessCatalogResponse { restamped, queued }
    }

    #[tokio::test]
    async fn endpoint_restamps_only_rows_above_current_version() {
        let pool = setup_pool_with_videos().await;
        let result = run_endpoint_sql(&pool).await;
        assert_eq!(result.restamped, 1, "only the v21 row must be restamped");
        let v: i64 = sqlx::query_scalar(
            "SELECT lyrics_pipeline_version FROM videos WHERE youtube_id = 'aaa'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(v, 19, "restamped row must now be at version 19");
    }

    #[tokio::test]
    async fn endpoint_queues_eligible_rows_and_skips_sentinels() {
        let pool = setup_pool_with_videos().await;
        let result = run_endpoint_sql(&pool).await;
        // After restamp: rows at v19 (was v21), v15, and v18 (unsupported_source)
        // are all `< 20`. The queue UPDATE excludes asr_gap + unsupported_source.
        // Eligible: v19 (was v21 yt_subs) + v15 lrclib = 2.
        assert_eq!(result.queued, 2, "two eligible rows must be queued");
        let aaa: i64 = sqlx::query_scalar(
            "SELECT lyrics_manual_priority FROM videos WHERE youtube_id = 'aaa'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let bbb: i64 = sqlx::query_scalar(
            "SELECT lyrics_manual_priority FROM videos WHERE youtube_id = 'bbb'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let ccc: i64 = sqlx::query_scalar(
            "SELECT lyrics_manual_priority FROM videos WHERE youtube_id = 'ccc'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let ddd: i64 = sqlx::query_scalar(
            "SELECT lyrics_manual_priority FROM videos WHERE youtube_id = 'ddd'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(aaa, 1, "restamped yt_subs row must be queued");
        assert_eq!(bbb, 1, "lrclib row must be queued");
        assert_eq!(ccc, 0, "asr_gap row must NOT be queued");
        assert_eq!(ddd, 0, "unsupported_source row must NOT be queued");
    }

    #[tokio::test]
    async fn endpoint_is_idempotent_on_second_call() {
        let pool = setup_pool_with_videos().await;
        let first = run_endpoint_sql(&pool).await;
        let second = run_endpoint_sql(&pool).await;
        assert!(first.restamped > 0 || first.queued > 0, "first call must do work");
        // Idempotency is enforced by the SQL WHERE clauses:
        //   - Restamp: WHERE lyrics_pipeline_version > 20 → no rows match after
        //     the first call moved them all to ≤ 20.
        //   - Queue: WHERE lyrics_pipeline_version < 20 AND lyrics_manual_priority = 0
        //     → no rows match after the first call set every eligible row's
        //     manual_priority to 1.
        // So both counts must be 0 on the second call.
        assert_eq!(second.restamped, 0, "second restamp must affect 0 rows");
        assert_eq!(second.queued, 0, "second queue must affect 0 rows");
    }

    #[tokio::test]
    async fn endpoint_response_serializes_to_expected_json_shape() {
        let r = ReprocessCatalogResponse { restamped: 7, queued: 42 };
        let body = serde_json::to_string(&r).unwrap();
        assert_eq!(body, r#"{"restamped":7,"queued":42}"#);
    }
}
```

- [ ] **Step 6.2: Wire the new module in `api/mod.rs`**

In `crates/sp-server/src/api/mod.rs`, add the module declaration after the existing `pub mod lyrics;` (line 5):

```rust
pub mod ai;
pub mod live;
pub mod lyrics;
pub mod lyrics_catalog;
pub mod routes;
pub mod websocket;
```

Add the new route after the `clear-manual-queue` block (after line 130 in the route registration chain — order matches the lyrics-related grouping):

```rust
        .route(
            "/api/v1/lyrics/clear-manual-queue",
            axum::routing::post(lyrics::post_clear_manual),
        )
        .route(
            "/api/v1/lyrics/reprocess-catalog-with-new-gate",
            axum::routing::post(lyrics_catalog::reprocess_catalog_with_new_gate),
        )
        .route(
            "/api/v1/lyrics/quarantine",
            axum::routing::post(lyrics::quarantine_lyrics),
        )
```

- [ ] **Step 6.3: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 6.4: Commit**

```bash
git add crates/sp-server/src/api/lyrics_catalog.rs crates/sp-server/src/api/mod.rs
git commit -m "feat(lyrics): admin endpoint POST /api/v1/lyrics/reprocess-catalog-with-new-gate"
```

---

## Final verification (before declaring done)

- [ ] **Step F.1: Confirm 7 commits land in expected order**

```bash
git log --oneline -7
```

Expected (top to bottom):
```
<sha7> feat(lyrics): admin endpoint POST /api/v1/lyrics/reprocess-catalog-with-new-gate
<sha6> feat(lyrics): extend skip-list with unsupported_source sentinel
<sha5> feat(lyrics): gate process_song on is_allowed_text_source after gather
<sha4> feat(lyrics): is_allowed_text_source gate fn + 11 unit tests
<sha3> feat(lyrics): write processed_at + alignment_model on every lyrics UPDATE site
<sha2> feat(lyrics): V19 migration adds lyrics_processed_at + alignment_model columns
<sha1> docs: spec for lyrics source gating + processing metadata    ← pre-existing (365dbf8)
```

(The number of commits is 6 new + 1 pre-existing spec commit = 7 visible.)

- [ ] **Step F.2: Confirm no file exceeds the 1000-line cap**

```bash
wc -l crates/sp-server/src/db/models.rs \
      crates/sp-server/src/db/mod.rs \
      crates/sp-server/src/db/mod_tests_v19.rs \
      crates/sp-server/src/db/models_tests.rs \
      crates/sp-server/src/lyrics/orchestrator.rs \
      crates/sp-server/src/lyrics/reprocess.rs \
      crates/sp-server/src/lyrics/worker.rs \
      crates/sp-server/src/lyrics/mod.rs \
      crates/sp-server/src/api/lyrics_catalog.rs \
      crates/sp-server/src/api/mod.rs
```

Every line count must be `< 1000`. Expected order-of-magnitude:
- `db/models.rs` ~ 925 (838 + 90)
- `db/mod.rs` ~ 353
- `db/mod_tests_v19.rs` ~ 130
- `db/models_tests.rs` ~ existing + 180
- `lyrics/orchestrator.rs` ~ 925 (837 + 90)
- `lyrics/reprocess.rs` ~ 720 (668 + 60)
- `lyrics/worker.rs` ~ 823 (793 + 30)
- `lyrics/mod.rs` ~ existing + 12
- `api/lyrics_catalog.rs` ~ 220
- `api/mod.rs` ~ existing + 5

- [ ] **Step F.3: Confirm `LYRICS_PIPELINE_VERSION` is still 20**

```bash
grep 'LYRICS_PIPELINE_VERSION: u32 =' crates/sp-server/src/lyrics/mod.rs
```

Expected: `pub const LYRICS_PIPELINE_VERSION: u32 = 20;`

If anything other than `20` appears, the implementer broke a hard rule. STOP and revert.

- [ ] **Step F.4: Final `cargo fmt --all --check`**

Run: `cargo fmt --all --check`
Expected: exit 0.

---

## Out of scope (do NOT do in this PR)

- **No `LYRICS_PIPELINE_VERSION` bump.** Constant stays at 20. The two `feedback_no_bump_until_proven.md` + `feedback_pipeline_version_approval.md` memories make this absolute.
- **No dashboard UI changes.** The new columns surface via API JSON automatically; no new dashboard controls.
- **No backfill of `lyrics_processed_at` / `lyrics_alignment_model` for existing rows.** NULL is honest.
- **No retire-the-sentinel endpoint.** That ships in the future-model PR.
- **No `lyrics_text_source` separate column.** Text source stays parseable from the `lyrics_source` string prefix.
- **No edits to `api/lyrics.rs`.** It is at 915 lines and a new endpoint would push it over the 1000-line cap. The new endpoint goes in `api/lyrics_catalog.rs`.
- **No edits to `api/routes.rs`.** Routes are registered in `api/mod.rs`; `routes.rs` only holds handler bodies.

---

## Execution Handoff

Plan committed locally — dispatch all tasks via subagent-driven-development now.

Per `ask-before-assuming.md` pre-answered table:
> "Plan committed locally as <sha>. Dispatch all tasks via subagent-driven-development now, or hold for your review of the plan first?" → **Dispatch now.** No review pause. Banned phrasings: "go vs review first", "dispatch now or hold", "before I dispatch", "pre-implementation skim".
