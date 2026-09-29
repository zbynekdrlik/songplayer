//! Metadata extraction — the provider chain (Claude, Gemini) + title regex parser.
//!
//! #136: the ONE production chain is [`chain::provider_chain`], built once in
//! `lib.rs` and shared by the download worker, the reprocess worker and the
//! API (`status.metadata`, `POST /api/v1/metadata/probe`).

pub mod chain;
pub mod claude;
pub mod gemini;
pub mod health;
pub mod parser;
pub mod sanitize;
#[cfg(test)]
pub(crate) mod test_support;

pub use chain::{ProviderChain, provider_chain};

use async_trait::async_trait;
use sp_core::metadata::VideoMetadata;

/// Errors from metadata providers.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("API request failed: {0}")]
    ApiError(String),
    #[error("Invalid response: {0}")]
    InvalidResponse(String),
    /// Every key / the provider is out of quota (429); the reprocess worker
    /// enters its cooldown. The text names the last status + body excerpt.
    #[error("rate limited: {0}")]
    RateLimited(String),
}

/// A pluggable metadata extraction backend.
#[async_trait]
pub trait MetadataProvider: Send + Sync {
    /// Try to extract metadata for the given video.
    async fn extract(&self, video_id: &str, title: &str) -> Result<VideoMetadata, MetadataError>;

    /// Human-readable provider name (for logging).
    fn name(&self) -> &str;
}

/// Try each provider in order; fall back to the title regex parser.
///
/// If providers were available but all failed, the returned metadata has
/// `gemini_failed = true` so the caller can schedule a retry later.
///
/// This is the single choke point for emoji sanitization (#135): both
/// return paths run `sanitize::strip_emoji` over `song`/`artist` before
/// returning, so every source — a provider's own output (Gemini, Claude)
/// and the regex-parser fallback — ships clean text regardless of whether
/// that source sanitizes itself.
pub async fn get_metadata(
    providers: &[Box<dyn MetadataProvider>],
    video_id: &str,
    title: &str,
) -> VideoMetadata {
    let has_providers = !providers.is_empty();

    for provider in providers {
        match provider.extract(video_id, title).await {
            Ok(mut meta) => {
                meta.song = sanitize::strip_emoji(&meta.song);
                meta.artist = sanitize::strip_emoji(&meta.artist);
                if meta.song.trim().is_empty() {
                    // A provider's own emptiness check runs on the RAW
                    // text — it can pass for e.g. an all-emoji song —
                    // but sanitization just reduced it to nothing.
                    // Never ship an empty song; fall back to the title
                    // parser the same way an outright provider failure
                    // would (#136).
                    tracing::warn!(
                        provider = provider.name(),
                        video_id,
                        "provider song sanitized to empty; falling back to title parser"
                    );
                    return fallback_from_title(title, true);
                }
                return meta;
            }
            Err(e) => {
                // The chain's `Recorded` wrapper already WARNed this failure
                // with its latency (#136); here only the next step.
                tracing::debug!(
                    provider = provider.name(),
                    error = %e,
                    video_id,
                    "metadata provider failed, trying next"
                );
            }
        }
    }

    // All providers failed (or none configured) — use regex parser.
    fallback_from_title(title, has_providers)
}

