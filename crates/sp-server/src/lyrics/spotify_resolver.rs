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

use crate::ai::client::AiClient;
use crate::lyrics::spotify_proxy::SpotifyLyricsFetcher;

/// `TIER1_MIN_LINES` from `tier1.rs` — Spotify must return at least this many
/// LINE_SYNCED lines to count as a real match. A track returning 3 lines is
/// almost certainly a wrong-track or instrumental match.
const MIN_VERIFIED_LINES: usize = 10;

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
    ///
    /// mutants::skip: this orchestration function chains three I/O steps
    /// (Claude call → parse → proxy fetch). Each leaf is unit-tested
    /// independently (parse_reply tests below; SpotifyLyricsFetcher tests
    /// in spotify_proxy.rs; AiClient::chat is itself mutants::skip'd in
    /// ai/client.rs). Mutating the orchestration body without touching the
    /// leaves cannot fail any unit test without a full wiremock harness,
    /// which is added in B.2.
    #[cfg_attr(test, mutants::skip)]
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
            .resolve(&ai, "Test Song", "Test Artist", "Test Title", "aaaaaaaaaaa")
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
            .resolve(&ai, "Test Song", "Test Artist", "Test Title", "aaaaaaaaaaa")
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
            .resolve(&ai, "Test Song", "Test Artist", "Test Title", "aaaaaaaaaaa")
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
            .resolve(&ai, "Test Song", "Test Artist", "Test Title", "aaaaaaaaaaa")
            .await;

        assert!(
            matches!(outcome, ResolveOutcome::Error(_)),
            "expected Error on Claude HTTP 500, got {outcome:?}"
        );
    }
}
