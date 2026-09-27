//! #215 addendum: which paced boundaries are the song's own DECODED pairs
//! ([`PacedSink::emit`], marked live on the paced handoff) and which are
//! STANDBY pairs ([`PacedSink::emit_standby`]: the idle and pre-roll black, a
//! starve fill, the paused frozen picture). The program bus's cue gate starts
//! a fade on the incoming source's first live pair.
//!
//! It also pins the B1 finding (#215 comment 5856377673): at a song start the
//! silence before the song's audio is the PRE-ROLL (standby pairs while the
//! decoder opens), and the song's FIRST decoder pair already carries its first
//! decoded audio block (media sample 0) — nothing in the song-start path drops
//! or delays audio. Audio values encode their own media sample index
//! (`enc(s) = s + 1_000_000`, exact in `f32`; silence = 0.0), as in
//! `pacer_tests_av_align.rs`.

use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::wallclock::{SettableClock, WallClock};
use sp_ndi::AudioFrame;
use std::cell::Cell;
use std::collections::VecDeque;

/// The k-th exact-rational grid boundary at 30 fps, in 100-ns units.
fn b(k: i64) -> i64 {
    k * 10_000_000 / 30
}

const ENC_BIAS: i64 = 1_000_000;

fn enc(s: i64) -> f32 {
    (s + ENC_BIAS) as f32
}

/// A 4×2 song frame at `pts` (100 ns) whose one stereo chunk carries media
/// samples `[start, start + len)` (its 0-based media time in the timecode,
/// as `to_paced_frame` builds it).
fn song_frame(pts_100ns: i64, start: i64, len: i64) -> PacedFrame {
    PacedFrame {
        pts_ns: pts_100ns * 100,
        width: 4,
        height: 2,
        stride: 4,
        video: SharedFrame::new(vec![0u8; 12]),
        audio: vec![AudioFrame {
            data: (start..start + len)
                .flat_map(|s| [enc(s), -enc(s)])
                .collect(),
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: Some(start * 10_000_000 / 48_000),
        }],
    }
}

#[derive(Debug, PartialEq)]
enum Kind {
    Decoded,
    Standby,
}

/// One recorded boundary: which sink call carried it, its video stamp and
/// channel 0 of its audio block.
#[derive(Default)]
struct Rec {
    pairs: Vec<(Kind, i64, Vec<f32>)>,
}

fn channel0(audio: &[AudioFrame]) -> Vec<f32> {
    audio
        .iter()
        .flat_map(|a| a.data.iter().step_by(a.channels.max(1) as usize).copied())
        .collect()
}

impl PacedSink for Rec {
    fn emit(&mut self, _video: &PacedFrame, audio: &[AudioFrame], vtc: i64, _atc: i64) {
        self.pairs.push((Kind::Decoded, vtc, channel0(audio)));
    }

    fn emit_standby(&mut self, _video: &PacedFrame, audio: &[AudioFrame], vtc: i64, _atc: i64) {
        self.pairs.push((Kind::Standby, vtc, channel0(audio)));
    }
}

fn anchored() -> (Pacer, SettableClock) {
    let (wall, clk) = WallClock::settable(0);
    let mut pacer = Pacer::with_wallclock(30, true, wall);
    clk.set(0);
    pacer.anchor();
    (pacer, clk)
}

fn black_frame() -> SharedFrame {
    SharedFrame::new(vec![16u8; 12])
}

fn black(video: &SharedFrame) -> StandbyBlack<'_> {
    StandbyBlack {
        width: 4,
        height: 2,
        stride: 4,
        video,
    }
}

/// Idle b(1)..=b(3), then a Play whose decoder is ready on its 4th readiness
/// poll (3 pre-roll boundaries), then the song from b(7): frame `j` due on
/// b(7 + j), frame 0 carrying 250 ms of audio (the paced read-ahead) and each
/// later frame the next 1600 samples.
fn song_start() -> (Pacer, SettableClock, Rec) {
    let (mut pacer, clk) = anchored();
    let blk = black_frame();
    let mut rec = Rec::default();
    for k in 1..=3 {
        clk.set(b(k));
        pacer.service_standby(black(&blk).standby(), &mut rec);
    }
    let polls = Cell::new(0);
    pacer.preroll(
        black(&blk),
        &mut rec,
        || {
            polls.set(polls.get() + 1);
            (polls.get() >= 4).then_some(())
        },
        |_, until| clk.set(until),
    );
    let lead = 12_000; // 250 ms
    let mut song: VecDeque<PacedFrame> = (0..3)
        .map(|j| {
            let (start, len) = if j == 0 {
                (0, lead)
            } else {
                (lead + (j - 1) * 1600, 1600)
            };
            song_frame(b(j), start, len)
        })
        .collect();
    for k in 7..=9 {
        clk.set(b(k));
        assert_eq!(
            pacer.service(|| song.pop_front(), &mut rec),
            ServiceOutcome::Emitted,
            "b({k})"
        );
    }
    (pacer, clk, rec)
}

