//! #229 item C: while this node's paid AI is off (`paid_ai`), a lyrics job
//! still takes a peer's copy and waits for a peer's job; wherever it would
//! run here it is held: picked again later, no attempt counted, nothing
//! recorded about a run here (no announcement, the wait and a stand-in as
//! they were). Other jobs run as before.

use super::*;
use crate::db::models::VideoLyricsRow;
use crate::db::models_peer::{start_wait, waited};
use crate::peer::decide::MAX_PEER_WAIT;
use crate::peer::kind::Job;
use crate::peer::rig::{SNV_KEY, TestNode, set};
use crate::peer::wire::now_ms;
use sp_core::config::SETTING_PAID_AI_ENABLED;

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

async fn paid_ai_off(node: &TestNode) {
    set(node.pool(), SETTING_PAID_AI_ENABLED, "false").await;
}

/// `(has_lyrics, lyrics_attempts, lyrics_next_attempt_at set)` of row `id`.
async fn lyrics_state(node: &TestNode, id: i64) -> (i64, i64, bool) {
    let (has, attempts, next): (i64, i64, Option<String>) = sqlx::query_as(
        "SELECT has_lyrics, lyrics_attempts, lyrics_next_attempt_at FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(node.pool())
    .await
    .unwrap();
    (has, attempts, next.is_some())
}

/// PP alone (no peer), paid AI off, one song with no lyrics.
async fn pp_alone_off() -> (TestNode, i64) {
    let pp = TestNode::start("pp", None).await;
    paid_ai_off(&pp).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    (pp, id)
}

/// With no peer to ask, a lyrics job is held: not announced, picked again
/// later with no attempt counted; a stems job still runs here.
#[tokio::test]
async fn with_no_peer_a_lyrics_job_is_held_and_stems_run() {
    let (pp, id) = pp_alone_off().await;
    let row = lyrics_row(&pp, id).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Deferred));
    assert_eq!(lyrics_state(&pp, id).await, (0, 0, true));
    assert!(pp.ex.board.snapshot("pp").is_empty(), "nothing announced");
    assert!(matches!(pp.ex.ask(Job::Lyrics, YT).await, Ask::Held));
    let Ask::Local(_guard) = pp.ex.ask(Job::Stems, YT).await else {
        panic!("a stems job calls no paid AI: it runs here")
    };
    assert_eq!(
        pp.ex.board.snapshot("pp").len(),
        2,
        "the stems job, both kinds"
    );
}

/// Settings that do not hold run a job here — not a lyrics job while paid
/// AI is off.
#[tokio::test]
async fn settings_that_do_not_hold_hold_a_lyrics_job() {
    let (pp, _id) = pp_alone_off().await;
    set(pp.pool(), "peers", "not a list").await;
    assert!(matches!(pp.ex.ask(Job::Lyrics, YT).await, Ask::Held));
    assert!(pp.ex.board.snapshot("pp").is_empty());
    let Ask::Local(_guard) = pp.ex.ask(Job::Download, YT).await else {
        panic!("a download runs here")
    };
}

/// An operator's ask is this node's own, but while paid AI is off it is
/// held too (an override text on the row).
#[tokio::test]
async fn an_operators_ask_is_held_while_paid_ai_is_off() {
    let (pp, id) = pp_alone_off().await;
    sqlx::query("UPDATE videos SET lyrics_override_text = 'la la' WHERE id = ?")
        .bind(id)
        .execute(pp.pool())
        .await
        .unwrap();
    let row = lyrics_row(&pp, id).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Deferred));
    assert_eq!(lyrics_state(&pp, id).await, (0, 0, true));
    assert!(pp.ex.board.snapshot("pp").is_empty());
}

/// SNV serving the song (its audio, the lyrics still to make); PP took the
/// audio from SNV and has waited past the 2 h bound for SNV's lyrics.
async fn pp_waited_the_bound() -> (TestNode, TestNode, i64) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    pp.audio_from(YT, "snv").await;
    let bound = MAX_PEER_WAIT.as_millis() as i64;
    start_wait(pp.pool(), YT, "lyrics", now_ms() - bound - 60_000)
        .await
        .unwrap();
    (snv, pp, id)
}

