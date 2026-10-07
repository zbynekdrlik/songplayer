//! #229: the lyrics job asks first.

use super::*;
use crate::db::models::VideoLyricsRow;
use crate::db::models_peer::fetch_record;
use crate::peer::kind::Job;
use crate::peer::rig::{SNV_KEY, TestNode};

const YT: &str = "aaaaaaaaaaa";
const ROW_SQL: &str = "SELECT v.id, v.youtube_id, COALESCE(v.song, '') AS song, \
     COALESCE(v.artist, '') AS artist, v.duration_ms, v.audio_file_path, p.youtube_url, \
     v.lyrics_override_text, v.lyrics_time_offset_ms, v.spotify_track_id, v.spotify_resolved_at \
     FROM videos v JOIN playlists p ON p.id = v.playlist_id WHERE v.id = ?";

async fn lyrics_row(node: &TestNode, id: i64) -> VideoLyricsRow {
    sqlx::query_as(ROW_SQL)
        .bind(id)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

#[derive(Debug, sqlx::FromRow)]
struct LyricsNow {
    has_lyrics: i64,
    lyrics_source: Option<String>,
    lyrics_pipeline_version: i64,
    lyrics_alignment_model: Option<String>,
    lyrics_reference: i64,
    lyrics_translation_version: i64,
    lyrics_translation_gender: Option<String>,
    lyrics_attempts: i64,
    lyrics_next_attempt_at: Option<String>,
}

async fn lyrics_now(node: &TestNode, id: i64) -> LyricsNow {
    sqlx::query_as(
        "SELECT has_lyrics, lyrics_source, lyrics_pipeline_version, lyrics_alignment_model, \
                lyrics_reference, lyrics_translation_version, lyrics_translation_gender, \
                lyrics_attempts, lyrics_next_attempt_at FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(node.pool())
    .await
    .unwrap()
}

/// SNV serves `mtl+g35t` lyrics with ★, SK translated (v2, masculine), aligned
/// by `mtl`; PP has the song, no lyrics, and asks SNV. Returns SNV's JSON
/// bytes.
async fn snv_and_pp() -> (TestNode, TestNode, i64, Vec<u8>) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    let json = snv.give_lyrics(snv_id, YT, "mtl+g35t").await;
    sqlx::query(
        "UPDATE videos SET lyrics_reference = 1, lyrics_translation_version = 2, \
         lyrics_translation_gender = 'm', lyrics_alignment_model = 'mtl' WHERE id = ?",
    )
    .bind(snv_id)
    .execute(snv.pool())
    .await
    .unwrap();
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    (snv, pp, id, json)
}

fn json_at(node: &TestNode) -> Option<Vec<u8>> {
    std::fs::read(node.cache().join(format!("{YT}_lyrics.json"))).ok()
}

/// No part of a fetch left behind (the dir may not exist when nothing was
/// fetched).
fn parts_left(node: &TestNode) -> usize {
    std::fs::read_dir(node.ex.parts_dir()).map_or(0, |d| d.count())
}

#[tokio::test]
async fn a_peers_lyrics_are_taken_with_their_row() {
    let (_snv, pp, id, json) = snv_and_pp().await;
    let row = lyrics_row(&pp, id).await;
    assert!(matches!(first(Some(&pp.ex), &row).await, PeerStep::Done));
    assert_eq!(json_at(&pp), Some(json));
    let now = lyrics_now(&pp, id).await;
    assert_eq!(
        (now.has_lyrics, now.lyrics_source.as_deref()),
        (1, Some("mtl+g35t"))
    );
    assert_eq!(
        now.lyrics_pipeline_version,
        i64::from(crate::lyrics::LYRICS_PIPELINE_VERSION)
    );
    assert_eq!(now.lyrics_alignment_model.as_deref(), Some("mtl"));
    assert_eq!(now.lyrics_reference, 1, "the ★ comes along");
    assert_eq!(
        (
            now.lyrics_translation_version,
            now.lyrics_translation_gender.as_deref()
        ),
        (2, Some("m")),
        "a row with no gender takes the peer's, its SK lines' gender"
    );
    let (node, _, _) = fetch_record(pp.pool(), YT, "lyrics")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(node, "snv");
    assert_eq!(parts_left(&pp), 0);
}

#[tokio::test]
async fn the_same_translation_gender_here_keeps_the_translation() {
    let (_snv, pp, id, _) = snv_and_pp().await;
    sqlx::query("UPDATE videos SET lyrics_translation_gender = 'm' WHERE id = ?")
        .bind(id)
        .execute(pp.pool())
        .await
        .unwrap();
    let row = lyrics_row(&pp, id).await;
    assert!(matches!(first(Some(&pp.ex), &row).await, PeerStep::Done));
    let now = lyrics_now(&pp, id).await;
    assert_eq!(
        (
            now.lyrics_translation_version,
            now.lyrics_translation_gender.as_deref()
        ),
        (2, Some("m"))
    );
}

#[tokio::test]
async fn another_translation_gender_here_asks_for_a_local_retranslation() {
    let (_snv, pp, id, _) = snv_and_pp().await;
    sqlx::query("UPDATE videos SET lyrics_translation_gender = 'f' WHERE id = ?")
        .bind(id)
        .execute(pp.pool())
        .await
        .unwrap();
    let row = lyrics_row(&pp, id).await;
    assert!(matches!(first(Some(&pp.ex), &row).await, PeerStep::Done));
    let now = lyrics_now(&pp, id).await;
    assert_eq!(
        (
            now.lyrics_translation_version,
            now.lyrics_translation_gender.as_deref()
        ),
        (0, Some("f"))
    );
}

#[tokio::test]
async fn an_operators_ask_here_is_never_answered_by_a_peer() {
    let (_snv, pp, id, _) = snv_and_pp().await;
    sqlx::query("UPDATE videos SET lyrics_manual_priority = 1 WHERE id = ?")
        .bind(id)
        .execute(pp.pool())
        .await
        .unwrap();
    let PeerStep::Local(Some(_guard)) = first(Some(&pp.ex), &lyrics_row(&pp, id).await).await
    else {
        panic!("a reprocess asked here runs here")
    };
    sqlx::query(
        "UPDATE videos SET lyrics_manual_priority = 0, lyrics_override_text = 'Moj text' WHERE id = ?",
    )
    .bind(id)
    .execute(pp.pool())
    .await
    .unwrap();
    let PeerStep::Local(Some(_guard)) = first(Some(&pp.ex), &lyrics_row(&pp, id).await).await
    else {
        panic!("an operator's text runs here")
    };
    assert_eq!(json_at(&pp), None, "nothing was fetched");
    assert!(pp.ex.client.last_reads().is_empty(), "no peer was asked");
    sqlx::query("UPDATE videos SET lyrics_override_text = '   ' WHERE id = ?")
        .bind(id)
        .execute(pp.pool())
        .await
        .unwrap();
    assert!(
        matches!(
            first(Some(&pp.ex), &lyrics_row(&pp, id).await).await,
            PeerStep::Done
        ),
        "a blank override is no operator's text"
    );
}

/// Review Focus 5 here: this node's `{yt}_lyrics.json` of a dubbed video is
/// the dub's subtitles, never overwritten by a peer's lyrics.
#[tokio::test]
async fn a_dubbed_video_here_is_never_answered_by_a_peer() {
    let (_snv, pp, id, _) = snv_and_pp().await;
    let dub = pp.add_video_to(2, YT).await;
    sqlx::query("UPDATE videos SET dub_requested = 1 WHERE id = ?")
        .bind(dub)
        .execute(pp.pool())
        .await
        .unwrap();
    std::fs::write(
        pp.cache().join(format!("{YT}_lyrics.json")),
        b"dub-subtitles",
    )
    .unwrap();
    let PeerStep::Local(Some(_guard)) = first(Some(&pp.ex), &lyrics_row(&pp, id).await).await
    else {
        panic!("a dubbed video's lyrics run here")
    };
    assert_eq!(json_at(&pp), Some(b"dub-subtitles".to_vec()));
    sqlx::query(
        "UPDATE videos SET dub_requested = 0, lyrics_source = 'gemini-live-translate' WHERE id = ?",
    )
    .bind(dub)
    .execute(pp.pool())
    .await
    .unwrap();
    let PeerStep::Local(Some(_guard)) = first(Some(&pp.ex), &lyrics_row(&pp, id).await).await
    else {
        panic!("a Live-Translate track here runs here")
    };
    assert_eq!(json_at(&pp), Some(b"dub-subtitles".to_vec()));
}

/// A job that runs here ends any earlier wait of it, so a later ask of the
/// same video and job never inherits an old start (and with it the 2 h bound
/// already spent).
#[tokio::test]
async fn an_operators_ask_here_ends_an_earlier_wait() {
    let (_snv, pp, id, _) = snv_and_pp().await;
    crate::db::models_peer::start_wait(pp.pool(), YT, "lyrics", 1_000)
        .await
        .unwrap();
    sqlx::query("UPDATE videos SET lyrics_manual_priority = 1 WHERE id = ?")
        .bind(id)
        .execute(pp.pool())
        .await
        .unwrap();
    let PeerStep::Local(Some(_guard)) = first(Some(&pp.ex), &lyrics_row(&pp, id).await).await
    else {
        panic!("a reprocess asked here runs here")
    };
    let waits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM peer_waits")
        .fetch_one(pp.pool())
        .await
        .unwrap();
    assert_eq!(waits, 0);
}

/// A peer's lyrics part is read whole: one over 16 MiB is refused before any
/// transfer.
#[test]
fn a_lyrics_track_over_16_mib_is_not_fetched() {
    assert!(lyrics_size_ok(0));
    assert!(lyrics_size_ok(16 * 1024 * 1024));
    assert!(!lyrics_size_ok(16 * 1024 * 1024 + 1));
}

/// A peer's source of any length is reported cut to the bounded error size.
#[tokio::test]
async fn a_source_mismatch_is_reported_bounded() {
    let (snv, pp, id, _) = snv_and_pp().await;
    let long = "x".repeat(2_000);
    sqlx::query("UPDATE videos SET lyrics_source = ?")
        .bind(&long)
        .execute(snv.pool())
        .await
        .unwrap();
    let Ask::Fetch(plan) = pp.ex.ask(Job::Lyrics, YT).await else {
        panic!("expected Fetch")
    };
    let err = adopt(&pp.ex, &lyrics_row(&pp, id).await, &plan)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("is not the row's"), "{err}");
    assert!(
        err.chars().count() <= 320,
        "{} characters",
        err.chars().count()
    );
}

