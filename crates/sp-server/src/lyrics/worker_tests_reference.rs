//! Lever-2 (#143) reference-stage tests for `lyrics::worker`. Split out of
//! `worker_tests.rs` to keep it under the 1000-line airuleset cap. Included
//! as a sibling file via
//! `#[path = "worker_tests_reference.rs"] #[cfg(test)] mod tests_reference;`
//! from `worker.rs`; shares `worker`'s items via `use super::*`.
//!
//! Also home to `new_for_test_with_tools_dir` — a `#[cfg(test)]` constructor
//! used only by these reference-stage tests. It lives in this child module
//! (rather than the sibling `worker_reference.rs`) so it can build the
//! `LyricsWorker` struct literal via the parent module's private fields.

use super::*;
use std::path::Path;

use crate::lyrics::worker_reference::alignment_model_for_source;

impl LyricsWorker {
    /// Same as `new_for_test`, but with a caller-supplied `tools_dir` — the
    /// Lever-2 (#143) reference-stage tests need a private/isolated
    /// `tools_dir` (mtl availability is read from a real file on disk via
    /// `MtlConfig::is_available`) so they cannot share `new_for_test`'s
    /// hardcoded `/tmp/tools`, which would race other concurrently-running
    /// tests over the same path.
    pub(crate) fn new_for_test_with_tools_dir(
        pool: SqlitePool,
        cache_dir: std::path::PathBuf,
        tools_dir: std::path::PathBuf,
        events_tx: broadcast::Sender<ServerMsg>,
    ) -> Self {
        use std::path::PathBuf;
        Self {
            pool,
            client: Client::new(),
            cache_dir,
            ytdlp_path: PathBuf::from("yt-dlp"),
            python_path: None,
            tools_dir,
            script_path: PathBuf::from("/tmp/script"),
            models_dir: PathBuf::from("/tmp/models"),
            ai_client: None,
            venv_python: tokio::sync::RwLock::new(None),
            retry_backoff: tokio::sync::Mutex::new(RetryBackoff::default()),
            events_tx,
            spotify_resolver: crate::lyrics::spotify_resolver::SpotifyResolver::new(),
            current_processing: Arc::new(RwLock::new(None)),
            ndi_health_registry: None,
            obs_state: None,
            wall_gate_log: std::sync::Mutex::new(crate::lyrics::idle_gate::GateLog::default()),
            peer: None,
        }
    }
}

// -----------------------------------------------------------------------
// Lever 2 (#143) — `alignment_model_for_source` (pure) tests
// -----------------------------------------------------------------------

#[test]
fn alignment_model_for_source_mtl_checked_first() {
    // The stamped label is "<candidate.source>+mtl@rev1/g35t-ok" — mtl must win.
    assert_eq!(
        alignment_model_for_source("description+mtl@rev1/g35t-ok"),
        Some(crate::lyrics::ALIGNMENT_MODEL_MTL_REV1)
    );
}

#[test]
fn alignment_model_for_source_g35t_base_tier() {
    assert_eq!(
        alignment_model_for_source("gemini-3-5-transcribe"),
        Some(crate::lyrics::ALIGNMENT_MODEL_G35T_REV1)
    );
}

#[test]
fn alignment_model_for_source_raw_ship_through() {
    assert_eq!(
        alignment_model_for_source("yt_subs"),
        Some(crate::lyrics::ALIGNMENT_MODEL_NONE)
    );
    assert_eq!(
        alignment_model_for_source("lrclib"),
        Some(crate::lyrics::ALIGNMENT_MODEL_NONE)
    );
    assert_eq!(
        alignment_model_for_source("spotify"),
        Some(crate::lyrics::ALIGNMENT_MODEL_NONE)
    );
}

#[test]
fn alignment_model_for_source_unknown_is_none() {
    assert_eq!(alignment_model_for_source("ensemble:gemini"), None);
    // #159: the whisperx / timed-merge routes are deleted, so their legacy
    // labels (only ever seen on un-reprocessed DB rows) map to None now.
    assert_eq!(
        alignment_model_for_source("description+whisperx-large-v3@rev1"),
        None
    );
    assert_eq!(alignment_model_for_source("lrclib+timed-merge"), None);
}

