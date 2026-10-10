//! The stem worker's tests (moved out of worker.rs for the 1000-line cap, #229).

use super::*;

#[test]
fn worker_enabled_defaults_on_and_parses_off_values() {
    assert!(worker_enabled(None));
    assert!(worker_enabled(Some("true")));
    assert!(worker_enabled(Some("1")));
    assert!(!worker_enabled(Some("false")));
    assert!(!worker_enabled(Some("0")));
    assert!(!worker_enabled(Some(" OFF ")));
    assert!(!worker_enabled(Some("no")));
}

fn test_worker(pool: SqlitePool, tools_dir: PathBuf) -> StemWorker {
    StemWorker::new(
        pool,
        tools_dir,
        Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
        Arc::new(RwLock::new(crate::obs::ObsState::default())),
    )
}

/// #207: `stem_worker.py` imports `win_replace` (the POSIX rename that
/// publishes a stem SongPlayer holds open), so both ship in `tools_dir`.
#[tokio::test]
async fn ensure_script_materialises_stem_worker_py_and_win_replace_py() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let worker = test_worker(pool, dir.path().to_path_buf());

    let path = worker.ensure_script().await.unwrap();

    assert_eq!(path, dir.path().join("stem_worker.py"));
    let written = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(written, include_str!("../../../../scripts/stem_worker.py"));
    let helper = tokio::fs::read_to_string(dir.path().join("win_replace.py")).await;
    let helper = helper.expect("win_replace.py is not shipped next to stem_worker.py");
    assert_eq!(helper, include_str!("../../../../scripts/win_replace.py"));
}

#[tokio::test]
async fn missing_venv_python_warns_and_skips_without_touching_db() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    // No lyrics_venv under the tempdir → venv_python_path does not exist.
    let worker = test_worker(pool, dir.path().to_path_buf());

    worker.process_next().await;

    assert!(
        worker
            .warned_no_python
            .load(std::sync::atomic::Ordering::Relaxed),
        "tick should warn once when the venv python is missing"
    );
}

// ---- #161 mid-job wall-abort re-queue semantics -----------------------

/// Seed playlist 1 (FK target) + one pending stem row (normalized, has an
/// audio path, stem_status NULL). Mirrors `worker_tests_idle_gate.rs`.
async fn seed_pending_stem_row(pool: &SqlitePool, video_id: i64) {
    sqlx::query(
        "INSERT OR IGNORE INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'stem_pl', 'u', 'SP-fast', 1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, audio_file_path) \
         VALUES (?, 1, 'yt_stem', 1, '/tmp/x_audio.flac')",
    )
    .bind(video_id)
    .execute(pool)
    .await
    .unwrap();
}

fn stem_job(video_id: i64) -> crate::db::models_stems::StemJob {
    crate::db::models_stems::StemJob {
        video_id,
        youtube_id: "yt_stem".into(),
        audio_file_path: "/tmp/x_audio.flac".into(),
        duration_ms: Some(180_000),
        song: Some("s".into()),
        artist: Some("a".into()),
    }
}

/// A wall abort re-queues with NO penalty: partial stems deleted, DB row
/// left pending (stem_status NULL, stem_attempts unchanged), and the
/// selector re-picks it immediately.
#[tokio::test]
async fn wall_abort_re_queues_with_no_penalty() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    seed_pending_stem_row(&pool, 1).await;
    let dir = tempfile::tempdir().unwrap();
    // Partial stem outputs the abort must delete.
    let vocals = dir.path().join("v.flac");
    let instr = dir.path().join("i.flac");
    std::fs::write(&vocals, b"partial").unwrap();
    std::fs::write(&instr, b"partial").unwrap();
    let worker = test_worker(pool.clone(), dir.path().to_path_buf());

    worker
        .record_stem_result(
            &stem_job(1),
            &vocals,
            &instr,
            StemStepResult::WallAborted("output playing".into()),
        )
        .await;

    let (status, attempts): (Option<String>, i64) =
        sqlx::query_as("SELECT stem_status, stem_attempts FROM videos WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(status.is_none(), "wall-abort must leave stem_status NULL");
    assert_eq!(attempts, 0, "wall-abort must not increment stem_attempts");
    assert!(!vocals.exists(), "partial vocals stem must be deleted");
    assert!(!instr.exists(), "partial instrumental stem must be deleted");

    let next = crate::db::models_stems::get_next_video_for_stems(&pool)
        .await
        .unwrap();
    assert_eq!(
        next.map(|j| j.video_id),
        Some(1),
        "the aborted row must be re-picked immediately"
    );
}

