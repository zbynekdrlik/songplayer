//! Windows-only lifecycle glue for the wall-clock audio emitter (#192).
//!
//! The pure emitter core (ring + grid + telemetry + the generic send seam)
//! lives in `audio_emitter.rs` and is Linux-tested. This module owns only the
//! parts that need the live Windows runtime and cannot run on the Linux CI:
//!
//! - the dedicated OS emit thread, raised to `THREAD_PRIORITY_TIME_CRITICAL`
//!   (audio-app class) so a heavy child's CPU burst cannot delay the grid;
//! - the wall-clock sleep-until + ≤ 2 ms spin loop that fires one grid slot
//!   every 33.333 ms (`WallClock` QPC readings, the 1 ms multimedia timer);
//! - the [`AudioEmitterThread`] guard whose `Drop` signals shutdown, wakes any
//!   blocked decoder push, and JOINS the thread — MUST run before the
//!   `FrameSubmitter` (and thus the `NdiSender`) is dropped, since
//!   `send_destroy` invalidates the handle the [`AudioSink`] holds.
//!
//! `mutants::skip` throughout — no Linux test path (box-verified), mirroring
//! `pipeline_paced_submit.rs`.

use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;

use sp_ndi::{AudioSink, RealNdiBackend};
use tracing::{info, warn};

use crate::playback::pipeline::audio_emitter::{
    AUDIO_LOOKAHEAD_MS, EmittedBlock, SharedEmitter, SpinMargin, clear_ring, decoder_tolerance_ms,
    drain_budget_ms, emit_one_block, hold_ring, push_blocking,
};
use crate::playback::wallclock::WallClock;

/// #192 round 4: is THIS decoded video frame late enough to drop (its audio is
/// already pushed) so the video catches up to the wall-clock audio position?
/// Reads the LIVE ring depth (the same `AudioEmitter::ring_depth_ms()` the
/// heartbeat logs) under the ring lock and applies the pure
/// [`CatchUp::step`](crate::playback::av_catchup::CatchUp::step): the target is
/// `target_ring_depth_ms()` and one-frame budget is the sync decoder's pairing
/// tolerance. No emitter (legacy audio-with-video path) → never late. Windows-only
/// glue (`mutants::skip`) over the Linux-tested pure decision.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn is_late_frame(
    catchup: &mut crate::playback::av_catchup::CatchUp,
    ring_depth_ms: Option<u64>,
    video_ts_ms: u64,
    duration_ms: u64,
) -> bool {
    // #198 item 8: the ring depth is read ONCE, under the lock
    // `push_or_collect_audio` already holds, and passed in here — this function
    // no longer takes the emitter mutex a SECOND time per decoded frame. No
    // emitter (legacy audio-with-video path) → `ring_depth_ms` is `None` → never
    // late (its audio rides with the video, so there is nothing to catch up to).
    let Some(depth_ms) = ring_depth_ms else {
        return false;
    };
    let target_ms = crate::playback::pipeline::audio_emitter::target_ring_depth_ms();
    let frame_ms = sp_decoder::split_sync::DEFAULT_TOLERANCE_MS;
    // Within the ring target of the end the audio stream is at EOF: a shallow
    // ring there is not a lag — never drop the tail.
    let at_end = video_ts_ms.saturating_add(target_ms) >= duration_ms;
    matches!(
        catchup.step(depth_ms, target_ms, frame_ms, at_end),
        crate::playback::av_catchup::Decision::Drop
    )
}

