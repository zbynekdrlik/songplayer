//! #136 tests for `song_input`: a stem / dub job runs on the input its song's
//! row records AFTER the job holds the heavy slot, not on the paths it read
//! when it was picked.

/// Byte offsets of `needles` in `src`, each searched from `async fn
/// process_next(` on (CRLF-normalised for the Windows checkout).
fn offsets_in_process_next(src: &str, needles: &[&str]) -> Vec<usize> {
    let src = src.replace("\r\n", "\n");
    let start = src
        .find("async fn process_next(")
        .expect("the worker must have process_next");
    needles
        .iter()
        .map(|needle| {
            let at = src[start..]
                .find(needle)
                .unwrap_or_else(|| panic!("process_next must contain {needle:?}"));
            start + at
        })
        .collect()
}

/// Structural: the stem worker takes the heavy slot, THEN re-reads the song's
/// input and derives the stem paths from it, THEN separates. A job picked
/// before a rename (the metadata repair) waited for the slot with a path that
/// no longer exists.
#[test]
fn the_stem_worker_reads_its_input_after_it_holds_the_heavy_slot() {
    let at = offsets_in_process_next(
        include_str!("stems/worker.rs"),
        &[
            "acquire_slot_for_spawn(",
            "song_input::job_input(",
            ".stem_job(job)",
            "stem_paths(",
            "separate_stems(",
        ],
    );
    assert!(
        at.windows(2).all(|w| w[0] < w[1]),
        "slot → re-read → the job on it → stem paths → separation, got offsets {at:?}"
    );
}

/// Structural: the dub worker takes the heavy slot, THEN re-reads the song's
/// input (the audio, and the vocals stem it may feed the session), THEN
/// synthesizes on it.
#[test]
fn the_dub_worker_reads_its_input_after_it_holds_the_heavy_slot() {
    let at = offsets_in_process_next(
        include_str!("dabing/worker.rs"),
        &[
            "acquire_slot_for_spawn(",
            "song_input::job_input(",
            ".dub_job(job)",
            "self.synthesize(",
        ],
    );
    assert!(
        at.windows(2).all(|w| w[0] < w[1]),
        "slot → re-read → the job on it → synthesis, got offsets {at:?}"
    );
}

// ── job_input on a real in-memory DB and a real cache dir ──────────────────

use super::{HeavyJob, INPUT_MISSING_RECHECK, SongInput, job_input};
use crate::db::models_dabing::DubJob;
use crate::db::models_stems::StemJob;
use sqlx::SqlitePool;

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

fn text(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Seconds from now to `column` of row `id`, or `None` when it is NULL.
async fn wait_secs(pool: &SqlitePool, column: &str, id: i64) -> Option<i64> {
    let sql = format!(
        "SELECT CAST(ROUND((julianday({column}) - julianday('now')) * 86400) AS INTEGER) \
         FROM videos WHERE id = ?"
    );
    sqlx::query_scalar(&sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The repair renamed the song while the job waited: the job was picked with
/// the old audio, the row (and the disk) now hold the new one. The job runs on
/// the new audio and the vocals stem named after it; everything else is the
/// pick's.
#[tokio::test]
async fn a_job_runs_on_the_audio_the_row_records_now() {
    let dir = tempfile::tempdir().unwrap();
    let old_audio = dir
        .path()
        .join("Old Song_Old Artist_IYAOosrh7HY_normalized_gf_audio.flac");
    let new_audio = dir
        .path()
        .join("Gods Not Dead_Enjoy Worship_IYAOosrh7HY_normalized_audio.flac");
    let new_vocals = dir
        .path()
        .join("Gods Not Dead_Enjoy Worship_IYAOosrh7HY_normalized_audio_vocals.flac");
    std::fs::write(&new_audio, b"a").unwrap();
    let pool = pool().await;
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, audio_file_path, \
                             vocals_file_path, stem_status) \
         VALUES (7, 1, 'IYAOosrh7HY', 1, ?, ?, 'done')",
    )
    .bind(text(&new_audio))
    .bind(text(&new_vocals))
    .execute(&pool)
    .await
    .unwrap();

    let input = job_input(&pool, 7, &text(&old_audio), HeavyJob::Stems)
        .await
        .expect("the song's audio is on disk under its new name");
    assert_eq!(
        input,
        SongInput {
            audio_file_path: text(&new_audio),
            vocals_file_path: Some(text(&new_vocals)),
            stem_status: Some("done".into()),
        }
    );

    let picked_stem = StemJob {
        video_id: 7,
        youtube_id: "IYAOosrh7HY".into(),
        audio_file_path: text(&old_audio),
        duration_ms: Some(215_000),
        song: Some("Old Song".into()),
        artist: Some("Old Artist".into()),
    };
    assert_eq!(
        input.clone().stem_job(picked_stem.clone()),
        StemJob {
            audio_file_path: text(&new_audio),
            ..picked_stem
        }
    );

    let picked_dub = DubJob {
        video_id: 7,
        youtube_id: "IYAOosrh7HY".into(),
        audio_file_path: text(&old_audio),
        duration_ms: Some(215_000),
        dub_status: "synth".into(),
        vocals_file_path: Some("old-vocals.flac".into()),
        instrumental_file_path: Some("old-instrumental.flac".into()),
        stem_status: None,
        dub_attempts: 1,
    };
    assert_eq!(
        input.dub_job(picked_dub.clone()),
        DubJob {
            audio_file_path: text(&new_audio),
            vocals_file_path: Some(text(&new_vocals)),
            stem_status: Some("done".into()),
            ..picked_dub
        }
    );
    // Found: nothing is rescheduled.
    assert_eq!(wait_secs(&pool, "stem_next_attempt_at", 7).await, None);
}

/// No audio on disk after the re-read: the stem job does not run, counts no
/// attempt, keeps its status, and waits out the recheck so the rest of the
/// queue goes first.
#[tokio::test]
async fn a_stem_job_whose_audio_is_gone_is_rechecked_later_without_a_penalty() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir
        .path()
        .join("Song_Artist_IYAOosrh7HY_normalized_audio.flac");
    let pool = pool().await;
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, audio_file_path, \
                             stem_status, stem_attempts) \
         VALUES (8, 1, 'IYAOosrh7HY', 1, ?, 'failed', 2)",
    )
    .bind(text(&gone))
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(
        job_input(&pool, 8, &text(&gone), HeavyJob::Stems).await,
        None
    );

    let (status, attempts): (Option<String>, i64) =
        sqlx::query_as("SELECT stem_status, stem_attempts FROM videos WHERE id = 8")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (status.as_deref(), attempts),
        (Some("failed"), 2),
        "no penalty"
    );
    let recheck = INPUT_MISSING_RECHECK.as_secs() as i64;
    let secs = wait_secs(&pool, "stem_next_attempt_at", 8)
        .await
        .expect("the recheck is scheduled");
    assert!((recheck - 10..=recheck).contains(&secs), "{secs} s ahead");
    assert_eq!(wait_secs(&pool, "dub_next_attempt_at", 8).await, None);
    assert_eq!(
        crate::db::models_stems::get_next_video_for_stems(&pool)
            .await
            .unwrap(),
        None,
        "the row waits out the recheck"
    );
}

