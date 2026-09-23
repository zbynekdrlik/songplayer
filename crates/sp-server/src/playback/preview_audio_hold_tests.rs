//! Unit tests for the #184 round-G3 preview audio hold (pure, Linux-runnable).

use super::*;

/// Stereo frames per millisecond (48 kHz).
const F: u64 = 48;
/// The SDK-clocked seam lead (`lead_ms_for(false)`).
const LEAD: u32 = 1500;

/// An interleaved-stereo block of `frames` frames, every sample `v`.
fn block(frames: usize, v: f32) -> Vec<f32> {
    vec![v; frames * 2]
}

/// Total stereo frames of content among `writes`.
fn content_frames(writes: &[AudioWrite]) -> usize {
    writes
        .iter()
        .map(|w| match w {
            AudioWrite::Samples(s) => s.len() / 2,
            AudioWrite::Silence(_) => 0,
        })
        .sum()
}

#[test]
fn write_ahead_is_200ms_and_never_more_than_the_lead() {
    assert_eq!(AUDIO_WRITE_AHEAD_MS, 200);
    assert_eq!(MAX_HELD_BLOCKS, 512);
    assert_eq!(AudioHold::new(0, LEAD).write_ahead_ms(), 200);
    // A lead shorter than the write-ahead caps it (never write ahead of content
    // that does not exist yet); the paced path has no lead at all.
    assert_eq!(AudioHold::new(0, 100).write_ahead_ms(), 100);
    assert_eq!(AudioHold::new(0, 0).write_ahead_ms(), 0);
}

#[test]
fn timeline_positions_are_exact() {
    let h = AudioHold::new(1_000, LEAD);
    // position = base + (now + write_ahead) × 48/ms
    assert_eq!(h.position_at(0), 1_000 + 200 * F);
    assert_eq!(h.position_at(1_000_000), 1_000 + 1_200 * F);
    assert_eq!(h.position_at(1_500), 1_000 + 201 * F + F / 2);
    // target = base + (arrival + lead) × 48/ms
    assert_eq!(h.block_target(0), 1_000 + 1_500 * F);
    assert_eq!(h.block_target(500_000), 1_000 + 2_000 * F);
    // due = arrival + lead − write_ahead
    assert_eq!(h.due_us(0), 1_300_000);
    assert_eq!(h.due_us(500_000), 1_800_000);
    // The paced path (no lead): due on arrival, placed at its arrival.
    let p = AudioHold::new(7, 0);
    assert_eq!(p.due_us(7_000), 7_000);
    assert_eq!(p.block_target(7_000), 7 + 7 * F);
    assert_eq!(p.position_at(7_000), 7 + 7 * F);
}

#[test]
fn a_block_is_due_exactly_when_the_position_reaches_its_target() {
    let h = AudioHold::new(96, LEAD);
    for arrival in [0u64, 1, 33_333, 999_999, 12_345_678] {
        assert_eq!(h.position_at(h.due_us(arrival)), h.block_target(arrival));
    }
}

#[test]
fn only_silence_is_written_before_a_block_is_due() {
    // THE round-G3 bug: G2 wrote a block (behind a lead-long silence) the moment
    // it arrived, parking the whole 1.5 s lead in the socket. Held, the socket
    // only ever gets the write-ahead of silence before the block is due.
    let mut h = AudioHold::new(0, LEAD);
    h.push(0, block(1_600, 0.5));
    let early = h.take_writes(0);
    assert_eq!(early, vec![AudioWrite::Silence((200 * F) as usize)]);
    assert_eq!(h.held_frames(), 1_600);
    let later = h.take_writes(1_299_999);
    assert_eq!(content_frames(&later), 0, "not due until 1.3 s");
    assert_eq!(h.written_frames(), (1_299_999 + 200_000) * F / 1_000);
}

#[test]
fn a_due_block_is_written_at_its_arrival_target() {
    let mut h = AudioHold::new(0, LEAD);
    h.push(0, block(1_600, 0.5));
    let _ = h.take_writes(1_299_999); // silence up to 71 999
    let w = h.take_writes(1_300_000);
    assert_eq!(w, vec![AudioWrite::Samples(block(1_600, 0.5))]);
    assert_eq!(h.written_frames(), 71_999 + 1_600);
    assert_eq!(h.held_frames(), 0);
    assert_eq!(h.skipped_frames(), 0);
    assert_eq!(h.padded_frames(), 71_999);
}

#[test]
fn a_late_written_block_pads_up_to_its_target_first() {
    // The feeder was blocked (nothing written): the due block still lands at its
    // ARRIVAL target, with silence in front — the encoder consumed nothing
    // meanwhile, so the content keeps its place on the timeline.
    let mut h = AudioHold::new(0, LEAD);
    h.push(0, block(1_600, 0.5));
    let w = h.take_writes(1_300_000);
    assert_eq!(
        w,
        vec![
            AudioWrite::Silence((1_500 * F) as usize),
            AudioWrite::Samples(block(1_600, 0.5)),
        ]
    );
}

