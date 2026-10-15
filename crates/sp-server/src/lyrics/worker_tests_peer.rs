//! #229: the lyrics worker asks its peers first.

use super::*;
use crate::peer::kind::Job;
use crate::peer::rig::{SNV_KEY, TestNode};

const YT: &str = "aaaaaaaaaaa";

fn worker(node: &TestNode) -> LyricsWorker {
    let (events, _) = broadcast::channel(16);
    LyricsWorker::new_for_test(node.pool().clone(), node.cache().to_path_buf(), events)
        .with_peer(node.ex.clone())
}

async fn lyrics_state(node: &TestNode, id: i64) -> (i64, i64, Option<String>) {
    sqlx::query_as(
        "SELECT has_lyrics, lyrics_attempts, lyrics_next_attempt_at FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(node.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn a_peers_lyrics_are_taken_by_the_lyrics_worker() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    snv.give_lyrics(snv_id, YT, "mtl+g35t").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    pp.audio_from(YT, "snv").await;
    let (events, mut rx) = broadcast::channel(16);
    LyricsWorker::new_for_test(pp.pool().clone(), pp.cache().to_path_buf(), events)
        .with_peer(pp.ex.clone())
        .process_next()
        .await;
    assert_eq!(lyrics_state(&pp, id).await.0, 1);
    assert!(pp.cache().join(format!("{YT}_lyrics.json")).exists());
    let mut completed = None;
    while let Ok(msg) = rx.try_recv() {
        if let ServerMsg::LyricsCompleted {
            video_id, source, ..
        } = msg
        {
            completed = Some((video_id, source));
        }
    }
    assert_eq!(
        completed,
        Some((id, "mtl+g35t".to_string())),
        "the dashboard hears of a peer's lyrics as of its own"
    );
}

/// #229 PP audit (comment 6054582866): PP's own track of a song SNV had
/// stands in for SNV's (`peer::standin`); once SNV has its lyrics, the
/// worker's tick takes them in its place and tells the dashboard.
#[tokio::test]
async fn the_worker_replaces_a_stand_in_with_the_peers_copy() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    let snv_json = snv.give_lyrics(snv_id, YT, "mtl+g35t").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    pp.audio_from(YT, "snv").await;
    pp.give_lyrics(id, YT, "gemini-3-5-transcribe").await;
    crate::db::models_peer::record_standin(pp.pool(), YT, "lyrics", "snv", 1_000, 0)
        .await
        .unwrap();
    let (events, mut rx) = broadcast::channel(16);
    LyricsWorker::new_for_test(pp.pool().clone(), pp.cache().to_path_buf(), events)
        .with_peer(pp.ex.clone())
        .process_next()
        .await;
    let json = std::fs::read(pp.cache().join(format!("{YT}_lyrics.json"))).unwrap();
    assert_eq!(json, snv_json);
    let mut completed = None;
    while let Ok(msg) = rx.try_recv() {
        if let ServerMsg::LyricsCompleted {
            video_id, source, ..
        } = msg
        {
            completed = Some((video_id, source));
        }
    }
    assert_eq!(completed, Some((id, "mtl+g35t".to_string())));
}

#[tokio::test]
async fn a_peer_with_the_lyrics_job_queued_defers_the_song() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    let snv_catalog = crate::peer::catalog::build(&snv.ex, "snv", None)
        .await
        .unwrap();
    assert!(
        snv_catalog.announces(YT, Job::Lyrics.makes()),
        "SNV has the song's lyrics queued"
    );
    worker(&pp).process_next().await;
    let (has, attempts, next) = lyrics_state(&pp, id).await;
    assert_eq!((has, attempts), (0, 0), "nothing ran here, no attempt");
    assert!(next.is_some(), "re-picked after the recheck");
}