#[tokio::test]
async fn the_same_track_already_served_here_is_nothing_newer() {
    let (_snv, pp, id, _) = snv_and_pp().await;
    crate::db::models_peer::start_wait(pp.pool(), YT, "lyrics", 1_000)
        .await
        .unwrap();
    pp.give_lyrics(id, YT, "mtl+g35t").await;
    std::fs::write(
        pp.cache().join(format!("{YT}_lyrics.json")),
        b"local-marker",
    )
    .unwrap();
    let row = lyrics_row(&pp, id).await;
    let PeerStep::Local(Some(_guard)) = first(Some(&pp.ex), &row).await else {
        panic!("the same source at the same version runs here (the daily full-mix upgrade)")
    };
    assert_eq!(
        json_at(&pp),
        Some(b"local-marker".to_vec()),
        "the local file is untouched"
    );
    assert_eq!(parts_left(&pp), 0, "nothing was fetched");
    assert_eq!(
        pp.ex.board.snapshot("pp").len(),
        1,
        "the job runs here, announced"
    );
    let waits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM peer_waits")
        .fetch_one(pp.pool())
        .await
        .unwrap();
    assert_eq!(waits, 0, "running here ends the earlier wait");
}

#[tokio::test]
async fn a_stale_copy_here_is_replaced_by_the_peers_current_one() {
    let (_snv, pp, id, json) = snv_and_pp().await;
    pp.give_lyrics(id, YT, "mtl+g35t").await;
    sqlx::query(
        "UPDATE videos SET lyrics_pipeline_version = lyrics_pipeline_version - 1 WHERE id = ?",
    )
    .bind(id)
    .execute(pp.pool())
    .await
    .unwrap();
    let row = lyrics_row(&pp, id).await;
    assert!(matches!(first(Some(&pp.ex), &row).await, PeerStep::Done));
    assert_eq!(json_at(&pp), Some(json));
    assert_eq!(
        lyrics_now(&pp, id).await.lyrics_pipeline_version,
        i64::from(crate::lyrics::LYRICS_PIPELINE_VERSION)
    );
}

