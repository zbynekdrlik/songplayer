//! #229 `peer::audio`: this node's hash of a row's audio, read from its
//! `peer_hashes` entry only while that still describes the file. The stems
//! and lyrics hooks' decisions are in `stems_tests.rs` / `lyrics_tests.rs`.

use std::time::{Duration, SystemTime};

use crate::peer::rig::{TestNode, bytes, song_audio_sha};

const YT: &str = "aaaaaaaaaaa";

/// PP with one downloaded song, hashed; its row id and audio path.
async fn hashed_song() -> (TestNode, i64, std::path::PathBuf) {
    let pp = TestNode::start("pp", None).await;
    let id = pp.add_video(YT).await;
    let (_, audio) = pp.give_song(id, YT, "Way Maker", "Sinach").await;
    pp.hash_now().await;
    (pp, id, audio)
}

#[tokio::test]
async fn the_rows_audio_hash_is_its_hashed_sha() {
    let (pp, id, _) = hashed_song().await;
    assert_eq!(pp.ex.audio_hash(id).await, Some(song_audio_sha()));
}

/// The same size, written again later: the old hash no longer describes it.
#[tokio::test]
async fn an_audio_rewritten_since_its_hash_has_none() {
    let (pp, id, audio) = hashed_song().await;
    std::fs::write(&audio, bytes(3_000, 9)).unwrap();
    let later = SystemTime::now() + Duration::from_secs(10);
    std::fs::File::options()
        .write(true)
        .open(&audio)
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert_eq!(pp.ex.audio_hash(id).await, None);
}

#[tokio::test]
async fn an_audio_never_hashed_or_gone_has_none() {
    let pp = TestNode::start("pp", None).await;
    let id = pp.add_video(YT).await;
    assert_eq!(pp.ex.audio_hash(id).await, None, "no audio recorded");
    let (_, audio) = pp.give_song(id, YT, "Way Maker", "Sinach").await;
    assert_eq!(pp.ex.audio_hash(id).await, None, "not hashed");
    pp.hash_now().await;
    std::fs::remove_file(&audio).unwrap();
    assert_eq!(pp.ex.audio_hash(id).await, None, "the file is gone");
}
