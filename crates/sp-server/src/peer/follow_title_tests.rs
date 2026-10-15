//! #269: a node follows its peers' titles — the lyrics worker's tick takes
//! a peer's title this node should take, one video per look, over two real
//! nodes (`peer::rig`), and the pure decisions on their own.

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

use super::*;
use crate::db::models_peer::{fetch_record, record_fetch};
use crate::downloader::cache::{audio_filename, video_filename};
use crate::peer::client::{PeerError, lock};
use crate::peer::config::{NodeConfig, PeerConfig};
use crate::peer::follow::{Candidate, FOLLOW_IDLE, FOLLOW_RETRY};
use crate::peer::hash::sha256_hex;
use crate::peer::kind::{ArtifactKind, Job};
use crate::peer::lan::ExchangeStatus;
use crate::peer::rig::{SNV_KEY, TestNode, set};
use crate::peer::wire::{Artifact, Catalog, now_ms};

const YT: &str = "aaaaaaaaaaa";
const YT2: &str = "bbbbbbbbbbb";
/// The title both nodes start with: a provider's (`TestNode::give_song`).
const SONG: &str = "Way Maker";
const ARTIST: &str = "Sinach";
/// SNV's operator's correction.
const LIVE: &str = "Way Maker (Live)";
/// PP's own operator's correction.
const MINE: &str = "Way Maker (PP)";

/// SNV and PP with the songs of `youtube_ids` downloaded under the same
/// provider's title, SNV PP's listed peer.
async fn snv_and_pp(youtube_ids: &[&str]) -> (TestNode, TestNode) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    for yt in youtube_ids {
        let id = snv.add_video(yt).await;
        snv.give_song(id, yt, SONG, ARTIST).await;
        let id = pp.add_video(yt).await;
        pp.give_song(id, yt, SONG, ARTIST).await;
    }
    (snv, pp)
}

/// A second row of `yt` at PP, in playlist 2, sharing the first one's files.
async fn second_row(pp: &TestNode, yt: &str) {
    let id = pp.add_video_to(2, yt).await;
    pp.give_song(id, yt, SONG, ARTIST).await;
}

/// `node`'s rows of `yt`, by id.
async fn rows_of(node: &TestNode, yt: &str) -> Vec<i64> {
    sqlx::query_scalar("SELECT id FROM videos WHERE youtube_id = ? ORDER BY id")
        .bind(yt)
        .fetch_all(node.pool())
        .await
        .unwrap()
}

/// An operator's correction of `yt` at `node`, as `PATCH /api/v1/videos/{id}`
/// writes it: its first row named `song` (`'manual'`), then spread to every
/// row with the files renamed (`apply_to_video`, the route's own step).
async fn correct(node: &TestNode, yt: &str, song: &str) {
    let id = rows_of(node, yt).await[0];
    sqlx::query(
        "UPDATE videos SET song = ?, gemini_failed = 0, metadata_source = 'manual' WHERE id = ?",
    )
    .bind(song)
    .bind(id)
    .execute(node.pool())
    .await
    .unwrap();
    crate::metadata::manual::apply_to_video(node.pool(), node.cache(), id)
        .await
        .unwrap();
}

/// A row's title and files: `(song, artist, metadata_source, gemini_failed,
/// file_path, audio_file_path)`.
type Named = (String, Option<String>, Option<String>, i64, String, String);

/// Every row of `yt` at `node` is titled `song` (with `ARTIST`) by `source`,
/// its files on disk under that title.
async fn every_row_named(node: &TestNode, yt: &str, song: &str, source: &str) {
    let video = node.cache().join(video_filename(song, ARTIST, yt, false));
    let audio = node.cache().join(audio_filename(song, ARTIST, yt, false));
    let rows: Vec<Named> = sqlx::query_as(
        "SELECT song, artist, metadata_source, gemini_failed, file_path, audio_file_path \
         FROM videos WHERE youtube_id = ? ORDER BY id",
    )
    .bind(yt)
    .fetch_all(node.pool())
    .await
    .unwrap();
    assert!(!rows.is_empty());
    let want: Named = (
        song.to_string(),
        Some(ARTIST.to_string()),
        Some(source.to_string()),
        0,
        video.to_string_lossy().into_owned(),
        audio.to_string_lossy().into_owned(),
    );
    for row in rows {
        assert_eq!(row, want);
    }
    assert!(video.is_file(), "{}", video.display());
    assert!(audio.is_file(), "{}", audio.display());
}

