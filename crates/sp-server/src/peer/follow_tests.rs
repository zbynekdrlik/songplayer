//! #264: a node follows its peers' lyrics — the lyrics worker's tick takes
//! a peer's copy this node does not hold, one song per look, over two real
//! nodes (`peer::rig`), and the pure decisions on their own.

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

use super::*;
use crate::db::models_peer::{fetch_record, record_fetch, record_standin};
use crate::peer::client::{PeerError, lock};
use crate::peer::hash::sha256_hex;
use crate::peer::kind::ArtifactKind;
use crate::peer::lan::ExchangeStatus;
use crate::peer::rig::{SNV_KEY, TestNode, bytes, set};
use crate::peer::wire::{Artifact, Catalog, now_ms};

const YT: &str = "aaaaaaaaaaa";
const YT2: &str = "bbbbbbbbbbb";
/// The source of the copy PP took from SNV before SNV made it again.
const OLD_SOURCE: &str = "gemini-3-5-transcribe";
/// The source of SNV's lyrics now.
const NEW_SOURCE: &str = "mtl+g35t";
const VERSION: u32 = crate::lyrics::LYRICS_PIPELINE_VERSION;

/// SNV with the songs of `youtube_ids` and their lyrics (`NEW_SOURCE`,
/// hashed); PP with the same songs in playlist 1, each audio fetched from
/// SNV, and SNV as its peer. PP's lyrics are each test's own.
async fn snv_and_pp(youtube_ids: &[&str]) -> (TestNode, TestNode) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    for yt in youtube_ids {
        let id = snv.add_video(yt).await;
        snv.give_song(id, yt, "Way Maker", "Sinach").await;
        snv.give_lyrics(id, yt, NEW_SOURCE).await;
    }
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    for yt in youtube_ids {
        let id = pp.add_video(yt).await;
        pp.give_song(id, yt, "Way Maker", "Sinach").await;
        pp.audio_from(yt, "snv").await;
    }
    (snv, pp)
}

/// A second row of `yt` at PP, in playlist 2 (one video, two rows).
async fn second_row(pp: &TestNode, yt: &str) {
    let id = pp.add_video_to(2, yt).await;
    pp.give_song(id, yt, "Way Maker", "Sinach").await;
}

/// `node`'s rows of `yt`, by id.
async fn rows_of(node: &TestNode, yt: &str) -> Vec<i64> {
    sqlx::query_scalar("SELECT id FROM videos WHERE youtube_id = ? ORDER BY id")
        .bind(yt)
        .fetch_all(node.pool())
        .await
        .unwrap()
}

