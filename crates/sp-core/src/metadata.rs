//! Video metadata types.

use serde::{Deserialize, Serialize};

/// How metadata was extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetadataSource {
    Gemini,
    Regex,
}

impl MetadataSource {
    /// Returns a stable string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::Regex => "regex",
        }
    }
}

/// Extracted metadata for a video.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VideoMetadata {
    pub song: String,
    pub artist: String,
    pub source: MetadataSource,
    pub gemini_failed: bool,
}