// -----------------------------------------------------------------------
// Lever 2 (#143) — `run_mtl_reference_stage` tests
// -----------------------------------------------------------------------

/// Backend that must never be called — used by the skip-condition tests
/// below, which must short-circuit before touching mtl/asr at all.
struct UnreachableBackend;

#[async_trait::async_trait]
impl crate::lyrics::orchestrator::ReferenceStageBackend for UnreachableBackend {
    async fn mtl_align(
        &self,
        _vocals_wav: &Path,
        _video_id: &str,
        _lines: &[String],
    ) -> anyhow::Result<crate::lyrics::mtl_aligner::MtlOutput> {
        unreachable!("mtl_align must not be called when the stage is skipped")
    }
    async fn asr_transcribe(
        &self,
        _vocals_wav: &Path,
    ) -> anyhow::Result<Vec<crate::lyrics::g35t_client::AsrWord>> {
        unreachable!("asr_transcribe must not be called when the stage is skipped")
    }
}

/// Single-use fake — mirrors `orchestrator_tests.rs`'s `FakeReferenceStageBackend`.
struct FakeReferenceStageBackend {
    mtl: std::sync::Mutex<Option<anyhow::Result<crate::lyrics::mtl_aligner::MtlOutput>>>,
    asr: std::sync::Mutex<Option<anyhow::Result<Vec<crate::lyrics::g35t_client::AsrWord>>>>,
}

#[async_trait::async_trait]
impl crate::lyrics::orchestrator::ReferenceStageBackend for FakeReferenceStageBackend {
    async fn mtl_align(
        &self,
        _vocals_wav: &Path,
        _video_id: &str,
        _lines: &[String],
    ) -> anyhow::Result<crate::lyrics::mtl_aligner::MtlOutput> {
        self.mtl
            .lock()
            .unwrap()
            .take()
            .expect("mtl_align called twice")
    }
    async fn asr_transcribe(
        &self,
        _vocals_wav: &Path,
    ) -> anyhow::Result<Vec<crate::lyrics::g35t_client::AsrWord>> {
        self.asr
            .lock()
            .unwrap()
            .take()
            .expect("asr_transcribe called twice")
    }
}

// Direct `run_reference_stage` transport-error coverage (re-homed from the
// deleted orchestrator_tests.rs — this is SURVIVING code): an `mtl_align`
// failure returns `ReferenceStageResult::Error` naming the stage, so the worker
// falls through to the g35t base tier. (A transcription failure is
// `worker_text_tiers::transcribe_vocal`'s since #144, tested there.)
#[tokio::test]
async fn run_reference_stage_mtl_align_error_returns_error_stage() {
    let backend = FakeReferenceStageBackend {
        mtl: std::sync::Mutex::new(Some(Err(anyhow::anyhow!("mtl boom")))),
        asr: std::sync::Mutex::new(None), // the stage never transcribes (#144)
    };
    let lines = vec!["a".to_string(), "b".to_string()];
    let words = vec![crate::lyrics::g35t_client::AsrWord {
        text: "a".into(),
        start_ms: 0,
        end_ms: 300,
    }];
    let result = crate::lyrics::orchestrator::run_reference_stage(
        &backend,
        Path::new("/x.wav"),
        "yt1",
        &lines,
        &words,
    )
    .await;
    match result {
        crate::lyrics::orchestrator::ReferenceStageResult::Error { stage, message } => {
            assert_eq!(stage, "mtl_align");
            assert!(message.contains("mtl boom"), "message: {message}");
        }
        _ => panic!("expected Error stage=mtl_align"),
    }
}

fn ref_candidate(source: &str, n_lines: usize) -> crate::lyrics::tier1::CandidateText {
    crate::lyrics::tier1::CandidateText {
        source: source.to_string(),
        lines: (0..n_lines).map(|i| format!("line {i}")).collect(),
        line_timings: None,
        has_timing: false,
    }
}