/// Row `id`'s lyrics as the wall serves them: `(has_lyrics, lyrics_source)`.
async fn served(node: &TestNode, id: i64) -> (i64, Option<String>) {
    sqlx::query_as("SELECT has_lyrics, lyrics_source FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

/// Every row of `yt` at `node` serves `source`.
async fn every_row_serves(node: &TestNode, yt: &str, source: &str) {
    for id in rows_of(node, yt).await {
        let row = served(node, id).await;
        assert_eq!(row, (1, Some(source.to_string())), "row {id}");
    }
}

fn json_of(node: &TestNode, yt: &str) -> Option<Vec<u8>> {
    std::fs::read(node.cache().join(format!("{yt}_lyrics.json"))).ok()
}

/// The sha256 of the lyrics copy `node` records as taken from a peer.
async fn took_sha(node: &TestNode, yt: &str) -> Option<String> {
    let record = fetch_record(node.pool(), yt, "lyrics").await.unwrap();
    record.map(|(_, _, sha256)| sha256)
}

/// PP's lyrics of `yt` (`source`), recorded as the copy it took from SNV;
/// returns its `{yt}_lyrics.json`.
async fn pp_took(pp: &TestNode, yt: &str, source: &str) -> Vec<u8> {
    let id = rows_of(pp, yt).await[0];
    let json = pp.give_lyrics(id, yt, source).await;
    let sha = sha256_hex(&json);
    record_fetch(pp.pool(), yt, "lyrics", "snv", VERSION, &sha, 1_000)
        .await
        .unwrap();
    json
}

/// Every row of `yt` at PP parked as `sentinel` at the current version.
async fn park(pp: &TestNode, yt: &str, sentinel: &str) {
    sqlx::query(
        "UPDATE videos SET has_lyrics = 0, lyrics_source = ?, lyrics_pipeline_version = ? \
         WHERE youtube_id = ?",
    )
    .bind(sentinel)
    .bind(i64::from(VERSION))
    .bind(yt)
    .execute(pp.pool())
    .await
    .unwrap();
}

/// The next look of `node` may run now.
fn due_now(node: &TestNode) {
    lock(&node.ex.follow).next_look_ms = 0;
}

/// The follow passes `yt` over for `d` from a look between `before` and
/// `after`.
fn passed_for(node: &TestNode, yt: &str, d: Duration, before: i64, after: i64) -> bool {
    let until = lock(&node.ex.follow).passed[yt].until_ms;
    (before + ms(d)..=after + ms(d)).contains(&until)
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap()
}

/// SNV made the song's lyrics again since PP took them: the next look puts
/// SNV's copy into EVERY row of the video (the file is the video's),
/// records it as the copy taken, and the status names it.
#[tokio::test]
async fn a_changed_copy_of_the_same_audio_goes_into_every_row() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    second_row(&pp, YT).await;
    pp_took(&pp, YT, OLD_SOURCE).await;
    let rows = follow_next(Some(&pp.ex)).await;
    assert_eq!(rows, rows_of(&pp, YT).await, "every row of the video");
    let snv_json = json_of(&snv, YT).unwrap();
    assert_eq!(json_of(&pp, YT), Some(snv_json.clone()));
    every_row_serves(&pp, YT, NEW_SOURCE).await;
    let (node, _, sha) = fetch_record(pp.pool(), YT, "lyrics")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((node.as_str(), sha), ("snv", sha256_hex(&snv_json)));
    // The status, through the exchange's own router.
    let req = Request::builder()
        .uri("/api/v1/exchange/status")
        .body(Body::empty())
        .unwrap();
    let resp = crate::peer::router(pp.ex.clone())
        .oneshot(req)
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains(r#""why":"changed""#));
    let status: ExchangeStatus = serde_json::from_slice(&body).unwrap();
    assert_eq!(status.follow.followed, 1);
    let last = status.follow.last.unwrap();
    let got = (last.youtube_id.as_str(), last.peer.as_str(), last.why);
    assert_eq!(got, (YT, "snv", FollowWhy::Changed));
    assert_eq!(status.follow.last_error, None);
    assert_eq!(status.follow.wanted, 1, "the look found it");
}

/// The record says PP holds the very copy SNV lists (here PP rewrote its
/// file since, as a retranslation for another gender does): nothing is
/// fetched, the file here stays, and the next look waits the idle time.
#[tokio::test]
async fn the_copy_this_node_took_is_not_fetched_again() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    let mine = pp
        .give_lyrics(rows_of(&pp, YT).await[0], YT, OLD_SOURCE)
        .await;
    let sha = sha256_hex(&json_of(&snv, YT).unwrap());
    record_fetch(pp.pool(), YT, "lyrics", "snv", VERSION, &sha, 1_000)
        .await
        .unwrap();
    let before = now_ms();
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    assert_eq!(json_of(&pp, YT), Some(mine), "no transfer");
    every_row_serves(&pp, YT, OLD_SOURCE).await;
    let state = lock(&pp.ex.follow);
    assert_eq!((state.status.wanted, state.status.followed), (0, 0));
    assert!(state.next_look_ms >= before + ms(FOLLOW_IDLE));
}

/// PP's audio is its own encode (no record, another hash): SNV's lyrics
/// were timed on another audio, so they are never taken, and the song is
/// passed over for `FOLLOW_KEPT`.
#[tokio::test]
async fn another_audio_is_never_taken() {
    let (_snv, pp) = snv_and_pp(&[YT]).await;
    park(&pp, YT, "asr_gap").await;
    sqlx::query("DELETE FROM peer_fetches")
        .execute(pp.pool())
        .await
        .unwrap();
    let audio: String =
        sqlx::query_scalar("SELECT audio_file_path FROM videos WHERE youtube_id = ?")
            .bind(YT)
            .fetch_one(pp.pool())
            .await
            .unwrap();
    std::fs::write(&audio, bytes(3_000, 9)).unwrap();
    let before = now_ms();
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    let after = now_ms();
    assert_eq!(json_of(&pp, YT), None);
    let row = served(&pp, rows_of(&pp, YT).await[0]).await;
    assert_eq!(row, (0, Some("asr_gap".to_string())));
    assert!(passed_for(&pp, YT, FOLLOW_KEPT, before, after));
    // The next look passes it over and takes nothing.
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    let status = pp.ex.follow_status();
    assert_eq!((status.wanted, status.passed_over), (0, 1));
    assert_eq!(json_of(&pp, YT), None);
}

/// The operator's pause: nothing is read or taken; once it ends, the next
/// tick follows (the pause schedules no wait).
#[tokio::test]
async fn nothing_while_this_nodes_transfers_are_paused() {
    let (_snv, pp) = snv_and_pp(&[YT]).await;
    let old = pp_took(&pp, YT, OLD_SOURCE).await;
    set(pp.pool(), "peer_transfers_paused", "true").await;
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    assert!(pp.ex.client.last_reads().is_empty(), "no catalog read");
    assert_eq!(json_of(&pp, YT), Some(old));
    assert_eq!(pp.ex.follow_status().last_look, None);
    set(pp.pool(), "peer_transfers_paused", "false").await;
    assert_eq!(follow_next(Some(&pp.ex)).await.len(), 1);
    every_row_serves(&pp, YT, NEW_SOURCE).await;
}

/// #230's background hold: the same — nothing read or taken while held.
#[tokio::test]
async fn nothing_while_the_background_is_held() {
    let (_snv, pp) = snv_and_pp(&[YT]).await;
    let old = pp_took(&pp, YT, OLD_SOURCE).await;
    crate::background_hold::hold_for_a_minute(pp.pool()).await;
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    assert!(pp.ex.client.last_reads().is_empty(), "no catalog read");
    assert_eq!(json_of(&pp, YT), Some(old));
    crate::background_hold::end_hold(pp.pool()).await;
    assert_eq!(follow_next(Some(&pp.ex)).await.len(), 1);
    every_row_serves(&pp, YT, NEW_SOURCE).await;
}

/// #263 point 3: a row parked here (in no lyrics bucket) while SNV serves
/// the video's lyrics is filled from SNV, every row of it.
#[tokio::test]
async fn a_parked_row_is_filled_from_the_peer() {
    for sentinel in ["asr_gap", "no_source", "unsupported_source"] {
        let (snv, pp) = snv_and_pp(&[YT]).await;
        second_row(&pp, YT).await;
        park(&pp, YT, sentinel).await;
        let rows = follow_next(Some(&pp.ex)).await;
        assert_eq!(rows.len(), 2, "{sentinel}");
        assert_eq!(json_of(&pp, YT), json_of(&snv, YT), "{sentinel}");
        every_row_serves(&pp, YT, NEW_SOURCE).await;
        let last = pp.ex.follow_status().last.unwrap();
        assert_eq!(last.why, FollowWhy::NoLyrics, "{sentinel}");
    }
}

/// SNV's copy cannot be taken (its file is not a lyrics track): the old
/// track stays on the wall — the file, every row, the record — the failure
/// is in the status, and the song is asked again after `FOLLOW_RETRY`.
#[tokio::test]
async fn a_failed_take_keeps_the_old_track_on_the_wall() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    let old = pp_took(&pp, YT, OLD_SOURCE).await;
    std::fs::write(
        snv.cache().join(format!("{YT}_lyrics.json")),
        b"not a track",
    )
    .unwrap();
    snv.hash_now().await;
    let before = now_ms();
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    let after = now_ms();
    assert_eq!(json_of(&pp, YT), Some(old.clone()));
    every_row_serves(&pp, YT, OLD_SOURCE).await;
    assert_eq!(took_sha(&pp, YT).await, Some(sha256_hex(&old)));
    let error = pp.ex.follow_status().last_error.unwrap();
    assert_eq!(
        (error.youtube_id.as_str(), error.peer.as_str()),
        (YT, "snv")
    );
    assert!(
        error.error.contains("not a lyrics track"),
        "{}",
        error.error
    );
    assert!(passed_for(&pp, YT, FOLLOW_RETRY, before, after));
    assert_eq!(pp.ex.follow_status().followed, 0);
}