/// The title `node` records as taken from a peer: `(node, version, sha256)`.
async fn took(node: &TestNode, yt: &str) -> Option<(String, i64, String)> {
    fetch_record(node.pool(), yt, "metadata").await.unwrap()
}

/// The sha256 of `yt`'s title as `node`'s catalog lists it.
async fn listed_sha(node: &TestNode, yt: &str) -> String {
    let titles = crate::peer::catalog::metadata_for(node.pool(), Some(yt))
        .await
        .unwrap();
    sha256_hex(&titles[0].to_bytes())
}

/// PP's configured peer `snv`.
async fn snv_peer(pp: &TestNode) -> PeerConfig {
    let cfg = NodeConfig::load(pp.pool()).await.unwrap();
    cfg.peer("snv").unwrap().clone()
}

/// The next look of `node` may run now.
fn due_now(node: &TestNode) {
    lock(&node.ex.title_follow).next_look_ms = 0;
}

/// The next look of `node` may run now and passes nothing over.
fn retry_now(node: &TestNode) {
    let mut state = lock(&node.ex.title_follow);
    state.next_look_ms = 0;
    state.passed.clear();
}

/// The title follow passes `yt` over for `d` from a look between `before`
/// and `after`.
fn passed_for(node: &TestNode, yt: &str, d: Duration, before: i64, after: i64) -> bool {
    let until = lock(&node.ex.title_follow).passed[yt].until_ms;
    (before + ms(d)..=after + ms(d)).contains(&until)
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap()
}

/// The owner's case: an operator corrects a title at SNV. The next look at
/// PP puts it into EVERY row of the video (`'manual'`, its rank kept), the
/// files renamed after it, records it as the copy taken from SNV (the very
/// bytes SNV's catalog lists), and the status names it.
#[tokio::test]
async fn an_operators_correction_at_the_peer_goes_into_every_row() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    second_row(&pp, YT).await;
    let old_video = pp.cache().join(video_filename(SONG, ARTIST, YT, false));
    correct(&snv, YT, LIVE).await;

    let rows = follow_next(Some(&pp.ex)).await;

    assert_eq!(rows, rows_of(&pp, YT).await, "every row of the video");
    assert_eq!(rows.len(), 2);
    every_row_named(&pp, YT, LIVE, "manual").await;
    assert!(!old_video.exists(), "the files moved");
    let sha = listed_sha(&snv, YT).await;
    assert_eq!(
        took(&pp, YT).await,
        Some(("snv".to_string(), 2, sha.clone()))
    );
    let held = held_titles(pp.pool(), Some(YT)).await.unwrap();
    assert_eq!(held[0].sha256, sha, "PP's title is SNV's very bytes");
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
    assert!(String::from_utf8_lossy(&body).contains(r#""why":"higher""#));
    let status: ExchangeStatus = serde_json::from_slice(&body).unwrap();
    assert_eq!(status.title_follow.followed, 1);
    let last = status.title_follow.last.unwrap();
    let got = (last.youtube_id.as_str(), last.peer.as_str(), last.why);
    assert_eq!(got, (YT, "snv", TitleWhy::Higher));
    assert_eq!(status.title_follow.last_error, None);
    assert_eq!(status.title_follow.wanted, 1, "the look found it");
    assert_eq!(status.follow.followed, 0, "the lyrics follow is apart");
}

/// The copy taken is wanted no more; a later correction at SNV (here back
/// to the old title) follows, because PP's title IS the copy it recorded.
#[tokio::test]
async fn the_peers_next_correction_follows_and_a_revert_follows_back() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    correct(&snv, YT, LIVE).await;
    assert_eq!(follow_next(Some(&pp.ex)).await.len(), 1);
    retry_now(&pp);
    let before = now_ms();
    assert!(follow_next(Some(&pp.ex)).await.is_empty(), "nothing new");
    assert!(lock(&pp.ex.title_follow).next_look_ms >= before + ms(FOLLOW_IDLE));
    every_row_named(&pp, YT, LIVE, "manual").await;

    correct(&snv, YT, SONG).await;
    pp.ex.client.forget_catalog("snv");
    due_now(&pp);
    assert_eq!(follow_next(Some(&pp.ex)).await, rows_of(&pp, YT).await);

    every_row_named(&pp, YT, SONG, "manual").await;
    let sha = listed_sha(&snv, YT).await;
    assert_eq!(took(&pp, YT).await, Some(("snv".to_string(), 2, sha)));
    let status = pp.ex.title_follow_status();
    assert_eq!(status.followed, 2);
    assert_eq!(status.last.unwrap().why, TitleWhy::Changed);
}

