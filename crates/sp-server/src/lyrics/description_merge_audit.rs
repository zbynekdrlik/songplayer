//! Diagnostic logging helpers for description_merge wall-verification (#78).
//!
//! Two structured trace events per song:
//!
//! 1. `description_merge: asr_words audit` — full whisperx word stream as
//!    `(start_ms, end_ms, norm)` tuples. Lets the operator cross-check what
//!    whisperx actually heard at any given audio time against the displayed
//!    line's start_ms.
//! 2. `description_merge: pre-Phase-5 audit` — emit boundaries before the
//!    cap+monotonic pass, plus the matched asr-word indices per emit. Lets
//!    the operator detect when Phase 5 floor-clamp shifted a line away from
//!    its real audio start time (8 s cap on a previous chorus repeat
//!    pushing the next line's start forward).

use tracing::info;

use super::{AsrWord, LineEmit};
use crate::lyrics::backend::AlignedLine;

pub(super) fn log_asr_words(
    asr_words: &[AsrWord],
    candidate_source: &str,
    asr_provenance: &str,
    ref_line_count: usize,
) {
    let asr_audit: Vec<(u32, u32, &str)> = asr_words
        .iter()
        .map(|w| (w.start_ms, w.end_ms, w.norm.as_str()))
        .collect();
    info!(
        candidate_source = %candidate_source,
        asr_provenance = %asr_provenance,
        ref_line_count,
        asr_word_count = asr_words.len(),
        asr_words = ?asr_audit,
        "description_merge: asr_words audit"
    );
}

pub(super) fn log_pre_phase5(output: &[AlignedLine], emits: &[LineEmit]) {
    let pre_p5: Vec<(u32, u32, &str)> = output
        .iter()
        .map(|l| (l.start_ms, l.end_ms, l.text.as_str()))
        .collect();
    let emit_matches: Vec<(&str, Vec<usize>)> = emits
        .iter()
        .map(|e| (e.text.as_str(), e.asr_word_indices.clone()))
        .collect();
    info!(
        emit_count = output.len(),
        pre_phase5_emits = ?pre_p5,
        emit_to_asr_indices = ?emit_matches,
        "description_merge: pre-Phase-5 audit"
    );
}
