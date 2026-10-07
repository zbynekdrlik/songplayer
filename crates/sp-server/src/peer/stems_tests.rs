//! #229: the stems job asks first.

use super::*;
use crate::db::models_peer::fetch_record;
use crate::db::models_stems::StemJob;
use crate::peer::kind::Job;
use crate::peer::rig::{SNV_KEY, TestNode, bytes};
use std::path::{Path, PathBuf};

const YT: &str = "aaaaaaaaaaa";

/// SNV has the song and its stems hashed; PP has the song under its own
/// title, its audio fetched from SNV (recorded), and asks SNV.
async fn snv_and_pp() -> (TestNode, TestNode, StemJob, PathBuf) {
    let (snv, pp, job, audio) = snv_and_pp_own_audio().await;
    pp.audio_from(YT, "snv").await;
    (snv, pp, job, audio)
}

/// As [`snv_and_pp`], but nothing records where PP's audio came from and
/// PP has not hashed it.
async fn snv_and_pp_own_audio() -> (TestNode, TestNode, StemJob, PathBuf) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    snv.give_stems(snv_id).await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    let (_, audio) = pp.give_song(id, YT, "Cesta", "Zbor").await;
    let job = StemJob {
        video_id: id,
        youtube_id: YT.into(),
        audio_file_path: audio.to_string_lossy().into_owned(),
        duration_ms: None,
        song: None,
        artist: None,
    };
    (snv, pp, job, audio)
}

type StemState = (
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
);

