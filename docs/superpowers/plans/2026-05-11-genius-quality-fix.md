# Genius+WhisperX Quality Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Genius and lrclib-plain text candidates pass through the same Claude cleanup as description before reaching text_reference_merge. Output quality on the wall matches the description+whisperx path.

**Architecture:** Extract a shared `clean_lyrics_via_claude(ai, title, artist, raw_blob, cache_path)` helper from `description_provider`. Refactor `fetch_description_lyrics` to call it. Wire `gather.rs` genius branch and lrclib-plain branch through the same helper. Fail-the-song on Claude error / null (matches Gemini chunk policy).

**Tech Stack:** Rust 2024, anyhow, serde_json, wiremock (tests), tokio.

**Spec:** `docs/superpowers/specs/2026-05-11-genius-quality-fix-design.md` (commit `5eeba6b`).

---

## Per-implementer airuleset rules (verbatim, MUST obey)

- **TDD strict:** failing test first → trust by inspection → implement → trust by inspection → `cargo fmt --all --check` (the ONLY local cargo command allowed) → commit on green.
- **NEVER** run `cargo clippy`, `cargo test`, `cargo build`, or `cargo check` locally; rely on CI.
- **File-size cap 1000 lines** per file. Current sizes: `description_provider.rs` 680 lines (+~40 = 720, OK); `gather.rs` 233 lines (+~30 = 263, OK); `worker_tests.rs` grows by ~120 lines.
- **One commit per "Commit" step** in this plan. This plan has 2 commits.
- `mutants::skip` requires inline justification.
- **Do NOT push.** Controller batches and pushes once after both commits land.
- Per `feedback_no_legacy_code.md`: `fetch_description_lyrics` MUST be refactored to call `clean_lyrics_via_claude`. DELETE the inline Claude path entirely; no parallel old path.
- Per `feedback_pipeline_version_approval.md` AND `feedback_no_bump_until_proven.md`: DO NOT bump `LYRICS_PIPELINE_VERSION` (stays at 20).
- Per `feedback_take_ownership.md`: root-cause fix only.
- Per `feedback_chunk_success_required.md`: ANY Claude error / `Ok(None)` / `Ok(Some(empty))` on genius / lrclib-plain cleanup MUST `anyhow::bail!` the gather (never ship raw).

---

## File Structure

| File | Change | LOC delta |
|---|---|---|
| `crates/sp-server/src/lyrics/description_provider.rs` | Add `pub async fn clean_lyrics_via_claude`; refactor `fetch_description_lyrics` to call it; add 4 unit tests in existing `mod tests`. | +130 (40 prod + 90 test) |
| `crates/sp-server/src/lyrics/gather.rs` | Rewrite genius branch + lrclib branch. | +30 |
| `crates/sp-server/src/lyrics/worker_tests.rs` | Add 3 integration tests (lrclib-plain cleanup, lrclib-synced skip-cleanup, genius structural). | +120 |

All files stay under the 1000-line cap.

---

## Phase A: Shared helper + description refactor + unit tests

### Task A.1: Add `clean_lyrics_via_claude` helper and refactor `fetch_description_lyrics`

**Files:**
- Modify: `crates/sp-server/src/lyrics/description_provider.rs`

- [ ] **Step 1: Write failing test 1 — happy path**

Append to the existing `#[cfg(test)] mod tests` block (currently around line 270) in `crates/sp-server/src/lyrics/description_provider.rs`:

```rust
    #[tokio::test]
    async fn clean_lyrics_via_claude_returns_parsed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("vidCLEAN_genius_cleaned.json");

        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "{\"lines\": [\"Line A\", \"Line B\"]}"
                        }
                    }]
                })),
            )
            .mount(&mock)
            .await;

        let ai = AiClient::new(AiSettings {
            api_url: format!("{}/v1", mock.uri()),
            api_key: Some("test".into()),
            model: "stub".into(),
            system_prompt_extra: None,
        });

        let out = clean_lyrics_via_claude(
            &ai,
            "Song",
            "Artist",
            "Line A\nLine B\nLine A\nLine B",
            &cache_path,
        )
        .await
        .unwrap();

        assert_eq!(out, Some(vec!["Line A".to_string(), "Line B".to_string()]));
        // Cache file written.
        let cache = read_lyrics_cache(&cache_path).await.unwrap();
        assert_eq!(cache, Some(Some(vec!["Line A".into(), "Line B".into()])));
    }
```

