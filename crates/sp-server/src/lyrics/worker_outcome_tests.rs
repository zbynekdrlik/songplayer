//! #144 (ROZHODNUTÉ 5905945274): a failed or empty re-run never darkens a
//! song the wall already serves. The two failure exits of the lyrics worker
//! (`fail_song`, the `Err` arm of `process_next`; `quarantine_empty_transcript`,
//! the g35t base tier's empty transcript) are driven directly on a real
//! in-memory DB and a real cache dir.
//!
//! A row serves lyrics when `has_lyrics = 1` AND its `<yt>_lyrics.json`
//! exists (what `playback/lyrics_loader.rs` loads). Such a row records only
//! the attempt: attempts + the `lyrics_next_attempt_at` backoff, and the manual
//! priority cleared so it does not loop. Its `has_lyrics`, `lyrics_source` and
//! file stay. A row with no served lyrics takes the terminal states as before.

use std::path::PathBuf;

use sqlx::SqlitePool;

use crate::lyrics::LYRICS_PIPELINE_VERSION;
use crate::lyrics::worker::LyricsWorker;

/// What the served song's `<yt>_lyrics.json` holds before the re-run.
const SERVED_BYTES: &[u8] = br#"{"version":22,"source":"gemini-3-5-transcribe","lines":[]}"#;
const SERVED_SOURCE: &str = "gemini-3-5-transcribe";

struct Rig {
    worker: LyricsWorker,
    pool: SqlitePool,
    dir: tempfile::TempDir,
}

impl Rig {
    fn lyrics_file(&self, youtube_id: &str) -> PathBuf {
        self.dir.path().join(format!("{youtube_id}_lyrics.json"))
    }
}

async fn rig() -> Rig {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'p', 'u', 'SP-test', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    let worker = LyricsWorker::new_for_test(pool.clone(), dir.path().to_path_buf(), events_tx);
    Rig { worker, pool, dir }
}

/// A song queued for a manual re-run (the #144 rollout's state): `has_lyrics`
/// as given, the served source, the current version, manual priority, and
/// `attempts` prior attempts.
async fn queued_song(rig: &Rig, id: i64, youtube_id: &str, has_lyrics: i64, attempts: i64) {
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
         lyrics_source, lyrics_pipeline_version, lyrics_manual_priority, lyrics_attempts) \
         VALUES (?, 1, ?, 1, ?, ?, ?, 1, ?)",
    )
    .bind(id)
    .bind(youtube_id)
    .bind(has_lyrics)
    .bind(if has_lyrics == 1 {
        Some(SERVED_SOURCE)
    } else {
        None
    })
    .bind(LYRICS_PIPELINE_VERSION as i64)
    .bind(attempts)
    .execute(&rig.pool)
    .await
    .unwrap();
}

