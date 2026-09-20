//! Wall-clock NDI audio emitter for the SDK-clocked path (#192).
//!
//! On the SDK-clocked production path (`genlock_pacing=false`,
//! `pipeline.rs::decode_and_send`) audio was submitted ONLY alongside each
//! video frame from `decoder.next_synced()`. At a natural song end the pipeline
//! emitted NOTHING on the NDI audio stream from the last audio chunk of song N
//! until song N+1's decoders opened and its first frame went out — a 200–400 ms
//! hole every song — and a heavy-child model-load burst stalled the decode loop
//! for a few more. The genlock OBS's ASRC servo (3 ms latency budget) reads any
//! ≥ 1-block hole as starvation → buffer collapse → re-lock → ~1 s of the next
//! song lost (#192); the burst-shaped arrival also shows as ±1500 ppm "rate
//! swings" on the servo.
//!
//! The cure (design on #192): make the NDI audio stream CONTINUOUS and clocked
//! by the wall clock, independent of the video submits. A dedicated OS thread
//! per pipeline (`pipeline_audio.rs`, Windows-only) emits ONE fixed-size block
//! (1600 samples @ 48 kHz stereo = 33.333 ms) every grid slot: it pops the
//! block from a bounded ring the decode loop fills, or — when the ring holds
//! fewer than a full block — emits a SILENCE block instead (never a partial
//! block, never a skipped slot). Between songs the ring is empty, so the stream
//! is filled with silence and never starves; on the next `Play` the decoded
//! audio simply resumes from the ring.
//!
//! This module is the PURE, cross-platform, Linux-tested core:
//! - [`AudioRing`] — the bounded interleaved-f32 FIFO (blocking push handled by
//!   the shared wrapper; never drops).
//! - [`AudioEmitter`] — the wall-clock grid + telemetry, owning one ring; its
//!   [`tick`](AudioEmitter::tick) produces exactly one [`Emitted`] block per
//!   grid slot.
//! - [`SharedEmitter`] — the `Mutex`/`Condvar` wrapper the decode thread pushes
//!   into (bounded, blocking) and the emit thread ticks; [`emit_one_block`]
//!   ticks it and sends through an [`sp_ndi::AudioSink`] (generic → Linux-tested
//!   via `MockNdiBackend`).
//! - [`EmitterTelemetry`] — lock-free atomics mirrored after each tick so the
//!   pipeline heartbeat reads emitter stats without contending the ring mutex.
//!
//! The Windows-only thread lifecycle (spawn with `THREAD_PRIORITY_TIME_CRITICAL`,
//! the sleep-until + spin loop, join before the sender is destroyed) lives in
//! `pipeline_audio.rs`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use sp_ndi::{AudioFrame, AudioSink, NdiBackend};

/// Nominal NDI audio rate — the FLAC pipeline is always 48 kHz.
pub const EMIT_RATE_HZ: u32 = 48_000;
/// Samples per channel in one grid block: 1600 @ 48 kHz = 33.333 ms = one
/// 30 fps grid slot (matches `AUDIO_SAMPLES_PER_BOUNDARY` on the paced path).
pub const EMIT_SAMPLES_PER_BLOCK: usize = 1600;
/// Ring capacity in whole blocks, DERIVED from [`AUDIO_LOOKAHEAD_MS`] via
/// [`ring_capacity_blocks`] so it can never drift below the cushion: the ring
/// must HOLD the whole lookahead the decoder reads ahead (else `push_blocking`
/// caps the realised cushion at the capacity and a ~1 s producer stall still
/// holes — #192 round 3). At the 1500 ms cushion that is 49 blocks ≈ 1633 ms
/// (~627 KB/pipeline of f32 stereo).
pub const RING_CAPACITY_BLOCKS: usize = ring_capacity_blocks(AUDIO_LOOKAHEAD_MS);
/// The emitter mode string surfaced on `/api/v1/ndi/health` (`audio.emitter.mode`).
pub const EMITTER_MODE: &str = "sdk-video/wallclock-audio";

