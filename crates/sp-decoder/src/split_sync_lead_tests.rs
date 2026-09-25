//! #148 v4 — the paced audio read-ahead (`audio_lead_ms`).
//!
//! The paced pipeline reads audio `lead` ms ahead of each video frame so the
//! pacer's media-time grid buffer holds a real cushion across a video decode
//! stall. The G5 read gate (#184) still bounds it: audio is read only when
//! nothing waits in `pending_audio`, so the read-ahead never exceeds the lead
//! plus one chunk. The pacing-OFF constructors keep the 40 ms pairing deadline.
//!
//! Nested in `split_sync_tests.rs` (1000-line cap) to reuse its mock readers.

use super::{CountingAudio, G5_CHUNK_MS, MockAudio, MockVideo};
use crate::split_sync::{DEFAULT_TOLERANCE_MS, SplitSyncedDecoder};
use crate::types::DecodedAudioFrame;

/// The paced lead these tests pin (production: `pacer::PACED_AUDIO_LEAD_MS`).
const LEAD_MS: u64 = 250;

fn ts_of(audio: &[DecodedAudioFrame]) -> Vec<u64> {
    audio.iter().map(|a| a.timestamp_ms).collect()
}

#[test]
fn with_audio_lead_pairs_audio_up_to_video_ts_plus_the_lead() {
    // 48 ms chunks from 0. Frame 0 (deadline 250) gets 0..=240, 288 waits;
    // frame 33 (deadline 283) finds 288 still waiting and reads nothing; frame
    // 66 (deadline 316) gets 288 and 336 waits.
    let chunks: Vec<u64> = (0..40).map(|j| j * G5_CHUNK_MS).collect();
    let v = Box::new(MockVideo::new(&[0, 33, 66]));
    let a = Box::new(MockAudio::new(&chunks, 1900));
    let mut dec = SplitSyncedDecoder::with_audio_lead(v, a, LEAD_MS).unwrap();
    assert_eq!(dec.audio_lead_ms(), LEAD_MS);

    let (f0, a0) = dec.next_synced().unwrap().unwrap();
    assert_eq!(f0.timestamp_ms, 0);
    assert_eq!(ts_of(&a0), vec![0, 48, 96, 144, 192, 240]);
    assert_eq!(dec.pending_audio.len(), 1, "288 waits past deadline 250");

    let (f1, a1) = dec.next_synced().unwrap().unwrap();
    assert_eq!(f1.timestamp_ms, 33);
    assert!(a1.is_empty(), "288 > 283 stays pending");

    let (f2, a2) = dec.next_synced().unwrap().unwrap();
    assert_eq!(f2.timestamp_ms, 66);
    assert_eq!(ts_of(&a2), vec![288]);
    assert_eq!(dec.pending_audio.len(), 1, "336 waits past deadline 316");
}

#[test]
fn with_audio_lead_deadline_is_inclusive_at_exactly_the_lead() {
    // A chunk exactly at video_ts + lead is paired; one ms later waits.
    let v = Box::new(MockVideo::new(&[100]));
    let a = Box::new(MockAudio::new(&[350, 351], 400));
    let mut dec = SplitSyncedDecoder::with_audio_lead(v, a, LEAD_MS).unwrap();
    let (_f, audio) = dec.next_synced().unwrap().unwrap();
    assert_eq!(ts_of(&audio), vec![350], "100 + 250 is inside the lead");
    assert_eq!(dec.pending_audio.len(), 1, "351 waits");
}

