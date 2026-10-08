//! #229: what the exchange moves (artifact kinds), the jobs that make them,
//! and the version of each kind this node takes from a peer.

use serde::{Deserialize, Serialize};
use sp_core::metadata::MetadataSource;

use crate::metadata::manual::MANUAL_SOURCE;

/// The format of a song's video + audio pair: the split layout (video stream
/// copied, audio loudnorm -14 LUFS FLAC, CLAUDE.md "Split-file audio layout").
/// Bump when the download / normalize output changes.
pub const MEDIA_VERSION: u32 = 1;
/// The format of the karaoke stems (`stem_worker.py`). Bump when the
/// separation output changes.
pub const STEMS_VERSION: u32 = 1;
/// Metadata versions: who named the title. A title parser's guess…
pub const METADATA_PARSER: u32 = 0;
/// …a metadata provider's answer…
pub const METADATA_PROVIDER: u32 = 1;
/// …or an operator's correction.
pub const METADATA_MANUAL: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Video,
    Audio,
    StemVocals,
    StemInstrumental,
    Lyrics,
    Metadata,
    /// A kind this node does not know (a newer peer's `dub`): dropped.
    #[serde(other)]
    Unknown,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::StemVocals => "stem_vocals",
            Self::StemInstrumental => "stem_instrumental",
            Self::Lyrics => "lyrics",
            Self::Metadata => "metadata",
            Self::Unknown => "unknown",
        }
    }

    /// The kind a URL segment names (lane 4's artifact route); `None` for
    /// anything else (`unknown` too).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "video" => Some(Self::Video),
            "audio" => Some(Self::Audio),
            "stem_vocals" => Some(Self::StemVocals),
            "stem_instrumental" => Some(Self::StemInstrumental),
            "lyrics" => Some(Self::Lyrics),
            "metadata" => Some(Self::Metadata),
            _ => None,
        }
    }
}

/// A heavy job a node asks its peers about before it runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Job {
    /// Download + normalize + metadata (`DownloadWorker::process_next`).
    Download,
    Lyrics,
    Stems,
}

impl Job {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Lyrics => "lyrics",
            Self::Stems => "stems",
        }
    }

    /// What a peer must hold for this job to be fetched instead of run
    /// (`peer::decide`).
    pub fn needs(self) -> &'static [ArtifactKind] {
        match self {
            Self::Download => &[ArtifactKind::Video, ArtifactKind::Audio],
            Self::Lyrics => &[ArtifactKind::Lyrics],
            Self::Stems => &[ArtifactKind::StemVocals, ArtifactKind::StemInstrumental],
        }
    }

    /// What this job announces in its node's catalog while it runs or is
    /// queued (one entry per kind, `JobBoard::snapshot`,
    /// `catalog::listed_jobs`).
    pub fn makes(self) -> &'static [ArtifactKind] {
        match self {
            Self::Download => &[
                ArtifactKind::Video,
                ArtifactKind::Audio,
                ArtifactKind::Metadata,
            ],
            Self::Lyrics => &[ArtifactKind::Lyrics],
            Self::Stems => &[ArtifactKind::StemVocals, ArtifactKind::StemInstrumental],
        }
    }

    /// The job waits while a listed peer has the song (its catalog lists the
    /// video's audio), as for a job the peer announces (`peer::decide`): only
    /// the lyrics (#229 PP audit, comment 6054582866). A node's own lyrics
    /// stay at its pipeline version for good, and a node with no AI proxy
    /// (PP) makes a degraded track, while the peer's lyrics are made from
    /// that very audio. Stems are the same model on every node, and the
    /// download fetches the song itself.
    pub fn waits_while_a_peer_has_the_song(self) -> bool {
        false
    }

    /// The job that makes `kind` (the one whose [`Job::makes`] holds it);
    /// `None` for a kind this node does not know.
    pub fn making(kind: ArtifactKind) -> Option<Self> {
        match kind {
            ArtifactKind::Video | ArtifactKind::Audio | ArtifactKind::Metadata => {
                Some(Self::Download)
            }
            ArtifactKind::Lyrics => Some(Self::Lyrics),
            ArtifactKind::StemVocals | ArtifactKind::StemInstrumental => Some(Self::Stems),
            ArtifactKind::Unknown => None,
        }
    }
}

/// Whether a peer's `version` of `kind` is one this node takes: exactly its
/// own current format, or for metadata any provider's or operator's title.
pub fn acceptable(kind: ArtifactKind, version: u32) -> bool {
    match kind {
        ArtifactKind::Video | ArtifactKind::Audio => version == MEDIA_VERSION,
        ArtifactKind::StemVocals | ArtifactKind::StemInstrumental => version == STEMS_VERSION,
        ArtifactKind::Lyrics => version == crate::lyrics::LYRICS_PIPELINE_VERSION,
        ArtifactKind::Metadata => version >= METADATA_PROVIDER,
        ArtifactKind::Unknown => false,
    }
}

/// A row's metadata version from its `metadata_source` and `gemini_failed`.
///
/// An operator's correction is [`METADATA_MANUAL`]. A provider's answer is
/// the chain's label (`gemini`, which the Claude provider uses too) with
/// `gemini_failed = 0`. Everything else is a parser's guess: `regex` (also
/// with `gemini_failed = 0`, when no provider is configured), no source, a
/// label this node does not know, and any row in the metadata repair queue.
pub fn metadata_version(source: Option<&str>, gemini_failed: bool) -> u32 {
    match source {
        Some(MANUAL_SOURCE) => METADATA_MANUAL,
        Some(s) if s == MetadataSource::Gemini.as_str() && !gemini_failed => METADATA_PROVIDER,
        _ => METADATA_PARSER,
    }
}

#[cfg(test)]
#[path = "kind_tests.rs"]
mod tests;
