//! Audit context: per-song debug-output sink shared across the lyrics
//! pipeline. When populated, every alignment + merge stage writes a JSON
//! sidecar to `cache_dir` so future debugging never requires a code change
//! to gain visibility into what whisperx heard or how the merge stages
//! transformed the data.
//!
//! Sidecar files written:
//!
//! - `{youtube_id}_whisperx_track.json` — raw `AlignedTrack` from the
//!   alignment backend, including word-level timings. Authoritative ground
//!   truth for any "where did the line start" question.
//! - `{youtube_id}_descmerge_audit.json` — description/override merge
//!   internal state at every phase boundary: flattened asr_words, post-
//!   Phase-1 emits with matched asr indices, post-Phase-2 chorus repeats,
//!   pre-Phase-5 emit boundaries, and final post-Phase-5 emits.
//!
//! Both files are overwritten on every reprocess so the LATEST run is
//! always available — no log rotation issues.
//!
//! Construction is `Option<AuditContext>` plumbed through
//! `OrchestratorInput.audit → claude_merge::merge → description_merge::
//! process`. When `None`, every stage skips its sidecar write — keeps unit
//! tests free of file-system side effects without a per-test setup.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tracing::warn;

use crate::lyrics::backend::AlignedTrack;

#[derive(Debug, Clone, Copy)]
pub struct AuditContext<'a> {
    pub cache_dir: &'a Path,
    pub youtube_id: &'a str,
}

impl AuditContext<'_> {
    pub fn whisperx_track_path(&self) -> PathBuf {
        self.cache_dir
            .join(format!("{}_whisperx_track.json", self.youtube_id))
    }

    pub fn descmerge_audit_path(&self) -> PathBuf {
        self.cache_dir
            .join(format!("{}_descmerge_audit.json", self.youtube_id))
    }
}

/// Write the raw `AlignedTrack` returned by the alignment backend to
/// `{cache_dir}/{youtube_id}_whisperx_track.json`. Pretty-printed for
/// human-readable diff. Errors are logged but never propagated — audit is
/// best-effort and must never block a successful reprocess.
pub async fn write_whisperx_track(audit: Option<&AuditContext<'_>>, asr: &AlignedTrack) {
    let Some(ctx) = audit else { return };
    let path = ctx.whisperx_track_path();
    match serde_json::to_string_pretty(asr) {
        Ok(json) => {
            if let Err(e) = tokio::fs::write(&path, json).await {
                warn!(path = %path.display(), %e, "audit: write whisperx_track failed");
            }
        }
        Err(e) => {
            warn!(path = %path.display(), %e, "audit: serialize whisperx_track failed");
        }
    }
}

/// Write a serializable description-merge phase snapshot to
/// `{cache_dir}/{youtube_id}_descmerge_audit.json`. The full set of
/// per-phase fields is composed by the caller; this helper just persists
/// the pre-built JSON value.
pub async fn write_descmerge_audit<T: Serialize + ?Sized>(
    audit: Option<&AuditContext<'_>>,
    payload: &T,
) {
    let Some(ctx) = audit else { return };
    let path = ctx.descmerge_audit_path();
    match serde_json::to_string_pretty(payload) {
        Ok(json) => {
            if let Err(e) = tokio::fs::write(&path, json).await {
                warn!(path = %path.display(), %e, "audit: write descmerge_audit failed");
            }
        }
        Err(e) => {
            warn!(path = %path.display(), %e, "audit: serialize descmerge_audit failed");
        }
    }
}
