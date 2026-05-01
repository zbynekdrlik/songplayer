# Spotify Tier-1 Wiring + Vocals Preservation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the existing `SpotifyLyricsFetcher` into production gather + dashboard input (#67) AND stop deleting `{youtube_id}_vocals16k.wav` after every alignment (#41), so reprocess reuses Demucs output.

**Architecture:** Six small phases across server / UI / E2E. No DB migration (V17 already added `videos.spotify_track_id`). No `LYRICS_PIPELINE_VERSION` bump (per `feedback_pipeline_version_approval.md` — operator reprocesses song-by-song). Every phase is independently shippable; controller pushes + monitors CI between phases.

**Tech Stack:** Rust 2024, axum 0.8, sqlx 0.8, reqwest 0.12, wiremock 0.6 (existing dev dep), Leptos 0.7 (sp-ui), Playwright (e2e).

**Spec:** `docs/superpowers/specs/2026-05-01-spotify-tier1-and-vocals-preservation-design.md` (commit `4a2ef0a`).

---

## Context for every implementer subagent

Pass these rules verbatim in the dispatch prompt — do not paraphrase. Subagents on smaller models obey precise wording better than abstractions.

**Branch + working dir:**

- Branch `dev` on `/home/newlevel/devel/songplayer`. VERSION is `0.29.0-dev.1` already; do NOT bump.
- Spec is committed at `4a2ef0a`. The plan is committed before Phase A starts.

**Airuleset rules:**

- TDD strict: failing test first → confirm fail (Rust: trust by inspection acceptable since `cargo test` is CI-only) → implement → confirm pass on inspection → `cargo fmt --all --check` (the ONLY local cargo command allowed) → commit on green.
- NEVER run `cargo clippy/test/build/check` locally; rely on CI.
- File-size cap 1000 lines per file.
- One commit per "Commit" step in the plan body.
- `mutants::skip` requires a one-line justification inline.
- Do NOT push — controller batches and pushes once per phase.

**Feedback-memory rules:**

- `feedback_no_legacy_code.md` — when replacing a code path, delete the old one entirely; no fallback retention.
- `feedback_pipeline_version_approval.md` — do NOT bump `LYRICS_PIPELINE_VERSION`; user reprocesses song-by-song.
- `feedback_line_timing_only.md` — `AlignedLine.words: Option<Vec<AlignedWord>>`; never synthesize per-word timings. Spotify is line-only, ships `words: None`.
- `feedback_no_autosub.md` — autosub stays banned; do not register it anywhere.
- `feedback_no_even_distribution.md` — Spotify line-level timings only; never synthesize uniform timings under any circumstances.

**Critical wiring details (already on disk, do NOT re-implement):**

- `crates/sp-server/src/lyrics/spotify_proxy.rs::SpotifyLyricsFetcher::fetch(&self, track_id: &str) -> Result<Option<tier1::CandidateText>, SpotifyError>` already exists with `tier1::CandidateText` output and full unit-test coverage from #66. It owns its own `reqwest::Client` (no client argument). Just call it.
- `crates/sp-server/src/lyrics/claude_merge.rs::source_priority` already maps `tier1:spotify` to priority 4 (between `override` at 5 and `lrclib` at 3). Do NOT touch.
- `crates/sp-server/src/lyrics/aligner.rs::preprocess_vocals` already has cache-hit reuse at lines 87-96 (skip Python invocation if `wav_out` exists and is `> 1_000_000` bytes). Do NOT touch.
- `videos.spotify_track_id` column exists from migration V17. Do NOT add a migration.
- `db/models.rs::VideoLyricsRow.spotify_track_id: Option<String>` is already populated (line 86 of `db/models.rs`).
- `db/models.rs::set_video_spotify_track_id(pool, video_id, Option<&str>)` already exists. Do NOT re-implement; reuse it from the PATCH handler.

**Two-stage code review per task:**

- After implementer reports DONE, dispatch the spec compliance reviewer first (must approve), then the code quality reviewer (must approve). Both must approve before marking the task complete in TodoWrite.

---

## Phase A — API surface for spotify_url

### Task A.1: Spotify URL → track-ID parser

**Files:**

- Modify: `crates/sp-server/src/api/routes.rs` — append a new `pub(crate) fn parse_spotify_track_id` plus its tests near the existing `PatchVideoReq` definition (currently at line 331).

**Model:** haiku.

- [ ] **Step 1: Write the failing tests**

Append a `#[cfg(test)] mod parse_spotify_tests` block at the bottom of `crates/sp-server/src/api/routes.rs`:

```rust
#[cfg(test)]
mod parse_spotify_tests {
    use super::parse_spotify_track_id;

    #[test]
    fn extracts_id_from_canonical_url() {
        let id = parse_spotify_track_id("https://open.spotify.com/track/3n3Ppam7vgaVa1iaRUc9Lp").unwrap();
        assert_eq!(id, "3n3Ppam7vgaVa1iaRUc9Lp");
    }

    #[test]
    fn extracts_id_from_url_with_si_query() {
        let id = parse_spotify_track_id("https://open.spotify.com/track/3n3Ppam7vgaVa1iaRUc9Lp?si=abcd").unwrap();
        assert_eq!(id, "3n3Ppam7vgaVa1iaRUc9Lp");
    }

    #[test]
    fn extracts_id_from_url_with_trailing_slash() {
        let id = parse_spotify_track_id("https://open.spotify.com/track/3n3Ppam7vgaVa1iaRUc9Lp/").unwrap();
        assert_eq!(id, "3n3Ppam7vgaVa1iaRUc9Lp");
    }

    #[test]
    fn extracts_id_from_intl_url() {
        // Spotify localizes its URLs; intl-cz/intl-de etc. variants must work.
        let id = parse_spotify_track_id("https://open.spotify.com/intl-cz/track/3n3Ppam7vgaVa1iaRUc9Lp?si=xyz").unwrap();
        assert_eq!(id, "3n3Ppam7vgaVa1iaRUc9Lp");
    }

    #[test]
    fn accepts_bare_track_id() {
        // Operator pastes just the ID without a URL wrapper.
        let id = parse_spotify_track_id("3n3Ppam7vgaVa1iaRUc9Lp").unwrap();
        assert_eq!(id, "3n3Ppam7vgaVa1iaRUc9Lp");
    }

    #[test]
    fn rejects_empty_string() {
        assert!(parse_spotify_track_id("").is_err());
    }

    #[test]
    fn rejects_whitespace_only() {
        assert!(parse_spotify_track_id("   ").is_err());
    }

    #[test]
    fn rejects_url_without_track_path() {
        assert!(parse_spotify_track_id("https://open.spotify.com/album/3n3Ppam7vgaVa1iaRUc9Lp").is_err());
    }

    #[test]
    fn rejects_id_too_short() {
        assert!(parse_spotify_track_id("3n3Ppam7vga").is_err());
    }

    #[test]
    fn rejects_id_too_long() {
        assert!(parse_spotify_track_id("3n3Ppam7vgaVa1iaRUc9LpXXX").is_err());
    }

    #[test]
    fn rejects_id_with_invalid_chars() {
        assert!(parse_spotify_track_id("3n3Ppam7vga!a1iaRUc9Lp").is_err());
    }
}
```

