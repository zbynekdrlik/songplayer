//! #144 (ROZHODNUTÉ 5905945274): a failed or empty re-run never darkens a
//! song the wall already serves. The two failure exits of the lyrics worker
//! (`fail_song`, the `Err` arm of `process_next`; `quarantine_no_lyrics`,
//! the g35t base tier's empty or too sparse transcript) are driven directly on a real
//! in-memory DB and a real cache dir.
//!
//! A row serves lyrics when `has_lyrics = 1` AND its `<yt>_lyrics.json`
//! exists (what `playback/lyrics_loader.rs` loads). Such a row records only
//! the attempt: attempts + the `lyrics_next_attempt_at` backoff, which the
//! manual bucket waits out (no loop). ROZHODNUTÉ 5908227646: it stays in the
//! manual queue until its 3rd failed attempt clears the manual priority. Its
//! `has_lyrics`, `lyrics_source` and file stay.
//!
//! #263: an ERROR on a row with no served lyrics is a deferral, never a
//! terminal state: the attempt and its backoff on every row of the video,
//! the row otherwise as it was, picked again once due. Its empty
//! transcript is still quarantined as `asr_gap`.

use std::path::PathBuf;

use sqlx::SqlitePool;

use crate::db::models::SERVED_RERUN_MAX_ATTEMPTS;
use crate::lyrics::LYRICS_PIPELINE_VERSION;
use crate::lyrics::worker::LyricsWorker;
use crate::test_log::{Captured, capturing};

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
    // ROZHODNUTÉ 5908227646: a failed attempt below the cap keeps the song
    // in the manual rollout; it waits out its backoff (no loop).
    assert_eq!(manual, 1, "the song stays queued for its next attempt");
    assert_eq!(
        next_pick(rig).await,
        None,
        "not before its backoff: no loop"
    );
    make_due(rig, id).await;
    assert_eq!(
        next_pick(rig).await,
        Some(id),
        "due after its backoff, the manual bucket picks it again"
    );
}

/// The row the lyrics worker would process next.
async fn next_pick(rig: &Rig) -> Option<i64> {
    crate::lyrics::reprocess::get_next_video_for_lyrics(&rig.pool, LYRICS_PIPELINE_VERSION)
        .await
        .unwrap()
        .map(|r| r.id)
}

/// Its backoff has run out.
async fn make_due(rig: &Rig, id: i64) {
    sqlx::query(
        "UPDATE videos SET lyrics_next_attempt_at = '2000-01-01T00:00:00.000Z' WHERE id = ?",
    )
    .bind(id)
    .execute(&rig.pool)
    .await
    .unwrap();
}

/// ROZHODNUTÉ 5908227646 (#144, lane A): a transient Claude / Gemini error
/// must not drop a served song out of the manual rollout. Its failed
/// re-runs keep `lyrics_manual_priority = 1` and wait out the backoff; only
/// the 3rd failed attempt clears it (WARN). The served lyrics stay
/// throughout.
#[tokio::test]
async fn a_served_song_stays_queued_until_its_third_failed_attempt() {
    let rig = rig().await;
    queued_song(&rig, 7, "yt_served07", 1, 0).await;
    std::fs::write(rig.lyrics_file("yt_served07"), SERVED_BYTES).unwrap();
    let error = anyhow::anyhow!("gather: lrclib-plain cleanup failed for yt_served07: 502");

    rig.worker.fail_song(7, "yt_served07", &error).await;
    rig.worker.fail_song(7, "yt_served07", &error).await;
    // Two attempts: `retry_backoff(2)` = 10 min; still queued, due after it.
    assert_served_kept(&rig, 7, "yt_served07", 2, 600).await;

    rig.worker.fail_song(7, "yt_served07", &error).await;
    let (has_lyrics, source, manual, attempts) = row(&rig.pool, 7).await;
    assert_eq!(
        (has_lyrics, source.as_deref(), attempts),
        (1, Some(SERVED_SOURCE), 3),
        "the served lyrics stay"
    );
    assert_eq!(
        manual, 0,
        "the 3rd failed attempt clears the manual priority"
    );
    make_due(&rig, 7).await;
    assert_eq!(
        next_pick(&rig).await,
        None,
        "it left the rollout: current, served"
    );
    assert_eq!(SERVED_RERUN_MAX_ATTEMPTS, 3);
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
        .quarantine_no_lyrics(2, "yt_served02", "empty_transcript", None)
        .await;

    // A first attempt: `retry_backoff(1)` = 5 min.
    assert_served_kept(&rig, 2, "yt_served02", 1, 300).await;
}

