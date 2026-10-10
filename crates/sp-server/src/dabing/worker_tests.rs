//! The dub worker's tests (moved out of `worker.rs`, #229 review round 13:
//! the 1000-line cap).

use std::sync::atomic::Ordering;

use super::*;
use crate::test_log::{Captured, capturing};

#[test]
fn worker_enabled_defaults_on_and_parses_off_values() {
    assert!(worker_enabled(None));
    assert!(worker_enabled(Some("true")));
    assert!(!worker_enabled(Some("false")));
    assert!(!worker_enabled(Some("0")));
    assert!(!worker_enabled(Some(" OFF ")));
    assert!(!worker_enabled(Some("no")));
}

#[test]
fn dub_voice_defaults_to_the_speaker_and_passes_through() {
    // Absent / blank / whitespace-only → the speaker's own voice.
    assert_eq!(dub_voice_from(None), "speaker");
    assert_eq!(dub_voice_from(Some("")), "speaker");
    assert_eq!(dub_voice_from(Some("   ")), "speaker");
    assert_eq!(dub_voice_from(Some("speaker")), "speaker");
    // Any non-blank prebuilt name passes through, trimmed.
    assert_eq!(dub_voice_from(Some("Kore")), "Kore");
    assert_eq!(dub_voice_from(Some("  Charon  ")), "Charon");
}

#[test]
fn dub_voice_speaker_is_recognised_in_any_case() {
    // The child treats `Speaker` as the speaker's own voice; the stored value
    // (and so the row's `hlas: rečník` label) must agree (review round 1).
    assert_eq!(dub_voice_from(Some("Speaker")), "speaker");
    assert_eq!(dub_voice_from(Some(" SPEAKER ")), "speaker");
}

#[test]
fn dub_model_defaults_to_the_live_translate_preview_and_passes_through() {
    assert_eq!(dub_model_from(None), "gemini-3.5-live-translate-preview");
    assert_eq!(
        dub_model_from(Some("")),
        "gemini-3.5-live-translate-preview"
    );
    assert_eq!(
        dub_model_from(Some("  ")),
        "gemini-3.5-live-translate-preview"
    );
    assert_eq!(
        dub_model_from(Some(" gemini-4-live-translate ")),
        "gemini-4-live-translate"
    );
}

#[test]
fn dub_input_is_the_vocals_stem_only_when_done_and_present() {
    let orig = "/c/a_audio.flac";
    let voc = Some("/c/a_audio_vocals.flac");
    // Stems done + path + file present → the vocals stem.
    assert_eq!(
        dub_input_audio(orig, voc, Some("done"), true),
        PathBuf::from("/c/a_audio_vocals.flac")
    );
    // The file is missing on disk → the original.
    assert_eq!(
        dub_input_audio(orig, voc, Some("done"), false),
        PathBuf::from(orig)
    );
    // Stems not done (pending / failed / unsupported) → the original.
    for status in [None, Some("failed"), Some("unsupported"), Some("ready")] {
        assert_eq!(
            dub_input_audio(orig, voc, status, true),
            PathBuf::from(orig)
        );
    }
    // No / empty vocals path → the original.
    assert_eq!(
        dub_input_audio(orig, None, Some("done"), true),
        PathBuf::from(orig)
    );
    assert_eq!(
        dub_input_audio(orig, Some(""), Some("done"), true),
        PathBuf::from(orig)
    );
}

#[test]
fn dub_eta_is_audio_plus_drain() {
    assert_eq!(dub_eta(60_000), Duration::from_secs(180));
}

