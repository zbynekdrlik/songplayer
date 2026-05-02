# Spotify Auto-Resolver + Priority Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace PR #70's manual Spotify URL UI with Claude-based auto-resolution running inside the lyrics worker. Bundle the `claude_merge::best_authoritative` priority fix so resolved Spotify lyrics actually win over longer-but-noisier candidates.

**Architecture:** Lazy per-song resolver — when the worker starts on a song with `spotify_track_id IS NULL AND spotify_resolved_at IS NULL`, ask Claude for the canonical Spotify track ID, verify by fetching the existing public proxy, persist with a timestamp so the gate short-circuits subsequent attempts. One Claude call per song lifetime; cached via the `spotify_resolved_at` column.

**Tech Stack:** Rust 2024, sqlx 0.8, reqwest 0.12, wiremock 0.6, serial_test 3, existing `AiClient` (CLIProxyAPI / Anthropic).

**Spec:** `docs/superpowers/specs/2026-05-02-spotify-auto-resolver-design.md` (commit `f757874`).

---

## Context for every implementer subagent

Pass these rules verbatim in the dispatch prompt — do not paraphrase.

**Branch + working dir:**

- Branch `dev` on `/home/newlevel/devel/songplayer`. VERSION `0.30.0-dev.1` already; do NOT bump.
- Spec at `f757874`. Plan committed before Phase A starts.

**Airuleset rules:**

- TDD strict: failing test first → trust by inspection on Rust → implement → trust by inspection → `cargo fmt --all --check` (the ONLY local cargo command allowed) → commit on green.
- NEVER run `cargo clippy/test/build/check` locally. Rely on CI.
- File-size cap 1000 lines per file.
- One commit per "Commit" step. The plan body counts commits.
- `mutants::skip` requires a one-line justification inline.
- Do NOT push — controller batches and pushes once per phase.

**Feedback-memory rules:**

- `feedback_no_legacy_code.md` — when replacing a code path, delete the old one entirely. The PR #70 manual-UI code MUST be deleted, not deprecated.
- `feedback_pipeline_version_approval.md` — do NOT bump LYRICS_PIPELINE_VERSION.
- `feedback_line_timing_only.md` and `feedback_no_even_distribution.md` — Spotify line-only output ships `words: None`.
- `feedback_no_autosub.md` — autosub stays banned.
- `feedback_llm_over_heuristics.md` — single LLM call for messy mapping; this resolver is exactly that.
- `feedback_event_status_user_authoritative.md` — never infer event status from scene state.
- `feedback_cliproxyapi_model.md` — use `claude-sonnet-4-20250514` (not opus-4-6 which returns empty via OAuth); always set `max_tokens`. The existing `AiClient::chat` already sets `max_tokens: 32000`. Do NOT introduce a new client; reuse `AiClient`.

**Critical wiring (already on disk, do NOT re-implement):**

- `crates/sp-server/src/lyrics/spotify_proxy.rs::SpotifyLyricsFetcher` exists. The new resolver USES it for verification. Do NOT touch the fetcher.
- `claude_merge::source_priority` already maps `tier1:spotify` to 4. Do NOT touch.
- `videos.spotify_track_id` exists from V17. V18 adds `spotify_resolved_at`.
- `crate::ai::client::AiClient::chat(&self, system: &str, user: &str) -> anyhow::Result<String>` is the call site. See `crates/sp-server/src/ai/client.rs:29`.
- The migration runner in `db/mod.rs::run_migrations` splits the SQL string by `;` and executes each statement separately, so V18 can have multiple statements.

**Two-stage code review per task:**

- After implementer reports DONE, dispatch the spec compliance reviewer (must approve), then the code quality reviewer (must approve). Both must approve before marking the task complete.

---

## Phase A — DB migration V18 + VideoLyricsRow plumbing

### Task A.1: V18 migration, struct field, 3 SELECTs, test literal

**Files:**

- Modify: `crates/sp-server/src/db/mod.rs` — add V18 migration constant + register in `MIGRATIONS` table.
- Modify: `crates/sp-server/src/db/models.rs` — add `spotify_resolved_at: Option<String>` to `VideoLyricsRow`.
- Modify: `crates/sp-server/src/lyrics/reprocess.rs` — extend 3 SELECT statements to pull `v.spotify_resolved_at`.
- Modify: `crates/sp-server/src/lyrics/worker_tests.rs` — extend 2 `VideoLyricsRow { ... }` literals with `spotify_resolved_at: None`.

**Model:** haiku.

- [ ] **Step 1: Register V18 in the migrations table**

In `crates/sp-server/src/db/mod.rs`, locate the `MIGRATIONS` constant (lines 12-29). Find the line `(17, MIGRATION_V17),` (currently line 28). Add a new line right after it:

```rust
    (18, MIGRATION_V18),
```

The closing `];` is on line 29. The new array should look like:

```rust
const MIGRATIONS: &[(i32, &str)] = &[
    (1, MIGRATION_V1),
    // ... 2 through 17 unchanged ...
    (17, MIGRATION_V17),
    (18, MIGRATION_V18),
];
```

- [ ] **Step 2: Add the V18 migration body**

In the same file, locate `MIGRATION_V17` (currently line 243). Append AFTER it:

```rust
const MIGRATION_V18: &str = "
ALTER TABLE videos ADD COLUMN spotify_resolved_at TIMESTAMP;
UPDATE videos SET spotify_resolved_at = datetime('now') WHERE spotify_track_id IS NOT NULL;
";
```

