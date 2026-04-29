//! Tier-1 — free text + line-timing fetchers.
//!
//! Fetchers run in parallel; if any returns has_timing=true with a plausible
//! line count, the orchestrator ships directly without calling Tier-2 (WhisperX).
//!
//! Task B.2 only seeds the CandidateText type. Task B.3 adds the
//! Tier1Collector with the parallel-fetch + short-circuit logic.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateText {
    /// "tier1:spotify" / "tier1:lrclib" / "tier1:yt_subs" / "genius" etc.
    pub source: String,
    pub lines: Vec<String>,
    /// `Some` when the fetcher has line-level timing. `start_ms`, `end_ms` per line.
    pub line_timings: Option<Vec<(u64, u64)>>,
    pub has_timing: bool,
}