/// A one-word transcript: enough for the stage to reach its other skip
/// checks (an EMPTY transcript is a skip of its own, #144).
fn one_word() -> Vec<crate::lyrics::g35t_client::AsrWord> {
    vec![crate::lyrics::g35t_client::AsrWord {
        text: "line".into(),
        start_ms: 0,
        end_ms: 300,
    }]
}

/// A tools dir with all three `MtlConfig::is_available()` paths present, so
/// `run_mtl_reference_stage` does not short-circuit on the availability
/// check. `new_for_test`'s hardcoded `/tmp/tools` deliberately has none of
/// these, so the availability skip and these tests never race each other.
fn available_mtl_tools_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let venv_scripts = tmp.path().join("mtl_aligner_venv").join("Scripts");
    std::fs::create_dir_all(&venv_scripts).unwrap();
    std::fs::write(venv_scripts.join("python.exe"), b"").unwrap();
    std::fs::write(tmp.path().join("lyrics_alignment_mtl_run.py"), b"").unwrap();
    std::fs::create_dir_all(tmp.path().join("LyricsAlignment-MTL")).unwrap();
    tmp
}

#[tokio::test]
async fn run_mtl_reference_stage_skips_when_tooling_unavailable() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let cache_dir = std::env::temp_dir().join("sp_reference_stage_unavailable_test");
    let _ = std::fs::create_dir_all(&cache_dir);
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    // new_for_test's hardcoded "/tmp/tools" has no mtl tooling installed.
    let worker =
        crate::lyrics::worker::LyricsWorker::new_for_test(pool, cache_dir.clone(), events_tx);
    let cand = ref_candidate("description", 6);
    let result = worker
        .run_mtl_reference_stage(
            "yt1",
            Some(&cand),
            Some(Path::new("/x.wav")),
            &one_word(),
            &UnreachableBackend,
        )
        .await;
    assert!(result.unwrap().is_none());
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn run_mtl_reference_stage_skips_when_no_vocals_wav() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let cache_dir = std::env::temp_dir().join("sp_reference_stage_novocals_test");
    let _ = std::fs::create_dir_all(&cache_dir);
    let tools = available_mtl_tools_dir();
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    let worker = crate::lyrics::worker::LyricsWorker::new_for_test_with_tools_dir(
        pool,
        cache_dir.clone(),
        tools.path().to_path_buf(),
        events_tx,
    );
    let cand = ref_candidate("description", 6);
    let result = worker
        .run_mtl_reference_stage("yt1", Some(&cand), None, &one_word(), &UnreachableBackend)
        .await;
    assert!(result.unwrap().is_none());
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn run_mtl_reference_stage_skips_when_no_candidate() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let cache_dir = std::env::temp_dir().join("sp_reference_stage_nocand_test");
    let _ = std::fs::create_dir_all(&cache_dir);
    let tools = available_mtl_tools_dir();
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    let worker = crate::lyrics::worker::LyricsWorker::new_for_test_with_tools_dir(
        pool,
        cache_dir.clone(),
        tools.path().to_path_buf(),
        events_tx,
    );
    let result = worker
        .run_mtl_reference_stage(
            "yt1",
            None,
            Some(Path::new("/x.wav")),
            &one_word(),
            &UnreachableBackend,
        )
        .await;
    assert!(result.unwrap().is_none());
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn run_mtl_reference_stage_skips_when_candidate_too_short() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let cache_dir = std::env::temp_dir().join("sp_reference_stage_short_test");
    let _ = std::fs::create_dir_all(&cache_dir);
    let tools = available_mtl_tools_dir();
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    let worker = crate::lyrics::worker::LyricsWorker::new_for_test_with_tools_dir(
        pool,
        cache_dir.clone(),
        tools.path().to_path_buf(),
        events_tx,
    );
    let cand = ref_candidate("description", 3); // below the 4-line floor
    let result = worker
        .run_mtl_reference_stage(
            "yt1",
            Some(&cand),
            Some(Path::new("/x.wav")),
            &one_word(),
            &UnreachableBackend,
        )
        .await;
    assert!(result.unwrap().is_none());
    let _ = std::fs::remove_dir_all(&cache_dir);
}