- [ ] **Step 2: Trust by inspection that tests fail**

`parse_spotify_track_id` does not exist yet — the module fails to compile, all tests in this module fail. No need to run.

- [ ] **Step 3: Implement the parser**

Add at the top of `crates/sp-server/src/api/routes.rs` (just after the existing `use` block, before `PatchVideoReq`):

```rust
/// Extract a Spotify track ID from any of:
/// - canonical URL: `https://open.spotify.com/track/<id>` (with or without `?si=...`, with or without trailing `/`)
/// - localized URL: `https://open.spotify.com/intl-cz/track/<id>?si=...`
/// - bare 22-char alphanumeric ID
///
/// Returns `Err` for empty input, missing `/track/` segment, or IDs that
/// don't match Spotify's 22-char base62 shape.
pub(crate) fn parse_spotify_track_id(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("spotify_url is empty");
    }
    let candidate = if let Some(idx) = trimmed.find("/track/") {
        // Take everything after `/track/`, then cut at the first `?` or `/`.
        let after = &trimmed[idx + "/track/".len()..];
        let cut = after
            .find(|c: char| c == '?' || c == '/')
            .unwrap_or(after.len());
        &after[..cut]
    } else {
        trimmed
    };
    if candidate.len() == 22 && candidate.chars().all(|c| c.is_ascii_alphanumeric()) {
        Ok(candidate.to_string())
    } else {
        Err("not a valid Spotify track ID (must be 22 alphanumeric chars)")
    }
}
```

- [ ] **Step 4: Trust by inspection that tests pass**

Inspect: every test case maps cleanly to the implementation branches above. No async, no I/O.

- [ ] **Step 5: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/api/routes.rs
git commit -m "feat(api): add parse_spotify_track_id for #67"
```

---

### Task A.2: PATCH `/api/v1/videos/{id}` accepts `spotify_url`

**Files:**

- Modify: `crates/sp-server/src/api/routes.rs:331-395` — extend `PatchVideoReq` and the handler.

**Model:** haiku.

- [ ] **Step 1: Write the failing test**

Add a new test module (or extend an existing test in the same file). The test exercises the parser + persistence. Append to `crates/sp-server/src/api/routes.rs` inside an existing `#[cfg(test)]` block, OR add a new one if none exists for this handler:

```rust
#[cfg(test)]
mod patch_spotify_tests {
    use super::*;
    use crate::AppState;
    use axum::{extract::State, http::StatusCode, Json};
    use crate::db;

    async fn fresh_state() -> (AppState, i64) {
        let pool = db::create_memory_pool().await.expect("memory pool");
        db::run_migrations(&pool).await.expect("migrations");
        // Insert one playlist + one video so we have a target id.
        sqlx::query("INSERT INTO playlists (name, url, ndi_output_name, scene_name, is_active) VALUES ('p', 'u', 'n', 's', 1)")
            .execute(&pool).await.unwrap();
        let row: (i64,) = sqlx::query_as("INSERT INTO videos (playlist_id, youtube_id, title) VALUES (1, 'aaaaaaaaaaa', 't') RETURNING id")
            .fetch_one(&pool).await.unwrap();
        let state = AppState::for_tests(pool);
        (state, row.0)
    }

    #[tokio::test]
    async fn patches_spotify_url_extracts_track_id() {
        let (state, video_id) = fresh_state().await;
        let req = PatchVideoReq {
            suppress_resolume_en: None,
            lyrics_override_text: None,
            spotify_url: Some("https://open.spotify.com/track/3n3Ppam7vgaVa1iaRUc9Lp?si=ab".into()),
        };
        let resp = patch_video(
            State(state.clone()),
            axum::extract::Path(video_id),
            Json(req),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Verify persisted track ID.
        let stored = db::models::get_video_spotify_track_id(&state.pool, video_id)
            .await
            .unwrap();
        assert_eq!(stored.as_deref(), Some("3n3Ppam7vgaVa1iaRUc9Lp"));
    }

    #[tokio::test]
    async fn empty_spotify_url_clears_track_id() {
        let (state, video_id) = fresh_state().await;
        // Pre-set a value.
        db::models::set_video_spotify_track_id(&state.pool, video_id, Some("3n3Ppam7vgaVa1iaRUc9Lp"))
            .await
            .unwrap();
        let req = PatchVideoReq {
            suppress_resolume_en: None,
            lyrics_override_text: None,
            spotify_url: Some("".into()),
        };
        let resp = patch_video(
            State(state.clone()),
            axum::extract::Path(video_id),
            Json(req),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let stored = db::models::get_video_spotify_track_id(&state.pool, video_id)
            .await
            .unwrap();
        assert!(stored.is_none(), "empty spotify_url must clear track ID to NULL");
    }

    #[tokio::test]
    async fn malformed_spotify_url_returns_400() {
        let (state, video_id) = fresh_state().await;
        let req = PatchVideoReq {
            suppress_resolume_en: None,
            lyrics_override_text: None,
            spotify_url: Some("not a valid url".into()),
        };
        let resp = patch_video(
            State(state),
            axum::extract::Path(video_id),
            Json(req),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn empty_body_still_returns_400() {
        // Existing behavior: empty body is rejected. Adding spotify_url field
        // must not break this.
        let (state, video_id) = fresh_state().await;
        let req = PatchVideoReq {
            suppress_resolume_en: None,
            lyrics_override_text: None,
            spotify_url: None,
        };
        let resp = patch_video(
            State(state),
            axum::extract::Path(video_id),
            Json(req),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
```