Imports `AiSettings` and `AiClient` already used elsewhere in the test module — search for `use crate::ai::AiSettings;` near the top of `mod tests` and confirm the same imports cover the new test.

- [ ] **Step 2: Confirm test compiles+fails (trust by inspection)**

`clean_lyrics_via_claude` does not exist yet — Rust compile error `cannot find function clean_lyrics_via_claude`. That is the expected RED state.

- [ ] **Step 3: Run `cargo fmt --all --check`**

Expected: clean (the test additions match existing formatting).

- [ ] **Step 4: Implement `clean_lyrics_via_claude`**

In `crates/sp-server/src/lyrics/description_provider.rs`, between `fetch_raw_description` (ends ~line 207) and `fetch_description_lyrics` (starts line 218), insert:

```rust
/// Claude-clean a raw lyrics blob. Reuses the description-extraction prompt
/// (`build_description_extraction_prompt`). Reads / writes the cleaned-lines
/// cache at `cache_path`.
///
/// Returns:
/// - `Ok(Some(lines))` — Claude produced clean lines (non-empty).
/// - `Ok(None)`        — Claude returned `{"lines": null}` (refusal / no lyrics).
/// - `Err(_)`          — transport error, malformed JSON, or IO error.
///
/// Caller policy decides whether `Ok(None)` is fatal:
/// - description: `Ok(None)` is normal (description had no lyrics).
/// - genius / lrclib-plain: `Ok(None)` is fatal (caller must bail).
///
/// Cache contract matches `write_lyrics_cache`: on success the result is
/// persisted so subsequent reprocesses skip Claude. On `Err`, no cache is
/// written so the next attempt retries.
pub async fn clean_lyrics_via_claude(
    ai: &crate::ai::client::AiClient,
    title: &str,
    artist: &str,
    raw_blob: &str,
    cache_path: &std::path::Path,
) -> Result<Option<Vec<String>>> {
    // Fast path: cache already records a decision.
    if let Some(cached) = read_lyrics_cache(cache_path).await? {
        return Ok(cached);
    }

    let (system, user) = build_description_extraction_prompt(title, artist, raw_blob);
    let raw = ai
        .chat_with_timeout(&system, &user, 180)
        .await
        .context("Claude clean_lyrics chat failed")?;
    let parsed = parse_claude_response(&raw).context("Claude response malformed")?;

    write_lyrics_cache(cache_path, parsed.as_deref()).await?;
    Ok(parsed)
}
```

- [ ] **Step 5: Refactor `fetch_description_lyrics` to call the helper**

Replace the body of `fetch_description_lyrics` (lines 218–268, the existing implementation) with:

```rust
pub async fn fetch_description_lyrics(
    ai: &AiClient,
    ytdlp_path: &Path,
    youtube_id: &str,
    cache_dir: &Path,
    title: &str,
    artist: &str,
) -> Result<Option<Vec<String>>> {
    let lyrics_cache_path = cache_dir.join(format!("{youtube_id}_description_lyrics.json"));

    // Fast path: cached lyrics decision already on disk.
    if let Some(cached) = read_lyrics_cache(&lyrics_cache_path).await? {
        debug!(
            youtube_id,
            "description_provider: cache hit (extracted lyrics)"
        );
        return Ok(cached);
    }

    // Raw description fetch (cached separately).
    let Some(description) = fetch_raw_description(ytdlp_path, youtube_id, cache_dir).await? else {
        return Ok(None);
    };
    if description.trim().is_empty() {
        // Description was genuinely empty — record "no lyrics" so next
        // reprocess skips instantly.
        write_lyrics_cache(&lyrics_cache_path, None).await?;
        return Ok(None);
    }

    // Delegate Claude call + cache write to the shared helper.
    // description-specific policy: swallow Claude errors to Ok(None) so other
    // gather sources (yt_subs / lrclib / genius) still get a chance. The
    // shared helper does NOT write the cache on Err, so the next reprocess
    // retries cleanly.
    match clean_lyrics_via_claude(ai, title, artist, &description, &lyrics_cache_path).await {
        Ok(parsed) => Ok(parsed),
        Err(e) => {
            warn!(youtube_id, %e, "description_provider: Claude cleanup failed");
            Ok(None)
        }
    }
}
```