/// One block the emitter hands to NDI for a grid slot.
#[derive(Clone, Debug, PartialEq)]
pub enum EmittedBlock {
    /// A full block of interleaved f32 audio popped from the ring.
    Audio(Vec<f32>),
    /// A full block of silence — the ring held fewer than a whole block.
    Silence,
}

/// The result of one [`AudioEmitter::tick`]: the block to send plus its
/// grid-derived NDI timecode (100 ns since the Unix epoch).
#[derive(Clone, Debug, PartialEq)]
pub struct Emitted {
    pub block: EmittedBlock,
    /// Grid boundary time for this slot: `origin + slot · block_duration`,
    /// evenly spaced so the receiver's servo sees a clean 48 kHz clock (kills
    /// the ±1500 ppm swings the bursty per-video-frame submit produced).
    pub timecode_100ns: i64,
}

/// A bounded interleaved-f32 FIFO of stereo (or mono) audio. The decode loop
/// pushes decoded frames; the emit thread pops whole blocks. Never drops on a
/// full ring — the caller (the shared wrapper) blocks the decoder instead
/// (bounded back-pressure), so the decode loop paces on video + ring depth.
pub struct AudioRing {
    /// Interleaved samples `[c0_s0, c1_s0, c0_s1, c1_s1, …]`.
    buf: VecDeque<f32>,
    /// Channel count, fixed on the first non-empty push (1–2 in practice).
    channels: usize,
    /// Hard cap in interleaved samples (= `capacity_blocks · block · channels`,
    /// computed once the channel count is known).
    capacity_blocks: usize,
    samples_per_block: usize,
}

impl AudioRing {
    pub fn new(samples_per_block: usize, capacity_blocks: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            channels: 0,
            capacity_blocks,
            samples_per_block,
        }
    }

    /// Interleaved-sample capacity, or `usize::MAX` until the channel count is
    /// known (nothing pushed yet → nothing to bound).
    fn cap_interleaved(&self) -> usize {
        if self.channels == 0 {
            usize::MAX
        } else {
            self.capacity_blocks * self.samples_per_block * self.channels
        }
    }

    /// Frames (samples-per-channel) currently buffered.
    pub fn len_frames(&self) -> usize {
        // 0 channels (nothing pushed yet) → 0 frames, never a divide-by-zero.
        self.buf.len().checked_div(self.channels).unwrap_or(0)
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Remaining interleaved-sample headroom before the cap.
    fn free_interleaved(&self) -> usize {
        self.cap_interleaved().saturating_sub(self.buf.len())
    }

    /// Append as much of `interleaved` (channel count `ch`) as fits under the
    /// cap; returns the number of interleaved samples ACCEPTED. Never overwrites
    /// existing content — a full ring accepts 0 and drops nothing.
    ///
    /// The channel count is fixed by the first non-empty push. A LATER push
    /// whose `ch` differs from the fixed layout CLEARS the buffered audio and
    /// re-fixes the layout to `ch` (a mono song after a stereo one must not be
    /// reinterpreted through the old frame size — #192 item 2). Only whole frames
    /// of `ch` are ever accepted (never a split sample group — #192 item 1).
    pub fn push_some(&mut self, interleaved: &[f32], ch: usize) -> usize {
        if interleaved.is_empty() || ch == 0 {
            return 0;
        }
        if self.channels != 0 && self.channels != ch {
            // Layout change: the buffered samples are a different frame size and
            // would desync the interleave — drop them and adopt the new layout.
            self.buf.clear();
        }
        self.channels = ch;
        // A whole number of frames only, sized by the pushed layout `ch`
        // (== self.channels after the re-fix above).
        let free_frames = self.free_interleaved() / ch;
        let want_frames = interleaved.len() / ch;
        let take_frames = free_frames.min(want_frames);
        let take = take_frames * ch;
        self.buf.extend(interleaved[..take].iter().copied());
        take
    }

    /// Drop all buffered audio; the established channel layout is kept.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Pop exactly one block (`samples_per_block` frames) of interleaved audio
    /// iff the ring holds at least that many frames; otherwise `None` (the
    /// caller emits silence). Never returns a partial block.
    pub fn pop_block(&mut self) -> Option<Vec<f32>> {
        if self.channels == 0 {
            return None;
        }
        let need = self.samples_per_block * self.channels;
        if self.buf.len() < need {
            return None;
        }
        let mut out = Vec::with_capacity(need);
        for _ in 0..need {
            out.push(self.buf.pop_front().unwrap_or(0.0));
        }
        Some(out)
    }
}

