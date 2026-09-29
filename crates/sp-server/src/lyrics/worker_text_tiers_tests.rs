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

/// A cache dir for a vocal path that does not exist: nothing is kept (the
/// vocal has no identity), so these tests exercise the transcription alone.
fn nowhere() -> std::path::PathBuf {
    std::env::temp_dir().join("sp_text_tiers_no_vocal")
}

#[tokio::test]
async fn the_isolated_vocal_is_transcribed_once() {
    let backend = TranscribeOnly::answering(Ok(words()));
    let got = transcribe_vocal(
        &backend,
        Some(Path::new("/v.wav")),
        &keys(),
        "yt1",
        &nowhere(),
    )
    .await;
    assert_eq!(got, Ok(Some(words())));
    assert_eq!(backend.calls(), 1);
}

/// A failed transcription defers the song (`g35t_error`): no mtl, no base
/// tier on a transcript that does not exist.
#[tokio::test]
async fn a_failed_transcription_defers_the_song() {
    let backend = TranscribeOnly::answering(Err(anyhow::anyhow!("503 from Gemini")));
    let got = transcribe_vocal(
        &backend,
        Some(Path::new("/v.wav")),
        &keys(),
        "yt1",
        &nowhere(),
    )
    .await;
    assert_eq!(got, Err("g35t_error"));
    assert_eq!(backend.calls(), 1);
}

/// No isolated vocal: nothing is transcribed here — the base tier decides
/// between the #171 full-mix fallback and a deferral.
#[tokio::test]
async fn no_isolated_vocal_takes_no_transcript() {
    let backend = TranscribeOnly::answering(Ok(words()));
    let got = transcribe_vocal(&backend, None, &keys(), "yt1", &nowhere()).await;
    assert_eq!(got, Ok(None));
    assert_eq!(backend.calls(), 0);
}

/// No Gemini key: nothing is transcribed here — the base tier defers
/// `gemini_key_missing`.
#[tokio::test]
async fn no_gemini_key_takes_no_transcript() {
    let backend = TranscribeOnly::answering(Ok(words()));
    let got = transcribe_vocal(&backend, Some(Path::new("/v.wav")), &[], "yt1", &nowhere()).await;
    assert_eq!(got, Ok(None));
    assert_eq!(backend.calls(), 0);
}

/// A no-penalty deferral re-picks the song: the transcript kept from this
/// vocal's first pick is reused, g35t is not asked again (the second backend
/// would fail the song if it were).
#[tokio::test]
async fn a_repick_of_the_same_vocal_reuses_the_kept_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("yt1_vocals16k.wav");
    std::fs::write(&wav, b"RIFF-vocal").unwrap();

    let first = TranscribeOnly::answering(Ok(words()));
    let got = transcribe_vocal(&first, Some(wav.as_path()), &keys(), "yt1", dir.path()).await;
    assert_eq!(got, Ok(Some(words())));
    assert_eq!(first.calls(), 1);
    assert!(dir.path().join("yt1_g35t_words.json").exists());

    let second = TranscribeOnly::answering(Err(anyhow::anyhow!("must not be asked")));
    let got = transcribe_vocal(&second, Some(wav.as_path()), &keys(), "yt1", dir.path()).await;
    assert_eq!(got, Ok(Some(words())));
    assert_eq!(second.calls(), 0);
}

/// A new vocal (isolation ran again: another length) is transcribed afresh.
#[tokio::test]
async fn a_new_vocal_is_transcribed_again() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("yt1_vocals16k.wav");
    std::fs::write(&wav, b"RIFF-vocal").unwrap();
    let first = TranscribeOnly::answering(Ok(words()));
    transcribe_vocal(&first, Some(wav.as_path()), &keys(), "yt1", dir.path())
        .await
        .unwrap();

    std::fs::write(&wav, b"RIFF-another-vocal").unwrap();
    let again = vec![AsrWord {
        text: "new".into(),
        start_ms: 0,
        end_ms: 300,
    }];
    let second = TranscribeOnly::answering(Ok(again.clone()));
    let got = transcribe_vocal(&second, Some(wav.as_path()), &keys(), "yt1", dir.path()).await;
    assert_eq!(got, Ok(Some(again)));
    assert_eq!(second.calls(), 1);
}