/// #144: an empty transcript can never pass the gate (no line matches), so
/// the stage skips before spending an mtl run on it; the base tier then
/// quarantines the song as `asr_gap` from the same empty transcript (or, for
/// a song the wall already serves, records only the attempt and keeps its
/// lyrics — `worker_outcome::quarantine_empty_transcript`).
#[tokio::test]
async fn run_mtl_reference_stage_skips_when_the_transcript_is_empty() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let cache_dir = std::env::temp_dir().join("sp_reference_stage_empty_transcript_test");
    let _ = std::fs::create_dir_all(&cache_dir);
    let tools = available_mtl_tools_dir();
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    let worker = crate::lyrics::worker::LyricsWorker::new_for_test_with_tools_dir(
        pool,
        cache_dir.clone(),
        tools.path().to_path_buf(),
        events_tx,
    );
    let cand = ref_candidate("description", 6);
    let result = worker
        .run_mtl_reference_stage(
            "yt1",
            Some(&cand),
            Some(Path::new("/x.wav")),
            &[],
            &UnreachableBackend,
        )
        .await;
    assert!(result.unwrap().is_none());
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn run_mtl_reference_stage_pass_stamps_source_and_leaves_the_star_to_the_persist() {
    use crate::lyrics::g35t_client::AsrWord;
    use crate::lyrics::mtl_aligner::{MtlLine, MtlOutput};

    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name) VALUES (1, 'p', 'u', 'n')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let video_id: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, song, artist, normalized) \
         VALUES (1, 'yt_pass', 'T', 'S', 'A', 1) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let cache_dir = std::env::temp_dir().join("sp_reference_stage_pass_test");
    let _ = std::fs::create_dir_all(&cache_dir);
    let tools = available_mtl_tools_dir();
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    let worker = crate::lyrics::worker::LyricsWorker::new_for_test_with_tools_dir(
        pool.clone(),
        cache_dir.clone(),
        tools.path().to_path_buf(),
        events_tx,
    );

    let cand = crate::lyrics::tier1::CandidateText {
        source: "description".to_string(),
        lines: vec![
            "amazing grace".into(),
            "how sweet the sound".into(),
            "that saved a wretch".into(),
            "like me".into(),
        ],
        line_timings: None,
        has_timing: false,
    };
    let mtl_out = MtlOutput {
        lines: vec![
            MtlLine {
                text: "amazing grace".into(),
                start_ms: Some(1000),
                end_ms: Some(2000),
            },
            MtlLine {
                text: "how sweet the sound".into(),
                start_ms: Some(2100),
                end_ms: Some(3500),
            },
            MtlLine {
                text: "that saved a wretch".into(),
                start_ms: Some(3600),
                end_ms: Some(4900),
            },
            MtlLine {
                text: "like me".into(),
                start_ms: Some(5000),
                end_ms: Some(6000),
            },
        ],
        device: "cuda".into(),
        elapsed_s: 42.0,
    };
    // Independent ASR agrees closely — a clean gate PASS (see the identical
    // fixture rationale in orchestrator_tests.rs::run_reference_stage_pass_*).
    // Each line's first word starts within ±50 ms of the mtl line start.
    let words = vec![
        AsrWord {
            text: "amazing".into(),
            start_ms: 1010,
            end_ms: 1500,
        },
        AsrWord {
            text: "grace".into(),
            start_ms: 1500,
            end_ms: 1990,
        },
        AsrWord {
            text: "how".into(),
            start_ms: 2120,
            end_ms: 2300,
        },
        AsrWord {
            text: "sweet".into(),
            start_ms: 2300,
            end_ms: 2600,
        },
        AsrWord {
            text: "the".into(),
            start_ms: 2600,
            end_ms: 2800,
        },
        AsrWord {
            text: "sound".into(),
            start_ms: 2800,
            end_ms: 3480,
        },
        AsrWord {
            text: "that".into(),
            start_ms: 3610,
            end_ms: 3900,
        },
        AsrWord {
            text: "saved".into(),
            start_ms: 3900,
            end_ms: 4200,
        },
        AsrWord {
            text: "a".into(),
            start_ms: 4200,
            end_ms: 4400,
        },
        AsrWord {
            text: "wretch".into(),
            start_ms: 4400,
            end_ms: 4880,
        },
        AsrWord {
            text: "like".into(),
            start_ms: 5020,
            end_ms: 5400,
        },
        AsrWord {
            text: "me".into(),
            start_ms: 5400,
            end_ms: 5980,
        },
    ];
    let backend = FakeReferenceStageBackend {
        mtl: std::sync::Mutex::new(Some(Ok(mtl_out))),
        asr: std::sync::Mutex::new(None), // the stage never transcribes (#144)
    };

    let result = worker
        .run_mtl_reference_stage(
            "yt_pass",
            Some(&cand),
            Some(Path::new("/x.wav")),
            &words,
            &backend,
        )
        .await;

    let track = result.unwrap().expect("expected Some(track) on gate PASS");
    assert_eq!(
        track.source, "description+mtl@rev1/g35t-ok",
        "source must be <candidate.source>+mtl@rev1/g35t-ok"
    );
    assert_eq!(
        alignment_model_for_source(&track.source),
        Some(crate::lyrics::ALIGNMENT_MODEL_MTL_REV1)
    );

    let reference: i64 = sqlx::query_scalar("SELECT lyrics_reference FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        reference, 0,
        "#144 F1: the gate writes no ★ — it comes with the persisted track, on every row"
    );

    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn run_mtl_reference_stage_fail_leaves_the_star_to_the_persist_and_writes_audit() {
    use crate::lyrics::g35t_client::AsrWord;
    use crate::lyrics::mtl_aligner::{MtlLine, MtlOutput};

    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name) VALUES (1, 'p', 'u', 'n')",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Starts with lyrics_reference = 1 to prove the gate leaves ★ alone
    // (#144 F1: the persisted base-tier track clears it).
    let video_id: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, song, artist, normalized, \
         lyrics_reference) VALUES (1, 'yt_fail', 'T', 'S', 'A', 1, 1) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let cache_dir = std::env::temp_dir().join("sp_reference_stage_fail_test");
    let _ = std::fs::create_dir_all(&cache_dir);
    let tools = available_mtl_tools_dir();
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    let worker = crate::lyrics::worker::LyricsWorker::new_for_test_with_tools_dir(
        pool.clone(),
        cache_dir.clone(),
        tools.path().to_path_buf(),
        events_tx,
    );

    let cand = crate::lyrics::tier1::CandidateText {
        source: "description".to_string(),
        lines: vec![
            "amazing grace".into(),
            "how sweet the sound".into(),
            "that saved a wretch".into(),
            "like me".into(),
        ],
        line_timings: None,
        has_timing: false,
    };
    let mtl_out = MtlOutput {
        lines: vec![
            MtlLine {
                text: "amazing grace".into(),
                start_ms: Some(1000),
                end_ms: Some(2000),
            },
            MtlLine {
                text: "how sweet the sound".into(),
                start_ms: Some(2100),
                end_ms: Some(3500),
            },
            MtlLine {
                text: "that saved a wretch".into(),
                start_ms: Some(3600),
                end_ms: Some(4900),
            },
            MtlLine {
                text: "like me".into(),
                start_ms: Some(5000),
                end_ms: Some(6000),
            },
        ],
        device: "cpu".into(),
        elapsed_s: 12.5,
    };
    // Same words as the PASS fixture but the whole song is shifted +30 000 ms
    // — must fail the gate on Offset regardless of matching algorithm
    // specifics (the design's whole-song sanity check, #130 2026-09-12
    // design comment).
    let words = vec![
        AsrWord {
            text: "amazing".into(),
            start_ms: 31010,
            end_ms: 31500,
        },
        AsrWord {
            text: "grace".into(),
            start_ms: 31500,
            end_ms: 31990,
        },
        AsrWord {
            text: "how".into(),
            start_ms: 32120,
            end_ms: 32300,
        },
        AsrWord {
            text: "sweet".into(),
            start_ms: 32300,
            end_ms: 32600,
        },
        AsrWord {
            text: "the".into(),
            start_ms: 32600,
            end_ms: 32800,
        },
        AsrWord {
            text: "sound".into(),
            start_ms: 32800,
            end_ms: 33480,
        },
        AsrWord {
            text: "that".into(),
            start_ms: 33610,
            end_ms: 33900,
        },
        AsrWord {
            text: "saved".into(),
            start_ms: 33900,
            end_ms: 34200,
        },
        AsrWord {
            text: "a".into(),
            start_ms: 34200,
            end_ms: 34400,
        },
        AsrWord {
            text: "wretch".into(),
            start_ms: 34400,
            end_ms: 34880,
        },
        AsrWord {
            text: "like".into(),
            start_ms: 35020,
            end_ms: 35400,
        },
        AsrWord {
            text: "me".into(),
            start_ms: 35400,
            end_ms: 35980,
        },
    ];
    let backend = FakeReferenceStageBackend {
        mtl: std::sync::Mutex::new(Some(Ok(mtl_out))),
        asr: std::sync::Mutex::new(None), // the stage never transcribes (#144)
    };

    let result = worker
        .run_mtl_reference_stage(
            "yt_fail",
            Some(&cand),
            Some(Path::new("/x.wav")),
            &words,
            &backend,
        )
        .await;
    assert!(
        result.unwrap().is_none(),
        "gate FAIL must return None so the caller falls through unchanged"
    );

    let reference: i64 = sqlx::query_scalar("SELECT lyrics_reference FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        reference, 1,
        "#144 F1: the gate leaves ★ alone — the persisted base-tier track clears it"
    );

    let audit_path = cache_dir.join("yt_fail_alignment_audit.json");
    let content = tokio::fs::read_to_string(&audit_path)
        .await
        .expect("_alignment_audit.json sidecar must be written on gate FAIL");
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(parsed["verdict"], "fail");

    let _ = std::fs::remove_dir_all(&cache_dir);
}

