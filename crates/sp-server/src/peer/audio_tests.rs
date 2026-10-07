//! #229 `peer::audio`: a row's audio file as this node finds it on disk, and
//! this node's hash of it (the stored one while it holds, else hashed now
//! and stored). The stems and lyrics hooks' decisions are in
//! `stems_tests.rs` / `lyrics_tests.rs`.

use std::time::{Duration, SystemTime};

use crate::db::models_peer::hash_of;
use crate::peer::hash::sha256_hex;
use crate::peer::rig::{TestNode, bytes, song_audio_sha};

const YT: &str = "aaaaaaaaaaa";

/// PP with one downloaded song; its row id and audio path.
async fn song() -> (TestNode, i64, std::path::PathBuf) {
    let pp = TestNode::start("pp", None).await;
    let id = pp.add_video(YT).await;
    let (_, audio) = pp.give_song(id, YT, "Way Maker", "Sinach").await;
    (pp, id, audio)
}

#[tokio::test]
async fn the_rows_audio_is_its_recorded_file_as_on_disk() {
    let (pp, id, audio) = song().await;
    let row = pp.ex.row_audio(id).await.expect("the audio is on disk");
    assert_eq!(row.path, audio.to_string_lossy());
    assert_eq!(row.size, 3_000);
    let modified = std::fs::metadata(&audio).unwrap().modified().unwrap();
    let ms = modified
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    assert_eq!(row.mtime_ms, i64::try_from(ms).unwrap());
}

#[tokio::test]
async fn a_row_with_no_audio_on_disk_has_none() {
    let pp = TestNode::start("pp", None).await;
    let id = pp.add_video(YT).await;
    assert_eq!(pp.ex.row_audio(id).await, None, "no audio recorded");
    let (_, audio) = pp.give_song(id, YT, "Way Maker", "Sinach").await;
    std::fs::remove_file(&audio).unwrap();
    assert_eq!(pp.ex.row_audio(id).await, None, "the file is gone");
}

/// A node that runs no hasher (PP in phase 1) hashes the row's audio when
/// asked, and stores the entry as the hasher would.
#[tokio::test]
async fn an_audio_never_hashed_is_hashed_now_and_stored() {
    let (pp, id, audio) = song().await;
    let path = audio.to_string_lossy().to_string();
    assert_eq!(hash_of(pp.pool(), &path).await.unwrap(), None);
    let row = pp.ex.row_audio(id).await.unwrap();
    assert_eq!(pp.ex.audio_sha(&row).await, Some(song_audio_sha()));
    let stored = hash_of(pp.pool(), &path)
        .await
        .unwrap()
        .expect("the entry is stored");
    assert_eq!((stored.size, stored.sha256), (3_000, song_audio_sha()));
}

/// A stored entry that still holds is used as it is: its sha (here a
/// planted one) is answered, the file is not hashed again.
#[tokio::test]
async fn a_stored_hash_that_holds_is_used() {
    let (pp, id, audio) = song().await;
    pp.hash_now().await;
    let planted = "0123456789abcdef".repeat(4);
    sqlx::query("UPDATE peer_hashes SET sha256 = ? WHERE path = ?")
        .bind(&planted)
        .bind(audio.to_string_lossy().to_string())
        .execute(pp.pool())
        .await
        .unwrap();
    let row = pp.ex.row_audio(id).await.unwrap();
    assert_eq!(pp.ex.audio_sha(&row).await, Some(planted));
}

/// The same size, written again later: the old entry no longer describes
/// it, so the file is hashed again and the entry replaced.
#[tokio::test]
async fn an_audio_rewritten_since_its_hash_is_hashed_again() {
    let (pp, id, audio) = song().await;
    pp.hash_now().await;
    std::fs::write(&audio, bytes(3_000, 9)).unwrap();
    let later = SystemTime::now() + Duration::from_secs(10);
    std::fs::File::options()
        .write(true)
        .open(&audio)
        .unwrap()
        .set_modified(later)
        .unwrap();
    let new_sha = sha256_hex(&bytes(3_000, 9));
    let row = pp.ex.row_audio(id).await.unwrap();
    assert_eq!(pp.ex.audio_sha(&row).await, Some(new_sha.clone()));
    let stored = hash_of(pp.pool(), &audio.to_string_lossy())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.sha256, new_sha);
}