If `AppState::for_tests` does not exist, write the test to construct an `AppState` whatever way the existing PATCH tests do (look in the rest of `routes.rs` or sibling test files for prior art) — or wrap in a small helper if no prior art exists. This is a judgment call; do not invent unfamiliar patterns.

- [ ] **Step 2: Trust by inspection that tests fail**

`spotify_url` is not a field on `PatchVideoReq` yet — compile fails. Tests cannot run.

- [ ] **Step 3: Extend `PatchVideoReq`**

Modify `crates/sp-server/src/api/routes.rs:331-341`:

```rust
#[derive(Debug, Deserialize)]
pub struct PatchVideoReq {
    #[serde(default)]
    pub suppress_resolume_en: Option<bool>,
    /// Operator-provided lyrics text. When Some(non-empty), the lyrics
    /// worker uses it as the top-priority reference for Gemini alignment,
    /// bypassing yt_subs / description / LRCLIB gather paths. Pass
    /// `Some("")` to clear the override.
    #[serde(default)]
    pub lyrics_override_text: Option<String>,
    /// Spotify track URL or bare 22-char ID. The handler extracts the
    /// track ID via `parse_spotify_track_id`. Pass `Some("")` to clear.
    /// Source for `videos.spotify_track_id` (V17 column), consumed by
    /// `gather.rs` to fetch line-synced lyrics from the proxy.
    #[serde(default)]
    pub spotify_url: Option<String>,
}
```

- [ ] **Step 4: Extend the handler**

Modify `patch_video` (currently lines 346-395). Apply these changes:

1. Extend the empty-body guard at line 352 to include `spotify_url`:

```rust
    if req.suppress_resolume_en.is_none()
        && req.lyrics_override_text.is_none()
        && req.spotify_url.is_none()
    {
        return (
            StatusCode::BAD_REQUEST,
            "request body must include at least one patchable field",
        )
            .into_response();
    }
```

2. Extract the spotify_track_id BEFORE building the UPDATE — empty/whitespace clears, malformed returns 400:

```rust
    // Resolve spotify_url into a track ID up-front so a malformed URL is a
    // 400 before we touch the DB.
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

3. Add the `sets.push("spotify_track_id = ?")` branch and the bind:

```rust
    if resolved_spotify_track_id.is_some() {
        sets.push("spotify_track_id = ?");
    }
```

(after the existing `lyrics_override_text` push)

```rust
    if let Some(opt_id) = resolved_spotify_track_id.as_ref() {
        q = q.bind::<Option<String>>(opt_id.clone());
    }
```

(after the existing `lyrics_override_text` bind, BEFORE `q = q.bind(video_id)`)

- [ ] **Step 5: Trust by inspection that tests pass**

Walk each test case through the modified handler:
- Canonical URL → `parse_spotify_track_id` returns Ok, branch pushes `spotify_track_id = ?`, bind issues UPDATE, 204.
- Empty string → `Some("")` matches the whitespace branch, `Some(None)`, persists NULL, 204.
- Malformed → `Err(...)`, returns 400 with `spotify_url: ...` message.
- All-None body → empty-body guard fires, 400.

- [ ] **Step 6: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 7: Commit**

```bash
git add crates/sp-server/src/api/routes.rs
git commit -m "feat(api): PATCH /videos/{id} accepts spotify_url for #67"
```

---

### Phase A push (controller-only)

Controller (NOT a subagent dispatch) runs `git push origin dev` and monitors CI to terminal state per `airuleset/modules/core/ci-monitoring.md`. If CI fails, fix on next subagent dispatch and re-push. Do NOT proceed to Phase B until Phase A's CI is green.

---

## Phase B — Wire Spotify into gather

### Task B.1: `From<tier1::CandidateText> for provider::CandidateText`

**Files:**

- Modify: `crates/sp-server/src/lyrics/provider.rs` — add a reverse `From` impl alongside the existing `From<provider::CandidateText> for tier1::CandidateText` (which lives in `tier1.rs:33`).

**Why:** `gather_sources_impl` produces `provider::CandidateText` items. `SpotifyLyricsFetcher::fetch` returns `tier1::CandidateText`. The two structs have identical fields. The existing `tier1.rs:33` impl is one-way (provider → tier1) and is comment-marked "Temporary bridge — Phase G deletes provider.rs and this impl with it." We add the reverse one-way impl as a sibling of the existing one. When provider.rs eventually goes away, both impls go with it.

**Model:** haiku.

- [ ] **Step 1: Write the failing test**

Append to the bottom of `crates/sp-server/src/lyrics/provider.rs` inside (or alongside) an existing `#[cfg(test)] mod tests` block:

```rust
#[cfg(test)]
mod from_tier1_tests {
    use super::CandidateText as ProviderCandidate;
    use crate::lyrics::tier1::CandidateText as Tier1Candidate;

    #[test]
    fn round_trips_all_fields() {
        let src = Tier1Candidate {
            source: "tier1:spotify".into(),
            lines: vec!["one".into(), "two".into()],
            line_timings: Some(vec![(0, 1000), (1000, 2000)]),
            has_timing: true,
        };
        let p: ProviderCandidate = src.clone().into();
        assert_eq!(p.source, "tier1:spotify");
        assert_eq!(p.lines, vec!["one", "two"]);
        assert_eq!(p.line_timings, Some(vec![(0, 1000), (1000, 2000)]));
        assert!(p.has_timing);
    }
}
```

- [ ] **Step 2: Trust by inspection that the test fails**

`From<tier1::CandidateText> for provider::CandidateText` does not exist; the test fails to compile.

- [ ] **Step 3: Add the `From` impl**

In `crates/sp-server/src/lyrics/provider.rs`, alongside (above or below) the existing `pub struct CandidateText` definition (around line 34):

