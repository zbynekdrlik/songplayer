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
    assert_eq!(files_ending(pp.cache(), ".replaced"), 0, "no old copy left");
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

/// The file half over a temp dir: `(own, parts)` with distinct bytes.
fn files(dir: &Path) -> (super::Pair, super::Pair) {
    let own = super::Pair {
        video: dir.join("song_video.mp4"),
        audio: dir.join("song_audio.flac"),
    };
    let parts = super::Pair {
        video: dir.join("v.part"),
        audio: dir.join("a.part"),
    };
    std::fs::write(&own.video, own_video()).unwrap();
    std::fs::write(&own.audio, own_audio()).unwrap();
    std::fs::write(&parts.video, bytes(2_000, 1)).unwrap();
    std::fs::write(&parts.audio, song_audio()).unwrap();
    (own, parts)
}

/// Both parts take the names, the old copies go.
#[test]
fn replace_pair_places_both_and_keeps_no_old_copy() {
    let dir = tempfile::tempdir().unwrap();
    let (own, parts) = files(dir.path());
    super::replace_pair(&own, &parts, (2_000, 3_000)).unwrap();
    assert_eq!(std::fs::read(&own.video).unwrap(), bytes(2_000, 1));
    assert_eq!(std::fs::read(&own.audio).unwrap(), song_audio());
    assert!(!parts.video.exists() && !parts.audio.exists());
    assert_eq!(files_ending(dir.path(), ".replaced"), 0);
}

/// The audio cannot take its name (here: its part is gone): the old video
/// is back under its name, the placed video back in its part (no transfer
/// next time), the old audio untouched, no link left.
#[test]
fn replace_pair_puts_the_old_song_back_when_the_audio_cannot_take_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let (own, parts) = files(dir.path());
    std::fs::remove_file(&parts.audio).unwrap();
    let err = super::replace_pair(&own, &parts, (2_000, 3_000)).unwrap_err();
    assert!(err.contains("audio"), "{err}");
    assert_eq!(std::fs::read(&own.video).unwrap(), own_video());
    assert_eq!(std::fs::read(&own.audio).unwrap(), own_audio());
    assert_eq!(std::fs::read(&parts.video).unwrap(), bytes(2_000, 1));
    assert_eq!(files_ending(dir.path(), ".replaced"), 0);
}

/// A placed file of another size than the catalog's: the old pair is back.
#[test]
fn replace_pair_puts_the_old_song_back_when_a_size_is_not_the_catalogs() {
    for sizes in [(2_001, 3_000), (2_000, 3_001)] {
        let dir = tempfile::tempdir().unwrap();
        let (own, parts) = files(dir.path());
        let err = super::replace_pair(&own, &parts, sizes).unwrap_err();
        assert!(err.contains("size"), "{err}");
        assert_eq!(std::fs::read(&own.video).unwrap(), own_video());
        assert_eq!(std::fs::read(&own.audio).unwrap(), own_audio());
        assert_eq!(files_ending(dir.path(), ".replaced"), 0);
    }
}

/// The video cannot take its name (its part is gone): nothing changes.
#[test]
fn replace_pair_changes_nothing_when_the_video_cannot_take_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let (own, parts) = files(dir.path());
    std::fs::remove_file(&parts.video).unwrap();
    let err = super::replace_pair(&own, &parts, (2_000, 3_000)).unwrap_err();
    assert!(err.contains("video"), "{err}");
    assert_eq!(std::fs::read(&own.video).unwrap(), own_video());
    assert_eq!(std::fs::read(&own.audio).unwrap(), own_audio());
    assert_eq!(std::fs::read(&parts.audio).unwrap(), song_audio());
    assert_eq!(files_ending(dir.path(), ".replaced"), 0);
}

/// A set-aside file left by an earlier unit move is never overwritten (it
/// may be the only copy of a file): the swap takes the next free name.
#[test]
fn replace_pair_never_overwrites_a_set_aside_leftover() {
    let dir = tempfile::tempdir().unwrap();
    let (own, parts) = files(dir.path());
    let mut leftover = own.audio.clone().into_os_string();
    leftover.push(".replaced");
    std::fs::write(&leftover, b"leftover").unwrap();
    super::replace_pair(&own, &parts, (2_000, 3_000)).unwrap();
    assert_eq!(std::fs::read(&own.audio).unwrap(), song_audio());
    assert_eq!(std::fs::read(&leftover).unwrap(), b"leftover");
    assert_eq!(
        files_ending(dir.path(), ".replaced"),
        1,
        "only the leftover"
    );
}

