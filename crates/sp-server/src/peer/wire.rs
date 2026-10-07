//! #229: the peer API's JSON, typed both ways. A peer's answer is untrusted:
//! no `serde_json::Value` anywhere inside (`rust-workspace.md`), unknown
//! fields are skipped, unknown kinds and job states read as `Unknown` and
//! are dropped by [`Catalog::sanitized`], with any entry whose id, sha256 or
//! node name does not hold.

use serde::{Deserialize, Serialize};

use super::config::valid_name;
use super::kind::{ArtifactKind, metadata_version};
use crate::downloader::cache::is_valid_video_id;

/// One artifact a node holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub youtube_id: String,
    pub kind: ArtifactKind,
    pub version: u32,
    pub size: u64,
    /// 64 lowercase hex digits.
    pub sha256: String,
    /// When the serving node listed (hashed) it; `None` for metadata.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Where a job a node announces stands. A peer waits for either (ROZHODNUTÉ
/// 6022851957: both sites sync the same playlists, so a song SNV has only
/// queued would otherwise be processed at PP too).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// The node runs it now (its in-memory job board).
    Running,
    /// The node will run it (its rows wait in that job's queue).
    Queued,
    /// A state this node does not know (a newer peer's): dropped.
    #[serde(other)]
    Unknown,
}

/// One job a node announces in its catalog, one entry per kind the job makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogJob {
    pub youtube_id: String,
    pub kind: ArtifactKind,
    pub node: String,
    pub state: JobState,
    /// When a running job started; `None` for a queued one.
    #[serde(default)]
    pub started_at: Option<String>,
}

/// `GET /api/v1/peer/catalog`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub node: String,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    #[serde(default)]
    pub jobs: Vec<CatalogJob>,
}

impl Catalog {
    /// Only what this node can use: an entry needs a known kind (and job
    /// state), a real YouTube id, a sha256 as 64 lowercase hex digits and, for
    /// a job, a node name that holds (`peer::config`'s rule). Separately, the
    /// catalog's own `node` reads as empty when it is not a node name, and a
    /// time that is not an RFC 3339 time reads as `None` (its entry stays: a
    /// time is information only, never a decision's input).
    pub fn sanitized(mut self) -> Self {
        if !valid_name(&self.node) {
            self.node.clear();
        }
        self.artifacts.retain(|a| {
            a.kind != ArtifactKind::Unknown
                && is_sha256_hex(&a.sha256)
                && is_valid_video_id(&a.youtube_id)
        });
        self.jobs.retain(|j| {
            j.kind != ArtifactKind::Unknown
                && j.state != JobState::Unknown
                && valid_name(&j.node)
                && is_valid_video_id(&j.youtube_id)
        });
        for a in &mut self.artifacts {
            a.updated_at = checked_time(a.updated_at.take());
        }
        for j in &mut self.jobs {
            j.started_at = checked_time(j.started_at.take());
        }
        self
    }

    /// The node announces a job for `youtube_id` making any of `kinds`,
    /// running or queued (on a [`Catalog::sanitized`] catalog).
    pub fn announces(&self, youtube_id: &str, kinds: &[ArtifactKind]) -> bool {
        self.jobs
            .iter()
            .any(|j| j.youtube_id == youtube_id && kinds.contains(&j.kind))
    }
}

/// A video's title as a node holds it: the `metadata` artifact's bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerMetadata {
    pub youtube_id: String,
    pub song: String,
    pub artist: String,
    pub metadata_source: Option<String>,
    pub gemini_failed: bool,
}

impl PeerMetadata {
    pub fn version(&self) -> u32 {
        metadata_version(self.metadata_source.as_deref(), self.gemini_failed)
    }

    /// The canonical bytes: the catalog's metadata sha256 is over these.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}

/// A peer's time, kept only when it is an RFC 3339 time (a peer controls the
/// text: it could be anything, of any length).
fn checked_time(time: Option<String>) -> Option<String> {
    time.filter(|t| rfc3339_to_ms(t).is_some())
}

/// 64 lowercase hex digits.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// `2026-10-06T16:00:00.123Z`; an instant out of chrono's range reads as "".
pub fn ms_to_rfc3339(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

pub fn rfc3339_to_ms(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|t| t.timestamp_millis())
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
