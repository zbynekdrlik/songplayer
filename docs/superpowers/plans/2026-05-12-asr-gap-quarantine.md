# ASR-Gap Quarantine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `POST /api/v1/lyrics/quarantine` endpoint that parks songs whose ASR transcription is unrecoverable so the lyrics worker stops re-picking them and the wall stops showing broken karaoke.

**Architecture:** New DB helper `quarantine_video_lyrics` writes a fresh `asr_gap` sentinel to `lyrics_source` at the current `LYRICS_PIPELINE_VERSION`, deletes the cached `{youtube_id}_lyrics.json` file, and emits a structured `tracing::warn`. Two existing bucket SQL WHERE clauses in `reprocess.rs` get the new sentinel added to their `NOT IN (...)` skip-list. New HTTP handler exposes the helper. A future `LYRICS_PIPELINE_VERSION` bump automatically re-picks every `asr_gap` row via the existing `OR v.lyrics_pipeline_version < ?` exception — no new infrastructure.

**Tech Stack:** Rust 2024, axum 0.8, sqlx 0.8 (SQLite), tokio fs, tracing, tower::ServiceExt (handler integration tests).

**Spec:** `docs/superpowers/specs/2026-05-12-asr-gap-quarantine-design.md` (commit `f161053`).

---

## Per-implementer airuleset rules (verbatim, MUST obey)

- **TDD strict:** failing test first → trust by inspection → implement → trust by inspection → `cargo fmt --all --check` (the ONLY local cargo command allowed) → commit on green.
- **NEVER** run `cargo clippy`, `cargo test`, `cargo build`, or `cargo check` locally; rely on CI.
- **File-size cap 1000 lines** per file. Current sizes: `models.rs` 759 (+~55 = 814, OK); `models_tests.rs` exists as sibling and grows by ~110; `reprocess.rs` 606 (+~10 prod, +~95 test = 711, OK); `api/lyrics.rs` 408 (+~75 = 483, OK); `routes_tests.rs` grows by ~90.
- **One commit per "Commit" step** in this plan. This plan has 3 commits.
- `mutants::skip` requires inline justification.
- **Do NOT push.** Controller batches and pushes once after all 3 commits land.
- Per `feedback_pipeline_version_approval.md` AND `feedback_no_bump_until_proven.md`: DO NOT bump `LYRICS_PIPELINE_VERSION` (stays at 20).
- Per `feedback_take_ownership.md`: root-cause fix only — no band-aids, no parallel paths.
- No schema migration. No new columns. No new constants other than the inline `'asr_gap'` string literal.

---

## File Structure

| File | Change | LOC delta |
|---|---|---|
| `crates/sp-server/src/db/models.rs` | Add `QuarantineOutcome` struct + `pub async fn quarantine_video_lyrics`. | +55 prod |
| `crates/sp-server/src/db/models_tests.rs` | Add 3 sibling tests (sentinel+cache-delete, missing-cache, not-found). | +110 test |
| `crates/sp-server/src/lyrics/reprocess.rs` | Extend `NOT IN (...)` skip-list in `fetch_bucket_manual` (line 48) and `fetch_bucket_null` (line 94). Update comment at line 73-77. Add 3 inline tests. | +10 prod, +95 test |
| `crates/sp-server/src/api/lyrics.rs` | Add `QuarantineRequest`, `QuarantineResponse`, `pub async fn quarantine_lyrics` handler. | +75 prod |
| `crates/sp-server/src/api/mod.rs` | Wire `.route("/api/v1/lyrics/quarantine", post(lyrics::quarantine_lyrics))`. | +4 |
| `crates/sp-server/src/api/routes_tests.rs` | Add 2 integration tests using `test_state_with_cache_dir` + `tower::ServiceExt`. | +90 test |

All files stay under the 1000-line cap.

---

## Phase A: ASR-gap quarantine

### Task A.1: DB helper, reprocess skip-list, HTTP endpoint, wiring

**Files:**
- Modify: `crates/sp-server/src/db/models.rs`
- Modify: `crates/sp-server/src/db/models_tests.rs`
- Modify: `crates/sp-server/src/lyrics/reprocess.rs`
- Modify: `crates/sp-server/src/api/lyrics.rs`
- Modify: `crates/sp-server/src/api/mod.rs`
- Modify: `crates/sp-server/src/api/routes_tests.rs`