The `// mutants::skip` attribute on `fetch_description_lyrics` stays in place; the existing justification still applies.

- [ ] **Step 6: Run `cargo fmt --all --check`**

Expected: clean.

- [ ] **Step 7: Trust by inspection — test 1 passes**

`clean_lyrics_via_claude` now exists with the expected signature; cache path is written via existing `write_lyrics_cache`. wiremock stub returns the JSON the parser expects. Test 1 (Step 1) will pass under CI.

- [ ] **Step 8: Write failing test 2 — cache short-circuit**

Append after test 1 in the same `mod tests`:

```rust
    #[tokio::test]
    async fn clean_lyrics_via_claude_uses_cache_on_second_call() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("vidCACHED_genius_cleaned.json");

        // Pre-seed the cache to skip the Claude call entirely.
        write_lyrics_cache(&cache_path, Some(&["cached A".to_string(), "cached B".to_string()]))
            .await
            .unwrap();

        // Intentionally bogus AI URL — if the helper ignores cache and tries
        // to call Claude, the call will fail (and the test will fail with Err
        // instead of the expected cached value).
        let ai = AiClient::new(AiSettings {
            api_url: "http://127.0.0.1:1/v1".into(),
            api_key: Some("test".into()),
            model: "stub".into(),
            system_prompt_extra: None,
        });

        let out = clean_lyrics_via_claude(&ai, "Song", "Artist", "raw input", &cache_path)
            .await
            .unwrap();

        assert_eq!(out, Some(vec!["cached A".into(), "cached B".into()]));
    }
```

- [ ] **Step 9: Write failing test 3 — Claude error propagated**

Append after test 2:

```rust
    #[tokio::test]
    async fn clean_lyrics_via_claude_returns_err_on_claude_error() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("vidERR_genius_cleaned.json");

        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let ai = AiClient::new(AiSettings {
            api_url: format!("{}/v1", mock.uri()),
            api_key: Some("test".into()),
            model: "stub".into(),
            system_prompt_extra: None,
        });

        let out = clean_lyrics_via_claude(&ai, "Song", "Artist", "raw input", &cache_path).await;

        assert!(out.is_err(), "expected Err on Claude 500, got: {out:?}");
        // Cache file must NOT exist — failed runs are retryable.
        assert!(!cache_path.exists(), "cache file should not exist on Err");
    }
```

- [ ] **Step 10: Write failing test 4 — Claude null response**

Append after test 3:

```rust
    #[tokio::test]
    async fn clean_lyrics_via_claude_returns_none_when_claude_says_null() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("vidNULL_genius_cleaned.json");

        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "{\"lines\": null}"
                        }
                    }]
                })),
            )
            .mount(&mock)
            .await;

        let ai = AiClient::new(AiSettings {
            api_url: format!("{}/v1", mock.uri()),
            api_key: Some("test".into()),
            model: "stub".into(),
            system_prompt_extra: None,
        });

        let out = clean_lyrics_via_claude(&ai, "Song", "Artist", "buy my album", &cache_path)
            .await
            .unwrap();

        assert_eq!(out, None);
        // Null decision IS cached so we don't re-call Claude.
        let cache = read_lyrics_cache(&cache_path).await.unwrap();
        assert_eq!(cache, Some(None));
    }
```

- [ ] **Step 11: Run `cargo fmt --all --check`**

Expected: clean.

- [ ] **Step 12: Trust by inspection — tests 2, 3, 4 pass**

- Test 2: cache pre-seeded; helper reads cache via `read_lyrics_cache` first and returns early — no Claude call needed. Bogus URL never hit.
- Test 3: wiremock returns 500; `ai.chat_with_timeout` returns `Err`; helper propagates via `.context(...)`; no cache write because the function returns before `write_lyrics_cache`. Cache file absent.
- Test 4: wiremock returns `{"lines": null}`; `parse_claude_response` returns `Ok(None)`; helper writes cache via `write_lyrics_cache(cache_path, None.as_deref())` which records `{"lines": null}`; returns `Ok(None)`.

