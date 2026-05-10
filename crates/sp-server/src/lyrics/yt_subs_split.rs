//! yt_subs caption-window clustering helper.
//!
//! YouTube auto-subtitle (`yt_subs`) lines have line-level timing that is
//! generally accurate, but the line BREAKS reflect the caption-display
//! window — they split mid-phrase ("Thank You for / today That You have
//! made"). cluster_caption_windows merges adjacent (gap == 0) yt_subs
//! lines into real phrases, after which the orchestrator routes them
//! through `text_reference_merge` (the same pipeline that processes
//! description+whisperx). text_reference_merge handles karaoke split
//! (Phase 3 Claude), chorus repeats (Phase 2 + 2.8 sliding-window LCS),
//! mishearing absorbs (2.6 / 2.65 / 2.7), and cap+monotonic (Phase 5).

use crate::lyrics::backend::AlignedLine;

/// Cluster YouTube caption-window adjacent yt_subs lines back into
/// real phrases. yt_subs auto-captions split mid-phrase wherever the
/// caption-display window ends, producing back-to-back lines with
/// `line[i].end_ms == line[i+1].start_ms`. Real phrase boundaries
/// always have a non-zero gap (singer pauses, instrumental). Merge
/// adjacent (gap == 0) lines so downstream pipelines receive full
/// phrases instead of caption fragments.
///
/// Whitespace inside merged text is normalized to single spaces.
pub(crate) fn cluster_caption_windows(lines: &[AlignedLine]) -> Vec<AlignedLine> {
    let mut out: Vec<AlignedLine> = Vec::with_capacity(lines.len());
    for line in lines {
        if let Some(last) = out.last_mut() {
            if last.end_ms >= line.start_ms {
                let merged = format!("{} {}", last.text.trim(), line.text.trim());
                last.text = merged.split_whitespace().collect::<Vec<_>>().join(" ");
                last.end_ms = line.end_ms;
                continue;
            }
        }
        out.push(line.clone());
    }
    out
}

#[cfg(test)]
#[path = "yt_subs_split_tests.rs"]
mod tests;
