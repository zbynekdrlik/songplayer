//! Description-merge per-phase audit state. Captures every transformation
//! between the raw asr_words and the final emitted AlignedTrack, then
//! writes a single JSON sidecar at `{cache_dir}/{youtube_id}_descmerge_audit.json`
//! when the caller provides an `AuditContext`.
//!
//! The captured state lets future debugging answer questions like "where
//! does whisperx time the word 'And' near 4:01?" or "did Phase 5
//! floor-clamp shift L45 forward?" without any code change to gain
//! visibility — the file always exists after a reprocess.
//!
//! Per `feedback_take_ownership.md`: the user shouldn't have to ask me to
//! add diagnostics every time something looks wrong on the wall. The
//! sidecar file is always written, so the data is there when needed.

use serde::Serialize;

use super::{AsrWord, LineEmit};
use crate::lyrics::audit_ctx::{AuditContext, write_descmerge_audit};
use crate::lyrics::backend::AlignedLine;

#[derive(Debug, Clone, Serialize)]
struct AsrWordRow {
    idx: usize,
    text: String,
    start_ms: u32,
    end_ms: u32,
}

#[derive(Debug, Clone, Serialize)]
struct EmitRow {
    line_idx: usize,
    text: String,
    matched_asr_indices: Vec<usize>,
    /// MIN of matched asr_words[i].start_ms — None when no match.
    derived_start_ms: Option<u32>,
    /// MAX of matched asr_words[i].end_ms — None when no match.
    derived_end_ms: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
struct AlignedLineRow {
    text: String,
    start_ms: u32,
    end_ms: u32,
}

#[derive(Debug, Serialize)]
pub(super) struct AuditPayload {
    candidate_source: String,
    asr_provenance: String,
    ref_line_count: usize,
    asr_word_count: usize,
    asr_words: Vec<AsrWordRow>,
    /// "claude" or "nw_dp" — which Phase-1 algorithm produced the initial emits.
    phase1_provider: Option<&'static str>,
    phase1_emits: Vec<EmitRow>,
    /// Includes Phase 2 chorus-repeat re-emissions, sorted by start_ms.
    phase2_emits: Vec<EmitRow>,
    /// AlignedLines after Phase 4 (sub-line word timing) but BEFORE the
    /// Phase 5 8 s cap + monotonic floor-clamp + drop-micro-window pass.
    pre_phase5_lines: Vec<AlignedLineRow>,
    /// Final lines after Phase 5 — what gets persisted to lyrics.json.
    post_phase5_lines: Vec<AlignedLineRow>,
}

pub(super) struct AuditState {
    candidate_source: String,
    asr_provenance: String,
    ref_line_count: usize,
    asr_words_snapshot: Vec<AsrWordRow>,
    phase1_provider: Option<&'static str>,
    phase1_emits: Vec<EmitRow>,
    phase2_emits: Vec<EmitRow>,
    pre_phase5_lines: Vec<AlignedLineRow>,
    post_phase5_lines: Vec<AlignedLineRow>,
}

impl AuditState {
    pub(super) fn new(
        ref_lines: &[String],
        asr_words: &[AsrWord],
        candidate_source: &str,
        asr_provenance: &str,
    ) -> Self {
        let asr_words_snapshot = asr_words
            .iter()
            .enumerate()
            .map(|(idx, w)| AsrWordRow {
                idx,
                text: w.norm.clone(),
                start_ms: w.start_ms,
                end_ms: w.end_ms,
            })
            .collect();
        Self {
            candidate_source: candidate_source.to_string(),
            asr_provenance: asr_provenance.to_string(),
            ref_line_count: ref_lines.len(),
            asr_words_snapshot,
            phase1_provider: None,
            phase1_emits: Vec::new(),
            phase2_emits: Vec::new(),
            pre_phase5_lines: Vec::new(),
            post_phase5_lines: Vec::new(),
        }
    }

    pub(super) fn record_phase1(
        &mut self,
        provider: &'static str,
        emits: &[LineEmit],
        asr_words: &[AsrWord],
    ) {
        self.phase1_provider = Some(provider);
        self.phase1_emits = build_emit_rows(emits, asr_words);
    }

    pub(super) fn record_phase2(&mut self, emits: &[LineEmit], asr_words: &[AsrWord]) {
        self.phase2_emits = build_emit_rows(emits, asr_words);
    }

    pub(super) fn record_pre_phase5(&mut self, lines: &[AlignedLine]) {
        self.pre_phase5_lines = build_line_rows(lines);
    }

    pub(super) fn record_post_phase5(&mut self, lines: &[AlignedLine]) {
        self.post_phase5_lines = build_line_rows(lines);
    }

    pub(super) async fn write_to_disk(self, audit_ctx: Option<&AuditContext<'_>>) {
        let payload = AuditPayload {
            candidate_source: self.candidate_source,
            asr_provenance: self.asr_provenance,
            ref_line_count: self.ref_line_count,
            asr_word_count: self.asr_words_snapshot.len(),
            asr_words: self.asr_words_snapshot,
            phase1_provider: self.phase1_provider,
            phase1_emits: self.phase1_emits,
            phase2_emits: self.phase2_emits,
            pre_phase5_lines: self.pre_phase5_lines,
            post_phase5_lines: self.post_phase5_lines,
        };
        write_descmerge_audit(audit_ctx, &payload).await;
    }
}

fn build_emit_rows(emits: &[LineEmit], asr_words: &[AsrWord]) -> Vec<EmitRow> {
    emits
        .iter()
        .enumerate()
        .map(|(line_idx, e)| {
            let derived_start_ms = e
                .asr_word_indices
                .iter()
                .map(|&i| asr_words[i].start_ms)
                .min();
            let derived_end_ms = e
                .asr_word_indices
                .iter()
                .map(|&i| asr_words[i].end_ms)
                .max();
            EmitRow {
                line_idx,
                text: e.text.clone(),
                matched_asr_indices: e.asr_word_indices.clone(),
                derived_start_ms,
                derived_end_ms,
            }
        })
        .collect()
}

fn build_line_rows(lines: &[AlignedLine]) -> Vec<AlignedLineRow> {
    lines
        .iter()
        .map(|l| AlignedLineRow {
            text: l.text.clone(),
            start_ms: l.start_ms,
            end_ms: l.end_ms,
        })
        .collect()
}