/// #184 G0.1: a dub-yield leaves the RESUME STATE intact — partial stems +
/// the #171 work dir are KEPT, and the DB row stays pending (stem_status
/// NULL, stem_attempts unchanged, no stem_next_attempt_at) so the selector
/// re-picks it. Distinct from WallAborted, which DELETES the partial stems.
#[tokio::test]
async fn yield_to_dub_keeps_resume_state_and_leaves_row_pending() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    seed_pending_stem_row(&pool, 1).await;
    let dir = tempfile::tempdir().unwrap();
    // Partial stem outputs + a resumable work dir the yield must PRESERVE.
    let vocals = dir.path().join("v.flac");
    let instr = dir.path().join("i.flac");
    std::fs::write(&vocals, b"partial").unwrap();
    std::fs::write(&instr, b"partial").unwrap();
    let work_dir = dir.path().join("yt_stem_stemsep");
    std::fs::create_dir_all(&work_dir).unwrap();
    std::fs::write(work_dir.join("seg_000.wav"), b"seg").unwrap();
    let worker = test_worker(pool.clone(), dir.path().to_path_buf());

    worker
        .record_stem_result(
            &stem_job(1),
            &vocals,
            &instr,
            StemStepResult::YieldedToDub("dub job waiting for the heavy slot".into()),
        )
        .await;

    let (status, attempts, next): (Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT stem_status, stem_attempts, stem_next_attempt_at FROM videos WHERE id = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(status.is_none(), "a dub-yield must leave stem_status NULL");
    assert_eq!(attempts, 0, "a dub-yield must not increment stem_attempts");
    assert!(
        next.is_none(),
        "a dub-yield must set no stem_next_attempt_at (no backoff)"
    );
    assert!(
        vocals.exists(),
        "the partial vocals stem must be KEPT — the #171 resume state"
    );
    assert!(instr.exists(), "the partial instrumental stem must be KEPT");
    assert!(
        work_dir.join("seg_000.wav").exists(),
        "the resumable work dir must be KEPT"
    );

    let next_job = crate::db::models_stems::get_next_video_for_stems(&pool)
        .await
        .unwrap();
    assert_eq!(
        next_job.map(|j| j.video_id),
        Some(1),
        "the yielded row must be re-picked (still pending)"
    );
}

/// A GENUINE separation failure still records the backoff deferral —
/// distinct from a wall abort.
#[tokio::test]
async fn genuine_failure_still_records_the_deferral() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    seed_pending_stem_row(&pool, 1).await;
    let dir = tempfile::tempdir().unwrap();
    let worker = test_worker(pool.clone(), dir.path().to_path_buf());

    worker
        .record_stem_result(
            &stem_job(1),
            &dir.path().join("v.flac"),
            &dir.path().join("i.flac"),
            StemStepResult::Failed(anyhow::anyhow!("separator boom")),
        )
        .await;

    let (status, attempts): (Option<String>, i64) =
        sqlx::query_as("SELECT stem_status, stem_attempts FROM videos WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        status.as_deref(),
        Some("failed"),
        "a real failure records 'failed'"
    );
    assert_eq!(
        attempts, 1,
        "a real failure increments stem_attempts (backoff)"
    );
}

/// #136 review round 1: the metadata repair renamed the song WHILE its
/// separation ran. The stems were written under the name the job started
/// with; they must end up where the song's audio now derives them, recorded
/// there, never left for a lyrics wait that cannot end.
#[tokio::test]
async fn stems_of_a_song_renamed_during_the_separation_land_under_its_new_name() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let old_audio = dir
        .path()
        .join("Old_A_IYAOosrh7HY_normalized_gf_audio.flac");
    let new_audio = dir.path().join("Song_A_IYAOosrh7HY_normalized_audio.flac");
    std::fs::write(&new_audio, b"a").unwrap();
    seed_pending_stem_row(&pool, 1).await;
    sqlx::query("UPDATE videos SET youtube_id = 'IYAOosrh7HY', audio_file_path = ?")
        .bind(new_audio.to_string_lossy().as_ref())
        .execute(&pool)
        .await
        .unwrap();
    let (old_vocals, old_instrumental) = crate::stems::stem_paths(&old_audio);
    std::fs::write(&old_vocals, b"v").unwrap();
    std::fs::write(&old_instrumental, b"i").unwrap();
    let job = crate::db::models_stems::StemJob {
        youtube_id: "IYAOosrh7HY".into(),
        audio_file_path: old_audio.to_string_lossy().into_owned(),
        ..stem_job(1)
    };
    let worker = test_worker(pool.clone(), dir.path().to_path_buf());

    worker
        .record_stem_result(&job, &old_vocals, &old_instrumental, StemStepResult::Done)
        .await;

    let (vocals, instrumental) = crate::stems::stem_paths(&new_audio);
    assert_eq!(std::fs::read(&vocals).unwrap(), b"v");
    assert_eq!(std::fs::read(&instrumental).unwrap(), b"i");
    assert!(!old_vocals.exists() && !old_instrumental.exists());
    let (status, recorded): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT stem_status, vocals_file_path FROM videos WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status.as_deref(), Some("done"));
    assert_eq!(recorded, Some(vocals.to_string_lossy().into_owned()));
}