/// A transcript kept long ago (outside the reuse window) is not reused: a
/// later reprocess transcribes afresh.
#[tokio::test]
async fn an_old_kept_transcript_is_not_reused() {
    use crate::lyrics::transcript_cache;
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("yt1_vocals16k.wav");
    std::fs::write(&wav, b"RIFF-vocal").unwrap();
    let vocal = transcript_cache::vocal_identity(&std::fs::metadata(&wav).unwrap()).unwrap();
    transcript_cache::store(
        &transcript_cache::path(dir.path(), "yt1"),
        vocal,
        1,
        &words(),
    )
    .await;

    let fresh = TranscribeOnly::answering(Ok(words()));
    let got = transcribe_vocal(&fresh, Some(wav.as_path()), &keys(), "yt1", dir.path()).await;
    assert_eq!(got, Ok(Some(words())));
    assert_eq!(
        fresh.calls(),
        1,
        "a 1970 transcript is past the reuse window"
    );
}

/// The helper contract: once the transcript is retired (as `run_text_tiers`
/// does when a pass ends), the next pass — a manual reprocess of the same
/// vocal, minutes later — transcribes afresh.
#[tokio::test]
async fn the_next_pass_transcribes_afresh() {
    use crate::lyrics::transcript_cache;
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("yt1_vocals16k.wav");
    std::fs::write(&wav, b"RIFF-vocal").unwrap();
    let first = TranscribeOnly::answering(Ok(words()));
    transcribe_vocal(&first, Some(wav.as_path()), &keys(), "yt1", dir.path())
        .await
        .unwrap();
    transcript_cache::retire(dir.path(), "yt1").await;

    let next = TranscribeOnly::answering(Ok(words()));
    let got = transcribe_vocal(&next, Some(wav.as_path()), &keys(), "yt1", dir.path()).await;
    assert_eq!(got, Ok(Some(words())));
    assert_eq!(next.calls(), 1);
}

fn candidate(source: &str) -> crate::lyrics::tier1::CandidateText {
    crate::lyrics::tier1::CandidateText {
        source: source.into(),
        lines: vec!["a".into()],
        line_timings: None,
        has_timing: false,
    }
}

/// The title search runs on a non-empty transcript, when no artist+title
/// lookup found the song and the ★ tier can use its result.
#[test]
fn the_title_search_runs_only_when_its_result_can_be_used() {
    let sung = words();
    let own = [candidate("description")];
    assert!(should_title_search(&sung, &own, true));
    assert!(!should_title_search(&[], &own, true), "empty transcript");
    assert!(!should_title_search(&sung, &own, false), "no mtl tooling");
    assert!(
        !should_title_search(&sung, &[candidate("lrclib")], true),
        "a lookup found the song"
    );
}

/// #144: a pass that ends in a track retires its kept transcript inside
/// `run_text_tiers` itself. Offline: `new_for_test`'s tools dir has no mtl
/// tooling (no title search, the ★ stage skips) and there is no Gemini key,
/// so the kept transcript is the only one and the base tier builds the track
/// from it.
#[tokio::test]
async fn a_pass_ending_in_a_track_retires_its_transcript() {
    use crate::lyrics::transcript_cache;
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    let worker = crate::lyrics::worker::LyricsWorker::new_for_test(
        pool,
        dir.path().to_path_buf(),
        events_tx,
    );
    let wav = dir.path().join("yt1_vocals16k.wav");
    std::fs::write(&wav, b"RIFF-vocal").unwrap();
    let vocal = transcript_cache::vocal_identity(&std::fs::metadata(&wav).unwrap()).unwrap();
    transcript_cache::store(
        &transcript_cache::path(dir.path(), "yt1"),
        vocal,
        transcript_cache::now_ms(),
        &words(),
    )
    .await;
    let row = crate::db::models::VideoLyricsRow {
        id: 1,
        youtube_id: "yt1".into(),
        song: "Amazing Grace".into(),
        artist: "A".into(),
        duration_ms: Some(200_000),
        audio_file_path: None,
        youtube_url: String::new(),
        lyrics_override_text: None,
        lyrics_time_offset_ms: 0,
        spotify_track_id: None,
        spotify_resolved_at: None,
    };

    let outcome = worker
        .run_text_tiers(
            &row,
            Vec::new(),
            Some(wav.as_path()),
            None,
            crate::lyrics::heavy_plan::ProcessingMode::LowPriority,
            0,
        )
        .await
        .unwrap();
    let TierOutcome::Track(track) = outcome else {
        panic!("expected the base-tier track from the kept transcript");
    };
    assert_eq!(track.source, crate::lyrics::g35t_transcript::SOURCE_G35T);
    assert_eq!(track.lines[0].en, "amazing grace");
    assert!(!transcript_cache::path(dir.path(), "yt1").exists());
    assert!(transcript_cache::used_path(dir.path(), "yt1").exists());
}