/// #144: a gate PASS writes the audit sidecar too, carrying the sung
/// coverage, so every ★ row has its gate numbers on disk (and an older FAIL
/// audit of the same song is replaced, not left behind).
#[tokio::test]
async fn run_mtl_reference_stage_pass_writes_the_audit_with_the_sung_coverage() {
    use crate::lyrics::g35t_client::AsrWord;
    use crate::lyrics::mtl_aligner::{MtlLine, MtlOutput};

    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name) VALUES (1, 'p', 'u', 'n')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, song, artist, normalized) \
         VALUES (1, 'yt_pass_audit', 'T', 'S', 'A', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let cache_dir = std::env::temp_dir().join("sp_reference_stage_pass_audit_test");
    let _ = std::fs::remove_dir_all(&cache_dir);
    std::fs::create_dir_all(&cache_dir).unwrap();
    // A stale FAIL audit from an earlier run of the same song.
    std::fs::write(
        cache_dir.join("yt_pass_audit_alignment_audit.json"),
        br#"{"verdict":"fail","reason":"coverage"}"#,
    )
    .unwrap();
    let tools = available_mtl_tools_dir();
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    let worker = crate::lyrics::worker::LyricsWorker::new_for_test_with_tools_dir(
        pool,
        cache_dir.clone(),
        tools.path().to_path_buf(),
        events_tx,
    );

    let texts = ["amazing grace", "how sweet", "the sound", "that saved"];
    let cand = crate::lyrics::tier1::CandidateText {
        source: "lrclib".to_string(),
        lines: texts.iter().map(|t| t.to_string()).collect(),
        line_timings: None,
        has_timing: false,
    };
    let mtl_out = MtlOutput {
        lines: texts
            .iter()
            .enumerate()
            .map(|(i, t)| MtlLine {
                text: t.to_string(),
                start_ms: Some(1_000 + i as u64 * 2_000),
                end_ms: Some(2_500 + i as u64 * 2_000),
            })
            .collect(),
        device: "cuda".into(),
        elapsed_s: 7.5,
    };
    // Every text word sung at its line's start, plus one sung word the text
    // lacks ("oh", 400 ms): 8 of 9 sung words covered.
    let mut words = Vec::new();
    for (i, t) in texts.iter().enumerate() {
        let start = 1_000 + i as u64 * 2_000;
        for (k, w) in t.split_whitespace().enumerate() {
            let s = start + k as u64 * 500;
            words.push(AsrWord {
                text: w.to_string(),
                start_ms: s,
                end_ms: s + 500,
            });
        }
    }
    words.push(AsrWord {
        text: "oh".into(),
        start_ms: 9_000,
        end_ms: 9_400,
    });
    let backend = FakeReferenceStageBackend {
        mtl: std::sync::Mutex::new(Some(Ok(mtl_out))),
        asr: std::sync::Mutex::new(None), // the stage never transcribes (#144)
    };

    let result = worker
        .run_mtl_reference_stage(
            "yt_pass_audit",
            Some(&cand),
            Some(Path::new("/x.wav")),
            &words,
            &backend,
        )
        .await;
    assert!(result.unwrap().is_some(), "expected a gate PASS");

    let content = tokio::fs::read_to_string(cache_dir.join("yt_pass_audit_alignment_audit.json"))
        .await
        .expect("a PASS must write the audit sidecar");
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(parsed["verdict"], "pass");
    assert_eq!(parsed["reason"], serde_json::Value::Null);
    assert_eq!(parsed["sung_words"], 9);
    assert_eq!(parsed["sung_covered_frac"], 8.0 / 9.0);
    assert_eq!(parsed["max_uncovered_sung_ms"], 400);
    assert_eq!(parsed["mtl_device"], "cuda");
    assert_eq!(parsed["mtl_elapsed_s"], 7.5);
    assert_eq!(parsed["asr_words"], 9);

    let _ = std::fs::remove_dir_all(&cache_dir);
}

