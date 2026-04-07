//! Metadata extraction — Gemini AI provider + title regex parser.

pub mod gemini;
pub mod parser;

use async_trait::async_trait;
use sp_core::metadata::VideoMetadata;

/// Errors from metadata providers.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("API request failed: {0}")]
    ApiError(String),
    #[error("Invalid response: {0}")]
    InvalidResponse(String),
    #[error("Rate limited")]
    RateLimited,
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
pub async fn get_metadata(
    providers: &[Box<dyn MetadataProvider>],
    video_id: &str,
    title: &str,
) -> VideoMetadata {
    let has_providers = !providers.is_empty();

    for provider in providers {
        match provider.extract(video_id, title).await {
            Ok(meta) => return meta,
            Err(e) => {
                tracing::warn!(
                    provider = provider.name(),
                    error = %e,
                    video_id,
                    "metadata provider failed, trying next"
                );
            }
        }
    }

    // All providers failed (or none configured) — use regex parser.
    let mut meta = parser::parse_title(title);
    if has_providers {
        meta.gemini_failed = true;
    }
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
}