/// The wall-clock grid + telemetry, owning one [`AudioRing`]. Pure and
/// single-threaded: [`tick`](Self::tick) is called once per grid slot by the
/// emit thread (under the shared mutex). No clock calls, no I/O — the caller
/// passes the wall reading in.
pub struct AudioEmitter {
    ring: AudioRing,
    samples_per_block: usize,
    rate_hz: u32,
    /// Grid origin: slot 0's boundary. Set lazily on the first tick.
    origin_100ns: Option<i64>,
    /// Slots emitted so far (also the index of the NEXT slot to emit).
    emitted_slots: u64,
    silence_blocks: u64,
    /// Emits that woke a whole block or more past their grid boundary.
    late_blocks: u64,
    /// Grid re-anchors after a > 1 s stall (see [`REANCHOR_THRESHOLD_100NS`]).
    resyncs: u64,
    /// Recent |emit − boundary| jitter samples (µs) for the p99 gauge.
    jitter_us: VecDeque<u64>,
    channels_hint: usize,
    /// Paused pipeline: emit silence WITHOUT popping, so the lookahead cushion
    /// (and the A/V alignment it carries) survives a pause.
    held: bool,
}

/// How many recent jitter samples the p99 gauge keeps (~30 s at 30 fps).
const JITTER_WINDOW: usize = 900;

/// A grid slot's boundary more than this far behind `now` (1 s, in 100 ns units)
/// re-anchors the grid rather than firing a TIME_CRITICAL catch-up burst after a
/// suspend/debugger stall (#192 item 4). Strictly greater — exactly 1 s late
/// holds the grid.
const REANCHOR_THRESHOLD_100NS: i64 = 10_000_000;

impl AudioEmitter {
    pub fn new(samples_per_block: usize, capacity_blocks: usize, rate_hz: u32) -> Self {
        Self {
            ring: AudioRing::new(samples_per_block, capacity_blocks),
            samples_per_block,
            rate_hz,
            origin_100ns: None,
            emitted_slots: 0,
            silence_blocks: 0,
            late_blocks: 0,
            resyncs: 0,
            jitter_us: VecDeque::new(),
            channels_hint: 2,
            held: false,
        }
    }

    /// Hold (pause) or release the ring: a held emitter keeps ticking silence on
    /// the grid but leaves the buffered audio untouched.
    pub fn set_held(&mut self, held: bool) {
        self.held = held;
    }

    /// Drop the buffered audio (seek / a new song interrupting the old one), so
    /// stale audio never queues ahead of the new position.
    pub fn clear_ring(&mut self) {
        self.ring.clear();
    }

    /// A production emitter: stereo, 1600-sample blocks, ~1.6 s ring
    /// ([`RING_CAPACITY_BLOCKS`], the 1.5 s cushion + headroom), 48 kHz.
    pub fn production() -> Self {
        Self::new(EMIT_SAMPLES_PER_BLOCK, RING_CAPACITY_BLOCKS, EMIT_RATE_HZ)
    }

    /// Grid units (100 ns) from the origin to `slot`: `slot · block / rate`,
    /// derived from the cumulative sample count so it never drifts (the block
    /// duration 1600/48000 s is not a whole number of 100 ns units). i128 math
    /// then a CHECKED cast keeps it exact and overflow-free over multi-day runs
    /// (#192 item 5 — the old `units as i64` truncated silently). ONE formula so
    /// both `boundary_for` and the re-anchor share the exact same cast.
    fn units_for(&self, slot: u64) -> i64 {
        let samples = (slot as i128) * (self.samples_per_block as i128);
        let units = samples * 10_000_000i128 / (self.rate_hz as i128);
        i64::try_from(units).unwrap_or(i64::MAX)
    }

    /// Exact-rational grid boundary for `slot`: `origin + units_for(slot)`.
    fn boundary_for(&self, origin_100ns: i64, slot: u64) -> i64 {
        origin_100ns.saturating_add(self.units_for(slot))
    }