#[test]
fn the_pre_roll_is_standby_silence_and_the_songs_first_decoder_pair_is_its_first_block() {
    let (_, _, rec) = song_start();
    let kinds: Vec<&Kind> = rec.pairs.iter().map(|p| &p.0).collect();
    assert_eq!(
        kinds,
        [
            &Kind::Standby,
            &Kind::Standby,
            &Kind::Standby,
            &Kind::Standby,
            &Kind::Standby,
            &Kind::Standby,
            &Kind::Decoded,
            &Kind::Decoded,
            &Kind::Decoded,
        ],
        "3 idle + 3 pre-roll standby pairs, then the song's decoded pairs"
    );
    let stamps: Vec<i64> = rec.pairs.iter().map(|p| p.1).collect();
    assert_eq!(stamps, (1..=9).map(b).collect::<Vec<_>>());
    for (kind, stamp, audio) in &rec.pairs[..6] {
        assert_eq!(kind, &Kind::Standby);
        assert_eq!(audio, &vec![0.0; 1600], "{stamp}: one silent block");
    }
    for (j, (_, stamp, audio)) in rec.pairs[6..].iter().enumerate() {
        let first = j as i64 * 1600;
        let want: Vec<f32> = (first..first + 1600).map(enc).collect();
        assert_eq!(
            audio, &want,
            "{stamp}: the song's decoded pair {j} carries media samples {first}.. \
             — the first one is its first decoded block"
        );
    }
}

#[test]
fn a_paused_frozen_picture_is_a_standby_pair_and_a_stall_repeat_stays_decoded() {
    let (mut pacer, clk, mut rec) = song_start();
    // The decoder stalls on b(10): the pacer repeats the last decoded frame
    // with the song's own next block — still the song's decoded pair.
    clk.set(b(10));
    assert_eq!(pacer.service(|| None, &mut rec), ServiceOutcome::Repeated);
    // Paused on b(11): the frozen picture with a silent block.
    clk.set(b(11));
    assert_eq!(
        pacer.service_standby(Standby::FrozenLast, &mut rec),
        ServiceOutcome::Repeated
    );
    let tail: Vec<(&Kind, i64)> = rec.pairs[9..].iter().map(|p| (&p.0, p.1)).collect();
    assert_eq!(
        tail,
        [(&Kind::Decoded, b(10)), (&Kind::Standby, b(11))],
        "a stall repeat is decoded content, a paused boundary is standby"
    );
    assert_eq!(
        rec.pairs[9].2,
        (4800..6400).map(enc).collect::<Vec<_>>(),
        "the repeat carries the song's next block"
    );
    assert_eq!(
        rec.pairs[10].2,
        vec![0.0; 1600],
        "the paused block is silent"
    );
}

#[test]
fn a_sink_that_only_emits_gets_the_standby_pairs_through_emit() {
    // The default `emit_standby` is `emit`: the NDI submitter and every
    // emit-only sink see the same pairs as before.
    #[derive(Default)]
    struct EmitOnly {
        stamps: Vec<i64>,
    }
    impl PacedSink for EmitOnly {
        fn emit(&mut self, _video: &PacedFrame, _audio: &[AudioFrame], vtc: i64, _atc: i64) {
            self.stamps.push(vtc);
        }
    }
    let (mut pacer, clk) = anchored();
    let blk = black_frame();
    let mut sink = EmitOnly::default();
    clk.set(b(1));
    pacer.service_standby(black(&blk).standby(), &mut sink);
    let picture = song_frame(0, 0, 0);
    sink.emit_standby(&picture, &[], b(2), b(2));
    assert_eq!(sink.stamps, vec![b(1), b(2)]);
}
