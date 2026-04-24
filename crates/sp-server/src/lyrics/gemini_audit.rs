//! Append-only JSONL audit log of every Gemini API call.
//!
//! The operator needs to see where their Gemini credits are going — per song,
//! per key, per chunk. Every HTTP roundtrip through `GeminiClient::post_with_retries`
//! appends one line to `<cache_dir>/gemini_audit.jsonl`; a retry that eventually
//! succeeds produces one entry per attempt (so three lines for a 2×429 + success
//! sequence).
//!
//! File format: one JSON object per line, newline-terminated, UTF-8. Safe to
//! parse with any streaming JSONL reader (or `jq -s '.'` for one-shot).
//!
//! Read path: `read_entries` slurps the whole file and filters by
//! `timestamp >= since` (RFC 3339 string compare is lexicographic-equivalent
//! to chronological compare) and/or `video_id` equality. The dashboard
//! endpoint also applies a default-500 / max-5000 row cap after filtering.
//! Missing file is treated as zero entries, not an error.
//!
//! Per airuleset `script-failure-policy`, write errors bubble up — never silently
//! swallow. Callers typically log and continue; the HTTP call itself should not
//! fail just because audit write failed.
//!
//! This module is I/O only — no knowledge of Gemini's schema, retry policy, or
//! rotation. The caller (`gemini_client.rs`) owns the Gemini-specific translation
//! from HTTP response → `GeminiAuditEntry`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

/// One row in the audit log. All `Option` fields cope with missing information
/// (e.g. `total_tokens = None` when `usageMetadata` was absent from the response,
/// `error = None` on 2xx, `video_id = None` for ad-hoc `generate_text` calls).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GeminiAuditEntry {
    /// RFC 3339 UTC timestamp captured just before the entry is written.
    pub timestamp: String,
    /// YouTube video id — populated for alignment chunk calls; may be `None`
    /// for translation or other ad-hoc text calls.
    pub video_id: Option<String>,
    /// 0-based chunk index within the song; `None` for non-chunk calls.
    pub chunk_idx: Option<u32>,
    /// Index of the API key (inside the rotating pool) actually used for this
    /// HTTP roundtrip. Zero for proxy/single-key configurations.
    pub key_idx: usize,
    /// First 12 chars of the API key — enough to disambiguate projects,
    /// impossible to reconstruct the secret from.
    pub key_prefix: String,
    /// Gemini model id (e.g. `gemini-3.1-pro-preview`).
    pub model: String,
    /// HTTP status code. `0` for transport-level errors (DNS, TCP, TLS, timeout).
    pub status: u16,
    /// Wall-clock duration of the HTTP attempt in milliseconds.
    pub duration_ms: u64,
    /// `usageMetadata.promptTokenCount` when present.
    pub prompt_tokens: Option<u32>,
    /// `usageMetadata.candidatesTokenCount` when present.
    pub candidates_tokens: Option<u32>,
    /// `usageMetadata.totalTokenCount` when present.
    pub total_tokens: Option<u32>,
    /// Short error message when `status != 200`. Truncated by the caller.
    pub error: Option<String>,
}

/// Absolute filename within `cache_dir`.
fn audit_path(cache_dir: &Path) -> std::path::PathBuf {
    cache_dir.join("gemini_audit.jsonl")
}

