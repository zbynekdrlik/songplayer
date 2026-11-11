//! #304: a lyrics job held for paid AI because the peer's copy is made from
//! another audio takes the peer's song (its video + audio) in place of this
//! node's own download, then the peer's lyrics in the same pick; this
//! node's stems of the old audio are dropped and asked for again.

use std::path::{Path, PathBuf};

use crate::db::models::VideoLyricsRow;
use crate::db::models_peer::fetch_record;
use crate::peer::PeerStep;
use crate::peer::rig::{SNV_KEY, TestNode, bytes, set, song_audio, song_audio_sha};
use sp_core::config::SETTING_PAID_AI_ENABLED;

const YT: &str = "aaaaaaaaaaa";
const ROW_SQL: &str = "SELECT v.id, v.youtube_id, COALESCE(v.song, '') AS song, \
     COALESCE(v.artist, '') AS artist, v.duration_ms, v.audio_file_path, p.youtube_url, \
     v.lyrics_override_text, v.lyrics_time_offset_ms, v.spotify_track_id, v.spotify_resolved_at \
     FROM videos v JOIN playlists p ON p.id = v.playlist_id WHERE v.id = ?";

/// PP's own download of the song: another encode than SNV's.
fn own_video() -> Vec<u8> {
    bytes(2_100, 8)
}

fn own_audio() -> Vec<u8> {
    bytes(3_100, 9)
}

/// SNV serving the song with its lyrics (and its stems when `snv_stems`);
/// PP with the song downloaded itself (its own video and audio, no fetch
/// record), paid AI off, asking SNV. Answers both nodes, PP's row and its
/// recorded `(video, audio)`.
async fn pp_downloaded_itself(snv_stems: bool) -> (TestNode, TestNode, i64, (PathBuf, PathBuf)) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    snv.give_lyrics(snv_id, YT, "mtl+g35t").await;
    if snv_stems {
        snv.give_stems(snv_id).await;
    }
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    set(pp.pool(), SETTING_PAID_AI_ENABLED, "false").await;
    let id = pp.add_video(YT).await;
    let (video, audio) = pp.give_song(id, YT, "Cesta", "Zbor").await;
    std::fs::write(&video, own_video()).unwrap();
    std::fs::write(&audio, own_audio()).unwrap();
    (snv, pp, id, (video, audio))
}

async fn lyrics_row(node: &TestNode, id: i64) -> VideoLyricsRow {
    sqlx::query_as(ROW_SQL)
        .bind(id)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

/// `(file_path, audio_file_path)` of row `id`.
async fn recorded(node: &TestNode, id: i64) -> (String, String) {
    sqlx::query_as("SELECT file_path, audio_file_path FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

async fn has_lyrics(node: &TestNode, id: i64) -> i64 {
    sqlx::query_scalar("SELECT has_lyrics FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

/// `(stem_status, vocals_file_path, instrumental_file_path)` of row `id`.
async fn stems_of(node: &TestNode, id: i64) -> (Option<String>, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT stem_status, vocals_file_path, instrumental_file_path FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(node.pool())
    .await
    .unwrap()
}

/// Files in `dir` whose name ends with `suffix`.
fn files_ending(dir: &Path, suffix: &str) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(suffix))
        .count()
}

/// The issue's case: the song PP downloaded itself takes SNV's video and
/// audio under the names PP's row records, then SNV's lyrics, in one pick.
/// The pair's origin is recorded (so the audio guard vouches for it from
/// now on), and no old copy is left behind.
#[tokio::test]
async fn a_held_lyrics_job_takes_the_peers_song_and_then_its_lyrics() {
    let (snv, pp, id, (video, audio)) = pp_downloaded_itself(false).await;
    let before = recorded(&pp, id).await;
    let row = lyrics_row(&pp, id).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Done), "the peer's lyrics taken");
    assert_eq!(std::fs::read(&audio).unwrap(), song_audio(), "SNV's audio");
    assert_eq!(
        std::fs::read(&video).unwrap(),
        bytes(2_000, 1),
        "SNV's video"
    );
    assert_eq!(recorded(&pp, id).await, before, "the names stay");
    assert_eq!(has_lyrics(&pp, id).await, 1);
    let snv_json = std::fs::read(snv.cache().join(format!("{YT}_lyrics.json"))).unwrap();
    let pp_json = std::fs::read(pp.cache().join(format!("{YT}_lyrics.json"))).unwrap();
    assert_eq!(pp_json, snv_json);
    let (node, _, sha) = fetch_record(pp.pool(), YT, "audio").await.unwrap().unwrap();
    assert_eq!((node.as_str(), sha), ("snv", song_audio_sha()));
    assert!(
        fetch_record(pp.pool(), YT, "video")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(files_ending(pp.cache(), ".peer-old"), 0, "no old copy left");
    assert!(pp.ex.board.snapshot("pp").is_empty(), "nothing runs here");
}

/// PP's stems were separated from its OLD audio: they are dropped with it
/// and the row asks again, so the stems job then takes SNV's stems (the
/// audio is SNV's now) instead of keeping stems of another audio.
#[tokio::test]
async fn the_old_stems_go_and_the_peers_are_taken_next() {
    let (_snv, pp, id, _) = pp_downloaded_itself(true).await;
    let (vocals, instrumental) = pp.give_stems(id).await;
    let row = lyrics_row(&pp, id).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Done));
    assert!(
        !vocals.exists() && !instrumental.exists(),
        "the old stems go"
    );
    assert_eq!(stems_of(&pp, id).await, (None, None, None), "asked again");
    let job = crate::db::models_stems::get_next_video_for_stems(pp.pool())
        .await
        .unwrap()
        .expect("the row is in the stems queue again");
    let step = crate::peer::stems::first(Some(&pp.ex), &job).await;
    assert!(matches!(step, PeerStep::Done), "SNV's stems taken");
    assert_eq!(std::fs::read(&vocals).unwrap(), bytes(1_500, 3));
    assert_eq!(stems_of(&pp, id).await.0.as_deref(), Some("done"));
}

/// With paid AI on nothing changes: the lyrics of another audio are made
/// here and this node's own song stays.
#[tokio::test]
async fn with_paid_ai_on_the_song_here_stays_and_the_lyrics_run_here() {
    let (_snv, pp, id, (video, audio)) = pp_downloaded_itself(false).await;
    set(pp.pool(), SETTING_PAID_AI_ENABLED, "true").await;
    let row = lyrics_row(&pp, id).await;
    let PeerStep::Local(Some(_guard)) = crate::peer::lyrics::first(Some(&pp.ex), &row).await else {
        panic!("paid AI on: the lyrics run here")
    };
    assert_eq!(std::fs::read(&audio).unwrap(), own_audio());
    assert_eq!(std::fs::read(&video).unwrap(), own_video());
    assert_eq!(fetch_record(pp.pool(), YT, "audio").await.unwrap(), None);
}
