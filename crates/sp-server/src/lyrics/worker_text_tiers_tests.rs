//! Tests for `worker_text_tiers.rs` (#144): the song's one g35t transcript.

use super::*;
use std::path::Path;
use std::sync::Mutex;

use crate::lyrics::g35t_client::AsrWord;
use crate::lyrics::orchestrator::ReferenceStageBackend;

/// A backend whose `asr_transcribe` answers once from `asr` and counts its
/// calls; `mtl_align` must never run here.
struct TranscribeOnly {
    asr: Mutex<Option<anyhow::Result<Vec<AsrWord>>>>,
    calls: Mutex<usize>,
}

impl TranscribeOnly {
    fn answering(asr: anyhow::Result<Vec<AsrWord>>) -> Self {
        Self {
            asr: Mutex::new(Some(asr)),
            calls: Mutex::new(0),
        }
    }
    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl ReferenceStageBackend for TranscribeOnly {
    async fn mtl_align(
        &self,
        _vocals_wav: &Path,
        _video_id: &str,
        _lines: &[String],
    ) -> anyhow::Result<crate::lyrics::mtl_aligner::MtlOutput> {
        unreachable!("the transcript step never runs mtl")
    }
    async fn asr_transcribe(&self, _vocals_wav: &Path) -> anyhow::Result<Vec<AsrWord>> {
        *self.calls.lock().unwrap() += 1;
        self.asr
            .lock()
            .unwrap()
            .take()
            .expect("asr_transcribe called twice")
    }
}

fn keys() -> Vec<String> {
    vec!["k1".to_string()]
}

fn words() -> Vec<AsrWord> {
    vec![
        AsrWord {
            text: "amazing".into(),
            start_ms: 1_000,
            end_ms: 1_400,
        },
        AsrWord {
            text: "grace".into(),
            start_ms: 1_400,
            end_ms: 2_000,
        },
    ]
}

#[tokio::test]
async fn the_isolated_vocal_is_transcribed_once() {
    let backend = TranscribeOnly::answering(Ok(words()));
    let got = transcribe_vocal(&backend, Some(Path::new("/v.wav")), &keys(), "yt1").await;
    assert_eq!(got, Ok(Some(words())));
    assert_eq!(backend.calls(), 1);
}

/// A failed transcription defers the song (`g35t_error`): no mtl, no base
/// tier on a transcript that does not exist.
#[tokio::test]
async fn a_failed_transcription_defers_the_song() {
    let backend = TranscribeOnly::answering(Err(anyhow::anyhow!("503 from Gemini")));
    let got = transcribe_vocal(&backend, Some(Path::new("/v.wav")), &keys(), "yt1").await;
    assert_eq!(got, Err("g35t_error"));
    assert_eq!(backend.calls(), 1);
}

/// No isolated vocal: nothing is transcribed here — the base tier decides
/// between the #171 full-mix fallback and a deferral.
#[tokio::test]
async fn no_isolated_vocal_takes_no_transcript() {
    let backend = TranscribeOnly::answering(Ok(words()));
    let got = transcribe_vocal(&backend, None, &keys(), "yt1").await;
    assert_eq!(got, Ok(None));
    assert_eq!(backend.calls(), 0);
}

/// No Gemini key: nothing is transcribed here — the base tier defers
/// `gemini_key_missing`.
#[tokio::test]
async fn no_gemini_key_takes_no_transcript() {
    let backend = TranscribeOnly::answering(Ok(words()));
    let got = transcribe_vocal(&backend, Some(Path::new("/v.wav")), &[], "yt1").await;
    assert_eq!(got, Ok(None));
    assert_eq!(backend.calls(), 0);
}