    /// True once the ring holds LESS than one whole block — nothing more for the
    /// emit thread to pop as audio. The natural-end drain polls this so a song's
    /// last partial block is emitted before the next song's `clear_ring` wipes
    /// the ring (#192 item 3).
    pub fn ring_drained(&self) -> bool {
        self.ring.len_frames() < self.samples_per_block
    }

    /// Grid re-anchors so far (a long stall snapped the origin forward). Surfaced
    /// only in the per-minute heartbeat log — no API/UI change (#192 item 4).
    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// The grid boundary of the NEXT slot to emit (for the emit thread's
    /// sleep-until target). Falls back to `default_now` before the origin is
    /// anchored (the first tick anchors it, so slot 0 fires immediately).
    pub fn next_boundary_100ns(&self, default_now: i64) -> i64 {
        match self.origin_100ns {
            Some(o) => self.boundary_for(o, self.emitted_slots),
            None => default_now,
        }
    }

    /// Emit the next grid block. Anchors the grid origin on the first call.
    /// Pops a full audio block from the ring, or a SILENCE block when the ring
    /// is short — never a partial block, never a skipped slot. `now_100ns` is
    /// the wall reading at the emit instant, used only for lateness/jitter
    /// accounting; the block's timecode is the exact grid boundary.
    pub fn tick(&mut self, now_100ns: i64) -> Emitted {
        let mut origin = *self.origin_100ns.get_or_insert(now_100ns);
        let mut boundary = self.boundary_for(origin, self.emitted_slots);

        // Re-anchor after a long stall (suspend / debugger): if THIS slot's grid
        // boundary is more than 1 s behind `now`, snap the origin so this slot is
        // due exactly now (`boundary == now`, the next boundary == now + one
        // block). Without it the loop would emit thousands of catch-up blocks in
        // one TIME_CRITICAL burst (#192 item 4).
        if now_100ns.saturating_sub(boundary) > REANCHOR_THRESHOLD_100NS {
            origin = now_100ns.saturating_sub(self.units_for(self.emitted_slots));
            self.origin_100ns = Some(origin);
            boundary = now_100ns;
            self.resyncs += 1;
        }

        // Lateness / jitter vs the (possibly re-anchored) boundary.
        let block_100ns = self.boundary_for(0, 1);
        // 100 ns units → µs is a divide-by-10 (1 µs = 10 × 100 ns).
        let jitter = (now_100ns - boundary).unsigned_abs() / 10; // µs
        if now_100ns.saturating_sub(boundary) >= block_100ns {
            self.late_blocks += 1;
        }
        self.jitter_us.push_back(jitter);
        while self.jitter_us.len() > JITTER_WINDOW {
            self.jitter_us.pop_front();
        }

        let popped = if self.held {
            None
        } else {
            self.ring.pop_block()
        };
        // `pop_block` already guarantees a whole block (or None), so no
        // interleaved-vs-per-channel length guard is needed here (#192 item 6).
        let block = match popped {
            Some(samples) => EmittedBlock::Audio(samples),
            None => {
                self.silence_blocks += 1;
                EmittedBlock::Silence
            }
        };
        if self.ring.channels() != 0 {
            self.channels_hint = self.ring.channels();
        }
        self.emitted_slots += 1;
        Emitted {
            block,
            timecode_100ns: boundary,
        }
    }

    /// Build the interleaved samples for [`emit_one_block`] to hand NDI: the
    /// audio block as-is, or a full block of silence at the current channel
    /// count. Kept beside `tick` so the silence layout is unit-testable.
    pub fn samples_for(&self, block: &EmittedBlock) -> (Vec<f32>, u32) {
        match block {
            EmittedBlock::Audio(s) => (s.clone(), self.channels_hint as u32),
            EmittedBlock::Silence => (
                vec![0.0f32; self.samples_per_block * self.channels_hint],
                self.channels_hint as u32,
            ),
        }
    }

    pub fn silence_blocks(&self) -> u64 {
        self.silence_blocks
    }
    pub fn late_blocks(&self) -> u64 {
        self.late_blocks
    }
    pub fn emitted_slots(&self) -> u64 {
        self.emitted_slots
    }

