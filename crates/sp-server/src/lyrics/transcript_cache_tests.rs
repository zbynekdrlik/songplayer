//! Tests for `transcript_cache.rs` (#144).

use super::*;
use std::path::Path;

fn cached(len: u64, mtime: u64, taken: u64, n_words: usize) -> CachedTranscript {
    CachedTranscript {
        wav_len: len,
        wav_mtime_ms: mtime,
        taken_at_ms: taken,
        words: (0..n_words as u64)
            .map(|i| CachedWord {
                text: format!("w{i}"),
                start_ms: i * 100,
                end_ms: i * 100 + 80,
            })
            .collect(),
    }
}

const T0: u64 = 1_000_000_000;

#[test]
fn the_same_vocal_within_the_window_is_reused() {
    assert!(reusable(&cached(10, 20, T0, 2), (10, 20), T0));
    assert!(reusable(
        &cached(10, 20, T0, 2),
        (10, 20),
        T0 + REUSE_WINDOW_MS - 1
    ));
}

#[test]
fn a_transcript_as_old_as_the_window_is_not_reused() {
    assert!(!reusable(
        &cached(10, 20, T0, 2),
        (10, 20),
        T0 + REUSE_WINDOW_MS
    ));
}

#[test]
fn another_vocal_is_never_served_the_kept_transcript() {
    assert!(!reusable(&cached(10, 20, T0, 2), (11, 20), T0));
    assert!(!reusable(&cached(10, 20, T0, 2), (10, 21), T0));
}

/// An empty transcript is never reused: the song is re-transcribed rather
/// than quarantined again from a kept empty answer.
#[test]
fn an_empty_transcript_is_not_reused() {
    assert!(!reusable(&cached(10, 20, T0, 0), (10, 20), T0));
}

#[test]
fn the_window_is_six_hours() {
    assert_eq!(REUSE_WINDOW_MS, 6 * 60 * 60 * 1000);
}

#[test]
fn the_kept_transcript_is_named_after_the_song() {
    assert_eq!(
        path(Path::new("/cache"), "yt1"),
        Path::new("/cache").join("yt1_g35t_words.json")
    );
}

#[test]
fn a_vocal_is_identified_by_its_length_and_modification_time() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("v.wav");
    std::fs::write(&wav, b"12345").unwrap();
    let meta = std::fs::metadata(&wav).unwrap();
    let (len, mtime) = vocal_identity(&meta).expect("the platform reports a modification time");
    assert_eq!(len, 5);
    let expected = meta
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert_eq!(mtime, expected);
    assert!(
        mtime > 1_600_000_000_000,
        "a real modification time: {mtime}"
    );
}

#[test]
fn now_is_the_wall_clock() {
    let before = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let now = now_ms();
    assert!(now >= before && now < before + 60_000, "{before} .. {now}");
}

/// What is stored loads back word for word.
#[tokio::test]
async fn a_stored_transcript_loads_back() {
    let dir = tempfile::tempdir().unwrap();
    let p = path(dir.path(), "yt1");
    let words = cached(0, 0, 0, 3).words();
    store(&p, (7, 8), T0, &words).await;
    let back = load(&p).await.expect("stored");
    assert_eq!(back, cached(7, 8, T0, 3));
    assert_eq!(back.words(), words);
}

#[tokio::test]
async fn nothing_kept_loads_nothing() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load(&path(dir.path(), "none")).await.is_none());
    std::fs::write(path(dir.path(), "bad"), b"{not json").unwrap();
    assert!(load(&path(dir.path(), "bad")).await.is_none());
}

#[test]
fn a_used_transcript_is_named_apart() {
    assert_eq!(
        used_path(Path::new("/cache"), "yt1"),
        Path::new("/cache").join("yt1_g35t_words_used.json")
    );
}

/// When the pass ends the kept transcript is retired: it no longer loads as
/// the kept one, and stays on disk, whole, as the used one.
#[tokio::test]
async fn a_retired_transcript_is_kept_apart_and_never_reused() {
    let dir = tempfile::tempdir().unwrap();
    let words = cached(0, 0, 0, 2).words();
    store(&path(dir.path(), "yt1"), (7, 8), T0, &words).await;
    retire(dir.path(), "yt1").await;
    assert!(load(&path(dir.path(), "yt1")).await.is_none());
    let used = load(&used_path(dir.path(), "yt1"))
        .await
        .expect("kept as used");
    assert_eq!(used, cached(7, 8, T0, 2));
}

/// Nothing kept: retiring is a no-op.
#[tokio::test]
async fn retiring_nothing_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    retire(dir.path(), "yt1").await;
    assert!(!used_path(dir.path(), "yt1").exists());
}