/// The placed audio's hash is the catalog's, stored as the hasher would:
/// the audio guard vouches for it without reading the file again.
#[tokio::test]
async fn the_placed_audios_hash_is_stored() {
    let (_snv, pp, id, (_, audio)) = pp_downloaded_itself(false).await;
    let row = lyrics_row(&pp, id).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Done));
    let hashed = crate::db::models_peer::hash_of(pp.pool(), &audio.to_string_lossy())
        .await
        .unwrap()
        .expect("the placed audio's hash is stored");
    assert_eq!(hashed.sha256, song_audio_sha());
    let (size, mtime_ms) = crate::peer::hasher::stat(&audio).await.unwrap();
    assert!(
        hashed.holds(size, mtime_ms),
        "it holds for the file on disk"
    );
}

/// A row that records no video (only an audio) has no pair to swap: held,
/// and nothing is transferred for it.
#[tokio::test]
async fn a_row_with_no_video_is_held_with_nothing_transferred() {
    let (_snv, pp, id, (_, audio)) = pp_downloaded_itself(false).await;
    sqlx::query("UPDATE videos SET file_path = '' WHERE id = ?")
        .bind(id)
        .execute(pp.pool())
        .await
        .unwrap();
    let row = lyrics_row(&pp, id).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Deferred), "held");
    assert_eq!(std::fs::read(&audio).unwrap(), own_audio());
    let parts = pp.cache().join(crate::peer::fetch::PARTS_DIR);
    let fetched = parts.exists() && std::fs::read_dir(&parts).unwrap().next().is_some();
    assert!(!fetched, "nothing transferred");
    assert_eq!(has_lyrics(&pp, id).await, 0);
}

/// A file that was not there counts as removed; any other error stays one.
#[test]
fn gone_forgives_only_a_missing_file() {
    use std::io::{Error, ErrorKind};
    assert!(super::gone(Ok(())).is_ok());
    assert!(super::gone(Err(Error::from(ErrorKind::NotFound))).is_ok());
    let denied = super::gone(Err(Error::from(ErrorKind::PermissionDenied)));
    assert_eq!(denied.unwrap_err().kind(), ErrorKind::PermissionDenied);
}

/// `path`'s mtime set to a fixed instant long ago: a later write or a
/// rename of another file over the name shows as another mtime.
fn mark(path: &Path) -> std::time::SystemTime {
    let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(at)
        .unwrap();
    at
}

fn mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

/// The parts kept in PP's peer dir.
fn parts(pp: &TestNode) -> Vec<PathBuf> {
    let dir = pp.cache().join(crate::peer::fetch::PARTS_DIR);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut parts: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    parts.sort();
    parts
}

/// Once swapped, the song here IS the peer's: a later pick of the same
/// song swaps nothing again (the record vouches for the audio).
#[tokio::test]
async fn a_swapped_song_is_never_swapped_again() {
    let (_snv, pp, id, (video, audio)) = pp_downloaded_itself(false).await;
    let row = lyrics_row(&pp, id).await;
    assert!(matches!(
        crate::peer::lyrics::first(Some(&pp.ex), &row).await,
        PeerStep::Done
    ));
    let at = mark(&video);
    sqlx::query("UPDATE videos SET has_lyrics = 0 WHERE id = ?")
        .bind(id)
        .execute(pp.pool())
        .await
        .unwrap();
    let row = lyrics_row(&pp, id).await;
    let again = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(!matches!(again, PeerStep::Deferred));
    assert_eq!(mtime(&video), at, "the video was not replaced again");
    assert_eq!(std::fs::read(&audio).unwrap(), song_audio());
    assert!(parts(&pp).is_empty(), "nothing transferred again");
}

/// A name the swap cannot take (here: a directory where the video was, as a
/// file a player holds open refuses its name on Windows): held, this node's
/// audio untouched, the parts kept; the next pick takes them with no
/// transfer.
#[tokio::test]
async fn a_refused_name_keeps_the_parts_and_the_next_pick_transfers_nothing() {
    let (_snv, pp, id, (video, audio)) = pp_downloaded_itself(false).await;
    std::fs::remove_file(&video).unwrap();
    std::fs::create_dir(&video).unwrap();
    let row = lyrics_row(&pp, id).await;
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Deferred), "held");
    assert_eq!(std::fs::read(&audio).unwrap(), own_audio());
    assert_eq!(fetch_record(pp.pool(), YT, "audio").await.unwrap(), None);
    let kept = parts(&pp);
    assert_eq!(kept.len(), 2, "both parts kept: {kept:?}");
    let at = mark(&kept[0]);
    assert_eq!(mark(&kept[1]), at);
    std::fs::remove_dir(&video).unwrap();
    std::fs::write(&video, own_video()).unwrap();
    let step = crate::peer::lyrics::first(Some(&pp.ex), &row).await;
    assert!(matches!(step, PeerStep::Done));
    assert_eq!(std::fs::read(&audio).unwrap(), song_audio());
    assert_eq!(mtime(&video), at, "the kept video part placed");
    assert_eq!(mtime(&audio), at, "the kept audio part placed");
    assert_eq!(files_ending(pp.cache(), ".replaced"), 0);
}