    /// Buffered audio in the ring, in milliseconds.
    pub fn ring_depth_ms(&self) -> u64 {
        if self.rate_hz == 0 {
            return 0;
        }
        (self.ring.len_frames() as u64) * 1000 / self.rate_hz as u64
    }

    /// 99th-percentile emit jitter (µs) over the recent window.
    pub fn emit_jitter_p99_us(&self) -> u64 {
        if self.jitter_us.is_empty() {
            return 0;
        }
        let mut v: Vec<u64> = self.jitter_us.iter().copied().collect();
        v.sort_unstable();
        // `ceil(n · 0.99) − 1` is always a valid index in `[0, n−1]` for n ≥ 1
        // (0.99 < 1 ⇒ ceil ≤ n ⇒ idx ≤ n−1; n·0.99 > 0 ⇒ ceil ≥ 1 ⇒ idx ≥ 0),
        // so no clamp is needed.
        let idx = ((v.len() as f64) * 0.99).ceil() as usize - 1;
        v[idx]
    }

    /// Mutable ring access for the shared wrapper's blocking push.
    pub fn ring_mut(&mut self) -> &mut AudioRing {
        &mut self.ring
    }
}

/// Lock-free snapshot of emitter telemetry, mirrored after each tick so the
/// pipeline heartbeat (on the decode thread) reads emitter stats without
/// locking the ring mutex.
#[derive(Debug, Default)]
pub struct EmitterTelemetry {
    pub enabled: AtomicBool,
    pub silence_blocks: AtomicU64,
    pub late_blocks: AtomicU64,
    pub ring_depth_ms: AtomicU64,
    pub emit_jitter_p99_us: AtomicU64,
}

impl EmitterTelemetry {
    /// Copy the emitter's current gauges into the atomics (called by
    /// [`emit_one_block`] under no lock — the emitter is already borrowed).
    pub fn mirror(&self, e: &AudioEmitter) {
        self.silence_blocks
            .store(e.silence_blocks(), Ordering::Relaxed);
        self.late_blocks.store(e.late_blocks(), Ordering::Relaxed);
        self.ring_depth_ms
            .store(e.ring_depth_ms(), Ordering::Relaxed);
        self.emit_jitter_p99_us
            .store(e.emit_jitter_p99_us(), Ordering::Relaxed);
    }
}

/// The shared emitter: the decode thread pushes into the ring (bounded,
/// blocking on `space`), the emit thread ticks it. A short mutex (a block copy,
/// sub-µs) rather than lock-free — the design accepts this on the SDK-clocked
/// path.
pub struct SharedEmitterInner {
    pub emitter: Mutex<AudioEmitter>,
    /// Notified when the emit thread frees a block, waking a blocked push.
    pub space: Condvar,
    pub telemetry: EmitterTelemetry,
    /// Set by the pipeline on teardown so a blocked push returns instead of
    /// waiting forever, and the emit loop exits.
    pub shutdown: AtomicBool,
}

pub type SharedEmitter = Arc<SharedEmitterInner>;

/// Build a shared production emitter with its telemetry marked enabled.
pub fn new_shared_emitter() -> SharedEmitter {
    let inner = SharedEmitterInner {
        emitter: Mutex::new(AudioEmitter::production()),
        space: Condvar::new(),
        telemetry: EmitterTelemetry::default(),
        shutdown: AtomicBool::new(false),
    };
    inner.telemetry.enabled.store(true, Ordering::Relaxed);
    Arc::new(inner)
}

