# YouTube Description Lyrics Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a fourth candidate-text source to the lyrics ensemble by fetching the YouTube video description via yt-dlp and extracting lyrics via a single Claude call, aiming to recover and exceed the pre-PR#38 catalog quality baseline (0.524 → ≥ 0.65).

**Architecture:** New `description_provider.rs` module with pure prompt/parse helpers (unit-tested) + I/O-wrapped orchestrator (wiremock-backed integration-tested). Two-layer disk cache (raw description + extracted lyrics JSON) so reprocesses reuse work. Plugs into existing `gather_sources` as the 4th concurrent source; zero orchestrator/text-merge changes required. `LYRICS_PIPELINE_VERSION` bumps 3 → 4 to trigger catalog auto-reprocess. A new CI quality-regression-fail step closes the green-CI-theater loophole PR #38 exposed.

**Tech Stack:** Rust 2024, `tokio` subprocess for yt-dlp, existing `AiClient::chat_with_timeout`, `wiremock` for Claude stubbing in tests, `serde_json` parsing. No new crate dependencies.

**Spec:** [`docs/superpowers/specs/2026-04-18-youtube-description-lyrics-provider-design.md`](../specs/2026-04-18-youtube-description-lyrics-provider-design.md)

---

## Scope reminders

- **This PR BLOCKS PR #38 merge.** Land this first, let catalog reprocess complete under v4, confirm `avg_confidence_mean ≥ 0.65`, THEN merge #38.
- **No user-visible VERSION bump.** Dev stays at `0.19.0-dev.1`. Only the internal `LYRICS_PIPELINE_VERSION` constant bumps.
- **Mutation-skip annotations MUST carry a one-line justification** per airuleset. Every task that adds one shows the justification.
- **`cargo fmt --all --check` is the last step before every commit.** Run it, confirm clean, then commit.
- **Do NOT run `cargo build`, `cargo clippy`, `cargo check`, or full workspace test.** Run scoped tests only (e.g., `cargo test -p sp-server lyrics::description_provider`).

## File Structure

**New files:**
- `crates/sp-server/src/lyrics/description_provider.rs` — the whole module; ~250 LOC including tests. Single responsibility: fetch description → extract lyrics via Claude → return `Option<Vec<String>>`.

**Modified files:**
- `crates/sp-server/src/lyrics/mod.rs` — declare `pub mod description_provider;` and bump `LYRICS_PIPELINE_VERSION` from 3 to 4, extending the version-history doc comment.
- `crates/sp-server/src/lyrics/worker.rs` — add 4th concurrent source block inside `gather_sources` at line ~315.
- `CLAUDE.md` — fix the v3 drift in the "History" list AND add v4.
- `.github/workflows/ci.yml` — add a "Fail merge on quality regression" step after the existing "Lyrics Quality Report" step.

Nothing else should need to change. The existing `CandidateText` struct (`provider.rs:34`) already supports `has_timing: false`; the existing `text_merge.rs` already accepts N candidates; the existing 3-bucket reprocess queue auto-triggers on any `LYRICS_PIPELINE_VERSION` bump.

---

## Task 1: Prompt builder (pure function)

**Files:**
- Create: `crates/sp-server/src/lyrics/description_provider.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add module declaration)

Mirror the pattern from `crates/sp-server/src/lyrics/text_merge.rs:25-50` (`build_text_merge_prompt`): a pure function returning `(system, user)` that's easy to unit-test.

- [ ] **Step 1: Write the failing test**

Append to `crates/sp-server/src/lyrics/description_provider.rs` (create the file with doc-comment first):

```rust
//! YouTube description lyrics provider.
//!
//! Fetches the raw description via yt-dlp, pipes it through a narrow Claude
//! prompt, and emits a `CandidateText { source: "description" }` for the
//! ensemble text-merge step. Caches both the raw description and the
//! extracted lyrics JSON on disk so reprocesses reuse the work.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use crate::ai::client::AiClient;

