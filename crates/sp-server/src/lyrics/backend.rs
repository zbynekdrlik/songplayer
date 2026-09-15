//! Aligned-track data structures produced by the v21 forced-alignment
//! reference stage (`orchestrator::run_reference_stage` → `worker_reference`).
//!
//! The pluggable `AlignmentBackend` trait + WhisperX/Replicate impl were
//! deleted in #159 (one-regime cleanup); only these plain data types survive,
//! carrying mtl line timings from `run_reference_stage` to
//! `worker::align_track_to_lyrics_track`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlignedWord {
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlignedLine {
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
    /// `None` for line-only output. Renderer falls back to line-level
    /// highlighting when None — never synthesize evenly distributed word
    /// timings (per `feedback_line_timing_only.md`).
    pub words: Option<Vec<AlignedWord>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlignedTrack {
    pub lines: Vec<AlignedLine>,
    /// e.g. "description+mtl@rev1/g35t-ok"
    pub provenance: String,
    /// Self-reported metadata. NOT a quality gate.
    pub raw_confidence: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_line_words_can_be_none() {
        let line = AlignedLine {
            text: "line-only output".into(),
            start_ms: 0,
            end_ms: 5000,
            words: None,
        };
        assert!(line.words.is_none());
    }

    #[test]
    fn aligned_track_round_trips_serde() {
        let t = AlignedTrack {
            lines: vec![AlignedLine {
                text: "hello world".into(),
                start_ms: 0,
                end_ms: 1000,
                words: None,
            }],
            provenance: "description+mtl@rev1/g35t-ok".into(),
            raw_confidence: 1.0,
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: AlignedTrack = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }
}