/// Push decoded interleaved audio into the shared ring, BLOCKING (bounded)
/// while the ring is full so the decoder paces on the emit thread rather than
/// dropping audio. Returns when all of `interleaved` is accepted or shutdown
/// fires. Cross-platform (std sync) so it is Linux-tested with a real thread.
pub fn push_blocking(shared: &SharedEmitter, interleaved: &[f32], channels: usize) {
    if interleaved.is_empty() || channels == 0 {
        return;
    }
    // Only whole frames are ever pushed. A trailing partial-frame residual (an
    // odd-length buffer against a stereo layout) is dropped with a single WARN —
    // never retried, because retrying a residual that can never form a frame
    // spun the decode thread in 250 ms `wait_timeout`s forever (#192 item 1).
    let usable = (interleaved.len() / channels) * channels;
    warn_dropped_residual(interleaved.len(), usable, channels);
    let mut guard = shared.emitter.lock().unwrap();
    // New audio means the pipeline is running again — release a pause hold.
    guard.set_held(false);
    // `rest` shrinks by exactly what the ring accepted; the loop ends when it is
    // empty (no offset arithmetic left to get subtly wrong — a redundant second
    // comparison here used to be an infinite-wait mutant).
    let mut rest = &interleaved[..usable];
    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            return;
        }
        // An empty `rest` (nothing usable) is accepted as 0 and returns below.
        let accepted = guard.ring_mut().push_some(rest, channels);
        rest = &rest[accepted..];
        if rest.is_empty() {
            return;
        }
        // Ring full — wait for the emit thread to free a block. Bounded so a
        // missed notify or a dead emit thread re-checks `shutdown` rather than
        // hanging the decode thread forever (design: bounded wait).
        let (g, _timeout) = shared
            .space
            .wait_timeout(guard, std::time::Duration::from_millis(250))
            .unwrap();
        guard = g;
    }
}

/// One WARN when a push carried a partial-frame residual (an odd-length buffer
/// against a stereo layout) that had to be dropped. Log-only — no behaviour
/// hangs off the condition, so it is not mutation-scored.
#[cfg_attr(test, mutants::skip)]
fn warn_dropped_residual(total: usize, usable: usize, channels: usize) {
    let dropped = total - usable;
    if dropped > 0 {
        tracing::warn!(
            dropped,
            channels,
            "audio-emitter: dropped a partial-frame audio residual"
        );
    }
}

/// How far ahead of the video the decoder reads audio when the emitter is
/// present (ms). This IS the ring cushion: with plain 40 ms pairing the ring sat
/// at 24–55 ms on the box, so any decode hiccup became a 33 ms silence block
/// mid-song (#192 round 2). Reading ahead fills the ring without delaying audio
/// against video — the emitter starts the first block as the first frame goes
/// out, and both then run in real time.
///
/// **Round 3 (#192): 100 → 1500.** A 100 ms cushion (ring ~266 ms) could not
/// cover the ~1 s PRODUCER stalls a resident stems child causes (box 20.9.2026:
/// `silence_blocks` up to 28 ≈ 930 ms on the on-program output while
/// `emit_call_max_us` was only 43–76 ms — the decode loop, not the SDK send).
/// 1500 ms covers every measured stall (≤ ~950 ms); the ring capacity
/// ([`ring_capacity_blocks`]) and the natural-end drain budget
/// ([`drain_budget_ms`]) are both DERIVED from this one constant so they never
/// drift. f32 stereo 48 kHz × 1.5 s ≈ 576 KB per pipeline.
pub const AUDIO_LOOKAHEAD_MS: u64 = 1500;

/// One grid slot's whole-millisecond duration: ⌈`EMIT_SAMPLES_PER_BLOCK` /
/// `EMIT_RATE_HZ`⌉ = ⌈1600 / 48 kHz⌉ = ⌈33.333 ms⌉ = 34 ms. Rounded UP so a
/// budget derived from it never falls short of a whole slot. Pure.
pub const fn block_ms() -> u64 {
    let num = EMIT_SAMPLES_PER_BLOCK as u64 * 1000;
    let den = EMIT_RATE_HZ as u64;
    (num + den - 1) / den
}

/// Natural-end drain budget (ms): at a natural song end the ring still holds the
/// whole lookahead cushion, so the emit thread must be given up to
/// `lookahead + one slot` to play the buffered tail out before the next song's
/// `clear_ring` wipes it. The round-2 fixed ≤ 400 ms would cut the last ~1.1 s
/// of every song once the lookahead is 1500 ms. Derived from the SAME constant
/// as the cushion so the two never drift. Pure.
pub const fn drain_budget_ms(lookahead_ms: u64) -> u64 {
    lookahead_ms + block_ms()
}