---

#### Part 1: DB helper `quarantine_video_lyrics`

- [ ] **Step 1: Write failing test 1 — sentinel set + cache file deleted**

Append to `crates/sp-server/src/db/models_tests.rs` (the file currently ends after the existing `mark_video_*` tests):

```rust
#[tokio::test]
async fn quarantine_video_lyrics_sets_sentinel_and_deletes_cache_file() {
    let (pool, id) = setup_with_video().await;
    // Pre-populate the row as a fully-processed song so we can prove
    // quarantine wipes it back to has_lyrics=0 with the asr_gap sentinel.
    sqlx::query(
        "UPDATE videos SET has_lyrics = 1, lyrics_source = 'ensemble:gemini', \
         lyrics_pipeline_version = 5, lyrics_manual_priority = 1 WHERE id = ?",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path();
    let cache_file = cache_dir.join("yt123_lyrics.json");
    tokio::fs::write(&cache_file, b"{\"version\":5,\"lines\":[]}")
        .await
        .unwrap();

    let outcome = quarantine_video_lyrics(&pool, id, cache_dir, "ASR missed bridge", 20)
        .await
        .unwrap();

    assert_eq!(outcome.youtube_id, "yt123");
    assert_eq!(outcome.previous_source.as_deref(), Some("ensemble:gemini"));
    assert!(outcome.deleted_cache_file);
    assert!(
        !cache_file.exists(),
        "cache file must be deleted so the wall stops showing broken karaoke"
    );

    let row = sqlx::query(
        "SELECT has_lyrics, lyrics_source, lyrics_pipeline_version, lyrics_manual_priority \
         FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("has_lyrics"), 0);
    assert_eq!(row.get::<String, _>("lyrics_source"), "asr_gap");
    assert_eq!(row.get::<i64, _>("lyrics_pipeline_version"), 20);
    assert_eq!(row.get::<i64, _>("lyrics_manual_priority"), 0);
}
```

- [ ] **Step 2: Verify test 1 fails by inspection**

The `quarantine_video_lyrics` function does not exist yet. Compilation fails with `cannot find function quarantine_video_lyrics in this scope`.

- [ ] **Step 3: Write failing test 2 — missing cache file handled**

Append to `crates/sp-server/src/db/models_tests.rs`:

```rust
#[tokio::test]
async fn quarantine_video_lyrics_handles_missing_cache_file() {
    let (pool, id) = setup_with_video().await;
    let tmp = tempfile::tempdir().unwrap();
    // No cache file written — quarantine must still succeed and the DB
    // update must still land. Operators should be able to quarantine
    // songs whose cache was already cleared by hand.
    let outcome = quarantine_video_lyrics(&pool, id, tmp.path(), "", 20)
        .await
        .unwrap();

    assert!(!outcome.deleted_cache_file);
    let source: String = sqlx::query_scalar("SELECT lyrics_source FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(source, "asr_gap");
}
```

- [ ] **Step 4: Verify test 2 fails by inspection**

Same compile error as Step 2.

- [ ] **Step 5: Write failing test 3 — missing video_id**

Append to `crates/sp-server/src/db/models_tests.rs`:

```rust
#[tokio::test]
async fn quarantine_video_lyrics_returns_not_found_for_missing_id() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let err = quarantine_video_lyrics(&pool, 999, tmp.path(), "", 20)
        .await
        .expect_err("expected NotFound for missing video_id");
    // Handler relies on this variant to map to 404; if the variant changes,
    // the integration test in routes_tests.rs will also catch the regression.
    assert!(
        matches!(err, sqlx::Error::RowNotFound),
        "must surface RowNotFound so the handler can return 404; got {err:?}"
    );
}
```

- [ ] **Step 6: Verify test 3 fails by inspection**

Same compile error as Step 2.

- [ ] **Step 7: Implement `QuarantineOutcome` + `quarantine_video_lyrics` in `models.rs`**

Append to `crates/sp-server/src/db/models.rs` BEFORE the trailing `#[path = "models_tests.rs"] #[cfg(test)] mod tests;` lines (which must remain the last items in the file):