#[test]
fn a_block_that_waited_past_its_target_is_trimmed_not_delayed() {
    // The block sat in the channel while silence already went out past its
    // target (the round-G3 box case): placed by ARRIVAL it is stale and must be
    // dropped, never appended seconds late.
    let mut h = AudioHold::new(0, LEAD);
    let _ = h.take_writes(2_000_000); // silence up to 2.2 s
    assert_eq!(h.written_frames(), 2_200 * F);
    h.push(300_000, block(1_600, 0.5)); // target 1.8 s — 400 ms behind
    let w = h.take_writes(2_000_000);
    assert_eq!(content_frames(&w), 0);
    assert_eq!(h.skipped_frames(), 1_600);
    // ≤ 300 ms stale is still written (the G2 skip band), whole.
    h.push(500_000, block(1_600, 0.25)); // target 2.0 s — 200 ms behind
    let w = h.take_writes(2_000_000);
    assert_eq!(w, vec![AudioWrite::Samples(block(1_600, 0.25))]);
    assert_eq!(h.skipped_frames(), 1_600);
}

#[test]
fn a_burst_keeps_300ms_and_drops_the_rest() {
    let mut h = AudioHold::new(0, LEAD);
    for _ in 0..30 {
        h.push(1_000_000, block(1_600, 0.5)); // 1 s of audio, one instant
    }
    let w = h.take_writes(2_300_000);
    // silence up to the target (2.5 s), then 9 whole blocks (300 ms) fit
    // before a block would end > 300 ms past the target; the rest is dropped.
    assert_eq!(w[0], AudioWrite::Silence((2_500 * F) as usize));
    assert_eq!(content_frames(&w), 9 * 1_600);
    assert_eq!(w.len(), 10);
    assert_eq!(h.skipped_frames(), 21 * 1_600);
    assert_eq!(h.written_frames(), 2_500 * F + 9 * 1_600);
}

#[test]
fn an_odd_length_block_writes_whole_frames_only() {
    let mut h = AudioHold::new(0, 0);
    h.push(0, vec![0.1, 0.2, 0.3, 0.4, 0.5]);
    let w = h.take_writes(0);
    assert_eq!(w, vec![AudioWrite::Samples(vec![0.1, 0.2, 0.3, 0.4])]);
    assert_eq!(h.written_frames(), 2);
}

#[test]
fn the_paced_path_writes_on_arrival_at_the_wall() {
    let mut h = AudioHold::new(0, 0);
    h.push(1_000_000, block(960, 0.5));
    let w = h.take_writes(1_000_000);
    assert_eq!(
        w,
        vec![
            AudioWrite::Silence((1_000 * F) as usize),
            AudioWrite::Samples(block(960, 0.5)),
        ]
    );
}

#[test]
fn wait_is_the_time_until_the_oldest_block_is_due_capped() {
    let mut h = AudioHold::new(0, LEAD);
    assert_eq!(h.wait_us(5, 200_000), 200_000, "nothing held");
    h.push(0, block(10, 0.5)); // due 1.3 s
    h.push(1_000_000, block(10, 0.5));
    assert_eq!(h.wait_us(1_000_000, 200_000), 200_000);
    assert_eq!(h.wait_us(1_250_000, 200_000), 50_000);
    assert_eq!(h.wait_us(1_300_000, 200_000), 0);
    assert_eq!(h.wait_us(1_400_000, 200_000), 0);
}

#[test]
fn the_cap_drops_the_oldest_held_block() {
    let mut h = AudioHold::new(0, LEAD);
    h.push(0, block(3, 0.5)); // the oldest — 3 frames
    for i in 1..MAX_HELD_BLOCKS as u64 {
        h.push(i, block(1, 0.5));
    }
    assert_eq!(
        h.dropped_blocks(),
        0,
        "exactly at the cap nothing is dropped"
    );
    assert_eq!(h.held_frames(), 3 + (MAX_HELD_BLOCKS as u64 - 1));
    h.push(9_999, block(1, 0.5));
    assert_eq!(h.dropped_blocks(), 1);
    assert_eq!(h.skipped_frames(), 3, "the OLDEST (3-frame) block went");
    assert_eq!(h.held_frames(), MAX_HELD_BLOCKS as u64);
}

#[test]
fn in_flight_audio_stays_within_the_write_ahead_for_a_minute() {
    // A steady 30 fps seam (1600-frame blocks, stamped on arrival) and an
    // encoder that consumes audio in step with its wall-clock video. The audio
    // written beyond what the encoder consumed is what must sit in the socket:
    // G2 parked the whole 1.5 s lead there (the box's small loopback buffers
    // could not hold it); held, it never exceeds write-ahead + 300 ms, and a
    // steady seam never loses a sample.
    let mut h = AudioHold::new(0, LEAD);
    let bound = (AUDIO_WRITE_AHEAD_MS + 300) * F;
    let mut max_in_flight = 0u64;
    let mut content = 0usize;
    let step_us = 33_333u64;
    for k in 0..1_800u64 {
        let now = k * step_us;
        h.push(now, block(1_600, 0.5));
        content += content_frames(&h.take_writes(now));
        let consumed = now * F / 1_000;
        let in_flight = h.written_frames().saturating_sub(consumed);
        max_in_flight = max_in_flight.max(in_flight);
        assert!(
            in_flight <= bound,
            "at {now} µs {in_flight} frames in flight > {bound}"
        );
    }
    assert!(
        max_in_flight >= AUDIO_WRITE_AHEAD_MS * F,
        "the write-ahead is kept"
    );
    assert_eq!(h.skipped_frames(), 0, "a steady seam loses nothing");
    // Every block not still held (the last ~1.3 s) was written in full.
    assert_eq!(content as u64 + h.held_frames(), 1_800 * 1_600);
    assert!(
        h.padded_frames() <= u64::from(LEAD) * F,
        "only the lead was padded"
    );
}