/// Past the bound the job would run here and stand in for SNV's copy; held
/// instead, its spent wait stays and no stand-in is recorded — once paid AI
/// is on, the same pick runs here at once and stands in.
#[tokio::test]
async fn past_the_bound_a_held_job_keeps_its_wait_and_stands_in_for_nothing() {
    let (_snv, pp, id) = pp_waited_the_bound().await;
    paid_ai_off(&pp).await;
    let row = lyrics_row(&pp, id).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Deferred));
    assert_eq!(lyrics_state(&pp, id).await, (0, 0, true));
    let spent = waited(pp.pool(), YT, "lyrics", now_ms()).await.unwrap();
    assert!(spent.is_some_and(|w| w > MAX_PEER_WAIT), "{spent:?}");
    assert_eq!(pp.ex.standin_peer(Job::Lyrics, YT).await, None);
    assert!(pp.ex.board.snapshot("pp").is_empty());
    set(pp.pool(), SETTING_PAID_AI_ENABLED, "true").await;
    let Ask::Local(_guard) = pp.ex.ask(Job::Lyrics, YT).await else {
        panic!("paid AI on: it runs here")
    };
    assert_eq!(
        pp.ex.standin_peer(Job::Lyrics, YT).await.as_deref(),
        Some("snv")
    );
}

/// A peer's lyrics are still taken while paid AI is off.
#[tokio::test]
async fn a_peers_lyrics_are_taken_while_paid_ai_is_off() {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    snv.give_lyrics(snv_id, YT, "mtl+g35t").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    paid_ai_off(&pp).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    pp.audio_from(YT, "snv").await;
    let row = lyrics_row(&pp, id).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Done));
    assert_eq!(lyrics_state(&pp, id).await.0, 1);
}

/// Lyrics this node may not make are no job of its: its catalog announces
/// none queued while paid AI is off (a peer would wait for them).
#[tokio::test]
async fn no_lyrics_are_announced_queued_while_paid_ai_is_off() {
    let pp = TestNode::start("pp", None).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    let lyrics_queued = |jobs: Vec<(String, Job)>| jobs.contains(&(YT.to_string(), Job::Lyrics));
    let on = crate::peer::queued::queued(pp.pool()).await.unwrap();
    assert!(lyrics_queued(on), "paid AI on: the lyrics are queued here");
    paid_ai_off(&pp).await;
    let off = crate::peer::queued::queued(pp.pool()).await.unwrap();
    assert!(!lyrics_queued(off));
}

/// SNV serving the song with `mtl+g35t` lyrics; PP has the song, paid AI
/// off. `audio_from_snv`: PP's audio is recorded as SNV's.
async fn snv_lyrics_pp_off(audio_from_snv: bool) -> (TestNode, TestNode, i64) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    snv.give_lyrics(snv_id, YT, "mtl+g35t").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    paid_ai_off(&pp).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    if audio_from_snv {
        pp.audio_from(YT, "snv").await;
    }
    (snv, pp, id)
}

fn json_at(node: &TestNode) -> Option<Vec<u8>> {
    std::fs::read(node.cache().join(format!("{YT}_lyrics.json"))).ok()
}

async fn wait_rows(node: &TestNode) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM peer_waits")
        .fetch_one(node.pool())
        .await
        .unwrap()
}

async fn audio_path(node: &TestNode, id: i64) -> String {
    sqlx::query_scalar("SELECT audio_file_path FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

/// The same track already served here would run here (nothing newer):
/// held instead, the local file untouched and the earlier wait kept.
#[tokio::test]
async fn nothing_newer_is_held_keeping_the_wait() {
    let (_snv, pp, id) = snv_lyrics_pp_off(true).await;
    start_wait(pp.pool(), YT, "lyrics", 1_000).await.unwrap();
    pp.give_lyrics(id, YT, "mtl+g35t").await;
    std::fs::write(pp.cache().join(format!("{YT}_lyrics.json")), b"local").unwrap();
    let row = lyrics_row(&pp, id).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Deferred));
    assert_eq!(json_at(&pp), Some(b"local".to_vec()));
    assert!(pp.ex.board.snapshot("pp").is_empty());
    assert_eq!(wait_rows(&pp).await, 1, "the wait is not ended");
}

/// A peer's copy made from another audio would run here: held instead.
#[tokio::test]
async fn another_audio_is_held() {
    let (_snv, pp, id) = snv_lyrics_pp_off(false).await;
    let audio = audio_path(&pp, id).await;
    std::fs::write(&audio, crate::peer::rig::bytes(3_000, 9)).unwrap();
    pp.hash_now().await;
    let row = lyrics_row(&pp, id).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Deferred));
    assert_eq!(lyrics_state(&pp, id).await, (0, 0, true));
    assert!(pp.ex.board.snapshot("pp").is_empty());
    assert_eq!(json_at(&pp), None);
}

