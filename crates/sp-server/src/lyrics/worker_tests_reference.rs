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
        }
    }
}

// -----------------------------------------------------------------------
// Lever 2 (#143) — `alignment_model_for_source` (pure) tests
// -----------------------------------------------------------------------

#[test]
fn alignment_model_for_source_prioritizes_mtl_over_whisperx() {
    assert_eq!(
        alignment_model_for_source("description+mtl@rev1/g35t-ok"),
        Some(crate::lyrics::ALIGNMENT_MODEL_MTL_REV1)
    );
}

#[test]
fn alignment_model_for_source_whisperx() {
    assert_eq!(
        alignment_model_for_source("description+whisperx-large-v3@rev1"),
        Some(crate::lyrics::ALIGNMENT_MODEL_WHISPERX_V3_REV1)
    );
}

#[test]
fn alignment_model_for_source_timed_merge() {
    assert_eq!(
        alignment_model_for_source("lrclib+timed-merge"),
        Some(crate::lyrics::ALIGNMENT_MODEL_TIMED_MERGE)
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

fn ref_candidate(source: &str, n_lines: usize) -> crate::lyrics::tier1::CandidateText {
    crate::lyrics::tier1::CandidateText {
        source: source.to_string(),
        lines: (0..n_lines).map(|i| format!("line {i}")).collect(),
        line_timings: None,
        has_timing: false,
    }
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
            1,
            "yt1",
            Some(&cand),
            Some(Path::new("/x.wav")),
            &UnreachableBackend,
        )
        .await;
    assert!(result.is_none());
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
        .run_mtl_reference_stage(1, "yt1", Some(&cand), None, &UnreachableBackend)
        .await;
    assert!(result.is_none());
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
            1,
            "yt1",
            None,
            Some(Path::new("/x.wav")),
            &UnreachableBackend,
        )
        .await;
    assert!(result.is_none());
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
            1,
            "yt1",
            Some(&cand),
            Some(Path::new("/x.wav")),
            &UnreachableBackend,
        )
        .await;
    assert!(result.is_none());
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn run_mtl_reference_stage_pass_stamps_source_and_sets_reference_flag() {
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
        lines: vec!["amazing grace".into(), "how sweet the sound".into()],
        line_timings: None,
        has_timing: false,
    };
    let mtl_out = MtlOutput {
        lines: vec![
            MtlLine {
                text: "amazing grace".into(),
                start_ms: 1000,
                end_ms: 2000,
            },
            MtlLine {
                text: "how sweet the sound".into(),
                start_ms: 2100,
                end_ms: 3500,
            },
        ],
        device: "cuda".into(),
        elapsed_s: 42.0,
    };
    // Independent ASR agrees closely — a clean gate PASS (see the identical
    // fixture rationale in orchestrator_tests.rs::run_reference_stage_pass_*).
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
    ];
    let backend = FakeReferenceStageBackend {
        mtl: std::sync::Mutex::new(Some(Ok(mtl_out))),
        asr: std::sync::Mutex::new(Some(Ok(words))),
    };

    let result = worker
        .run_mtl_reference_stage(
            video_id,
            "yt_pass",
            Some(&cand),
            Some(Path::new("/x.wav")),
            &backend,
        )
        .await;

    let track = result.expect("expected Some(track) on gate PASS");
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
    assert_eq!(reference, 1, "lyrics_reference must be set on gate PASS");

    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn run_mtl_reference_stage_fail_clears_reference_flag_and_writes_audit() {
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
    // Starts with lyrics_reference = 1 to prove a Fail actively CLEARS it,
    // not merely leaves the default at 0.
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
        lines: vec!["amazing grace".into(), "how sweet the sound".into()],
        line_timings: None,
        has_timing: false,
    };
    let mtl_out = MtlOutput {
        lines: vec![
            MtlLine {
                text: "amazing grace".into(),
                start_ms: 1000,
                end_ms: 2000,
            },
            MtlLine {
                text: "how sweet the sound".into(),
                start_ms: 2100,
                end_ms: 3500,
            },
        ],
        device: "cpu".into(),
        elapsed_s: 12.5,
    };
    // 30s whole-song offset — must fail the gate regardless of matching
    // algorithm specifics (the design's whole-song sanity check, #130
    // 2026-09-12 design comment).
    let words = vec![
        AsrWord {
            text: "amazing".into(),
            start_ms: 31000,
            end_ms: 31500,
        },
        AsrWord {
            text: "grace".into(),
            start_ms: 31500,
            end_ms: 32000,
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
    ];
    let backend = FakeReferenceStageBackend {
        mtl: std::sync::Mutex::new(Some(Ok(mtl_out))),
        asr: std::sync::Mutex::new(Some(Ok(words))),
    };

    let result = worker
        .run_mtl_reference_stage(
            video_id,
            "yt_fail",
            Some(&cand),
            Some(Path::new("/x.wav")),
            &backend,
        )
        .await;
    assert!(
        result.is_none(),
        "gate FAIL must return None so the caller falls through unchanged"
    );

    let reference: i64 = sqlx::query_scalar("SELECT lyrics_reference FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        reference, 0,
        "lyrics_reference must be cleared on gate FAIL"
    );

    let audit_path = cache_dir.join("yt_fail_alignment_audit.json");
    let content = tokio::fs::read_to_string(&audit_path)
        .await
        .expect("_alignment_audit.json sidecar must be written on gate FAIL");
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(parsed["verdict"], "fail");

    let _ = std::fs::remove_dir_all(&cache_dir);
}