/// Extra ring headroom (whole blocks) above the decoder's audio-ahead depth so
/// `push_blocking` paces on the emit thread rather than sitting at the cap every
/// frame (round 2: capacity 266 ms vs a ~225 ms max depth ≈ 1 block of slack).
const RING_HEADROOM_BLOCKS: usize = 2;

/// Ring capacity (whole blocks) sized to HOLD the decoder's full audio-ahead —
/// `DEFAULT_TOLERANCE_MS + lookahead` — plus [`RING_HEADROOM_BLOCKS`]. A capacity
/// below the lookahead would cap the realised cushion at the capacity, so a
/// deeper lookahead alone would not cover a ~1 s producer stall (#192 round 3).
/// Derived from the SAME constant as the cushion. Pure, exact-boundary tested.
pub const fn ring_capacity_blocks(lookahead_ms: u64) -> usize {
    let depth_ms = sp_decoder::split_sync::DEFAULT_TOLERANCE_MS + lookahead_ms;
    let num = depth_ms * EMIT_RATE_HZ as u64;
    let den = 1000 * EMIT_SAMPLES_PER_BLOCK as u64;
    // ceil(depth_ms / block_ms), in whole blocks, plus headroom.
    let blocks = ((num + den - 1) / den) as usize;
    blocks + RING_HEADROOM_BLOCKS
}

/// Pairing tolerance for `SplitSyncedDecoder`: the default, plus the lookahead
/// when the wall-clock emitter carries the audio. Without an emitter the audio
/// rides with the video frames and must not run ahead.
pub fn decoder_tolerance_ms(emitter_present: bool) -> u64 {
    if emitter_present {
        sp_decoder::split_sync::DEFAULT_TOLERANCE_MS + AUDIO_LOOKAHEAD_MS
    } else {
        sp_decoder::split_sync::DEFAULT_TOLERANCE_MS
    }
}

/// Pause: keep the cushion (see [`AudioEmitter::set_held`]). Released by the next
/// [`push_blocking`].
pub fn hold_ring(shared: &SharedEmitter) {
    shared.emitter.lock().unwrap().set_held(true);
}

/// Seek / new playback: drop stale buffered audio and wake a blocked push.
pub fn clear_ring(shared: &SharedEmitter) {
    shared.emitter.lock().unwrap().clear_ring();
    shared.space.notify_all();
}

/// Whether the shared ring holds less than one whole block — the natural-end
/// drain's poll predicate (#192 item 3). Delegates to the pure
/// [`AudioEmitter::ring_drained`] under the ring lock.
pub fn ring_is_drained(shared: &SharedEmitter) -> bool {
    shared.emitter.lock().unwrap().ring_drained()
}

/// The full [`AudioStats`](crate::playback::ndi_health::AudioStats) a
/// SDK-clocked heartbeat reports: default pacing/PLL fields (unused on this
/// path) with the wall-clock emitter telemetry filled from `emitter`. `None`
/// (emitter absent) reports the all-default (disabled) stats. Keeps the seam
/// out of `pipeline.rs` (1000-line cap).
pub fn heartbeat_audio_stats(
    emitter: Option<&SharedEmitter>,
) -> crate::playback::ndi_health::AudioStats {
    match emitter {
        Some(e) => crate::playback::ndi_health::AudioStats {
            emitter: emitter_stats(e),
            ..Default::default()
        },
        None => crate::playback::ndi_health::AudioStats::default(),
    }
}

/// Read the lock-free telemetry into the health-document [`EmitterStats`] the
/// pipeline heartbeat serialises. Cross-platform (the decode thread calls it on
/// the SDK-clocked path).
pub fn emitter_stats(shared: &SharedEmitter) -> crate::playback::ndi_health::EmitterStats {
    let t = &shared.telemetry;
    let enabled = t.enabled.load(Ordering::Relaxed);
    crate::playback::ndi_health::EmitterStats {
        enabled,
        mode: if enabled {
            EMITTER_MODE.to_string()
        } else {
            String::new()
        },
        silence_blocks: t.silence_blocks.load(Ordering::Relaxed),
        ring_depth_ms: t.ring_depth_ms.load(Ordering::Relaxed),
        emit_jitter_p99_us: t.emit_jitter_p99_us.load(Ordering::Relaxed),
        late_blocks: t.late_blocks.load(Ordering::Relaxed),
    }
}

