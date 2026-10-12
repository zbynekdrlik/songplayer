//! #233 (the SoundGrid loop check): the analyzer over a scripted loop at
//! 8 kHz, fed 10 ms (80 frames) a drain — the search's schedule and range,
//! the lock, the judge's verdicts and runs (one event per run, its UTC
//! time and length), the search again after 8 bad blocks and the slip,
//! `no_return`, an overflow that keeps the lock, a new run; its pure
//! helpers at their edges; the taps and the rings; the thread's tick over
//! real rings; the worker's link. Every pin comes from a scratch model of
//! the analyzer and the search (`rust-workspace.md`, deriving pins).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use super::*;

const RATE: f64 = 8_000.0;
/// UTC of frame 0, ms.
const T0: i64 = 1_760_000_000_000;
/// One drain: 10 ms at 8 kHz.
const TICK: usize = 80;

/// Noise in [−0.5, 0.5), exact in f32 (a 24-bit LCG word / 2^24 − 0.5).
fn noise(seed: u64, n: usize) -> Vec<f32> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (x >> 40) as f32 / 16_777_216.0 - 0.5
        })
        .collect()
}

/// `x` delayed by `d` frames (silence before it).
fn delayed(x: &[f32], t: usize, d: usize) -> f32 {
    if t >= d { x[t - d] } else { 0.0 }
}

/// The console's loop: input 0 returns R inverted at half level, input 1
/// nothing, input 2 L — `d(t)` frames after they were sent.
fn console(d: impl Fn(usize) -> usize) -> impl Fn(usize, usize, &[f32], &[f32]) -> f32 {
    move |input: usize, t: usize, l: &[f32], r: &[f32]| -> f32 {
        match input {
            0 => delayed(r, t, d(t)) * -0.5,
            2 => delayed(l, t, d(t)),
            _ => 0.0,
        }
    }
}

/// A program (L, R noise), what returns on `k` inputs, and the analyzer
/// fed both a drain at a time.
struct Rig {
    an: LoopAnalyzer,
    sent: Vec<f32>,
    back: Vec<f32>,
    k: usize,
    fed: usize,
    overflow: u64,
}

impl Rig {
    /// `n` frames; input `i` at frame `t` returns `route(i, t, L, R)`.
    fn new(n: usize, k: usize, route: impl Fn(usize, usize, &[f32], &[f32]) -> f32) -> Self {
        Self::of(noise(1, n), noise(2, n), k, route)
    }

    fn of(
        l: Vec<f32>,
        r: Vec<f32>,
        k: usize,
        route: impl Fn(usize, usize, &[f32], &[f32]) -> f32,
    ) -> Self {
        let sent = l.iter().zip(&r).flat_map(|(&a, &b)| [a, b]).collect();
        let mut back = Vec::with_capacity(l.len() * k);
        for t in 0..l.len() {
            for i in 0..k {
                // A generic `Fn`'s arguments are not coerced: pass slices.
                back.push(route(i, t, &l[..], &r[..]));
            }
        }
        let mut an = LoopAnalyzer::default();
        an.start_run(RATE, k);
        Self {
            an,
            sent,
            back,
            k,
            fed: 0,
            overflow: 0,
        }
    }

    /// Feed up to frame `to`, a drain at a time, each at the UTC time of
    /// its newest frame.
    fn feed_to(&mut self, to: usize) {
        while self.fed < to {
            let next = (self.fed + TICK).min(to);
            self.an.ingest(
                &self.back[self.fed * self.k..next * self.k],
                &self.sent[self.fed * 2..next * 2],
                self.overflow,
                now_of(next),
            );
            self.fed = next;
        }
    }

    /// The glue drops `frames` on both sides (its rings were full).
    fn overflow(&mut self, frames: usize, callbacks: u64) {
        self.fed += frames;
        self.overflow += callbacks;
    }

    fn status(&self) -> LoopStatus {
        self.an.status(now_of(self.fed))
    }
}

fn now_of(frames: usize) -> i64 {
    T0 + frames as i64 * 1_000 / RATE as i64
}

fn event(kind: LoopEventKind, at_ms: i64, frames: i64) -> (LoopEventKind, i64, i64) {
    (kind, T0 + at_ms, frames)
}