/// The same for a dub job: no attempt, the status stays `synth`.
#[tokio::test]
async fn a_dub_job_whose_audio_is_gone_is_rechecked_later_without_a_penalty() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir
        .path()
        .join("Talk_Speaker_IYAOosrh7HY_normalized_audio.flac");
    let pool = pool().await;
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, audio_file_path, \
                             dub_requested, dub_status, dub_attempts) \
         VALUES (9, 1, 'IYAOosrh7HY', 1, ?, 1, 'synth', 1)",
    )
    .bind(text(&gone))
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(job_input(&pool, 9, &text(&gone), HeavyJob::Dub).await, None);

    let (status, attempts): (String, i64) =
        sqlx::query_as("SELECT dub_status, dub_attempts FROM videos WHERE id = 9")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), attempts), ("synth", 1), "no penalty");
    let recheck = INPUT_MISSING_RECHECK.as_secs() as i64;
    let secs = wait_secs(&pool, "dub_next_attempt_at", 9)
        .await
        .expect("the recheck is scheduled");
    assert!((recheck - 10..=recheck).contains(&secs), "{secs} s ahead");
    assert_eq!(wait_secs(&pool, "stem_next_attempt_at", 9).await, None);
    assert_eq!(
        crate::db::models_dabing::get_next_dub_job(&pool)
            .await
            .unwrap(),
        None,
        "the row waits out the recheck"
    );
}

/// A row that records no audio (or is gone) has no input either.
#[tokio::test]
async fn a_row_without_audio_has_no_input() {
    let pool = pool().await;
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized) \
         VALUES (10, 1, 'IYAOosrh7HY', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(
        job_input(&pool, 10, "picked.flac", HeavyJob::Stems).await,
        None
    );
    assert_eq!(
        job_input(&pool, 99, "picked.flac", HeavyJob::Dub).await,
        None
    );
}

/// Review round 1: read under the lock, a missing audio file is a real loss,
/// not the rename race. The dub worker records why on the row (`dub_error`,
/// what the Dabing section shows), still with no attempt counted and the
/// `synth` status kept; a finished dub clears it (`mark_dub_ready`).
#[tokio::test]
async fn a_dub_job_whose_audio_is_gone_says_why_on_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir
        .path()
        .join("Talk_Speaker_IYAOosrh7HY_normalized_audio.flac");
    let pool = pool().await;
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, audio_file_path, \
                             dub_requested, dub_status, dub_attempts) \
         VALUES (11, 1, 'IYAOosrh7HY', 1, ?, 1, 'synth', 1)",
    )
    .bind(text(&gone))
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(
        job_input(&pool, 11, &text(&gone), HeavyJob::Dub).await,
        None
    );

    let (status, attempts, error): (String, i64, Option<String>) =
        sqlx::query_as("SELECT dub_status, dub_attempts, dub_error FROM videos WHERE id = 11")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), attempts), ("synth", 1), "no penalty");
    let error = error.expect("dub_error says why the dub waits");
    assert!(error.contains("audio file is missing"), "{error}");
    assert!(
        error.contains(&text(&gone)),
        "names the recorded path: {error}"
    );
}
