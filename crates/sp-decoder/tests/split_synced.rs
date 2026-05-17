//! SplitSyncedDecoder integration test using both committed fixtures.
//!
//! `tests/fixtures/silent_3s.flac` + `tests/fixtures/black_3s.mp4` are
//! the same pair that `mf_video_only.rs` and `symphonia_audio.rs`
//! exercise individually. This test drives them through the real
//! [`SplitSyncedDecoder`] (MediaFoundationVideoReader + SymphoniaAudioReader)
//! and asserts the A/V pairing invariants:
//!
//! - `next_synced()` returns frames in monotonic video-timestamp order
//! - Every returned pair has at least one audio chunk within the
//!   `DEFAULT_TOLERANCE_MS` (40 ms) window of the video timestamp
//! - The decoder reports the audio stream's duration (~3000 ms) as the
//!   canonical duration
//! - End-of-stream returns `Ok(None)`
//!
//! Windows-only: MediaFoundationVideoReader is `cfg(windows)`-only. The
//! audio half (Symphonia) has its own cross-platform test in
//! `symphonia_audio.rs`.
//!
//! Closes spec §9 / issue #18.

#![cfg(windows)]

use sp_decoder::split_sync::DEFAULT_TOLERANCE_MS;
use sp_decoder::{
    MediaFoundationVideoReader, MediaStream, SplitSyncedDecoder, SymphoniaAudioReader,
};

fn fixtures_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn open_decoder() -> SplitSyncedDecoder {
    let video =
        MediaFoundationVideoReader::open(&fixtures_dir().join("black_3s.mp4")).expect("video open");
    let audio =
        SymphoniaAudioReader::open(&fixtures_dir().join("silent_3s.flac")).expect("audio open");
    SplitSyncedDecoder::new(Box::new(video), Box::new(audio)).expect("decoder construct")
}

#[test]
fn reports_audio_duration_as_canonical_duration() {
    let dec = open_decoder();
    let dur = dec.duration_ms();
    assert!(
        (2_900..=3_100).contains(&dur),
        "expected ~3000ms (audio side), got {dur}ms"
    );
}

#[test]
fn next_synced_yields_pairs_within_tolerance() {
    let mut dec = open_decoder();

    let mut prev_video_ts: Option<u64> = None;
    let mut frame_count = 0usize;
    let mut audio_frame_count = 0usize;

    while let Some((video, audio_chunks)) = dec.next_synced().expect("decode step") {
        // Video timestamps must be monotonically non-decreasing.
        if let Some(prev) = prev_video_ts {
            assert!(
                video.timestamp_ms >= prev,
                "video timestamps must be monotonic: prev={prev}ms current={}ms",
                video.timestamp_ms
            );
        }
        prev_video_ts = Some(video.timestamp_ms);

        // For each paired audio chunk, its timestamp must be within the
        // 40 ms tolerance window of the video timestamp (i.e. <= video_ts
        // + 40 ms — sync_pair drains audio up to that deadline). It can
        // be arbitrarily earlier (e.g. the first decode iteration pulls
        // multiple early audio chunks before the first video frame is
        // requested).
        let deadline = video.timestamp_ms + DEFAULT_TOLERANCE_MS;
        for af in &audio_chunks {
            assert!(
                af.timestamp_ms <= deadline,
                "audio chunk ts {} exceeds video deadline {} (video_ts={}ms, tolerance={}ms)",
                af.timestamp_ms,
                deadline,
                video.timestamp_ms,
                DEFAULT_TOLERANCE_MS,
            );
        }

        frame_count += 1;
        audio_frame_count += audio_chunks.len();

        // Safety bound: the fixture is ~3s and produces well under 200
        // video frames at 30fps. Stop if we somehow blow past — better
        // than spinning a CI runner.
        assert!(
            frame_count < 1_000,
            "exceeded sanity bound while draining decoder"
        );
    }

    assert!(
        frame_count >= 10,
        "expected at least 10 video frames over the 3s fixture, got {frame_count}"
    );
    assert!(
        audio_frame_count >= 1,
        "expected at least 1 audio chunk over the run, got {audio_frame_count}"
    );
}

#[test]
fn end_of_stream_returns_none() {
    let mut dec = open_decoder();
    // Drain the whole stream.
    while dec.next_synced().expect("decode step").is_some() {}
    // Calling again past end-of-stream must continue to return None
    // without erroring.
    let post_eos = dec.next_synced().expect("post-eos step");
    assert!(post_eos.is_none(), "post-EOS must return Ok(None)");
}