/// Build the Claude extraction prompt for a single video description.
///
/// The system prompt is intentionally specific: request a JSON object with
/// one `lines` key (array of strings or null), strip section markers, keep
/// non-English as-is, and refuse to fabricate. Returns `(system, user)`.
pub fn build_description_extraction_prompt(
    title: &str,
    artist: &str,
    description: &str,
) -> (String, String) {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_has_rule_about_null_when_no_lyrics() {
        let (system, _user) =
            build_description_extraction_prompt("Song", "Artist", "some description");
        assert!(
            system.contains("null"),
            "system prompt must mention the null case: {system}"
        );
        assert!(
            system.contains("\"lines\""),
            "system prompt must name the JSON key: {system}"
        );
    }

    #[test]
    fn prompt_includes_title_artist_and_description_in_user_message() {
        let (_system, user) = build_description_extraction_prompt(
            "How Great Thou Art",
            "Planetshakers",
            "Here are the lyrics:\nHow great thou art",
        );
        assert!(user.contains("How Great Thou Art"), "title missing: {user}");
        assert!(user.contains("Planetshakers"), "artist missing: {user}");
        assert!(
            user.contains("How great thou art"),
            "description body missing: {user}"
        );
    }

    #[test]
    fn prompt_forbids_fabrication() {
        let (system, _user) = build_description_extraction_prompt("S", "A", "desc");
        assert!(
            system.contains("fabricate") || system.contains("not confident"),
            "system prompt must warn against fabrication: {system}"
        );
    }

    #[test]
    fn prompt_requires_original_language() {
        let (system, _user) = build_description_extraction_prompt("S", "A", "desc");
        assert!(
            system.contains("Preserve") && system.contains("translate"),
            "system prompt must require original-language preservation: {system}"
        );
    }
}
```

Then declare the module in `crates/sp-server/src/lyrics/mod.rs`. Find the `pub mod` declarations block near the top (should include `autosub_provider`, `text_merge`, etc.) and add:

```rust
pub mod description_provider;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p sp-server lyrics::description_provider`
Expected: FAIL with compile error on `unimplemented!()` OR `panicked at 'not implemented'`.

- [ ] **Step 3: Implement the prompt**

Replace the `unimplemented!()` body in `build_description_extraction_prompt` with:

```rust
pub fn build_description_extraction_prompt(
    title: &str,
    artist: &str,
    description: &str,
) -> (String, String) {
    let system = String::from(
        "You are a lyrics extractor. Given a YouTube video description, return the song's lyrics \
         as a JSON object with exactly one key, \"lines\", whose value is either:\n\
           - an array of strings (one per lyric line, in reading order, in the song's original language), OR\n\
           - null, when the description contains NO lyrics.\n\
         \n\
         Rules:\n\
         1. Strip section markers (\"Verse 1:\", \"Chorus:\", \"Bridge:\", etc.), keep the line text.\n\
         2. Preserve non-English lyrics as-is. Do NOT translate.\n\
         3. Ignore: artist bio, social links, streaming/buy links, copyright notices, producer/\n\
            writer credits, album promo, tour dates, comment/like/subscribe prompts.\n\
         4. If multiple languages appear (e.g., English + Spanish side-by-side or verse/translation \
            blocks), include ALL lines in reading order — downstream reconciliation handles dedupe.\n\
         5. Do not fabricate lyrics. If you are not confident the text is the song's lyrics, \
            return {\"lines\": null}.\n\
         6. Output ONLY the JSON object. No preamble, no markdown fences, no commentary.",
    );
    let user = format!(
        "Video title: {title}\nArtist: {artist}\n\nDescription:\n---\n{description}\n---"
    );
    (system, user)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sp-server lyrics::description_provider`
Expected: PASS (4 tests).

- [ ] **Step 5: cargo fmt + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/description_provider.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): add description_provider prompt builder"
```

---

## Task 2: Claude response parser (pure function)

**Files:**
- Modify: `crates/sp-server/src/lyrics/description_provider.rs`

Mirror the pattern from `crates/sp-server/src/lyrics/text_merge.rs:52-99` — a separate parser function that handles the three cases (`{"lines": [...]}`, `{"lines": null}`, malformed) distinctly.

- [ ] **Step 1: Write the failing tests**

Append to the `#[cfg(test)] mod tests { }` block in `crates/sp-server/src/lyrics/description_provider.rs`:

```rust
#[test]
fn parse_lines_array_returns_some() {
    let raw = r#"{"lines": ["How great thou art", "O Lord my God"]}"#;
    let out = parse_claude_response(raw).unwrap();
    assert_eq!(
        out,
        Some(vec![
            "How great thou art".to_string(),
            "O Lord my God".to_string(),
        ])
    );
}

#[test]
fn parse_lines_null_returns_none() {
    let raw = r#"{"lines": null}"#;
    let out = parse_claude_response(raw).unwrap();
    assert_eq!(out, None);
}

#[test]
fn parse_handles_markdown_fences() {
    let raw = "```json\n{\"lines\": [\"line one\"]}\n```";
    let out = parse_claude_response(raw).unwrap();
    assert_eq!(out, Some(vec!["line one".to_string()]));
}

#[test]
fn parse_handles_preamble_before_fences() {
    let raw = "I'll analyze the description.\n```json\n{\"lines\":null}\n```";
    let out = parse_claude_response(raw).unwrap();
    assert_eq!(out, None);
}

#[test]
fn parse_rejects_invalid_json() {
    assert!(parse_claude_response("not json at all").is_err());
    assert!(parse_claude_response("{ not json").is_err());
}

#[test]
fn parse_rejects_missing_lines_key() {
    assert!(parse_claude_response(r#"{"foo": []}"#).is_err());
}

#[test]
fn parse_rejects_wrong_lines_type() {
    assert!(parse_claude_response(r#"{"lines": "string not array"}"#).is_err());
    assert!(parse_claude_response(r#"{"lines": 42}"#).is_err());
}
```

And add this declaration above the `#[cfg(test)]`:

```rust
pub(crate) fn parse_claude_response(raw: &str) -> Result<Option<Vec<String>>> {
    unimplemented!()
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sp-server lyrics::description_provider::tests::parse_`
Expected: FAIL on `unimplemented!()`.

- [ ] **Step 3: Implement the parser**

Replace the `unimplemented!()` body with:

```rust
pub(crate) fn parse_claude_response(raw: &str) -> Result<Option<Vec<String>>> {
    let cleaned = crate::ai::client::strip_markdown_fences(raw);
    let v: serde_json::Value = serde_json::from_str(&cleaned)
        .with_context(|| format!("failed to parse Claude response as JSON: {cleaned}"))?;
    let lines = v
        .get("lines")
        .ok_or_else(|| anyhow::anyhow!("missing 'lines' key in Claude response: {cleaned}"))?;
    if lines.is_null() {
        return Ok(None);
    }
    let arr = lines
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("'lines' is not an array or null: {cleaned}"))?;
    let out: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if out.len() != arr.len() {
        anyhow::bail!("some elements of 'lines' were not strings: {cleaned}");
    }
    Ok(Some(out))
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p sp-server lyrics::description_provider`
Expected: PASS (4 prompt tests + 7 parser tests = 11 total).

- [ ] **Step 5: cargo fmt + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/description_provider.rs
git commit -m "feat(lyrics): add description Claude response parser"
```

---

## Task 3: Disk cache helpers (read + write)

**Files:**
- Modify: `crates/sp-server/src/lyrics/description_provider.rs`

Two small async functions to read/write the extracted-lyrics JSON cache at `{cache_dir}/{youtube_id}_description_lyrics.json`. Shape `{"lines": [...]}` or `{"lines": null}`.

- [ ] **Step 1: Write the failing tests**

Append to `#[cfg(test)] mod tests`:

```rust
use tempfile::tempdir;

#[tokio::test]
async fn cache_roundtrip_with_lyrics() {
    let dir = tempdir().unwrap();
    let p = dir.path().join("abc_description_lyrics.json");
    write_lyrics_cache(&p, Some(&["one".into(), "two".into()]))
        .await
        .unwrap();
    let back = read_lyrics_cache(&p).await.unwrap();
    assert_eq!(back, Some(Some(vec!["one".into(), "two".into()])));
}

#[tokio::test]
async fn cache_roundtrip_with_null() {
    let dir = tempdir().unwrap();
    let p = dir.path().join("abc_description_lyrics.json");
    write_lyrics_cache(&p, None).await.unwrap();
    let back = read_lyrics_cache(&p).await.unwrap();
    assert_eq!(back, Some(None));
}

#[tokio::test]
async fn cache_missing_file_returns_ok_none() {
    let dir = tempdir().unwrap();
    let p = dir.path().join("nonexistent_description_lyrics.json");
    let back = read_lyrics_cache(&p).await.unwrap();
    assert_eq!(back, None);
}
```

If `tempfile` is not already a dev-dependency of `sp-server`, you'll see a compile error. Check `grep tempfile crates/sp-server/Cargo.toml`; it IS already a dev-dep for existing tests — if not, add it under `[dev-dependencies]`.

Add these function declarations (not yet implemented):

```rust
/// Read the cached extracted-lyrics JSON.
///
/// Returns:
/// - `Ok(None)` when the file does not exist (no cache yet).
/// - `Ok(Some(None))` when the cache records that this song has no lyrics in its description.
/// - `Ok(Some(Some(lines)))` when the cache has extracted lyric lines.
/// - `Err` when the file exists but is malformed (we refuse to silently discard it).
pub(crate) async fn read_lyrics_cache(path: &Path) -> Result<Option<Option<Vec<String>>>> {
    unimplemented!()
}

/// Write the extracted-lyrics JSON cache atomically.
///
/// `lines = Some(&[...])` writes `{"lines": [...]}`.
/// `lines = None` writes `{"lines": null}`.
pub(crate) async fn write_lyrics_cache(path: &Path, lines: Option<&[String]>) -> Result<()> {
    unimplemented!()
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sp-server lyrics::description_provider::tests::cache_`
Expected: FAIL on `unimplemented!()`.

- [ ] **Step 3: Implement**

Replace the function bodies:

```rust
pub(crate) async fn read_lyrics_cache(path: &Path) -> Result<Option<Option<Vec<String>>>> {
    let Ok(bytes) = tokio::fs::read(path).await else {
        return Ok(None);
    };
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).context("malformed description_lyrics cache")?;
    let lines = v
        .get("lines")
        .ok_or_else(|| anyhow::anyhow!("cache missing 'lines' key"))?;
    if lines.is_null() {
        return Ok(Some(None));
    }
    let arr = lines
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("cache 'lines' is not array or null"))?;
    let out: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if out.len() != arr.len() {
        anyhow::bail!("cache 'lines' contains non-string elements");
    }
    Ok(Some(Some(out)))
}

pub(crate) async fn write_lyrics_cache(path: &Path, lines: Option<&[String]>) -> Result<()> {
    let body = match lines {
        Some(l) => serde_json::json!({ "lines": l }),
        None => serde_json::json!({ "lines": null }),
    };
    let s = serde_json::to_string(&body)?;
    tokio::fs::write(path, s).await.context("write cache")?;
    Ok(())
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p sp-server lyrics::description_provider`
Expected: PASS (11 previous tests + 3 cache tests = 14).

- [ ] **Step 5: cargo fmt + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/description_provider.rs
git commit -m "feat(lyrics): add description_provider cache read/write helpers"
```

---

## Task 4: `fetch_raw_description` — yt-dlp subprocess + cache

**Files:**
- Modify: `crates/sp-server/src/lyrics/description_provider.rs`

Mirror the pattern from `autosub_provider.rs::fetch_autosub` at lines 322-370: tokio subprocess, `CREATE_NO_WINDOW` on Windows, `kill_on_drop(true)`. Cached at `{cache_dir}/{youtube_id}_description.txt`. This function is I/O-only; annotate with `mutants::skip` (one-line justification).

- [ ] **Step 1: Write the failing tests**

Append to `#[cfg(test)] mod tests`:

```rust
#[tokio::test]
async fn fetch_raw_description_returns_cached_without_subprocess() {
    let dir = tempdir().unwrap();
    let cached_path = dir.path().join("videoid_description.txt");
    tokio::fs::write(&cached_path, "hello from cache")
        .await
        .unwrap();

    // If the function tries to spawn the bogus ytdlp path below, the test
    // fails. Cached-read path must short-circuit.
    let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");
    let out = fetch_raw_description(bogus_ytdlp, "videoid", dir.path())
        .await
        .unwrap();
    assert_eq!(out.as_deref(), Some("hello from cache"));
}

#[tokio::test]
async fn fetch_raw_description_returns_none_when_ytdlp_missing_and_no_cache() {
    let dir = tempdir().unwrap();
    let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");
    let out = fetch_raw_description(bogus_ytdlp, "novideo", dir.path()).await;
    // Subprocess spawn failure returns Ok(None) — description is optional.
    assert!(matches!(out, Ok(None)));
    // No cache file should have been written on failure.
    assert!(!dir.path().join("novideo_description.txt").exists());
}
```

Add the function declaration (not yet implemented):

```rust
/// Fetch the YouTube video description, using a disk cache keyed by `youtube_id`.
///
/// Returns `Ok(Some(text))` when the description is available (cached or freshly
/// fetched), `Ok(None)` when yt-dlp failed and no cache exists. Never creates a
/// cache file on failure — so the next reprocess retries.
// mutants::skip: subprocess I/O wrapper; behaviour covered by cached-hit test and
// subprocess-failure integration test.
#[cfg_attr(test, mutants::skip)]
pub(crate) async fn fetch_raw_description(
    ytdlp_path: &Path,
    youtube_id: &str,
    cache_dir: &Path,
) -> Result<Option<String>> {
    unimplemented!()
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sp-server lyrics::description_provider::tests::fetch_raw_description`
Expected: FAIL on `unimplemented!()`.

- [ ] **Step 3: Implement**

Replace the body:

```rust
pub(crate) async fn fetch_raw_description(
    ytdlp_path: &Path,
    youtube_id: &str,
    cache_dir: &Path,
) -> Result<Option<String>> {
    let cache_path = cache_dir.join(format!("{youtube_id}_description.txt"));
    if let Ok(cached) = tokio::fs::read_to_string(&cache_path).await {
        debug!(youtube_id, "description_provider: cache hit (raw description)");
        return Ok(Some(cached));
    }

    let url = format!("https://www.youtube.com/watch?v={youtube_id}");
    let mut cmd = tokio::process::Command::new(ytdlp_path);
    cmd.arg("--skip-download")
        .arg("--no-warnings")
        .arg("--print")
        .arg("%(description)s")
        .arg(&url);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd.kill_on_drop(true);

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            warn!(youtube_id, %e, "description_provider: yt-dlp spawn failed; skipping");
            return Ok(None);
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            youtube_id,
            status = ?output.status,
            stderr = %stderr,
            "description_provider: yt-dlp returned non-zero; skipping"
        );
        return Ok(None);
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        debug!(youtube_id, "description_provider: yt-dlp returned empty description");
        // Cache the empty result so we don't re-spawn yt-dlp on reprocess.
        let _ = tokio::fs::write(&cache_path, "").await;
        return Ok(Some(String::new()));
    }
    tokio::fs::write(&cache_path, &text)
        .await
        .context("write description cache")?;
    Ok(Some(text))
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p sp-server lyrics::description_provider`
Expected: PASS (14 prior + 2 new = 16).

- [ ] **Step 5: cargo fmt + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/description_provider.rs
git commit -m "feat(lyrics): add yt-dlp description fetch with disk cache"
```

---

## Task 5: `fetch_description_lyrics` orchestrator — cached path

**Files:**
- Modify: `crates/sp-server/src/lyrics/description_provider.rs`

This is the public entry point. First test covers the fully-cached path: both `_description.txt` and `_description_lyrics.json` exist → read cached → return without any subprocess or HTTP.

- [ ] **Step 1: Write the failing test**

Append to `#[cfg(test)] mod tests`:

```rust
use crate::ai::AiSettings;

#[tokio::test]
async fn fetch_description_lyrics_returns_cached_lines_with_no_claude_call() {
    let dir = tempdir().unwrap();
    // Pre-seed both cache files.
    tokio::fs::write(
        dir.path().join("vid123_description.txt"),
        "[Verse 1]\nAmazing grace how sweet the sound",
    )
    .await
    .unwrap();
    write_lyrics_cache(
        &dir.path().join("vid123_description_lyrics.json"),
        Some(&["Amazing grace".into(), "how sweet the sound".into()]),
    )
    .await
    .unwrap();

    // AiClient pointed at an unreachable URL. If the code calls Claude, the
    // test hangs/errors and we'd notice.
    let ai = AiClient::new(AiSettings {
        api_url: "http://127.0.0.1:1/v1".into(),
        api_key: Some("never-used".into()),
        model: "stub".into(),
        system_prompt_extra: None,
    });

    let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");
    let out = fetch_description_lyrics(
        &ai,
        bogus_ytdlp,
        "vid123",
        dir.path(),
        "Amazing Grace",
        "Chris Tomlin",
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        Some(vec![
            "Amazing grace".into(),
            "how sweet the sound".into(),
        ])
    );
}

#[tokio::test]
async fn fetch_description_lyrics_returns_none_when_cache_records_no_lyrics() {
    let dir = tempdir().unwrap();
    tokio::fs::write(dir.path().join("vidNOLYR_description.txt"), "just promo text")
        .await
        .unwrap();
    write_lyrics_cache(
        &dir.path().join("vidNOLYR_description_lyrics.json"),
        None,
    )
    .await
    .unwrap();

    let ai = AiClient::new(AiSettings {
        api_url: "http://127.0.0.1:1/v1".into(),
        api_key: Some("never-used".into()),
        model: "stub".into(),
        system_prompt_extra: None,
    });
    let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");

    let out = fetch_description_lyrics(
        &ai,
        bogus_ytdlp,
        "vidNOLYR",
        dir.path(),
        "Song",
        "Artist",
    )
    .await
    .unwrap();
    assert_eq!(out, None);
}
```

Declare the public function:

```rust
/// Fetch and extract lyrics from a YouTube video description.
///
/// Caches both the raw description and the extracted lyrics JSON per
/// `youtube_id`, so subsequent calls short-circuit any yt-dlp or Claude
/// work. `Ok(None)` means no lyrics available; `Ok(Some(lines))` means
/// the caller should push a `CandidateText { source: "description" }`.
// mutants::skip: orchestration across yt-dlp + Claude I/O; behaviour covered by
// cached-hit, no-lyrics, success, malformed-response, and ytdlp-failure tests.
#[cfg_attr(test, mutants::skip)]
pub async fn fetch_description_lyrics(
    ai: &AiClient,
    ytdlp_path: &Path,
    youtube_id: &str,
    cache_dir: &Path,
    title: &str,
    artist: &str,
) -> Result<Option<Vec<String>>> {
    unimplemented!()
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p sp-server lyrics::description_provider::tests::fetch_description_lyrics_returns_cached`
Expected: FAIL on `unimplemented!()`.

- [ ] **Step 3: Implement the cached-path portion**

Replace the body:

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
        debug!(youtube_id, "description_provider: cache hit (extracted lyrics)");
        return Ok(cached);
    }

    // Raw description fetch (cached separately).
    let Some(description) = fetch_raw_description(ytdlp_path, youtube_id, cache_dir).await?
    else {
        return Ok(None);
    };
    if description.trim().is_empty() {
        // Description was genuinely empty — record "no lyrics" so next
        // reprocess skips instantly.
        write_lyrics_cache(&lyrics_cache_path, None).await?;
        return Ok(None);
    }

    // Call Claude. On any error, return Ok(None) WITHOUT writing a cache
    // entry so the next reprocess retries.
    let (system, user) = build_description_extraction_prompt(title, artist, &description);
    let raw = match ai.chat_with_timeout(&system, &user, 180).await {
        Ok(r) => r,
        Err(e) => {
            warn!(youtube_id, %e, "description_provider: Claude extraction failed");
            return Ok(None);
        }
    };
    let parsed = match parse_claude_response(&raw) {
        Ok(p) => p,
        Err(e) => {
            warn!(youtube_id, %e, "description_provider: Claude response malformed");
            return Ok(None);
        }
    };
    // Persist the decision (Some or None) so next reprocess skips Claude.
    write_lyrics_cache(&lyrics_cache_path, parsed.as_deref()).await?;
    Ok(parsed)
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p sp-server lyrics::description_provider`
Expected: PASS (16 prior + 2 new = 18).

- [ ] **Step 5: cargo fmt + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/description_provider.rs
git commit -m "feat(lyrics): fetch_description_lyrics orchestrator (cached path)"
```

---

## Task 6: `fetch_description_lyrics` — live Claude call with wiremock

**Files:**
- Modify: `crates/sp-server/src/lyrics/description_provider.rs`

Mirror the wiremock pattern in `crates/sp-server/src/lyrics/text_merge.rs:160-195` — spin up a mock HTTP server, configure a canned Claude response, assert the orchestrator parses it and writes the cache.

- [ ] **Step 1: Write the failing test**

Append to `#[cfg(test)] mod tests`:

```rust
#[tokio::test]
async fn fetch_description_lyrics_calls_claude_and_caches_result() {
    let dir = tempdir().unwrap();
    // Pre-seed ONLY the raw description so yt-dlp is skipped.
    tokio::fs::write(
        dir.path().join("vidCALL_description.txt"),
        "[Verse]\nFull lyrics below:\nLine A\nLine B",
    )
    .await
    .unwrap();

    // wiremock stubs the Claude endpoint.
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
            serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "{\"lines\": [\"Line A\", \"Line B\"]}"
                    }
                }]
            }),
        ))
        .mount(&mock)
        .await;

    let ai = AiClient::new(AiSettings {
        api_url: format!("{}/v1", mock.uri()),
        api_key: Some("test".into()),
        model: "claude-opus-4-20250514".into(),
        system_prompt_extra: None,
    });
    let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");

    let out = fetch_description_lyrics(
        &ai,
        bogus_ytdlp,
        "vidCALL",
        dir.path(),
        "Song",
        "Artist",
    )
    .await
    .unwrap();
    assert_eq!(out, Some(vec!["Line A".into(), "Line B".into()]));

    // Cache should now contain the parsed result.
    let cache = read_lyrics_cache(&dir.path().join("vidCALL_description_lyrics.json"))
        .await
        .unwrap();
    assert_eq!(cache, Some(Some(vec!["Line A".into(), "Line B".into()])));
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test -p sp-server lyrics::description_provider::tests::fetch_description_lyrics_calls_claude_and_caches_result`
Expected: PASS — the orchestrator body already implements this path from Task 5. If the test FAILS, re-read Task 5's implementation and check for typos. The wiremock dependency is already in the dev-deps via text_merge tests; no Cargo.toml edits needed.

- [ ] **Step 3: cargo fmt + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/description_provider.rs
git commit -m "test(lyrics): wiremock-verified Claude call + cache write"
```

---

## Task 7: `fetch_description_lyrics` — no-lyrics-in-description branch

**Files:**
- Modify: `crates/sp-server/src/lyrics/description_provider.rs`

- [ ] **Step 1: Write the failing test**

Append to `#[cfg(test)] mod tests`:

```rust
#[tokio::test]
async fn fetch_description_lyrics_caches_null_when_claude_says_no_lyrics() {
    let dir = tempdir().unwrap();
    tokio::fs::write(
        dir.path().join("vidNULL_description.txt"),
        "Buy my album! Subscribe! Links below.",
    )
    .await
    .unwrap();

    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
            serde_json::json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "{\"lines\": null}"}
                }]
            }),
        ))
        .mount(&mock)
        .await;
    let ai = AiClient::new(AiSettings {
        api_url: format!("{}/v1", mock.uri()),
        api_key: Some("test".into()),
        model: "stub".into(),
        system_prompt_extra: None,
    });
    let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");

    let out = fetch_description_lyrics(
        &ai, bogus_ytdlp, "vidNULL", dir.path(), "Song", "Artist"
    )
    .await
    .unwrap();
    assert_eq!(out, None);

    let cache = read_lyrics_cache(&dir.path().join("vidNULL_description_lyrics.json"))
        .await
        .unwrap();
    assert_eq!(cache, Some(None), "null result must be cached for instant reprocess");
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p sp-server lyrics::description_provider::tests::fetch_description_lyrics_caches_null`
Expected: PASS — Task 5's implementation already handles this.

- [ ] **Step 3: cargo fmt + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/description_provider.rs
git commit -m "test(lyrics): null extraction result is cached for fast reprocess"
```

---

## Task 8: `fetch_description_lyrics` — malformed Claude response

**Files:**
- Modify: `crates/sp-server/src/lyrics/description_provider.rs`

Ensure malformed responses return `Ok(None)` AND do NOT write a cache file (so the next reprocess retries).

- [ ] **Step 1: Write the failing test**

Append to `#[cfg(test)] mod tests`:

```rust
#[tokio::test]
async fn fetch_description_lyrics_no_cache_on_malformed_claude_response() {
    let dir = tempdir().unwrap();
    tokio::fs::write(
        dir.path().join("vidBAD_description.txt"),
        "some description",
    )
    .await
    .unwrap();

    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
            serde_json::json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "this is not JSON at all"}
                }]
            }),
        ))
        .mount(&mock)
        .await;
    let ai = AiClient::new(AiSettings {
        api_url: format!("{}/v1", mock.uri()),
        api_key: Some("test".into()),
        model: "stub".into(),
        system_prompt_extra: None,
    });
    let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");

    let out = fetch_description_lyrics(
        &ai, bogus_ytdlp, "vidBAD", dir.path(), "S", "A"
    )
    .await
    .unwrap();
    assert_eq!(out, None);
    // CRITICAL: no cache file on malformed response — we MUST retry on next reprocess.
    assert!(
        !dir.path()
            .join("vidBAD_description_lyrics.json")
            .exists(),
        "malformed Claude response must NOT write a cache entry"
    );
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p sp-server lyrics::description_provider::tests::fetch_description_lyrics_no_cache_on_malformed`
Expected: PASS — the implementation returns `Ok(None)` before the `write_lyrics_cache` call on parse failure.

- [ ] **Step 3: cargo fmt + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/description_provider.rs
git commit -m "test(lyrics): malformed Claude response does not poison cache"
```

---

## Task 9: Empty description cached as null

**Files:**
- Modify: `crates/sp-server/src/lyrics/description_provider.rs`

- [ ] **Step 1: Write the failing test**

Append to `#[cfg(test)] mod tests`:

```rust
#[tokio::test]
async fn fetch_description_lyrics_empty_raw_caches_null_no_claude() {
    let dir = tempdir().unwrap();
    // Empty raw description (yt-dlp returns empty string).
    tokio::fs::write(dir.path().join("vidEMPTY_description.txt"), "")
        .await
        .unwrap();

    // AiClient at unreachable URL — Claude must NOT be called for empty descriptions.
    let ai = AiClient::new(AiSettings {
        api_url: "http://127.0.0.1:1/v1".into(),
        api_key: Some("never".into()),
        model: "stub".into(),
        system_prompt_extra: None,
    });
    let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");

    let out = fetch_description_lyrics(
        &ai, bogus_ytdlp, "vidEMPTY", dir.path(), "S", "A"
    )
    .await
    .unwrap();
    assert_eq!(out, None);

    let cache = read_lyrics_cache(&dir.path().join("vidEMPTY_description_lyrics.json"))
        .await
        .unwrap();
    assert_eq!(cache, Some(None), "empty description must cache null");
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p sp-server lyrics::description_provider::tests::fetch_description_lyrics_empty_raw`
Expected: PASS — Task 5's `if description.trim().is_empty()` branch handles this.

- [ ] **Step 3: cargo fmt + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/description_provider.rs
git commit -m "test(lyrics): empty description caches null without Claude call"
```

---

## Task 10: Plug description provider into `gather_sources`

**Files:**
- Modify: `crates/sp-server/src/lyrics/worker.rs`

Insert a 4th concurrent source block immediately after the autosub block (~line 315) and before the `candidate_texts.is_empty()` guard. Re-use existing `self.ai_client`, `self.ytdlp_path`, `self.cache_dir`.

- [ ] **Step 1: Read the current gather_sources to find the insertion point**

Open `crates/sp-server/src/lyrics/worker.rs`. Confirm: the function body ends with a block that builds `candidate_texts: Vec<CandidateText>` from `yt_subs_track` (lines ~318-325) and `lrclib_track` (lines ~326-333). Right before the `if candidate_texts.is_empty()` guard is where the new source plugs in.

- [ ] **Step 2: Write the failing test**

Create a new test in `crates/sp-server/src/lyrics/worker.rs` inside the existing `#[cfg(test)] mod tests` block (or a sibling file if tests.rs convention already applies — check the file header):

```rust
#[tokio::test]
async fn gather_sources_pushes_description_candidate_when_claude_returns_lyrics() {
    use crate::ai::{AiClient, AiSettings};
    use crate::db::models::VideoLyricsRow;
    use std::path::PathBuf;
    use tempfile::tempdir;

    let cache_dir = tempdir().unwrap();
    // Pre-seed the raw description cache so yt-dlp isn't invoked.
    tokio::fs::write(
        cache_dir.path().join("vidDESC_description.txt"),
        "Lyrics:\nAmazing grace\nHow sweet the sound",
    )
    .await
    .unwrap();

    // Stub Claude.
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content":
                "{\"lines\": [\"Amazing grace\", \"How sweet the sound\"]}"}}]
        })))
        .mount(&mock)
        .await;
    let ai = AiClient::new(AiSettings {
        api_url: format!("{}/v1", mock.uri()),
        api_key: Some("test".into()),
        model: "stub".into(),
        system_prompt_extra: None,
    });

    // Build a minimal worker that has only what gather_sources reads.
    // Use an unreachable ytdlp path so yt_subs + autosub both fail, and
    // artist empty so lrclib also skips. Only description remains.
    let worker = LyricsWorker::for_test_gather_sources(
        PathBuf::from("/definitely/does/not/exist/ytdlp"),
        cache_dir.path().to_path_buf(),
        ai,
    );

    let row = VideoLyricsRow {
        id: 1,
        youtube_id: "vidDESC".into(),
        song: "Amazing Grace".into(),
        artist: "".into(), // empty -> lrclib skipped
        duration_ms: Some(180_000),
        audio_file_path: None,
        youtube_url: "https://www.youtube.com/watch?v=vidDESC".into(),
    };

    let autosub_tmp = tempdir().unwrap();
    let ctx = worker.gather_sources(&row, autosub_tmp.path()).await.unwrap();

    assert_eq!(ctx.candidate_texts.len(), 1, "only description should be present");
    assert_eq!(ctx.candidate_texts[0].source, "description");
    assert_eq!(
        ctx.candidate_texts[0].lines,
        vec!["Amazing grace".to_string(), "How sweet the sound".to_string()]
    );
    assert!(!ctx.candidate_texts[0].has_timing);
}
```

Also add a small test-only constructor to `LyricsWorker` that exposes the minimum fields needed. Find the existing `LyricsWorker` struct in `worker.rs` (around lines 40-90) and add alongside the real `new()`:

```rust
#[cfg(test)]
impl LyricsWorker {
    /// Minimal constructor used ONLY by gather_sources tests. Exposes just
    /// the fields gather_sources reads from — avoids standing up full
    /// worker lifecycle for a focused integration test.
    pub fn for_test_gather_sources(
        ytdlp_path: PathBuf,
        cache_dir: PathBuf,
        ai_client: AiClient,
    ) -> Self {
        // Fill non-relevant fields with defaults/channel stubs. The exact
        // set depends on the current struct — copy the pattern of `new()`
        // but pass no-op senders where the constructor needs them.
        // (See the real `new` signature at the top of this file.)
        unimplemented!("see comment above")
    }
}
```

The implementer will need to fill in this constructor to match whatever fields `LyricsWorker` currently has. If building a test-only constructor becomes non-trivial (e.g., requires spawning real channels or DB pools), fall back to the alternate approach below:

**Alternative if `LyricsWorker::for_test_gather_sources` is impractical:**
Extract `gather_sources` into a free function that takes only its dependencies:
```rust
pub(crate) async fn gather_sources_impl(
    ai: &AiClient,
    ytdlp: &Path,
    cache_dir: &Path,
    client: &reqwest::Client,  // for lrclib
    row: &VideoLyricsRow,
    autosub_tmp: &Path,
) -> Result<SongContext>
```
Then make the method a one-line wrapper that calls the free function. This is a small refactor, fits inside this task, and makes the test trivial.

Either approach is acceptable; the test assertions stay identical.

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p sp-server lyrics::worker lyrics::description_provider`
Expected: FAIL — the test won't compile (constructor unimplemented) and/or the assertion fails because gather_sources hasn't been wired yet.

- [ ] **Step 4: Wire the 4th source into gather_sources**

In `crates/sp-server/src/lyrics/worker.rs::gather_sources`, immediately after the autosub block (look for `let autosub_json3 = match fetch_autosub(...)` and the following lines assembling `candidate_texts`) and BEFORE the existing `if yt_subs_track ...push(CandidateText {...})` block that builds candidate_texts, add a new block. Actually the cleanest insertion is RIGHT AFTER the existing `candidate_texts.push` calls for yt_subs and lrclib (around line 333 in the spec's reference), and BEFORE the `candidate_texts.is_empty()` guard. Paste this block there:

```rust
        // 4. YouTube description lyrics (LLM-extracted). Best-effort.
        let description_lines = match crate::lyrics::description_provider::fetch_description_lyrics(
            &self.ai_client,
            &self.ytdlp_path,
            &youtube_id,
            &self.cache_dir,
            &row.song,
            &row.artist,
        )
        .await
        {
            Ok(Some(lines)) if !lines.is_empty() => {
                info!(
                    youtube_id = %youtube_id,
                    line_count = lines.len(),
                    "gather: description lyrics hit"
                );
                Some(lines)
            }
            Ok(_) => {
                debug!("gather: no description lyrics for {youtube_id}");
                None
            }
            Err(e) => {
                warn!("gather: description fetch error for {youtube_id}: {e}");
                None
            }
        };
        if let Some(lines) = description_lines {
            candidate_texts.push(CandidateText {
                source: "description".into(),
                lines,
                has_timing: false,
                line_timings: None,
            });
        }
```

If the worker struct doesn't already have an `ai_client: AiClient` field, check where it's currently instantiated — look for `Orchestrator` construction further down in `process_song`. The AiClient must be available on `self`; if not, the worker's `new` should accept it. Most likely it's already there from PR #38 (used by `merge_candidate_texts` and `translate_track`). Grep with `grep -n "ai_client" crates/sp-server/src/lyrics/worker.rs`.

- [ ] **Step 5: Implement `for_test_gather_sources` (or the alternative refactor)**

Choose one path:
- **Path A**: fill the test-only constructor by reading the real `new(...)` signature and stubbing out channels. Most senders can be `broadcast::channel(1)` or `mpsc::channel(1)` where the receiver is dropped immediately.
- **Path B**: extract `gather_sources_impl` as described above and call it from both the real method and the test.

- [ ] **Step 6: Run the gather_sources test**

Run: `cargo test -p sp-server lyrics::worker::tests::gather_sources_pushes_description_candidate`
Expected: PASS. If not, check (a) the `ai_client` field is populated in the test constructor, (b) the yt_subs/lrclib/autosub branches all return None in this test setup, (c) the description provider's cached path is triggered by the pre-seeded `_description.txt`.

- [ ] **Step 7: Run the full lyrics test suite to confirm no regressions**

Run: `cargo test -p sp-server lyrics`
Expected: all existing tests still pass plus the new one.

- [ ] **Step 8: cargo fmt + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/worker.rs
git commit -m "feat(lyrics): plug description provider as 4th gather_sources candidate"
```

---

## Task 11: Bump `LYRICS_PIPELINE_VERSION` to 4

**Files:**
- Modify: `crates/sp-server/src/lyrics/mod.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block at the bottom of `crates/sp-server/src/lyrics/mod.rs` (create the block if it doesn't exist):

```rust
#[test]
fn lyrics_pipeline_version_is_v4() {
    assert_eq!(
        LYRICS_PIPELINE_VERSION, 4,
        "version bump is the signal for catalog auto-reprocess; see CLAUDE.md history"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p sp-server lyrics::tests::lyrics_pipeline_version_is_v4`
Expected: FAIL with `left: 3, right: 4`.

- [ ] **Step 3: Bump the constant + extend history comment**

In `crates/sp-server/src/lyrics/mod.rs`, find `pub const LYRICS_PIPELINE_VERSION: u32 = 3;` and the preceding doc comment. Change the literal to `4` and append a new bullet to the version-history doc comment:

```rust
/// Monotonic version of the lyrics pipeline output. Bump when prompts, provider
/// list, merge algorithm, or reference-text selection changes. Every bump
/// triggers auto-reprocess of existing songs via the stale-version bucket.
///
/// Version history:
/// - v1 (implicit, pre-this-PR): single-path yt_subs→Qwen3 or lrclib-line-level
/// - v2 (this PR): ensemble orchestrator with AutoSubProvider + Claude text-merge
/// - v3 (this PR): merge prompt reworked — weight by base_confidence^2,
///   prefer higher-confidence provider on >1000ms disagreement. Fixes
///   regression seen on h-A1Tzkjsi4 (v2 got 0.48 vs baseline 0.63).
/// - v4: description provider added as 4th text candidate (YouTube video
///   description parsed via Claude). Targets recovering from v3 regression
///   (0.524 -> >= 0.65) by giving text_merge reliable reference text on
///   songs lacking yt_subs/lrclib coverage.
pub const LYRICS_PIPELINE_VERSION: u32 = 4;
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p sp-server lyrics::tests::lyrics_pipeline_version_is_v4`
Expected: PASS.

Also run the full `lyrics::tests` module to ensure no other test hard-coded `== 3`:

```
cargo test -p sp-server lyrics::
```

If a test hard-codes `LYRICS_PIPELINE_VERSION == 3`, update it to `4` or use the constant directly.

- [ ] **Step 5: cargo fmt + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): bump LYRICS_PIPELINE_VERSION to 4 (description provider)"
```

---

## Task 12: Update `CLAUDE.md` — fix v3 drift + add v4

**Files:**
- Modify: `CLAUDE.md`

The "History" list in `CLAUDE.md` under `## Pipeline versioning (lyrics)` currently reads only through v2 even though the mod.rs already has v3. Fix the drift AND add v4.

- [ ] **Step 1: Read the current CLAUDE.md history section**

Open `CLAUDE.md` and find the "## Pipeline versioning (lyrics)" section — line ~168. Locate the "**History:**" list at the bottom of that section.

- [ ] **Step 2: Replace the history list**

Find this block:
```markdown
**History:**
- v1 (pre-#33): single-path yt_subs→Qwen3 or lrclib-line-level
- v2 (this PR): ensemble orchestrator + AutoSubProvider + Claude text-merge
```

Replace with:
```markdown
**History:**
- v1 (pre-#33): single-path yt_subs→Qwen3 or lrclib-line-level
- v2 (#34/#35): ensemble orchestrator + AutoSubProvider + Claude text-merge
- v3 (#34/#35): merge prompt reworked — confidence-weighted, disagreement rule, compact output schema
- v4 (#42): description provider added as 4th text candidate (raw YouTube description → Claude extraction → candidate_texts)
```

- [ ] **Step 3: Verify no tests or scripts hard-code the old history**

Run: `grep -rn "v2.*this PR\|v3.*this PR" . --include='*.md' --include='*.rs'`
Expected: zero matches (the replacement used specific PR numbers instead).

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: update CLAUDE.md lyrics pipeline version history (v3 + v4)"
```

---

## Task 13: CI quality-regression fail gate

**Files:**
- Modify: `.github/workflows/ci.yml`

Add a CI step that fails the build if post-deploy `avg_confidence_mean` has regressed by more than `0.02` relative to the pre-deploy baseline. This closes the green-CI-theater gap that let PR #38 ship with a −17% regression.

- [ ] **Step 1: Locate the existing "Lyrics Quality Report" job**

Open `.github/workflows/ci.yml`. Search for `Lyrics Quality Report` — there's an existing job that runs `measure_lyrics_quality.py` before and after deploy and uploads the comparison as an artifact. The new fail-gate step is ADDED to that job (not a new job) so it runs in the same step ordering: baseline → deploy → 30-min wait → after snapshot → **new fail step** → upload comparison artifact.

- [ ] **Step 2: Add the fail-on-regression step**

After the existing "Generate comparison report" step (the one that prints `"## Pipeline improvement: ..."`) and before the "Upload comparison report" step, insert:

```yaml
      - name: Fail on quality regression
        shell: python {0}
        run: |
            import json
            import pathlib
            import sys

            before_path = pathlib.Path(r"C:\ProgramData\SongPlayer\baseline_before.json")
            after_path = pathlib.Path(r"C:\ProgramData\SongPlayer\measure_after.json")
            TOLERANCE = 0.02

            try:
                before = json.loads(before_path.read_text(encoding="utf-8"))["aggregate"]
                after = json.loads(after_path.read_text(encoding="utf-8"))["aggregate"]
            except Exception as e:
                print(f"cannot read snapshots (first deploy?): {e}", file=sys.stderr)
                sys.exit(0)  # soft-skip on first deploy where baseline is absent

            b = before.get("avg_confidence_mean")
            a = after.get("avg_confidence_mean")
            if b is None or a is None:
                print(f"avg_confidence_mean missing from snapshot: before={b}, after={a}",
                      file=sys.stderr)
                sys.exit(0)  # soft-skip if the metric wasn't computed

            print(f"avg_confidence_mean: {b:.3f} -> {a:.3f} (tolerance {TOLERANCE})")
            if a < b - TOLERANCE:
                print(
                    f"REGRESSION: avg_confidence_mean dropped by more than {TOLERANCE}",
                    file=sys.stderr,
                )
                sys.exit(1)
            print("OK: no regression")
```

Indentation must match the surrounding YAML (4 spaces inside `steps:`). Use the existing step structure as a visual template — the indent of `name:` and `run:` must line up with the neighboring steps.

- [ ] **Step 3: Validate YAML syntax**

Run: `python -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))"`
Expected: exit code 0 (no output). If it complains about mapping/indentation, fix the indent of the new block.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci(lyrics): fail build on avg_confidence_mean regression > 0.02"
```

---

## Task 14: Local sanity, push, CI monitor

**Files:**
- None (operational).

- [ ] **Step 1: Run the full lyrics + AI test suite**

```
cargo test -p sp-server lyrics ai
```
Expected: all pass. If a test breaks unexpectedly, read the failure carefully — most likely a hard-coded version number or a worker-struct field drift from Task 10.

- [ ] **Step 2: cargo fmt check**

```
cargo fmt --all --check
```
Expected: clean. If not, run `cargo fmt --all` and commit the fmt change in its own commit.

- [ ] **Step 3: Push**

```
git push origin dev
```

- [ ] **Step 4: Monitor CI to terminal state**

Use the airuleset `ci-monitoring` pattern — single background `sleep N && gh run view <run-id>` poll, NOT `/loop` or a custom monitor script. Example:

```
gh run list --branch dev --limit 1 --json databaseId --jq '.[0].databaseId'
# then:
# Bash(run_in_background: true, command: "sleep 300 && gh run view <id> --json status,conclusion,jobs")
```

Expected: all jobs green, including the new "Fail on quality regression" step. The new step may emit `cannot read snapshots (first deploy?)` and soft-skip with exit 0 — that's expected behavior on the first deploy of this code (baseline may not yet exist). It's still correct behavior — the next deploy with both snapshots present will enforce the gate.

If CI fails because of a **real** quality regression caught by the new step, that's the gate working. DO NOT merge. Investigate which songs regressed and re-tune.

If Mutation Testing finds survivors, fix them in a new commit (pattern: pin the mutation with a direct test, or extract arithmetic/comparisons into a helper and test the helper directly — see PR #38 commits `7926b27` and `b21b01b` for the precedent).

---

## Task 15: Post-deploy 24–48h acceptance measurement

**Files:**
- None (acceptance operation on win-resolume).

This task only starts AFTER Task 14's CI is green AND CI has deployed the new binary to win-resolume via the existing Deploy job.

- [ ] **Step 1: Capture the post-deploy baseline**

On win-resolume via the `win-resolume` MCP Shell, run:

```powershell
$py = "python"
$cache = "C:\ProgramData\SongPlayer\cache\tools"
& $py "$cache\measure_lyrics_quality.py" > "C:\ProgramData\SongPlayer\baseline_post_v4.json"
Get-Content "C:\ProgramData\SongPlayer\baseline_post_v4.json" | ConvertFrom-Json | Select-Object -ExpandProperty aggregate
```

Expected initial value: `avg_confidence_mean` ≈ 0.524 (the post-PR#38 regressed state — should not have changed yet because the stale bucket just got filled).

- [ ] **Step 2: Let the worker reprocess for 24–48 hours**

The 3-bucket queue processes:
- bucket 0 (manual): empty normally
- bucket 1 (null-lyrics): existing work
- bucket 2 (stale, `version < 4`): now contains all ~47 songs

Expected throughput: ~4–5 min/song. Catalog drain time: ~4 hours. Buffer extends to 24–48h to allow reprocess across all cached songs (including the ~180 in bucket 1).

No action required during this window beyond periodically checking `/api/v1/lyrics/queue` counts or the `/lyrics` dashboard to monitor progress.

- [ ] **Step 3: Take the final measurement**

```powershell
& $py "$cache\measure_lyrics_quality.py" > "C:\ProgramData\SongPlayer\measure_final_v4.json"
Get-Content "C:\ProgramData\SongPlayer\measure_final_v4.json" | ConvertFrom-Json | Select-Object -ExpandProperty aggregate
```

- [ ] **Step 4: Report the delta**

Report to the user:
- Pre-PR#38 baseline: 0.631
- Post-PR#38 (regressed): 0.524
- Post-this-PR: **<measured value>**
- Target: ≥ 0.65 (exceeds pre-PR#38 baseline by 2% tolerance)
- Source distribution: how many of the 47 now include `description` as an ensemble component
- Per-song improvements: list songs whose quality score rose the most, and any that regressed

- [ ] **Step 5: Decision point**

- **If `avg_confidence_mean ≥ 0.65`**: report to the user that this PR is ready to merge AND that PR #38 is now unblocked (post-this-PR-merge, PR #38 can be merged too).
- **If `< 0.65`**: do NOT recommend merge. Sample specific songs' alignment audit logs (`cache/<youtube_id>_alignment_audit.json`) to identify where the merge is still losing confidence. Possible follow-ups: tune `text_merge.rs` to weight description higher, raise `pass_through_baseline` from 0.7 to 0.85, or reduce the noise floor on Qwen3. Open a new spec/plan cycle for the tuning work; do NOT merge this PR until the gate hits the target.

- [ ] **Step 6: Create a GitHub issue for any follow-up work identified in Step 5**

If Step 5 surfaces specific regressions that need follow-up:

```bash
gh issue create --title "Lyrics quality tuning — <specific issue>" --body "<findings + proposed fixes>"
```

---

## Verification

After all 15 tasks:

1. `cargo test -p sp-server` passes (all suites, including lyrics + description_provider + worker integration).
2. `cargo fmt --all --check` is clean.
3. CI on `dev` for the final commit is entirely green, including Mutation Testing + the new "Fail on quality regression" step.
4. `LYRICS_PIPELINE_VERSION` is 4 in `crates/sp-server/src/lyrics/mod.rs`.
5. `CLAUDE.md` history lists v1 through v4 accurately.
6. Description cache files (`{id}_description.txt`, `{id}_description_lyrics.json`) are being written to the real cache dir on win-resolume (spot check via the MCP Shell).
7. Post-deploy measurement after 24–48h shows `avg_confidence_mean ≥ 0.65`.
8. Per-song source distribution includes `description` as a contributor in the reconciled reference text.

Once verified, report to the user. Await explicit "merge it" instruction per airuleset `pr-merge-policy` — green CI and a passing measurement are BOTH necessary but NEITHER is permission to merge.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-04-18-youtube-description-lyrics-provider.md`.

**Execution modes:**

1. **Subagent-Driven (recommended — default per airuleset `ask-before-assuming`)** — I dispatch a fresh subagent per task, two-stage review between tasks (spec-compliance first, then code-quality), fast iteration.

2. **Inline Execution** — execute tasks in this session using `superpowers:executing-plans`, batch execution with checkpoints.

Which approach?
