//! Linux-testable unit tests for the wall-clock audio emitter (#192).
//!
//! The production constants (`EMIT_SAMPLES_PER_BLOCK`, 48 kHz) are used through
//! [`AudioEmitter::production`] so a wrong block size fails the block-length /
//! grid-timecode / full-ring / stall tests below; expectations are hardcoded to
//! the known-correct values (1600 samples, 333333/333334 ns steps), not derived
//! from the constant under test.

use super::*;
use sp_ndi::NdiSender;
use sp_ndi::test_util::MockNdiBackend;
use std::sync::Arc;

/// Known-correct block size (the RED constant is deliberately 1601).
const SPB: usize = 1600;

fn mock_sink() -> (Arc<MockNdiBackend>, AudioSink<MockNdiBackend>) {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "AE", false, false).unwrap();
    let sink = sender.audio_sink();
    // The sink holds its own Arc<backend> + handle; leak the sender so its Drop
    // (a mock send_destroy) doesn't record extra calls mid-test.
    std::mem::forget(sender);
    (backend, sink)
}

fn stereo_block(v: f32) -> Vec<f32> {
    vec![v; SPB * 2]
}

// ---------------------------------------------------------------------------
// AudioRing — bounded FIFO, never drops
// ---------------------------------------------------------------------------

#[test]
fn ring_pop_block_returns_none_until_a_full_block_is_present() {
    let mut ring = AudioRing::new(SPB, 8);
    // One sample short of a full stereo block.
    ring.push_some(&vec![1.0f32; SPB * 2 - 2], 2);
    assert!(
        ring.pop_block().is_none(),
        "partial ring must not pop a block"
    );
    // Add the missing frame.
    ring.push_some(&[1.0, 1.0], 2);
    let block = ring.pop_block().expect("full block now available");
    assert_eq!(block.len(), SPB * 2, "block is exactly 1600 frames stereo");
    assert!(
        ring.pop_block().is_none(),
        "ring drained after the one block"
    );
}

#[test]
fn ring_push_some_never_drops_and_accepts_zero_when_full() {
    // Capacity 2 blocks stereo = 2 * 1600 * 2 interleaved samples.
    let mut ring = AudioRing::new(SPB, 2);
    let full = vec![7.0f32; SPB * 2 * 2];
    let accepted = ring.push_some(&full, 2);
    assert_eq!(accepted, SPB * 2 * 2, "accepts a whole capacity");
    assert_eq!(ring.len_frames(), SPB * 2);
    // Pushing more into a full ring accepts nothing and drops nothing.
    let more = vec![9.0f32; SPB * 2];
    assert_eq!(ring.push_some(&more, 2), 0, "full ring accepts 0");
    assert_eq!(
        ring.len_frames(),
        SPB * 2,
        "content unchanged — never drops"
    );
    // The buffered content is still the original 7.0 samples, in order.
    let b0 = ring.pop_block().unwrap();
    assert!(
        b0.iter().all(|&s| s == 7.0),
        "no foreign (9.0) samples leaked in"
    );
}

#[test]
fn ring_fixes_channel_count_on_first_push() {
    let mut ring = AudioRing::new(SPB, 8);
    ring.push_some(&[1.0, 2.0], 2);
    assert_eq!(ring.channels(), 2);
    // A later mono push is clamped to the established stereo layout (whole
    // frames only) — it never desyncs the interleave.
    let before = ring.len_frames();
    ring.push_some(&[3.0, 4.0, 5.0, 6.0], 2);
    assert_eq!(ring.len_frames(), before + 2);
}

// ---------------------------------------------------------------------------
// AudioEmitter::tick — one block per grid slot, silence fill, grid timecodes
// ---------------------------------------------------------------------------

#[test]
fn empty_ring_emits_silence_every_slot_never_skipping() {
    let mut e = AudioEmitter::production();
    let mut last_tc = i64::MIN;
    for i in 0..20u64 {
        let em = e.tick(0);
        assert_eq!(em.block, EmittedBlock::Silence, "empty ring → silence");
        assert!(
            em.timecode_100ns > last_tc,
            "grid timecodes strictly increase"
        );
        last_tc = em.timecode_100ns;
        assert_eq!(
            e.emitted_slots(),
            i + 1,
            "exactly one slot advanced per tick"
        );
    }
    assert_eq!(
        e.silence_blocks(),
        20,
        "every empty slot counted as silence"
    );
}