```rust
/// Outcome of a successful `quarantine_video_lyrics` call. Surfaced through
/// the HTTP layer so operators can confirm what was changed.
pub struct QuarantineOutcome {
    pub youtube_id: String,
    pub previous_source: Option<String>,
    pub deleted_cache_file: bool,
}

/// Park a song with unrecoverable ASR transcription so the lyrics worker
/// stops re-picking it. Sets `lyrics_source = 'asr_gap'`, clears
/// `has_lyrics` and `lyrics_manual_priority`, stamps the current pipeline
/// version, and best-effort deletes the cached `{youtube_id}_lyrics.json`
/// file so the wall stops rendering whatever broken karaoke shipped
/// previously.
///
/// Future `LYRICS_PIPELINE_VERSION` bumps re-pick `asr_gap` rows
/// automatically via the existing `OR lyrics_pipeline_version < ?`
/// exception in `reprocess.rs::fetch_bucket_null` and
/// `fetch_bucket_manual` — no separate ASR-version constant is needed.
///
/// Returns `Err(sqlx::Error::RowNotFound)` when `video_id` does not exist;
/// the HTTP handler maps that variant to 404.
#[cfg_attr(test, mutants::skip)] // 3 integration tests below cover the
// happy path, missing-cache-file branch, and not-found branch. The body
// is one SELECT + one UPDATE + one fs::remove_file + a tracing line —
// every observable side effect is asserted by the tests, so mutation
// targets reduce to the test-equivalent SQL string literals that
// cargo-mutants cannot mutate meaningfully.
pub async fn quarantine_video_lyrics(
    pool: &SqlitePool,
    video_id: i64,
    cache_dir: &std::path::Path,
    reason: &str,
    current_pipeline_version: u32,
) -> Result<QuarantineOutcome, sqlx::Error> {
    let row = sqlx::query(
        "SELECT youtube_id, lyrics_source FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_optional(pool)
    .await?;
    let row = row.ok_or(sqlx::Error::RowNotFound)?;
    let youtube_id: String = row.get("youtube_id");
    let previous_source: Option<String> = row.try_get("lyrics_source").ok();

    sqlx::query(
        "UPDATE videos SET has_lyrics = 0, lyrics_source = 'asr_gap', \
         lyrics_pipeline_version = ?, lyrics_manual_priority = 0 WHERE id = ?",
    )
    .bind(current_pipeline_version as i64)
    .bind(video_id)
    .execute(pool)
    .await?;

    let cache_path = cache_dir.join(format!("{youtube_id}_lyrics.json"));
    let deleted_cache_file = tokio::fs::remove_file(&cache_path).await.is_ok();

    tracing::warn!(
        video_id,
        youtube_id = %youtube_id,
        reason = %reason,
        previous_source = ?previous_source,
        deleted_cache_file,
        "lyrics quarantined as asr_gap"
    );

    Ok(QuarantineOutcome {
        youtube_id,
        previous_source,
        deleted_cache_file,
    })
}
```

- [ ] **Step 8: Verify tests 1-3 pass by inspection**

- Test 1: row pre-populated with `lyrics_source='ensemble:gemini'`. SELECT returns the row, SQL UPDATE writes the four fields, `tokio::fs::remove_file` deletes the existing cache file (returns `Ok(())` → `is_ok()` is true). Assertions match.
- Test 2: row exists but no cache file on disk. SELECT/UPDATE land. `tokio::fs::remove_file` returns an `Err` (file not found) → `is_ok()` is false. `deleted_cache_file == false`. DB row still updated. Assertions match.
- Test 3: empty `videos` table. SELECT returns `None` → `ok_or(sqlx::Error::RowNotFound)` produces the expected error variant.

- [ ] **Step 9: `cargo fmt --all --check`**

Run: `cargo fmt --all --check`
Expected: exit 0, no output.

- [ ] **Step 10: Commit (DB helper)**

```bash
git add crates/sp-server/src/db/models.rs crates/sp-server/src/db/models_tests.rs
git commit -m "feat(lyrics): add quarantine_video_lyrics DB helper for asr_gap sentinel"
```

---

#### Part 2: Extend reprocess skip-list

- [ ] **Step 11: Write failing test 4 — null bucket skips `asr_gap` at current version**