/// Regex-parser fallback shared by both "all providers failed" and "a
/// provider's song sanitized to empty" — always runs the sanitizer over
/// its own output too, since a title can itself carry emoji.
fn fallback_from_title(title: &str, mark_gemini_failed: bool) -> VideoMetadata {
    let mut meta = parser::parse_title(title);
    if mark_gemini_failed {
        meta.gemini_failed = true;
    }
    meta.song = sanitize::strip_emoji(&meta.song);
    meta.artist = sanitize::strip_emoji(&meta.artist);
    meta
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sp_core::metadata::MetadataSource;

    /// A provider that always succeeds with fixed metadata.
    struct SuccessProvider {
        song: String,
        artist: String,
    }

    #[async_trait]
    impl MetadataProvider for SuccessProvider {
        async fn extract(
            &self,
            _video_id: &str,
            _title: &str,
        ) -> Result<VideoMetadata, MetadataError> {
            Ok(VideoMetadata {
                song: self.song.clone(),
                artist: self.artist.clone(),
                source: MetadataSource::Gemini,
                gemini_failed: false,
            })
        }

        fn name(&self) -> &str {
            "success-mock"
        }
    }

    /// A provider that always fails.
    struct FailProvider;

    #[async_trait]
    impl MetadataProvider for FailProvider {
        async fn extract(
            &self,
            _video_id: &str,
            _title: &str,
        ) -> Result<VideoMetadata, MetadataError> {
            Err(MetadataError::ApiError("mock failure".into()))
        }

        fn name(&self) -> &str {
            "fail-mock"
        }
    }

    #[tokio::test]
    async fn single_provider_success() {
        let providers: Vec<Box<dyn MetadataProvider>> = vec![Box::new(SuccessProvider {
            song: "Test Song".into(),
            artist: "Test Artist".into(),
        })];

        let meta = get_metadata(&providers, "abc123", "ignored title").await;
        assert_eq!(meta.song, "Test Song");
        assert_eq!(meta.artist, "Test Artist");
        assert_eq!(meta.source, MetadataSource::Gemini);
        assert!(!meta.gemini_failed);
    }

    #[tokio::test]
    async fn fallback_to_second_provider() {
        let providers: Vec<Box<dyn MetadataProvider>> = vec![
            Box::new(FailProvider),
            Box::new(SuccessProvider {
                song: "Second".into(),
                artist: "Provider".into(),
            }),
        ];

        let meta = get_metadata(&providers, "abc123", "ignored").await;
        assert_eq!(meta.song, "Second");
        assert_eq!(meta.artist, "Provider");
        assert!(!meta.gemini_failed);
    }

    #[tokio::test]
    async fn all_providers_fail_uses_parser_with_gemini_failed() {
        let providers: Vec<Box<dyn MetadataProvider>> =
            vec![Box::new(FailProvider), Box::new(FailProvider)];

        let meta = get_metadata(&providers, "abc123", "Elevation Worship - The Blessing").await;
        assert_eq!(meta.song, "The Blessing");
        assert_eq!(meta.artist, "Elevation Worship");
        assert_eq!(meta.source, MetadataSource::Regex);
        assert!(
            meta.gemini_failed,
            "should mark gemini_failed when providers existed but all failed"
        );
    }

    #[tokio::test]
    async fn no_providers_uses_parser_without_gemini_failed() {
        let providers: Vec<Box<dyn MetadataProvider>> = vec![];

        let meta = get_metadata(&providers, "abc123", "Elevation Worship - The Blessing").await;
        assert_eq!(meta.song, "The Blessing");
        assert_eq!(meta.artist, "Elevation Worship");
        assert_eq!(meta.source, MetadataSource::Regex);
        assert!(
            !meta.gemini_failed,
            "should NOT mark gemini_failed when no providers configured"
        );
    }

    /// A provider whose song sanitizes to nothing (e.g. an all-emoji
    /// title) validated itself against the RAW, unsanitized text — its
    /// own check passes, but `strip_emoji` at the choke point can still
    /// turn it into `""`. Mirrors the live production defect: five
    /// ytalex rows shipped `song=""`, `gemini_failed=false` because
    /// nothing re-checked emptiness AFTER sanitization.
    struct EmptyAfterSanitizeProvider;

    #[async_trait]
    impl MetadataProvider for EmptyAfterSanitizeProvider {
        async fn extract(
            &self,
            _video_id: &str,
            _title: &str,
        ) -> Result<VideoMetadata, MetadataError> {
            Ok(VideoMetadata {
                song: "🔥".into(),
                artist: "Ignored Artist".into(),
                source: MetadataSource::Gemini,
                gemini_failed: false,
            })
        }

        fn name(&self) -> &str {
            "empty-after-sanitize-mock"
        }
    }

    #[tokio::test]
    async fn provider_song_empty_after_sanitization_falls_back_to_parser() {
        let providers: Vec<Box<dyn MetadataProvider>> = vec![Box::new(EmptyAfterSanitizeProvider)];

        let meta = get_metadata(&providers, "abc123", "Elevation Worship - The Blessing").await;

        assert_eq!(
            meta.song, "The Blessing",
            "an empty-after-sanitization song must fall back to the title parser"
        );
        assert_eq!(meta.artist, "Elevation Worship");
        assert_eq!(meta.source, MetadataSource::Regex);
        assert!(
            meta.gemini_failed,
            "falling back after a provider's song sanitized to empty must set gemini_failed"
        );
    }
}