fn events(s: &LoopStatus) -> Vec<(LoopEventKind, i64, i64)> {
    s.recent
        .iter()
        .map(|e| (e.kind, e.utc_ms, e.frames))
        .collect()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// The search's first attempt has only 48 frames of range (the sent
/// history then); the second, 0.5 s of returned audio later, locks: each
/// input's channel, the round trip, each gain.
#[test]
fn it_searches_every_half_second_and_locks_on_the_loop() {
    let mut rig = Rig::new(17_000, 3, console(|_| 1_234));
    assert_eq!(rig.status().state, LoopState::Searching);
    rig.feed_to(8_160);
    assert_eq!(
        rig.an.search_work(),
        0,
        "under a window of return: no attempt"
    );
    rig.feed_to(8_240);
    assert!(rig.an.search_work() > 0, "the first attempt");
    rig.feed_to(12_160);
    assert_eq!(rig.status().state, LoopState::Searching);
    rig.feed_to(12_240);
    let s = rig.status();
    assert_eq!(s.state, LoopState::Locked);
    assert_eq!(s.inputs, [Some(2), Some(0)]);
    assert_eq!(s.delay_frames, Some(1_234));
    assert_eq!(s.gain_db, [Some(gain_db(1.0)), Some(gain_db(-0.5))]);
    assert_eq!((s.checked_s, s.blind_s), (0.0, 0.0));
    // 14 whole blocks after the lock by 2 s, all as sent.
    rig.feed_to(16_000);
    let s = rig.status();
    assert!(close(s.checked_s, 14.0 * 0.032), "{}", s.checked_s);
    assert_eq!(
        (s.mismatches, s.dropouts, s.slips, s.overflows),
        (0, 0, 0, 0)
    );
    assert_eq!((s.last_event, s.recent.len()), (None, 0));
    // What it keeps: a window + a block of return, and the range on top.
    assert_eq!(rig.an.held(), (8_192 + 256 + 4_000, 8_192 + 256));
}

/// One event per run of one kind, its UTC time its first block's, its
/// length the run's; a kind change with no good block between is two runs.
#[test]
fn each_run_of_bad_blocks_is_one_event() {
    let base = console(|_| 1_234);
    let mut rig = Rig::new(18_400, 3, |i, t, l: &[f32], r: &[f32]| {
        let v = base(i, t, l, r);
        let returned = i == 0 || i == 2;
        if i == 2 && ((15_000..15_010).contains(&t) || (17_400..17_410).contains(&t)) {
            v + 0.5
        } else if returned && ((16_080..16_848).contains(&t) || (17_616..18_128).contains(&t)) {
            0.0
        } else {
            v
        }
    });
    rig.feed_to(18_400);
    let s = rig.status();
    assert_eq!(s.state, LoopState::Locked);
    assert_eq!(
        events(&s),
        [
            event(LoopEventKind::Mismatch, 1_850, 256),
            event(LoopEventKind::Dropout, 2_010, 768),
            event(LoopEventKind::Mismatch, 2_170, 256),
            event(LoopEventKind::Dropout, 2_202, 512),
        ]
    );
    assert_eq!((s.mismatches, s.dropouts, s.slips), (2, 2, 0));
    assert!(close(s.checked_s, 24.0 * 0.032), "{}", s.checked_s);
    // The last event, aged to the status's time (18 400 frames = 2 300 ms).
    let last = s.last_event.unwrap();
    assert_eq!(
        (last.kind, last.utc_ms),
        (LoopEventKind::Dropout, T0 + 2_202)
    );
    assert!(close(last.ago_s, 0.098), "{}", last.ago_s);
    assert_eq!(s.recent.last(), Some(&last));
    // The events not yet taken (for their WARNs), once.
    let fresh: Vec<_> = rig
        .an
        .take_fresh()
        .iter()
        .map(|e| (e.kind, e.utc_ms, e.frames))
        .collect();
    assert_eq!(fresh, events(&s));
    assert!(rig.an.take_fresh().is_empty());
}

/// A block whose sent side is under −50 dBFS on every tapped channel is
/// blind, not judged; one quiet channel alone still judges the other.
#[test]
fn a_quiet_block_is_blind_and_judges_nothing() {
    let (mut l, mut r) = (noise(1, 16_000), noise(2, 16_000));
    // A quiet program (noise at 0.004, about −63 dBFS) under blocks 10
    // and 11 (both channels), then under blocks 12 and 13 (L only): their
    // sent side is 1 234 frames earlier.
    l[13_566..14_590].iter_mut().for_each(|x| *x *= 0.004);
    r[13_566..14_078].iter_mut().for_each(|x| *x *= 0.004);
    let mut rig = Rig::of(l, r, 3, console(|_| 1_234));
    rig.feed_to(16_000);
    let s = rig.status();
    assert_eq!(s.state, LoopState::Locked);
    assert!(close(s.blind_s, 2.0 * 0.032), "{}", s.blind_s);
    assert!(close(s.checked_s, 12.0 * 0.032), "{}", s.checked_s);
    assert!(s.recent.is_empty());
}

/// 7 frames inserted on the way at frame 14 900: 8 bad blocks in a row
/// (one mismatch run of 2 048 frames) search again; an attempt whose window
/// still holds the old round trip finds nothing; the next locks 7 frames
/// later, a slip of +7 dated when the bad run began.
#[test]
fn a_slip_searches_again_and_is_named_with_its_frames() {
    let mut rig = Rig::new(
        25_200,
        3,
        console(|t| if t >= 14_900 { 1_241 } else { 1_234 }),
    );
    rig.feed_to(16_800);
    let s = rig.status();
    assert_eq!(
        (s.state, s.mismatches),
        (LoopState::Locked, 0),
        "7 bad blocks"
    );
    rig.feed_to(16_880);
    let s = rig.status();
    assert_eq!(s.state, LoopState::Searching, "the 8th searches again");
    assert_eq!((s.inputs, s.delay_frames), ([None, None], None));
    assert_eq!(events(&s), [event(LoopEventKind::Mismatch, 1_850, 2_048)]);
    rig.feed_to(24_800);
    assert_eq!(rig.status().state, LoopState::Searching);
    rig.feed_to(24_880);
    let s = rig.status();
    assert_eq!((s.state, s.delay_frames), (LoopState::Locked, Some(1_241)));
    assert_eq!(
        events(&s),
        [
            event(LoopEventKind::Mismatch, 1_850, 2_048),
            event(LoopEventKind::Slip, 1_850, 7),
        ]
    );
    assert_eq!((s.mismatches, s.slips), (1, 1));
    rig.feed_to(25_200);
    assert!(close(rig.status().checked_s, 19.0 * 0.032));
}

/// 10 s of sent energy with nothing locked: `no_return` (no input at all
/// here, at a rate where 10 s is a whole number of blocks); a quiet
/// program never counts.
#[test]
fn ten_seconds_of_energy_with_no_lock_is_no_return() {
    let rate = 8_192.0;
    let (l, r) = (noise(1, 82_000), noise(2, 82_000));
    let sent: Vec<f32> = l.iter().zip(&r).flat_map(|(&a, &b)| [a, b]).collect();
    let mut an = LoopAnalyzer::default();
    an.start_run(rate, 0);
    for chunk in sent[..81_664 * 2].chunks(200) {
        an.ingest(&[], chunk, 0, T0);
    }
    assert_eq!(an.status(T0).state, LoopState::Searching, "319 blocks");
    an.ingest(&[], &sent[81_664 * 2..81_920 * 2], 0, T0);
    assert_eq!(an.status(T0).state, LoopState::NoReturn, "320 blocks: 10 s");
    // A quiet program (noise at 0.004, about −63 dBFS) counts nothing.
    let mut quiet = LoopAnalyzer::default();
    quiet.start_run(rate, 0);
    let soft: Vec<f32> = noise(5, 2 * 90_000).iter().map(|x| x * 0.004).collect();
    quiet.ingest(&[], &soft, 0, T0);
    assert_eq!(quiet.status(T0).state, LoopState::Searching);
}

/// An input that carries something else: searched every 0.5 s, never
/// locked, `no_return` once 10 s of the program had energy.
#[test]
fn an_input_that_carries_something_else_is_no_return() {
    let other = noise(3, 80_200);
    let mut rig = Rig::new(80_200, 1, |_, t, _: &[f32], _: &[f32]| other[t]);
    rig.feed_to(80_080);
    assert_eq!(rig.status().state, LoopState::Searching);
    rig.feed_to(80_160);
    let s = rig.status();
    assert_eq!((s.state, s.delay_frames), (LoopState::NoReturn, None));
    assert!(s.recent.is_empty());
}

/// The glue drops a callback (then two) on both sides: the drains around
/// each are dropped, the lock stays, nothing is read as a difference, and
/// the judge goes on where both sides are whole again.
#[test]
fn an_overflow_keeps_the_lock_and_makes_no_event() {
    let mut rig = Rig::new(20_000, 3, console(|_| 1_234));
    rig.feed_to(14_000);
    assert!(close(rig.status().checked_s, 6.0 * 0.032));
    rig.overflow(80, 1);
    rig.feed_to(15_040);
    let s = rig.status();
    assert_eq!((s.state, s.overflows), (LoopState::Locked, 1));
    rig.overflow(160, 2);
    rig.feed_to(20_000);
    let s = rig.status();
    assert_eq!((s.state, s.delay_frames), (LoopState::Locked, Some(1_234)));
    assert_eq!((s.overflows, s.mismatches, s.dropouts), (3, 0, 0));
    assert!(s.recent.is_empty());
    assert!(close(s.checked_s, 19.0 * 0.032), "{}", s.checked_s);
}

/// The run ends (the driver closed): a bad run so far is an event, the
/// state is off, the counters stay; a new run searches afresh, and its
/// first lock at another round trip is no slip.
#[test]
fn a_new_run_searches_afresh_and_keeps_the_counters() {
    let base = console(|_| 1_234);
    let mut rig = Rig::new(13_000, 3, |i, t, l: &[f32], r: &[f32]| {
        let v = base(i, t, l, r);
        if i == 2 && (12_500..12_510).contains(&t) {
            v + 0.5
        } else {
            v
        }
    });
    rig.feed_to(12_752);
    assert_eq!(rig.status().mismatches, 0, "the run is open");
    rig.an.end_run();
    let s = rig.status();
    assert_eq!(s.state, LoopState::Off);
    assert_eq!(
        (s.inputs, s.delay_frames, s.gain_db),
        ([None, None], None, [None, None])
    );
    assert_eq!(events(&s), [event(LoopEventKind::Mismatch, 1_562, 256)]);
    // A reopen: frames count from 0 again, the loop 766 frames longer.
    let mut next = Rig::new(13_000, 3, console(|_| 2_000));
    next.an = std::mem::take(&mut rig.an);
    next.an.start_run(RATE, 3);
    next.feed_to(12_240);
    let s = next.status();
    assert_eq!((s.state, s.delay_frames), (LoopState::Locked, Some(2_000)));
    assert_eq!((s.mismatches, s.slips, s.recent.len()), (1, 0, 1));
}

/// The status lists the last 32 events, oldest first; the counters go on.
#[test]
fn the_status_lists_the_last_32_events() {
    let base = console(|_| 1_234);
    let marks: Vec<usize> = (0..66).step_by(2).map(|k| 12_240 + 256 * k + 10).collect();
    let n = 12_240 + 256 * 67;
    let mut rig = Rig::new(n, 3, |i, t, l: &[f32], r: &[f32]| {
        let v = base(i, t, l, r);
        if i == 2 && marks.contains(&t) {
            v + 0.5
        } else {
            v
        }
    });
    rig.feed_to(n);
    let s = rig.status();
    assert_eq!((s.mismatches, s.recent.len()), (33, RECENT_EVENTS));
    assert_eq!(s.recent[0].utc_ms, T0 + 1_594, "the first event is gone");
    assert_eq!(s.last_event.map(|e| e.utc_ms), Some(T0 + 3_578));
}

#[test]
fn the_pure_rules_at_their_edges() {
    assert_eq!(frames_of(0.5, 96_000.0), 48_000);
    assert_eq!(frames_of(0.5, 44_100.0), 22_050);
    assert_eq!(frames_of(1.0, 4.9), 4);
    // −50 dBFS over a block: at the floor is no energy, over it is.
    let floor = FLOOR_MS * 256.0;
    assert!(!has_energy(floor, 256));
    assert!(has_energy(floor * 1.000_001, 256));
    assert!(has_energy(floor, 255));
    // The judge: blind, then a return at or under −40 dB of what was
    // expected is a dropout, a residual over it a mismatch.
    let (s, g) = (2.0, 0.5);
    let allowed = g * g * s * MISMATCH_RATIO;
    assert_eq!(verdict(256, floor, 1.0, 0.0, 1.0), Verdict::Blind);
    assert_eq!(verdict(256, s, allowed, 0.0, g), Verdict::Dropout);
    assert_eq!(
        verdict(256, s, allowed * 1.01, allowed * 2.0, g),
        Verdict::Mismatch
    );
    assert_eq!(verdict(256, s, allowed * 1.01, allowed, g), Verdict::Ok);
    assert_eq!(
        verdict(256, s, 1.0, 0.0, -g),
        Verdict::Ok,
        "a polarity flip"
    );
    assert_eq!(verdict(256, s, 0.4, 0.0, g), Verdict::Ok);
    assert_eq!(verdict(256, s, 0.4, 0.001, g), Verdict::Mismatch);
    // Dropout over mismatch over ok over blind.
    assert!(Verdict::Dropout > Verdict::Mismatch);
    assert!(Verdict::Mismatch > Verdict::Ok && Verdict::Ok > Verdict::Blind);
    assert_eq!(gain_db(1.0), 0.0);
    assert_eq!(gain_db(-100.0), 40.0, "its size");
    assert!(close(gain_db(0.5), -6.020_599_913_279_624));
    assert_eq!(event_utc_ms(T0, 800, 8_000.0), T0 - 100);
    assert_eq!(event_utc_ms(T0, 3, 8_000.0), T0, "0.375 ms rounds to 0");
    assert_eq!(event_utc_ms(T0, 5, 8_000.0), T0 - 1, "0.625 ms rounds to 1");
}

#[test]
fn what_was_played_is_tapped_as_the_encoder_saw_it() {
    let mut played = vec![9.0f32; 8];
    copy_played(&[0.5, -1.5, f32::NAN, 0.25], &mut played);
    assert_eq!(played, vec![0.5, -1.0, 0.0, 0.25, 0.0, 0.0, 0.0, 0.0]);
    let mut short = vec![9.0f32; 2];
    copy_played(&[0.5, 0.5, 0.5], &mut short);
    assert_eq!(short, vec![0.5, 0.5]);
}

/// Both rings or neither: a callback the received ring has no room for is
/// an overflow and the sent ring takes nothing either (and the other way
/// round); exactly full fits.
#[test]
fn the_taps_push_both_rings_or_neither() {
    let (mut taps, mut feed) = loop_rings(4.0); // 4 frames: 8 sent, 32 received
    assert_eq!(
        (
            feed.sent.buffer().capacity(),
            feed.received.buffer().capacity()
        ),
        (8, 4 * MAX_LOOP_INPUTS)
    );
    taps.push(&[1.0; 6], &[2.0; 30]);
    taps.push(&[3.0; 2], &[4.0; 4]);
    assert_eq!(feed.drained(), (vec![1.0; 6], vec![2.0; 30], 1));
    taps.push(&[5.0; 8], &[6.0; 2]);
    assert_eq!(
        feed.drained(),
        (vec![5.0; 8], vec![6.0; 2], 1),
        "exactly full"
    );
    taps.push(&[7.0; 2], &[8.0; 2]);
    taps.push(&[7.0; 8], &[8.0; 2]);
    assert_eq!(feed.drained(), (vec![7.0; 2], vec![8.0; 2], 2));
    assert_eq!(feed.with_inputs(3).inputs, 3);
    let (_, feed) = loop_rings(96_000.0);
    assert_eq!(feed.sent.buffer().capacity(), 2 * 96_000);
    assert_eq!(feed.with_inputs(64).inputs, MAX_LOOP_INPUTS);
}

#[test]
fn a_drain_takes_whole_frames_only() {
    let (mut p, mut c) = rtrb::RingBuffer::<f32>::new(8);
    let _ = p.push_entire_slice(&[1.0, 2.0, 3.0, 4.0, 5.0]);
    assert_eq!(drain(&mut c, 2), vec![1.0, 2.0, 3.0, 4.0]);
    assert_eq!(drain(&mut c, 2), Vec::<f32>::new(), "half a frame waits");
    let _ = p.push_entire_slice(&[6.0]);
    assert_eq!(drain(&mut c, 2), vec![5.0, 6.0]);
}

/// The thread's tick over real rings: a run's rings, a lock, the taps
/// gone (the driver closed) ends the run once drained, an overflowing
/// ring is counted.
#[test]
fn the_thread_s_tick_reads_the_rings_and_ends_with_the_taps() {
    let mut core = LoopCore::new("out-2");
    let (l, r) = (noise(1, 12_240), noise(2, 12_240));
    let (mut taps, feed) = loop_rings(RATE);
    let s = core.tick(Some(feed.with_inputs(2)), T0);
    assert_eq!(s.state, LoopState::Searching);
    for start in (0..12_240).step_by(TICK) {
        let frames = start..start + TICK;
        let sent: Vec<f32> = frames.clone().flat_map(|t| [l[t], r[t]]).collect();
        let back: Vec<f32> = frames
            .flat_map(|t| [delayed(&l, t, 300), delayed(&r, t, 300)])
            .collect();
        taps.push(&sent, &back);
        core.tick(None, now_of(start + TICK));
    }
    let s = core.tick(None, now_of(12_240));
    assert_eq!((s.state, s.delay_frames), (LoopState::Locked, Some(300)));
    assert_eq!(s.inputs, [Some(0), Some(1)]);
    assert!(core.analyzer().search_work() > 0);
    drop(taps);
    assert_eq!(core.tick(None, now_of(12_240)).state, LoopState::Off);
    // A new run: a ring the analyzer did not drain overflows.
    let (mut taps, feed) = loop_rings(RATE);
    core.tick(Some(feed.with_inputs(2)), T0);
    for _ in 0..101 {
        taps.push(&[0.1; 2 * TICK], &[0.1; 2 * TICK]);
    }
    let s = core.tick(None, T0);
    assert_eq!((s.state, s.overflows), (LoopState::Searching, 1));
}

/// The worker's link: the starter gets the thread's ends once, a run's
/// rings reach it, its status is what the link reads.
#[test]
fn the_link_starts_the_thread_once_and_reads_its_status() {
    let started: Arc<Mutex<Vec<LoopThread>>> = Arc::default();
    let sink = started.clone();
    let link = LoopLink::start("out-2", &move |t: LoopThread| sink.lock().unwrap().push(t));
    let threads = started.lock().unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].id, "out-2");
    let (_taps, feed) = loop_rings(48_000.0);
    link.run(feed.with_inputs(2));
    let got = threads[0].feeds.try_recv().expect("the run's rings");
    assert_eq!((got.rate, got.inputs), (48_000.0, 2));
    let shown = LoopStatus {
        state: LoopState::Locked,
        slips: 4,
        ..LoopStatus::default()
    };
    *threads[0].status.lock().unwrap() = shown.clone();
    assert_eq!(link.status(), shown);
    // The overflow counter the taps share with the feed.
    assert_eq!(got.overflow.load(Ordering::Relaxed), 0);
}