/// A correction made at PP is final: SNV's never replaces it, nor does a
/// later one when PP had taken SNV's title before its own operator
/// corrected it (the record names another title than PP's).
#[tokio::test]
async fn a_correction_made_here_stays() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    correct(&pp, YT, MINE).await;
    correct(&snv, YT, LIVE).await;
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    every_row_named(&pp, YT, MINE, "manual").await;
    assert_eq!(took(&pp, YT).await, None);

    let sha = listed_sha(&snv, YT).await;
    record_fetch(pp.pool(), YT, "metadata", "snv", 2, &sha, 1_000)
        .await
        .unwrap();
    correct(&snv, YT, "Way Maker (Studio)").await;
    pp.ex.client.forget_catalog("snv");
    due_now(&pp);
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    every_row_named(&pp, YT, MINE, "manual").await;
    assert_eq!(pp.ex.title_follow_status().followed, 0);
}

/// A provider's title at PP of its own (no record) stays against SNV's
/// provider title: an equal rank. A parser's title at PP (outside the
/// repair queue: no provider was configured) takes it, with SNV's label.
#[tokio::test]
async fn an_equal_rank_stays_and_a_higher_one_is_taken() {
    let (_snv, pp) = snv_and_pp(&[YT]).await;
    sqlx::query("UPDATE videos SET song = 'Way Makers' WHERE youtube_id = ?")
        .bind(YT)
        .execute(pp.pool())
        .await
        .unwrap();
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    let song: String = sqlx::query_scalar("SELECT song FROM videos WHERE youtube_id = ?")
        .bind(YT)
        .fetch_one(pp.pool())
        .await
        .unwrap();
    assert_eq!(song, "Way Makers");

    sqlx::query("UPDATE videos SET metadata_source = 'regex' WHERE youtube_id = ?")
        .bind(YT)
        .execute(pp.pool())
        .await
        .unwrap();
    due_now(&pp);
    assert_eq!(follow_next(Some(&pp.ex)).await.len(), 1);
    every_row_named(&pp, YT, SONG, "gemini").await;
    let last = pp.ex.title_follow_status().last.unwrap();
    assert_eq!(last.why, TitleWhy::Higher);
    let (node, version, _) = took(&pp, YT).await.unwrap();
    assert_eq!((node.as_str(), version), ("snv", 1));
}

/// A file the player holds (Windows refuses to rename it; here a directory
/// under the new name refuses the move on every platform): nothing is
/// written, no record, no error, and the look after `FOLLOW_RETRY` takes it.
#[tokio::test]
async fn a_busy_file_is_retried_on_a_later_look() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    correct(&snv, YT, LIVE).await;
    let blocked = pp.cache().join(video_filename(LIVE, ARTIST, YT, false));
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(blocked.join("blocker"), b"x").unwrap();

    let before = now_ms();
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    let after = now_ms();

    every_row_named(&pp, YT, SONG, "gemini").await;
    assert_eq!(took(&pp, YT).await, None);
    assert!(passed_for(&pp, YT, FOLLOW_RETRY, before, after));
    assert_eq!(pp.ex.title_follow_status().last_error, None);
    assert_eq!(pp.ex.title_follow_status().followed, 0);

    std::fs::remove_dir_all(&blocked).unwrap();
    retry_now(&pp);
    assert_eq!(follow_next(Some(&pp.ex)).await.len(), 1);
    every_row_named(&pp, YT, LIVE, "manual").await;
    assert!(took(&pp, YT).await.is_some());
}

