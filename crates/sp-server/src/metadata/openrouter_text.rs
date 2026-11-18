//! The metadata chain's text-AI provider (#321: OpenRouter, the task's
//! model `ai_model_metadata`, blank = the newest Gemini).

use async_trait::async_trait;
use sp_core::metadata::{MetadataSource, VideoMetadata};
use std::sync::Arc;

use super::{MetadataError, MetadataProvider};
use crate::ai::client::AiClient;

/// The model's `{song, artist}` answer (#321: typed, unknown fields skipped).
#[derive(serde::Deserialize)]
struct TitleAnswer {
    #[serde(default)]
    song: Option<String>,
    #[serde(default)]
    artist: Option<String>,
}

/// The text-AI metadata provider: the same `{song, artist}` question as
/// the Gemini provider, through the metadata task's OpenRouter model.
pub struct OpenRouterMetadataProvider {
    ai_client: Arc<AiClient>,
}

impl OpenRouterMetadataProvider {
    pub fn new(ai_client: Arc<AiClient>) -> Self {
        Self { ai_client }
    }
}

#[async_trait]
impl MetadataProvider for OpenRouterMetadataProvider {
    #[cfg_attr(test, mutants::skip)]
    async fn extract(&self, _video_id: &str, title: &str) -> Result<VideoMetadata, MetadataError> {
        let system = "You extract song metadata from YouTube video titles. \
            Return ONLY a JSON object with these fields: \
            {\"song\": \"<song title>\", \"artist\": \"<artist name>\"}. \
            Clean the title: remove 'Official Video', 'Live', 'Lyrics', \
            'feat.', bracket content, etc. If the title is 'Artist - Song', \
            split accordingly. If uncertain, make your best guess.";

        // #321: the metadata task's model on OpenRouter; the answer read
        // into a typed struct, never `serde_json::Value` (an untrusted body).
        let task = sp_core::ai_tasks::AiTask::Metadata;
        let response: TitleAnswer = self
            .ai_client
            .chat_json_for(task, system, title)
            .await
            .map_err(|e| MetadataError::ApiError(format!("{e:#}")))?;

        let song = response
            .song
            .ok_or_else(|| MetadataError::InvalidResponse("missing 'song' field".into()))?;

        let artist = response
            .artist
            .ok_or_else(|| MetadataError::InvalidResponse("missing 'artist' field".into()))?;

        if song.is_empty() || artist.is_empty() {
            return Err(MetadataError::InvalidResponse(
                "empty song or artist".into(),
            ));
        }

        // Clean featuring from song title (same as Gemini provider)
        let song = super::parser::clean_song_title(&song);
        let artist = super::parser::shorten_artist(&artist);

        Ok(VideoMetadata {
            song,
            artist,
            source: MetadataSource::Gemini, // reuse existing enum variant
            gemini_failed: false,
        })
    }

    fn name(&self) -> &str {
        "openrouter"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn provider_name() {
        let client = Arc::new(crate::ai::test_support::ai_client_unused().await);
        let provider = OpenRouterMetadataProvider::new(client);
        assert_eq!(provider.name(), "openrouter");
    }
}
