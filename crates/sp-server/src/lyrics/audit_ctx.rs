//! Audit context: per-song debug-output sink for the lyrics pipeline.
//!
//! After #159 the only sidecar still written is the Lever-2 (#143)
//! reference-gate decision:
//!
//! - `{youtube_id}_alignment_audit.json` — the reviewable record for a
//!   `Fail`/`Error` reference-stage outcome (a `Pass` needs no audit; the
//!   stamped `+mtl@rev1/g35t-ok` source IS the record).
//!
//! The v20 whisperx-track and description-merge sidecars were removed with
//! those routes. When `audit` is `None`, the write is skipped — keeps unit
//! tests free of file-system side effects.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tracing::warn;

#[derive(Debug, Clone, Copy)]
pub struct AuditContext<'a> {
    pub cache_dir: &'a Path,
    pub youtube_id: &'a str,
}

impl AuditContext<'_> {
    /// #143 — Lever-2 forced-alignment reference-gate decision sidecar.
    pub fn alignment_audit_path(&self) -> PathBuf {
        self.cache_dir
            .join(format!("{}_alignment_audit.json", self.youtube_id))
    }
}

/// Write the Lever-2 (#143) reference-gate decision to
/// `{cache_dir}/{youtube_id}_alignment_audit.json`. Best-effort: a write
/// failure is logged, never propagated.
pub async fn write_alignment_audit<T: Serialize + ?Sized>(
    audit: Option<&AuditContext<'_>>,
    payload: &T,
) {
    let Some(ctx) = audit else { return };
    let path = ctx.alignment_audit_path();
    match serde_json::to_string_pretty(payload) {
        Ok(json) => {
            if let Err(e) = tokio::fs::write(&path, json).await {
                warn!(path = %path.display(), %e, "audit: write alignment_audit failed");
            }
        }
        Err(e) => {
            warn!(path = %path.display(), %e, "audit: serialize alignment_audit failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(dir: &'a Path, id: &'a str) -> AuditContext<'a> {
        AuditContext {
            cache_dir: dir,
            youtube_id: id,
        }
    }

    #[test]
    fn alignment_audit_path_includes_youtube_id_and_extension() {
        let dir = Path::new("/cache");
        let path = ctx(dir, "ref001").alignment_audit_path();
        let s = path.to_string_lossy();
        assert!(
            s.contains("ref001_alignment_audit.json"),
            "must contain id+suffix; got {s}"
        );
        assert!(path.parent().is_some_and(|p| p == Path::new("/cache")));
    }

    #[tokio::test]
    async fn write_alignment_audit_creates_file_with_pretty_json() {
        let tmp = tempfile::tempdir().unwrap();
        let audit_ctx = ctx(tmp.path(), "ref-test");
        let payload = serde_json::json!({"verdict": "fail", "reason": "offset"});
        write_alignment_audit(Some(&audit_ctx), &payload).await;
        let content = tokio::fs::read_to_string(audit_ctx.alignment_audit_path())
            .await
            .expect("file must exist");
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["verdict"], "fail");
    }

    #[tokio::test]
    async fn write_alignment_audit_is_noop_when_audit_is_none() {
        let payload = serde_json::json!({"verdict": "fail"});
        write_alignment_audit::<serde_json::Value>(None, &payload).await;
    }
}