/// A peer's copy that cannot be taken now (this node's audio cannot be
/// hashed) waits with no bound while paid AI is off: held, no wait
/// started (no give-up WARN can come of it).
#[tokio::test]
async fn a_copy_not_taken_now_is_held_with_no_wait_started() {
    let (_snv, pp, id) = snv_lyrics_pp_off(true).await;
    let audio = audio_path(&pp, id).await;
    std::fs::remove_file(&audio).unwrap();
    std::fs::create_dir(&audio).unwrap();
    let row = lyrics_row(&pp, id).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Deferred));
    assert_eq!(wait_rows(&pp).await, 0);
    assert_eq!(lyrics_state(&pp, id).await, (0, 0, true));
    assert!(pp.ex.board.snapshot("pp").is_empty());
}

/// A stand-in handed the copy past the bound would run here, still
/// standing in: held instead, the stand-in kept.
#[tokio::test]
async fn a_stand_in_past_the_bound_is_held() {
    let (_snv, pp, id) = snv_lyrics_pp_off(true).await;
    pp.give_lyrics(id, YT, "mtl+g35t").await;
    crate::db::models_peer::record_standin(pp.pool(), YT, "lyrics", "snv", 1_000, i64::MAX)
        .await
        .unwrap();
    let bound = MAX_PEER_WAIT.as_millis() as i64;
    start_wait(pp.pool(), YT, "lyrics", now_ms() - bound - 60_000)
        .await
        .unwrap();
    let row = lyrics_row(&pp, id).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Deferred));
    assert_eq!(
        pp.ex.standin_peer(Job::Lyrics, YT).await.as_deref(),
        Some("snv")
    );
    assert!(pp.ex.board.snapshot("pp").is_empty());
}

/// Row `id` with `columns` (an SQL `SET` list).
async fn mark(node: &TestNode, id: i64, columns: &str) {
    sqlx::query(&format!("UPDATE videos SET {columns} WHERE id = ?"))
        .bind(id)
        .execute(node.pool())
        .await
        .unwrap();
}

async fn kept_here(node: &TestNode) -> bool {
    crate::peer::lyrics::kept_local(node.pool(), YT)
        .await
        .unwrap()
}

/// #264 (ROZHODNUTÉ 6185176417): an operator's reprocess flag asks for a
/// new run HERE, which paid AI off cannot make, so with paid AI off it no
/// longer keeps the video's lyrics local; with paid AI on it does, as
/// before.
#[tokio::test]
async fn a_reprocess_flag_keeps_the_lyrics_here_only_while_paid_ai_is_on() {
    let (pp, id) = pp_alone_off().await;
    mark(&pp, id, "lyrics_manual_priority = 1").await;
    assert!(
        !kept_here(&pp).await,
        "paid AI off: the flag cannot run here"
    );
    set(pp.pool(), SETTING_PAID_AI_ENABLED, "true").await;
    assert!(kept_here(&pp).await, "paid AI on: a new run here is asked");
}

/// #264: with paid AI off an operator's text, a dub and a Live-Translate
/// track still keep the lyrics local: they are this node's own content,
/// not a run of the shared pipeline.
#[tokio::test]
async fn an_operators_text_or_a_dub_keeps_the_lyrics_here_while_paid_ai_is_off() {
    for owner in [
        "lyrics_override_text = 'Moj text'",
        "dub_requested = 1",
        "lyrics_source = 'gemini-live-translate'",
    ] {
        let (pp, id) = pp_alone_off().await;
        mark(&pp, id, owner).await;
        assert!(kept_here(&pp).await, "{owner}");
    }
}

/// #264, PP's rows 166, 225, 11 and 39: an operator's Reprocess made at a
/// node with paid AI off was held every 30 min for good. The hook now takes
/// the peer's copy for it (the same audio rule), and the take clears the
/// flag on every row of the video, as a finished local run does.
#[tokio::test]
async fn an_operators_reprocess_takes_the_peers_copy_while_paid_ai_is_off() {
    let (_snv, pp, id) = snv_lyrics_pp_off(true).await;
    let two = pp.add_video_to(2, YT).await;
    pp.give_song(two, YT, "Way Maker", "Sinach").await;
    for row in [id, two] {
        mark(&pp, row, "lyrics_manual_priority = 1").await;
    }
    let row = lyrics_row(&pp, id).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Done), "the peer's copy is taken");
    assert!(json_at(&pp).is_some());
    for row in [id, two] {
        assert_eq!(lyrics_state(&pp, row).await, (1, 0, false), "row {row}");
    }
    assert_eq!(
        pp.manual_flags(YT).await,
        vec![0, 0],
        "the flag cleared on every row"
    );
    let (node, _, _) = crate::db::models_peer::fetch_record(pp.pool(), YT, "lyrics")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(node, "snv");
}

