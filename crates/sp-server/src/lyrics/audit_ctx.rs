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
//! `OrchestratorInput.audit → text_reference_merge::process`. When `None`,
//! every stage skips its sidecar write — keeps unit tests free of
//! file-system side effects without a per-test setup.

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::backend::{AlignedLine, AlignedTrack};

    fn ctx<'a>(dir: &'a Path, id: &'a str) -> AuditContext<'a> {
        AuditContext {
            cache_dir: dir,
            youtube_id: id,
        }
    }

    #[test]
    fn whisperx_track_path_includes_youtube_id_and_extension() {
        let dir = Path::new("/tmp/cache");
        let path = ctx(dir, "abc123").whisperx_track_path();
        // Default::default() for PathBuf is empty — would not contain id
        // or end in .json. Kills `replace -> PathBuf with Default::default()`.
        let s = path.to_string_lossy();
        assert!(
            s.contains("abc123_whisperx_track.json"),
            "must contain id+suffix; got {s}"
        );
        assert!(path.parent().is_some_and(|p| p == Path::new("/tmp/cache")));
    }

    #[test]
    fn descmerge_audit_path_includes_youtube_id_and_extension() {
        let dir = Path::new("/var/data");
        let path = ctx(dir, "xyz789").descmerge_audit_path();
        let s = path.to_string_lossy();
        assert!(
            s.contains("xyz789_descmerge_audit.json"),
            "must contain id+suffix; got {s}"
        );
        assert!(path.parent().is_some_and(|p| p == Path::new("/var/data")));
    }

    #[tokio::test]
    async fn write_whisperx_track_creates_file_with_pretty_json() {
        // Mutation `replace write_whisperx_track with ()` makes the function
        // a no-op. With audit=Some(...) and a tempdir, the original writes
        // a file; the mutation does not. Test: file exists + parses back to
        // a structurally-correct AlignedTrack.
        let tmp = tempfile::tempdir().unwrap();
        let id = "test-vid";
        let audit_ctx = ctx(tmp.path(), id);
        let track = AlignedTrack {
            lines: vec![AlignedLine {
                text: "hello world".into(),
                start_ms: 0,
                end_ms: 1000,
                words: None,
            }],
            provenance: "test@1".into(),
            raw_confidence: 0.5,
        };
        write_whisperx_track(Some(&audit_ctx), &track).await;
        let written_path = audit_ctx.whisperx_track_path();
        assert!(written_path.exists(), "file must be created");
        let body = tokio::fs::read_to_string(&written_path).await.unwrap();
        let parsed: AlignedTrack = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.provenance, "test@1");
        assert_eq!(parsed.lines.len(), 1);
        assert_eq!(parsed.lines[0].text, "hello world");
    }

    #[tokio::test]
    async fn write_whisperx_track_no_op_when_audit_none() {
        // The early-return `let Some(ctx) = audit else { return };` is the
        // documented behaviour. Verifies the function does not panic and
        // does not need a real cache_dir when audit is None.
        let track = AlignedTrack {
            lines: vec![],
            provenance: "x".into(),
            raw_confidence: 0.0,
        };
        write_whisperx_track(None, &track).await;
    }

    #[tokio::test]
    async fn write_descmerge_audit_creates_file_with_payload() {
        // Mutation `replace write_descmerge_audit with ()` makes the
        // function a no-op. Same shape as the whisperx-track test: write
        // → read back → assert payload.
        let tmp = tempfile::tempdir().unwrap();
        let id = "test-merge";
        let audit_ctx = ctx(tmp.path(), id);
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Payload {
            phase: u32,
            note: String,
        }
        let payload = Payload {
            phase: 5,
            note: "ok".into(),
        };
        write_descmerge_audit(Some(&audit_ctx), &payload).await;
        let written_path = audit_ctx.descmerge_audit_path();
        assert!(written_path.exists(), "file must be created");
        let body = tokio::fs::read_to_string(&written_path).await.unwrap();
        let parsed: Payload = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.phase, 5);
        assert_eq!(parsed.note, "ok");
    }

    #[tokio::test]
    async fn write_descmerge_audit_no_op_when_audit_none() {
        write_descmerge_audit::<()>(None, &()).await;
    }
}