Append inside the existing `#[cfg(test)] mod tests` block in `crates/sp-server/src/lyrics/reprocess.rs` (the block that ends near line 480 with the last `#[tokio::test]`):

```rust
#[tokio::test]
async fn null_bucket_skips_asr_gap_at_current_version() {
    // A row marked 'asr_gap' under the CURRENT pipeline version must be
    // parked: the worker has already determined the ASR cannot
    // transcribe this song under the current pipeline, and re-picking
    // it would just burn another Demucs+Gemini cycle.
    let pool = setup().await;
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
         lyrics_source, lyrics_pipeline_version, lyrics_manual_priority) VALUES \
             (1, 1, 'asr_gap_curr', 1, 0, 'asr_gap', 20, 0), \
             (2, 1, 'fresh',        1, 0, NULL,       0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let row = fetch_bucket_null(&pool, 20).await.unwrap().unwrap();
    assert_eq!(
        row.youtube_id, "fresh",
        "null bucket must skip asr_gap rows at the current pipeline version"
    );
}
```

- [ ] **Step 12: Write failing test 5 — null bucket picks `asr_gap` when version older**

Append inside the same `mod tests` block:

```rust
#[tokio::test]
async fn null_bucket_picks_asr_gap_when_pipeline_version_older() {
    // Proves the future-ASR-upgrade retry path is unbroken by the new
    // sentinel: when LYRICS_PIPELINE_VERSION is bumped (e.g. a better
    // ASR provider lands), every asr_gap row from the previous version
    // flows back through the null bucket for retry under the new ASR.
    let pool = setup().await;
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
         lyrics_source, lyrics_pipeline_version, lyrics_manual_priority) VALUES \
             (1, 1, 'asr_gap_old', 1, 0, 'asr_gap', 19, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let row = fetch_bucket_null(&pool, 20).await.unwrap();
    assert!(
        row.is_some() && row.as_ref().unwrap().youtube_id == "asr_gap_old",
        "older-version asr_gap rows must be re-picked when pipeline bumps"
    );
}
```

- [ ] **Step 13: Write failing test 6 — manual bucket skips `asr_gap` at current version**

Append inside the same `mod tests` block:

```rust
#[tokio::test]
async fn manual_bucket_skips_asr_gap_at_current_version() {
    // Mirrors `manual_bucket_skips_failed_songs_so_user_reprocess_does_not_loop`
    // for the new sentinel. Without this, a quarantined song with
    // manual_priority=1 (e.g. someone clicked Reprocess by mistake)
    // would loop on every worker tick.
    let pool = setup().await;
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
         lyrics_source, lyrics_pipeline_version, lyrics_manual_priority) VALUES \
             (1, 1, 'asr_gap_manual', 1, 0, 'asr_gap', 20, 1), \
             (2, 1, 'manual_retry',   1, 0, NULL,      0, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let row = get_next_video_for_lyrics(&pool, 20).await.unwrap().unwrap();
    assert_eq!(
        row.youtube_id, "manual_retry",
        "manual bucket must skip asr_gap rows at the current pipeline version"
    );
}
```

- [ ] **Step 14: Verify tests 4-6 fail by inspection**

The current `NOT IN ('failed', 'empty', 'no_source')` clauses do not include `'asr_gap'`, so:
- Test 4: both rows pass the WHERE clause; `ORDER BY RANDOM()` may pick either row. The test asserts the fresh row is picked, which can fail intermittently. (Actually the asr_gap row IS eligible → wrong answer → test must fail.)
- Test 5: the row should pass (pipeline_version<current) and the test would already pass without the fix because the row is currently eligible via the `pipeline_version < ?` clause. After the fix, the row is still picked (proves the exception still works). KEEP this test — it guards against accidentally adding `asr_gap` to the wrong filter spot.
- Test 6: same as test 4; the asr_gap row currently passes the manual-bucket filter, gets picked first by `ORDER BY v.id ASC`, fails the assertion.

- [ ] **Step 15: Extend `NOT IN` skip-list in `fetch_bucket_manual` and `fetch_bucket_null`**

Edit `crates/sp-server/src/lyrics/reprocess.rs`:

Line 48 (in `fetch_bucket_manual`):