#[test]
fn embedded_tool_scripts_ship_worker_and_helpers() {
    let scripts = embedded_tool_scripts();
    let names: Vec<&str> = scripts.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        names,
        vec![
            "dub_worker.py",
            "dub_live_session.py",
            "dub_loudness.py",
            "win_replace.py"
        ]
    );
    for (name, content) in scripts {
        assert!(!content.is_empty(), "{name} embedded empty");
    }
    let worker = scripts[0].1;
    // #184 round H step 2: the ONE continuous session ships next to the worker
    // that imports it at module load (a missing module = every dub fails).
    assert!(
        scripts[1].1.contains("class ContinuousSession"),
        "dub_live_session.py (round-H continuous session) is not shipped"
    );
    assert!(
        worker.contains("import dub_live_session"),
        "dub_worker.py does not import the shipped dub_live_session module"
    );
    // The superseded per-chunk machinery is gone from the shipped worker.
    for gone in ["chunk_reusable", "build_mix_filter", "chunk_voice_drift"] {
        assert!(!worker.contains(gone), "dub_worker.py still has {gone}");
    }
    // #184 round F: the loudness-matched assembly imports `dub_loudness`.
    assert!(
        scripts[2].1.contains("def build_loudnorm_second_pass"),
        "dub_loudness.py (round-F loudness rules) is not shipped"
    );
    assert!(
        worker.contains("import dub_loudness"),
        "dub_worker.py does not import the shipped dub_loudness module"
    );
    // #184 round F2: the dub is promoted with a POSIX-semantics rename.
    assert!(
        scripts[3].1.contains("FILE_RENAME_FLAG_POSIX_SEMANTICS"),
        "win_replace.py (round-F2 POSIX rename) is not shipped"
    );
    assert!(
        worker.contains("import win_replace"),
        "dub_worker.py does not import the shipped win_replace module"
    );
}

/// #136: the dub worker takes the FIRST key of the `gemini_api_key` list
/// (split by the shared `gemini_api::gemini_keys_from_setting`).
#[tokio::test]
async fn the_dub_worker_uses_the_first_key_of_the_gemini_key_list() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let worker = DubWorker::new(
        pool.clone(),
        PathBuf::from("."),
        Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
        Arc::new(RwLock::new(crate::obs::ObsState::default())),
    );

    assert_eq!(worker.first_gemini_key().await, None, "setting unset");
    crate::db::models::set_setting(&pool, "gemini_api_key", " , ")
        .await
        .unwrap();
    assert_eq!(worker.first_gemini_key().await, None, "no key in the list");
    crate::db::models::set_setting(&pool, "gemini_api_key", " k1 , k2")
        .await
        .unwrap();
    assert_eq!(worker.first_gemini_key().await.as_deref(), Some("k1"));
}

/// #230: while the background is held the dub tick starts nothing — not
/// even its one-shot subtitle backfill; after the release it runs.
#[tokio::test]
async fn a_held_background_starts_no_dub_work_until_released() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let worker = DubWorker::new(
        pool.clone(),
        PathBuf::from("."),
        Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
        Arc::new(RwLock::new(crate::obs::ObsState::default())),
    );
    crate::background_hold::hold_for_a_minute(&pool).await;
    worker.process_next().await;
    assert!(!worker.subtitles_backfilled.load(Ordering::Relaxed), "held");
    crate::background_hold::end_hold(&pool).await;
    worker.process_next().await;
    assert!(
        worker.subtitles_backfilled.load(Ordering::Relaxed),
        "released"
    );
}

/// #229 item C (review round 12): with paid AI off only a dub job that
/// would run now is held (its video named), none when there is no job,
/// so the status names no dub that is not waiting.
#[tokio::test]
async fn only_a_dub_job_that_would_run_is_held() {
    let cap = Captured::default();
    let _log = tracing::subscriber::set_default(capturing(&cap));
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    crate::db::models::set_setting(&pool, "paid_ai_enabled", "false")
        .await
        .unwrap();
    let worker = DubWorker::new(
        pool.clone(),
        PathBuf::from("."),
        Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
        Arc::new(RwLock::new(crate::obs::ObsState::default())),
    );
    let holds = || cap.lines_with("paid AI is off");
    assert!(!worker.may_dub().await);
    assert_eq!(holds(), Vec::<String>::new(), "no job, nothing held");
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, audio_file_path, \
                             dub_requested, dub_status) \
         VALUES (1, 'dubjob00001', 1, '/x_audio.flac', 1, 'queued')",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(!worker.may_dub().await);
    let held = holds();
    assert!(
        held.iter()
            .any(|l| l.contains("job=\"dub\"") && l.contains("dubjob00001")),
        "{held:?}"
    );
}

