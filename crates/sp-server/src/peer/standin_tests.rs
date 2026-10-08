//! #229 (PP audit, comment 6054582866): a lyrics track made here while a
//! listed peer had the song stands in for the peer's copy, which replaces it
//! once the peer has one.

use std::time::Duration;

use super::*;
use crate::db::models_peer::{fetch_record, record_standin, start_wait};
use crate::peer::PeerStep;
use crate::peer::ask::Ask;
use crate::peer::client::PeerError;
use crate::peer::decide::MAX_PEER_WAIT;
use crate::peer::rig::{SNV_KEY, TestNode, set};

const YT: &str = "aaaaaaaaaaa";
/// A source PP's own (degraded) track carries.
const PP_SOURCE: &str = "gemini-3-5-transcribe";

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct Standin {
    youtube_id: String,
    job: String,
    peer: String,
    made_at_ms: i64,
    next_check_ms: i64,
}

async fn standins(node: &TestNode) -> Vec<Standin> {
    sqlx::query_as("SELECT * FROM peer_standins ORDER BY youtube_id, job")
        .fetch_all(node.pool())
        .await
        .unwrap()
}

async fn lyrics_source(node: &TestNode, id: i64) -> (i64, Option<String>) {
    sqlx::query_as("SELECT has_lyrics, lyrics_source FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

fn json_at(node: &TestNode) -> Option<Vec<u8>> {
    std::fs::read(node.cache().join(format!("{YT}_lyrics.json"))).ok()
}

/// SNV has the song (hashed); PP has it too, its audio fetched from SNV, in
/// playlists 1 and 2 (two rows of one video), both rows with PP's own track,
/// and a stand-in of it due now. Returns the nodes, PP's rows and PP's JSON.
async fn snv_and_pp_standin() -> (TestNode, TestNode, [i64; 2], Vec<u8>) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let one = pp.add_video(YT).await;
    let two = pp.add_video_to(2, YT).await;
    pp.give_song(one, YT, "Way Maker", "Sinach").await;
    pp.give_song(two, YT, "Way Maker", "Sinach").await;
    pp.audio_from(YT, "snv").await;
    pp.give_lyrics(two, YT, PP_SOURCE).await;
    let json = pp.give_lyrics(one, YT, PP_SOURCE).await;
    record_standin(pp.pool(), YT, "lyrics", "snv", 1_000, 0)
        .await
        .unwrap();
    (snv, pp, [one, two], json)
}

/// SNV's lyrics of the song, `mtl+g35t` with the ★ (hashed); its JSON.
async fn snv_lyrics(snv: &TestNode) -> Vec<u8> {
    let id: i64 = sqlx::query_scalar("SELECT id FROM videos WHERE youtube_id = ?")
        .bind(YT)
        .fetch_one(snv.pool())
        .await
        .unwrap();
    let json = snv.give_lyrics(id, YT, "mtl+g35t").await;
    sqlx::query("UPDATE videos SET lyrics_reference = 1 WHERE id = ?")
        .bind(id)
        .execute(snv.pool())
        .await
        .unwrap();
    snv.hash_now().await;
    json
}

/// The peer's copy replaces the stand-in: the file, and every row of the
/// video takes the peer's lyrics columns (the file is the video's); the
/// origin is recorded and the stand-in is over.
#[tokio::test]
async fn a_stand_in_is_replaced_by_the_peers_copy_with_every_row() {
    let (snv, pp, rows, _) = snv_and_pp_standin().await;
    let snv_json = snv_lyrics(&snv).await;
    assert_eq!(supersede_next(Some(&pp.ex)).await, rows.to_vec());
    assert_eq!(json_at(&pp), Some(snv_json));
    for id in rows {
        assert_eq!(
            lyrics_source(&pp, id).await,
            (1, Some("mtl+g35t".to_string()))
        );
        let reference: i64 = sqlx::query_scalar("SELECT lyrics_reference FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(pp.pool())
            .await
            .unwrap();
        assert_eq!(reference, 1, "the ★ comes along");
    }
    let (node, _, _) = fetch_record(pp.pool(), YT, "lyrics")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(node, "snv");
    assert!(standins(&pp).await.is_empty());
    assert!(supersede_next(Some(&pp.ex)).await.is_empty(), "done once");
}

/// A row this node's track came from failed and parked (`no_source`) stands
/// in the same way: the peer's copy is taken.
#[tokio::test]
async fn a_parked_stand_in_takes_the_peers_copy_too() {
    let (snv, pp, rows, _) = snv_and_pp_standin().await;
    sqlx::query("UPDATE videos SET has_lyrics = 0, lyrics_source = 'no_source'")
        .execute(pp.pool())
        .await
        .unwrap();
    snv_lyrics(&snv).await;
    assert_eq!(supersede_next(Some(&pp.ex)).await, rows.to_vec());
    assert_eq!(
        lyrics_source(&pp, rows[0]).await,
        (1, Some("mtl+g35t".to_string()))
    );
}

/// The peer has no lyrics of the song yet: nothing changes, and the
/// stand-in is looked at again after its recheck — a quarter of its age,
/// here 4 h → 1 h — not on the next tick.
#[tokio::test]
async fn a_stand_in_waits_for_the_peer_and_is_rescheduled() {
    let (snv, pp, rows, json) = snv_and_pp_standin().await;
    snv.hash_now().await;
    let made = crate::peer::wire::now_ms() - 4 * 3_600_000;
    record_standin(pp.pool(), YT, "lyrics", "snv", made, 0)
        .await
        .unwrap();
    let before = crate::peer::wire::now_ms();
    assert!(supersede_next(Some(&pp.ex)).await.is_empty());
    let after = crate::peer::wire::now_ms();
    assert_eq!(json_at(&pp), Some(json), "the track here stays");
    assert_eq!(
        lyrics_source(&pp, rows[0]).await,
        (1, Some(PP_SOURCE.to_string()))
    );
    let s = standins(&pp).await;
    assert_eq!(s.len(), 1, "kept: {s:?}");
    assert_eq!((s[0].made_at_ms, s[0].peer.as_str()), (made, "snv"));
    // A quarter of its age (4 h and the few ms since): 1 h after the look.
    let lo = before + 3_600_000;
    let hi = after + (after - made) / 4;
    assert!((lo..=hi).contains(&s[0].next_check_ms), "{s:?}");
    // Not due: the peer's copy now on offer is not looked at before then.
    snv_lyrics(&snv).await;
    assert!(supersede_next(Some(&pp.ex)).await.is_empty());
    assert_eq!(
        lyrics_source(&pp, rows[0]).await,
        (1, Some(PP_SOURCE.to_string()))
    );
}

/// The peer's copy is made from another audio than this node's (PP's audio
/// is its own encode: no record, another hash): it never fits, so the track
/// here is kept for good and the stand-in dropped.
#[tokio::test]
async fn a_stand_in_whose_peer_copy_is_of_another_audio_is_kept_for_good() {
    let (snv, pp, rows, json) = snv_and_pp_standin().await;
    snv_lyrics(&snv).await;
    sqlx::query("DELETE FROM peer_fetches")
        .execute(pp.pool())
        .await
        .unwrap();
    let audio: String = sqlx::query_scalar("SELECT audio_file_path FROM videos WHERE id = ?")
        .bind(rows[0])
        .fetch_one(pp.pool())
        .await
        .unwrap();
    std::fs::write(&audio, crate::peer::rig::bytes(3_000, 9)).unwrap();
    assert!(supersede_next(Some(&pp.ex)).await.is_empty());
    assert_eq!(json_at(&pp), Some(json));
    assert!(standins(&pp).await.is_empty());
}

/// An operator's text or ask, or a dub, on any row of the video: the track
/// here is theirs, kept for good, and the peer is not asked.
#[tokio::test]
async fn a_stand_in_of_a_video_an_operator_or_a_dub_owns_is_kept_for_good() {
    for owner in [
        "lyrics_override_text = 'Moj text'",
        "lyrics_manual_priority = 1",
        "dub_requested = 1",
        "lyrics_source = 'gemini-live-translate'",
    ] {
        let (snv, pp, rows, json) = snv_and_pp_standin().await;
        snv_lyrics(&snv).await;
        sqlx::query(&format!("UPDATE videos SET {owner} WHERE id = ?"))
            .bind(rows[1])
            .execute(pp.pool())
            .await
            .unwrap();
        assert!(supersede_next(Some(&pp.ex)).await.is_empty(), "{owner}");
        assert_eq!(json_at(&pp), Some(json), "{owner}");
        assert!(standins(&pp).await.is_empty(), "{owner}");
        assert!(
            pp.ex.client.last_reads().is_empty(),
            "{owner}: no peer asked"
        );
    }
}

/// No row of the video is left here (removed from its playlists): nothing
/// to replace, the stand-in is dropped.
#[tokio::test]
async fn a_stand_in_with_no_row_left_is_dropped() {
    let (_snv, pp, _, _) = snv_and_pp_standin().await;
    sqlx::query("DELETE FROM videos")
        .execute(pp.pool())
        .await
        .unwrap();
    assert!(supersede_next(Some(&pp.ex)).await.is_empty());
    assert!(standins(&pp).await.is_empty());
}

/// A worker with no exchange has no stand-ins.
#[tokio::test]
async fn with_no_exchange_nothing_is_superseded() {
    assert!(supersede_next(None).await.is_empty());
}

#[test]
fn a_stand_in_is_looked_at_every_quarter_of_its_age_10_min_to_6_h() {
    let m = |n: u64| Duration::from_secs(n * 60);
    assert_eq!(standin_recheck(Duration::ZERO), m(10));
    assert_eq!(standin_recheck(m(40)), m(10), "the floor exactly");
    assert_eq!(standin_recheck(m(80)), m(20));
    assert_eq!(
        standin_recheck(m(24 * 60)),
        m(6 * 60),
        "the ceiling exactly"
    );
    assert_eq!(standin_recheck(m(48 * 60)), m(6 * 60));
    assert_eq!(
        (STANDIN_MIN_RECHECK, STANDIN_MAX_RECHECK),
        (m(10), m(6 * 60))
    );
}

/// The bound case of the PP audit: SNV has the song but announces no lyrics
/// job; PP waited the 2 h and makes its own track, which stands in for
/// SNV's (first looked at 10 min later). A job no peer had, and the stems,
/// stand in for nothing.
#[tokio::test]
async fn a_lyrics_job_run_here_after_waiting_for_a_peer_with_the_song_stands_in() {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    set(snv.pool(), "lyrics_worker_enabled", "false").await;
    set(snv.pool(), "stem_worker_enabled", "false").await;
    let id = snv.add_video(YT).await;
    snv.give_song(id, YT, "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let long_ago = crate::peer::wire::now_ms() - i64::try_from(MAX_PEER_WAIT.as_millis()).unwrap();
    for job in ["lyrics", "stems"] {
        start_wait(pp.pool(), YT, job, long_ago).await.unwrap();
    }
    let Ask::Local(_guard) = pp.ex.ask(Job::Stems, YT).await else {
        panic!("the stems run here after the bound")
    };
    assert!(
        standins(&pp).await.is_empty(),
        "the stems stand in for nothing"
    );
    let before = crate::peer::wire::now_ms();
    let Ask::Local(_guard) = pp.ex.ask(Job::Lyrics, YT).await else {
        panic!("the lyrics run here after the bound")
    };
    let s = standins(&pp).await;
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(
        (
            s[0].youtube_id.as_str(),
            s[0].job.as_str(),
            s[0].peer.as_str()
        ),
        (YT, "lyrics", "snv")
    );
    assert!(s[0].made_at_ms >= before, "{s:?}");
    assert_eq!(s[0].next_check_ms - s[0].made_at_ms, 600_000);
    let Ask::Local(_guard) = pp.ex.ask(Job::Lyrics, "ccccccccccc").await else {
        panic!("a song no peer has runs here")
    };
    assert_eq!(standins(&pp).await.len(), 1, "nobody had that one");
}

/// A peer's copy that kept failing for the 2 h: the lyrics run here and
/// stand in for it (the peer has the song); the stems do not.
#[tokio::test]
async fn a_lyrics_job_run_here_after_a_failing_fetch_stands_in() {
    let (_snv, pp, rows, _) = snv_and_pp_standin().await;
    sqlx::query("DELETE FROM peer_standins")
        .execute(pp.pool())
        .await
        .unwrap();
    let long_ago = crate::peer::wire::now_ms() - i64::try_from(MAX_PEER_WAIT.as_millis()).unwrap();
    for job in ["lyrics", "stems"] {
        start_wait(pp.pool(), YT, job, long_ago).await.unwrap();
    }
    let step = pp
        .ex
        .after_failed_fetch(Job::Stems, rows[0], YT, "snv", &PeerError::NotFound)
        .await;
    assert!(matches!(step, PeerStep::Local(Some(_))));
    assert!(standins(&pp).await.is_empty());
    let step = pp
        .ex
        .after_failed_fetch(Job::Lyrics, rows[0], YT, "snv", &PeerError::NotFound)
        .await;
    assert!(matches!(step, PeerStep::Local(Some(_))));
    let s = standins(&pp).await;
    assert_eq!(
        s.iter()
            .map(|s| (s.job.as_str(), s.peer.as_str()))
            .collect::<Vec<_>>(),
        [("lyrics", "snv")]
    );
}

/// A job that runs here for another reason (an operator's ask, another
/// audio, nothing newer: `run_here`) ends the stand-in; so does a peer's
/// copy taken through the hooks (`fetched`).
#[tokio::test]
async fn running_here_or_taking_the_peers_copy_ends_the_stand_in() {
    let (_snv, pp, _, _) = snv_and_pp_standin().await;
    let _guard = pp.ex.run_here(Job::Lyrics, YT).await;
    assert!(standins(&pp).await.is_empty());
    record_standin(pp.pool(), YT, "lyrics", "snv", 1_000, 0)
        .await
        .unwrap();
    pp.ex.fetched(Job::Lyrics, YT, "snv", &[]).await;
    assert!(standins(&pp).await.is_empty());
}