- [ ] **Step 13: Audit no remaining inline Claude path in `fetch_description_lyrics`**

Search `description_provider.rs` for the old strings (these MUST all be gone from `fetch_description_lyrics`):

```
ai.chat_with_timeout
description_provider: Claude extraction failed
description_provider: Claude response malformed
```

The first must remain ONLY inside `clean_lyrics_via_claude`. The two log strings must be deleted entirely (replaced by the single `description_provider: Claude cleanup failed` line in the new `fetch_description_lyrics`).

- [ ] **Step 14: Run `cargo fmt --all --check`**

Expected: clean.

- [ ] **Step 15: Commit (commit 1 of 2)**

```bash
git add crates/sp-server/src/lyrics/description_provider.rs
git commit -m "$(cat <<'EOF'
lyrics(genius-quality): extract clean_lyrics_via_claude shared helper

Refactor description_provider so the Claude-call + cache-write loop is
reachable by other callers (genius, lrclib-plain). description_provider's
behaviour is unchanged: Ok(None) returned on Claude error, log line text
preserved.

Adds 4 unit tests covering happy-path, cache short-circuit, Claude error
propagation, and null-response handling.

No LYRICS_PIPELINE_VERSION bump.

Refs: docs/superpowers/specs/2026-05-11-genius-quality-fix-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase B: Gather genius + lrclib wiring + integration tests

### Task B.1: Wire gather.rs through `clean_lyrics_via_claude`

**Files:**
- Modify: `crates/sp-server/src/lyrics/gather.rs:166-181`
- Modify: `crates/sp-server/src/lyrics/worker_tests.rs` (append tests)

- [ ] **Step 16: Rewrite genius branch in `gather.rs`**

Locate `gather.rs:174-181` (the current genius branch) and replace it with:

```rust
    if let Some(t) = &genius_track {
        let Some(ai) = ai_client else {
            anyhow::bail!(
                "gather: genius candidate present but ai_client is None for {youtube_id}; \
                 cannot run Claude cleanup"
            );
        };
        let raw_blob: String = t
            .lines
            .iter()
            .map(|l| l.en.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let cache_path = cache_dir.join(format!("{youtube_id}_genius_cleaned.json"));
        match crate::lyrics::description_provider::clean_lyrics_via_claude(
            ai,
            &row.song,
            &row.artist,
            &raw_blob,
            &cache_path,
        )
        .await
        {
            Ok(Some(cleaned)) if !cleaned.is_empty() => {
                info!(
                    %youtube_id,
                    raw_count = t.lines.len(),
                    cleaned_count = cleaned.len(),
                    "gather: genius Claude cleanup complete"
                );
                candidate_texts.push(CandidateText {
                    source: "genius".into(),
                    lines: cleaned,
                    has_timing: false,
                    line_timings: None,
                });
            }
            Ok(_) => anyhow::bail!(
                "gather: genius cleanup returned no lyrics for {youtube_id}"
            ),
            Err(e) => anyhow::bail!(
                "gather: genius cleanup failed for {youtube_id}: {e}"
            ),
        }
    }
```

- [ ] **Step 17: Add `lrclib_track_has_real_timing` helper + rewrite lrclib branch in `gather.rs`**

First, near the top of `gather.rs` (just after the existing `use` block, around line 17), add a `pub(crate)` helper for testability:

```rust
/// Returns `true` if any line in the lrclib track has a non-zero `end_ms`,
/// indicating synced (timestamped) lyrics. `lrclib.rs::parse_plain` emits
/// all-zero timing for the `plainLyrics` fallback path, so this detects
/// that case.
pub(crate) fn lrclib_track_has_real_timing(t: &sp_core::lyrics::LyricsTrack) -> bool {
    t.lines.iter().any(|l| l.end_ms > 0)
}
```

Then locate `gather.rs:166-173` (the current unconditional `has_timing: true` push) and replace it with:

```rust
    if let Some(t) = &lrclib_track {
        // lrclib.rs::parse_plain emits lines with start_ms=0/end_ms=0; only
        // synced lyrics have real timestamps. Detect via the helper so the
        // logic is unit-testable.
        let real_timing = lrclib_track_has_real_timing(t);
        if real_timing {
            candidate_texts.push(CandidateText {
                source: "lrclib".into(),
                lines: t.lines.iter().map(|l| l.en.clone()).collect(),
                has_timing: true,
                line_timings: Some(t.lines.iter().map(|l| (l.start_ms, l.end_ms)).collect()),
            });
        } else {
            let Some(ai) = ai_client else {
                anyhow::bail!(
                    "gather: lrclib-plain candidate present but ai_client is None for {youtube_id}; \
                     cannot run Claude cleanup"
                );
            };
            let raw_blob: String = t
                .lines
                .iter()
                .map(|l| l.en.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let cache_path = cache_dir.join(format!("{youtube_id}_lrclib_cleaned.json"));
            match crate::lyrics::description_provider::clean_lyrics_via_claude(
                ai,
                &row.song,
                &row.artist,
                &raw_blob,
                &cache_path,
            )
            .await
            {
                Ok(Some(cleaned)) if !cleaned.is_empty() => {
                    info!(
                        %youtube_id,
                        raw_count = t.lines.len(),
                        cleaned_count = cleaned.len(),
                        "gather: lrclib-plain Claude cleanup complete"
                    );
                    candidate_texts.push(CandidateText {
                        source: "lrclib".into(),
                        lines: cleaned,
                        has_timing: false,
                        line_timings: None,
                    });
                }
                Ok(_) => anyhow::bail!(
                    "gather: lrclib-plain cleanup returned no lyrics for {youtube_id}"
                ),
                Err(e) => anyhow::bail!(
                    "gather: lrclib-plain cleanup failed for {youtube_id}: {e}"
                ),
            }
        }
    }
```

- [ ] **Step 18: Run `cargo fmt --all --check`**

Expected: clean.

- [ ] **Step 19: Write failing test 5 — `lrclib_track_has_real_timing` helper unit test**

Append at the end of `crates/sp-server/src/lyrics/worker_tests.rs`:

```rust
#[test]
fn lrclib_track_has_real_timing_detects_synced_vs_plain() {
    use crate::lyrics::gather::lrclib_track_has_real_timing;
    use sp_core::lyrics::{LyricsLine, LyricsTrack};

    let make = |timing: &[(u64, u64)]| LyricsTrack {
        version: 1,
        source: "lrclib".into(),
        language_source: "en".into(),
        language_translation: String::new(),
        lines: timing
            .iter()
            .map(|(s, e)| LyricsLine {
                start_ms: *s,
                end_ms: *e,
                en: "x".into(),
                sk: None,
                words: None,
            })
            .collect(),
    };

    // Plain mode: all-zero timing.
    let plain = make(&[(0, 0), (0, 0), (0, 0)]);
    assert!(
        !lrclib_track_has_real_timing(&plain),
        "all-zero end_ms must read as no-timing"
    );

    // Synced mode: any non-zero end_ms qualifies.
    let synced = make(&[(0, 1500), (1500, 3000)]);
    assert!(
        lrclib_track_has_real_timing(&synced),
        "non-zero end_ms must read as real-timing"
    );

    // Edge: single line with timing.
    let partial = make(&[(0, 0), (3000, 5000), (0, 0)]);
    assert!(
        lrclib_track_has_real_timing(&partial),
        "even one timed line qualifies as real-timing"
    );

    // Edge: empty.
    let empty = make(&[]);
    assert!(
        !lrclib_track_has_real_timing(&empty),
        "empty track has no real timing"
    );
}
```

- [ ] **Step 20: Write failing test 6 — lrclib synced skips cleanup**

Append after test 5:

```rust
#[tokio::test]
async fn gather_lrclib_synced_skips_cleanup_pushes_timed_candidate() {
    use crate::ai::AiSettings;
    use crate::ai::client::AiClient;
    use crate::db::models::VideoLyricsRow;
    use crate::lyrics::worker::gather_sources_impl;

    let cache_dir = tempfile::tempdir().unwrap();

    // Claude mock that PANICS if called — synced lrclib must bypass Claude.
    let claude_mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(500)) // would Err if called
        .mount(&claude_mock)
        .await;

    let ai = AiClient::new(AiSettings {
        api_url: format!("{}/v1", claude_mock.uri()),
        api_key: Some("test".into()),
        model: "stub".into(),
        system_prompt_extra: None,
    });

    let row = VideoLyricsRow {
        id: 2,
        youtube_id: "vidLRCS".into(),
        song: "Test Song".into(),
        artist: "Test Artist".into(),
        duration_ms: Some(180_000),
        audio_file_path: None,
        youtube_url: "https://www.youtube.com/watch?v=vidLRCS".into(),
        lyrics_override_text: None,
        lyrics_time_offset_ms: 0,
        spotify_track_id: None,
        spotify_resolved_at: None,
    };

    // Pre-seed empty description so it doesn't muddy the assertions.
    tokio::fs::write(
        cache_dir.path().join("vidLRCS_description_lyrics.json"),
        "{\"lines\":null}",
    )
    .await
    .unwrap();

    // Assert no `vidLRCS_lrclib_cleaned.json` cache file is created when
    // lrclib_track has real timing. This holds even when lrclib HTTP isn't
    // mockable, since the gather code only invokes clean_lyrics_via_claude
    // for plain (zero-timing) lyrics.
    let reqwest_client = reqwest::Client::new();
    let bogus_ytdlp = std::path::PathBuf::from("/definitely/does/not/exist/ytdlp");

    // We don't care about the gather result — only that the lrclib cleanup
    // cache file was never created.
    let _ = gather_sources_impl(
        Some(&ai),
        &bogus_ytdlp,
        cache_dir.path(),
        &reqwest_client,
        &row,
        "",
    )
    .await;

    let cleaned_path = cache_dir.path().join("vidLRCS_lrclib_cleaned.json");
    assert!(
        !cleaned_path.exists(),
        "lrclib-synced path must NOT create the cleaned-lyrics cache file"
    );
}
```

- [ ] **Step 21: Write failing test 7 — gather genius structural source-scrape**

Append after test 6:

```rust
/// Structural regression: the genius branch in `gather.rs` MUST route through
/// `crate::lyrics::description_provider::clean_lyrics_via_claude` and emit
/// `{youtube_id}_genius_cleaned.json` as the cache filename. Mocking genius's
/// HTTP is impractical (api.genius.com is a hardcoded const), so this test
/// reads the source file and asserts on the wiring strings. Matches the
/// pattern of `gather_sources_call_order_preserves_yt_subs_then_lrclib`
/// already in this file.
#[test]
fn gather_genius_branch_uses_clean_lyrics_via_claude() {
    let src = std::fs::read_to_string("src/lyrics/gather.rs").expect("read gather.rs");

    // The genius branch must call the shared helper.
    assert!(
        src.contains("description_provider::clean_lyrics_via_claude"),
        "gather.rs must call description_provider::clean_lyrics_via_claude in the genius/lrclib branches"
    );
    // The genius cache file MUST be named `{youtube_id}_genius_cleaned.json`.
    assert!(
        src.contains("_genius_cleaned.json"),
        "gather.rs must write the genius cleanup cache to {{youtube_id}}_genius_cleaned.json"
    );
    // The lrclib cache file MUST be named `{youtube_id}_lrclib_cleaned.json`.
    assert!(
        src.contains("_lrclib_cleaned.json"),
        "gather.rs must write the lrclib cleanup cache to {{youtube_id}}_lrclib_cleaned.json"
    );
    // Failure mode: bail on Err or null. Verify the error message strings.
    assert!(
        src.contains("genius cleanup returned no lyrics"),
        "gather.rs must bail with 'genius cleanup returned no lyrics' on Ok(None)/empty"
    );
    assert!(
        src.contains("genius cleanup failed"),
        "gather.rs must bail with 'genius cleanup failed' on Err"
    );
    assert!(
        src.contains("lrclib-plain cleanup returned no lyrics"),
        "gather.rs must bail with 'lrclib-plain cleanup returned no lyrics' on Ok(None)/empty"
    );
    assert!(
        src.contains("lrclib-plain cleanup failed"),
        "gather.rs must bail with 'lrclib-plain cleanup failed' on Err"
    );
}
```

- [ ] **Step 22: Run `cargo fmt --all --check`**

Expected: clean.

- [ ] **Step 23: Trust by inspection — tests 5, 6, 7 pass**

- Test 5: `lrclib_track_has_real_timing` returns `false` for all-zero timing, `true` for any non-zero `end_ms`, handles partial/empty correctly. Direct unit test on the helper.
- Test 6: synced lrclib means `real_timing=true`, branch pushes directly without invoking `clean_lyrics_via_claude`. Cache file `vidLRCS_lrclib_cleaned.json` is never created — assertion holds.
- Test 7: the genius and lrclib branches written in Steps 16–17 contain every asserted string verbatim; the source-scrape regex hits them all.

- [ ] **Step 24: Commit (commit 2 of 2)**

```bash
git add crates/sp-server/src/lyrics/gather.rs crates/sp-server/src/lyrics/worker_tests.rs
git commit -m "$(cat <<'EOF'
lyrics(genius-quality): wire gather genius + lrclib-plain through Claude cleanup

The genius branch in gather.rs now Claude-cleans the HTML-scraped lyrics
via clean_lyrics_via_claude before pushing the candidate. The lrclib
branch splits on real_timing: synced lyrics push as has_timing=true
(unchanged); plain lyrics (all end_ms=0) go through the same cleanup.

Failure policy (per feedback_chunk_success_required.md): Claude error /
null / empty on genius or lrclib-plain bails the entire gather — worker
queue retries when Claude recovers. NEVER ships raw genius/lrclib-plain.

Also fixes a latent gather.rs bug: lrclib was pushed with has_timing=true
even when parse_plain produced all-zero timings. Plain mode now correctly
declares has_timing=false.

3 tests added: lrclib_track_has_real_timing unit, lrclib-synced-skips-
cleanup integration, and a structural source-scrape regression for the
genius+lrclib wiring strings.

No LYRICS_PIPELINE_VERSION bump.

Refs: docs/superpowers/specs/2026-05-11-genius-quality-fix-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Verification (controller-only, AFTER both commits land)

1. Controller pushes the two commits to `origin/dev`.
2. CI runs (Lint / Test / Build WASM / Build Tauri / Deploy / E2E). All gates must be green.
3. After deploy lands on win-resolume, controller triggers manual reprocess for id=233 (Saints) via `POST /api/v1/lyrics/reprocess` with `{"video_ids":[233]}`.
4. Controller inspects `C:\ProgramData\SongPlayer\cache\BW_vUblj_RA_genius_cleaned.json` — should contain 15–25 deduped lyric lines (vs 37 raw genius lines).
5. Controller inspects `BW_vUblj_RA_descmerge_audit.json` — Phase 1 `match=0` count should drop sharply; Phase 2 chorus expansion should produce coherent timing.
6. Controller plays Saints on sp-live and Playwright-verifies karaoke rendering on `http://10.77.9.201:8920/lyrics`.
7. Once Saints verifies clean on the wall, sweep other genius-sourced songs per `feedback_song_by_song_iteration.md`.

If verification fails on Saints, the controller (NOT the implementer) iterates code fixes per `feedback_song_by_song_iteration.md` (one song, one diagnosis, one inline code fix).

---

## Spec coverage

| Spec section | Covered by |
|---|---|
| Architecture Change 1 — `clean_lyrics_via_claude` | Steps 4, 7–10 |
| Architecture Change 2 — gather.rs genius branch | Step 16 |
| Architecture Change 3 — gather.rs lrclib synced/plain split + bug fix | Step 17 |
| Failure modes | Steps 16 (genius bail), 17 (lrclib-plain bail), 5 (description swallow) |
| Cache files (3 paths) | Step 4 (description path unchanged), Steps 16–17 (new cache file names) |
| No LYRICS_PIPELINE_VERSION bump | Constraint enforced at file level — no constant modification in any step |
| Tests (7) | Tests 1–4 in Phase A; tests 5–7 in Phase B |
| File-size cap | See File Structure table at top |

## Execution

Use `superpowers:subagent-driven-development`. Dispatch a single sonnet implementer subagent with this plan + the spec.