```rust
                    OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source', 'asr_gap') \
```

Line 94 (in `fetch_bucket_null`):

```rust
                    OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source', 'asr_gap') \
```

Update the explanatory comment block at lines 73-77 (inside `fetch_bucket_null`) to mention the new sentinel. Replace this block:

```rust
    // `lyrics_source NOT IN ('failed','empty','no_source')` skips rows that the
    // worker has already tried and bailed on — without this filter a song with
    // zero text sources (no yt_subs, no LRCLIB match, no description/CCLI yet)
    // gets picked every 10s forever, blocking every other null-lyric song
    // behind it. Matches the pre-refactor guard in get_next_video_without_lyrics.
    // Exception: if a row's recorded failure is from an OLDER pipeline version,
    // allow it through — the worker may have new capability (e.g., a new
    // provider added in the version bump) that succeeds where prior runs failed.
```

with:

```rust
    // `lyrics_source NOT IN ('failed','empty','no_source','asr_gap')` skips rows
    // that the worker has already tried and bailed on — without this filter a
    // song with zero text sources (no yt_subs, no LRCLIB match, no description/
    // CCLI yet) gets picked every 10s forever, blocking every other null-lyric
    // song behind it. `asr_gap` is the operator-driven parking sentinel set by
    // POST /api/v1/lyrics/quarantine when a song's ASR transcription is
    // unrecoverable under the current pipeline; see
    // `db::models::quarantine_video_lyrics` and
    // `docs/superpowers/specs/2026-05-12-asr-gap-quarantine-design.md`.
    // Matches the pre-refactor guard in get_next_video_without_lyrics.
    // Exception: if a row's recorded failure is from an OLDER pipeline version,
    // allow it through — the worker may have new capability (e.g., a new
    // provider added in the version bump, or a new ASR backend that resolves
    // asr_gap rows) that succeeds where prior runs failed.
```

- [ ] **Step 16: Verify tests 4-6 pass by inspection**

- Test 4: `asr_gap_curr` has `lyrics_source='asr_gap'` and `lyrics_pipeline_version=20`. New WHERE clause: `lyrics_source IS NULL` is false; `NOT IN (..., 'asr_gap')` is false; `lyrics_pipeline_version < 20` is false (20 < 20 is false). Row excluded. Only `fresh` (with NULL source) survives → returned.
- Test 5: `asr_gap_old` has `lyrics_pipeline_version=19`. The `< ?` clause is true (19 < 20) → row eligible → returned.
- Test 6: Manual bucket SQL still has `lyrics_manual_priority=1` filter, plus the new `NOT IN` clause excludes `asr_gap_manual` at version 20. `manual_retry` (with NULL source) passes. `ORDER BY v.id ASC LIMIT 1` returns `asr_gap_manual` first… wait — the asr_gap row is EXCLUDED, so `manual_retry` is the only candidate. Returned.

- [ ] **Step 17: `cargo fmt --all --check`**

Run: `cargo fmt --all --check`
Expected: exit 0, no output.

- [ ] **Step 18: Commit (reprocess skip-list)**

```bash
git add crates/sp-server/src/lyrics/reprocess.rs
git commit -m "feat(lyrics): extend null+manual bucket skip-list with asr_gap sentinel"
```

---

#### Part 3: HTTP endpoint + route wiring + integration tests

- [ ] **Step 19: Write failing test 7 — endpoint marks row, returns outcome, deletes cache file**

Append inside the existing `#[cfg(test)] mod tests` block in `crates/sp-server/src/api/routes_tests.rs` (after the last existing `#[tokio::test]` fn). Use the existing `test_state_with_cache_dir` helper:

