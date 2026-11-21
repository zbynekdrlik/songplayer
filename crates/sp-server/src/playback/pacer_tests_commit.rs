//! #289 step 1: characterization of every (picture, audio) pair the pacer
//! emits, through the paced emit loop's own order (`pipeline_paced::
//! decode_and_send_paced`, `pipeline_paced_idle::run_idle_wait`): per
//! boundary the command first (pause, resume, seek, stop), then
//! `prepare(target, pop)`, the sleep to the target, `service`, the end check
//! (`drained && !has_pending`) and the play-out; the idle standby and the
//! song-start pre-roll around it.
//!
//! Each scenario's pairs are pinned by a fingerprint taken over the pacer
//! BEFORE step 1 (a 64-bit FNV-1a of every pair's kind, stamps, picture,
//! blend and audio samples, in order), so a later pacer must give exactly the
//! same pairs.

use std::collections::VecDeque;

use sp_core::genlock::strict_next_boundary_100ns;
use sp_ndi::AudioFrame;

use crate::playback::frame_buf::SharedFrame;
use crate::playback::pacer::{
    AUDIO_LEAD_SLOTS, FrameBlend, PacedFrame, PacedSink, Pacer, ServiceOutcome, Standby,
    StandbyBlack,
};
use crate::playback::picture::Picture;
use crate::playback::wallclock::{SettableClock, WallClock};

/// A bound on every loop here: every scenario ends long before it.
const MAX_BOUNDARIES: usize = 2_000;

/// The standby black's id (its first two bytes).
const BLACK_ID: u16 = 0xEEEE;

/// The audio rate of every song here.
const RATE_HZ: i64 = 48_000;

/// Which `PacedSink` method a pair left through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Emit,
    Standby,
    Blended,
    Shared,
}

/// One emitted pair: what a consumer of the sink sees.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pair {
    kind: Kind,
    vtc: i64,
    atc: i64,
    pic: u16,
    size: (u32, u32, u32),
    pts_ns: i64,
    blend: Option<(u16, u32)>,
    /// Per audio frame: channels, then every sample's bits.
    audio: Vec<Vec<u32>>,
}

/// The picture's id: its first two bytes.
fn pic_id(p: &Picture) -> u16 {
    let b = p.expect_cpu();
    u16::from_le_bytes([b[0], b[1]])
}

fn audio_bits(audio: &[AudioFrame]) -> Vec<Vec<u32>> {
    audio
        .iter()
        .map(|a| {
            let mut v = vec![a.channels, a.sample_rate];
            v.extend(a.data.iter().map(|s| s.to_bits()));
            v
        })
        .collect()
}

/// Records every pair and the REAL wall instant it left at.
struct Rec {
    clk: SettableClock,
    pairs: Vec<Pair>,
    at: Vec<i64>,
    /// The real wall at each command the emit loop read.
    cmds: Vec<i64>,
}

impl Rec {
    fn new(clk: SettableClock) -> Self {
        Self {
            clk,
            pairs: Vec::new(),
            at: Vec::new(),
            cmds: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        kind: Kind,
        video: &PacedFrame,
        blend: Option<(u16, u32)>,
        audio: &[AudioFrame],
        vtc: i64,
        atc: i64,
    ) {
        self.pairs.push(Pair {
            kind,
            vtc,
            atc,
            pic: pic_id(&video.video),
            size: (video.width, video.height, video.stride),
            pts_ns: video.pts_ns,
            blend,
            audio: audio_bits(audio),
        });
        self.at.push(self.clk.get());
    }
}

impl PacedSink for Rec {
    fn emit(&mut self, video: &PacedFrame, audio: &[AudioFrame], vtc: i64, atc: i64) {
        self.push(Kind::Emit, video, None, audio, vtc, atc);
    }

    fn emit_standby(&mut self, video: &PacedFrame, audio: &[AudioFrame], vtc: i64, atc: i64) {
        self.push(Kind::Standby, video, None, audio, vtc, atc);
    }

    fn emit_blended(
        &mut self,
        video: &PacedFrame,
        blend: FrameBlend,
        audio: &[AudioFrame],
        vtc: i64,
        atc: i64,
    ) {
        let b = Some((pic_id(&blend.video), blend.weight_q8));
        self.push(Kind::Blended, video, b, audio, vtc, atc);
    }

