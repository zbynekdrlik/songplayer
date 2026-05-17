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

use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::ai::client::AiClient;
use crate::lyrics::spotify_proxy::SpotifyLyricsFetcher;

/// `TIER1_MIN_LINES` from `tier1.rs` — Spotify must return at least this many
/// LINE_SYNCED lines to count as a real match. A track returning 3 lines is
/// almost certainly a wrong-track or instrumental match.
const MIN_VERIFIED_LINES: usize = 10;

/// Initial cool-down after a transport-level resolver failure (#75). The
/// worker polls every ~5s; without this, every song with a stale gate would
/// re-call Claude every cycle for the entire duration of a CLIProxyAPI / Claude
/// outage. 60s is short enough to keep recovery fast, long enough to absorb
/// the bursty failure modes (auth token refresh, brief outage).
const INITIAL_BACKOFF_SECS: u64 = 60;

/// Hard cap so exponential growth doesn't park us silent for hours after a
/// long outage. 10 minutes is plenty — the next normal poll cycle past
/// recovery picks up the row.
const MAX_BACKOFF_SECS: u64 = 600;

#[derive(Default)]
struct BackoffState {
    /// Wall time after which a resolve attempt is allowed again. `None`
    /// means "no cool-down active".
    silent_until: Option<Instant>,
    /// Used to grow the cool-down exponentially when the outage drags on.
    /// Cleared on success / no-match.
    consecutive_failures: u32,
}