```rust
#[tokio::test]
async fn quarantine_endpoint_marks_row_and_deletes_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().to_path_buf();
    let state = test_state_with_cache_dir(cache_dir.clone()).await;

    // Seed a playlist + a video row + a fake lyrics cache file so we can
    // prove the endpoint deletes the file and writes the sentinel.
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'p', 'u', 'n', 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
         lyrics_source, lyrics_pipeline_version) VALUES \
             (42, 1, 'ytQUAR', 1, 1, 'ensemble:gemini', 20)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let cache_file = cache_dir.join("ytQUAR_lyrics.json");
    tokio::fs::write(&cache_file, b"{\"version\":20,\"lines\":[]}")
        .await
        .unwrap();

    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lyrics/quarantine")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "video_id": 42,
                        "reason": "ASR missed bridge"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["video_id"], 42);
    assert_eq!(json["youtube_id"], "ytQUAR");
    assert_eq!(json["previous_source"], "ensemble:gemini");
    assert_eq!(json["deleted_cache_file"], true);

    assert!(!cache_file.exists(), "cache file must be gone");
    let source: String =
        sqlx::query_scalar("SELECT lyrics_source FROM videos WHERE id = 42")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(source, "asr_gap");
}
```

- [ ] **Step 20: Write failing test 8 — endpoint returns 404 for missing video_id**

Append inside the same `mod tests` block:

```rust
#[tokio::test]
async fn quarantine_endpoint_returns_404_for_missing_video() {
    let tmp = tempfile::tempdir().unwrap();
    let state = test_state_with_cache_dir(tmp.path().to_path_buf()).await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lyrics/quarantine")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"video_id": 999, "reason": ""}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
```

- [ ] **Step 21: Verify tests 7-8 fail by inspection**

The route `/api/v1/lyrics/quarantine` is not registered in `api/mod.rs`. Both requests get 404 from the router's fallback, but:
- Test 7 expects 200 with a JSON body → fails on `assert_eq!(resp.status(), StatusCode::OK)`.
- Test 8 expects 404 → already 404 by fallback, so this test passes accidentally. We must add the test now (before implementing) anyway: once the handler exists, the fallback 404 changes into a handler-produced 404 and the assertion still holds. Keep the test as-is — it locks the contract.

- [ ] **Step 22: Add `QuarantineRequest`, `QuarantineResponse`, and the `quarantine_lyrics` handler to `api/lyrics.rs`**

Append to `crates/sp-server/src/api/lyrics.rs` BEFORE the trailing `#[cfg(test)] mod tests { ... }` block (the inline test module must remain the last item in the file):

```rust
/// Request body for `POST /api/v1/lyrics/quarantine`.
///
/// `reason` is optional free-text; it is logged via `tracing::warn` for an
/// audit trail but never persisted to the DB. Keeping it out of the schema
/// avoids a migration. See
/// `docs/superpowers/specs/2026-05-12-asr-gap-quarantine-design.md`.
#[derive(Debug, Deserialize)]
pub struct QuarantineRequest {
    pub video_id: i64,
    #[serde(default)]
    pub reason: String,
}

/// Response body for `POST /api/v1/lyrics/quarantine`. `previous_source` is
/// JSON `null` when the row had `lyrics_source IS NULL` before the call.
#[derive(Debug, Serialize)]
pub struct QuarantineResponse {
    pub video_id: i64,
    pub youtube_id: String,
    pub previous_source: Option<String>,
    pub deleted_cache_file: bool,
}

/// POST /api/v1/lyrics/quarantine
///
/// Park a song with unrecoverable ASR transcription. Sets the row's
/// `lyrics_source` to `'asr_gap'`, clears `has_lyrics` and
/// `lyrics_manual_priority`, stamps the current `LYRICS_PIPELINE_VERSION`,
/// and best-effort deletes the cached `_lyrics.json` file. See
/// `db::models::quarantine_video_lyrics` for the DB-level contract.
#[cfg_attr(test, mutants::skip)] // Thin glue: parse request → call helper →
// map RowNotFound to 404 → wrap outcome in 200 JSON. Both branches plus
// the JSON shape are covered by `quarantine_endpoint_marks_row_and_deletes_cache`
// and `quarantine_endpoint_returns_404_for_missing_video` in routes_tests.rs.
pub async fn quarantine_lyrics(
    State(state): State<crate::AppState>,
    Json(req): Json<QuarantineRequest>,
) -> impl IntoResponse {
    use crate::lyrics::LYRICS_PIPELINE_VERSION;
    match crate::db::models::quarantine_video_lyrics(
        &state.pool,
        req.video_id,
        &state.cache_dir,
        &req.reason,
        LYRICS_PIPELINE_VERSION,
    )
    .await
    {
        Ok(outcome) => Json(QuarantineResponse {
            video_id: req.video_id,
            youtube_id: outcome.youtube_id,
            previous_source: outcome.previous_source,
            deleted_cache_file: outcome.deleted_cache_file,
        })
        .into_response(),
        Err(sqlx::Error::RowNotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            warn!("quarantine_lyrics error for video {}: {e}", req.video_id);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}
```