/// PP already serves SNV's very copy (`mtl+g35t` at the current version)
/// in playlists 1 and 2, its audio SNV's, paid AI off; its
/// `{yt}_lyrics.json` here reads `local`, and a lyrics wait is recorded.
/// Returns the nodes and PP's two rows.
async fn pp_serves_snvs_copy() -> (TestNode, TestNode, [i64; 2]) {
    let (snv, pp, id) = snv_lyrics_pp_off(true).await;
    let two = pp.add_video_to(2, YT).await;
    pp.give_song(two, YT, "Way Maker", "Sinach").await;
    pp.give_lyrics(id, YT, "mtl+g35t").await;
    std::fs::write(pp.cache().join(format!("{YT}_lyrics.json")), b"local").unwrap();
    start_wait(pp.pool(), YT, "lyrics", 1_000).await.unwrap();
    (snv, pp, [id, two])
}

/// #264 (ROZHODNUTÉ, option a): with paid AI off, an operator's Reprocess
/// of a song whose track here already IS the peer's copy (nothing newer)
/// can neither run here nor fetch anything new: the flag is cleared on
/// every row, the wait ends and the job is done (nothing fetched, nothing
/// announced, the row not held). The follow brings any later copy.
#[tokio::test]
async fn a_reprocess_of_the_peers_copy_served_here_is_answered_while_paid_ai_is_off() {
    let (_snv, pp, rows) = pp_serves_snvs_copy().await;
    for row in rows {
        mark(&pp, row, "lyrics_manual_priority = 1").await;
    }
    let row = lyrics_row(&pp, rows[0]).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Done), "answered by the copy here");
    assert_eq!(pp.manual_flags(YT).await, vec![0, 0], "every row's flag");
    assert_eq!(wait_rows(&pp).await, 0, "the wait is over");
    assert_eq!(lyrics_state(&pp, rows[0]).await, (1, 0, false), "not held");
    assert_eq!(json_at(&pp), Some(b"local".to_vec()), "nothing fetched");
    assert!(pp.ex.board.snapshot("pp").is_empty(), "nothing announced");
}

/// #264: with paid AI on nothing changes. The copy served here runs here
/// (nothing newer), and a reprocess flag left on a row of an inactive
/// playlist (one the lyrics queue never acts on, so it keeps nothing
/// local) stays as it was.
#[tokio::test]
async fn with_paid_ai_on_the_peers_copy_served_here_runs_here_and_keeps_a_flag() {
    let (_snv, pp, rows) = pp_serves_snvs_copy().await;
    set(pp.pool(), SETTING_PAID_AI_ENABLED, "true").await;
    mark(&pp, rows[1], "lyrics_manual_priority = 1").await;
    sqlx::query("UPDATE playlists SET is_active = 0 WHERE id = 2")
        .execute(pp.pool())
        .await
        .unwrap();
    let row = lyrics_row(&pp, rows[0]).await;
    let PeerStep::Local(Some(_guard)) = crate::peer::lyrics::first(Some(&pp.ex), &row).await else {
        panic!("paid AI on: nothing newer runs here")
    };
    assert_eq!(pp.manual_flags(YT).await, vec![0, 1], "the flag kept");
}

/// #264: with paid AI off a reprocess flag on a row with an operator's text
/// or a dub is left alone: the lyrics stay local, the row is held as
/// before, its flag and the wait kept.
#[tokio::test]
async fn a_reprocess_flag_with_an_operators_text_or_a_dub_is_left_alone_while_paid_ai_is_off() {
    for owner in ["lyrics_override_text = 'Moj text'", "dub_requested = 1"] {
        let (_snv, pp, rows) = pp_serves_snvs_copy().await;
        mark(
            &pp,
            rows[0],
            &format!("lyrics_manual_priority = 1, {owner}"),
        )
        .await;
        let row = lyrics_row(&pp, rows[0]).await;
        let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
        assert!(matches!(step, PeerStep::Deferred), "{owner}: held");
        assert_eq!(pp.manual_flags(YT).await, vec![1, 0], "{owner}");
        assert_eq!(wait_rows(&pp).await, 1, "{owner}: the wait kept");
        assert_eq!(json_at(&pp), Some(b"local".to_vec()), "{owner}");
    }
}