```rust
// Temporary bridge — Phase G deletes provider.rs and this impl with it.
// Mirrors the forward impl in `tier1.rs::33`.
impl From<crate::lyrics::tier1::CandidateText> for CandidateText {
    fn from(c: crate::lyrics::tier1::CandidateText) -> Self {
        Self {
            source: c.source,
            lines: c.lines,
            has_timing: c.has_timing,
            line_timings: c.line_timings,
        }
    }
}
```

- [ ] **Step 4: Trust by inspection that the test passes**

Field-for-field copy; the test asserts each.

- [ ] **Step 5: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/lyrics/provider.rs
git commit -m "feat(lyrics): add From<tier1::CandidateText> for provider::CandidateText"
```

---

### Task B.2: Call `SpotifyLyricsFetcher` from `gather_sources_impl`

**Files:**

- Modify: `crates/sp-server/src/lyrics/gather.rs` — add a Spotify fetch block after the Genius block (around line 95), and a `candidate_texts.push` between the existing `override` push (line 102-121) and `yt_subs` push (line 122-129).

**Model:** sonnet — touches existing async I/O ordering.

- [ ] **Step 1: Add the Spotify fetch block**

Modify `crates/sp-server/src/lyrics/gather.rs`. At the top, extend the `use` block:

```rust
use crate::lyrics::{genius, lrclib, spotify_proxy::SpotifyLyricsFetcher, youtube_subs};
```

After the existing Genius block (line 80-94, ends with `} else { None };`), add a new block:

```rust
    // 3. Spotify (operator pasted track URL via dashboard). Authoritative
    //    LINE_SYNCED lyrics for songs the other Tier-1 sources miss
    //    (chant, dense vocal, niche worship). Best-effort; transport /
    //    proxy errors log and skip.
    let spotify_track = if let Some(track_id) = row.spotify_track_id.as_deref() {
        let fetcher = SpotifyLyricsFetcher::new();
        match fetcher.fetch(track_id).await {
            Ok(Some(t)) => {
                info!(%youtube_id, line_count = t.lines.len(), "gather: Spotify hit");
                Some(t)
            }
            Ok(None) => {
                debug!("gather: no Spotify synced lyrics for {youtube_id}");
                None
            }
            Err(e) => {
                warn!("gather: Spotify error for {youtube_id}: {e}");
                None
            }
        }
    } else {
        None
    };
```

In the `candidate_texts` assembly section, insert a Spotify push BETWEEN the existing `override` block (lines 102-121) and the `yt_subs` push (lines 122-129):

```rust
    // Spotify priority sits between override and other Tier-1 sources;
    // claude_merge::source_priority maps "tier1:spotify" to 4.
    if let Some(t) = spotify_track {
        candidate_texts.push(t.into());
    }
```

(`t` here is `tier1::CandidateText`; `.into()` uses the new impl from Task B.1 to convert to `provider::CandidateText`.)

- [ ] **Step 2: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 3: Commit**

```bash
git add crates/sp-server/src/lyrics/gather.rs
git commit -m "feat(lyrics): call SpotifyLyricsFetcher in gather_sources_impl for #67"
```

---

### Task B.3: Wiremock unit tests for gather + Spotify

**Files:**

- Create or modify: `crates/sp-server/src/lyrics/gather.rs` — extend the existing test module (or add one) at the bottom of the file with wiremock-backed tests.

**Why:** the Spotify fetcher itself is unit-tested for parser branches in `spotify_proxy.rs`. What is NOT covered: gather correctly emits a `tier1:spotify` `CandidateText` when `row.spotify_track_id.is_some()`, and gracefully no-ops when it's `None` / proxy returns 404 / proxy returns malformed.

**Model:** sonnet.

**Approach:** the `SpotifyLyricsFetcher::fetch` hits a hardcoded URL (`PROXY_BASE` in `spotify_proxy.rs`). Two ways to mock it for the gather tests:

(a) **Override `PROXY_BASE` via env var** during tests. Cleanest. Requires `spotify_proxy.rs` to read the base URL from env; one-line change.

(b) **Stub `SpotifyLyricsFetcher` behind a trait + inject** through gather. Bigger refactor.

Pick (a) — minimal change, no new abstraction. Subtask B.3a does the env wiring; B.3b writes the tests.

- [ ] **Step 1 (B.3a): Make `PROXY_BASE` overridable**

Modify `crates/sp-server/src/lyrics/spotify_proxy.rs`. Replace:

```rust
const PROXY_BASE: &str = "https://spotify-lyrics-api-khaki.vercel.app";
```

with:

```rust
/// Spotify lyrics proxy base URL. Overridable via env var `SPOTIFY_LYRICS_PROXY_BASE`
/// for tests. Defaults to the public proxy.
fn proxy_base() -> String {
    std::env::var("SPOTIFY_LYRICS_PROXY_BASE")
        .unwrap_or_else(|_| "https://spotify-lyrics-api-khaki.vercel.app".to_string())
}
```

Update the call site in `fetch`:

```rust
        let url = format!("{}/?trackid={}", proxy_base(), track_id);