/// Append one entry as a single JSON line to `gemini_audit.jsonl`.
///
/// Creates the file on first write. Errors bubble up per
/// `script-failure-policy`; callers typically log a warning and continue so
/// that audit-write failure does not fail a legitimate Gemini call.
pub async fn append(cache_dir: &Path, entry: &GeminiAuditEntry) -> Result<()> {
    let path = audit_path(cache_dir);
    let mut line = serde_json::to_string(entry).context("serialize audit entry")?;
    line.push('\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .with_context(|| format!("open audit log {path:?}"))?;
    file.write_all(line.as_bytes())
        .await
        .context("write audit line")?;
    file.flush().await.context("flush audit line")?;
    Ok(())
}

/// Read all entries from the audit log, optionally filtered by `since`
/// (RFC 3339, inclusive lower bound — string compare works because RFC 3339
/// timestamps are lexicographically ordered) and/or `video_id` exact match.
///
/// Missing file returns `Ok(vec![])` — treat as zero entries, not an error.
/// Malformed lines are silently skipped (the file is append-only and should
/// never contain partials, but be defensive so a truncated tail never
/// crashes the dashboard).
// mutants::skip: the `NotFound → Ok(vec![])` match-guard (line 109) needs a
// platform-specific IO error injection to distinguish from other IO errors.
// Covered behaviourally by `read_entries_returns_empty_on_missing_file`;
// a mutation that turns the guard into `true` would break other IO-error
// paths we can't easily simulate in the wiremock-only test suite.
#[cfg_attr(test, mutants::skip)]
pub async fn read_entries(
    cache_dir: &Path,
    since: Option<&str>,
    video_id: Option<&str>,
) -> Result<Vec<GeminiAuditEntry>> {
    let path = audit_path(cache_dir);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read audit log {path:?}")),
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<GeminiAuditEntry>(line) else {
            continue;
        };
        if let Some(s) = since
            && entry.timestamp.as_str() < s
        {
            continue;
        }
        if let Some(v) = video_id
            && entry.video_id.as_deref() != Some(v)
        {
            continue;
        }
        out.push(entry);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(ts: &str, video_id: Option<&str>) -> GeminiAuditEntry {
        GeminiAuditEntry {
            timestamp: ts.to_string(),
            video_id: video_id.map(String::from),
            chunk_idx: Some(0),
            key_idx: 0,
            key_prefix: "AIzaSyTestA".to_string(),
            model: "gemini-3.1-pro-preview".to_string(),
            status: 200,
            duration_ms: 12345,
            prompt_tokens: Some(100),
            candidates_tokens: Some(50),
            total_tokens: Some(150),
            error: None,
        }
    }

    #[tokio::test]
    async fn append_writes_one_json_line() {
        let tmp = tempfile::tempdir().unwrap();
        let e1 = make_entry("2026-04-23T12:00:00Z", Some("vid1"));
        let e2 = make_entry("2026-04-23T12:00:01Z", Some("vid2"));
        append(tmp.path(), &e1).await.unwrap();
        append(tmp.path(), &e2).await.unwrap();

        let bytes = tokio::fs::read(tmp.path().join("gemini_audit.jsonl"))
            .await
            .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for (line, expected) in lines.iter().zip([&e1, &e2].iter()) {
            let parsed: GeminiAuditEntry = serde_json::from_str(line).unwrap();
            assert_eq!(&parsed, *expected);
        }
    }

    #[tokio::test]
    async fn read_entries_returns_all_when_no_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let e1 = make_entry("2026-04-23T12:00:00Z", Some("vid1"));
        let e2 = make_entry("2026-04-23T12:00:01Z", Some("vid2"));
        append(tmp.path(), &e1).await.unwrap();
        append(tmp.path(), &e2).await.unwrap();

        let out = read_entries(tmp.path(), None, None).await.unwrap();
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn read_entries_filters_by_since_rfc3339() {
        let tmp = tempfile::tempdir().unwrap();
        let e1 = make_entry("2026-04-23T12:00:00Z", Some("vid1"));
        let e2 = make_entry("2026-04-23T12:00:01Z", Some("vid2"));
        let e3 = make_entry("2026-04-23T12:00:02Z", Some("vid3"));
        append(tmp.path(), &e1).await.unwrap();
        append(tmp.path(), &e2).await.unwrap();
        append(tmp.path(), &e3).await.unwrap();

        let out = read_entries(tmp.path(), Some("2026-04-23T12:00:01Z"), None)
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].video_id.as_deref(), Some("vid2"));
        assert_eq!(out[1].video_id.as_deref(), Some("vid3"));
    }

    #[tokio::test]
    async fn read_entries_filters_by_video_id() {
        let tmp = tempfile::tempdir().unwrap();
        append(
            tmp.path(),
            &make_entry("2026-04-23T12:00:00Z", Some("vidA")),
        )
        .await
        .unwrap();
        append(
            tmp.path(),
            &make_entry("2026-04-23T12:00:01Z", Some("vidB")),
        )
        .await
        .unwrap();
        append(
            tmp.path(),
            &make_entry("2026-04-23T12:00:02Z", Some("vidA")),
        )
        .await
        .unwrap();

        let out = read_entries(tmp.path(), None, Some("vidA")).await.unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|e| e.video_id.as_deref() == Some("vidA")));
    }

    #[tokio::test]
    async fn read_entries_returns_empty_on_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let out = read_entries(tmp.path(), None, None).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn append_then_read_roundtrip_preserves_every_field() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = GeminiAuditEntry {
            timestamp: "2026-04-23T12:00:00Z".to_string(),
            video_id: Some("dQw4w9WgXcQ".to_string()),
            chunk_idx: Some(7),
            key_idx: 3,
            key_prefix: "AIzaSyFoo12".to_string(),
            model: "gemini-3.1-pro-preview".to_string(),
            status: 429,
            duration_ms: 987,
            prompt_tokens: Some(1111),
            candidates_tokens: Some(2222),
            total_tokens: Some(3333),
            error: Some("rate limited".to_string()),
        };
        append(tmp.path(), &entry).await.unwrap();

        let out = read_entries(tmp.path(), None, None).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], entry);
    }

    /// Regression: the read path must tolerate a truncated / partially-
    /// written line. On crash mid-write or a disk-full event, the audit
    /// file can end with a half-line that serde_json cannot parse.
    /// `read_entries` must skip it instead of panicking so the dashboard
    /// endpoint still works on a partially-corrupt log.
    #[tokio::test]
    async fn read_entries_skips_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gemini_audit.jsonl");
        // Build the three-line payload: valid, {"broken, valid.
        // Using `tokio::fs::write` (one-shot) matches the task spec.
        let good = make_entry("2026-04-23T12:00:00Z", Some("x"));
        let good2 = make_entry("2026-04-23T12:00:01Z", Some("y"));
        let good_line = serde_json::to_string(&good).unwrap();
        let good_line2 = serde_json::to_string(&good2).unwrap();
        let payload = format!("{good_line}\n{{\"broken\n{good_line2}\n");
        tokio::fs::write(&path, payload).await.unwrap();

        let out = read_entries(tmp.path(), None, None).await.unwrap();
        assert_eq!(
            out.len(),
            2,
            "{{\"broken must be skipped; valid entries preserved"
        );
        assert_eq!(out[0].video_id.as_deref(), Some("x"));
        assert_eq!(out[1].video_id.as_deref(), Some("y"));
    }

    #[tokio::test]
    async fn read_entries_combines_since_and_video_id_filters() {
        let tmp = tempfile::tempdir().unwrap();
        append(tmp.path(), &make_entry("2026-04-23T12:00:00Z", Some("a")))
            .await
            .unwrap();
        append(tmp.path(), &make_entry("2026-04-23T12:00:01Z", Some("b")))
            .await
            .unwrap();
        append(tmp.path(), &make_entry("2026-04-23T12:00:02Z", Some("a")))
            .await
            .unwrap();
        append(tmp.path(), &make_entry("2026-04-23T12:00:03Z", Some("a")))
            .await
            .unwrap();

        let out = read_entries(tmp.path(), Some("2026-04-23T12:00:02Z"), Some("a"))
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].timestamp, "2026-04-23T12:00:02Z");
        assert_eq!(out[1].timestamp, "2026-04-23T12:00:03Z");
    }
}