    fn submit_shared(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        video: Picture,
        audio: &[AudioFrame],
        vtc: i64,
        atc: i64,
    ) {
        let frame = PacedFrame {
            pts_ns: 0,
            width,
            height,
            stride,
            video,
            audio: Vec::new(),
        };
        self.push(Kind::Shared, &frame, None, audio, vtc, atc);
    }
}

/// A sample's value: its media index, exactly representable.
fn enc(s: i64) -> f32 {
    (s % 1_000_000) as f32 / 1_048_576.0 + 0.001
}

/// `n` frames at `num/den` fps of a song played from `start_ms`, frame `k`
/// with the decoder's 0-based integer-ms pts (a seek restarts it at 0,
/// `paced_producer::pts_ns`) and id `id_base + k`; each carries the stereo
/// audio from its own pts to the next frame's (the last one `tail_ms` more),
/// every sample's value its media index in the song.
fn song(
    num: i64,
    den: i64,
    n: i64,
    id_base: u16,
    start_ms: i64,
    tail_ms: i64,
) -> VecDeque<PacedFrame> {
    let ms = |k: i64| k * 1000 * den / num;
    let origin = start_ms * RATE_HZ / 1000;
    (0..n)
        .map(|k| {
            let from = ms(k) * RATE_HZ / 1000;
            let to = if k + 1 == n {
                (ms(k) + 1000 * den / num + tail_ms) * RATE_HZ / 1000
            } else {
                ms(k + 1) * RATE_HZ / 1000
            };
            let mut data = Vec::with_capacity(((to - from) * 2) as usize);
            for s in from..to {
                data.push(enc(origin + s));
                data.push(-enc(origin + s));
            }
            let id = id_base + k as u16;
            let mut px = vec![0u8; 12];
            px[..2].copy_from_slice(&id.to_le_bytes());
            PacedFrame {
                pts_ns: ms(k) * 1_000_000,
                width: 4,
                height: 2,
                stride: 4,
                video: SharedFrame::new(px).into(),
                audio: vec![AudioFrame {
                    data,
                    channels: 2,
                    sample_rate: 48_000,
                    timecode_100ns: Some(ms(k) * 10_000),
                }],
            }
        })
        .collect()
}

/// A command the emit loop reads at an iteration's top.
enum Cmd {
    Pause,
    Resume,
    /// A seek: the producer's queue flushed, then refilled with these
    /// frames after this many more iterations (the decoder's seek).
    Seek(VecDeque<PacedFrame>, usize),
    /// Stop (or the next song's Play): the song leaves the loop at once.
    Stop,
}

/// The emit loop over one pacer and a settable wall.
struct Rig {
    pacer: Pacer,
    clk: SettableClock,
    /// The commit lead (100 ns): the decision clock reads the wall + it.
    lead: i64,
    rec: Rec,
    black: Picture,
}

impl Rig {
    /// A pacer committing `lead_slots` ahead, its decision clock starting at
    /// timeline 0.
    fn new(lead_slots: i64) -> Self {
        let lead = lead_slots * 333_333;
        let (wall, clk) = WallClock::settable(-lead);
        let pacer = Pacer::with_lead(30, true, wall, lead_slots);
        assert_eq!(pacer.lead_100ns(), lead);
        let mut px = vec![0u8; 12];
        px[..2].copy_from_slice(&BLACK_ID.to_le_bytes());
        Self {
            pacer,
            rec: Rec::new(clk.clone()),
            clk,
            lead,
            black: SharedFrame::new(px).into(),
        }
    }

    /// The real wall reaches the instant the decision clock reads `until`
    /// (production: `sleep_to_boundary` over `pacer.now_100ns()`).
    fn wait_to(clk: &SettableClock, lead: i64, until: i64) {
        if clk.get() < until - lead {
            clk.set(until - lead);
        }
    }

    /// `n` idle boundaries (black + silence), as `run_idle_wait`.
    fn idle(&mut self, n: usize) {
        let mut served = 0;
        for _ in 0..MAX_BOUNDARIES {
            if served == n {
                return;
            }
            let black = StandbyBlack {
                width: 4,
                height: 2,
                stride: 4,
                video: &self.black,
            };
            match self.pacer.service_standby(black.standby(), &mut self.rec) {
                ServiceOutcome::Wait { until_100ns } => {
                    Self::wait_to(&self.clk, self.lead, until_100ns)
                }
                _ => {
                    self.pacer.tick_wall();
                    served += 1;
                }
            }
        }
        panic!("idle never served {n} boundaries");
    }

