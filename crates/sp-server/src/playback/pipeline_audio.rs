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
    EmittedBlock, SharedEmitter, emit_one_block, push_blocking,
};
use crate::playback::wallclock::WallClock;

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
) -> Vec<sp_ndi::AudioFrame> {
    match emitter {
        Some(shared) => {
            for af in &audio_frames {
                push_blocking(shared, &af.data, af.channels as usize);
            }
            Vec::new()
        }
        None => audio_frames
            .into_iter()
            .map(|af| sp_ndi::AudioFrame {
                data: af.data,
                channels: af.channels,
                sample_rate: af.sample_rate,
                // Stamped by FrameSubmitter at submission time (#146).
                timecode_100ns: None,
            })
            .collect(),
    }
}

/// Spin the last ~2 ms to the grid boundary (matches the paced path's margin).
const SPIN_MARGIN_100NS: i64 = 20_000;

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
        sleep_until(&clock, target);
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }

        let emit_now = clock.now_100ns();
        let emitted = emit_one_block(&shared, &sink, emit_now);

        // Transition log: one line per gap, on the silence→audio edge.
        match emitted.block {
            EmittedBlock::Silence => silence_run += 1,
            EmittedBlock::Audio(_) => {
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
            let (slots, silence, late, jitter, ring_ms) = {
                let g = shared.emitter.lock().unwrap();
                (
                    g.emitted_slots(),
                    g.silence_blocks(),
                    g.late_blocks(),
                    g.emit_jitter_p99_us(),
                    g.ring_depth_ms(),
                )
            };
            info!(
                ndi_name,
                slots,
                silence,
                late,
                jitter_p99_us = jitter,
                ring_ms,
                "audio-emitter: heartbeat"
            );
        }
    }
    info!(ndi_name, "audio-emitter: loop exited (shutdown)");
}

/// Coarse-sleep the monotonic clock to ~2 ms before `target_100ns`, then spin
/// to the boundary. A target already in the past returns immediately (the emit
/// thread catches up one slot per iteration — it never skips a slot, and the
/// grid timecode stays correct, so a burst is absorbed by the receiver buffer).
#[cfg_attr(test, mutants::skip)]
fn sleep_until(clock: &WallClock, target_100ns: i64) {
    let delta = target_100ns - clock.now_100ns();
    if delta <= 0 {
        return;
    }
    if delta > SPIN_MARGIN_100NS {
        let coarse = delta - SPIN_MARGIN_100NS;
        std::thread::sleep(Duration::from_nanos((coarse * 100) as u64));
    }
    loop {
        if target_100ns - clock.now_100ns() <= 0 {
            break;
        }
        std::hint::spin_loop();
    }
}

/// Raise the emit thread to `THREAD_PRIORITY_TIME_CRITICAL` so a heavy child's
/// CPU/memory burst cannot delay a grid slot.
#[cfg_attr(test, mutants::skip)]
fn raise_thread_priority(ndi_name: &str) {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };
    // SAFETY: GetCurrentThread returns a pseudo-handle valid for the calling
    // thread; SetThreadPriority is a leaf call with primitive args.
    let ok = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
    if ok == 0 {
        warn!(
            ndi_name,
            "audio-emitter: SetThreadPriority(TIME_CRITICAL) failed"
        );
    } else {
        info!(ndi_name, "audio-emitter: thread priority = TIME_CRITICAL");
    }
}