/// The operator's pause and #230's hold: nothing is read or taken; once
/// both end, the next tick follows.
#[tokio::test]
async fn nothing_while_paused_or_held() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    correct(&snv, YT, LIVE).await;
    set(pp.pool(), "peer_transfers_paused", "true").await;
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    assert!(pp.ex.client.last_reads().is_empty(), "no catalog read");
    set(pp.pool(), "peer_transfers_paused", "false").await;
    crate::background_hold::hold_for_a_minute(pp.pool()).await;
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    assert!(pp.ex.client.last_reads().is_empty(), "no catalog read");
    assert_eq!(pp.ex.title_follow_status().last_look, None);
    every_row_named(&pp, YT, SONG, "gemini").await;
    crate::background_hold::end_hold(pp.pool()).await;
    assert_eq!(follow_next(Some(&pp.ex)).await.len(), 1);
    every_row_named(&pp, YT, LIVE, "manual").await;
}

/// SNV lists no peer: its follow reads nothing and touches nothing (and no
/// exchange wired, nothing at all).
#[tokio::test]
async fn a_node_that_asks_no_peer_follows_nothing() {
    let (snv, _pp) = snv_and_pp(&[YT]).await;
    assert!(follow_next(Some(&snv.ex)).await.is_empty());
    assert!(snv.ex.client.last_reads().is_empty(), "no catalog read");
    assert_eq!(snv.ex.title_follow_status().last_look, None);
    assert!(
        lock(&snv.ex.title_follow).next_look_ms > 0,
        "looked at again later"
    );
    assert!(follow_next(None).await.is_empty());
}

/// One video per look; a look that finds nothing makes the next one wait
/// `FOLLOW_IDLE`.
#[tokio::test]
async fn one_video_per_look_then_the_idle_wait() {
    let (snv, pp) = snv_and_pp(&[YT, YT2]).await;
    correct(&snv, YT2, LIVE).await;
    correct(&snv, YT, LIVE).await;
    assert_eq!(follow_next(Some(&pp.ex)).await, rows_of(&pp, YT).await);
    every_row_named(&pp, YT, LIVE, "manual").await;
    every_row_named(&pp, YT2, SONG, "gemini").await;
    assert_eq!(follow_next(Some(&pp.ex)).await, rows_of(&pp, YT2).await);
    every_row_named(&pp, YT2, LIVE, "manual").await;
    let before = now_ms();
    assert!(follow_next(Some(&pp.ex)).await.is_empty(), "nothing left");
    assert!(lock(&pp.ex.title_follow).next_look_ms >= before + ms(FOLLOW_IDLE));
    assert_eq!(pp.ex.title_follow_status().followed, 2);
}

/// A row in PP's metadata repair queue is the repair's (it takes a peer's
/// title first): the follow leaves the whole video to it.
#[tokio::test]
async fn a_video_in_the_repair_queue_is_left_to_the_repair() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    second_row(&pp, YT).await;
    correct(&snv, YT, LIVE).await;
    let queued = rows_of(&pp, YT).await[1];
    sqlx::query("UPDATE videos SET gemini_failed = 1, metadata_source = 'regex' WHERE id = ?")
        .bind(queued)
        .execute(pp.pool())
        .await
        .unwrap();
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    let first = rows_of(&pp, YT).await[0];
    let title: (String, Option<String>) =
        sqlx::query_as("SELECT song, metadata_source FROM videos WHERE id = ?")
            .bind(first)
            .fetch_one(pp.pool())
            .await
            .unwrap();
    assert_eq!(title, (SONG.to_string(), Some("gemini".to_string())));
}