/// The first attempt's range is what the sent history holds, a whole number
/// of coarse steps: 9 192 frames in one drain leave 1 000 frames of range,
/// so 992 — a round trip of 500 locks at once, one of 1 500 does not.
#[test]
fn the_first_attempt_searches_the_history_it_has() {
    let mut near = Rig::new(9_192, 2, |i, t, l: &[f32], r: &[f32]| {
        delayed(if i == 0 { l } else { r }, t, 500)
    });
    near.an.ingest(&near.back, &near.sent, 0, now_of(9_192));
    let s = near.an.status(now_of(9_192));
    assert_eq!((s.state, s.delay_frames), (LoopState::Locked, Some(500)));
    let mut far = Rig::new(9_192, 2, |i, t, l: &[f32], r: &[f32]| {
        delayed(if i == 0 { l } else { r }, t, 1_500)
    });
    far.an.ingest(&far.back, &far.sent, 0, now_of(9_192));
    assert_eq!(far.an.status(now_of(9_192)).state, LoopState::Searching);
}

/// An attempt as soon as a window of return is in (exactly 8 192 frames,
/// no range yet: the round trip 0 only); a block ending exactly at the
/// newest frame is judged at once.
#[test]
fn a_window_exactly_full_is_searched_and_a_block_exactly_in_is_judged() {
    let n = 8_192 + 256;
    let mut rig = Rig::new(
        n,
        2,
        |i, t, l: &[f32], r: &[f32]| {
            if i == 0 { l[t] } else { r[t] }
        },
    );
    rig.an.ingest(
        &rig.back[..2 * 8_192],
        &rig.sent[..2 * 8_192],
        0,
        now_of(8_192),
    );
    let s = rig.an.status(now_of(8_192));
    assert_eq!((s.state, s.delay_frames), (LoopState::Locked, Some(0)));
    rig.an
        .ingest(&rig.back[2 * 8_192..], &rig.sent[2 * 8_192..], 0, now_of(n));
    assert!(close(rig.an.status(now_of(n)).checked_s, 0.032));
}