#[test]
fn lead_read_ahead_stays_within_lead_plus_one_chunk_over_10k_frames() {
    // 10 000 integer-ms 30-fps frames against contiguous 48 ms chunks that run
    // past the last deadline (the reader never runs dry). On EVERY call:
    // - the reader has read PAST the deadline (the cushion reaches the lead);
    // - it never read more than one chunk beyond it (G5 bound kept);
    // - at most one chunk waits in `pending_audio`;
    // - the frame gets exactly the chunks in (previous deadline, deadline].
    const FRAMES: u64 = 10_000;
    let vframes: Vec<u64> = (0..FRAMES).map(|k| k * 1000 / 30).collect();
    let last_deadline = vframes[vframes.len() - 1] + LEAD_MS;
    let n_chunks = last_deadline / G5_CHUNK_MS + 3;
    let chunks: Vec<u64> = (0..n_chunks).map(|j| j * G5_CHUNK_MS).collect();
    let audio = CountingAudio::new(chunks.clone());
    let max_ts_read = std::sync::Arc::clone(&audio.max_ts_read);
    let mut dec = SplitSyncedDecoder::with_audio_lead(
        Box::new(MockVideo::new(&vframes)),
        Box::new(audio),
        LEAD_MS,
    )
    .expect("valid mock readers");

    // Index of the first chunk not yet delivered: a frame's expected chunks are
    // the undelivered ones up to its deadline, i.e. (previous deadline, deadline].
    let mut next_chunk = 0usize;
    let mut delivered_all: Vec<u64> = Vec::new();
    let mut calls = 0u64;
    while let Some((frame, audio)) = dec.next_synced().unwrap() {
        let deadline = frame.timestamp_ms + LEAD_MS;
        let read = max_ts_read.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            read > deadline,
            "frame {calls} (ts {}): audio read only up to {read} ms, short of the \
             {LEAD_MS} ms lead (deadline {deadline})",
            frame.timestamp_ms
        );
        assert!(
            read <= deadline + G5_CHUNK_MS,
            "frame {calls}: audio read up to {read} ms, more than one \
             {G5_CHUNK_MS} ms chunk past the deadline {deadline}"
        );
        assert!(
            dec.pending_audio.len() <= 1,
            "frame {calls}: pending_audio grew to {}",
            dec.pending_audio.len()
        );
        let expected: Vec<u64> = chunks[next_chunk..]
            .iter()
            .copied()
            .take_while(|&ts| ts <= deadline)
            .collect();
        assert_eq!(
            ts_of(&audio),
            expected,
            "frame {calls} (deadline {deadline}): wrong audio pairing"
        );
        next_chunk += expected.len();
        delivered_all.extend(expected);
        calls += 1;
    }
    assert_eq!(calls, FRAMES, "every video frame is delivered");
    let expected_all: Vec<u64> = chunks
        .iter()
        .copied()
        .take_while(|&ts| ts <= last_deadline)
        .collect();
    assert_eq!(
        delivered_all, expected_all,
        "every chunk up to the last deadline exactly once, in order"
    );
}

#[test]
fn new_keeps_the_40_ms_pairing_deadline() {
    // The pacing-OFF path without the emitter (`new`, and `with_audio_lead`
    // via `decoder_tolerance_ms(false)`) is unchanged: 40 ms, not the paced lead.
    assert_eq!(DEFAULT_TOLERANCE_MS, 40);
    let v = Box::new(MockVideo::new(&[0]));
    let a = Box::new(MockAudio::new(&[40, 41], 100));
    let mut dec = SplitSyncedDecoder::new(v, a).unwrap();
    assert_eq!(dec.audio_lead_ms(), DEFAULT_TOLERANCE_MS);
    let (_f, audio) = dec.next_synced().unwrap().unwrap();
    assert_eq!(ts_of(&audio), vec![40]);
    assert_eq!(
        dec.pending_audio.len(),
        1,
        "41 waits past the 40 ms deadline"
    );

    let v = Box::new(MockVideo::new(&[0]));
    let a = Box::new(MockAudio::new(&[0], 100));
    let dec = SplitSyncedDecoder::with_audio_lead(v, a, 1540).unwrap();
    assert_eq!(
        dec.audio_lead_ms(),
        1540,
        "the emitter path's 1540 ms deadline is kept verbatim"
    );
}