/// #144 F3: a transcript of `text`'s words, one every 300 ms from `start_ms`.
fn sung_from(text: &str, start_ms: u64) -> Vec<crate::lyrics::g35t_client::AsrWord> {
    text.split_whitespace()
        .zip(0u64..)
        .map(|(word, i)| crate::lyrics::g35t_client::AsrWord {
            text: word.into(),
            start_ms: start_ms + i * 300,
            end_ms: start_ms + i * 300 + 280,
        })
        .collect()
}

/// #144 F3: the four lines of `sung_from`'s 11-word hymn.
fn hymn_candidate() -> crate::lyrics::tier1::CandidateText {
    crate::lyrics::tier1::CandidateText {
        source: "description".to_string(),
        lines: vec![
            "amazing grace".into(),
            "how sweet the sound".into(),
            "that saved a wretch".into(),
            "like".into(),
        ],
        line_timings: None,
        has_timing: false,
    }
}

const HYMN: &str = "amazing grace how sweet the sound that saved a wretch like";

/// #144 F3: a text that covers under `MIN_SUNG_COVERED_FRAC` of the sung
/// words fails the gate BEFORE mtl. The gate's Coverage verdict reads only
/// the text, and mtl returns every line with its text unchanged.
/// Such a text (written once, sung many times) is what upstream's DP loops
/// on: its backtrack raised `IndexError: index -462 is out of bounds` on 5
/// SNV songs (text 9–23 % of the sung words). No mtl, no heavy slot: the
/// gate's own FAIL audit, with no mtl device.
#[tokio::test]
async fn a_text_that_covers_too_little_of_the_singing_fails_the_gate_before_mtl() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let cache_dir = std::env::temp_dir().join("sp_reference_stage_uncovered_test");
    let _ = std::fs::remove_dir_all(&cache_dir);
    std::fs::create_dir_all(&cache_dir).unwrap();
    let tools = available_mtl_tools_dir();
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    let worker = crate::lyrics::worker::LyricsWorker::new_for_test_with_tools_dir(
        pool,
        cache_dir.clone(),
        tools.path().to_path_buf(),
        events_tx,
    );
    // The 11 words of the text, then 10 it does not carry: 11 / 21 < 0.55.
    let mut words = sung_from(HYMN, 1_000);
    words.extend(sung_from(&"hallelujah ".repeat(10), 5_000));

    let result = worker
        .run_mtl_reference_stage(
            "yt_uncovered",
            Some(&hymn_candidate()),
            Some(Path::new("/x.wav")),
            &words,
            &UnreachableBackend,
        )
        .await;
    assert!(result.unwrap().is_none(), "the base tier takes the song");

    let audit: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(cache_dir.join("yt_uncovered_alignment_audit.json"))
            .expect("the gate's FAIL writes the audit"),
    )
    .unwrap();
    assert_eq!(audit["verdict"], "fail");
    assert_eq!(audit["reason"], "coverage");
    assert_eq!(audit["sung_words"], 21);
    assert_eq!(audit["sung_covered_frac"].as_f64(), Some(11.0 / 21.0));
    assert_eq!(audit["sung_coverage_ok"], false);
    assert_eq!(audit["lines_total"], 4);
    assert_eq!(audit["lines_matched"], 4);
    assert_eq!(audit["lines_timed"], 0);
    assert_eq!(audit["asr_words"], 21);
    assert!(audit["mtl_device"].is_null(), "no mtl ran: {audit}");
    assert!(audit["mtl_elapsed_s"].is_null(), "no mtl ran: {audit}");
    assert_eq!(audit["before_mtl"], true, "{audit}");
    let _ = std::fs::remove_dir_all(&cache_dir);
}

