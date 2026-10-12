//! #257: a block cut short is counted by where the song is — `start` before
//! the map's first full block, `mid` once the audio flowed — and once the
//! source ENDED it is the song's own end, never an underrun. A song stopped
//! before its source ended counts one `end` underrun. `super::*` resolves to
//! the `audio_grid` module under test.

use super::*;

/// A one-channel ramp `start, start+1, …` (exact integer-valued `f32`s).
fn ramp(start: i64, n: usize) -> Vec<Vec<f32>> {
    vec![(0..n as i64).map(|i| (start + i) as f32).collect()]
}

/// `(start, mid, end, total)` of `buf`'s underruns.
fn phases(buf: &AudioGridBuffer) -> (u64, u64, u64, u64) {
    (
        buf.underruns_start(),
        buf.underruns_mid(),
        buf.underruns_end(),
        buf.underruns(),
    )
}

#[test]
fn a_short_take_before_the_first_full_block_is_a_start_underrun_then_mid() {
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push(&ramp(0, 800));
    buf.take_boundary_chunk(1600); // short, nothing has flowed yet
    assert_eq!(phases(&buf), (1, 0, 0, 1));

    buf.push(&ramp(800, 3200));
    buf.take_boundary_chunk(1600); // full: the audio flows
    buf.take_boundary_chunk(1600); // full, the buffer is empty after it
    assert_eq!(phases(&buf), (1, 0, 0, 1), "full blocks count nothing");

    buf.push(&ramp(4000, 400));
    buf.take_boundary_chunk(1600); // short mid-song
    buf.take_boundary_chunk(1600); // empty mid-song
    assert_eq!(phases(&buf), (1, 2, 0, 3));
}

#[test]
fn once_the_source_ended_a_short_block_is_the_songs_end_not_an_underrun() {
    // The decoder handed out the song's last sample (2000): the block it ends
    // in plays its 400 real samples, then padding, and nothing is counted.
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push(&ramp(0, 2000));
    assert_eq!(buf.take_boundary_chunk(1600)[0], ramp(0, 1600)[0]);
    buf.end_source();
    let last = buf.take_boundary_chunk(1600);
    assert_eq!(
        last[0][..400],
        ramp(1600, 400)[0][..],
        "the song's last samples"
    );
    assert!(last[0][400..].iter().all(|&s| s == 0.0), "then padding");
    let after = buf.take_boundary_chunk(1600);
    assert!(after[0].iter().all(|&s| s == 0.0), "after the end: silence");
    assert_eq!(
        phases(&buf),
        (0, 0, 0, 0),
        "no sample of the song is missing"
    );
}

#[test]
fn a_clear_starts_the_next_map_in_the_start_phase_with_a_live_source() {
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push(&ramp(0, 3200));
    buf.take_boundary_chunk(1600);
    buf.end_source();
    buf.clear(); // a seek, a new song, a resume
    buf.push(&ramp(0, 800));
    buf.take_boundary_chunk(1600);
    assert_eq!(
        phases(&buf),
        (1, 0, 0, 1),
        "a new map: nothing flowed yet, and its source is live"
    );
}

#[test]
fn a_full_block_through_the_correction_or_past_a_skipped_one_starts_the_flow() {
    // The corrected (interpolated) take consumes 1648 for 1600: the audio flows.
    let mut corrected = AudioGridBuffer::new(48_000);
    corrected.push(&ramp(0, 1648 + 800));
    assert_eq!(
        corrected.take_block(1600, 48).1,
        48,
        "the correction applied"
    );
    corrected.take_boundary_chunk(1600);
    assert_eq!(phases(&corrected), (0, 1, 0, 1), "mid-song, not the start");

    // 1620 buffered: too few for the correction, a full block all the same.
    let mut skipped = AudioGridBuffer::new(48_000);
    skipped.push(&ramp(0, 1620));
    assert_eq!(skipped.take_block(1600, 48).1, 0, "the correction skipped");
    skipped.take_boundary_chunk(1600);
    assert_eq!(phases(&skipped), (0, 1, 0, 1), "mid-song, not the start");
}

#[test]
fn a_song_stopped_before_its_source_ended_counts_one_end_underrun_and_no_more() {
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push(&ramp(0, 2000));
    buf.take_boundary_chunk(1600);
    buf.end_song(); // the decoder never reached the audio's end
    assert_eq!(phases(&buf), (0, 0, 1, 1));
    buf.take_boundary_chunk(1600); // the play-out's short last block
    buf.end_song();
    assert_eq!(phases(&buf), (0, 0, 1, 1), "the cut is counted once");
}

#[test]
fn a_song_whose_source_ended_counts_no_end_underrun() {
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push(&ramp(0, 1600));
    buf.take_boundary_chunk(1600);
    buf.end_source();
    buf.end_song();
    assert_eq!(phases(&buf), (0, 0, 0, 0));
}

#[test]
fn the_total_adds_every_phase() {
    let mut buf = AudioGridBuffer::new(48_000);
    buf.push(&ramp(0, 800));
    buf.take_boundary_chunk(1600);
    buf.take_boundary_chunk(1600); // still nothing flowed: start 2
    buf.push(&ramp(800, 1600));
    buf.take_boundary_chunk(1600);
    buf.take_boundary_chunk(1600); // mid 1
    buf.end_song(); // end 1
    buf.clear();
    buf.end_song(); // end 2: a new map, its source live
    buf.clear();
    buf.end_song(); // end 3
    assert_eq!(phases(&buf), (2, 1, 3, 6));
}