/// PP's operator corrects the song between the look and the take: the
/// take checks the look's answer again under the song-files lock and
/// leaves the correction alone.
#[tokio::test]
async fn a_correction_made_here_during_the_take_stays() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    correct(&snv, YT, LIVE).await;
    let held = held_titles(pp.pool(), None).await.unwrap();
    let peer = snv_peer(&pp).await;
    let catalog = pp.ex.client.catalog(&peer).await.unwrap();
    let found = candidates(&held, &[("snv", Some(&*catalog))]);
    assert_eq!(found.len(), 1);

    correct(&pp, YT, MINE).await;
    let outcome = take(&pp.ex, &found[0], &peer).await;

    assert!(matches!(outcome, Outcome::Later(_)), "{outcome:?}");
    every_row_named(&pp, YT, MINE, "manual").await;
    assert_eq!(took(&pp, YT).await, None);
}

/// The take renames and writes only while it holds `cache::SONG_FILES` (a
/// title PATCH, the repair and the re-link hold it too): with the lock held
/// nothing moves and no title is written; once it is free the look ends
/// with the peer's title in every row. The 300 ms window can only fail a
/// take that writes without the lock.
#[tokio::test]
async fn the_take_waits_for_the_song_files_lock() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    second_row(&pp, YT).await;
    correct(&snv, YT, LIVE).await;

    let held = crate::downloader::cache::SONG_FILES.lock().await;
    let ex = pp.ex.clone();
    let run = tokio::spawn(async move { follow_next(Some(&ex)).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!run.is_finished(), "the take waits for the lock");
    every_row_named(&pp, YT, SONG, "gemini").await;
    assert_eq!(took(&pp, YT).await, None);
    drop(held);
    let rows = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(rows, rows_of(&pp, YT).await);
    every_row_named(&pp, YT, LIVE, "manual").await;
}

/// ROZHODNUTÉ (#269): PP's title is byte for byte SNV's with no record of
/// it (a phase-0 copy of an operator's title). The look records SNV as its
/// origin and takes nothing: no row, no rename. Recorded once, and SNV's
/// later correction then follows.
#[tokio::test]
async fn a_byte_identical_title_is_recorded_then_followed() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    correct(&snv, YT, LIVE).await;
    correct(&pp, YT, LIVE).await;
    assert_eq!(took(&pp, YT).await, None, "a phase-0 copy: no record");

    assert!(follow_next(Some(&pp.ex)).await.is_empty(), "nothing taken");

    every_row_named(&pp, YT, LIVE, "manual").await;
    let sha = listed_sha(&snv, YT).await;
    assert_eq!(took(&pp, YT).await, Some(("snv".to_string(), 2, sha)));
    let status = pp.ex.title_follow_status();
    assert_eq!(status.last.unwrap().why, TitleWhy::Same);
    let before = now_ms();
    assert!(follow_next(Some(&pp.ex)).await.is_empty(), "recorded once");
    assert!(lock(&pp.ex.title_follow).next_look_ms >= before + ms(FOLLOW_IDLE));

    correct(&snv, YT, "Way Maker (Studio)").await;
    pp.ex.client.forget_catalog("snv");
    due_now(&pp);
    assert_eq!(follow_next(Some(&pp.ex)).await, rows_of(&pp, YT).await);
    every_row_named(&pp, YT, "Way Maker (Studio)", "manual").await;
    assert_eq!(
        pp.ex.title_follow_status().last.unwrap().why,
        TitleWhy::Changed
    );
}