#[tokio::test]
async fn another_track_here_at_the_current_version_is_replaced_by_the_peers() {
    let (_snv, pp, id, json) = snv_and_pp().await;
    pp.give_lyrics(id, YT, "gemini-3-5-transcribe/fullmix")
        .await;
    let row = lyrics_row(&pp, id).await;
    assert!(matches!(first(Some(&pp.ex), &row).await, PeerStep::Done));
    assert_eq!(json_at(&pp), Some(json));
    assert_eq!(
        lyrics_now(&pp, id).await.lyrics_source.as_deref(),
        Some("mtl+g35t")
    );
}

/// Review Focus 5 at the adopting end.
#[tokio::test]
async fn a_track_whose_source_differs_from_the_row_is_refused() {
    let (snv, pp, id, _) = snv_and_pp().await;
    let mut track: sp_core::lyrics::LyricsTrack =
        serde_json::from_slice(&json_at(&snv).unwrap()).unwrap();
    track.source = crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE.into();
    std::fs::write(
        snv.cache().join(format!("{YT}_lyrics.json")),
        serde_json::to_vec(&track).unwrap(),
    )
    .unwrap();
    sqlx::query("DELETE FROM peer_hashes")
        .execute(snv.pool())
        .await
        .unwrap();
    snv.hash_now().await;
    let row = lyrics_row(&pp, id).await;
    assert!(matches!(
        first(Some(&pp.ex), &row).await,
        PeerStep::Deferred
    ));
    assert_eq!(json_at(&pp), None);
    let now = lyrics_now(&pp, id).await;
    assert_eq!((now.has_lyrics, now.lyrics_attempts), (0, 0));
    assert!(now.lyrics_next_attempt_at.is_some(), "re-picked later");
    assert_eq!(parts_left(&pp), 0, "the refused part is dropped");
}