    /// The song-start pre-roll, ready at the `ready_at`-th poll.
    fn preroll(&mut self, ready_at: usize) {
        let mut polls = 0;
        let black = StandbyBlack {
            width: 4,
            height: 2,
            stride: 4,
            video: &self.black,
        };
        let (clk, lead) = (self.clk.clone(), self.lead);
        self.pacer.preroll(
            black,
            &mut self.rec,
            || {
                polls += 1;
                (polls >= ready_at).then_some(())
            },
            |_, until| Self::wait_to(&clk, lead, until),
        );
    }

    /// Play `frames` through the emit loop, `script` read at its iterations;
    /// returns at the song's end (after the play-out) or at a stop.
    fn play(&mut self, mut frames: VecDeque<PacedFrame>, mut script: Vec<(usize, Cmd)>) {
        let (clk, lead) = (self.clk.clone(), self.lead);
        let pacer = &mut self.pacer;
        let rec = &mut self.rec;
        let mut paused = false;
        let mut refill: Option<(usize, VecDeque<PacedFrame>)> = None;
        for i in 1..=MAX_BOUNDARIES {
            if refill.as_ref().is_some_and(|(at, _)| *at == i) {
                frames = refill.take().expect("checked").1;
            }
            if let Some(pos) = script.iter().position(|(at, _)| *at == i) {
                rec.cmds.push(clk.get());
                match script.remove(pos).1 {
                    Cmd::Pause => paused = true,
                    Cmd::Resume => {
                        paused = false;
                        pacer.audio_resume_reset();
                    }
                    Cmd::Seek(after, delay) => {
                        frames.clear();
                        refill = Some((i + delay, after));
                        pacer.anchor_seek();
                    }
                    Cmd::Stop => return,
                }
            }
            if paused {
                match pacer.service_standby(Standby::FrozenLast, &mut *rec) {
                    ServiceOutcome::Wait { until_100ns } => Self::wait_to(&clk, lead, until_100ns),
                    _ => pacer.tick_wall(),
                }
                continue;
            }
            let target = pacer.next_boundary_100ns();
            pacer.prepare(target, || frames.pop_front());
            if frames.is_empty() && refill.is_none() {
                pacer.audio_source_ended();
            }
            Self::wait_to(&clk, lead, target);
            match pacer.service(|| None, &mut *rec) {
                ServiceOutcome::Wait { until_100ns }
                | ServiceOutcome::Reanchored { until_100ns, .. } => {
                    Self::wait_to(&clk, lead, until_100ns)
                }
                _ => {
                    pacer.tick_wall();
                    if frames.is_empty() && refill.is_none() && !pacer.has_pending() {
                        pacer
                            .play_out_audio(&mut *rec, |_, until| Self::wait_to(&clk, lead, until));
                        return;
                    }
                }
            }
        }
        panic!("the song never ended");
    }