The second statement backfills `spotify_resolved_at` for rows where `spotify_track_id` is already set (the 5 manually-resolved songs from PR #70). Without this, the worker's gate would think those rows are "never tried" and re-run Claude on them.

The migration runner splits on `;` and executes each statement, so this works as-is.

- [ ] **Step 3: Extend `VideoLyricsRow`**

In `crates/sp-server/src/db/models.rs`, locate the `VideoLyricsRow` struct (currently lines 387-407, with `spotify_track_id: Option<String>` already added by PR #70). Add a new field:

```rust
    /// V18 column. Set to `datetime('now')` whenever the Spotify resolver runs
    /// on this song (success OR no-match). NULL means "never attempted." Worker
    /// gate uses (spotify_track_id IS NULL AND spotify_resolved_at IS NULL) to
    /// decide whether to ask Claude.
    pub spotify_resolved_at: Option<String>,
```

Place it right after `pub spotify_track_id: Option<String>,` (the last existing field).

- [ ] **Step 4: Extend the 3 SELECTs in `reprocess.rs`**

In `crates/sp-server/src/lyrics/reprocess.rs`, three SELECT strings end with `v.spotify_track_id` (lines 44, 90, 130 after PR #70's plumbing). Append `, v.spotify_resolved_at` to each. Use `Edit` with `replace_all: true` on the unique pattern:

```rust
                v.spotify_track_id \
```

Replace with:

```rust
                v.spotify_track_id, v.spotify_resolved_at \
```

This updates all three SELECTs in one edit.

- [ ] **Step 5: Extend the 2 `VideoLyricsRow` literals in `worker_tests.rs`**

In `crates/sp-server/src/lyrics/worker_tests.rs`, two `VideoLyricsRow { ... }` literals exist (around lines 236-247 and 325-336 after PR #70). Each ends with:

```rust
        spotify_track_id: None,
    };
```

Replace both with:

```rust
        spotify_track_id: None,
        spotify_resolved_at: None,
    };
```

Use `Edit` with `replace_all: true` on the unique 2-line pattern.

- [ ] **Step 6: Verify formatting**

Run: `cargo fmt --all --check`. Expected: exit 0.

- [ ] **Step 7: Commit**

```bash
git add crates/sp-server/src/db/mod.rs \
        crates/sp-server/src/db/models.rs \
        crates/sp-server/src/lyrics/reprocess.rs \
        crates/sp-server/src/lyrics/worker_tests.rs
git commit -m "feat(db): V18 migration adds spotify_resolved_at + plumbing for #73"
```

---

### Phase A push (controller-only)

Controller (NOT a subagent dispatch) runs `git push origin dev` and monitors CI to terminal state. Do NOT proceed to Phase B until Phase A's CI is green.

---

## Phase B — SpotifyResolver module

### Task B.1: SpotifyResolver new module + prompt builder + outcome parser + unit tests

**Files:**

- Create: `crates/sp-server/src/lyrics/spotify_resolver.rs`.
- Modify: `crates/sp-server/src/lyrics/mod.rs` — register the new module.

**Model:** sonnet.

- [ ] **Step 1: Add the module declaration**

In `crates/sp-server/src/lyrics/mod.rs`, find the existing `pub mod` declarations and add (alphabetically alongside `spotify_proxy`):

```rust
pub mod spotify_resolver;
```

Look at how `spotify_proxy` is declared and match the pattern.

- [ ] **Step 2: Create the resolver file with prompt builder + parser + outcome enum**

Create `crates/sp-server/src/lyrics/spotify_resolver.rs`:

```rust
//! Spotify track ID auto-resolver.
//!
//! Per `feedback_llm_over_heuristics.md` — uses a single Claude call to map
//! a YouTube song to its canonical Spotify track ID. Replaces PR #70's
//! manual UI: operators don't paste URLs anymore.
//!
//! The resolver is gated by the worker: it only fires when both
//! `spotify_track_id IS NULL` and `spotify_resolved_at IS NULL`. Once a
//! resolution attempt is recorded (success OR no-match), the gate keeps
//! the worker from re-querying Claude on every reprocess.

use anyhow::Result;

use crate::ai::client::AiClient;
use crate::lyrics::spotify_proxy::SpotifyLyricsFetcher;

/// `TIER1_MIN_LINES` from `tier1.rs` — Spotify must return at least this many
/// LINE_SYNCED lines to count as a real match. A track returning 3 lines is
/// almost certainly a wrong-track or instrumental match.
const MIN_VERIFIED_LINES: usize = 10;

/// Outcome of a single resolve attempt.
#[derive(Debug, Clone)]
pub enum ResolveOutcome {
    /// Claude returned a 22-char ID and the proxy verified ≥10 LINE_SYNCED lines.
    Resolved(String),
    /// Claude returned `NONE` literally, OR Claude returned an ID but the proxy
    /// failed to verify (404 / error:true / non-LINE_SYNCED / <10 lines).
    /// Caller persists `spotify_track_id = NULL, spotify_resolved_at = now()`.
    NoMatch,
    /// Transient failure (HTTP error, timeout, parse error). Caller MUST NOT
    /// persist `spotify_resolved_at` — leaves the row eligible for retry on
    /// the next worker pass.
    Error(anyhow::Error),
}

pub struct SpotifyResolver {
    fetcher: SpotifyLyricsFetcher,
}

impl Default for SpotifyResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl SpotifyResolver {
    pub fn new() -> Self {
        Self {
            fetcher: SpotifyLyricsFetcher::new(),
        }
    }

    /// Build the prompt that asks Claude for the canonical Spotify track ID.
    ///
    /// Prompt design rationale:
    /// - Constrain output strictly: 22-char alphanumeric ID OR the literal
    ///   string `NONE`. Anything else is treated as `NoMatch`.
    /// - Explicitly tell Claude to return `NONE` for cover/remix/live versions
    ///   that aren't on Spotify as the canonical recording.
    /// - Give Claude all four fields (song, artist, youtube_title, youtube_id)
    ///   so it can disambiguate versions.
    pub(crate) fn build_prompt(
        song: &str,
        artist: &str,
        youtube_title: &str,
        youtube_id: &str,
    ) -> (String, String) {
        let system = "You map YouTube videos to canonical Spotify track IDs. \
                      Reply with EXACTLY one of: a 22-character base62 Spotify track ID, or the literal string NONE. \
                      No prose. No explanation. No markdown."
            .to_string();
        let user = format!(
            "song: {song}\n\
             artist: {artist}\n\
             youtube_title: {youtube_title}\n\
             youtube_id: {youtube_id}\n\
             \n\
             Return the canonical Spotify track ID for this exact recording. \
             If this is a cover, remix, live performance, or instrumental that does NOT exist on Spotify as the canonical version, return NONE. \
             If you are not certain, return NONE."
        );
        (system, user)
    }

    /// Parse a Claude reply.
    ///
    /// Acceptable inputs (all case-insensitive on `NONE`):
    /// - `"3n3Ppam7vgaVa1iaRUc9Lp"` → `Some("3n3Ppam7vgaVa1iaRUc9Lp")`
    /// - `"  3n3Ppam7vgaVa1iaRUc9Lp  "` → `Some(...)` (trimmed)
    /// - `"NONE"`, `"none"`, `"None"` → `None`
    /// - Anything else (too short / too long / non-alphanumeric / multiple lines) → `None`
    pub(crate) fn parse_reply(raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.eq_ignore_ascii_case("NONE") {
            return None;
        }
        if trimmed.len() == 22 && trimmed.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Some(trimmed.to_string());
        }
        None
    }

    /// Resolve a song. Single Claude call + single proxy verification call.
    /// Returns `ResolveOutcome::Resolved(id)` on a confirmed canonical match,
    /// `NoMatch` on Claude-NONE or proxy-rejected, `Error` on transport-level
    /// failure (which the caller treats as "retry next time").
    pub async fn resolve(
        &self,
        ai_client: &AiClient,
        song: &str,
        artist: &str,
        youtube_title: &str,
        youtube_id: &str,
    ) -> ResolveOutcome {
        let (system, user) = Self::build_prompt(song, artist, youtube_title, youtube_id);

        let raw = match ai_client.chat(&system, &user).await {
            Ok(s) => s,
            Err(e) => return ResolveOutcome::Error(e),
        };

        let candidate = match Self::parse_reply(&raw) {
            Some(id) => id,
            None => return ResolveOutcome::NoMatch,
        };

        match self.fetcher.fetch(&candidate).await {
            Ok(Some(track)) if track.lines.len() >= MIN_VERIFIED_LINES => {
                ResolveOutcome::Resolved(candidate)
            }
            Ok(_) => ResolveOutcome::NoMatch,
            Err(_) => ResolveOutcome::NoMatch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reply_accepts_canonical_22char_id() {
        assert_eq!(
            SpotifyResolver::parse_reply("3n3Ppam7vgaVa1iaRUc9Lp"),
            Some("3n3Ppam7vgaVa1iaRUc9Lp".to_string())
        );
    }

    #[test]
    fn parse_reply_trims_whitespace() {
        assert_eq!(
            SpotifyResolver::parse_reply("  3n3Ppam7vgaVa1iaRUc9Lp  "),
            Some("3n3Ppam7vgaVa1iaRUc9Lp".to_string())
        );
    }

    #[test]
    fn parse_reply_returns_none_for_literal_NONE_uppercase() {
        assert_eq!(SpotifyResolver::parse_reply("NONE"), None);
    }

    #[test]
    fn parse_reply_returns_none_for_lowercase_none() {
        assert_eq!(SpotifyResolver::parse_reply("none"), None);
    }

    #[test]
    fn parse_reply_returns_none_for_mixed_case_None() {
        assert_eq!(SpotifyResolver::parse_reply("None"), None);
    }

    #[test]
    fn parse_reply_returns_none_for_too_short() {
        assert_eq!(SpotifyResolver::parse_reply("3n3Ppam7vga"), None);
    }

    #[test]
    fn parse_reply_returns_none_for_too_long() {
        assert_eq!(SpotifyResolver::parse_reply("3n3Ppam7vgaVa1iaRUc9LpXXX"), None);
    }

    #[test]
    fn parse_reply_returns_none_for_invalid_chars() {
        assert_eq!(SpotifyResolver::parse_reply("3n3Ppam7vga!a1iaRUc9Lp"), None);
    }

    #[test]
    fn parse_reply_returns_none_for_prose_response() {
        // Defensive: if Claude ignores the "no prose" instruction.
        assert_eq!(
            SpotifyResolver::parse_reply("Sorry, I cannot determine the track ID"),
            None
        );
    }

    #[test]
    fn build_prompt_includes_all_four_fields() {
        let (system, user) = SpotifyResolver::build_prompt(
            "Amazing Grace",
            "Chris Tomlin",
            "Amazing Grace (My Chains Are Gone)",
            "dQw4w9WgXcQ",
        );
        assert!(system.contains("22-character"));
        assert!(system.contains("NONE"));
        assert!(user.contains("Amazing Grace"));
        assert!(user.contains("Chris Tomlin"));
        assert!(user.contains("My Chains Are Gone"));
        assert!(user.contains("dQw4w9WgXcQ"));
    }

    #[test]
    fn min_verified_lines_matches_tier1_threshold() {
        // The constant must stay in sync with tier1.rs::TIER1_MIN_LINES (10).
        // If the tier1 short-circuit threshold changes, this test forces a
        // conscious decision about whether the resolver should match.
        assert_eq!(MIN_VERIFIED_LINES, 10);
        assert_eq!(MIN_VERIFIED_LINES, crate::lyrics::tier1::TIER1_MIN_LINES);
    }
}
```

- [ ] **Step 3: Verify formatting**

Run: `cargo fmt --all --check`. Expected: exit 0.

- [ ] **Step 4: Verify file size**

Run: `wc -l crates/sp-server/src/lyrics/spotify_resolver.rs`. Expected: well under 1000 lines (~190).

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/lyrics/mod.rs \
        crates/sp-server/src/lyrics/spotify_resolver.rs
git commit -m "feat(lyrics): add SpotifyResolver for Claude-based Spotify track ID resolution"
```

---

### Task B.2: Wiremock integration tests for `SpotifyResolver::resolve`

**Files:**

- Modify: `crates/sp-server/src/lyrics/spotify_resolver.rs` — append a second `#[cfg(test)] mod integration_tests` block at the bottom.

**Model:** sonnet.

**Why a separate task from B.1:** B.1 ships pure-function unit tests (parser, prompt builder, constants). B.2 stands up wiremock servers for both Claude and the Spotify proxy, exercising the full async `resolve()` flow with `#[serial_test::serial]` because it shares the env-var-overridable proxy base from PR #70 (`SPOTIFY_LYRICS_PROXY_BASE`).

- [ ] **Step 1: Append the integration test module**

At the very bottom of `crates/sp-server/src/lyrics/spotify_resolver.rs`, after the existing `#[cfg(test)] mod tests { ... }` block, append:

```rust
#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::ai::AiSettings;

    fn ai_client_pointed_at(uri: &str) -> AiClient {
        AiClient::new(AiSettings {
            api_url: format!("{uri}/v1"),
            api_key: Some("test".into()),
            model: "stub".into(),
            system_prompt_extra: None,
        })
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolves_when_claude_returns_id_and_proxy_verifies() {
        let claude_mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "3n3Ppam7vgaVa1iaRUc9Lp"
                    }
                }]
            })))
            .mount(&claude_mock)
            .await;

        let proxy_mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/"))
            .and(wiremock::matchers::query_param("trackid", "3n3Ppam7vgaVa1iaRUc9Lp"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error": false,
                "syncType": "LINE_SYNCED",
                "lines": (0..12).map(|i| serde_json::json!({
                    "startTimeMs": format!("{}", i * 1000),
                    "words": format!("line {i}"),
                })).collect::<Vec<_>>()
            })))
            .mount(&proxy_mock)
            .await;
        // SAFETY: marked serial above; no other test races on this env var while
        // this test runs.
        unsafe { std::env::set_var("SPOTIFY_LYRICS_PROXY_BASE", proxy_mock.uri()); }

        let resolver = SpotifyResolver::new();
        let ai = ai_client_pointed_at(&claude_mock.uri());
        let outcome = resolver
            .resolve(&ai, "Test Song", "Test Artist", "Test Title", "aaaaaaaaaaa")
            .await;

        match outcome {
            ResolveOutcome::Resolved(id) => assert_eq!(id, "3n3Ppam7vgaVa1iaRUc9Lp"),
            other => panic!("expected Resolved, got {other:?}"),
        }

        unsafe { std::env::remove_var("SPOTIFY_LYRICS_PROXY_BASE"); }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn no_match_when_claude_returns_none() {
        let claude_mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "NONE" }
                }]
            })))
            .mount(&claude_mock)
            .await;

        let resolver = SpotifyResolver::new();
        let ai = ai_client_pointed_at(&claude_mock.uri());
        let outcome = resolver
            .resolve(&ai, "Test Song", "Test Artist", "Test Title", "aaaaaaaaaaa")
            .await;

        assert!(
            matches!(outcome, ResolveOutcome::NoMatch),
            "expected NoMatch on Claude NONE, got {outcome:?}"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn no_match_when_proxy_returns_404() {
        let claude_mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "3n3Ppam7vgaVa1iaRUc9Lp" }
                }]
            })))
            .mount(&claude_mock)
            .await;

        let proxy_mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&proxy_mock)
            .await;
        unsafe { std::env::set_var("SPOTIFY_LYRICS_PROXY_BASE", proxy_mock.uri()); }

        let resolver = SpotifyResolver::new();
        let ai = ai_client_pointed_at(&claude_mock.uri());
        let outcome = resolver
            .resolve(&ai, "Test Song", "Test Artist", "Test Title", "aaaaaaaaaaa")
            .await;

        assert!(
            matches!(outcome, ResolveOutcome::NoMatch),
            "expected NoMatch on proxy 404, got {outcome:?}"
        );
        unsafe { std::env::remove_var("SPOTIFY_LYRICS_PROXY_BASE"); }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn no_match_when_proxy_returns_too_few_lines() {
        let claude_mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "3n3Ppam7vgaVa1iaRUc9Lp" }
                }]
            })))
            .mount(&claude_mock)
            .await;

        let proxy_mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error": false,
                "syncType": "LINE_SYNCED",
                "lines": [
                    {"startTimeMs": "0",    "words": "only"},
                    {"startTimeMs": "1000", "words": "three"},
                    {"startTimeMs": "2000", "words": "lines"}
                ]
            })))
            .mount(&proxy_mock)
            .await;
        unsafe { std::env::set_var("SPOTIFY_LYRICS_PROXY_BASE", proxy_mock.uri()); }

        let resolver = SpotifyResolver::new();
        let ai = ai_client_pointed_at(&claude_mock.uri());
        let outcome = resolver
            .resolve(&ai, "Test Song", "Test Artist", "Test Title", "aaaaaaaaaaa")
            .await;

        assert!(
            matches!(outcome, ResolveOutcome::NoMatch),
            "expected NoMatch on <10 lines, got {outcome:?}"
        );
        unsafe { std::env::remove_var("SPOTIFY_LYRICS_PROXY_BASE"); }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn error_when_claude_transport_fails() {
        let claude_mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&claude_mock)
            .await;

        let resolver = SpotifyResolver::new();
        let ai = ai_client_pointed_at(&claude_mock.uri());
        let outcome = resolver
            .resolve(&ai, "Test Song", "Test Artist", "Test Title", "aaaaaaaaaaa")
            .await;

        assert!(
            matches!(outcome, ResolveOutcome::Error(_)),
            "expected Error on Claude HTTP 500, got {outcome:?}"
        );
    }
}
```

- [ ] **Step 2: Verify formatting**

Run: `cargo fmt --all --check`. Expected: exit 0.

- [ ] **Step 3: Verify file size**

Run: `wc -l crates/sp-server/src/lyrics/spotify_resolver.rs`. Expected: ~400 lines, well under 1000.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/spotify_resolver.rs
git commit -m "test(lyrics): wiremock-cover SpotifyResolver::resolve outcomes"
```

---

### Phase B push (controller-only)

Controller pushes + monitors CI to terminal state. Block before C.1.

---

## Phase C — Worker pre-gather hook

### Task C.1: Wire the resolver into `worker.rs::process_song`

**Files:**

- Modify: `crates/sp-server/src/lyrics/worker.rs` — add a pre-gather step that runs the resolver when the gate condition holds.
- Modify: `crates/sp-server/src/db/models.rs` — add a helper that atomically writes both `spotify_track_id` and `spotify_resolved_at`.

**Model:** haiku.

- [ ] **Step 1: Add the DB helper**

In `crates/sp-server/src/db/models.rs`, after the existing `set_video_spotify_track_id` helper (currently around line 163), add:

```rust
/// Atomically record the outcome of a Spotify resolution attempt. Sets both
/// `spotify_track_id` and `spotify_resolved_at = datetime('now')` in one
/// statement. Pass `None` for the track ID to record a no-match attempt.
///
/// Returns the number of rows affected (0 = no row with that id).
pub async fn set_video_spotify_resolution(
    pool: &SqlitePool,
    video_id: i64,
    spotify_track_id: Option<&str>,
) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "UPDATE videos SET spotify_track_id = ?1, spotify_resolved_at = datetime('now') WHERE id = ?2",
    )
    .bind(spotify_track_id)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}
```

- [ ] **Step 2: Read the worker's existing `process_song` to find the gather call site**

Run: `grep -nE "gather_sources_impl|let row = " crates/sp-server/src/lyrics/worker.rs | head -10`

The hook goes IMMEDIATELY BEFORE the call to `gather_sources_impl`. After your read locates the call, identify the line and proceed.

- [ ] **Step 3: Add the resolver field on the worker + construct it**

In `crates/sp-server/src/lyrics/worker.rs`, locate the `LyricsWorker` struct (currently around line 35-45 — the struct that has `ai_client: Option<Arc<AiClient>>`). Add a field:

```rust
    /// Spotify track ID auto-resolver. Constructed once at worker startup.
    /// Per-song, the worker checks the gate (spotify_track_id IS NULL AND
    /// spotify_resolved_at IS NULL) before invoking it.
    spotify_resolver: crate::lyrics::spotify_resolver::SpotifyResolver,
```

In the worker's `new(...)` constructor (currently around line 60-90 — the function that takes `ai_client: Option<Arc<AiClient>>`), add the field initializer:

```rust
            spotify_resolver: crate::lyrics::spotify_resolver::SpotifyResolver::new(),
```

inside the `Self { ... }` literal alongside the other field initializers.

- [ ] **Step 4: Add the pre-gather hook**

In `process_song` — IMMEDIATELY BEFORE the `gather_sources_impl(...)` call — add:

```rust
        // Spotify auto-resolution gate (#73). Run Claude once per song lifetime
        // when `spotify_track_id` is NULL and we've never recorded an attempt.
        // The result (success OR no-match) is persisted with a timestamp so the
        // gate short-circuits on subsequent reprocesses.
        if row.spotify_track_id.is_none() && row.spotify_resolved_at.is_none() {
            if let Some(ai_client) = self.ai_client.as_deref() {
                if !row.song.is_empty() && !row.artist.is_empty() {
                    use crate::lyrics::spotify_resolver::ResolveOutcome;
                    let title_str = row.title.clone().unwrap_or_default();
                    let outcome = self
                        .spotify_resolver
                        .resolve(ai_client, &row.song, &row.artist, &title_str, &row.youtube_id)
                        .await;
                    match outcome {
                        ResolveOutcome::Resolved(id) => {
                            if let Err(e) = crate::db::models::set_video_spotify_resolution(
                                &self.pool,
                                row.id,
                                Some(&id),
                            )
                            .await
                            {
                                warn!(
                                    "worker: failed to persist resolved spotify_track_id for {}: {e}",
                                    row.youtube_id
                                );
                            } else {
                                info!(
                                    youtube_id = %row.youtube_id,
                                    track_id = %id,
                                    "spotify_resolver: resolved + verified"
                                );
                                row.spotify_track_id = Some(id);
                            }
                        }
                        ResolveOutcome::NoMatch => {
                            if let Err(e) = crate::db::models::set_video_spotify_resolution(
                                &self.pool,
                                row.id,
                                None,
                            )
                            .await
                            {
                                warn!(
                                    "worker: failed to persist no-match for {}: {e}",
                                    row.youtube_id
                                );
                            } else {
                                debug!(
                                    youtube_id = %row.youtube_id,
                                    "spotify_resolver: no canonical match"
                                );
                            }
                        }
                        ResolveOutcome::Error(e) => {
                            warn!(
                                "worker: spotify resolution transport error for {}: {e}",
                                row.youtube_id
                            );
                            // Intentionally do NOT persist resolved_at — leaves
                            // the row eligible for retry on the next worker pass.
                        }
                    }
                }
            }
        }
```

The `if !row.song.is_empty() && !row.artist.is_empty()` guard avoids wasting Claude calls on songs that lack the metadata Claude needs to find the track. (These songs would also fail LRCLIB / Genius for the same reason — they're upstream of the metadata-extraction step.)

The `row.spotify_track_id = Some(id)` mutation is local-only; it primes `gather_sources_impl` so it sees the just-resolved ID without re-fetching from DB. Worker iteration boundary is per-song, so this mutation doesn't leak.

`row` must be `mut` for the local-mutation line to compile. If it's currently `let row = ...`, change to `let mut row = ...` (typically already mut since the worker mutates several fields). If not, add the `mut`.

- [ ] **Step 5: Verify formatting**

Run: `cargo fmt --all --check`. Expected: exit 0.

- [ ] **Step 6: Verify file size**

Run: `wc -l crates/sp-server/src/lyrics/worker.rs`. Expected: under 1000 lines (worker.rs was ~720 lines after PR #70's vocals delete removal; this hook adds ~50 lines, lands ~770).

- [ ] **Step 7: Commit**

```bash
git add crates/sp-server/src/lyrics/worker.rs \
        crates/sp-server/src/db/models.rs
git commit -m "feat(lyrics): wire SpotifyResolver into worker pre-gather hook for #73"
```

---

### Phase C push (controller-only)

Controller pushes + monitors CI to terminal state. Block before D.1.

---

## Phase D — `claude_merge::best_authoritative` priority fix

### Task D.1: Rank by `(source_priority, lines.len())` instead of `(lines.len(), source_priority)`

**Files:**

- Modify: `crates/sp-server/src/lyrics/claude_merge.rs:157` — flip the tuple order in `max_by_key`.
- Add tests demonstrating the new ranking.

**Model:** haiku.

- [ ] **Step 1: Write the failing tests**

In `crates/sp-server/src/lyrics/claude_merge.rs`, find the existing `// ── best_authoritative tests ──` section (currently around line 625). Append two new tests inside the same `#[cfg(test)] mod tests { ... }` block AFTER the existing `best_authoritative_uses_priority_for_tie` test:

```rust
    #[test]
    fn best_authoritative_priority_beats_longer_lower_priority_candidate() {
        // The whole point of source_priority: a high-priority short candidate
        // (e.g. tier1:spotify with 12 lines) MUST win over a longer noisy
        // low-priority candidate (e.g. yt_subs with 50 lines). Pre-fix
        // ranking was (lines.len(), priority) which got this backwards.
        let result = best_authoritative(&[
            CandidateText {
                source: "yt_subs".into(),
                lines: (0..50).map(|i| format!("yt line {i}")).collect(),
                line_timings: None,
                has_timing: false,
            },
            CandidateText {
                source: "tier1:spotify".into(),
                lines: (0..12).map(|i| format!("spotify line {i}")).collect(),
                line_timings: None,
                has_timing: false,
            },
        ]);
        assert_eq!(result.len(), 12);
        assert!(result[0].starts_with("spotify"));
    }

    #[test]
    fn best_authoritative_override_beats_spotify() {
        // Override (priority 5) is the absolute top — even short overrides
        // beat longer Spotify candidates.
        let result = best_authoritative(&[
            CandidateText {
                source: "tier1:spotify".into(),
                lines: (0..30).map(|i| format!("spotify line {i}")).collect(),
                line_timings: None,
                has_timing: false,
            },
            CandidateText {
                source: "override".into(),
                lines: vec!["op line 1".into(), "op line 2".into()],
                line_timings: None,
                has_timing: false,
            },
        ]);
        assert_eq!(result.len(), 2);
        assert!(result[0].starts_with("op line"));
    }
```

- [ ] **Step 2: Trust by inspection that the new tests fail**

The current implementation at line 157 (`max_by_key(|c| (c.lines.len(), source_priority(&c.source)))`) picks yt_subs (50 lines) over tier1:spotify (12 lines), and tier1:spotify (30 lines) over override (2 lines). Both new tests fail under this ranking.

- [ ] **Step 3: Apply the ranking fix**

In `crates/sp-server/src/lyrics/claude_merge.rs:157`, find:

```rust
        .max_by_key(|c| (c.lines.len(), source_priority(&c.source)))
```

Replace with:

```rust
        .max_by_key(|c| (source_priority(&c.source), c.lines.len()))
```

Single-line change. The tuple's first element is now the priority; SQL-style: order by priority DESC first, then by lines.len() DESC for tiebreak.

- [ ] **Step 4: Trust by inspection that all tests pass**

Walk through every test in the `best_authoritative tests` section:

- `best_authoritative_picks_most_lines` (existing): two candidates with the SAME source — ranking falls back to `lines.len()` because priority is equal. Still passes.
- `best_authoritative_uses_priority_for_tie` (existing): two candidates with the SAME `lines.len()` — ranking is decided by priority. Still passes.
- `best_authoritative_empty_returns_empty` (existing): unaffected. Still passes.
- `best_authoritative_priority_beats_longer_lower_priority_candidate` (new): now passes.
- `best_authoritative_override_beats_spotify` (new): now passes.

- [ ] **Step 5: Verify formatting**

Run: `cargo fmt --all --check`. Expected: exit 0.

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/lyrics/claude_merge.rs
git commit -m "fix(lyrics): rank best_authoritative by (priority, lines) for #72"
```

---

### Phase D push (controller-only)

Controller pushes + monitors CI to terminal state. Block before E.1.

---

## Phase E — Removals from PR #70

### Task E.1: Delete the manual UI + PATCH path + parser + tests + E2E spec

**Files:**

- DELETE: `crates/sp-server/src/api/routes_tests_spotify.rs`.
- DELETE: `e2e/tests/spotify-url-input.spec.ts`.
- Modify: `crates/sp-server/src/api/routes.rs` — drop `spotify_url` field on `PatchVideoReq`, drop the parsing block in `patch_video`, drop `parse_spotify_track_id` + `parse_spotify_tests` mod, drop the `mod tests_spotify;` declaration.
- Modify: `crates/sp-server/src/api/lyrics.rs` — drop `spotify_track_id` field on `SongListItem` + the SELECT mention + the field setters in both `list_songs` and `get_song_detail`.
- Modify: `sp-ui/src/components/live_setlist.rs` — drop the 🎵 button code block + the `spotify_track_id_initial` extraction.
- Modify: `sp-ui/src/api.rs` — drop `patch_video_spotify_url` helper.
- Modify: `sp-ui/style.css` — drop the `.live-setlist-btn-spotify` and `.live-setlist-btn-spotify.has-spotify` rules.

**Model:** haiku.

**Why one task:** all deletions are mechanical, mutually consistent, and best done atomically. Splitting into per-file tasks would create transient compile breakage between commits.

- [ ] **Step 1: Delete the test file**

```bash
git rm crates/sp-server/src/api/routes_tests_spotify.rs
```

- [ ] **Step 2: Delete the E2E spec**

```bash
git rm e2e/tests/spotify-url-input.spec.ts
```

- [ ] **Step 3: Drop the `mod tests_spotify;` declaration in `routes.rs`**

In `crates/sp-server/src/api/routes.rs`, find:

```rust
#[cfg(test)]
#[path = "routes_tests_spotify.rs"]
mod tests_spotify;
```

Delete those three lines. (The location was added in PR #70's a2ed044; should be alongside the existing `mod tests;` declaration near the bottom of the file.)

- [ ] **Step 4: Drop `spotify_url` field from `PatchVideoReq` in `routes.rs`**

In `crates/sp-server/src/api/routes.rs`, locate the `PatchVideoReq` struct (currently around lines 332-347 after PR #70). The struct currently has:

```rust
    /// Spotify track URL or bare 22-char ID. The handler extracts the
    /// track ID via `parse_spotify_track_id`. Pass `Some("")` to clear.
    /// Source for `videos.spotify_track_id` (V17 column), consumed by
    /// `gather.rs` to fetch line-synced lyrics from the proxy.
    #[serde(default)]
    pub spotify_url: Option<String>,
```

Delete the entire block (5 lines + 1 blank line above the doc comment if present). The struct should end with `pub lyrics_override_text: Option<String>,` followed by `}`.

- [ ] **Step 5: Drop the spotify parsing logic in `patch_video` handler**

In `crates/sp-server/src/api/routes.rs::patch_video` (currently lines 350-415), three blocks of code reference `spotify_url` / `spotify_track_id`:

(a) The empty-body guard:

```rust
    if req.suppress_resolume_en.is_none()
        && req.lyrics_override_text.is_none()
        && req.spotify_url.is_none()
    {
```

Replace with:

```rust
    if req.suppress_resolume_en.is_none() && req.lyrics_override_text.is_none() {
```

(b) The `resolved_spotify_track_id` block (currently lines 369-383):

```rust
    // Resolve spotify_url into a track ID up-front so a malformed URL is a
    // 400 before we touch the DB. Distinguish:
    //   - None              → field absent (don't UPDATE this column)
    //   - Some(empty/ws)    → clear column to NULL
    //   - Some(valid url)   → extract 22-char track ID and persist it
    let resolved_spotify_track_id: Option<Option<String>> = match req.spotify_url.as_deref() {
        None => None,
        Some(s) if s.trim().is_empty() => Some(None),
        Some(s) => match parse_spotify_track_id(s) {
            Ok(id) => Some(Some(id)),
            Err(msg) => {
                return (StatusCode::BAD_REQUEST, format!("spotify_url: {msg}")).into_response();
            }
        },
    };
```

Delete the entire block.

(c) The `sets.push("spotify_track_id = ?")` block (currently lines 394-396):

```rust
    if resolved_spotify_track_id.is_some() {
        sets.push("spotify_track_id = ?");
    }
```

Delete.

(d) The bind block (currently lines 412-414):

```rust
    if let Some(opt_id) = resolved_spotify_track_id.as_ref() {
        q = q.bind::<Option<String>>(opt_id.clone());
    }
```

Delete.

- [ ] **Step 6: Drop `parse_spotify_track_id` and its tests from `routes.rs`**

In `crates/sp-server/src/api/routes.rs`, locate the function (currently around lines 868-895 after PR #70):

```rust
/// Extract a Spotify track ID from any of:
/// - canonical URL: `https://open.spotify.com/track/<id>` (with or without `?si=...`, with or without trailing `/`)
// ... [rest of doc comment]
pub(crate) fn parse_spotify_track_id(input: &str) -> Result<String, &'static str> {
    // ... function body ...
}
```

Delete the entire function (doc comment + signature + body).

Also delete the `#[cfg(test)] mod parse_spotify_tests { ... }` block immediately following (currently lines 896-955).

- [ ] **Step 7: Drop `spotify_track_id` from `SongListItem` in `api/lyrics.rs`**

In `crates/sp-server/src/api/lyrics.rs`, locate the `SongListItem` struct (currently lines 74-99 after PR #70). The struct currently has:

```rust
    /// `videos.spotify_track_id` — operator-pasted Spotify track ID (V17),
    /// consumed by `gather.rs` to fetch LINE_SYNCED lyrics. The /live setlist
    /// UI uses this to show a `.has-spotify` visual state on the 🎵 button
    /// and to pre-fill the edit prompt with the saved value.
    pub spotify_track_id: Option<String>,
```

Delete the block (5 lines).

In the SELECT statements in BOTH `list_songs` AND `get_song_detail` (currently `..., suppress_resolume_en, spotify_track_id \` on lines 103 and 165), replace the trailing `, spotify_track_id` so it becomes `, suppress_resolume_en \` (one line, no `spotify_track_id`).

In the `SongListItem { ... }` constructor in BOTH `list_songs` (currently around line 142) AND `get_song_detail` (currently around line 197), delete the line:

```rust
                spotify_track_id: r.try_get("spotify_track_id").ok(),
```

(in `list_songs`) and

```rust
        spotify_track_id: row.try_get("spotify_track_id").ok(),
```

(in `get_song_detail`).

- [ ] **Step 8: Drop the 🎵 button from `live_setlist.rs`**

In `sp-ui/src/components/live_setlist.rs`, find the `let spotify_track_id_initial = ...` extraction (currently around line 109-112) and the entire `<button class={...} ... >"🎵"</button>` block that follows the EN checkbox label (currently around lines 184-220 after PR #70's c1).

Delete:
1. The `spotify_track_id_initial` extraction (entire `let` statement).
2. The button JSX block from the opening `<button` through `>"🎵"</button>`.

Keep the surrounding EN-checkbox `</label>` and the remove-button `<button class="live-setlist-btn live-setlist-btn-remove" ... >` intact.

- [ ] **Step 9: Drop `patch_video_spotify_url` helper from `sp-ui/src/api.rs`**

In `sp-ui/src/api.rs`, locate the helper (currently around lines 280-294 after PR #70):

```rust
/// PATCH `/api/v1/videos/{id}` with the operator-pasted `spotify_url`.
// ... [rest of doc comment]
pub async fn patch_video_spotify_url(video_id: i64, spotify_url: &str) -> Result<(), String> {
    let body = serde_json::json!({ "spotify_url": spotify_url });
    patch_json_empty(&format!("/api/v1/videos/{video_id}"), &body).await
}
```

Delete the entire helper (doc comment + signature + body).

- [ ] **Step 10: Drop the Spotify CSS from `sp-ui/style.css`**

In `sp-ui/style.css`, find the rules added by PR #70:

```css
.live-setlist-btn-spotify {
    /* ... */
}
.live-setlist-btn-spotify.has-spotify {
    /* ... */
}
```

Delete both rule blocks.

- [ ] **Step 11: Verify formatting**

Run: `cargo fmt --all --check`. Expected: exit 0.

- [ ] **Step 12: Verify trunk build (sp-ui WASM)**

Run: `cd sp-ui && trunk build`. Expected: success.

This catches any orphan references in sp-ui that the deletes might have missed (e.g., a leftover call to `patch_video_spotify_url` somewhere).

- [ ] **Step 13: Verify file sizes**

Run: `wc -l crates/sp-server/src/api/routes.rs crates/sp-server/src/api/lyrics.rs sp-ui/src/components/live_setlist.rs sp-ui/src/api.rs sp-ui/style.css`. All should remain well under 1000 lines (each shrinks).

- [ ] **Step 14: Commit**

```bash
git add -A
git commit -m "refactor: remove PR #70 manual Spotify UI + parser + tests for #73

Auto-resolution lands in this branch (Phases A-C), so the manual UI is
obsolete. Per feedback_no_legacy_code.md: deleted, not deprecated.

Removed:
- 🎵 button on live setlist row (sp-ui)
- patch_video_spotify_url helper (sp-ui)
- .live-setlist-btn-spotify CSS rules
- spotify_url field on PATCH /videos/{id} + parse_spotify_track_id parser + 11 unit tests
- routes_tests_spotify.rs sibling test file (4 PATCH tests)
- spotify_track_id field on SongListItem API response
- spotify-url-input.spec.ts Playwright spec"
```

---

### Phase E push (controller-only)

Controller pushes + monitors CI to terminal state. After Phase E green, the PR is ready.

---

## Pre-PR checklist (controller-only)

Before opening a PR from `dev` to `main`:

- [ ] All phases A–E pushed and CI green individually.
- [ ] Inspect `git log --oneline origin/main..dev` — should be 7-9 well-named commits (1 plan + A.1 + B.1 + B.2 + C.1 + D.1 + E.1 minimum, more if any reviewer-fix commits).
- [ ] No `LYRICS_PIPELINE_VERSION` bump (must remain 20).
- [ ] No additional DB migration (only V18).
- [ ] VERSION file unchanged at `0.30.0-dev.1`.
- [ ] Bump VERSION to `0.30.0` and run `./scripts/sync-version.sh` + `cargo metadata --offline` (workspace + sp-ui) to refresh both lockfiles. Commit as `chore: release 0.30.0`. Push and wait for CI.
- [ ] Open PR via `gh pr create --base main --head dev` with a body summarizing #73 + #72 deliverables.
- [ ] After PR open: poll mergeable + mergeStateStatus until CLEAN.

---

## Verification

After PR merge to main and deploy to win-resolume:

1. **Server liveness:** `/api/v1/status` returns `version: 0.30.0`.
2. **No new manual UI:** the live setlist no longer shows a 🎵 button on rows.
3. **Auto-resolution working:** trigger reprocess on a known song (e.g. one of the 115 currently-missing). Watch worker log for `spotify_resolver: resolved + verified` (success path) or `spotify_resolver: no canonical match` (no-match path). Verify the `videos` row gets `spotify_resolved_at` populated.
4. **Spotify lyrics on the wall:** for a successfully-resolved song, the resulting `_lyrics.json` has `lyrics_source = tier1:spotify` (no longer beaten by longer yt_subs / description candidates after the priority fix).
5. **No re-resolution of existing songs:** the 5 manually-entered Spotify rows from PR #70 stay non-NULL `spotify_track_id` AND get their `spotify_resolved_at` backfilled by V18. No Claude calls fire on them.

---

## Plan self-review

**Spec coverage:**

- V18 migration with backfill: A.1 ✓
- VideoLyricsRow + reprocess.rs SELECT plumbing: A.1 ✓
- SpotifyResolver module + outcome enum + parser: B.1 ✓
- Wiremock integration tests across all 5 outcome paths: B.2 ✓
- Worker pre-gather hook: C.1 ✓
- DB helper `set_video_spotify_resolution`: C.1 ✓
- claude_merge priority fix: D.1 ✓
- Removal of all PR #70 manual UI surfaces: E.1 ✓

**Type consistency:**

- `ResolveOutcome` enum used same way in B.1 (definition), B.2 (matched in tests), C.1 (matched in worker hook). ✓
- `set_video_spotify_resolution(pool, video_id, Option<&str>)` signature consistent across A.1 declaration and C.1 call site. ✓
- `spotify_resolved_at: Option<String>` on `VideoLyricsRow` matches the `try_get` pattern used elsewhere in the codebase. ✓

**No placeholders.** Every code block contains the actual content. The "implementer judgment" notes (locating struct fields, finding existing helpers) point at observable code with concrete grep commands.

**Bite-sized:** each numbered step is one observable action. Failing test → impl → fmt → commit chain holds.

---

## Execution handoff

REQUIRED SUB-SKILL: `superpowers:subagent-driven-development`. Per airuleset `ask-before-assuming.md`, the "subagent or inline?" question is pre-answered to subagent. Begin Phase A Task A.1 immediately after the plan is committed.