#[test]
fn grid_timecodes_are_the_exact_rational_48khz_grid() {
    let mut e = AudioEmitter::production();
    // Anchor origin at 0 for readable expectations.
    let t0 = e.tick(0).timecode_100ns;
    assert_eq!(t0, 0, "slot 0 boundary = origin");
    let t1 = e.tick(0).timecode_100ns;
    // 1600 samples @ 48 kHz = 1600/48000 s = 333333.33 ns/100 → floor 333333.
    assert_eq!(t1, 333_333, "slot 1 boundary");
    let t2 = e.tick(0).timecode_100ns;
    // 3200/48000 s = 666666.66 → floor 666666.
    assert_eq!(t2, 666_666, "slot 2 boundary");
    let t3 = e.tick(0).timecode_100ns;
    // 4800/48000 s = 0.1 s exactly = 1_000_000 units — the cumulative-sample
    // formula lands EXACTLY here (no drift), unlike a fixed 333333 step.
    assert_eq!(t3, 1_000_000, "slot 3 boundary is exact — no grid drift");
}

#[test]
fn full_ring_emits_zero_silence() {
    let mut e = AudioEmitter::production();
    // Four whole stereo blocks buffered (fits the 8-block ring).
    for k in 0..4 {
        assert!(e.ring_mut().push_some(&stereo_block(k as f32), 2) > 0);
    }
    for _ in 0..4 {
        let em = e.tick(0);
        assert!(
            matches!(em.block, EmittedBlock::Audio(_)),
            "audio, not silence"
        );
    }
    assert_eq!(e.silence_blocks(), 0, "a full ring never emits silence");
    // The 5th slot (ring now empty) is silence.
    assert_eq!(e.tick(0).block, EmittedBlock::Silence);
    assert_eq!(e.silence_blocks(), 1);
}

#[test]
fn decode_stall_inserts_silence_for_missing_slots_then_resumes_without_dropping() {
    let mut e = AudioEmitter::production();
    // Song N: 3 blocks pushed, all emitted as audio.
    for k in 0..3 {
        e.ring_mut().push_some(&stereo_block(10.0 + k as f32), 2);
    }
    for _ in 0..3 {
        assert!(matches!(e.tick(0).block, EmittedBlock::Audio(_)));
    }
    // ~300 ms decode stall = 9 grid slots with nothing pushed → 9 silence.
    for _ in 0..9 {
        assert_eq!(e.tick(0).block, EmittedBlock::Silence);
    }
    assert_eq!(
        e.silence_blocks(),
        9,
        "exactly the missing slots are silence"
    );
    // Song N+1 resumes: push 2 blocks; they are emitted intact (nothing dropped
    // by the stall) with the marker value preserved.
    e.ring_mut().push_some(&stereo_block(42.0), 2);
    e.ring_mut().push_some(&stereo_block(43.0), 2);
    match e.tick(0).block {
        EmittedBlock::Audio(s) => assert!(s.iter().all(|&x| x == 42.0), "resumed audio intact"),
        EmittedBlock::Silence => panic!("audio must resume after the stall"),
    }
    match e.tick(0).block {
        EmittedBlock::Audio(s) => assert!(s.iter().all(|&x| x == 43.0)),
        EmittedBlock::Silence => panic!("second resumed block must be audio"),
    }
    assert_eq!(e.silence_blocks(), 9, "no extra silence after resume");
}

#[test]
fn late_blocks_and_jitter_account_for_wall_lateness() {
    let mut e = AudioEmitter::production();
    // Slot 0 anchors origin at 1_000_000 (all subsequent boundaries derive from
    // it). Emit ON TIME → no lateness.
    let base = 1_000_000i64;
    e.tick(base);
    assert_eq!(e.late_blocks(), 0);
    // Slot 1 boundary is base + 333333. Emit a whole block (>= 333333 ns) late.
    let boundary1 = base + 333_333;
    e.tick(boundary1 + 400_000); // 40 ms late > one 33.3 ms block
    assert_eq!(
        e.late_blocks(),
        1,
        "an emit ≥ one block past its boundary is late"
    );
    assert!(
        e.emit_jitter_p99_us() >= 40_000,
        "p99 jitter reflects the 40 ms lateness"
    );
}