```

- [ ] **Step 2: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 3: Commit B.3a**

```bash
git add crates/sp-server/src/lyrics/spotify_proxy.rs
git commit -m "refactor(lyrics): make spotify_proxy base URL env-overridable for tests"
```

- [ ] **Step 4 (B.3b): Add wiremock tests for gather**

Look at how existing gather tests are structured. Reference: search for `gather_sources` or `gather_sources_impl` invocations in test code (`grep -rn 'gather_sources_impl' crates/sp-server/src/lyrics/`). Likely they live in `worker_tests.rs` (which re-exports from `worker.rs::gather_sources_impl`).

Add the new tests there (or in a new sibling test file `crates/sp-server/src/lyrics/gather_tests.rs` if a clean separation is preferred — match existing convention, do not invent).

Required tests (each uses `wiremock` to stand up a local HTTP server + sets `SPOTIFY_LYRICS_PROXY_BASE` to its URL):

```rust
#[tokio::test]
async fn gather_emits_tier1_spotify_candidate_when_track_id_set() {
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/"))
        .and(wiremock::matchers::query_param("trackid", "3n3Ppam7vgaVa1iaRUc9Lp"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error": false,
                "syncType": "LINE_SYNCED",
                "lines": [
                    {"startTimeMs": "1000", "words": "Amazing grace"},
                    {"startTimeMs": "3000", "words": "How sweet the sound"}
                ]
            })),
        )
        .mount(&mock)
        .await;

    // SAFETY: tests in this file run sequentially because they share the
    // process-wide env var; mark the test fn with #[serial_test::serial]
    // if the crate already uses serial_test, otherwise use std::env directly
    // and accept the mutex (existing tests in this codebase set env vars
    // directly — match existing convention).
    std::env::set_var("SPOTIFY_LYRICS_PROXY_BASE", mock.uri());

    let row = make_test_row(); // helper that returns a VideoLyricsRow with
                                // spotify_track_id = Some("3n3Ppam7vgaVa1iaRUc9Lp")
                                // and song/artist/youtube_id set so other Tier-1
                                // fetchers also have a chance to fire.

    let pool = test_pool().await;
    let client = reqwest::Client::new();
    // No yt-dlp / no AI client: pass dummy paths / None so the Spotify
    // branch is the only one that fires here. (LRCLIB / Genius will hit
    // their real network endpoints — wrap them with their own wiremock
    // OR pass empty song/artist to skip those branches.)
    let row = VideoLyricsRow {
        song: "".into(),    // empty song → LRCLIB + Genius branches skipped
        artist: "".into(),
        spotify_track_id: Some("3n3Ppam7vgaVa1iaRUc9Lp".into()),
        ..make_test_row()
    };
    let ctx = gather_sources_impl(
        None,
        std::path::Path::new("/nonexistent/yt-dlp"),
        std::path::Path::new("/tmp"),
        &client,
        &row,
        "",
    )
    .await
    .expect("gather succeeds");

    let spotify = ctx
        .candidate_texts
        .iter()
        .find(|c| c.source == "tier1:spotify")
        .expect("Spotify candidate present");
    assert!(spotify.has_timing);
    assert_eq!(spotify.lines.len(), 2);
    assert_eq!(spotify.lines[0], "Amazing grace");
    assert!(spotify.line_timings.is_some());

    std::env::remove_var("SPOTIFY_LYRICS_PROXY_BASE");
}

#[tokio::test]
async fn gather_omits_spotify_when_track_id_is_null() {
    // No spotify_track_id => no Spotify candidate.
    let row = VideoLyricsRow {
        song: "".into(),
        artist: "".into(),
        spotify_track_id: None,
        ..make_test_row()
    };
    let ctx = gather_sources_impl(
        None,
        std::path::Path::new("/nonexistent/yt-dlp"),
        std::path::Path::new("/tmp"),
        &reqwest::Client::new(),
        &row,
        "",
    )
    .await;
    // gather_sources_impl bails if NO sources are available — for this
    // negative test, set a non-empty operator override so it has something
    // to return.
    let row_with_override = VideoLyricsRow {
        lyrics_override_text: Some("operator line".into()),
        ..row
    };
    let ctx = gather_sources_impl(
        None,
        std::path::Path::new("/nonexistent/yt-dlp"),
        std::path::Path::new("/tmp"),
        &reqwest::Client::new(),
        &row_with_override,
        "",
    )
    .await
    .expect("gather succeeds");
    assert!(ctx.candidate_texts.iter().all(|c| c.source != "tier1:spotify"));
}

#[tokio::test]
async fn gather_skips_spotify_on_404() {
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(404))
        .mount(&mock)
        .await;
    std::env::set_var("SPOTIFY_LYRICS_PROXY_BASE", mock.uri());

    let row = VideoLyricsRow {
        song: "".into(),
        artist: "".into(),
        lyrics_override_text: Some("operator line".into()),
        spotify_track_id: Some("3n3Ppam7vgaVa1iaRUc9Lp".into()),
        ..make_test_row()
    };
    let ctx = gather_sources_impl(
        None,
        std::path::Path::new("/nonexistent/yt-dlp"),
        std::path::Path::new("/tmp"),
        &reqwest::Client::new(),
        &row,
        "",
    )
    .await
    .expect("gather succeeds even when Spotify returns 404");
    assert!(ctx.candidate_texts.iter().all(|c| c.source != "tier1:spotify"));

    std::env::remove_var("SPOTIFY_LYRICS_PROXY_BASE");
}

#[tokio::test]
async fn gather_skips_spotify_on_proxy_error_field() {
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"error": true, "message": "track not found"})),
        )
        .mount(&mock)
        .await;
    std::env::set_var("SPOTIFY_LYRICS_PROXY_BASE", mock.uri());

    let row = VideoLyricsRow {
        song: "".into(),
        artist: "".into(),
        lyrics_override_text: Some("operator line".into()),
        spotify_track_id: Some("3n3Ppam7vgaVa1iaRUc9Lp".into()),
        ..make_test_row()
    };
    let ctx = gather_sources_impl(
        None,
        std::path::Path::new("/nonexistent/yt-dlp"),
        std::path::Path::new("/tmp"),
        &reqwest::Client::new(),
        &row,
        "",
    )
    .await
    .expect("gather succeeds even when proxy returns error:true");
    assert!(ctx.candidate_texts.iter().all(|c| c.source != "tier1:spotify"));

    std::env::remove_var("SPOTIFY_LYRICS_PROXY_BASE");
}
```

Implementer judgment: `make_test_row()` and `test_pool()` may already exist as helpers in `worker_tests.rs` or a sibling test module. If they do, reuse them. If not, write the smallest helper that constructs a valid `VideoLyricsRow` with reasonable defaults.

Implementer judgment: the wiremock dev-dep may not be present. Verify with `grep wiremock Cargo.toml crates/sp-server/Cargo.toml`. If missing, add it under `[dev-dependencies]` in `crates/sp-server/Cargo.toml`:

```toml
wiremock = { workspace = true }
```

(or matching the workspace pattern; confirm by checking the root `Cargo.toml`'s workspace deps for `wiremock`).

- [ ] **Step 5: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 6: Commit B.3b**

```bash
git add crates/sp-server/src/lyrics/ crates/sp-server/Cargo.toml
git commit -m "test(lyrics): wiremock-cover Spotify branch in gather_sources_impl"
```

---

### Phase B push (controller-only)

Controller pushes + monitors CI. If green, proceed to Phase C.

---

## Phase C — sp-ui Spotify URL input

### Task C.1: Add Spotify URL field on the lyrics override card

**Files:**

- Modify: `sp-ui/src/components/<file with the lyrics_override_text input>.rs` — exact file resolved during implementation. Likely `sp-ui/src/components/lyrics_queue_card.rs` or `sp-ui/src/pages/live.rs`. Implementer should `grep -rn lyrics_override_text sp-ui/src/` to locate the existing override input.

**Model:** sonnet — Leptos component work + RwSignal/api wiring.

- [ ] **Step 1: Locate the existing `lyrics_override_text` input**

Run: `grep -rnE 'lyrics_override_text|spotify_url' sp-ui/src/components/ sp-ui/src/pages/ | head -20`

Expected: at least one match showing where the override input lives. The new Spotify URL input goes IMMEDIATELY adjacent to it (visually next to it on the same card).

- [ ] **Step 2: Add the API helper**

In `sp-ui/src/api.rs`, add (or extend) a PATCH helper for `spotify_url`. If a `patch_video` helper already exists, extend it to accept `spotify_url`. Otherwise add:

```rust
pub async fn patch_video_spotify_url(video_id: i64, spotify_url: &str) -> Result<(), String> {
    let body = serde_json::json!({ "spotify_url": spotify_url });
    let resp = gloo_net::http::Request::patch(&format!("/api/v1/videos/{video_id}"))
        .header("content-type", "application/json")
        .body(body.to_string())
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}: {}", resp.status(), resp.status_text()));
    }
    Ok(())
}
```

- [ ] **Step 3: Add the input component**

Adjacent to the existing override input, add:

```rust
let spotify_url = RwSignal::new(String::new());
// On mount, populate from the video's current spotify_track_id (if any).
// If the source signal exposes spotify_track_id, prepopulate; otherwise leave blank.

