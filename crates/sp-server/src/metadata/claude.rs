//! Claude Opus metadata provider via CLIProxyAPI.

use async_trait::async_trait;
use sp_core::metadata::{MetadataSource, VideoMetadata};
use std::sync::Arc;

use super::{MetadataError, MetadataProvider};
use crate::ai::client::AiClient;

/// Claude Opus metadata provider.
///
/// Uses the same prompt as Gemini but routes through CLIProxyAPI → Claude.
pub struct ClaudeMetadataProvider {
    ai_client: Arc<AiClient>,
}

impl ClaudeMetadataProvider {
    pub fn new(ai_client: Arc<AiClient>) -> Self {
        Self { ai_client }
    }
}

#[async_trait]
impl MetadataProvider for ClaudeMetadataProvider {
    async fn extract(&self, _video_id: &str, title: &str) -> Result<VideoMetadata, MetadataError> {
        let system = "You extract song metadata from YouTube video titles. \
            Return ONLY a JSON object with these fields: \
            {\"song\": \"<song title>\", \"artist\": \"<artist name>\"}. \
            Clean the title: remove 'Official Video', 'Live', 'Lyrics', \
            'feat.', bracket content, etc. If the title is 'Artist - Song', \
            split accordingly. If uncertain, make your best guess.";

        let response: serde_json::Value = self
            .ai_client
            .chat_json(system, title)
            .await
            .map_err(|e| MetadataError::ApiError(e.to_string()))?;

        let song = response["song"]
            .as_str()
            .ok_or_else(|| MetadataError::InvalidResponse("missing 'song' field".into()))?
            .to_string();

        let artist = response["artist"]
            .as_str()
            .ok_or_else(|| MetadataError::InvalidResponse("missing 'artist' field".into()))?
            .to_string();

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
        "claude"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::AiSettings;

    #[test]
    fn provider_name() {
        let client = Arc::new(AiClient::new(AiSettings::default()));
        let provider = ClaudeMetadataProvider::new(client);
        assert_eq!(provider.name(), "claude");
    }
}
