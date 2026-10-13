//! V41 (#263): every row an error parked as `no_source` that serves no
//! lyrics is queued once more, under the rule that an error is a deferral.

use super::test_helpers::{apply_first_n, apply_upto};
use super::*;

/// One row: `(youtube_id, has_lyrics, lyrics_source, lyrics_attempts,
/// lyrics_next_attempt_at, lyrics_manual_priority)`.
type Row = (String, i64, Option<String>, i64, Option<String>, i64);

/// A fixture row as [`Row`], plus whether V41 clears it.
type Case = (
    &'static str,
    i64,
    Option<&'static str>,
    i64,
    Option<&'static str>,
    i64,
    bool,
);

/// A backoff still running when V41 lands.
const LATER: &str = "2999-01-01T00:00:00.000Z";
/// The last pass of every fixture row (SNV's Joy, `vh70Rbmcg8g`).
const PROCESSED: &str = "2026-09-15T04:38:59.000Z";

/// Cleared: `no_source` with `has_lyrics = 0` (its sentinel, attempts and
/// backoff), and nothing else of it. Untouched: a row that serves lyrics
/// (even one marked `no_source`), the 30-min policy's `unsupported_source`,
/// the `asr_gap` quarantine, the older `failed` / `empty` sentinels, a
/// served row and an unserved row in its backoff.
#[tokio::test]
async fn v41_queues_every_row_an_error_parked_once_more() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 40).await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (41001, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    let cases: [Case; 10] = [
        ("parkednow01", 0, Some("no_source"), 0, None, 0, true),
        ("parkedwait1", 0, Some("no_source"), 3, Some(LATER), 0, true),
        ("parkedmanu1", 0, Some("no_source"), 1, Some(LATER), 1, true),
        (
            "servesns001",
            1,
            Some("no_source"),
            2,
            Some(LATER),
            0,
            false,
        ),
        (
            "unsupport01",
            0,
            Some("unsupported_source"),
            0,
            None,
            0,
            false,
        ),
        ("asrgap00001", 0, Some("asr_gap"), 1, Some(LATER), 0, false),
        ("failed00001", 0, Some("failed"), 0, None, 0, false),
        ("empty000001", 0, Some("empty"), 0, None, 1, false),
        (
            "served00001",
            1,
            Some("gemini-3-5-transcribe"),
            1,
            Some(LATER),
            0,
            false,
        ),
        ("deferred001", 0, None, 2, Some(LATER), 0, false),
    ];
    for (youtube_id, has_lyrics, source, attempts, next, manual, _) in cases {
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_source, lyrics_attempts, lyrics_next_attempt_at, \
             lyrics_manual_priority, lyrics_pipeline_version, lyrics_processed_at) \
             VALUES (41001, ?, 1, ?, ?, ?, ?, ?, 22, ?)",
        )
        .bind(youtube_id)
        .bind(has_lyrics)
        .bind(source)
        .bind(attempts)
        .bind(next)
        .bind(manual)
        .bind(PROCESSED)
        .execute(&pool)
        .await
        .unwrap();
    }

    apply_upto(&pool, 41).await;

    let got: Vec<Row> = sqlx::query_as(
        "SELECT youtube_id, has_lyrics, lyrics_source, lyrics_attempts, \
         lyrics_next_attempt_at, lyrics_manual_priority FROM videos ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let want: Vec<Row> = cases
        .iter()
        .map(
            |&(youtube_id, has_lyrics, source, attempts, next, manual, cleared)| {
                if cleared {
                    (youtube_id.to_string(), has_lyrics, None, 0, None, manual)
                } else {
                    (
                        youtube_id.to_string(),
                        has_lyrics,
                        source.map(str::to_string),
                        attempts,
                        next.map(str::to_string),
                        manual,
                    )
                }
            },
        )
        .collect();
    assert_eq!(got, want);
    // Nothing else of any row moves: its version and its last pass.
    let kept: Vec<(i64, Option<String>)> = sqlx::query_as(
        "SELECT lyrics_pipeline_version, lyrics_processed_at FROM videos ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        kept.iter().all(|k| *k == (22, Some(PROCESSED.to_string()))),
        "{kept:?}"
    );
    assert_eq!(current_schema_version(&pool).await.unwrap(), 41);
}

/// What V41 is for: a song parked `no_source` at the CURRENT pipeline
/// version (SNV's Joy since 15.9.2026, I Gotta Praise since 25.9.2026) is
/// taken by the lyrics queue again. `run_migrations` brings the schema the
/// selector reads, whatever later versions add.
#[tokio::test]
async fn v41_gives_a_parked_song_to_the_lyrics_queue_again() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 40).await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (41002, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
         lyrics_source, lyrics_pipeline_version) VALUES (41012, 41002, 'vh70Rbmcg8g', 1, 0, \
         'no_source', ?)",
    )
    .bind(i64::from(crate::lyrics::LYRICS_PIPELINE_VERSION))
    .execute(&pool)
    .await
    .unwrap();

    run_migrations(&pool).await.unwrap();

    let next = crate::lyrics::reprocess::get_next_video_for_lyrics(
        &pool,
        crate::lyrics::LYRICS_PIPELINE_VERSION,
    )
    .await
    .unwrap();
    assert_eq!(next.map(|r| r.id), Some(41012));
}

#[tokio::test]
async fn migration_v41_is_in_the_list() {
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 41, "{latest}");
}