/// The SDK-clocked decode loop's audio seam (#192): with the wall-clock emitter
/// present, PUSH each decoded frame's interleaved audio into its bounded ring
/// (before that video frame's submit) and hand `submit_nv12` NO audio — the
/// emit thread clocks it out continuously. Without an emitter (spawn failed),
/// fall back to the legacy behaviour so audio is never silently dropped: return
/// the `AudioFrame`s for `submit_nv12` to send alongside the video. Kept here to
/// hold `pipeline.rs` under the 1000-line cap. `mutants::skip` — the pure
/// `push_blocking` it delegates to is Linux-tested.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn push_or_collect_audio(
    emitter: Option<&SharedEmitter>,
    audio_frames: Vec<sp_decoder::DecodedAudioFrame>,
) -> (Vec<sp_ndi::AudioFrame>, Option<u64>) {
    match emitter {
        Some(shared) => {
            for af in &audio_frames {
                push_blocking(shared, &af.data, af.channels as usize);
            }
            // #198 item 8: read the live ring depth ONCE here, under the emitter
            // lock this function already deals with, and return it so
            // `is_late_frame` consumes it instead of taking the mutex a second
            // time per frame.
            let ring_depth_ms = shared.emitter.lock().unwrap().ring_depth_ms();
            (Vec::new(), Some(ring_depth_ms))
        }
        None => (
            audio_frames
                .into_iter()
                .map(|af| sp_ndi::AudioFrame {
                    data: af.data,
                    channels: af.channels,
                    sample_rate: af.sample_rate,
                    // Stamped by FrameSubmitter at submission time (#146).
                    timecode_100ns: None,
                })
                .collect(),
            None,
        ),
    }
}

/// Open the split A/V decoder for the SDK-clocked loop (#192 round 2). With the
/// emitter present: drop stale ring audio left by an interrupted song, and pair
/// audio AHEAD of the video by the cushion (`decoder_tolerance_ms`); without it,
/// the plain pairing — audio then rides with the video frames.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn open_synced_decoder(
    video: Box<dyn sp_decoder::VideoStream>,
    audio: Box<dyn sp_decoder::AudioStream>,
    emitter: Option<&SharedEmitter>,
) -> Result<sp_decoder::SplitSyncedDecoder, sp_decoder::DecoderError> {
    if let Some(shared) = emitter {
        clear_ring(shared);
    }
    sp_decoder::SplitSyncedDecoder::with_audio_lead(
        video,
        audio,
        decoder_tolerance_ms(emitter.is_some()),
    )
}

/// Pause: keep the ring cushion while the decode loop idles (released by the
/// next audio push).
#[cfg_attr(test, mutants::skip)]
pub(crate) fn hold_if_present(emitter: Option<&SharedEmitter>) {
    if let Some(shared) = emitter {
        hold_ring(shared);
    }
}

/// Seek: drop the stale pre-seek audio so it never queues ahead of the new
/// position.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn clear_if_present(emitter: Option<&SharedEmitter>) {
    if let Some(shared) = emitter {
        clear_ring(shared);
    }
}