view! {
    <div class="lyrics-override-row">
        <label>"Spotify URL"</label>
        <input
            type="text"
            class="spotify-url-input"
            data-testid="spotify-url-input"
            placeholder="https://open.spotify.com/track/..."
            prop:value=move || spotify_url.get()
            on:input=move |ev| spotify_url.set(event_target_value(&ev))
            on:blur=move |_| {
                let v = spotify_url.get();
                let video_id = video_id; // captured from parent scope
                spawn_local(async move {
                    if let Err(e) = api::patch_video_spotify_url(video_id, &v).await {
                        web_sys::console::error_1(&format!("PATCH spotify_url failed: {e}").into());
                    }
                });
            }
        />
    </div>
}
```

Implementer judgment: the existing override input may use a different signal pattern (e.g. shared with a parent store). Match the existing pattern. The above is a reference structure — adapt for the actual file.

- [ ] **Step 4: Build the WASM bundle**

Run: `cd sp-ui && trunk build` (this is the ONE WASM build step the implementer is allowed to run locally — it is not a `cargo` command).

Expected: build succeeds, dist/ updated. If it fails, fix the component code (likely a typo or signal-scoping issue).

- [ ] **Step 5: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 6: Commit**

```bash
git add sp-ui/src/
git commit -m "feat(ui): Spotify URL input on lyrics override card for #67"
```

---

### Phase C push (controller-only)

Controller pushes + monitors CI.

---

## Phase D — Stop deleting `_vocals16k.wav`

### Task D.1: Remove the two `remove_file` calls in worker.rs

**Files:**

- Modify: `crates/sp-server/src/lyrics/worker.rs:481-487` (orchestrator-error path) and `crates/sp-server/src/lyrics/worker.rs:493-495` (success-path cleanup).

**Model:** haiku.

- [ ] **Step 1: Remove the orchestrator-error-path delete**

In `crates/sp-server/src/lyrics/worker.rs`, find the `Err(e) =>` arm of the `match orch.process(...).await` (currently around lines 481-487):

```rust
            Err(e) => {
                warn!("worker: orchestrator failed for {youtube_id}: {e}");
                let wav_path = self.cache_dir.join(format!("{youtube_id}_vocals16k.wav"));
                let _ = tokio::fs::remove_file(&wav_path).await;
                self.clear_processing().await;
                return Err(anyhow::anyhow!("orchestrator: {e}"));
            }
```

Replace with:

```rust
            Err(e) => {
                warn!("worker: orchestrator failed for {youtube_id}: {e}");
                // Vocals WAV intentionally preserved on disk — aligner's
                // cache-hit path (aligner.rs:87-96) reuses it on next run,
                // saving Demucs minutes per song. Self-heal removes orphans
                // when the parent video is removed (cache.rs).
                self.clear_processing().await;
                return Err(anyhow::anyhow!("orchestrator: {e}"));
            }
```

- [ ] **Step 2: Remove the success-path delete**

In the same file, find the success-path cleanup (currently around lines 493-495):

```rust
        // Cleanup scratch files.
        let wav_path = self.cache_dir.join(format!("{youtube_id}_vocals16k.wav"));
        let _ = tokio::fs::remove_file(&wav_path).await;
```

Replace with:

```rust
        // Vocals WAV intentionally preserved on disk — aligner's cache-hit
        // path (aligner.rs:87-96) reuses it on next run, saving Demucs
        // minutes per song. Self-heal removes orphans (cache.rs) when the
        // parent video is removed.
```

(The two lines are deleted; the comment block replaces them.)

- [ ] **Step 3: Trust by inspection**

`aligner::preprocess_vocals` (lines 87-96 of aligner.rs) is unchanged: when the `wav_out` path exists and is `> 1_000_000` bytes, it returns early. Removing the deletes means subsequent reprocesses of the same song hit that cache path. No behavioral change is needed in aligner.

- [ ] **Step 4: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/lyrics/worker.rs
git commit -m "fix(lyrics): preserve _vocals16k.wav across reprocess for #41"
```

---

### Phase D push (controller-only)

Controller pushes + monitors CI.

---

## Phase E — Self-heal `_vocals16k.wav` orphans

### Task E.1: Extend `scan_cache` + `cleanup_removed` to include vocals files

**Files:**