/// A row that refuses the peer's columns (a test trigger): the copy is not
/// recorded as taken, so it is taken again once the rows accept it.
#[tokio::test]
async fn a_copy_not_every_row_took_is_not_recorded() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    second_row(&pp, YT).await;
    let old = pp_took(&pp, YT, OLD_SOURCE).await;
    sqlx::query(
        "CREATE TRIGGER refuse BEFORE UPDATE OF lyrics_source ON videos \
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .execute(pp.pool())
    .await
    .unwrap();
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    assert_eq!(took_sha(&pp, YT).await, Some(sha256_hex(&old)));
    assert!(pp.ex.follow_status().last_error.is_some());
    assert_eq!(pp.ex.follow_status().followed, 0);
    sqlx::query("DROP TRIGGER refuse")
        .execute(pp.pool())
        .await
        .unwrap();
    lock(&pp.ex.follow).passed.clear();
    assert_eq!(follow_next(Some(&pp.ex)).await.len(), 2);
    let snv_sha = sha256_hex(&json_of(&snv, YT).unwrap());
    assert_eq!(took_sha(&pp, YT).await, Some(snv_sha));
}

/// The copy went into every row but its origin could not be recorded (a
/// test trigger on `peer_fetches`): the song is not fetched again on every
/// tick; it waits `FOLLOW_RETRY` like a failed take.
#[tokio::test]
async fn a_copy_whose_record_failed_is_not_fetched_every_tick() {
    let (_snv, pp) = snv_and_pp(&[YT]).await;
    let old = pp_took(&pp, YT, OLD_SOURCE).await;
    sqlx::query(
        "CREATE TRIGGER refuse BEFORE INSERT ON peer_fetches \
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .execute(pp.pool())
    .await
    .unwrap();
    let before = now_ms();
    assert_eq!(follow_next(Some(&pp.ex)).await.len(), 1);
    let after = now_ms();
    every_row_serves(&pp, YT, NEW_SOURCE).await;
    assert_eq!(took_sha(&pp, YT).await, Some(sha256_hex(&old)), "no record");
    assert!(
        follow_next(Some(&pp.ex)).await.is_empty(),
        "not again at once"
    );
    assert!(passed_for(&pp, YT, FOLLOW_RETRY, before, after));
}

/// One song per look; a look that finds nothing makes the next one wait
/// `FOLLOW_IDLE`, after which a copy SNV made again is followed once more.
#[tokio::test]
async fn one_song_per_look_then_the_idle_wait() {
    let (snv, pp) = snv_and_pp(&[YT, YT2]).await;
    let old2 = pp_took(&pp, YT2, OLD_SOURCE).await;
    pp_took(&pp, YT, OLD_SOURCE).await;
    assert_eq!(follow_next(Some(&pp.ex)).await, rows_of(&pp, YT).await);
    every_row_serves(&pp, YT, NEW_SOURCE).await;
    assert_eq!(json_of(&pp, YT2), Some(old2), "one song per look");
    assert_eq!(follow_next(Some(&pp.ex)).await, rows_of(&pp, YT2).await);
    every_row_serves(&pp, YT2, NEW_SOURCE).await;
    let before = now_ms();
    assert!(follow_next(Some(&pp.ex)).await.is_empty(), "nothing left");
    assert!(lock(&pp.ex.follow).next_look_ms >= before + ms(FOLLOW_IDLE));
    // SNV makes YT's lyrics again: not looked at before the idle wait ends…
    let snv_id = rows_of(&snv, YT).await[0];
    let newest = snv.give_lyrics(snv_id, YT, "lrclib").await;
    snv.hash_now().await;
    pp.ex.client.forget_catalog("snv");
    assert!(follow_next(Some(&pp.ex)).await.is_empty(), "not due yet");
    every_row_serves(&pp, YT, NEW_SOURCE).await;
    // …then followed.
    due_now(&pp);
    assert_eq!(follow_next(Some(&pp.ex)).await, rows_of(&pp, YT).await);
    assert_eq!(json_of(&pp, YT), Some(newest));
    every_row_serves(&pp, YT, "lrclib").await;
    assert_eq!(pp.ex.follow_status().followed, 3);
}

/// A track no peer record names (a phase-0 copy, an old attempt here) is
/// this node's own while it makes lyrics itself (paid AI on); with paid AI
/// off it makes none, and the peer's copy replaces it.
#[tokio::test]
async fn an_untracked_track_follows_only_while_paid_ai_is_off() {
    let (_snv, pp) = snv_and_pp(&[YT]).await;
    let mine = pp
        .give_lyrics(rows_of(&pp, YT).await[0], YT, OLD_SOURCE)
        .await;
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    assert_eq!(json_of(&pp, YT), Some(mine));
    set(pp.pool(), "paid_ai_enabled", "false").await;
    due_now(&pp);
    assert_eq!(follow_next(Some(&pp.ex)).await.len(), 1);
    every_row_serves(&pp, YT, NEW_SOURCE).await;
    let last = pp.ex.follow_status().last.unwrap();
    assert_eq!(last.why, FollowWhy::Untracked);
}

/// SNV lists no peer: its follow reads nothing and touches nothing (and no
/// exchange wired, nothing at all).
#[tokio::test]
async fn a_node_that_asks_no_peer_follows_nothing() {
    let (snv, _pp) = snv_and_pp(&[YT]).await;
    park(&snv, YT, "asr_gap").await;
    assert!(follow_next(Some(&snv.ex)).await.is_empty());
    assert!(snv.ex.client.last_reads().is_empty(), "no catalog read");
    assert_eq!(snv.ex.follow_status().last_look, None);
    assert!(
        lock(&snv.ex.follow).next_look_ms > 0,
        "looked at again later"
    );
    assert!(follow_next(None).await.is_empty());
}

/// A song in this node's lyrics queue is the lyrics hook's, a stand-in the
/// stand-in's look's: the follow takes neither.
#[tokio::test]
async fn the_queue_and_a_stand_in_are_left_to_their_owners() {
    let (_snv, pp) = snv_and_pp(&[YT, YT2]).await;
    // YT: a copy taken from SNV, served under an older pipeline version (the
    // stale bucket); outside the queue it would be a `changed` copy.
    pp_took(&pp, YT, OLD_SOURCE).await;
    sqlx::query("UPDATE videos SET lyrics_pipeline_version = ? WHERE youtube_id = ?")
        .bind(i64::from(VERSION) - 1)
        .bind(YT)
        .execute(pp.pool())
        .await
        .unwrap();
    // YT2: a track made here that stands in for SNV's copy.
    pp_took(&pp, YT2, OLD_SOURCE).await;
    record_standin(pp.pool(), YT2, "lyrics", "snv", 1_000, i64::MAX)
        .await
        .unwrap();
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    assert_eq!(pp.ex.follow_status().wanted, 0);
    every_row_serves(&pp, YT, OLD_SOURCE).await;
    every_row_serves(&pp, YT2, OLD_SOURCE).await;
}

/// An operator's text here, or a Live-Translate track: the lyrics stay this
/// node's, passed over for `FOLLOW_KEPT`.
#[tokio::test]
async fn an_operators_lyrics_or_a_dub_track_stay_here() {
    for owner in [
        "lyrics_override_text = 'Moj text'",
        "lyrics_source = 'gemini-live-translate'",
    ] {
        let (_snv, pp) = snv_and_pp(&[YT]).await;
        let old = pp_took(&pp, YT, OLD_SOURCE).await;
        sqlx::query(&format!("UPDATE videos SET {owner} WHERE youtube_id = ?"))
            .bind(YT)
            .execute(pp.pool())
            .await
            .unwrap();
        let before = now_ms();
        assert!(follow_next(Some(&pp.ex)).await.is_empty(), "{owner}");
        let after = now_ms();
        assert_eq!(json_of(&pp, YT), Some(old), "{owner}");
        assert!(passed_for(&pp, YT, FOLLOW_KEPT, before, after), "{owner}");
    }
}

/// SNV made the song's lyrics again since PP took them, and PP carries an
/// operator's reprocess flag outside its lyrics queue for now (here on a
/// second row not downloaded yet: the queue takes it once it is, so the
/// flag asks for a run here then, and meanwhile the follow meets the
/// video). `paid_ai` is PP's switch. Returns PP's old JSON.
async fn flagged_outside_the_queue(paid_ai: &str) -> (TestNode, TestNode, Vec<u8>) {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    set(pp.pool(), "paid_ai_enabled", paid_ai).await;
    let old = pp_took(&pp, YT, OLD_SOURCE).await;
    second_row(&pp, YT).await;
    sqlx::query("UPDATE videos SET lyrics_manual_priority = 1, normalized = 0 WHERE id = ?")
        .bind(rows_of(&pp, YT).await[1])
        .execute(pp.pool())
        .await
        .unwrap();
    (snv, pp, old)
}

/// #264 (ROZHODNUTÉ 6185176417): with paid AI on, an operator's reprocess
/// flag keeps the lyrics this node's own (it asks for a new run here), so
/// the follow passes the song over for `FOLLOW_KEPT`; with paid AI off that
/// run cannot happen, so the follow takes the peer's copy into every row
/// and the take clears the flag.
#[tokio::test]
async fn a_reprocess_flag_keeps_the_lyrics_from_the_follow_only_while_paid_ai_is_on() {
    let (_snv, pp, old) = flagged_outside_the_queue("true").await;
    let before = now_ms();
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    let after = now_ms();
    assert_eq!(json_of(&pp, YT), Some(old));
    assert!(passed_for(&pp, YT, FOLLOW_KEPT, before, after));

    let (_snv, pp, _) = flagged_outside_the_queue("false").await;
    let rows = rows_of(&pp, YT).await;
    assert_eq!(follow_next(Some(&pp.ex)).await, rows);
    every_row_serves(&pp, YT, NEW_SOURCE).await;
    assert_eq!(
        pp.manual_flags(YT).await,
        vec![0, 0],
        "the take clears the flag"
    );
}

// ---- the pure decisions ----

fn sha(seed: &str) -> String {
    sha256_hex(seed.as_bytes())
}

/// A lyrics artifact of `yt` at `version` with the sha of `seed`.
fn lyrics(yt: &str, version: u32, seed: &str) -> Artifact {
    Artifact {
        youtube_id: yt.into(),
        kind: ArtifactKind::Lyrics,
        version,
        size: 100,
        sha256: sha(seed),
        updated_at: None,
    }
}

fn held(yt: &str, has_lyrics: bool, took: Option<(&str, &str)>) -> Held {
    Held {
        youtube_id: yt.into(),
        has_lyrics,
        took: took.map(|(node, seed)| (node.to_string(), sha(seed))),
    }
}

#[test]
fn wants_a_copy_this_node_does_not_hold() {
    let listed = lyrics(YT, VERSION, "now");
    let cases = [
        (
            held(YT, false, Some(("snv", "now"))),
            true,
            Some(FollowWhy::NoLyrics),
        ),
        (held(YT, false, None), false, Some(FollowWhy::NoLyrics)),
        (held(YT, true, Some(("snv", "now"))), true, None),
        (
            held(YT, true, Some(("snv", "then"))),
            false,
            Some(FollowWhy::Changed),
        ),
        (
            held(YT, true, Some(("pp2", "now"))),
            false,
            Some(FollowWhy::Changed),
        ),
        (held(YT, true, None), true, Some(FollowWhy::Untracked)),
        (held(YT, true, None), false, None),
    ];
    for (h, untracked, want) in cases {
        assert_eq!(
            wants(&h, "snv", &listed, untracked),
            want,
            "{h:?} {untracked}"
        );
    }
}

#[test]
fn candidates_come_from_the_first_peer_holding_them_in_order() {
    let ccc = "ccccccccccc";
    let older = Catalog {
        node: "snv".into(),
        artifacts: vec![lyrics(YT, VERSION - 1, "old")],
        jobs: Vec::new(),
    };
    let good = Catalog {
        node: "pp2".into(),
        artifacts: vec![
            lyrics(YT, VERSION, "now"),
            lyrics(YT2, VERSION, "now"),
            lyrics(ccc, VERSION, "now"),
        ],
        jobs: Vec::new(),
    };
    let reads = [("x", None), ("snv", Some(&older)), ("pp2", Some(&good))];
    let held_now = [
        held(ccc, true, Some(("pp2", "then"))),
        held(YT2, true, Some(("pp2", "then"))),
        held(YT, false, None),
        held("ddddddddddd", false, None),
    ];
    let found = candidates(&held_now, &reads, false);
    let got: Vec<(&str, &str, FollowWhy)> = found
        .iter()
        .map(|c| (c.youtube_id.as_str(), c.peer.as_str(), c.why))
        .collect();
    assert_eq!(
        got,
        [
            (YT, "pp2", FollowWhy::NoLyrics),
            (YT2, "pp2", FollowWhy::Changed),
            (ccc, "pp2", FollowWhy::Changed),
        ]
    );
    assert_eq!(found[0].artifact, lyrics(YT, VERSION, "now"));
}

fn candidate(yt: &str, seed: &str) -> Candidate {
    Candidate {
        youtube_id: yt.into(),
        peer: "snv".into(),
        why: FollowWhy::Changed,
        artifact: lyrics(yt, VERSION, seed),
    }
}

#[test]
fn a_look_takes_the_first_song_not_passed_over_and_counts_the_rest() {
    let mut state = FollowState::default();
    let (one, two) = (candidate(YT, "now"), candidate(YT2, "now"));
    state.pass(&one, 1_000, Duration::from_secs(10));
    assert_eq!(state.pick(&[one.clone(), two.clone()], 2_000), Some(two));
    assert_eq!((state.status.wanted, state.status.passed_over), (1, 1));
    assert!(state.due(2_000), "the next look is the next tick");
    assert!(state.status.last_look.is_some());
}

#[test]
fn a_passed_over_song_waits_its_time_or_a_new_copy() {
    let mut state = FollowState::default();
    let one = candidate(YT, "now");
    state.pass(&one, 1_000, Duration::from_secs(10));
    assert_eq!(state.pick(std::slice::from_ref(&one), 10_999), None);
    assert!(!state.due(10_999 + ms(FOLLOW_IDLE) - 1));
    assert!(state.due(10_999 + ms(FOLLOW_IDLE)));
    let another = candidate(YT, "newer");
    assert_eq!(
        state.pick(std::slice::from_ref(&another), 5_000),
        Some(another)
    );
    assert_eq!(state.pick(std::slice::from_ref(&one), 11_000), Some(one));
}

#[test]
fn the_status_counts_every_follow_and_names_the_last_failure() {
    let mut state = FollowState::default();
    let (one, two) = (candidate(YT, "now"), candidate(YT2, "now"));
    state.followed(&one, 1_000);
    state.followed(&two, 2_000);
    assert_eq!(state.status.followed, 2);
    assert_eq!(state.status.last.as_ref().unwrap().youtube_id, YT2);
    state.failed(&one, 3_000, &PeerError::NotFound);
    let error = state.status.last_error.clone().unwrap();
    assert_eq!(error.youtube_id, YT);
    assert_eq!(state.passed[YT].until_ms, 3_000 + ms(FOLLOW_RETRY));
}