/// `(has_lyrics, lyrics_source, lyrics_manual_priority, lyrics_attempts)`.
async fn row(pool: &SqlitePool, id: i64) -> (i64, Option<String>, i64, i64) {
    sqlx::query_as(
        "SELECT COALESCE(has_lyrics, 0), lyrics_source, lyrics_manual_priority, \
                COALESCE(lyrics_attempts, 0) \
         FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Seconds from now to `lyrics_next_attempt_at`, or `None` when it is NULL.
async fn backoff_secs(pool: &SqlitePool, id: i64) -> Option<i64> {
    sqlx::query_scalar(
        "SELECT CAST(ROUND((julianday(lyrics_next_attempt_at) - julianday('now')) * 86400) \
                AS INTEGER) \
         FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The served song kept everything the wall reads, and recorded one attempt
/// with the backoff of `attempts` (`downloader::retry_backoff`).
async fn assert_served_kept(rig: &Rig, id: i64, youtube_id: &str, attempts: i64, backoff: i64) {
    let (has_lyrics, source, manual, got_attempts) = row(&rig.pool, id).await;
    assert_eq!(has_lyrics, 1, "the wall must keep serving the song");
    assert_eq!(source.as_deref(), Some(SERVED_SOURCE), "the source is kept");
    assert_eq!(
        std::fs::read(rig.lyrics_file(youtube_id)).expect("the served lyrics file is kept"),
        SERVED_BYTES,
        "the served lyrics file is untouched"
    );
    assert_eq!(got_attempts, attempts, "the attempt is recorded");
    let secs = backoff_secs(&rig.pool, id).await.expect("a backoff is set");
    assert!(
        (backoff - 10..=backoff).contains(&secs),
        "the next attempt is {secs} s ahead, expected ~{backoff} s"
    );
    assert_eq!(
        manual, 0,
        "the manual priority is cleared, so it does not loop"
    );
    // Nothing re-picks it now: it is current and serves lyrics.
    let next =
        crate::lyrics::reprocess::get_next_video_for_lyrics(&rig.pool, LYRICS_PIPELINE_VERSION)
            .await
            .unwrap();
    assert!(next.is_none(), "the row must not loop: {next:?}");
}

#[tokio::test]
async fn a_failing_rerun_keeps_the_lyrics_the_wall_serves() {
    let rig = rig().await;
    queued_song(&rig, 1, "yt_served01", 1, 1).await;
    std::fs::write(rig.lyrics_file("yt_served01"), SERVED_BYTES).unwrap();

    let error = anyhow::anyhow!("gather: lrclib-plain cleanup failed for yt_served01: 502");
    rig.worker.fail_song(1, "yt_served01", &error).await;

    // A second attempt: `retry_backoff(2)` = 10 min.
    assert_served_kept(&rig, 1, "yt_served01", 2, 600).await;
}

#[tokio::test]
async fn an_empty_transcript_keeps_the_lyrics_the_wall_serves() {
    let rig = rig().await;
    queued_song(&rig, 2, "yt_served02", 1, 0).await;
    std::fs::write(rig.lyrics_file("yt_served02"), SERVED_BYTES).unwrap();

    rig.worker
        .quarantine_empty_transcript(2, "yt_served02")
        .await;

    // A first attempt: `retry_backoff(1)` = 5 min.
    assert_served_kept(&rig, 2, "yt_served02", 1, 300).await;
}

/// Pin: a song with NO served lyrics still takes today's terminal state.
#[tokio::test]
async fn a_failing_run_of_an_unserved_song_is_marked_no_source() {
    let rig = rig().await;
    queued_song(&rig, 3, "yt_unserved3", 0, 1).await;

    let error = anyhow::anyhow!("gather: genius fallback cleanup failed");
    rig.worker.fail_song(3, "yt_unserved3", &error).await;

    assert_eq!(
        row(&rig.pool, 3).await,
        (0, Some("no_source".to_string()), 0, 0),
        "an unserved song is parked as no_source, as before"
    );
    assert_eq!(backoff_secs(&rig.pool, 3).await, None);
}

/// Pin: an empty transcript on a song with NO served lyrics is still
/// quarantined as `asr_gap`, and a leftover lyrics file of an unserved row is
/// still removed (the row did not serve it: `has_lyrics = 0`).
#[tokio::test]
async fn an_empty_transcript_on_an_unserved_song_quarantines_it_as_asr_gap() {
    let rig = rig().await;
    queued_song(&rig, 4, "yt_unserved4", 0, 0).await;
    std::fs::write(rig.lyrics_file("yt_unserved4"), SERVED_BYTES).unwrap();

    rig.worker
        .quarantine_empty_transcript(4, "yt_unserved4")
        .await;

    assert_eq!(
        row(&rig.pool, 4).await,
        (0, Some("asr_gap".to_string()), 0, 0),
        "an unserved song is quarantined as asr_gap, as before"
    );
    assert!(
        !rig.lyrics_file("yt_unserved4").exists(),
        "the quarantine removes the unserved row's file"
    );
}

/// Pin: `has_lyrics = 1` with the file gone is not served (the wall has
/// nothing to show), so the failure takes the terminal state.
#[tokio::test]
async fn a_row_whose_lyrics_file_is_gone_is_not_served() {
    let rig = rig().await;
    queued_song(&rig, 5, "yt_nofile05", 1, 0).await;

    let error = anyhow::anyhow!("gather: lrclib-plain candidate present but ai_client is None");
    rig.worker.fail_song(5, "yt_nofile05", &error).await;

    let (has_lyrics, source, _, _) = row(&rig.pool, 5).await;
    assert_eq!(
        (has_lyrics, source.as_deref()),
        (0, Some("no_source")),
        "no file on disk = nothing served = the terminal state"
    );
}

/// Review round 1: a served base-tier full-mix row is re-attempted for the ★
/// tier at most once a day (#171, `fetch_bucket_fullmix_upgrade`, gated on
/// `lyrics_processed_at`). A failed upgrade that keeps the served lyrics must
/// end the pass like every other ended pass (success, `no_source`, `asr_gap`
/// all stamp `lyrics_processed_at`), or the row would come back after the
/// 5-minute backoff instead of a day later.
#[tokio::test]
async fn a_failed_upgrade_of_a_served_full_mix_row_waits_a_day() {
    let rig = rig().await;
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
         lyrics_source, lyrics_pipeline_version, lyrics_manual_priority, lyrics_processed_at) \
         VALUES (6, 1, 'yt_fullmix6', 1, 1, ?, ?, 0, '2000-01-01T00:00:00.000Z')",
    )
    .bind(crate::lyrics::g35t_transcript::SOURCE_G35T_FULLMIX)
    .bind(LYRICS_PIPELINE_VERSION as i64)
    .execute(&rig.pool)
    .await
    .unwrap();
    std::fs::write(rig.lyrics_file("yt_fullmix6"), SERVED_BYTES).unwrap();
    // The upgrade bucket picks it now (processed a day+ ago).
    let picked =
        crate::lyrics::reprocess::get_next_video_for_lyrics(&rig.pool, LYRICS_PIPELINE_VERSION)
            .await
            .unwrap();
    assert_eq!(picked.map(|r| r.id), Some(6));

    let error = anyhow::anyhow!("gather: genius fallback cleanup failed");
    rig.worker.fail_song(6, "yt_fullmix6", &error).await;

    let (has_lyrics, source, _, attempts) = row(&rig.pool, 6).await;
    assert_eq!(
        (has_lyrics, source.as_deref(), attempts),
        (
            1,
            Some(crate::lyrics::g35t_transcript::SOURCE_G35T_FULLMIX),
            1
        ),
        "the served full-mix lyrics are kept, the attempt recorded"
    );
    // Even once the backoff has run out, the next upgrade waits a day.
    sqlx::query(
        "UPDATE videos SET lyrics_next_attempt_at = '2000-01-01T00:00:00.000Z' WHERE id = 6",
    )
    .execute(&rig.pool)
    .await
    .unwrap();
    let next =
        crate::lyrics::reprocess::get_next_video_for_lyrics(&rig.pool, LYRICS_PIPELINE_VERSION)
            .await
            .unwrap();
    assert!(
        next.is_none(),
        "a failed upgrade must wait out the once-a-day gate: {next:?}"
    );
}
