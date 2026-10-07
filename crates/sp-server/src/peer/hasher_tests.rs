//! #229 `peer::hasher`: the sha256 cache behind the catalog.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::db::models_peer::all_hashes;
use crate::peer::catalog::path_key;
use crate::peer::hash::sha256_hex;
use crate::peer::rig::{SNV_KEY, TestNode, bytes, set};

const YT: &str = "aaaaaaaaaaa";

fn set_mtime(path: &std::path::Path, secs: u64) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

#[tokio::test]
async fn a_pass_hashes_each_file_once_then_finds_it_fresh() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (video, audio) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    let first = node.hash_now().await;
    assert_eq!(
        first,
        HashPass {
            hashed: 2,
            ..HashPass::default()
        }
    );
    let all = all_hashes(node.pool()).await.unwrap();
    let a = &all[&path_key(&audio)];
    assert_eq!(a.sha256, sha256_hex(&bytes(3_000, 2)));
    assert_eq!(a.size, 3_000);
    assert!(a.hashed_at_ms > 0);
    let v = &all[&path_key(&video)];
    assert_eq!(
        (v.size, v.sha256.clone()),
        (2_000, sha256_hex(&bytes(2_000, 1)))
    );
    let second = node.hash_now().await;
    assert_eq!(
        second,
        HashPass {
            fresh: 2,
            ..HashPass::default()
        }
    );
}

/// Same size, another mtime: hashed again (the cache key is path + size +
/// mtime).
#[tokio::test]
async fn a_changed_file_is_hashed_again() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (_, audio) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    node.hash_now().await;
    std::fs::write(&audio, bytes(3_000, 9)).unwrap();
    set_mtime(&audio, 1_900_000_000);
    let pass = node.hash_now().await;
    assert_eq!((pass.hashed, pass.fresh), (1, 1));
    let all = all_hashes(node.pool()).await.unwrap();
    assert_eq!(all[&path_key(&audio)].sha256, sha256_hex(&bytes(3_000, 9)));
}

#[tokio::test]
async fn a_missing_file_loses_its_entry() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (video, _) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    node.hash_now().await;
    std::fs::remove_file(&video).unwrap();
    let pass = node.hash_now().await;
    assert_eq!((pass.missing, pass.fresh, pass.hashed), (1, 1, 0));
    let all = all_hashes(node.pool()).await.unwrap();
    assert!(!all.contains_key(&path_key(&video)));
}

#[tokio::test]
async fn a_renamed_song_drops_the_old_paths() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (video, audio) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    node.hash_now().await;
    let new_video = node
        .cache()
        .join("Renamed_Sinach_aaaaaaaaaaa_normalized_video.mp4");
    let new_audio = node
        .cache()
        .join("Renamed_Sinach_aaaaaaaaaaa_normalized_audio.flac");
    std::fs::rename(&video, &new_video).unwrap();
    std::fs::rename(&audio, &new_audio).unwrap();
    sqlx::query("UPDATE videos SET file_path = ?, audio_file_path = ? WHERE id = ?")
        .bind(new_video.to_string_lossy().to_string())
        .bind(new_audio.to_string_lossy().to_string())
        .bind(id)
        .execute(node.pool())
        .await
        .unwrap();
    let pass = node.hash_now().await;
    assert_eq!((pass.hashed, pass.pruned), (2, 2));
    let mut left: Vec<String> = all_hashes(node.pool()).await.unwrap().into_keys().collect();
    left.sort();
    let mut want = vec![path_key(&new_audio), path_key(&new_video)];
    want.sort();
    assert_eq!(left, want);
}

#[tokio::test]
async fn a_paused_node_hashes_nothing() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    node.give_song(id, YT, "Way Maker", "Sinach").await;
    set(node.pool(), "peer_transfers_paused", "true").await;
    let pass = node.hash_now().await;
    assert_eq!((pass.hashed, pass.paused), (0, true));
    assert!(all_hashes(node.pool()).await.unwrap().is_empty());
}

/// stat → hash → stat: a file whose mtime moves while it is read (every few
/// ms, during the ~1.5 s a 2 000 B/s pass reads the 3 000-byte audio) is not
/// stored; the next pass hashes it.
#[tokio::test]
async fn a_file_that_changes_while_hashed_is_skipped() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (video, audio) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    let stop = Arc::new(AtomicBool::new(false));
    let (path, flag) = (audio.clone(), stop.clone());
    let bumper = std::thread::spawn(move || {
        let mut secs = 1_900_000_000;
        while !flag.load(Ordering::Relaxed) {
            secs += 1;
            set_mtime(&path, secs);
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let pass = hash_pass(&node.ex, 2_000).await.unwrap();
    stop.store(true, Ordering::Relaxed);
    bumper.join().unwrap();
    assert_eq!((pass.hashed, pass.changed), (1, 1), "{pass:?}");
    let all = all_hashes(node.pool()).await.unwrap();
    assert!(all.contains_key(&path_key(&video)));
    assert!(!all.contains_key(&path_key(&audio)));
}

#[tokio::test]
async fn only_a_serving_node_that_is_not_paused_hashes() {
    let off = TestNode::start("snv", None).await;
    assert!(!should_hash(&off.ex).await, "not serving");
    let on = TestNode::start("snv", Some(SNV_KEY)).await;
    assert!(should_hash(&on.ex).await);
    set(on.pool(), "peer_transfers_paused", "true").await;
    assert!(!should_hash(&on.ex).await, "paused");
}