/// Row `id`'s lyrics as the wall serves them: `(has_lyrics, lyrics_source)`.
async fn served_lyrics(node: &TestNode, id: i64) -> (i64, Option<String>) {
    sqlx::query_as("SELECT has_lyrics, lyrics_source FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

/// The rows the worker told the dashboard about (`LyricsCompleted`), sorted.
fn completed_rows(rx: &mut broadcast::Receiver<ServerMsg>) -> Vec<i64> {
    let mut rows = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if let ServerMsg::LyricsCompleted { video_id, .. } = msg {
            rows.push(video_id);
        }
    }
    rows.sort_unstable();
    rows
}

/// SNV (its lyrics hashed: `mtl+g35t`) and PP with the same song, its audio
/// fetched from SNV, in playlists 1 and 2 (two rows of one video). Returns
/// the nodes, PP's rows and SNV's `{yt}_lyrics.json`.
async fn snv_lyrics_and_pp() -> (TestNode, TestNode, [i64; 2], Vec<u8>) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    let snv_json = snv.give_lyrics(snv_id, YT, "mtl+g35t").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let one = pp.add_video(YT).await;
    let two = pp.add_video_to(2, YT).await;
    pp.give_song(one, YT, "Way Maker", "Sinach").await;
    pp.give_song(two, YT, "Way Maker", "Sinach").await;
    pp.audio_from(YT, "snv").await;
    (snv, pp, [one, two], snv_json)
}

/// #264: PP took SNV's lyrics once (its `peer_fetches` record names the
/// copy it took) and SNV made them again since (a reprocess, a register
/// fix, a better translation). No row of the video is in PP's lyrics queue
/// (served at the current version), so before #264 nothing asked SNV
/// again; PP's next lyrics tick takes SNV's newer copy into every row.
#[tokio::test]
async fn the_worker_follows_the_peers_newer_lyrics() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let (_snv, pp, rows, snv_json) = snv_lyrics_and_pp().await;
    let older = pp.give_lyrics(rows[0], YT, "gemini-3-5-transcribe").await;
    let took = crate::peer::hash::sha256_hex(&older);
    let version = crate::lyrics::LYRICS_PIPELINE_VERSION;
    crate::db::models_peer::record_fetch(pp.pool(), YT, "lyrics", "snv", version, &took, 1_000)
        .await
        .unwrap();
    let (events, mut rx) = broadcast::channel(16);
    LyricsWorker::new_for_test(pp.pool().clone(), pp.cache().to_path_buf(), events)
        .with_peer(pp.ex.clone())
        .process_next()
        .await;
    let json = std::fs::read(pp.cache().join(format!("{YT}_lyrics.json"))).unwrap();
    assert_eq!(json, snv_json, "SNV's newer copy is served here");
    for id in rows {
        let row = served_lyrics(&pp, id).await;
        assert_eq!(row, (1, Some("mtl+g35t".to_string())), "row {id}");
    }
    assert_eq!(completed_rows(&mut rx), rows.to_vec());
}

/// #263 point 3 / #264: a row parked at PP (`asr_gap` at the current
/// version: in no lyrics bucket) never asked SNV, which has the song's
/// lyrics now: PP's tick fills every row of the video from SNV.
#[tokio::test]
async fn the_worker_fills_a_parked_row_from_the_peer() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let (_snv, pp, rows, snv_json) = snv_lyrics_and_pp().await;
    sqlx::query(
        "UPDATE videos SET has_lyrics = 0, lyrics_source = 'asr_gap', \
         lyrics_pipeline_version = ?",
    )
    .bind(i64::from(crate::lyrics::LYRICS_PIPELINE_VERSION))
    .execute(pp.pool())
    .await
    .unwrap();
    worker(&pp).process_next().await;
    let json = std::fs::read(pp.cache().join(format!("{YT}_lyrics.json"))).unwrap();
    assert_eq!(json, snv_json);
    for id in rows {
        let row = served_lyrics(&pp, id).await;
        assert_eq!(row, (1, Some("mtl+g35t".to_string())), "row {id}");
    }
}

/// #269: an operator corrected the song's title at SNV. PP's lyrics tick
/// follows it too, right after the lyrics follow: every row of the video
/// takes SNV's title as SNV's operator named it.
#[tokio::test]
async fn the_worker_follows_the_peers_corrected_title() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let (snv, pp, rows, _) = snv_lyrics_and_pp().await;
    sqlx::query("UPDATE videos SET song = 'Way Maker (Live)', metadata_source = 'manual'")
        .execute(snv.pool())
        .await
        .unwrap();
    worker(&pp).process_next().await;
    for id in rows {
        let title: (String, Option<String>) =
            sqlx::query_as("SELECT song, metadata_source FROM videos WHERE id = ?")
                .bind(id)
                .fetch_one(pp.pool())
                .await
                .unwrap();
        let want = ("Way Maker (Live)".to_string(), Some("manual".to_string()));
        assert_eq!(title, want, "row {id}");
    }
}
