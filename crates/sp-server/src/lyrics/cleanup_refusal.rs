//! #144: a scraped-lyrics / description cleanup Claude's upstream content
//! filter refused, remembered in the cleanup's own cache file for THAT raw
//! text (its sha256): the same text is refused every time, and every refusal
//! puts the proxy's one credential into its 60 s cooldown (every AI call
//! refused meanwhile), so it is never sent again — until the text changes
//! (a Genius page edited, a new description). The marker carries no
//! `lines` key, so it never reads as a cleanup's answer.

use std::path::Path;

/// The sha256 (lowercase hex) of a cleanup's raw text.
pub(crate) fn raw_sha256(raw: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(raw.as_bytes()))
}

/// Whether the cache at `path` records a refusal of exactly `raw`.
pub(crate) async fn refused_earlier(path: &Path, raw: &str) -> bool {
    recorded_sha(path).await.as_deref() == Some(raw_sha256(raw).as_str())
}

/// Whether the cache at `path` records a refusal at all (of any text).
pub(crate) async fn records_a_refusal(path: &Path) -> bool {
    recorded_sha(path).await.is_some()
}

async fn recorded_sha(path: &Path) -> Option<String> {
    let bytes = tokio::fs::read(path).await.ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("refused_raw_sha256")?.as_str().map(str::to_string)
}

/// Record that the content filter refused the cleanup of `raw` (a failed
/// write is only logged: the next pass asks once more).
pub(crate) async fn remember(path: &Path, raw: &str) {
    let marker = serde_json::json!({
        "refused": "content_filter",
        "refused_raw_sha256": raw_sha256(raw),
    });
    if let Err(e) = tokio::fs::write(path, marker.to_string()).await {
        tracing::warn!(cache_path = %path.display(), %e, "cleanup: could not record the content filter's refusal");
    }
}

#[cfg(test)]
#[path = "cleanup_refusal_tests.rs"]
mod tests;