    /// The scope ends: every committed pair handed out at its stamp, as the
    /// emit loops do before they drop their feed.
    fn end_scope(&mut self) {
        let (clk, lead) = (self.clk.clone(), self.lead);
        let wait = |_: &Pacer, until| Self::wait_to(&clk, lead, until);
        self.pacer.flush_line(&mut self.rec, wait);
        assert_eq!(self.pacer.committed(), 0, "nothing left in the line");
    }
}

/// 64-bit FNV-1a over the pairs' `Debug` text, in order.
fn fingerprint(pairs: &[Pair]) -> u64 {
    let text = format!("{pairs:?}");
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Every stamp is a whole grid slot after the one before, audio and video
/// on one boundary, every pair leaving at or after its stamp and within its
/// slot.
fn assert_on_grid(rec: &Rec) {
    for (i, p) in rec.pairs.iter().enumerate() {
        assert_eq!(p.vtc, p.atc, "pair {i}: audio and video on one boundary");
        if i > 0 {
            let prev = rec.pairs[i - 1].vtc;
            let slot = p.vtc - prev;
            assert!(
                slot == 333_333 || slot == 333_334,
                "pair {i}: the next grid slot after the last ({prev} -> {})",
                p.vtc
            );
        }
        let late = rec.at[i] - p.vtc;
        assert!(
            (0..333_333).contains(&late),
            "pair {i} (stamp {}) left {late} after its stamp",
            p.vtc
        );
    }
}

/// How many pairs of each kind, and how many carry real song audio.
fn census(pairs: &[Pair]) -> (usize, usize, usize, usize, usize) {
    let n = |k: Kind| pairs.iter().filter(|p| p.kind == k).count();
    let audible = pairs
        .iter()
        .filter(|p| p.audio.iter().any(|a| a[2..].iter().any(|&s| s != 0)))
        .count();
    (
        n(Kind::Emit),
        n(Kind::Standby),
        n(Kind::Blended),
        n(Kind::Shared),
        audible,
    )
}

/// Run one scenario with no lead and with the production lead, and check
/// each run's pairs against the pin.
fn check(scenario: fn(i64) -> Rig, census_pin: (usize, usize, usize, usize, usize), fp_pin: u64) {
    for lead_slots in [0, AUDIO_LEAD_SLOTS] {
        let rig = scenario(lead_slots);
        assert_on_grid(&rig.rec);
        let pairs = &rig.rec.pairs;
        assert_eq!(
            census(pairs),
            census_pin,
            "K = {lead_slots}: kinds + audible pairs"
        );
        assert_eq!(
            fingerprint(pairs),
            fp_pin,
            "K = {lead_slots}: every pair as before #289 step 1"
        );
    }
}

/// A 30 fps song with audio, its audio 500 ms past the picture: idle, the
/// pre-roll, the song, its play-out, idle.
fn song_30(lead_slots: i64) -> Rig {
    let mut rig = Rig::new(lead_slots);
    rig.idle(4);
    rig.preroll(3);
    rig.play(song(30, 1, 90, 1, 0, 500), Vec::new());
    rig.idle(4);
    rig.end_scope();
    rig
}

#[test]
fn a_30fps_song_emits_the_same_pairs() {
    check(song_30, (90, 15, 0, 10, 105), 8_643_186_858_566_154_753);
}

/// A 25 fps song on the 30 fps grid: boundaries between two frames blend.
fn song_25(lead_slots: i64) -> Rig {
    let mut rig = Rig::new(lead_slots);
    rig.idle(2);
    rig.preroll(2);
    rig.play(song(25, 1, 75, 1, 0, 0), Vec::new());
    rig.idle(3);
    rig.end_scope();
    rig
}

#[test]
fn a_blended_25fps_song_emits_the_same_pairs() {
    check(song_25, (16, 1, 74, 6, 90), 14_227_950_745_936_186_259);
}

/// A seek 30 boundaries in, its frames 4 iterations later: the refill holds
/// the pre-seek picture, then the
/// song plays on from 4 s.
fn seek(lead_slots: i64) -> Rig {
    let mut rig = Rig::new(lead_slots);
    rig.idle(2);
    rig.preroll(2);
    let script = vec![(30, Cmd::Seek(song(30, 1, 60, 1000, 4000, 0), 4))];
    rig.play(song(30, 1, 200, 1, 0, 0), script);
    rig.idle(2);
    rig.end_scope();
    rig
}

#[test]
fn a_seek_emits_the_same_pairs() {
    check(seek, (85, 1, 0, 9, 85), 4_719_203_122_329_271_307);
}

/// A pause 20 boundaries in, resumed 12 later.
fn pause_resume(lead_slots: i64) -> Rig {
    let mut rig = Rig::new(lead_slots);
    rig.idle(2);
    rig.preroll(2);
    let script = vec![(20, Cmd::Pause), (32, Cmd::Resume)];
    rig.play(song(25, 1, 60, 1, 0, 0), script);
    rig.idle(2);
    rig.end_scope();
    rig
}

#[test]
fn a_pause_and_resume_emit_the_same_pairs() {
    check(pause_resume, (12, 7, 54, 5, 66), 16_810_695_633_823_938_371);
}

/// A stop 25 boundaries in (no play-out), then idle.
fn stop(lead_slots: i64) -> Rig {
    let mut rig = Rig::new(lead_slots);
    rig.idle(2);
    rig.preroll(1);
    rig.play(song(30, 1, 90, 1, 0, 0), vec![(25, Cmd::Stop)]);
    rig.end_scope();
    rig.idle(3);
    rig.end_scope();
    rig
}

#[test]
fn a_stop_emits_the_same_pairs() {
    check(stop, (24, 0, 0, 5, 24), 2_164_604_794_920_010_690);
}

/// A pause read between two boundaries freezes the first boundary not yet
/// committed: with no lead the next one, with the production lead the one
/// after it (the next one already left the decision with a fresh frame).
#[test]
fn a_pause_applies_from_the_first_boundary_not_yet_committed() {
    for (lead_slots, slots) in [(0, 1), (AUDIO_LEAD_SLOTS, 2)] {
        let mut rig = Rig::new(lead_slots);
        rig.idle(2);
        rig.preroll(1);
        let script = vec![(10, Cmd::Pause), (14, Cmd::Stop)];
        rig.play(song(30, 1, 90, 1, 0, 0), script);
        rig.end_scope();
        let paused_at = rig.rec.cmds[0];
        let frozen = rig.rec.pairs.iter().position(|p| p.kind == Kind::Standby);
        let first = rig.rec.pairs[frozen.expect("the pause froze a boundary")].vtc;
        let before = rig.rec.pairs[..frozen.unwrap()]
            .last()
            .expect("a pair before");
        assert_eq!(
            before.kind,
            Kind::Emit,
            "K = {lead_slots}: a fresh frame before"
        );
        let after = first - paused_at;
        assert!(
            after > (slots - 1) * 333_333 && after <= slots * 333_334,
            "K = {lead_slots}: the first frozen boundary {after} after the pause"
        );
    }
}

/// Between two boundaries the production line holds exactly the pair it
/// committed ahead, stamped one slot after the pair that just left; with no
/// lead nothing waits.
#[test]
fn the_line_holds_one_committed_pair_between_boundaries() {
    for (lead_slots, held) in [(0, 0), (AUDIO_LEAD_SLOTS, 1)] {
        let mut rig = Rig::new(lead_slots);
        for n in 1..=6 {
            rig.idle(1);
            assert_eq!(
                rig.pacer.committed(),
                held,
                "K = {lead_slots}, boundary {n}"
            );
            assert_eq!(rig.rec.pairs.len(), n - held, "K = {lead_slots}: pairs out");
        }
        rig.end_scope();
        assert_eq!(
            rig.rec.pairs.len(),
            6,
            "K = {lead_slots}: the flush hands it out"
        );
        assert_on_grid(&rig.rec);
    }
}

/// `Pacer::new` (production) commits `AUDIO_LEAD_SLOTS` = one slot ahead.
#[test]
fn the_production_pacer_commits_one_slot_ahead() {
    let pacer = Pacer::new(30, true);
    assert_eq!(pacer.lead_100ns(), 333_333);
    let real = pacer.wall_now_100ns();
    let decision = pacer.now_100ns();
    let ahead = decision - real;
    assert!(
        (333_333..333_333 + 10_000_000).contains(&ahead),
        "the decision clock reads one slot ahead of the wall ({ahead})"
    );
}

/// A committed pair the wall fell more than the lead + one slot behind (a
/// backward step) leaves at the next call, in order; one exactly the lead +
/// one slot ahead still waits.
#[test]
fn a_pair_stranded_by_a_backward_wall_step_leaves_at_once() {
    let mut rig = Rig::new(AUDIO_LEAD_SLOTS);
    rig.idle(3);
    let held = rig.pacer.committed();
    assert_eq!(held, 1);
    let out = rig.rec.pairs.len();
    let stamp = strict_next_boundary_100ns(rig.rec.pairs[out - 1].vtc, 30);
    rig.clk.set(stamp - 666_666);
    let black = rig.black.clone();
    let standby = StandbyBlack {
        width: 4,
        height: 2,
        stride: 4,
        video: &black,
    };
    rig.pacer.service_standby(standby.standby(), &mut rig.rec);
    assert_eq!(
        rig.rec.pairs.len(),
        out,
        "exactly lead + one slot ahead: it waits"
    );
    rig.clk.set(stamp - 666_667);
    rig.pacer.service_standby(standby.standby(), &mut rig.rec);
    assert_eq!(rig.rec.pairs.len(), out + 1, "stranded: it leaves at once");
    assert!(
        rig.rec.pairs[out].vtc > rig.rec.pairs[out - 1].vtc,
        "in order"
    );
}

/// A flush whose waits never move the wall hands every pair out anyway,
/// after its bounded waits.
#[test]
fn a_flush_on_a_wall_that_stands_still_still_empties_the_line() {
    let mut rig = Rig::new(AUDIO_LEAD_SLOTS);
    rig.idle(2);
    assert_eq!(rig.pacer.committed(), 1);
    let mut waits = 0;
    rig.pacer.flush_line(&mut rig.rec, |_, _| waits += 1);
    assert_eq!(rig.pacer.committed(), 0);
    assert_eq!(rig.rec.pairs.len(), 2);
    assert_eq!(waits, 4, "FLUSH_MAX_WAITS");
}