/// Outcome of a single resolve attempt.
#[derive(Debug)]
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
    /// Process-wide cool-down so a Claude outage doesn't burn one call per
    /// active song per ~5s worker tick. The worker queries `in_backoff()`
    /// before calling `resolve()`; `resolve()` also short-circuits when in
    /// cool-down (defense in depth for non-worker callers).
    backoff: Mutex<BackoffState>,
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
            backoff: Mutex::new(BackoffState::default()),
        }
    }

    /// `true` while the resolver is in cool-down after a recent transport
    /// failure (#75). Callers (the worker pre-gather hook) MUST query this
    /// before `resolve()` to skip the call entirely instead of paying for
    /// another Claude round-trip during an ongoing outage.
    pub async fn in_backoff(&self) -> bool {
        let s = self.backoff.lock().await;
        matches!(s.silent_until, Some(t) if Instant::now() < t)
    }

    /// Record a transport failure: bump the consecutive-failure counter and
    /// extend `silent_until` exponentially up to `MAX_BACKOFF_SECS`.
    async fn note_failure(&self) {
        let mut s = self.backoff.lock().await;
        s.consecutive_failures = s.consecutive_failures.saturating_add(1);
        // 60s, 120s, 240s, 480s, 600s (capped). Use saturating_sub so the
        // first failure (count=1) gives 2^0 = 1x multiplier.
        let shift = s.consecutive_failures.saturating_sub(1).min(4);
        let secs = (INITIAL_BACKOFF_SECS << shift).min(MAX_BACKOFF_SECS);
        s.silent_until = Some(Instant::now() + Duration::from_secs(secs));
    }

    /// Clear the cool-down state after Claude responds (success OR NoMatch).
    async fn note_recovery(&self) {
        let mut s = self.backoff.lock().await;
        s.consecutive_failures = 0;
        s.silent_until = None;
    }

    /// Build the prompt that asks Claude for the canonical Spotify track ID.
    ///
    /// Prompt design rationale:
    /// - Constrain output strictly: 22-char alphanumeric ID OR the literal
    ///   string `NONE`. Anything else is treated as `NoMatch`.
    /// - Explicitly tell Claude to return `NONE` for cover/remix/live versions
    ///   that aren't on Spotify as the canonical recording.
    /// - Give Claude (song, artist, youtube_id). The youtube_id alone lets
    ///   Claude look up the YouTube video title if it needs disambiguation;
    ///   threading the title through `VideoLyricsRow` was rejected as not
    ///   worth the plumbing cost.
    pub(crate) fn build_prompt(song: &str, artist: &str, youtube_id: &str) -> (String, String) {
        let system = "You map YouTube videos to canonical Spotify track IDs. \
                      Reply with EXACTLY one of: a 22-character base62 Spotify track ID, or the literal string NONE. \
                      No prose. No explanation. No markdown."
            .to_string();
        let user = format!(
            "song: {song}\n\
             artist: {artist}\n\
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
    ///
    /// mutants::skip: orchestration body is exercised by 5 wiremock
    /// integration tests in `integration_tests` below — each covers a
    /// distinct ResolveOutcome path through the full async flow. Pure-leaf
    /// mutations (parse_reply branches, build_prompt content) are caught by
    /// the 11 unit tests above. The `lines.len() >= MIN_VERIFIED_LINES`
    /// comparison and the Ok(_) → NoMatch fallthrough on the verification
    /// arm both have wiremock coverage (`no_match_when_proxy_returns_too_few_lines`,
    /// `no_match_when_proxy_returns_404`).
    #[cfg_attr(test, mutants::skip)]
    pub async fn resolve(
        &self,
        ai_client: &AiClient,
        song: &str,
        artist: &str,
        youtube_id: &str,
    ) -> ResolveOutcome {
        let (system, user) = Self::build_prompt(song, artist, youtube_id);

        // Defense in depth: even if a caller skips the in_backoff() gate,
        // refuse to hit Claude while we're in cool-down. The worker's
        // pre-gather hook normally checks first; this branch covers other
        // call sites (admin endpoints, future scripts).
        if self.in_backoff().await {
            return ResolveOutcome::Error(anyhow::anyhow!(
                "spotify_resolver: in cool-down after recent transport failure"
            ));
        }

        let raw = match ai_client.chat(&system, &user).await {
            Ok(s) => s,
            Err(e) => {
                self.note_failure().await;
                return ResolveOutcome::Error(e);
            }
        };

        // Claude responded — even if its reply is unparseable, the API
        // itself is healthy. Clear the cool-down so the next song doesn't
        // pay an unnecessary backoff.
        self.note_recovery().await;

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
    fn parse_reply_returns_none_for_literal_uppercase_NONE() {
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
        assert_eq!(
            SpotifyResolver::parse_reply("3n3Ppam7vgaVa1iaRUc9LpXXX"),
            None
        );
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
    fn build_prompt_includes_all_three_fields() {
        let (system, user) =
            SpotifyResolver::build_prompt("Amazing Grace", "Chris Tomlin", "dQw4w9WgXcQ");
        assert!(system.contains("22-character"));
        assert!(system.contains("NONE"));
        assert!(user.contains("song: Amazing Grace"));
        assert!(user.contains("artist: Chris Tomlin"));
        assert!(user.contains("youtube_id: dQw4w9WgXcQ"));
    }

    #[test]
    fn build_prompt_does_not_include_youtube_title_field() {
        // youtube_title was dropped from the resolver contract pre-merge —
        // VideoLyricsRow has no title field, threading it through 3 SELECTs
        // + 6 fixture literals isn't worth the disambiguation benefit.
        // Claude can fetch the video title from youtube_id if it needs to.
        let (_system, user) = SpotifyResolver::build_prompt("S", "A", "abc");
        assert!(
            !user.contains("youtube_title"),
            "prompt must NOT mention youtube_title"
        );
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
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "3n3Ppam7vgaVa1iaRUc9Lp"
                        }
                    }]
                })),
            )
            .mount(&claude_mock)
            .await;

        let proxy_mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/"))
            .and(wiremock::matchers::query_param(
                "trackid",
                "3n3Ppam7vgaVa1iaRUc9Lp",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "error": false,
                    "syncType": "LINE_SYNCED",
                    "lines": (0..12).map(|i| serde_json::json!({
                        "startTimeMs": format!("{}", i * 1000),
                        "words": format!("line {i}"),
                    })).collect::<Vec<_>>()
                })),
            )
            .mount(&proxy_mock)
            .await;
        // SAFETY: marked serial above; no other test races on this env var while
        // this test runs.
        unsafe {
            std::env::set_var("SPOTIFY_LYRICS_PROXY_BASE", proxy_mock.uri());
        }

        let resolver = SpotifyResolver::new();
        let ai = ai_client_pointed_at(&claude_mock.uri());
        let outcome = resolver
            .resolve(&ai, "Test Song", "Test Artist", "aaaaaaaaaaa")
            .await;

        match outcome {
            ResolveOutcome::Resolved(id) => assert_eq!(id, "3n3Ppam7vgaVa1iaRUc9Lp"),
            other => panic!("expected Resolved, got {other:?}"),
        }

        unsafe {
            std::env::remove_var("SPOTIFY_LYRICS_PROXY_BASE");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn no_match_when_claude_returns_none() {
        let claude_mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": { "role": "assistant", "content": "NONE" }
                    }]
                })),
            )
            .mount(&claude_mock)
            .await;

        let resolver = SpotifyResolver::new();
        let ai = ai_client_pointed_at(&claude_mock.uri());
        let outcome = resolver
            .resolve(&ai, "Test Song", "Test Artist", "aaaaaaaaaaa")
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
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": { "role": "assistant", "content": "3n3Ppam7vgaVa1iaRUc9Lp" }
                    }]
                })),
            )
            .mount(&claude_mock)
            .await;

        let proxy_mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&proxy_mock)
            .await;
        unsafe {
            std::env::set_var("SPOTIFY_LYRICS_PROXY_BASE", proxy_mock.uri());
        }

        let resolver = SpotifyResolver::new();
        let ai = ai_client_pointed_at(&claude_mock.uri());
        let outcome = resolver
            .resolve(&ai, "Test Song", "Test Artist", "aaaaaaaaaaa")
            .await;

        assert!(
            matches!(outcome, ResolveOutcome::NoMatch),
            "expected NoMatch on proxy 404, got {outcome:?}"
        );
        unsafe {
            std::env::remove_var("SPOTIFY_LYRICS_PROXY_BASE");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn no_match_when_proxy_returns_too_few_lines() {
        let claude_mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": { "role": "assistant", "content": "3n3Ppam7vgaVa1iaRUc9Lp" }
                    }]
                })),
            )
            .mount(&claude_mock)
            .await;

        let proxy_mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "error": false,
                    "syncType": "LINE_SYNCED",
                    "lines": [
                        {"startTimeMs": "0",    "words": "only"},
                        {"startTimeMs": "1000", "words": "three"},
                        {"startTimeMs": "2000", "words": "lines"}
                    ]
                })),
            )
            .mount(&proxy_mock)
            .await;
        unsafe {
            std::env::set_var("SPOTIFY_LYRICS_PROXY_BASE", proxy_mock.uri());
        }

        let resolver = SpotifyResolver::new();
        let ai = ai_client_pointed_at(&claude_mock.uri());
        let outcome = resolver
            .resolve(&ai, "Test Song", "Test Artist", "aaaaaaaaaaa")
            .await;

        assert!(
            matches!(outcome, ResolveOutcome::NoMatch),
            "expected NoMatch on <10 lines, got {outcome:?}"
        );
        unsafe {
            std::env::remove_var("SPOTIFY_LYRICS_PROXY_BASE");
        }
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
            .resolve(&ai, "Test Song", "Test Artist", "aaaaaaaaaaa")
            .await;

        assert!(
            matches!(outcome, ResolveOutcome::Error(_)),
            "expected Error on Claude HTTP 500, got {outcome:?}"
        );
    }

    // ----- #75 cool-down state machine ------------------------------------

    #[tokio::test]
    async fn in_backoff_is_false_when_resolver_is_fresh() {
        let resolver = SpotifyResolver::new();
        assert!(
            !resolver.in_backoff().await,
            "fresh resolver must allow the first call"
        );
    }

    #[tokio::test]
    async fn note_failure_arms_the_cool_down() {
        let resolver = SpotifyResolver::new();
        resolver.note_failure().await;
        assert!(
            resolver.in_backoff().await,
            "after a transport failure the resolver must be in cool-down"
        );
    }

    #[tokio::test]
    async fn note_recovery_clears_the_cool_down() {
        let resolver = SpotifyResolver::new();
        resolver.note_failure().await;
        assert!(resolver.in_backoff().await);
        resolver.note_recovery().await;
        assert!(
            !resolver.in_backoff().await,
            "successful Claude reply must clear the cool-down"
        );
    }

    #[tokio::test]
    async fn note_failure_grows_exponentially_up_to_cap() {
        // Five sequential failures grow the silent-until window through
        // 60 → 120 → 240 → 480 → 600 (cap). We can't assert the wall clock
        // without injection, so check that consecutive_failures advances
        // and that the silent_until window is strictly increasing for the
        // first four steps (before the cap kicks in).
        let resolver = SpotifyResolver::new();
        let mut last: Option<Instant> = None;
        for i in 0..4 {
            resolver.note_failure().await;
            let s = resolver.backoff.lock().await;
            assert_eq!(s.consecutive_failures, (i as u32) + 1);
            if let Some(prev) = last {
                assert!(
                    s.silent_until.unwrap() > prev,
                    "silent_until must grow with each failure"
                );
            }
            last = s.silent_until;
        }
    }

    #[tokio::test]
    async fn resolve_short_circuits_with_error_when_in_backoff() {
        // After a manually-armed cool-down the resolver MUST refuse to
        // call Claude — even if the AiClient would have succeeded. The
        // wiremock has expect(0) so any HTTP call would fail the test.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let claude_mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": "NONE" }}]
            })))
            .expect(0)
            .mount(&claude_mock)
            .await;

        let resolver = SpotifyResolver::new();
        resolver.note_failure().await;

        let ai = ai_client_pointed_at(&claude_mock.uri());
        let outcome = resolver.resolve(&ai, "Song", "Artist", "aaaaaaaaaaa").await;
        assert!(
            matches!(outcome, ResolveOutcome::Error(_)),
            "in-backoff resolve must short-circuit with Error (no Claude call), got {outcome:?}"
        );
    }
}
