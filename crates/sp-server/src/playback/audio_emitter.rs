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
/// Ring capacity in whole blocks (~250 ms of headroom): covers a decode stall
/// up to ~200 ms (a heavy child's model-load burst) without starving the grid.
pub const RING_CAPACITY_BLOCKS: usize = 8;
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
        if self.channels == 0 {
            0
        } else {
            self.buf.len() / self.channels
        }
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
    /// existing content — a full ring accepts 0 and drops nothing. The first
    /// non-empty push fixes the channel count.
    pub fn push_some(&mut self, interleaved: &[f32], ch: usize) -> usize {
        if interleaved.is_empty() || ch == 0 {
            return 0;
        }
        if self.channels == 0 {
            self.channels = ch;
        }
        // A whole number of frames only (never split a sample-pair).
        let free_frames = self.free_interleaved() / self.channels;
        let want_frames = interleaved.len() / self.channels;
        let take_frames = free_frames.min(want_frames);
        let take = take_frames * self.channels;
        self.buf.extend(interleaved[..take].iter().copied());
        take
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
    /// Recent |emit − boundary| jitter samples (µs) for the p99 gauge.
    jitter_us: VecDeque<u64>,
    channels_hint: usize,
}

/// How many recent jitter samples the p99 gauge keeps (~30 s at 30 fps).
const JITTER_WINDOW: usize = 900;

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
            jitter_us: VecDeque::new(),
            channels_hint: 2,
        }
    }

    /// A production emitter: stereo, 1600-sample blocks, ~250 ms ring, 48 kHz.
    pub fn production() -> Self {
        Self::new(EMIT_SAMPLES_PER_BLOCK, RING_CAPACITY_BLOCKS, EMIT_RATE_HZ)
    }

    /// Exact-rational grid boundary for `slot`: `origin + slot · block / rate`,
    /// derived from the cumulative sample count so it never drifts (the block
    /// duration 1600/48000 s is not a whole number of 100 ns units). i128 math
    /// then a checked cast keeps it exact and overflow-free over multi-day runs.
    fn boundary_for(&self, origin_100ns: i64, slot: u64) -> i64 {
        let samples = (slot as i128) * (self.samples_per_block as i128);
        let units = samples * 10_000_000i128 / (self.rate_hz as i128);
        origin_100ns.saturating_add(units as i64)
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
        let origin = *self.origin_100ns.get_or_insert(now_100ns);
        let boundary = self.boundary_for(origin, self.emitted_slots);

        // Lateness / jitter vs the ideal boundary.
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

        let block = match self.ring.pop_block() {
            Some(samples) if samples.len() >= self.samples_per_block => {
                EmittedBlock::Audio(samples)
            }
            _ => {
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
        let idx = ((v.len() as f64) * 0.99).ceil() as usize;
        let idx = idx.saturating_sub(1).min(v.len() - 1);
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
    let mut offset = 0usize;
    let mut guard = shared.emitter.lock().unwrap();
    while offset < interleaved.len() {
        if shared.shutdown.load(Ordering::Relaxed) {
            return;
        }
        let accepted = guard.ring_mut().push_some(&interleaved[offset..], channels);
        offset += accepted;
        if offset < interleaved.len() {
            // Ring full — wait for the emit thread to free a block. Bounded so a
            // missed notify or a dead emit thread re-checks `shutdown` rather
            // than hanging the decode thread forever (design: bounded wait).
            let (g, _timeout) = shared
                .space
                .wait_timeout(guard, std::time::Duration::from_millis(250))
                .unwrap();
            guard = g;
        }
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

#[cfg(test)]
#[path = "audio_emitter_tests.rs"]
mod audio_emitter_tests;