/// #144 F1: a video in two playlists has two rows and ONE `<yt>_lyrics.json`.
/// A row whose SIBLING serves the file serves it too: its empty transcript
/// records only the attempt and never deletes the file the wall plays for
/// the other playlist (before, the unserved row was quarantined and the
/// shared file deleted).
#[tokio::test]
async fn an_empty_transcript_never_deletes_the_file_a_sibling_row_serves() {
    let rig = rig().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (2, 'q', 'u2', 'SP-other', 1)",
    )
    .execute(&rig.pool)
    .await
    .unwrap();
    queued_song(&rig, 5, "yt_shared05", 0, 0).await;
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
         lyrics_source, lyrics_pipeline_version) VALUES (6, 2, 'yt_shared05', 1, 1, ?, ?)",
    )
    .bind(SERVED_SOURCE)
    .bind(LYRICS_PIPELINE_VERSION as i64)
    .execute(&rig.pool)
    .await
    .unwrap();
    std::fs::write(rig.lyrics_file("yt_shared05"), SERVED_BYTES).unwrap();

    rig.worker
        .quarantine_no_lyrics(5, "yt_shared05", "empty_transcript", None)
        .await;

    assert_eq!(
        std::fs::read(rig.lyrics_file("yt_shared05")).unwrap(),
        SERVED_BYTES,
        "the file the sibling serves stays"
    );
    assert_eq!(
        row(&rig.pool, 6).await.0,
        1,
        "the sibling still serves its lyrics"
    );
}

/// #263 (re-pinned: this was the `no_source` pin): a song queued by hand
/// that the wall does not serve is DEFERRED by an error. It records the
/// attempt with its backoff, stays in the manual queue as it was, and the
/// manual bucket picks it again once due, never before.
#[tokio::test]
async fn a_failing_run_of_an_unserved_queued_song_is_deferred() {
    let rig = rig().await;
    queued_song(&rig, 3, "yt_unserved3", 0, 1).await;

    let error = anyhow::anyhow!("gather: genius fallback cleanup failed");
    rig.worker.fail_song(3, "yt_unserved3", &error).await;

    assert_eq!(
        row(&rig.pool, 3).await,
        (0, None, 1, 2),
        "the row as it was, still queued, its second attempt recorded"
    );
    // A second attempt: `retry_backoff(2)` = 10 min.
    assert_backoff(&rig.pool, 3, 600).await;
    assert_eq!(next_pick(&rig).await, None, "not before its backoff");
    make_due(&rig, 3).await;
    assert_eq!(
        next_pick(&rig).await,
        Some(3),
        "due: the manual bucket picks it again"
    );
}

/// A song no row serves, outside the manual queue, in `playlist`: no lyrics,
/// `lyrics_source` and the pipeline version as given, no attempt yet.
async fn unserved_song(
    rig: &Rig,
    id: i64,
    playlist: i64,
    youtube_id: &str,
    source: Option<&str>,
    version: i64,
) {
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
         lyrics_source, lyrics_pipeline_version) VALUES (?, ?, ?, 1, 0, ?, ?)",
    )
    .bind(id)
    .bind(playlist)
    .bind(youtube_id)
    .bind(source)
    .bind(version)
    .execute(&rig.pool)
    .await
    .unwrap();
}

/// The row's next attempt is `backoff` seconds ahead (`downloader::retry_backoff`).
async fn assert_backoff(pool: &SqlitePool, id: i64, backoff: i64) {
    let secs = backoff_secs(pool, id).await.expect("a backoff is set");
    assert!(
        (backoff - 10..=backoff).contains(&secs),
        "row {id}: the next attempt is {secs} s ahead, expected ~{backoff} s"
    );
}