- [ ] **Step 23: Wire the route in `api/mod.rs`**

Edit `crates/sp-server/src/api/mod.rs`. Insert the following `.route(...)` call between the existing `/api/v1/lyrics/clear-manual-queue` route (around line 127-130) and the `// WebSocket` comment line (around line 131). The new block:

```rust
        .route(
            "/api/v1/lyrics/quarantine",
            axum::routing::post(lyrics::quarantine_lyrics),
        )
```

After this edit the surrounding context looks like:

```rust
        .route(
            "/api/v1/lyrics/clear-manual-queue",
            axum::routing::post(lyrics::post_clear_manual),
        )
        .route(
            "/api/v1/lyrics/quarantine",
            axum::routing::post(lyrics::quarantine_lyrics),
        )
        // WebSocket
        .route("/api/v1/ws", axum::routing::get(websocket::ws_handler))
```

- [ ] **Step 24: Verify tests 7-8 pass by inspection**

- Test 7: POST to the now-registered route hits `quarantine_lyrics`, which calls `quarantine_video_lyrics` with `(pool, 42, cache_dir, "ASR missed bridge", 20)`. The DB row exists, SELECT returns `('ytQUAR', 'ensemble:gemini')`, UPDATE writes the four fields, cache file deletes successfully. Handler wraps outcome in `QuarantineResponse` with `video_id=42, youtube_id="ytQUAR", previous_source=Some("ensemble:gemini"), deleted_cache_file=true`. JSON body matches all assertions. DB `lyrics_source` is now `'asr_gap'`. Cache file gone.
- Test 8: POST with `video_id=999`. SELECT returns `None`, helper returns `Err(sqlx::Error::RowNotFound)`. Handler matches that variant and returns `StatusCode::NOT_FOUND`. Assertion passes.

- [ ] **Step 25: `cargo fmt --all --check`**

Run: `cargo fmt --all --check`
Expected: exit 0, no output.

- [ ] **Step 26: Commit (endpoint + wiring)**

```bash
git add crates/sp-server/src/api/lyrics.rs crates/sp-server/src/api/mod.rs crates/sp-server/src/api/routes_tests.rs
git commit -m "feat(lyrics): POST /api/v1/lyrics/quarantine endpoint for asr_gap parking"
```

---

## Verification (controller-only, after all 3 commits land)

1. **Push:** `git push origin dev`.
2. **CI monitor:** poll the latest run until terminal state. All jobs MUST be green: Test Integrity Check, Test, Lint, Test WASM, File Size Check, Security Audit, Dev Version Check, Build WASM, Build (Windows), Coverage, Frontend E2E, Build Tauri, Gate, Deploy to win-resolume, E2E Tests (win-resolume).
3. **Wall-verify:** on win-resolume, with id=233 Saints currently playing the broken Gemini karaoke:
   ```
   curl -X POST http://10.77.9.201:8920/api/v1/lyrics/quarantine \
     -H 'content-type: application/json' \
     -d '{"video_id":233,"reason":"ASR missed bridge 1:54-2:10"}'
   ```
   Expected response: `200` with `{"video_id":233,"youtube_id":"BW_vUblj_RA","previous_source":"<whatever>","deleted_cache_file":true}`.
4. **Wall observation:** karaoke overlay disappears (renderer falls back to no-lyrics) within one playback poll. Cache file `C:\ProgramData\SongPlayer\cache\BW_vUblj_RA_lyrics.json` must be gone.
5. **Queue smoke:** `curl http://10.77.9.201:8920/api/v1/lyrics/queue` shows id=233 NOT in any bucket count delta.
6. **Open PR** dev → main once CI is fully green and wall-verified.

---

## Execution Handoff

**Plan complete and saved to `docs/superpowers/plans/2026-05-12-asr-gap-quarantine.md`.**

Dispatching via **superpowers:subagent-driven-development** (pre-answered per airuleset: subagent-driven, dispatch now, no review pause).