/// Tick the shared emitter once and send the resulting block through `sink`,
/// then mirror telemetry and wake any blocked push. Generic over the NDI
/// backend so it is Linux-tested with `MockNdiBackend`. Returns the [`Emitted`]
/// for the emit thread's per-minute log.
pub fn emit_one_block<B: NdiBackend>(
    shared: &SharedEmitter,
    sink: &AudioSink<B>,
    now_100ns: i64,
) -> Emitted {
    let (emitted, interleaved, channels) = {
        let mut guard = shared.emitter.lock().unwrap();
        let emitted = guard.tick(now_100ns);
        let (samples, ch) = guard.samples_for(&emitted.block);
        shared.telemetry.mirror(&guard);
        (emitted, samples, ch)
    };
    // Freed a slot of ring headroom — wake a blocked decoder push.
    shared.space.notify_one();

    if channels > 0 && !interleaved.is_empty() {
        let frame = AudioFrame {
            data: interleaved,
            channels,
            sample_rate: EMIT_RATE_HZ,
            timecode_100ns: Some(emitted.timecode_100ns),
        };
        sink.send_audio(&frame);
    }
    emitted
}

/// Smallest spin margin before a grid slot (2 ms) — also the margin of a
/// pipeline that carries no audio.
pub const SPIN_MARGIN_MIN_100NS: i64 = 20_000;
/// Largest spin margin (6 ms): a long stall must never become a long spin.
pub const SPIN_MARGIN_MAX_100NS: i64 = 60_000;
/// Headroom added on top of the worst recent coarse-sleep overshoot (0.5 ms).
const SPIN_HEADROOM_100NS: i64 = 5_000;
/// How many recent coarse sleeps the margin remembers (~30 s of slots).
const SPIN_WINDOW: usize = 900;
/// Silent slots after the last audio block during which the margin stays
/// precise (~10 s) — a song transition is exactly when jitter matters.
const AUDIO_HOLD_SLOTS: u32 = 300;

/// Adaptive spin margin for the emit thread's sleep-until (#192 box finding).
///
/// On the mostly idle, Balanced-plan box the coarse sleep before a grid slot
/// overshoots by several ms (parked cores), so a fixed 2 ms spin left the p99
/// emit jitter at 0.4–5.5 ms. The margin follows the worst recent overshoot plus
/// headroom, clamped — and only for a pipeline that carries audio; a silent idle
/// pipeline keeps the cheap minimum so ten idle emitters do not spin.
#[derive(Debug, Default)]
pub struct SpinMargin {
    overshoots_100ns: VecDeque<i64>,
    /// Slots of precision left; refilled by every audio block, 0 = silent.
    audio_hold_left: u32,
}

impl SpinMargin {
    /// Record how far past its intended wake time one coarse sleep returned.
    pub fn observe(&mut self, overshoot_100ns: i64) {
        self.overshoots_100ns.push_back(overshoot_100ns.max(0));
        while self.overshoots_100ns.len() > SPIN_WINDOW {
            self.overshoots_100ns.pop_front();
        }
    }

    /// Record whether the slot just emitted carried audio or silence.
    pub fn note_block(&mut self, is_audio: bool) {
        self.audio_hold_left = if is_audio {
            AUDIO_HOLD_SLOTS
        } else {
            self.audio_hold_left.saturating_sub(1)
        };
    }

    /// The spin margin to use for the next slot.
    pub fn margin_100ns(&self) -> i64 {
        if self.audio_hold_left == 0 {
            return SPIN_MARGIN_MIN_100NS;
        }
        let worst = self.overshoots_100ns.iter().copied().max().unwrap_or(0);
        (worst + SPIN_HEADROOM_100NS).clamp(SPIN_MARGIN_MIN_100NS, SPIN_MARGIN_MAX_100NS)
    }
}

#[cfg(test)]
#[path = "audio_emitter_tests.rs"]
mod audio_emitter_tests;