/// A title byte for byte SNV's that PP's operator changes between the look
/// and the record is no copy of SNV's: nothing is recorded, and the
/// correction stays PP's own.
#[tokio::test]
async fn a_title_changed_before_its_record_is_not_recorded() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    let held = held_titles(pp.pool(), None).await.unwrap();
    let peer = snv_peer(&pp).await;
    let catalog = pp.ex.client.catalog(&peer).await.unwrap();
    let found = candidates(&held, &[("snv", Some(&*catalog))]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].why, TitleWhy::Same, "both start with one title");

    correct(&pp, YT, MINE).await;
    let outcome = take(&pp.ex, &found[0], &peer).await;

    assert!(matches!(outcome, Outcome::Later(_)), "{outcome:?}");
    assert_eq!(took(&pp, YT).await, None);
    correct(&snv, YT, LIVE).await;
    pp.ex.client.forget_catalog("snv");
    assert!(follow_next(Some(&pp.ex)).await.is_empty());
    every_row_named(&pp, YT, MINE, "manual").await;
}

/// ROZHODNUTÉ (#269): a download run here (a local re-download) keeps the
/// title's record while PP's title is still the copy it took: SNV's later
/// correction still follows. The pair's own records go, as before.
#[tokio::test]
async fn a_download_here_keeps_the_record_of_the_title_it_keeps() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    correct(&snv, YT, LIVE).await;
    assert_eq!(follow_next(Some(&pp.ex)).await.len(), 1);
    pp.audio_from(YT, "snv").await;

    let _guard = pp.ex.run_here(Job::Download, YT).await;

    let sha = listed_sha(&snv, YT).await;
    assert_eq!(took(&pp, YT).await, Some(("snv".to_string(), 2, sha)));
    assert_eq!(fetch_record(pp.pool(), YT, "audio").await.unwrap(), None);
    correct(&snv, YT, "Way Maker (Studio)").await;
    pp.ex.client.forget_catalog("snv");
    retry_now(&pp);
    assert_eq!(follow_next(Some(&pp.ex)).await.len(), 1);
    every_row_named(&pp, YT, "Way Maker (Studio)", "manual").await;
}

/// …and forgets it once PP's title is no longer that copy: the title the
/// record names changed here.
#[tokio::test]
async fn a_download_here_forgets_the_record_of_a_changed_title() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    correct(&snv, YT, LIVE).await;
    assert_eq!(follow_next(Some(&pp.ex)).await.len(), 1);
    correct(&pp, YT, MINE).await;

    let _guard = pp.ex.run_here(Job::Download, YT).await;

    assert_eq!(took(&pp, YT).await, None);
}

/// The peer's row must be the title its catalog lists: a read between two
/// corrections there is not taken (a failure, retried); a fresh catalog
/// lists the newer title, taken at once.
#[tokio::test]
async fn a_title_that_no_longer_matches_the_catalog_is_not_taken() {
    let (snv, pp) = snv_and_pp(&[YT]).await;
    correct(&snv, YT, LIVE).await;
    pp.ex.client.catalog(&snv_peer(&pp).await).await.unwrap();
    correct(&snv, YT, "Way Maker (Studio)").await;

    assert!(follow_next(Some(&pp.ex)).await.is_empty());

    every_row_named(&pp, YT, SONG, "gemini").await;
    assert_eq!(took(&pp, YT).await, None);
    let error = pp.ex.title_follow_status().last_error.unwrap();
    assert_eq!(error.youtube_id, YT);
    assert!(error.error.contains("sha256 mismatch"), "{}", error.error);
    pp.ex.client.forget_catalog("snv");
    assert_eq!(follow_next(Some(&pp.ex)).await.len(), 1);
    every_row_named(&pp, YT, "Way Maker (Studio)", "manual").await;
}