/// #144 F3: at exactly `MIN_SUNG_COVERED_FRAC` the text may still pass, so
/// mtl runs (here it fails, and the audit is mtl's error).
#[tokio::test]
async fn a_text_that_covers_the_minimum_share_of_the_singing_still_reaches_mtl() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let cache_dir = std::env::temp_dir().join("sp_reference_stage_minimum_cover_test");
    let _ = std::fs::remove_dir_all(&cache_dir);
    std::fs::create_dir_all(&cache_dir).unwrap();
    let tools = available_mtl_tools_dir();
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    let worker = crate::lyrics::worker::LyricsWorker::new_for_test_with_tools_dir(
        pool,
        cache_dir.clone(),
        tools.path().to_path_buf(),
        events_tx,
    );
    // 11 / 20 = 0.55.
    let mut words = sung_from(HYMN, 1_000);
    words.extend(sung_from(&"hallelujah ".repeat(9), 5_000));
    let backend = FakeReferenceStageBackend {
        mtl: std::sync::Mutex::new(Some(Err(anyhow::anyhow!("mtl boom")))),
        asr: std::sync::Mutex::new(None),
    };

    let result = worker
        .run_mtl_reference_stage(
            "yt_minimum",
            Some(&hymn_candidate()),
            Some(Path::new("/x.wav")),
            &words,
            &backend,
        )
        .await;
    assert!(result.unwrap().is_none());
    assert!(
        backend.mtl.lock().unwrap().is_none(),
        "mtl was asked to align the text"
    );
    let audit: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(cache_dir.join("yt_minimum_alignment_audit.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(audit["verdict"], "error");
    assert_eq!(audit["before_mtl"], false, "{audit}");
    let _ = std::fs::remove_dir_all(&cache_dir);
}