#[tokio::test]
async fn a_peers_row_that_does_not_match_its_catalog_is_refused() {
    let (_snv, pp, id, _) = snv_and_pp().await;
    let Ask::Fetch(mut plan) = pp.ex.ask(Job::Lyrics, YT).await else {
        panic!("expected Fetch")
    };
    plan.artifacts[0].version += 1;
    let row = lyrics_row(&pp, id).await;
    assert!(adopt(&pp.ex, &row, &plan).await.is_err());
    assert_eq!(json_at(&pp), None);
    assert_eq!(parts_left(&pp), 0, "nothing was fetched");
}

#[tokio::test]
async fn a_peer_running_the_lyrics_job_defers_the_row_without_an_attempt() {
    let (snv, pp, _, _) = snv_and_pp().await;
    let _running = snv.ex.announce("bbbbbbbbbbb", Job::Lyrics);
    let id = pp.add_video("bbbbbbbbbbb").await;
    pp.give_song(id, "bbbbbbbbbbb", "Iny", "Zbor").await;
    let row = lyrics_row(&pp, id).await;
    assert!(matches!(
        first(Some(&pp.ex), &row).await,
        PeerStep::Deferred
    ));
    let now = lyrics_now(&pp, id).await;
    assert_eq!((now.has_lyrics, now.lyrics_attempts), (0, 0));
    let next = chrono::DateTime::parse_from_rfc3339(&now.lyrics_next_attempt_at.unwrap()).unwrap();
    let ahead = (next.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
    assert!((100..=125).contains(&ahead), "{ahead}");
}

#[tokio::test]
async fn with_no_exchange_the_worker_runs_as_before() {
    let (_snv, pp, id, _) = snv_and_pp().await;
    let row = lyrics_row(&pp, id).await;
    assert!(matches!(first(None, &row).await, PeerStep::Local(None)));
}