- Modify: `crates/sp-server/src/downloader/cache.rs` — add a `VOCALS_RE`, a `vocals_files` field on `ScanResult`, populate it during `scan_cache`, and delete inactive vocals files in `cleanup_removed`.

**Model:** haiku.

- [ ] **Step 1: Add the failing test**

Append to the existing `#[cfg(test)] mod tests` block in `crates/sp-server/src/downloader/cache.rs`:

```rust
    #[test]
    fn scan_cache_picks_up_vocals_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("dQw4w9WgXcQ_vocals16k.wav"),
            "fake vocals",
        )
        .unwrap();
        fs::write(dir.path().join("aBcDeFgHiJk_vocals16k.wav"), "fake").unwrap();
        let result = scan_cache(dir.path());
        assert_eq!(result.vocals_files.len(), 2);
        let ids: HashSet<&str> = result
            .vocals_files
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert!(ids.contains("dQw4w9WgXcQ"));
        assert!(ids.contains("aBcDeFgHiJk"));
    }

    #[test]
    fn cleanup_removed_deletes_vocals_for_inactive_videos() {
        let dir = tempfile::tempdir().unwrap();
        // active song
        fs::write(
            dir.path().join("Song_Artist_aaaaaaaaaaa_normalized_video.mp4"),
            "v",
        )
        .unwrap();
        fs::write(
            dir.path().join("Song_Artist_aaaaaaaaaaa_normalized_audio.flac"),
            "a",
        )
        .unwrap();
        fs::write(dir.path().join("aaaaaaaaaaa_vocals16k.wav"), "active").unwrap();
        // removed song's vocals
        fs::write(dir.path().join("xxxxxxxxxxx_vocals16k.wav"), "stale").unwrap();

        let mut active: HashSet<String> = HashSet::new();
        active.insert("aaaaaaaaaaa".into());
        cleanup_removed(dir.path(), &active, None);

        assert!(dir.path().join("aaaaaaaaaaa_vocals16k.wav").exists(), "active vocals must be kept");
        assert!(!dir.path().join("xxxxxxxxxxx_vocals16k.wav").exists(), "stale vocals must be deleted");
    }

    #[test]
    fn cleanup_removed_preserves_vocals_for_currently_playing() {
        let dir = tempfile::tempdir().unwrap();
        // Vocals for a song that is NOT in active_ids but IS playing.
        fs::write(dir.path().join("playingidxxx_vocals16k.wav"), "playing").unwrap();
        let active: HashSet<String> = HashSet::new();
        cleanup_removed(dir.path(), &active, Some("playingidxxx"));
        assert!(
            dir.path().join("playingidxxx_vocals16k.wav").exists(),
            "currently-playing vocals must not be deleted"
        );
    }
```

- [ ] **Step 2: Trust by inspection that tests fail**

`vocals_files` is not a field on `ScanResult` yet; `cleanup_removed` does not touch vocals. Compile fails on the new test (and the cleanup tests would not match expectations).

- [ ] **Step 3: Add `VOCALS_RE` regex**

Near the existing regex statics (lines 60-71 of cache.rs):

```rust
static VOCALS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-zA-Z0-9_-]{11})_vocals16k\.wav$").unwrap());
```

- [ ] **Step 4: Add `vocals_files` field to `ScanResult`**

Modify the existing `pub struct ScanResult` (currently around line 50-58):

```rust
#[derive(Debug, Default)]
pub struct ScanResult {
    pub songs: Vec<CachedSong>,
    pub legacy: Vec<LegacyFile>,
    pub orphans: Vec<Orphan>,
    pub lyrics_files: Vec<(String, PathBuf)>,
    /// `(youtube_id, path)` for every preprocess-vocals output found in
    /// the cache directory. Persisted across alignment runs (see #41) so
    /// reprocess reuses Demucs output via aligner.rs cache-hit logic.
    pub vocals_files: Vec<(String, PathBuf)>,
}
```

- [ ] **Step 5: Populate `vocals_files` in `scan_cache`**

In `scan_cache` (around line 90-177), declare a local `vocals_files`:

```rust
    let mut vocals_files: Vec<(String, PathBuf)> = Vec::new();
```

(alongside the existing `let mut lyrics_files: ...` declaration, around line 103).

In the per-entry match block (after the existing `LYRICS_RE` branch at lines 138-141):

```rust
        if let Some(caps) = VOCALS_RE.captures(filename) {
            vocals_files.push((caps[1].to_string(), path));
            continue;
        }
```

In the `ScanResult { ... }` constructor at the bottom of `scan_cache` (around line 171-176):

```rust
    ScanResult {
        songs,
        legacy,
        orphans,
        lyrics_files,
        vocals_files,
    }
```

- [ ] **Step 6: Delete inactive vocals in `cleanup_removed`**

Modify `cleanup_removed` (currently lines 181-212). Append, just before the closing `}`:

```rust
    // Delete vocals for video_ids no longer in active_ids (and not currently
    // playing). Preserves the cache-hit path on next reprocess for active
    // songs while preventing orphan accumulation per #41.
    for (vid, path) in result.vocals_files {
        if active_ids.contains(&vid) {
            continue;
        }
        if playing_id == Some(vid.as_str()) {
            continue;
        }
        tracing::info!(
            "removing orphan vocals for removed video {}: {}",
            vid,
            path.display()
        );
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!("failed to remove vocals {}: {e}", path.display());
        }
    }
```

- [ ] **Step 7: Verify formatting**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 8: Trust by inspection that tests pass**

Walk each new test:
- `scan_cache_picks_up_vocals_files`: writes two WAV files matching `VOCALS_RE`; `scan_cache` populates `vocals_files`; assertions match.
- `cleanup_removed_deletes_vocals_for_inactive_videos`: writes one active video pair + active vocals + stale vocals; `cleanup_removed` keeps active, deletes stale.
- `cleanup_removed_preserves_vocals_for_currently_playing`: vocals for an ID not in `active_ids` but matching `playing_id` is preserved.

- [ ] **Step 9: Commit**

```bash
git add crates/sp-server/src/downloader/cache.rs
git commit -m "feat(cache): self-heal _vocals16k.wav orphans for #41"
```

---

### Phase E push (controller-only)

Controller pushes + monitors CI.

