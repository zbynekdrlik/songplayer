//! #229 `peer::audio`: what this node has of a row's audio (its size on disk,
//! its own hash while that still describes the file). The stems and lyrics
//! hooks' decisions are in `stems_tests.rs` / `lyrics_tests.rs`.

use std::time::{Duration, SystemTime};

use super::RowAudio;
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

fn row(size: Option<u64>, hashed: Option<String>) -> RowAudio {
    RowAudio { size, hashed }
}

#[tokio::test]
async fn the_rows_audio_is_its_size_and_hashed_sha() {
    let (pp, id, _) = hashed_song().await;
    assert_eq!(
        pp.ex.row_audio(id).await,
        row(Some(3_000), Some(song_audio_sha()))
    );
}

/// The same size, written again later: the old hash no longer describes it.
#[tokio::test]
async fn an_audio_rewritten_since_its_hash_has_no_hash() {
    let (pp, id, audio) = hashed_song().await;
    std::fs::write(&audio, bytes(3_000, 9)).unwrap();
    let later = SystemTime::now() + Duration::from_secs(10);
    std::fs::File::options()
        .write(true)
        .open(&audio)
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert_eq!(pp.ex.row_audio(id).await, row(Some(3_000), None));
}

#[tokio::test]
async fn an_audio_never_hashed_or_gone_reads_as_it_is() {
    let pp = TestNode::start("pp", None).await;
    let id = pp.add_video(YT).await;
    assert_eq!(
        pp.ex.row_audio(id).await,
        row(None, None),
        "no audio recorded"
    );
    let (_, audio) = pp.give_song(id, YT, "Way Maker", "Sinach").await;
    assert_eq!(
        pp.ex.row_audio(id).await,
        row(Some(3_000), None),
        "not hashed"
    );
    pp.hash_now().await;
    std::fs::remove_file(&audio).unwrap();
    assert_eq!(
        pp.ex.row_audio(id).await,
        row(None, None),
        "the file is gone"
    );
}
