//! V31 (#229 PP audit, comment 6054582866): the stand-ins, back-filled once.

use super::MIGRATIONS;
use super::test_helpers::{apply_first_n, apply_upto, column_names};
use super::*;

/// One video per case: its rows (`(lyrics_source, extra SET)`), whether a
/// peer gave its audio and its lyrics; whether V31 makes it a stand-in.
type Case = (
    &'static str,
    &'static [(Option<&'static str>, &'static str)],
    bool,
    bool,
    bool,
);

/// PP's state when V31 lands: a song whose pair came from SNV and whose
/// lyrics PP made itself (served or parked) stands in for SNV's copy, due at
/// once (`8ohdO2nINEI`). Not: lyrics taken from the peer, a song this node
/// downloaded itself, no lyrics made yet, a dub or a Live-Translate track
/// on any row of the video, or an operator's mark the lyrics queue acts on:
/// a text on a row of an ACTIVE playlist, or a reprocess flag on such a row
/// that is not parked (review rounds 7-8: a flag on an inactive playlist's
/// row or on a parked row, `asr_gap` here, is never taken, and the worker
/// never makes an inactive row's lyrics). Playlist 2, every second row's,
/// is inactive.
#[tokio::test]
async fn migration_v31_creates_the_stand_ins_and_back_fills_them() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 30).await;
    // A video's rows sit in different playlists (one row per playlist).
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, is_active) \
         VALUES (1, 'p', 'u', 1), (2, 'q', 'v', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let cases: [Case; 13] = [
        (
            "served00001",
            &[(Some("gemini-3-5-transcribe"), "")],
            true,
            false,
            true,
        ),
        ("parked00001", &[(Some("no_source"), "")], true, false, true),
        (
            "tworows0001",
            &[(None, ""), (Some("gemini-3-5-transcribe"), "")],
            true,
            false,
            true,
        ),
        ("fromsnv0001", &[(Some("mtl+g35t"), "")], true, true, false),
        (
            "ownaudio001",
            &[(Some("gemini-3-5-transcribe"), "")],
            false,
            false,
            false,
        ),
        ("nolyrics001", &[(None, "")], true, false, false),
        (
            "override001",
            &[
                (None, ", lyrics_override_text = 'Moj text'"),
                (Some("gemini-3-5-transcribe"), ""),
            ],
            true,
            false,
            false,
        ),
        (
            "overrideoff",
            &[
                (Some("gemini-3-5-transcribe"), ""),
                (None, ", lyrics_override_text = 'Moj text'"),
            ],
            true,
            false,
            true,
        ),
        (
            "manualgap01",
            &[
                (Some("asr_gap"), ", lyrics_manual_priority = 1"),
                (Some("gemini-3-5-transcribe"), ""),
            ],
            true,
            false,
            true,
        ),
        (
            "manual00001",
            &[(
                Some("gemini-3-5-transcribe"),
                ", lyrics_manual_priority = 1",
            )],
            true,
            false,
            false,
        ),
        (
            "manualoff01",
            &[
                (Some("gemini-3-5-transcribe"), ""),
                (None, ", lyrics_manual_priority = 1"),
            ],
            true,
            false,
            true,
        ),
        (
            "livetrans01",
            &[
                (Some("gemini-3-5-transcribe"), ""),
                (Some("gemini-live-translate"), ""),
            ],
            true,
            false,
            false,
        ),
        (
            "dubbed00001",
            &[
                (Some("gemini-3-5-transcribe"), ""),
                (Some("gemini-live-translate"), ", dub_requested = 1"),
            ],
            true,
            false,
            false,
        ),
    ];
    for (youtube_id, rows, audio_from_snv, lyrics_from_snv, _) in cases {
        for (playlist, (source, extra)) in (1i64..).zip(rows) {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO videos (playlist_id, youtube_id, lyrics_source) VALUES (?, ?, ?) \
                 RETURNING id",
            )
            .bind(playlist)
            .bind(youtube_id)
            .bind(source)
            .fetch_one(&pool)
            .await
            .unwrap();
            if !extra.is_empty() {
                sqlx::query(&format!("UPDATE videos SET id = id{extra} WHERE id = ?"))
                    .bind(id)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }
        for (kind, from_snv) in [("audio", audio_from_snv), ("lyrics", lyrics_from_snv)] {
            if from_snv {
                sqlx::query(
                    "INSERT INTO peer_fetches (youtube_id, kind, node, version, sha256, \
                     fetched_at_ms) VALUES (?, ?, 'snv', 1, 'ab', 2)",
                )
                .bind(youtube_id)
                .bind(kind)
                .execute(&pool)
                .await
                .unwrap();
            }
        }
    }
    let before_ms = chrono::Utc::now().timestamp() * 1000;
    apply_upto(&pool, 31).await;
    assert_eq!(
        column_names(&pool, "peer_standins").await,
        vec!["youtube_id", "job", "peer", "made_at_ms", "next_check_ms"]
    );
    let rows: Vec<(String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT youtube_id, job, peer, made_at_ms, next_check_ms FROM peer_standins \
         ORDER BY youtube_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut want: Vec<&str> = cases.iter().filter(|c| c.4).map(|c| c.0).collect();
    want.sort_unstable();
    assert_eq!(rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), want);
    for (_, job, peer, made_at_ms, next_check_ms) in &rows {
        assert_eq!(
            (job.as_str(), peer.as_str(), *next_check_ms),
            ("lyrics", "snv", 0)
        );
        assert!(*made_at_ms >= before_ms, "made when V31 ran: {made_at_ms}");
    }
    let one = "INSERT INTO peer_standins (youtube_id, job, peer, made_at_ms, next_check_ms) \
               VALUES ('aaaaaaaaaaa', 'lyrics', 'snv', 1, 2)";
    sqlx::query(one).execute(&pool).await.unwrap();
    assert!(
        sqlx::query(one).execute(&pool).await.is_err(),
        "one stand-in per video + job"
    );
    assert_eq!(current_schema_version(&pool).await.unwrap(), 31);
}

#[tokio::test]
async fn migration_v31_advances_schema_version() {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 31, "V31 must be part of the migration list");
    assert_eq!(current_schema_version(&pool).await.unwrap(), latest);
}