async fn stem_state(node: &TestNode, id: i64) -> StemState {
    sqlx::query_as(
        "SELECT stem_status, stem_attempts, stem_next_attempt_at, vocals_file_path, \
                instrumental_file_path FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(node.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn a_peers_stems_land_under_this_nodes_audio_and_are_done() {
    let (_snv, pp, job, audio) = snv_and_pp().await;
    assert!(matches!(first(Some(&pp.ex), &job).await, PeerStep::Done));
    let (vocals, instrumental) = crate::stems::stem_paths(&audio);
    assert_eq!(std::fs::read(&vocals).unwrap(), bytes(1_500, 3));
    assert_eq!(std::fs::read(&instrumental).unwrap(), bytes(1_700, 4));
    let (status, attempts, next, recorded_vocals, recorded_instrumental) =
        stem_state(&pp, job.video_id).await;
    assert_eq!((status.as_deref(), attempts, next), (Some("done"), 0, None));
    assert_eq!(
        recorded_vocals.as_deref(),
        Some(vocals.to_string_lossy().as_ref())
    );
    assert_eq!(
        recorded_instrumental.as_deref(),
        Some(instrumental.to_string_lossy().as_ref())
    );
    let (node, _, _) = fetch_record(pp.pool(), YT, "stem_vocals")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(node, "snv");
    assert_eq!(
        std::fs::read_dir(pp.ex.parts_dir()).unwrap().count(),
        0,
        "no part left"
    );
}

/// PP's audio is its own encode (downloaded here; its own hash is not the
/// audio SNV lists): SNV's stems were separated from another audio, so they
/// are separated here. Nothing is fetched or placed, the job is announced.
#[tokio::test]
async fn stems_made_from_another_audio_are_separated_here() {
    let (_snv, pp, job, audio) = snv_and_pp_own_audio().await;
    std::fs::write(&audio, bytes(3_000, 9)).unwrap();
    pp.hash_now().await;
    let PeerStep::Local(Some(_guard)) = first(Some(&pp.ex), &job).await else {
        panic!("expected Local")
    };
    assert_eq!(pp.ex.board.snapshot("pp").len(), 2, "both stems announced");
    assert!(
        !crate::stems::stem_paths(&audio).0.exists(),
        "no stems placed"
    );
    assert!(!pp.ex.parts_dir().exists(), "nothing transferred");
    let (status, attempts, ..) = stem_state(&pp, job.video_id).await;
    assert_eq!((status, attempts), (None, 0));
}

/// PP fetched its audio from SNV, but at a sha SNV no longer lists (SNV
/// downloaded it again since): not the audio SNV's stems are made from.
#[tokio::test]
async fn an_audio_fetched_at_an_older_sha_is_not_the_peers_now() {
    let (_snv, pp, job, audio) = snv_and_pp_own_audio().await;
    crate::db::models_peer::record_fetch(
        pp.pool(),
        YT,
        "audio",
        "snv",
        crate::peer::kind::MEDIA_VERSION,
        &"0123456789abcdef".repeat(4),
        1_000,
    )
    .await
    .unwrap();
    assert!(matches!(
        first(Some(&pp.ex), &job).await,
        PeerStep::Local(Some(_))
    ));
    assert!(!crate::stems::stem_paths(&audio).0.exists());
}

/// Nothing records where PP's audio came from, but PP's own hash of it is
/// the sha SNV lists (phase 2: a node that serves hashes its audio): the
/// same audio, so SNV's stems are taken.
#[tokio::test]
async fn an_own_hash_equal_to_the_peers_audio_takes_its_stems() {
    let (_snv, pp, job, audio) = snv_and_pp_own_audio().await;
    pp.hash_now().await;
    assert!(matches!(first(Some(&pp.ex), &job).await, PeerStep::Done));
    assert_eq!(
        std::fs::read(crate::stems::stem_paths(&audio).0).unwrap(),
        bytes(1_500, 3)
    );
}

/// Review Focus 4: a rename here while the transfer ran. The test holds the
/// peer's transfer slot, so `adopt` waits INSIDE its first fetch; the song is
/// renamed then, and only after that is the slot released.
#[tokio::test]
async fn stems_fetched_during_a_rename_land_under_the_new_name() {
    let (_snv, pp, job, audio) = snv_and_pp().await;
    let Ask::Fetch(plan) = pp.ex.ask(Job::Stems, YT).await else {
        panic!("expected Fetch")
    };
    let slot = pp.ex.client.slot("snv");
    let held = slot.lock().await;
    let ex = pp.ex.clone();
    let (spawned_job, spawned_plan) = (job.clone(), plan.clone());
    let adopting = tokio::spawn(async move { adopt(&ex, &spawned_job, &spawned_plan).await });
    // The fetch took its own handle of the slot: it waits for the lock now.
    for _ in 0..2_000 {
        if std::sync::Arc::strong_count(&slot) >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        std::sync::Arc::strong_count(&slot) >= 3,
        "adopt reached the transfer"
    );
    let renamed = pp
        .cache()
        .join("Opravena_Zbor_aaaaaaaaaaa_normalized_audio.flac");
    std::fs::rename(&audio, &renamed).unwrap();
    sqlx::query("UPDATE videos SET audio_file_path = ? WHERE id = ?")
        .bind(renamed.to_string_lossy().to_string())
        .bind(job.video_id)
        .execute(pp.pool())
        .await
        .unwrap();
    drop(held);
    adopting.await.unwrap().unwrap();
    let (new_vocals, new_instrumental) = crate::stems::stem_paths(&renamed);
    let (old_vocals, old_instrumental) = crate::stems::stem_paths(Path::new(&job.audio_file_path));
    assert!(new_vocals.exists());
    assert!(new_instrumental.exists());
    assert!(!old_vocals.exists());
    assert!(!old_instrumental.exists());
    let (_, _, _, recorded, _) = stem_state(&pp, job.video_id).await;
    assert_eq!(
        recorded.as_deref(),
        Some(new_vocals.to_string_lossy().as_ref())
    );
}

#[tokio::test]
async fn a_peer_separating_it_defers_without_an_attempt() {
    let (snv, pp, job, _) = snv_and_pp().await;
    let other = pp.add_video("bbbbbbbbbbb").await;
    let (_, audio) = pp.give_song(other, "bbbbbbbbbbb", "Iny", "Zbor").await;
    let _running = snv.ex.announce("bbbbbbbbbbb", Job::Stems);
    let waiting = StemJob {
        video_id: other,
        youtube_id: "bbbbbbbbbbb".into(),
        audio_file_path: audio.to_string_lossy().into_owned(),
        ..job
    };
    assert!(matches!(
        first(Some(&pp.ex), &waiting).await,
        PeerStep::Deferred
    ));
    let (status, attempts, next, _, _) = stem_state(&pp, other).await;
    assert_eq!((status, attempts), (None, 0));
    let next = chrono::DateTime::parse_from_rfc3339(&next.unwrap()).unwrap();
    let ahead = (next.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
    assert!((100..=125).contains(&ahead), "{ahead}");
    assert!(!crate::stems::stem_paths(&audio).0.exists());
}

#[tokio::test]
async fn a_song_with_no_audio_here_is_deferred_not_failed() {
    let (_snv, pp, job, audio) = snv_and_pp().await;
    std::fs::remove_file(&audio).unwrap();
    assert!(matches!(
        first(Some(&pp.ex), &job).await,
        PeerStep::Deferred
    ));
    let (status, attempts, next, _, _) = stem_state(&pp, job.video_id).await;
    assert_eq!((status, attempts), (None, 0));
    assert!(next.is_some());
    assert!(
        !pp.ex.parts_dir().exists(),
        "nothing transferred for a song with no audio here"
    );
    let waits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM peer_waits")
        .fetch_one(pp.pool())
        .await
        .unwrap();
    assert_eq!(waits, 0, "no failed fetch: no wait");
}

#[tokio::test]
async fn nobody_has_them_so_they_are_separated_here_announced() {
    let (_snv, pp, _, _) = snv_and_pp().await;
    let id = pp.add_video("ccccccccccc").await;
    let (_, audio) = pp.give_song(id, "ccccccccccc", "Iny", "Zbor").await;
    let job = StemJob {
        video_id: id,
        youtube_id: "ccccccccccc".into(),
        audio_file_path: audio.to_string_lossy().into_owned(),
        duration_ms: None,
        song: None,
        artist: None,
    };
    let PeerStep::Local(Some(guard)) = first(Some(&pp.ex), &job).await else {
        panic!("expected Local")
    };
    assert_eq!(pp.ex.board.snapshot("pp").len(), 2, "both stems announced");
    drop(guard);
    assert!(pp.ex.board.snapshot("pp").is_empty());
    assert!(matches!(first(None, &job).await, PeerStep::Local(None)));
}
