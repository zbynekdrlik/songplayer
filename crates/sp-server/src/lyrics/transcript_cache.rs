//! #144: the song's one g35t transcript, kept on disk as
//! `{youtube_id}_g35t_words.json`.
//!
//! The transcript is taken right after isolation, before mtl. A no-penalty
//! deferral after it — the idle-only wall gate before mtl, the heavy-slot
//! memory floor, the startup grace, an mtl wall-abort — re-picks the song;
//! the kept transcript spares that re-pick a second g35t call. It is reused
//! only for the SAME isolated vocal (its byte length and modification time)
//! and within `REUSE_WINDOW_MS` of being taken, so a later reprocess
//! transcribes afresh. The file also keeps the gate's own transcript on disk
//! for a later threshold measurement (#144 had to measure on the v20
//! WhisperX transcripts instead).

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::lyrics::g35t_client::AsrWord;

/// How long a kept transcript may be reused: long enough for any no-penalty
/// deferral re-pick, short of a later reprocess (6 h).
pub const REUSE_WINDOW_MS: u64 = 21_600_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CachedWord {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// The file's content: the vocal it was taken from, when, and the words.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CachedTranscript {
    pub wav_len: u64,
    pub wav_mtime_ms: u64,
    pub taken_at_ms: u64,
    pub words: Vec<CachedWord>,
}

impl CachedTranscript {
    pub(crate) fn words(&self) -> Vec<AsrWord> {
        self.words
            .iter()
            .map(|w| AsrWord {
                text: w.text.clone(),
                start_ms: w.start_ms,
                end_ms: w.end_ms,
            })
            .collect()
    }
}

/// A vocal file's identity: its byte length and modification time (ms since
/// the epoch). `None` when the platform reports no modification time.
pub(crate) fn vocal_identity(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((meta.len(), mtime.as_millis() as u64))
}

/// A kept transcript is reused for the same vocal, when it holds words, and
/// within `REUSE_WINDOW_MS` of being taken.
pub(crate) fn reusable(cached: &CachedTranscript, vocal: (u64, u64), now_ms: u64) -> bool {
    cached.wav_len == vocal.0
        && cached.wav_mtime_ms == vocal.1
        && !cached.words.is_empty()
        && now_ms.saturating_sub(cached.taken_at_ms) < REUSE_WINDOW_MS
}

pub(crate) fn path(cache_dir: &Path, youtube_id: &str) -> PathBuf {
    cache_dir.join(format!("{youtube_id}_g35t_words.json"))
}

/// Milliseconds since the epoch, now.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The kept transcript at `path`, if one parses.
pub(crate) async fn load(path: &Path) -> Option<CachedTranscript> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Keep `words` taken from the vocal `vocal` at `taken_at_ms`. Best-effort:
/// a failed write is logged, the song goes on.
pub(crate) async fn store(path: &Path, vocal: (u64, u64), taken_at_ms: u64, words: &[AsrWord]) {
    let cached = CachedTranscript {
        wav_len: vocal.0,
        wav_mtime_ms: vocal.1,
        taken_at_ms,
        words: words
            .iter()
            .map(|w| CachedWord {
                text: w.text.clone(),
                start_ms: w.start_ms,
                end_ms: w.end_ms,
            })
            .collect(),
    };
    let result = match serde_json::to_vec(&cached) {
        Ok(bytes) => tokio::fs::write(path, bytes)
            .await
            .map_err(anyhow::Error::from),
        Err(e) => Err(anyhow::Error::from(e)),
    };
    if let Err(e) = result {
        warn!(path = %path.display(), error = %e, "g35t: keeping the transcript failed");
    }
}

#[cfg(test)]
#[path = "transcript_cache_tests.rs"]
mod tests;