/// Natural song end (#192 item 3): give the emit thread up to
/// [`drain_budget_ms`]`(AUDIO_LOOKAHEAD_MS)` to drain the ring so the song's
/// buffered tail is emitted BEFORE the decoder returns and the next song's
/// `clear_ring` wipes it. The budget tracks the cushion (round 3: 1500 ms + one
/// slot) — a fixed 400 ms would cut the last ~1.1 s of every song once the
/// lookahead is 1500 ms. Polls the pure [`ring_is_drained`] every 5 ms and
/// breaks the moment the ring holds less than one block; a no-op when no emitter
/// is present (legacy path). Only the natural-end path calls this —
/// Stop/Play/Shutdown clear the ring instead. `mutants::skip` glue over the
/// Linux-tested `ring_is_drained` + the pure `drain_budget_ms`.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn drain_if_present(emitter: Option<&SharedEmitter>) {
    let Some(shared) = emitter else { return };
    let deadline =
        std::time::Instant::now() + Duration::from_millis(drain_budget_ms(AUDIO_LOOKAHEAD_MS));
    while std::time::Instant::now() < deadline {
        if crate::playback::pipeline::audio_emitter::ring_is_drained(shared) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A running audio-emitter thread bound to one pipeline's NDI sender. Dropping
/// it stops the emitter and joins — do this BEFORE the sender is destroyed.
pub(crate) struct AudioEmitterThread {
    shared: SharedEmitter,
    join: Option<JoinHandle<()>>,
}

impl AudioEmitterThread {
    /// The shared emitter the decode loop pushes decoded audio into.
    pub(crate) fn shared(&self) -> &SharedEmitter {
        &self.shared
    }
}

impl Drop for AudioEmitterThread {
    #[cfg_attr(test, mutants::skip)]
    fn drop(&mut self) {
        // Signal the emit loop to exit and wake any decode-thread push blocked
        // on a full ring so it returns instead of hanging.
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.shared.space.notify_all();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        info!("audio-emitter: thread joined (pipeline teardown)");
    }
}

/// Spawn the wall-clock audio emitter thread for one pipeline's `sink`. Returns
/// the guard that owns the thread + the shared ring the decode loop fills, or
/// `None` when the OS thread could not be spawned — in which case the caller
/// leaves `audio_emitter = None` so `decode_and_send` takes the LEGACY
/// audio-with-video path. Returning a guard with a dead (never-drained) ring
/// would hang the decode thread once the ring filled — the sole drain is
/// `run_emit_loop`, which never ran.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn spawn_audio_emitter(
    ndi_name: &str,
    sink: AudioSink<RealNdiBackend>,
    shared: SharedEmitter,
) -> Option<AudioEmitterThread> {
    // 1 ms multimedia timer so the coarse sleep lands within the 2 ms spin
    // margin (ref-counted; the paced path requests the same).
    crate::playback::pipeline_paced::request_high_res_timer();

    let thread_shared = shared.clone();
    let name = ndi_name.to_string();
    match std::thread::Builder::new()
        .name(format!("audio-emit-{name}"))
        .spawn(move || run_emit_loop(&name, sink, thread_shared))
    {
        Ok(join) => Some(AudioEmitterThread {
            shared,
            join: Some(join),
        }),
        Err(e) => {
            warn!(error = %e, ndi_name, "audio-emitter: failed to spawn thread — falling back to legacy audio-with-video submit");
            None
        }
    }
}

/// The emit loop: one grid block every 33.333 ms on the wall-clock grid,
/// TIME_CRITICAL, until shutdown. Sends audio-or-silence through `sink` and
/// logs a per-minute heartbeat plus one line per silence→audio transition.
#[cfg_attr(test, mutants::skip)]
fn run_emit_loop(ndi_name: &str, sink: AudioSink<RealNdiBackend>, shared: SharedEmitter) {
    raise_thread_priority(ndi_name);
    let clock = WallClock::system();

    let mut last_log_minute: i64 = i64::MIN;
    let mut silence_run: u64 = 0;
    // Adaptive spin margin: follows the coarse sleep's observed overshoot while
    // the pipeline carries audio (box finding: a fixed 2 ms left p99 at 2–5 ms).
    let mut spin = SpinMargin::default();
    // Longest single emit call (ring lock + NDI send_audio) this minute: tells a
    // late slot caused by the SDK/lock apart from one caused by the wake-up.
    let mut emit_call_max_100ns: i64 = 0;
    info!(
        ndi_name,
        "audio-emitter: started (sdk-video/wallclock-audio, 48kHz/1600-block grid)"
    );

    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        // Sleep until this slot's grid boundary.
        let now = clock.now_100ns();
        let target = {
            let g = shared.emitter.lock().unwrap();
            g.next_boundary_100ns(now)
        };
        let overshoot = sleep_until(&clock, target, spin.margin_100ns());
        spin.observe(overshoot);
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }

        let emit_now = clock.now_100ns();
        let emitted = emit_one_block(&shared, &sink, emit_now);
        emit_call_max_100ns = emit_call_max_100ns.max(clock.now_100ns() - emit_now);

        // Transition log: one line per gap, on the silence→audio edge.
        match emitted.block {
            EmittedBlock::Silence => {
                silence_run += 1;
                spin.note_block(false);
            }
            EmittedBlock::Audio => {
                spin.note_block(true);
                if silence_run > 0 {
                    info!(
                        ndi_name,
                        silence_blocks = silence_run,
                        "audio-emitter: audio resumed after silence (song transition / stall)"
                    );
                    silence_run = 0;
                }
            }
        }

        // Per-minute emitter heartbeat.
        let minute = emit_now / 600_000_000; // 100 ns units per minute
        if minute != last_log_minute {
            last_log_minute = minute;
            let (slots, silence, late, jitter, ring_ms, resyncs) = {
                let g = shared.emitter.lock().unwrap();
                (
                    g.emitted_slots(),
                    g.silence_blocks(),
                    g.late_blocks(),
                    g.emit_jitter_p99_us(),
                    g.ring_depth_ms(),
                    g.resyncs(),
                )
            };
            info!(
                ndi_name,
                slots,
                silence,
                late,
                resyncs,
                jitter_p99_us = jitter,
                ring_ms,
                spin_margin_us = spin.margin_100ns() / 10,
                emit_call_max_us = emit_call_max_100ns / 10,
                "audio-emitter: heartbeat"
            );
            emit_call_max_100ns = 0;
        }
    }
    info!(ndi_name, "audio-emitter: loop exited (shutdown)");
}