/// What the follow holds of PP's videos: a downloaded one out of the repair
/// queue, with its title's version and sha256 as a catalog lists them (here
/// SNV's: the same title) and its record; never a video not downloaded, in
/// the repair queue, removed from its playlist or the test item.
#[tokio::test]
async fn the_held_titles_are_the_downloaded_videos_out_of_the_repair_queue() {
    let (snv, pp) = snv_and_pp(&[YT, YT2]).await;
    record_fetch(pp.pool(), YT, "metadata", "snv", 1, "recorded", 1_000)
        .await
        .unwrap();
    sqlx::query("UPDATE videos SET removed_at = 1760000000000 WHERE youtube_id = ?")
        .bind(YT2)
        .execute(pp.pool())
        .await
        .unwrap();
    pp.add_video("ccccccccccc").await;
    let queued = pp.add_video("ddddddddddd").await;
    pp.give_song(queued, "ddddddddddd", SONG, ARTIST).await;
    sqlx::query("UPDATE videos SET gemini_failed = 1, metadata_source = 'regex' WHERE id = ?")
        .bind(queued)
        .execute(pp.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, kind) VALUES (9, 't', 'u', 'test')")
        .execute(pp.pool())
        .await
        .unwrap();
    let test_item = pp.add_video_to(9, "eeeeeeeeeee").await;
    pp.give_song(test_item, "eeeeeeeeeee", SONG, ARTIST).await;

    let held = held_titles(pp.pool(), None).await.unwrap();

    let want = HeldTitle {
        youtube_id: YT.to_string(),
        version: 1,
        sha256: listed_sha(&snv, YT).await,
        took: Some(("snv".to_string(), "recorded".to_string())),
    };
    assert_eq!(held.len(), 1, "one held title: {held:?}");
    assert_eq!(held[0], want);
    assert_eq!(held_titles(pp.pool(), Some(YT)).await.unwrap(), [want]);
    let none = held_titles(pp.pool(), Some("ddddddddddd")).await.unwrap();
    assert!(none.is_empty());
}

// ---- the pure decisions ----

fn sha(seed: &str) -> String {
    sha256_hex(seed.as_bytes())
}

/// The title of `yt` a catalog lists, at `version`, with the sha of `seed`.
fn title_of(yt: &str, version: u32, seed: &str) -> Artifact {
    Artifact {
        youtube_id: yt.into(),
        kind: ArtifactKind::Metadata,
        version,
        size: 100,
        sha256: sha(seed),
        updated_at: None,
    }
}

/// This node's title of `yt` at `version`, with the sha of `seed`, and the
/// record `(node, seed)` of the copy it took.
fn held_of(yt: &str, version: u32, seed: &str, took: Option<(&str, &str)>) -> HeldTitle {
    HeldTitle {
        youtube_id: yt.into(),
        version,
        sha256: sha(seed),
        took: took.map(|(node, seed)| (node.to_string(), sha(seed))),
    }
}

/// One case of [`wants`]: this node's title, the peer's, the answer.
type WantsCase = (HeldTitle, Artifact, Option<TitleWhy>);

#[test]
fn wants_follows_a_higher_rank_or_the_peers_newer_copy_never_a_correction_made_here() {
    let h = |version, seed, took| held_of(YT, version, seed, took);
    let t = |version, seed| title_of(YT, version, seed);
    let cases: [WantsCase; 18] = [
        // A higher rank: a provider's over a parser's, an operator's over a
        // provider's.
        (h(0, "mine", None), t(1, "snv"), Some(TitleWhy::Higher)),
        (h(1, "mine", None), t(2, "snv"), Some(TitleWhy::Higher)),
        // The copy taken from that peer, a new title there.
        (
            h(1, "was", Some(("snv", "was"))),
            t(1, "now"),
            Some(TitleWhy::Changed),
        ),
        (h(1, "now", Some(("snv", "now"))), t(1, "now"), None),
        // An equal rank not from that peer stays.
        (h(1, "mine", None), t(1, "snv"), None),
        (h(1, "was", Some(("pp2", "was"))), t(1, "now"), None),
        // A correction made here stays: no record, a record of another
        // title, another peer's record.
        (h(2, "mine", None), t(2, "snv"), None),
        (h(2, "mine", Some(("snv", "was"))), t(2, "now"), None),
        (h(2, "was", Some(("pp2", "was"))), t(2, "now"), None),
        // Even under a rank a newer peer may list one day above an
        // operator's.
        (h(2, "mine", None), t(3, "snv"), None),
        // An operator's title that IS the recorded copy follows.
        (
            h(2, "was", Some(("snv", "was"))),
            t(2, "now"),
            Some(TitleWhy::Changed),
        ),
        // A parser's title there is never taken.
        (h(0, "was", Some(("snv", "was"))), t(0, "now"), None),
        (h(0, "mine", None), t(0, "snv"), None),
        // ROZHODNUTÉ: byte for byte the peer's title with no record of it
        // (a phase-0 operator's title, a provider's, another peer's record,
        // a record of an older title) is recorded; never a parser's.
        (h(2, "same", None), t(2, "same"), Some(TitleWhy::Same)),
        (h(1, "same", None), t(1, "same"), Some(TitleWhy::Same)),
        (
            h(2, "same", Some(("pp2", "same"))),
            t(2, "same"),
            Some(TitleWhy::Same),
        ),
        (
            h(2, "same", Some(("snv", "was"))),
            t(2, "same"),
            Some(TitleWhy::Same),
        ),
        (h(0, "same", None), t(0, "same"), None),
    ];
    for (held, listed, want) in cases {
        assert_eq!(wants(&held, "snv", &listed), want, "{held:?} {listed:?}");
    }
}