/// The peer's plan for the song, its audio as `peer_audio`.
async fn plan_of(snv: &TestNode, pp: &TestNode) -> crate::peer::FetchPlan {
    let peer = snv.as_peer(SNV_KEY);
    let catalog = pp.ex.client.catalog(&peer).await.unwrap();
    let audio = crate::peer::decide::listed_audio(&catalog, YT).cloned();
    crate::peer::FetchPlan {
        peer,
        artifacts: Vec::new(),
        peer_audio: audio,
    }
}

/// A peer whose listed song's audio is not the one its copy was made from
/// (its catalog moved between two reads): held, nothing transferred.
#[tokio::test]
async fn a_peer_audio_other_than_the_copys_is_held_with_nothing_transferred() {
    let (snv, pp, id, (_, audio)) = pp_downloaded_itself(false).await;
    let mut plan = plan_of(&snv, &pp).await;
    plan.peer_audio.as_mut().unwrap().sha256 = "0".repeat(64);
    let step = pp
        .ex
        .take_peers_song(crate::peer::kind::Job::Lyrics, &plan, id, YT)
        .await;
    assert!(matches!(step, Some(PeerStep::Deferred)));
    assert_eq!(std::fs::read(&audio).unwrap(), own_audio());
    assert!(parts(&pp).is_empty(), "nothing transferred");
}

/// The row's video replaced in place after the fetch began (the video
/// upgrade or the repair keep its name): the swap refuses, nothing placed.
#[tokio::test]
async fn a_video_replaced_in_place_meanwhile_stops_the_swap() {
    let (snv, pp, id, (video, audio)) = pp_downloaded_itself(false).await;
    let plan = plan_of(&snv, &pp).await;
    let own = pp.ex.recorded(id).await.unwrap();
    let pair = pp.ex.peers_pair(&plan, YT).await.unwrap();
    let fetched = pp.ex.fetch_pair(&plan, &pair).await.unwrap();
    std::fs::write(&video, bytes(2_500, 4)).unwrap();
    let err = pp
        .ex
        .swap_song(id, YT, "snv", &own, &fetched, &pair)
        .await
        .unwrap_err();
    assert!(err.contains("changed"), "{err}");
    assert_eq!(std::fs::read(&video).unwrap(), bytes(2_500, 4));
    assert_eq!(std::fs::read(&audio).unwrap(), own_audio());
    assert_eq!(fetch_record(pp.pool(), YT, "audio").await.unwrap(), None);
}

/// A stems separation in flight holds the swap only when it is of a row of
/// this song (`stems::progress::in_flight`, passed in: the cell is global).
#[tokio::test]
async fn a_stems_run_of_the_song_holds_the_swap() {
    let (_snv, pp, id, _) = pp_downloaded_itself(false).await;
    let other = pp.add_video("bbbbbbbbbbb").await;
    assert!(pp.ex.stems_run_on(YT, Some(id)).await);
    assert!(!pp.ex.stems_run_on(YT, Some(other)).await);
    assert!(!pp.ex.stems_run_on(YT, Some(id + 1_000)).await);
    assert!(!pp.ex.stems_run_on(YT, None).await);
}

/// The swap forgets the replaced video's upgrade record and the old audio's
/// stems resume dir, so neither a rollback nor a resumed separation brings
/// this node's old song back.
#[tokio::test]
async fn the_upgrade_record_and_the_stems_resume_dir_of_the_old_song_go() {
    let (_snv, pp, id, _) = pp_downloaded_itself(false).await;
    sqlx::query(
        "UPDATE videos SET video_upgrade_cap = 2160, video_upgrade_state = 'upgraded', \
         video_upgrade_at = 5 WHERE id = ?",
    )
    .bind(id)
    .execute(pp.pool())
    .await
    .unwrap();
    let resume = pp.cache().join(format!("{YT}_stemsep"));
    std::fs::create_dir_all(&resume).unwrap();
    std::fs::write(resume.join("seg_000.flac"), b"old").unwrap();
    let row = lyrics_row(&pp, id).await;
    assert!(matches!(
        crate::peer::lyrics::first(Some(&pp.ex), &row).await,
        PeerStep::Done
    ));
    let upgrade: (Option<i64>, Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT video_upgrade_cap, video_upgrade_state, video_upgrade_at FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pp.pool())
    .await
    .unwrap();
    assert_eq!(upgrade, (None, None, None));
    assert!(!resume.exists(), "the old audio's segments go");
}