/// Coarse-sleep the monotonic clock to `margin_100ns` before `target_100ns`, then
/// spin to the boundary. Returns how far past its intended wake time the coarse
/// sleep came back (0 when no coarse sleep ran) — the input of [`SpinMargin`]. A
/// target already in the past returns immediately (the emit thread catches up
/// one slot per iteration — it never skips a slot, and the grid timecode stays
/// correct, so a burst is absorbed by the receiver buffer).
#[cfg_attr(test, mutants::skip)]
fn sleep_until(clock: &WallClock, target_100ns: i64, margin_100ns: i64) -> i64 {
    let delta = target_100ns - clock.now_100ns();
    if delta <= 0 {
        return 0;
    }
    let mut overshoot = 0;
    if delta > margin_100ns {
        let coarse = delta - margin_100ns;
        std::thread::sleep(Duration::from_nanos((coarse * 100) as u64));
        overshoot = clock.now_100ns() - (target_100ns - margin_100ns);
    }
    loop {
        if target_100ns - clock.now_100ns() <= 0 {
            break;
        }
        std::hint::spin_loop();
    }
    overshoot
}

/// Raise the calling thread to `THREAD_PRIORITY_TIME_CRITICAL` so a heavy
/// child's CPU/memory burst cannot delay a grid slot. `ndi_name` labels the log
/// line (the audio emit thread's NDI name, or `vban-output` for the #210 VBAN
/// sender thread, which shares this helper).
#[cfg_attr(test, mutants::skip)]
pub(crate) fn raise_thread_priority(ndi_name: &str) {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };
    // SAFETY: GetCurrentThread returns a pseudo-handle valid for the calling
    // thread; SetThreadPriority is a leaf call with primitive args.
    let ok = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
    if ok == 0 {
        warn!(
            ndi_name,
            "paced thread: SetThreadPriority(TIME_CRITICAL) failed"
        );
    } else {
        info!(ndi_name, "paced thread: priority = TIME_CRITICAL");
    }
}

// This module is `#[cfg(windows)]`, so these tests run on the Windows CI job's
// `cargo test --workspace`. `is_late_frame` is now PURE (it takes the ring depth
// as a parameter instead of locking the emitter), so its decision is testable.
#[cfg(test)]
mod tests {
    use super::is_late_frame;
    use crate::playback::av_catchup::CatchUp;
    use crate::playback::pipeline::audio_emitter::target_ring_depth_ms;

    #[test]
    fn is_late_frame_uses_the_supplied_ring_depth() {
        let mut c = CatchUp::new();
        // No emitter → the ring depth is `None` → never late (legacy path: the
        // audio rides with the video, nothing to catch up to).
        assert!(!is_late_frame(&mut c, None, 0, u64::MAX));

        let target = target_ring_depth_ms();
        // A full ring primes the catch-up and is on time (not late)…
        assert!(!is_late_frame(&mut c, Some(target), 0, u64::MAX));
        // …then a drained ring (deep lag) once primed → drop this frame's video.
        assert!(is_late_frame(&mut c, Some(0), 0, u64::MAX));
        // Near the song end a shallow ring is EOF, not a lag → never late.
        assert!(!is_late_frame(&mut c, Some(0), 100, 100));
    }
}