/// Returned frames with no sent ones yet are kept until the sent ones come:
/// nothing is searched or judged past what was sent.
#[test]
fn a_return_ahead_of_what_was_sent_waits_for_it() {
    let mut rig = Rig::new(
        9_000,
        2,
        |i, t, l: &[f32], r: &[f32]| {
            if i == 0 { l[t] } else { r[t] }
        },
    );
    rig.an.ingest(&rig.back, &[], 0, now_of(9_000));
    assert_eq!(rig.an.search_work(), 0);
    assert_eq!(rig.an.status(now_of(9_000)).state, LoopState::Searching);
}

/// At 44.1 kHz the range is 0.5 s rounded down to a whole coarse step
/// (22 048 frames): a round trip of 22 000 locks at the second attempt.
#[test]
fn the_range_is_half_a_second_in_whole_coarse_steps() {
    let n = 44_100;
    let (l, r) = (noise(1, n), noise(2, n));
    let mut an = LoopAnalyzer::default();
    an.start_run(44_100.0, 1);
    let sent: Vec<f32> = l.iter().zip(&r).flat_map(|(&a, &b)| [a, b]).collect();
    let back: Vec<f32> = (0..n).map(|t| delayed(&l, t, 22_000)).collect();
    for start in (0..n).step_by(441) {
        let at = T0 + (start + 441) as i64 * 10 / 441;
        an.ingest(
            &back[start..start + 441],
            &sent[2 * start..2 * (start + 441)],
            0,
            at,
        );
    }
    let s = an.status(T0 + 1_000);
    assert_eq!((s.state, s.delay_frames), (LoopState::Locked, Some(22_000)));
    assert_eq!(s.inputs, [Some(0), None]);
}

/// A drain that holds many blocks judges each in turn, at its own time:
/// 10 blocks in one drain, the 4th of them bad.
#[test]
fn many_blocks_in_one_drain_are_each_judged_in_turn() {
    let base = console(|_| 1_234);
    let mut rig = Rig::new(14_800, 3, |i, t, l: &[f32], r: &[f32]| {
        let v = base(i, t, l, r);
        if i == 2 && (13_050..13_060).contains(&t) {
            v + 0.5
        } else {
            v
        }
    });
    rig.feed_to(12_240);
    assert_eq!(rig.status().state, LoopState::Locked);
    rig.an.ingest(
        &rig.back[12_240 * 3..],
        &rig.sent[12_240 * 2..],
        0,
        now_of(14_800),
    );
    let s = rig.an.status(now_of(14_800));
    assert_eq!(events(&s), [event(LoopEventKind::Mismatch, 1_626, 256)]);
    assert!(close(s.checked_s, 10.0 * 0.032), "{}", s.checked_s);
    // The next drain goes on after the 10th block, judging none twice.
    rig.an.ingest(&[], &[], 0, now_of(14_800));
    assert!(close(rig.an.status(now_of(14_800)).checked_s, 10.0 * 0.032));
}