#[test]
fn emit_jitter_p99_is_the_exact_percentile_index() {
    // 50 distinct jitter samples 0..49 (µs). ceil(50·0.99)−1 = 50−1 = 49 → the
    // 49th (largest) value. Pins the p99 index formula: a floor (→48), a dropped
    // `−1` (→ out-of-bounds panic), or a wrong 0.99 all diverge from 49.
    let mut e = AudioEmitter::production();
    for i in 0..50i64 {
        let b = e.next_boundary_100ns(0); // slot i's boundary (origin anchored on first tick)
        e.tick(b + i * 10); // jitter = i*10 (100ns) = i µs
    }
    assert_eq!(e.emit_jitter_p99_us(), 49, "p99 of 0..49 is the 49th value");
}

#[test]
fn next_boundary_is_the_default_until_anchored_then_the_grid() {
    let mut e = AudioEmitter::production();
    // Before the first tick the origin is unset → the caller's `now` is used so
    // slot 0 fires immediately.
    assert_eq!(
        e.next_boundary_100ns(5000),
        5000,
        "unanchored → default now"
    );
    // First tick anchors origin at 1000; the NEXT slot's boundary is on the grid.
    e.tick(1000);
    assert_eq!(
        e.next_boundary_100ns(999_999),
        1000 + 333_333,
        "anchored → origin + one grid block, independent of the passed now"
    );
}

#[test]
fn ring_depth_ms_tracks_buffered_audio() {
    let mut e = AudioEmitter::production();
    assert_eq!(e.ring_depth_ms(), 0, "empty ring is 0 ms");
    // Two stereo blocks = 3200 frames @ 48 kHz = 66 ms.
    e.ring_mut().push_some(&stereo_block(1.0), 2);
    e.ring_mut().push_some(&stereo_block(2.0), 2);
    assert_eq!(e.ring_depth_ms(), 66, "3200 frames @ 48 kHz = 66 ms");
}

// ---------------------------------------------------------------------------
// Shared emitter + emit_one_block — the send seam (decode pushes, emitter sends)
// ---------------------------------------------------------------------------

#[test]
fn emitter_uses_the_ring_channel_count_and_remembers_it_for_silence() {
    let shared = new_shared_emitter();
    let (backend, sink) = mock_sink();
    // Mono block in → audio emitted at ch=1.
    push_blocking(&shared, &vec![0.5f32; SPB], 1);
    emit_one_block(&shared, &sink, 0);
    assert!(
        backend
            .calls()
            .iter()
            .any(|c| c == "send_audio(42,sr=48000,ch=1,spc=1600)"),
        "audio block uses the ring's mono channel count: {:?}",
        backend.calls()
    );
    // Ring now empty → the silence block is sent at the REMEMBERED mono count,
    // not the default stereo (proves channels_hint is carried across a gap).
    emit_one_block(&shared, &sink, 0);
    assert!(
        backend
            .calls()
            .iter()
            .filter(|c| c.as_str() == "send_audio(42,sr=48000,ch=1,spc=1600)")
            .count()
            >= 2,
        "silence keeps the remembered mono channel count: {:?}",
        backend.calls()
    );
}

#[test]
fn decode_side_push_does_not_send_audio_emitter_thread_does() {
    let shared = new_shared_emitter();
    let (backend, sink) = mock_sink();

    // The decode-side seam: push decoded audio into the ring. This must NOT
    // touch the NDI sender — the whole #192 change is that audio is no longer
    // submitted alongside video.
    push_blocking(&shared, &stereo_block(0.5), 2);
    assert!(
        backend.calls().iter().all(|c| !c.starts_with("send_audio")),
        "pushing decoded audio must not call send_audio"
    );

    // The emitter thread's per-slot send DOES call send_audio, with a full
    // 1600-sample block and the grid timecode.
    let em = emit_one_block(&shared, &sink, 0);
    assert!(matches!(em.block, EmittedBlock::Audio(_)));
    let calls = backend.calls();
    assert!(
        calls
            .iter()
            .any(|c| c == "send_audio(42,sr=48000,ch=2,spc=1600)"),
        "emitter sends one 1600-sample block: {calls:?}"
    );
}