/// `(lyrics_source, lyrics_pipeline_version)`.
async fn source_and_version(pool: &SqlitePool, id: i64) -> (Option<String>, i64) {
    sqlx::query_as("SELECT lyrics_source, lyrics_pipeline_version FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// #263: SNV's Joy (`vh70Rbmcg8g`) hit one error on 15.9.2026, was parked
/// `no_source` at the current pipeline version, and no queue took it again.
/// An error on a song no row serves is a DEFERRAL now: the attempt with the
/// downloader's backoff (5 min, then 10 min, …, capped at 24 h), the row as
/// it was, and the null bucket picks it again once due, never before.
#[tokio::test]
async fn an_unserved_songs_error_is_retried_after_its_backoff() {
    let rig = rig().await;
    let current = LYRICS_PIPELINE_VERSION as i64;
    unserved_song(&rig, 10, 1, "yt_unserv10", None, current).await;
    assert_eq!(next_pick(&rig).await, Some(10), "a new song is due");
    let error = anyhow::anyhow!("gather: lrclib-plain cleanup failed for yt_unserv10: 502");

    rig.worker.fail_song(10, "yt_unserv10", &error).await;
    assert_eq!(
        row(&rig.pool, 10).await,
        (0, None, 0, 1),
        "the row as it was, its first attempt recorded"
    );
    assert_eq!(source_and_version(&rig.pool, 10).await, (None, current));
    // A first attempt: `retry_backoff(1)` = 5 min.
    assert_backoff(&rig.pool, 10, 300).await;
    assert_eq!(next_pick(&rig).await, None, "not before its backoff");

    make_due(&rig, 10).await;
    assert_eq!(
        next_pick(&rig).await,
        Some(10),
        "due: the null bucket picks it again"
    );

    rig.worker.fail_song(10, "yt_unserv10", &error).await;
    assert_eq!(row(&rig.pool, 10).await, (0, None, 0, 2));
    // The backoff grows: `retry_backoff(2)` = 10 min.
    assert_backoff(&rig.pool, 10, 600).await;
    assert_eq!(next_pick(&rig).await, None, "not before its longer backoff");
}

/// #263: the deferral leaves `lyrics_source` and the version as they were.
/// A song parked at an OLDER pipeline version (due again by the version
/// rule) keeps its sentinel at that version, so an error never re-parks it
/// at the current one, and it is picked again once its backoff ran out.
#[tokio::test]
async fn an_error_never_re_parks_a_stale_sentinel_at_the_current_version() {
    let rig = rig().await;
    let older = LYRICS_PIPELINE_VERSION as i64 - 1;
    unserved_song(&rig, 11, 1, "yt_stale011", Some("no_source"), older).await;
    assert_eq!(
        next_pick(&rig).await,
        Some(11),
        "an older version's sentinel is due"
    );

    let error = anyhow::anyhow!("gather: genius fallback cleanup failed: CLIProxy unreachable");
    rig.worker.fail_song(11, "yt_stale011", &error).await;

    assert_eq!(
        source_and_version(&rig.pool, 11).await,
        (Some("no_source".to_string()), older),
        "the sentinel stays at its older version"
    );
    assert_eq!(row(&rig.pool, 11).await.3, 1, "the attempt is recorded");
    assert_backoff(&rig.pool, 11, 300).await;
    assert_eq!(next_pick(&rig).await, None, "not before its backoff");
    make_due(&rig, 11).await;
    assert_eq!(next_pick(&rig).await, Some(11), "picked again once due");
}

/// #263 + #144 F1 (one lyrics state per VIDEO): a video in two playlists
/// has two rows and one `{yt}_lyrics.json`; the error of one row defers
/// every row of it, so no sibling is picked during the backoff.
#[tokio::test]
async fn every_row_of_the_video_takes_the_deferral() {
    let rig = rig().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (2, 'q', 'u2', 'SP-other', 1)",
    )
    .execute(&rig.pool)
    .await
    .unwrap();
    let current = LYRICS_PIPELINE_VERSION as i64;
    unserved_song(&rig, 12, 1, "yt_shared12", None, current).await;
    unserved_song(&rig, 13, 2, "yt_shared12", None, current).await;

    let error = anyhow::anyhow!("gather: lrclib-plain cleanup failed for yt_shared12: 502");
    rig.worker.fail_song(12, "yt_shared12", &error).await;

    for id in [12, 13] {
        assert_eq!(row(&rig.pool, id).await, (0, None, 0, 1), "row {id}");
        assert_backoff(&rig.pool, id, 300).await;
    }
    assert_eq!(
        next_pick(&rig).await,
        None,
        "no row of the video before its backoff"
    );
    make_due(&rig, 13).await;
    assert_eq!(next_pick(&rig).await, Some(13), "picked again once due");
}

/// #263: one WARN per failed attempt, naming the song and the error (the
/// text `process_song` returned; a Gemini error is redacted where it is
/// made, `gemini_api::redact_keys`).
#[tokio::test]
async fn an_unserved_songs_error_is_one_warn_naming_it() {
    let rig = rig().await;
    let current = LYRICS_PIPELINE_VERSION as i64;
    unserved_song(&rig, 14, 1, "yt_unserv14", None, current).await;
    let cap = Captured::default();
    let _log = tracing::subscriber::set_default(capturing(&cap));
    let error = anyhow::anyhow!("gather: genius fallback cleanup failed: 503");

    rig.worker.fail_song(14, "yt_unserv14", &error).await;
    rig.worker.fail_song(14, "yt_unserv14", &error).await;

    let warns: Vec<String> = cap
        .lines_with(" WARN ")
        .into_iter()
        .filter(|l| l.contains("process_error: gather: genius fallback cleanup failed: 503"))
        .collect();
    assert_eq!(warns.len(), 2, "one WARN per failed attempt: {warns:?}");
    assert!(
        warns.iter().all(|l| l.contains("yt_unserv14")),
        "each names the song: {warns:?}"
    );
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
        .quarantine_no_lyrics(4, "yt_unserved4", "empty_transcript", None)
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
/// nothing to show), so a failure takes the unserved path: since #263 a
/// deferral, which keeps the song in the manual queue, where a served
/// song leaves it on its `SERVED_RERUN_MAX_ATTEMPTS`th failed attempt.
#[tokio::test]
async fn a_row_whose_lyrics_file_is_gone_is_not_served() {
    let rig = rig().await;
    queued_song(&rig, 5, "yt_nofile05", 1, 0).await;

    let error = anyhow::anyhow!("gather: lrclib-plain candidate present but ai_client is None");
    for _ in 0..SERVED_RERUN_MAX_ATTEMPTS {
        rig.worker.fail_song(5, "yt_nofile05", &error).await;
    }

    assert_eq!(
        row(&rig.pool, 5).await,
        (
            1,
            Some(SERVED_SOURCE.to_string()),
            1,
            i64::from(SERVED_RERUN_MAX_ATTEMPTS)
        ),
        "no file on disk = nothing served = the unserved deferral, still queued"
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

// --- #283: a transcript too sparse to be lyrics ---

/// A song of four minutes: 20 words is exactly five a minute.
const SONG_MS: i64 = 240_000;

/// `n` sung words, one a second from 10 s, as the transcript gives them.
fn sung(n: u64) -> Vec<crate::lyrics::g35t_client::AsrWord> {
    (0..n)
        .map(|i| crate::lyrics::g35t_client::AsrWord {
            text: "sing".to_string(),
            start_ms: 10_000 + i * 1_000,
            end_ms: 10_500 + i * 1_000,
        })
        .collect()
}

/// Writes a served `<yt>_lyrics.json` of `lines` lines of four words each,
/// and returns its bytes.
fn served_track(rig: &Rig, youtube_id: &str, lines: u64) -> Vec<u8> {
    let track = sp_core::lyrics::LyricsTrack {
        version: LYRICS_PIPELINE_VERSION,
        source: SERVED_SOURCE.to_string(),
        language_source: "en".into(),
        language_translation: "sk".into(),
        lines: (0..lines)
            .map(|i| sp_core::lyrics::LyricsLine {
                start_ms: i * 5_000,
                end_ms: i * 5_000 + 4_000,
                en: "we lift your name".into(),
                sk: None,
                words: None,
            })
            .collect(),
        register: None,
    };
    let bytes = serde_json::to_vec(&track).unwrap();
    std::fs::write(rig.lyrics_file(youtube_id), &bytes).unwrap();
    bytes
}

/// The base tier on the song's one transcript (given, so nothing is sent):
/// whether it ended the song without a track.
async fn base_tier_quarantined(rig: &Rig, id: i64, youtube_id: &str, words: u64) -> bool {
    let outcome = rig
        .worker
        .run_g35t_transcript_branch(
            None,
            None,
            Some(sung(words)),
            &[],
            id,
            youtube_id,
            "Song",
            "Artist",
            0,
            Some(SONG_MS),
        )
        .await
        .unwrap();
    matches!(
        outcome,
        crate::lyrics::worker_g35t::G35tOutcome::Quarantined
    )
}

/// 242's case (#283): a transcript of a few shouts over the whole song is a
/// failed transcription. A song with no lyrics served is quarantined as
/// `asr_gap`, never given a track of the shouts.
#[tokio::test]
async fn a_sparse_transcript_of_an_unserved_song_quarantines_it_as_asr_gap() {
    let rig = rig().await;
    queued_song(&rig, 11, "yt_sparse011", 0, 0).await;

    assert!(
        base_tier_quarantined(&rig, 11, "yt_sparse011", 16).await,
        "16 words in 4 minutes is no track"
    );
    assert_eq!(
        row(&rig.pool, 11).await,
        (0, Some("asr_gap".to_string()), 0, 0),
    );
    assert!(!rig.lyrics_file("yt_sparse011").exists());
}

/// A served track of real lyrics survives a sparse re-run, like any failed
/// re-run (#144): only the attempt is recorded.
#[tokio::test]
async fn a_sparse_rerun_keeps_a_served_track_of_real_lyrics() {
    let rig = rig().await;
    queued_song(&rig, 12, "yt_sparse012", 1, 0).await;
    let served = served_track(&rig, "yt_sparse012", 30);

    assert!(base_tier_quarantined(&rig, 12, "yt_sparse012", 16).await);
    let (has_lyrics, source, _, attempts) = row(&rig.pool, 12).await;
    assert_eq!(
        (has_lyrics, source.as_deref(), attempts),
        (1, Some(SERVED_SOURCE), 1),
        "120 words in 4 minutes are lyrics: kept, the attempt recorded"
    );
    assert_eq!(
        std::fs::read(rig.lyrics_file("yt_sparse012")).unwrap(),
        served
    );
}

/// What 242 served — its five shouts — is not kept over a sparse re-run:
/// the song is quarantined as `asr_gap` and the file goes.
#[tokio::test]
async fn a_sparse_rerun_quarantines_a_served_track_of_only_shouts() {
    let rig = rig().await;
    queued_song(&rig, 13, "yt_sparse013", 1, 0).await;
    served_track(&rig, "yt_sparse013", 4);

    assert!(base_tier_quarantined(&rig, 13, "yt_sparse013", 16).await);
    assert_eq!(
        row(&rig.pool, 13).await,
        (0, Some("asr_gap".to_string()), 0, 0),
        "16 served words in 4 minutes are no lyrics"
    );
    assert!(!rig.lyrics_file("yt_sparse013").exists());
}

/// The same for an EMPTY re-run: a served track of only shouts is not kept.
#[tokio::test]
async fn an_empty_rerun_quarantines_a_served_track_of_only_shouts() {
    let rig = rig().await;
    queued_song(&rig, 14, "yt_sparse014", 1, 0).await;
    served_track(&rig, "yt_sparse014", 4);

    assert!(base_tier_quarantined(&rig, 14, "yt_sparse014", 0).await);
    assert_eq!(
        row(&rig.pool, 14).await,
        (0, Some("asr_gap".to_string()), 0, 0),
    );
    assert!(!rig.lyrics_file("yt_sparse014").exists());
}

/// Five words a minute is a track: the base tier ships it.
#[tokio::test]
async fn a_transcript_of_five_words_a_minute_is_a_track() {
    let rig = rig().await;
    queued_song(&rig, 15, "yt_sparse015", 0, 0).await;

    assert!(!base_tier_quarantined(&rig, 15, "yt_sparse015", 20).await);
    assert_eq!(row(&rig.pool, 15).await.1, None, "nothing written");
}