---

## Phase F — E2E Playwright spec for Spotify URL input

### Task F.1: Playwright spec — paste Spotify URL → save → PATCH issued

**Files:**

- Create: `e2e/spotify-url-input.spec.ts`.
- Modify: `e2e/mock-api.mjs` — add a stubbed PATCH endpoint that records the body and returns 204 (if not already supported).

**Model:** sonnet.

- [ ] **Step 1: Verify mock-api.mjs PATCH handler**

Run: `grep -nE "PATCH|patch_video|/api/v1/videos/" e2e/mock-api.mjs | head -10`

If no PATCH handler exists, add one:

```javascript
// PATCH /api/v1/videos/:id — record body, return 204
const patchedVideos = []; // exposed for tests via /test/patches if needed
app.patch('/api/v1/videos/:id', express.json(), (req, res) => {
    patchedVideos.push({ id: parseInt(req.params.id), body: req.body });
    res.status(204).send();
});

app.get('/test/patches', (_req, res) => {
    res.json(patchedVideos);
});
```

If a PATCH handler already exists, extend its recording so the new test can read back `spotify_url`.

- [ ] **Step 2: Add the Playwright spec**

Create `e2e/spotify-url-input.spec.ts`:

```typescript
import { test, expect } from '@playwright/test';

test.describe('Spotify URL input (#67)', () => {
    let consoleMessages: string[] = [];

    test.beforeEach(async ({ page }) => {
        consoleMessages = [];
        page.on('console', (msg) => {
            if (msg.type() === 'error' || msg.type() === 'warning') {
                consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
            }
        });
    });

    test.afterEach(async () => {
        expect(consoleMessages, 'browser console must be clean').toEqual([]);
    });

    test('paste Spotify URL on a video card → save on blur → PATCH issued with extracted track ID', async ({ page, request }) => {
        await page.goto('/');
        // Navigate to wherever the lyrics override card lives — exact selector
        // to be confirmed at implementation time. Use [data-testid="spotify-url-input"].
        const input = page.locator('[data-testid="spotify-url-input"]').first();
        await input.waitFor({ state: 'visible', timeout: 5000 });

        await input.fill('https://open.spotify.com/track/3n3Ppam7vgaVa1iaRUc9Lp?si=abcd');
        // Blur the input — triggers save.
        await page.locator('body').click({ position: { x: 0, y: 0 } });

        // Wait for the PATCH to land at the mock API.
        await page.waitForTimeout(300);
        const patches = await request.get('http://127.0.0.1:8920/test/patches').then((r) => r.json());
        expect(patches.length).toBeGreaterThan(0);
        const last = patches[patches.length - 1];
        // The PATCH body should carry spotify_url (server-side extracts the
        // track ID; mock API does not extract — just records).
        expect(last.body).toHaveProperty('spotify_url');
        expect(last.body.spotify_url).toBe('https://open.spotify.com/track/3n3Ppam7vgaVa1iaRUc9Lp?si=abcd');
    });
});
```

- [ ] **Step 3: Run the spec locally**

Run: `cd e2e && npm run test -- spotify-url-input.spec.ts`
Expected: PASS, zero console errors.

If FAIL: fix selector / DOM structure issues; do not loosen assertions.

- [ ] **Step 4: Commit**

```bash
git add e2e/
git commit -m "test(e2e): Playwright spec for Spotify URL input for #67"
```

---

### Phase F push (controller-only)

Controller pushes + monitors CI to terminal state. Once green, the PR is ready.

---

## Pre-PR checklist (controller-only)

Before opening a PR from `dev` to `main`:

- [ ] All phases A–F pushed and CI green individually.
- [ ] Inspect `git log --oneline origin/main..dev` — should be 8-10 well-named commits.
- [ ] No `LYRICS_PIPELINE_VERSION` bump (must remain at current value 20).
- [ ] No DB migration added (V17 is the latest).
- [ ] VERSION file unchanged at `0.29.0-dev.1`.
- [ ] Run `gh pr create --base main --head dev` with a body summarizing #67 + #41 deliverables.

---

## Verification

After PR merge to main and deploy to win-resolume:

1. **Server liveness:** `/api/v1/status` returns `version: 0.29.0-dev.1` (or whatever the merged version is).
2. **Spotify wiring:** PATCH a known-good Spotify track URL onto a real video, trigger reprocess, observe `lyrics_source = "tier1:spotify"` in the resulting `_lyrics.json` AND in the dashboard's lyrics queue card source label.
3. **Vocals preservation:** trigger reprocess on a song whose `_vocals16k.wav` already exists. Watch the worker log — it should log `preprocess-vocals: cache hit, reusing existing WAV` and skip the Python subprocess. Reprocess time drops from ~3 min to ~40 s.
4. **Self-heal:** remove a video from a playlist, restart SongPlayer (or wait for the next startup self-heal), confirm its `_vocals16k.wav` is deleted.
5. **No breakage:** existing songs continue to align via Tier-1 (yt_subs / LRCLIB / Genius / description) when no `spotify_track_id` is set.

---

## Plan self-review

**Spec coverage:**
- API extension (spotify_url): A.1 (parser) + A.2 (handler) ✓
- gather wiring (tier1:spotify candidate): B.1 (From impl) + B.2 (call) + B.3 (tests) ✓
- UI input: C.1 ✓
- Stop deleting `_vocals16k.wav`: D.1 ✓
- Self-heal orphans: E.1 ✓
- E2E: F.1 ✓
- Tests across: each task includes a failing-test step ✓

**Type consistency:** `tier1::CandidateText` vs `provider::CandidateText` distinction is called out at top, B.1 adds the conversion, B.2 uses `.into()`. Same struct fields each time.

**No placeholders:** all code blocks are concrete. The few "implementer judgment" notes (helper reuse, file location) are bounded and observable from `grep`.

**Bite-sized tasks:** each numbered step is a single observable action. Failing test → impl → fmt → commit.

---

## Execution handoff

REQUIRED SUB-SKILL: `superpowers:subagent-driven-development`. Per airuleset `ask-before-assuming.md`, the "subagent or inline?" question is pre-answered to subagent — no prompt to the user. Begin Phase A Task A.1 immediately after the plan is committed.