// ---- duration terminal-skip (2026-09-15) ------------------------------

/// A worker whose venv python "exists" (an empty stub file at the
/// platform-correct path), so `process_next` clears the venv gate and
/// reaches the terminal duration-skip check.
fn worker_with_stub_venv(pool: SqlitePool, dir: &std::path::Path) -> StemWorker {
    let python = crate::lyrics::bootstrap::venv_python_path(dir);
    if let Some(parent) = python.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&python, b"").unwrap();
    test_worker(pool, dir.to_path_buf())
}

/// A 121-minute row (over the 120-min `STEM_MAX_DURATION_MS` cap) must be
/// marked terminal `unsupported`, with no backoff bookkeeping touched —
/// mirrors the exact-boundary coverage in `stem_duration_supported`'s pure
/// tests (`worker_plan_tests.rs`), but proves the worker's real spawn seam
/// (`stem_duration_too_long`) actually gates on it.
#[tokio::test]
async fn process_next_marks_overlong_song_unsupported() {
    // This test reaches the #184 G0.1 dub tick-defer (stub venv passes the
    // venv gate), so serialize + clear the process-global dub flag so a
    // parallel flag test can't make it defer instead of marking the row.
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    seed_pending_stem_row(&pool, 1).await;
    sqlx::query("UPDATE videos SET duration_ms = ? WHERE id = 1")
        .bind(7_260_000i64) // 121 min
        .execute(&pool)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let worker = worker_with_stub_venv(pool.clone(), dir.path());

    worker.process_next().await;

    let (status, attempts): (Option<String>, i64) =
        sqlx::query_as("SELECT stem_status, stem_attempts FROM videos WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        status.as_deref(),
        Some("unsupported"),
        "a 121-min song must be marked terminal unsupported"
    );
    assert_eq!(
        attempts, 0,
        "the duration skip must not touch stem_attempts — it is not a backoff"
    );
}

/// #230: while the background is held the stem tick starts no job — the
/// overlong row is not even looked at; after the release the tick marks it.
#[tokio::test]
async fn a_held_background_starts_no_stem_job_until_released() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    seed_pending_stem_row(&pool, 1).await;
    sqlx::query("UPDATE videos SET duration_ms = ? WHERE id = 1")
        .bind(7_260_000i64) // 121 min
        .execute(&pool)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let worker = worker_with_stub_venv(pool.clone(), dir.path());
    let before = stem_status(&pool).await;
    crate::background_hold::hold_for_a_minute(&pool).await;
    worker.process_next().await;
    assert_eq!(
        stem_status(&pool).await,
        before,
        "held: the row is untouched"
    );
    crate::background_hold::end_hold(&pool).await;
    worker.process_next().await;
    assert_eq!(stem_status(&pool).await.as_deref(), Some("unsupported"));
}

async fn stem_status(pool: &SqlitePool) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT stem_status FROM videos WHERE id = 1")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A normal-length (4-min) row is never touched by the terminal-skip path.
/// No stub venv is provided here — `process_next` stops at the missing
/// venv-python gate before reaching the duration check at all (same as
/// `missing_venv_python_warns_and_skips_without_touching_db`); that is
/// fine, since the exact 120-min boundary is already proven by the pure
/// `stem_duration_supported` tests. This just proves a normal row is left
/// pending, never spuriously marked unsupported.
#[tokio::test]
async fn process_next_leaves_a_normal_length_song_pending() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    seed_pending_stem_row(&pool, 1).await;
    sqlx::query("UPDATE videos SET duration_ms = ? WHERE id = 1")
        .bind(240_000i64) // 4 min
        .execute(&pool)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let worker = test_worker(pool.clone(), dir.path().to_path_buf());

    worker.process_next().await;

    let status: Option<String> = sqlx::query_scalar("SELECT stem_status FROM videos WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(status.is_none(), "a 4-minute song must stay pending");
}