#[test]
fn candidates_come_from_the_first_peer_listing_a_title_this_node_takes() {
    let ccc = "ccccccccccc";
    let lyrics = Artifact {
        kind: ArtifactKind::Lyrics,
        version: crate::lyrics::LYRICS_PIPELINE_VERSION,
        ..title_of(YT, 0, "lyrics")
    };
    let parser = Catalog {
        node: "snv".into(),
        artifacts: vec![
            lyrics,
            title_of(YT, 0, "guess"),
            title_of("eeeeeeeeeee", 2, "other"),
        ],
        jobs: Vec::new(),
    };
    let good = Catalog {
        node: "pp2".into(),
        artifacts: vec![
            title_of(YT, 2, "operator"),
            title_of(YT2, 1, "now"),
            title_of(ccc, 1, "now"),
        ],
        jobs: Vec::new(),
    };
    let reads = [("x", None), ("snv", Some(&parser)), ("pp2", Some(&good))];
    let held_now = [
        held_of(ccc, 1, "was", Some(("pp2", "was"))),
        held_of(YT2, 1, "was", Some(("pp2", "was"))),
        held_of(YT, 1, "mine", None),
        held_of("ddddddddddd", 0, "mine", None),
    ];
    let found = candidates(&held_now, &reads);
    let got: Vec<(&str, &str, TitleWhy)> = found
        .iter()
        .map(|c| (c.youtube_id.as_str(), c.peer.as_str(), c.why))
        .collect();
    assert_eq!(
        got,
        [
            (YT, "pp2", TitleWhy::Higher),
            (YT2, "pp2", TitleWhy::Changed),
            (ccc, "pp2", TitleWhy::Changed),
        ]
    );
    assert_eq!(found[0].artifact, title_of(YT, 2, "operator"));
}

/// The title follow's memory is the lyrics follow's machinery over
/// `TitleWhy`: one taken, the rest counted, a failure named and its video
/// passed over.
#[test]
fn a_follow_state_of_titles_takes_one_and_counts_the_rest() {
    let mut state = crate::peer::follow::FollowState::<TitleWhy>::default();
    let one = Candidate {
        youtube_id: YT.into(),
        peer: "snv".into(),
        why: TitleWhy::Changed,
        artifact: title_of(YT, 2, "now"),
    };
    let two = Candidate {
        youtube_id: YT2.into(),
        ..one.clone()
    };
    assert_eq!(
        state.pick(&[one.clone(), two.clone()], 1_000),
        Some(one.clone())
    );
    assert_eq!((state.status.wanted, state.status.passed_over), (2, 0));
    state.failed(&one, 2_000, &PeerError::NotFound);
    assert_eq!(state.pick(&[one, two.clone()], 3_000), Some(two));
    assert_eq!((state.status.wanted, state.status.passed_over), (1, 1));
    assert_eq!(state.status.last_error.unwrap().youtube_id, YT);
}