/// #229 item C: while this node's paid AI is off no dub runs and no key
/// reaches a Live-Translate child.
#[tokio::test]
async fn no_dub_and_no_key_while_paid_ai_is_off() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let worker = DubWorker::new(
        pool.clone(),
        PathBuf::from("."),
        Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
        Arc::new(RwLock::new(crate::obs::ObsState::default())),
    );
    crate::db::models::set_setting(&pool, "gemini_api_key", "k1")
        .await
        .unwrap();
    assert!(worker.may_dub().await, "on by default");
    crate::db::models::set_setting(&pool, "paid_ai_enabled", "false")
        .await
        .unwrap();
    assert!(!worker.may_dub().await);
    assert_eq!(worker.first_gemini_key().await, None);
}

/// #136 review round 2: the metadata repair renamed the song WHILE its dub
/// was synthesized. The dub + transcripts were written under the name the
/// job started with; recording the finished dub brings them under the name
/// the song's audio now derives, and records the dub there.
#[tokio::test]
async fn a_dub_of_a_song_renamed_during_the_synthesis_lands_under_its_new_name() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let old_audio = dir
        .path()
        .join("Old_A_IYAOosrh7HY_normalized_gf_audio.flac");
    let new_audio = dir.path().join("Song_A_IYAOosrh7HY_normalized_audio.flac");
    std::fs::write(&new_audio, b"a").unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, audio_file_path, \
                             dub_status) \
         VALUES (1, 1, 'IYAOosrh7HY', 1, ?, 'synth')",
    )
    .bind(new_audio.to_string_lossy().as_ref())
    .execute(&pool)
    .await
    .unwrap();
    let old_dub = crate::stems::dub_path(&old_audio);
    std::fs::write(&old_dub, b"d").unwrap();
    std::fs::write(crate::stems::dub_transcripts_path(&old_audio), b"t").unwrap();
    let job = models_dabing::DubJob {
        video_id: 1,
        youtube_id: "IYAOosrh7HY".into(),
        audio_file_path: old_audio.to_string_lossy().into_owned(),
        duration_ms: Some(180_000),
        dub_status: "synth".into(),
        vocals_file_path: None,
        instrumental_file_path: None,
        stem_status: None,
        dub_attempts: 0,
    };
    let worker = DubWorker::new(
        pool.clone(),
        dir.path().to_path_buf(),
        Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
        Arc::new(RwLock::new(crate::obs::ObsState::default())),
    );

    worker.record_dub_ready(&job, &old_dub).await;

    let dub = crate::stems::dub_path(&new_audio);
    assert_eq!(std::fs::read(&dub).unwrap(), b"d");
    assert_eq!(
        std::fs::read(crate::stems::dub_transcripts_path(&new_audio)).unwrap(),
        b"t"
    );
    assert!(!old_dub.exists());
    let (status, recorded): (String, Option<String>) =
        sqlx::query_as("SELECT dub_status, dub_file_path FROM videos WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "ready");
    assert_eq!(recorded, Some(dub.to_string_lossy().into_owned()));
}