#[test]
fn emitter_sends_a_full_silence_block_when_the_ring_is_empty() {
    let shared = new_shared_emitter();
    let (backend, sink) = mock_sink();
    // Nothing pushed → silence, but STILL a full 1600-sample block is sent so
    // the NDI audio stream never starves.
    let em = emit_one_block(&shared, &sink, 0);
    assert_eq!(em.block, EmittedBlock::Silence);
    let calls = backend.calls();
    assert!(
        calls
            .iter()
            .any(|c| c == "send_audio(42,sr=48000,ch=2,spc=1600)"),
        "silence is a full 1600-sample block, not a gap: {calls:?}"
    );
    // Silence samples are zero.
    assert!(backend.last_audio_planar().iter().all(|&s| s == 0.0));
}

#[test]
fn emit_one_block_mirrors_telemetry_into_the_atomics() {
    use std::sync::atomic::Ordering;
    let shared = new_shared_emitter();
    let (_backend, sink) = mock_sink();
    for _ in 0..5 {
        emit_one_block(&shared, &sink, 0);
    }
    assert!(shared.telemetry.enabled.load(Ordering::Relaxed));
    assert_eq!(shared.telemetry.silence_blocks.load(Ordering::Relaxed), 5);
}

#[test]
fn push_blocking_blocks_when_full_and_never_drops_until_the_emitter_frees_space() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    let shared = new_shared_emitter();
    let (_backend, sink) = mock_sink();

    // Fill the ring to capacity (8 blocks) then hand the pusher 4 MORE blocks:
    // it must block until the emitter drains, and every sample must survive.
    let total_blocks = RING_CAPACITY_BLOCKS + 4;
    let mut payload: Vec<f32> = Vec::new();
    for k in 0..total_blocks {
        payload.extend(vec![100.0 + k as f32; SPB * 2]);
    }

    let shared_push = shared.clone();
    let done = Arc::new(AtomicBool::new(false));
    let done_w = done.clone();
    let pusher = std::thread::spawn(move || {
        push_blocking(&shared_push, &payload, 2);
        done_w.store(true, Ordering::SeqCst);
    });

    // Give the pusher time to fill the ring and block on the last blocks.
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !done.load(Ordering::SeqCst),
        "push must still be blocked on a full ring"
    );

    // Drain every block; each drain frees a slot and wakes the pusher.
    let mut seen: Vec<f32> = Vec::new();
    for _ in 0..total_blocks {
        if let EmittedBlock::Audio(s) = emit_one_block(&shared, &sink, 0).block {
            seen.push(s[0]);
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    pusher.join().unwrap();
    assert!(
        done.load(Ordering::SeqCst),
        "push completes once space frees"
    );
    // Every block's marker value arrived, in order — nothing dropped.
    let expected: Vec<f32> = (0..total_blocks).map(|k| 100.0 + k as f32).collect();
    assert_eq!(
        seen, expected,
        "all pushed blocks emitted in order, none dropped"
    );
}

// ---------------------------------------------------------------------------
// EmitterStats serialisation on the health snapshot
// ---------------------------------------------------------------------------

#[test]
fn emitter_stats_reads_the_telemetry_and_sets_the_mode_when_enabled() {
    let shared = new_shared_emitter();
    let (_backend, sink) = mock_sink();
    // A silence emit bumps silence_blocks; ring_depth stays 0.
    emit_one_block(&shared, &sink, 0);
    let stats = emitter_stats(&shared);
    assert!(stats.enabled, "a spawned emitter reports enabled");
    assert_eq!(
        stats.mode, "sdk-video/wallclock-audio",
        "mode set when enabled"
    );
    assert_eq!(stats.silence_blocks, 1);
    assert_eq!(stats.late_blocks, 0);

    // A disabled telemetry reports an empty mode (paced / idle default).
    let disabled = SharedEmitterInner {
        emitter: std::sync::Mutex::new(AudioEmitter::production()),
        space: std::sync::Condvar::new(),
        telemetry: EmitterTelemetry::default(),
        shutdown: std::sync::atomic::AtomicBool::new(false),
    };
    let ds = emitter_stats(&Arc::new(disabled));
    assert!(!ds.enabled);
    assert_eq!(ds.mode, "", "disabled emitter has no mode string");
}

#[test]
fn heartbeat_audio_stats_carries_the_emitter_when_present_else_disabled_default() {
    let shared = new_shared_emitter();
    let (_backend, sink) = mock_sink();
    emit_one_block(&shared, &sink, 0); // one silence block
    let with = heartbeat_audio_stats(Some(&shared));
    assert!(with.emitter.enabled, "present emitter → enabled telemetry");
    assert_eq!(with.emitter.silence_blocks, 1);
    // The pacing/PLL fields stay default on the SDK-clocked path.
    assert!(!with.enabled);

    let without = heartbeat_audio_stats(None);
    assert!(!without.emitter.enabled, "no emitter → disabled default");
    assert_eq!(without, crate::playback::ndi_health::AudioStats::default());
}

#[test]
fn audio_stats_emitter_serialises_under_the_emitter_key() {
    use crate::playback::ndi_health::{AudioStats, EmitterStats};
    let stats = AudioStats {
        enabled: false,
        emitter: EmitterStats {
            enabled: true,
            mode: EMITTER_MODE.to_string(),
            silence_blocks: 9,
            ring_depth_ms: 66,
            emit_jitter_p99_us: 120,
            late_blocks: 0,
        },
        ..Default::default()
    };
    let json = serde_json::to_value(&stats).unwrap();
    let em = &json["emitter"];
    assert_eq!(em["enabled"], true);
    assert_eq!(em["mode"], "sdk-video/wallclock-audio");
    assert_eq!(em["silence_blocks"], 9);
    assert_eq!(em["ring_depth_ms"], 66);
    assert_eq!(em["late_blocks"], 0);
    // Default AudioStats carries a disabled emitter (paced / idle path).
    let default_json = serde_json::to_value(AudioStats::default()).unwrap();
    assert_eq!(default_json["emitter"]["enabled"], false);
}

// ── Adaptive spin margin (#192 box finding: coarse-sleep overshoot) ──────────

/// A margin that has just seen an audio block (so precision is wanted).
fn carrying() -> SpinMargin {
    let mut m = SpinMargin::default();
    m.note_block(true);
    m
}

#[test]
fn spin_margin_starts_at_the_minimum() {
    assert_eq!(carrying().margin_100ns(), 20_000); // 2 ms
}

#[test]
fn spin_margin_follows_the_worst_recent_overshoot_plus_headroom() {
    let mut m = carrying();
    m.observe(10_000);
    m.observe(30_000); // worst: 3 ms
    m.observe(12_000);
    assert_eq!(m.margin_100ns(), 35_000); // 3 ms + 0.5 ms headroom
}

#[test]
fn spin_margin_is_clamped_to_min_and_max() {
    let mut m = carrying();
    m.observe(15_000); // 1.5 ms + 0.5 ms = exactly the 2 ms minimum
    assert_eq!(m.margin_100ns(), 20_000);
    m.observe(55_000); // 5.5 ms + 0.5 ms = exactly the 6 ms maximum
    assert_eq!(m.margin_100ns(), 60_000);
    m.observe(400_000); // a 40 ms stall must not turn into a 40 ms spin
    assert_eq!(m.margin_100ns(), 60_000);
}

#[test]
fn spin_margin_ignores_negative_overshoot() {
    let mut m = carrying();
    m.observe(-50_000);
    assert_eq!(m.margin_100ns(), 20_000);
}

#[test]
fn spin_margin_forgets_an_overshoot_after_the_window() {
    let mut m = carrying();
    m.observe(40_000);
    for _ in 0..899 {
        m.observe(0);
    }
    assert_eq!(m.margin_100ns(), 45_000); // still inside the 900-sample window
    m.observe(0);
    assert_eq!(m.margin_100ns(), 20_000); // evicted
}

#[test]
fn spin_margin_stays_minimal_on_a_silent_pipeline() {
    // Never carried audio → cheap minimum even with a large overshoot.
    let mut m = SpinMargin::default();
    m.observe(50_000);
    assert_eq!(m.margin_100ns(), 20_000);
    // Audio arrives → precision on.
    m.note_block(true);
    assert_eq!(m.margin_100ns(), 55_000);
    // 299 silent slots (< 10 s) keep precision — a song transition must stay tight.
    for _ in 0..299 {
        m.note_block(false);
    }
    assert_eq!(m.margin_100ns(), 55_000);
    // The 300th silent slot drops back to the minimum.
    m.note_block(false);
    assert_eq!(m.margin_100ns(), 20_000);
}

// ── Ring cushion (#192 round 2: mid-song underruns on the box) ───────────────

#[test]
fn decoder_tolerance_adds_the_lookahead_only_with_an_emitter() {
    // With the emitter the decoder reads audio the whole cushion ahead of video
    // (round 3: DEFAULT_TOLERANCE 40 + AUDIO_LOOKAHEAD_MS 1500 = 1540 ms); without
    // it (legacy audio-with-video) the plain 40 ms pairing.
    assert_eq!(decoder_tolerance_ms(true), 1540);
    assert_eq!(decoder_tolerance_ms(false), 40);
}

#[test]
fn a_held_emitter_emits_silence_without_consuming_the_ring() {
    let mut e = AudioEmitter::production();
    e.ring_mut().push_some(&stereo_block(0.5), 2);
    e.set_held(true);
    assert_eq!(e.tick(0).block, EmittedBlock::Silence);
    assert_eq!(e.ring_depth_ms(), 33, "a paused pipeline keeps its cushion");
    e.set_held(false);
    assert_eq!(e.tick(0).block, EmittedBlock::Audio(stereo_block(0.5)));
}

#[test]
fn clearing_the_ring_drops_the_buffered_audio_but_keeps_the_layout() {
    let mut e = AudioEmitter::production();
    e.ring_mut().push_some(&stereo_block(0.5), 2);
    e.ring_mut().push_some(&stereo_block(0.6), 2);
    e.clear_ring();
    assert_eq!(e.ring_depth_ms(), 0);
    assert_eq!(e.tick(0).block, EmittedBlock::Silence);
    // The stereo layout survives: the next song's audio plays normally.
    e.ring_mut().push_some(&stereo_block(0.7), 2);
    assert_eq!(e.tick(0).block, EmittedBlock::Audio(stereo_block(0.7)));
}

#[test]
fn shared_hold_and_clear_reach_the_emitter_and_a_push_releases_the_hold() {
    let shared = new_shared_emitter();
    push_blocking(&shared, &stereo_block(0.5), 2);
    hold_ring(&shared);
    assert_eq!(
        shared.emitter.lock().unwrap().tick(0).block,
        EmittedBlock::Silence,
        "held: the cushion is not consumed"
    );
    // New audio (resume / next frame) releases the hold.
    push_blocking(&shared, &stereo_block(0.6), 2);
    assert_eq!(
        shared.emitter.lock().unwrap().tick(0).block,
        EmittedBlock::Audio(stereo_block(0.5))
    );
    clear_ring(&shared);
    assert_eq!(shared.emitter.lock().unwrap().ring_depth_ms(), 0);
}

// ── #192 review fixes (release code review, items 1–6) ───────────────────────

#[test]
fn push_some_refixes_the_layout_on_a_channel_change_clearing_stale_audio() {
    // A stereo song fills the ring; a mono song then pushes with ch=1. The ring
    // must DROP the stereo remainder and re-fix to mono — never reinterpret the
    // mono samples through the old 2-channel frame size (#192 item 2).
    let mut ring = AudioRing::new(SPB, 8);
    ring.push_some(&[1.0, 2.0, 3.0, 4.0], 2); // 2 stereo frames
    assert_eq!(ring.channels(), 2);
    assert_eq!(ring.len_frames(), 2);
    // Mono push (ch=1) with a DIFFERENT layout → clears + re-fixes.
    let accepted = ring.push_some(&[5.0, 6.0, 7.0], 1);
    assert_eq!(ring.channels(), 1, "layout re-fixed to mono");
    assert_eq!(accepted, 3, "all 3 mono samples accepted as whole frames");
    assert_eq!(ring.len_frames(), 3, "stale stereo audio was cleared");
    // Back to stereo → clears again and re-fixes.
    ring.push_some(&[8.0, 9.0], 2);
    assert_eq!(ring.channels(), 2);
    assert_eq!(ring.len_frames(), 1);
}

#[test]
fn push_some_accepts_only_whole_frames_of_the_pushed_layout() {
    // Odd-length input against a stereo layout: only whole stereo frames land,
    // the trailing single sample is NOT accepted (never splits a frame).
    let mut ring = AudioRing::new(SPB, 8);
    let accepted = ring.push_some(&[1.0, 2.0, 3.0], 2); // 1 frame + 1 residual
    assert_eq!(
        accepted, 2,
        "one whole stereo frame accepted, residual left"
    );
    assert_eq!(ring.len_frames(), 1);
}

#[test]
fn push_blocking_drops_a_partial_frame_residual_instead_of_spinning_forever() {
    // Odd-length stereo input has a 1-sample residual that can NEVER form a
    // frame. The old code looped in 250 ms waits forever (accepted==0 while free
    // space existed); now push_blocking drops the residual and RETURNS. The test
    // completing at all is the guard against the infinite loop (#192 item 1).
    let shared = new_shared_emitter();
    let (_backend, sink) = mock_sink();
    // 3 samples, ch=2 → 1 whole frame usable, 1-sample residual dropped.
    push_blocking(&shared, &[1.0, 2.0, 3.0], 2);
    // The whole frame is buffered; the residual did not hang or corrupt it.
    let g = shared.emitter.lock().unwrap();
    assert_eq!(
        g.ring_depth_ms(),
        0,
        "one frame is < a block → still sub-block"
    );
    drop(g);
    // A full stereo block still emits as audio (proves the ring is usable).
    push_blocking(&shared, &stereo_block(0.5), 2);
    assert!(matches!(
        emit_one_block(&shared, &sink, 0).block,
        EmittedBlock::Audio(_)
    ));
}

#[test]
fn ring_drained_is_true_below_one_block_exact_boundary() {
    let mut e = AudioEmitter::production();
    assert!(e.ring_drained(), "empty ring is drained");
    // One frame short of a full block → still drained.
    e.ring_mut().push_some(&vec![1.0f32; (SPB - 1) * 2], 2);
    assert!(e.ring_drained(), "one frame short of a block is drained");
    // Exactly one full block → NOT drained (a block is still poppable).
    e.ring_mut().push_some(&[1.0, 1.0], 2);
    assert!(!e.ring_drained(), "exactly one full block is not drained");
}

#[test]
fn ring_is_drained_reports_less_than_one_block_on_the_shared_emitter() {
    let shared = new_shared_emitter();
    assert!(ring_is_drained(&shared), "empty shared ring is drained");
    push_blocking(&shared, &stereo_block(0.5), 2); // one full block
    assert!(!ring_is_drained(&shared), "one full block is NOT drained");
}

#[test]
fn tick_does_not_reanchor_at_exactly_one_second_late() {
    let mut e = AudioEmitter::production();
    e.tick(0); // origin = 0, emitted_slots → 1
    let b1 = 333_333i64; // slot 1 boundary
    let em = e.tick(b1 + 10_000_000); // exactly 1 s late
    assert_eq!(em.timecode_100ns, b1, "the grid holds at exactly 1 s late");
    assert_eq!(e.resyncs(), 0, "no resync at exactly 1 s");
}

#[test]
fn tick_reanchors_when_more_than_one_second_late_snapping_this_slot_to_now() {
    let mut e = AudioEmitter::production();
    e.tick(0); // origin = 0, emitted_slots → 1
    let b1 = 333_333i64;
    let now = b1 + 10_000_000 + 100; // 1 s + 100 ns late (past the > threshold)
    let em = e.tick(now);
    assert_eq!(
        em.timecode_100ns, now,
        "resync snaps THIS slot's boundary to now"
    );
    assert_eq!(e.resyncs(), 1, "one resync counted");
    // The following boundary is now + one block (units_for(2) − units_for(1)).
    let next = e.tick(now);
    assert_eq!(
        next.timecode_100ns,
        now + 333_333,
        "the following boundary is now + one block"
    );
    assert_eq!(e.resyncs(), 1, "an on-time slot does not resync");
}

// ── #192 round 3: 1.5 s cushion + derived drain budget + ring sizing ─────────

#[test]
fn lookahead_is_the_round3_cushion() {
    // The round-3 cushion must cover the measured ~1 s producer stalls; the
    // decoder then reads DEFAULT_TOLERANCE(40) + 1500 = 1540 ms of audio ahead.
    assert_eq!(AUDIO_LOOKAHEAD_MS, 1500);
    assert_eq!(decoder_tolerance_ms(true), 1540);
}

#[test]
fn target_ring_depth_is_the_catchup_target() {
    // #192 round 4: the catch-up target_depth_ms is the nominal audio-ahead depth
    // (DEFAULT_TOLERANCE 40 + AUDIO_LOOKAHEAD 1500). Derived from the same
    // constants as decoder_tolerance_ms(true), never a literal.
    assert_eq!(target_ring_depth_ms(), 1540);
    assert_eq!(target_ring_depth_ms(), decoder_tolerance_ms(true));
}

#[test]
fn block_ms_is_one_grid_slot_rounded_up() {
    // 1600 samples @ 48 kHz = 33.333 ms → rounded UP to a whole 34 ms so a
    // budget derived from it never falls short of a whole slot.
    assert_eq!(block_ms(), 34);
}

#[test]
fn drain_budget_is_the_lookahead_plus_one_slot() {
    // Pure formula (constant-independent); exact boundaries pin the arithmetic
    // so a +/-/* mutant on `lookahead + block_ms()` is killed.
    assert_eq!(drain_budget_ms(0), 34);
    assert_eq!(drain_budget_ms(100), 134);
    // The ring holds up to target_ring_depth_ms() (tolerance + lookahead) at a
    // natural end, so the budget must cover THAT plus one slot: 40 + 1500 + 34.
    assert_eq!(drain_budget_ms(1500), 1574);
    // Derived from the LIVE cushion — the natural-end drain waits this long, and
    // it must exceed the round-2 fixed 400 ms so the last ~1.1 s is not cut.
    assert_eq!(
        drain_budget_ms(AUDIO_LOOKAHEAD_MS),
        target_ring_depth_ms() + block_ms()
    );
    assert!(
        drain_budget_ms(AUDIO_LOOKAHEAD_MS) > 400,
        "the round-3 drain budget must exceed the old 400 ms bound"
    );
}

#[test]
fn ring_capacity_blocks_holds_tolerance_plus_lookahead_with_headroom() {
    // The ring must hold at least DEFAULT_TOLERANCE + lookahead ms for EVERY
    // lookahead, else push_blocking caps the realised cushion below the lookahead.
    for &la in &[0u64, 100, 1500, 3000] {
        let blocks = ring_capacity_blocks(la);
        let cap_ms = blocks as u64 * EMIT_SAMPLES_PER_BLOCK as u64 * 1000 / EMIT_RATE_HZ as u64;
        let need = sp_decoder::split_sync::DEFAULT_TOLERANCE_MS + la;
        assert!(cap_ms >= need, "la={la}: cap {cap_ms} ms < need {need} ms");
    }
    // Exact values pin the ceil + the 2-block headroom (kills off-by-one and
    // ceil→floor mutants).
    assert_eq!(ring_capacity_blocks(0), 4);
    assert_eq!(ring_capacity_blocks(100), 7);
    assert_eq!(ring_capacity_blocks(1500), 49);
    assert_eq!(ring_capacity_blocks(3000), 94);
}

#[test]
fn ring_capacity_holds_the_round3_cushion() {
    // With the round-3 lookahead the production ring must HOLD ≥ tolerance +
    // 1500 ms of audio — the old 8-block (266 ms) ring could not, so a ~1 s
    // producer stall drained it and holed the on-program output.
    let cap_ms =
        RING_CAPACITY_BLOCKS as u64 * EMIT_SAMPLES_PER_BLOCK as u64 * 1000 / EMIT_RATE_HZ as u64;
    assert!(
        cap_ms >= 1540,
        "the production ring holds {cap_ms} ms, need ≥ 1540 for the 1.5 s cushion"
    );
    assert_eq!(
        RING_CAPACITY_BLOCKS, 49,
        "49 blocks ≈ 1633 ms at the 1.5 s cushion"
    );
}