/// #136 review round 4: a song with a dub is dubbed AGAIN, and the metadata
/// repair renames it meanwhile. The rename carries the OLD dub to the new
/// name, then the job writes the NEW dub under the name it started with.
/// The job's own output is the fresh copy: it must end up under the new
/// name, replacing the old dub, never stranded under the old name.
#[tokio::test]
async fn a_re_dub_finished_after_a_rename_replaces_the_old_dub_under_the_new_name() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let old_audio = dir
        .path()
        .join("Old_A_IYAOosrh7HY_normalized_gf_audio.flac");
    let new_audio = dir.path().join("Song_A_IYAOosrh7HY_normalized_audio.flac");
    std::fs::write(&new_audio, b"a").unwrap();
    let (dub, transcripts) = (
        crate::stems::dub_path(&new_audio),
        crate::stems::dub_transcripts_path(&new_audio),
    );
    std::fs::write(&dub, b"old dub").unwrap();
    std::fs::write(&transcripts, b"old t").unwrap();
    let fresh = crate::stems::dub_path(&old_audio);
    std::fs::write(&fresh, b"new dub").unwrap();
    std::fs::write(crate::stems::dub_transcripts_path(&old_audio), b"new t").unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, audio_file_path, \
                             dub_status, dub_file_path) \
         VALUES (1, 1, 'IYAOosrh7HY', 1, ?, 'synth', ?)",
    )
    .bind(new_audio.to_string_lossy().as_ref())
    .bind(dub.to_string_lossy().as_ref())
    .execute(&pool)
    .await
    .unwrap();
    let job = models_dabing::DubJob {
        video_id: 1,
        youtube_id: "IYAOosrh7HY".into(),
        audio_file_path: old_audio.to_string_lossy().into_owned(),
        duration_ms: Some(180_000),
        dub_status: "synth".into(),
        vocals_file_path: None,
        instrumental_file_path: None,
        stem_status: None,
        dub_attempts: 0,
    };
    let worker = DubWorker::new(
        pool.clone(),
        dir.path().to_path_buf(),
        Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
        Arc::new(RwLock::new(crate::obs::ObsState::default())),
    );

    worker.record_dub_ready(&job, &fresh).await;

    assert_eq!(std::fs::read(&dub).unwrap(), b"new dub");
    assert_eq!(std::fs::read(&transcripts).unwrap(), b"new t");
    assert!(!fresh.exists(), "nothing stranded under the old name");
    let recorded: Option<String> =
        sqlx::query_scalar("SELECT dub_file_path FROM videos WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(recorded, Some(dub.to_string_lossy().into_owned()));
}

/// A dub worker over a fresh database with one dub job that would run now
/// (`dubjob00002`), and the job.
async fn worker_with_a_dub_job() -> (DubWorker, SqlitePool, models_dabing::DubJob) {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, audio_file_path, \
                             dub_requested, dub_status) \
         VALUES (1, 'dubjob00002', 1, '/x_audio.flac', 1, 'queued')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let job = models_dabing::get_next_dub_job(&pool)
        .await
        .unwrap()
        .unwrap();
    let worker = DubWorker::new(
        pool.clone(),
        PathBuf::from("."),
        Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
        Arc::new(RwLock::new(crate::obs::ObsState::default())),
    );
    (worker, pool, job)
}

/// `(dub_status, dub_attempts)` of the job's row.
async fn dub_state(pool: &SqlitePool, job: &models_dabing::DubJob) -> (String, i64) {
    sqlx::query_as("SELECT dub_status, dub_attempts FROM videos WHERE id = ?")
        .bind(job.video_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// #229 item C (review round 13): paid AI switched off after the tick's
/// `may_dub` but before the job's key is read: the job is held where it
/// is, never failed (no attempt, no failure mark).
#[tokio::test]
async fn a_dub_switched_off_after_its_pick_is_held_never_failed() {
    let (worker, pool, job) = worker_with_a_dub_job().await;
    crate::db::models::set_setting(&pool, "gemini_api_key", "k1")
        .await
        .unwrap();
    crate::db::models::set_setting(&pool, "paid_ai_enabled", "false")
        .await
        .unwrap();
    assert_eq!(worker.job_key(&job).await, None);
    assert_eq!(dub_state(&pool, &job).await, ("queued".to_string(), 0));
}

/// With paid AI on and no key set, the job is deferred as before: an
/// attempt and a failure mark naming the setting.
#[tokio::test]
async fn a_dub_with_no_key_is_deferred() {
    let (worker, pool, job) = worker_with_a_dub_job().await;
    assert_eq!(worker.job_key(&job).await, None);
    assert_eq!(dub_state(&pool, &job).await, ("failed".to_string(), 1));
    crate::db::models::set_setting(&pool, "gemini_api_key", "k1")
        .await
        .unwrap();
    assert_eq!(worker.job_key(&job).await.as_deref(), Some("k1"));
}
