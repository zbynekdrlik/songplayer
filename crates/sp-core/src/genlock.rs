//! Genlock wall-clock boundary math (#146).
//!
//! Pure integer arithmetic, WASM-safe: this module makes **no** clock calls
//! (`Instant::now` / `SystemTime::now` are forbidden in `sp-core`) — callers
//! supply the wall-clock reading. The functions mirror camera-box#1294 §3
//! (grid) and §4 (video timecode) 1:1 in both algorithm and rounding, so the
//! contract's reference vectors transfer directly (see `genlock_tests.rs`).
//!
//! **FLOOR, never ceil.** The video timecode is the greatest grid boundary at
//! or before the present wall time. A future-dated (ceil) stamp arms the
//! receiver's backward-step guard — the 2026-08-07 −900 ms hold collapse
//! (camera-box#1009/#1007, measured margin 0.3 ms at trigger). See
//! [`floor_boundary_100ns`].

/// 100-ns units per second (the NDI timecode unit).
pub const UNITS_PER_SECOND: i64 = 10_000_000;

/// Re-sample the monotonic-to-realtime offset at least every this many frames
/// (camera-box `OFFSET_RESAMPLE_INTERVAL_FRAMES`).
pub const OFFSET_RESAMPLE_INTERVAL_FRAMES: u64 = 100;

/// The nominal grid rate SongPlayer stamps against — the receiving canvas
/// rate (cg OBS = 30 fps), camera-box#1294 §3. Per-output overrides are a
/// later ticket (#147, open question 1).
pub const GENLOCK_GRID_FPS: i64 = 30;

/// Length of one grid interval in 100-ns units: `1e7 / fps`.
///
/// `30 fps -> 333_333`, `60 fps -> 166_666`. Returns 0 for a non-positive
/// `fps` rather than dividing by zero.
pub fn interval_100ns(fps: i64) -> i64 {
    if fps <= 0 {
        return 0;
    }
    UNITS_PER_SECOND / fps
}

/// The genlock video timecode: the greatest grid boundary `<= now_100ns`, in
/// 100-ns units since the Unix epoch. camera-box `src/ndi.rs:78-97`.
///
/// Algorithm: anchor to the containing second, recover the slot by integer
/// division, then apply the load-bearing promotion fix. Because `1e7/fps` is
/// truncated (`b1 @ 30 = 333_333`, below the exact rational `333_333.33`),
/// naive slot recovery under-counts on an exact boundary; the promotion adds
/// the missing slot back. A non-positive `fps` returns `now_100ns` unchanged.
///
/// **Input domain:** `now_100ns` is a wall-clock reading in 100-ns units since
/// the Unix epoch, so by contract it is non-negative and post-2020 (the
/// receiver only ts-aligns values inside the 2020..2100 window, camera-box#1294
/// §4). Negative inputs are out of scope and are NOT handled specially: integer
/// division truncates toward zero, so a negative `now_100ns` would floor toward
/// zero (the wrong direction). Production never supplies one.
pub fn floor_boundary_100ns(now_100ns: i64, fps: i64) -> i64 {
    if fps <= 0 {
        return now_100ns;
    }
    // Start of the containing second.
    let cs = (now_100ns / UNITS_PER_SECOND) * UNITS_PER_SECOND;
    let off = now_100ns - cs;
    let mut slot = off * fps / UNITS_PER_SECOND;
    // Promotion fix: if the next slot's boundary is still <= off, we
    // under-counted by one (integer-truncated interval).
    if (slot + 1) * UNITS_PER_SECOND / fps <= off {
        slot += 1;
    }
    cs + slot * UNITS_PER_SECOND / fps
}

/// Round a rational frame rate `n/d` to the nearest integer grid rate
/// (59.94 -> 60, 29.97 -> 30). camera-box `src/ndi.rs:133-139`.
///
/// Returns 0 when `d == 0`. Truncation here would silently drift the grid to
/// 59/29 — the defect this round guards against.
pub fn fps_from_frame_rate(n: i64, d: i64) -> i64 {
    if d == 0 {
        return 0;
    }
    (n + d / 2) / d
}

/// Realtime timecode for a captured frame: `mono + offset`, saturating so a
/// clock at the i64 extremes never wraps. camera-box capture path.
pub fn capture_realtime_100ns(mono: i64, offset: i64) -> i64 {
    mono.saturating_add(offset)
}

/// Whether the monotonic-to-realtime offset should be re-sampled, given how
/// many frames have elapsed since the last resample (`>= 100`, camera-box
/// `OFFSET_RESAMPLE_INTERVAL_FRAMES`).
pub fn should_resample_mono_to_real_offset(frames_since: u64) -> bool {
    frames_since >= OFFSET_RESAMPLE_INTERVAL_FRAMES
}

// ---------------------------------------------------------------------------
// Boundary-paced emission (#147) — the pure decision layer.
//
// Ported 1:1 in semantics from camera-box `src/genlock_pacing.rs`
// (`genlock_emit_gate`, `GENLOCK_MAX_CATCHUP_INTERVALS`, `genlock_emit_on_time`,
// `genlock_lag_intervals`, `boundary_skip_count`, the latched-boundary helper,
// `starvation_repeat_timecode`) plus the ceil twin `next_boundary_100ns`.
// SongPlayer uses `i64` throughout this module (camera-box uses `u64`); the
// inputs are non-negative wall-clock readings so the arithmetic is identical.
//
// Units: PACING works in wall-clock **ns** since the Unix epoch
// (`interval_ns`, `I30 = 33_333_333`); the emitted timecode STAMP is in 100-ns
// units via `floor_boundary_100ns` (never `next_boundary_100ns`, which is a
// ceil sleep target only).
// ---------------------------------------------------------------------------

/// Nanoseconds per second — the wall-clock pacing grid unit (vs. the 100-ns
/// stamp unit [`UNITS_PER_SECOND`]).
pub const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// The largest lag, in whole emit-boundary intervals, the pacer absorbs by
/// CATCHING UP one interval per emit before it gives up and grid-resyncs
/// (leaps forward, dropping the intervening boundaries). camera-box
/// `genlock_pacing.rs::GENLOCK_MAX_CATCHUP_INTERVALS`. 8 = 2× the 4-deep
/// capture queue's max buffered drain, while staying far below a real
/// wall-clock STEP (seconds → hundreds of intervals).
pub const GENLOCK_MAX_CATCHUP_INTERVALS: i64 = 8;

/// Length of one pacing interval in whole nanoseconds: `1e9 / fps`.
///
/// `30 fps -> 33_333_333` (`I30`), `60 fps -> 16_666_666`. Returns 0 for a
/// non-positive `fps` (genlock off) rather than dividing by zero — the guarded
/// divisor case matching [`genlock_emit_gate`].
pub fn interval_ns(fps: i64) -> i64 {
    if fps <= 0 {
        return 0;
    }
    NANOS_PER_SECOND / fps
}

/// The next grid boundary STRICTLY GREATER than `now_100ns` — the ceil twin of
/// [`floor_boundary_100ns`], in 100-ns units. camera-box `src/ndi.rs`
/// `next_boundary_100ns`.
///
/// **Sleep / pacing target ONLY, never a timecode stamp.** A stamp must FLOOR
/// (§4); a future-dated ceil stamp arms the receiver's backward-step guard.
/// A non-positive `fps` returns `now_100ns` unchanged (genlock off → zero wait).
///
/// **Idempotent on the promotion boundaries it produces.** This is a faithful
/// port of the camera-box ceil twin, which lacks [`floor_boundary_100ns`]'s
/// promotion fix. So for an input that sits EXACTLY on a boundary whose slot
/// recovery under-counts (`b_1 @30 = 333_333`, below the rational `333_333.33`),
/// `next_boundary_100ns(b) == b` rather than the next slot. When you need the
/// grid boundary STRICTLY greater than a value that may already be on the grid
/// (the emit gate's one-slot catch-up / resync / re-latch targets), use
/// [`strict_next_boundary_100ns`], which advances even from an on-grid input.
pub fn next_boundary_100ns(now_100ns: i64, fps: i64) -> i64 {
    if fps <= 0 {
        return now_100ns;
    }
    let current_second_100ns = (now_100ns / UNITS_PER_SECOND) * UNITS_PER_SECOND;
    let offset_in_second = now_100ns - current_second_100ns;
    // Which frame within this second (0 .. fps-1); multiply before divide.
    let frame_in_second = (offset_in_second * fps) / UNITS_PER_SECOND;
    let next_frame_in_second = frame_in_second + 1;
    if next_frame_in_second >= fps {
        // Next boundary is the start of the next second (exactly X.000).
        current_second_100ns + UNITS_PER_SECOND
    } else {
        current_second_100ns + (next_frame_in_second * UNITS_PER_SECOND / fps)
    }
}

/// The exact-rational grid boundary STRICTLY GREATER than `x_100ns`, always
/// advancing — the on-grid-safe wrapper around [`next_boundary_100ns`].
///
/// [`next_boundary_100ns`] is idempotent on the promotion boundaries it itself
/// produces (`next_boundary_100ns(333_333) == 333_333`), so feeding it a value
/// that is already on the grid can stall. The exact-grid emit gate
/// ([`genlock_emit_gate_100ns`]) advances a boundary it produced (the pending
/// `next_boundary`) by ONE slot, and re-latches / resyncs from a possibly
/// on-grid instant — every one of those needs a value STRICTLY greater than the
/// input. This helper delivers it: it uses the plain ceil when that already
/// advances, and otherwise nudges one 100-ns tick past `x` (landing inside the
/// next slot) before ceiling, so an on-grid input yields the NEXT slot, never
/// itself. A non-positive `fps` returns `x_100ns` unchanged.
pub fn strict_next_boundary_100ns(x_100ns: i64, fps: i64) -> i64 {
    if fps <= 0 {
        return x_100ns;
    }
    let nb = next_boundary_100ns(x_100ns, fps);
    if nb > x_100ns {
        nb
    } else {
        // `x` is exactly on a promotion boundary (ceil was idempotent): step one
        // tick into the next slot, then ceil back onto the grid.
        next_boundary_100ns(x_100ns + 1, fps)
    }
}

/// `true` iff `floor_now` sits MORE than [`GENLOCK_MAX_CATCHUP_INTERVALS`] grid
/// slots past `boundary` — the exact-grid resync threshold. The exact-rational
/// slots are 333_333 or 333_334 wide (at 30 fps), so a fixed-interval division
/// miscounts across a second boundary; this STEPS the grid instead, exactly
/// `GENLOCK_MAX_CATCHUP_INTERVALS + 1` times. If the stepped boundary is still
/// at-or-before `floor_now`, the lag exceeds the bound. Bounded work (9 steps at
/// 30 fps) — the whole count is never materialised, only "> bound?". Also the
/// #212 NDI input thread's resync rule (`playback::ndi_input::grid_step`).
pub fn lag_over_catchup_bound_100ns(boundary_100ns: i64, floor_now_100ns: i64, fps: i64) -> bool {
    let mut b = boundary_100ns;
    for _ in 0..=GENLOCK_MAX_CATCHUP_INTERVALS {
        b = strict_next_boundary_100ns(b, fps);
    }
    b <= floor_now_100ns
}

/// The EXACT-100-ns-grid twin of [`genlock_emit_gate`] — the gate the `Pacer`
/// actually uses, because SongPlayer GENERATES its own timing (it stamps and
/// sleeps on the same second-anchored exact-rational grid, so pacing and stamps
/// must share ONE grid). Same decision rules as the ns gate, but every boundary
/// is produced by the exact-rational helpers, so the returned boundary is always
/// on the grid, strictly increasing, and never future-dated.
///
/// - latched boundary = `if nb == 0 || nb > floor_boundary_100ns(now) +
///   interval_100ns { strict_next_boundary_100ns(now) } else { nb }`
///   (init / backward-step re-latch to the next grid boundary after `now`).
/// - `now < boundary` → do not emit; keep the boundary.
/// - crossed the boundary → emit; advance ONE grid slot
///   ([`strict_next_boundary_100ns`] of the boundary), UNLESS the lag exceeds
///   [`GENLOCK_MAX_CATCHUP_INTERVALS`] AND nothing is buffered
///   (`!queue_had_frame`) → grid-resync to the next boundary after `now`.
///   Lag is counted by STEPPING the grid (`lag_over_catchup_bound_100ns`), not
///   by dividing by a nominal interval, because the slots are unequal
///   (333_333 vs 333_334 wide within a second).
///
/// **Deviation from the ns port, documented deliberately:** the ns gate advances
/// with `boundary + interval` and re-latches with `now - now % interval +
/// interval`; the exact-grid twin uses [`strict_next_boundary_100ns`] rather
/// than the bare [`next_boundary_100ns`] the naming would suggest, because the
/// latter is idempotent on the promotion boundaries it produces and would stall
/// the pacer (a one-slot catch-up from an on-grid `nb` would return `nb`).
pub fn genlock_emit_gate_100ns(
    now_100ns: i64,
    next_boundary_100ns_in: i64,
    fps: i64,
    queue_had_frame: bool,
) -> (bool, i64) {
    let interval = interval_100ns(fps);
    if interval == 0 {
        return (false, next_boundary_100ns_in);
    }
    let floor_now = floor_boundary_100ns(now_100ns, fps);
    let boundary = if next_boundary_100ns_in == 0 || next_boundary_100ns_in > floor_now + interval {
        strict_next_boundary_100ns(now_100ns, fps)
    } else {
        next_boundary_100ns_in
    };
    if now_100ns < boundary {
        return (false, boundary);
    }
    let next = if !queue_had_frame && lag_over_catchup_bound_100ns(boundary, floor_now, fps) {
        // Genuine wall-clock step with nothing buffered → grid-resync.
        strict_next_boundary_100ns(now_100ns, fps)
    } else {
        // Catch up exactly one grid slot (never leap past a buffered frame).
        strict_next_boundary_100ns(boundary, fps)
    };
    (true, next)
}

/// The wall-clock boundary [`genlock_emit_gate`] latches for `now_ns`, factored
/// out so [`genlock_emit_on_time`] and [`genlock_lag_intervals`] compute the
/// IDENTICAL boundary. Initialises (`next_boundary_ns == 0`) or re-latches a
/// BACKWARD clock step (`next_boundary_ns > now_ns + interval_ns`) to the next
/// boundary above `now_ns`; otherwise keeps the pending boundary. camera-box
/// `genlock_pacing.rs::genlock_latched_boundary`. The caller guards
/// `interval_ns != 0`.
fn genlock_latched_boundary(now_ns: i64, next_boundary_ns: i64, interval_ns: i64) -> i64 {
    if next_boundary_ns == 0 || next_boundary_ns > now_ns + interval_ns {
        now_ns - (now_ns % interval_ns) + interval_ns
    } else {
        next_boundary_ns
    }
}

/// The wall-clock (ns) emit gate: given the current wall time `now_ns`, the
/// pending emit boundary `next_boundary_ns` (0 = uninitialised), the boundary
/// `interval_ns` (`1e9 / fps`), and `queue_had_frame` (is a real frame still
/// buffered/pending — #1131), decide whether THIS boundary emits and return the
/// updated next boundary. camera-box `genlock_pacing.rs::genlock_emit_gate`.
///
/// This is the faithful **reference port** of camera-box (which decimates over
/// grabber ARRIVALS on a uniform ns grid). SongPlayer's `Pacer` does NOT use it
/// — it uses the exact-grid [`genlock_emit_gate_100ns`] twin, because SongPlayer
/// GENERATES the timing itself and must pace on the same second-anchored
/// exact-rational 100-ns grid it stamps on. This port is kept, with its 43
/// contract vectors, so the SongPlayer twin can be diffed against the normative
/// camera-box semantics.
///
/// - `interval_ns == 0` → genlock off: never emit, boundary unchanged, no panic.
/// - `now_ns < boundary` → between boundaries: do not emit; keep the boundary.
/// - crossed the boundary → emit; advance one interval, UNLESS the lag exceeds
///   [`GENLOCK_MAX_CATCHUP_INTERVALS`] **and** nothing is buffered
///   (`!queue_had_frame`), in which case grid-resync to just after `now_ns`
///   (a real clock STEP; the skipped boundaries had no content). A lag at or
///   within the bound, or any lag while a frame is buffered, catches up ONE
///   interval so no buffered frame is leaped-past and dropped.
pub fn genlock_emit_gate(
    now_ns: i64,
    next_boundary_ns: i64,
    interval_ns: i64,
    queue_had_frame: bool,
) -> (bool, i64) {
    if interval_ns == 0 {
        return (false, next_boundary_ns);
    }
    let boundary = genlock_latched_boundary(now_ns, next_boundary_ns, interval_ns);
    if now_ns < boundary {
        return (false, boundary);
    }
    let mut next = boundary + interval_ns;
    if next <= now_ns {
        // Fell behind. A lag beyond the catch-up bound with NOTHING buffered is
        // a genuine wall-clock STEP → grid-resync; otherwise catch up one
        // interval (fill the next un-emitted boundary) so no buffered frame is
        // discarded in a run.
        let lag_intervals = (now_ns - boundary) / interval_ns; // >= 1 here
        if lag_intervals > GENLOCK_MAX_CATCHUP_INTERVALS && !queue_had_frame {
            next = now_ns - (now_ns % interval_ns) + interval_ns;
        }
    }
    (true, next)
}

/// Is `now_ns` an ON-TIME boundary crossing (the "surplus" regime), as opposed
/// to a LATE catch-up crossing? True iff the capture has reached the pending
/// boundary AND the NEXT boundary is still in the future. FALSE both between
/// boundaries (`now < boundary`) and once the gate has fallen behind
/// (`boundary + interval <= now`). Shares [`genlock_latched_boundary`] with
/// [`genlock_emit_gate`]. camera-box `genlock_pacing.rs::genlock_emit_on_time`.
pub fn genlock_emit_on_time(now_ns: i64, next_boundary_ns: i64, interval_ns: i64) -> bool {
    if interval_ns == 0 {
        return false;
    }
    let boundary = genlock_latched_boundary(now_ns, next_boundary_ns, interval_ns);
    now_ns >= boundary && boundary + interval_ns > now_ns
}

/// How many WHOLE emit-boundary intervals `now_ns` sits PAST the pending
/// boundary: `0` on-time/surplus or still before the boundary, `>= 1` once the
/// gate has fallen behind. Shares [`genlock_latched_boundary`] with
/// [`genlock_emit_gate`]. camera-box `genlock_pacing.rs::genlock_lag_intervals`.
pub fn genlock_lag_intervals(now_ns: i64, next_boundary_ns: i64, interval_ns: i64) -> i64 {
    if interval_ns == 0 {
        return 0;
    }
    let boundary = genlock_latched_boundary(now_ns, next_boundary_ns, interval_ns);
    if now_ns >= boundary {
        (now_ns - boundary) / interval_ns
    } else {
        0
    }
}

/// How many whole emit-boundary intervals were SKIPPED (never emitted) between
/// the boundary held before [`genlock_emit_gate`] and the one it returned:
/// `max(0, (new - old) / interval - 1)`. A normal advance (one interval) is 0;
/// a forward resync of `k` intervals reports `k - 1` skipped boundaries.
/// `interval == 0`, an uninitialised `old == 0`, or a backward step
/// (`new <= old`) all report 0. camera-box `genlock_pacing.rs::boundary_skip_count`.
pub fn boundary_skip_count(old_boundary_ns: i64, new_boundary_ns: i64, interval_ns: i64) -> i64 {
    if interval_ns == 0 || old_boundary_ns == 0 || new_boundary_ns <= old_boundary_ns {
        return 0;
    }
    let advanced = new_boundary_ns - old_boundary_ns;
    (advanced / interval_ns - 1).max(0)
}

/// How many whole grid slots `floor_now_100ns` sits PAST `boundary_100ns` — a
/// division-based lag GAUGE for telemetry and the #147 playback re-anchor
/// threshold. `0` when `floor_now <= boundary` (never negative), and `0` for a
/// non-positive `fps` (guarded divisor).
///
/// Unlike the STAMP grid — which must stay exact-rational (`floor_boundary_100ns`
/// / `strict_next_boundary_100ns`) because a 10 ns/s phase error compounds over
/// hours — this is a coarse count: it divides by the nominal `interval_100ns`
/// (333_333 @30 fps), so across a second boundary (where slots are 333_334 wide)
/// it can be off by one slot. That is immaterial for a gauge and for a
/// "> GENLOCK_MAX_CATCHUP_INTERVALS" threshold gated by a 1 s sustain window
/// (#147 lane 3, change 2). The exact resync decision still uses the stepping
/// gate inside [`genlock_emit_gate_100ns`].
pub fn lag_slots_100ns(boundary_100ns: i64, floor_now_100ns: i64, fps: i64) -> i64 {
    let interval = interval_100ns(fps);
    if interval == 0 || floor_now_100ns <= boundary_100ns {
        return 0;
    }
    (floor_now_100ns - boundary_100ns) / interval
}

/// The NDI emit timecode (100-ns units) for the `repeat_index`-th STARVATION
/// last-frame repeat — the boundary `repeat_index` whole send-fps frames BEFORE
/// the current frame's boundary `base_timecode_100ns`. Each repeat MUST carry
/// its own strictly-decreasing timecode or the downstream genlock FIFO collapses
/// them into one slot. One send-fps frame is `1e7 / fps` in 100-ns units.
/// `fps <= 0` returns the base unchanged (guarded divisor). `repeat_index` is
/// 1-based. camera-box `genlock_pacing.rs::starvation_repeat_timecode_100ns`.
///
/// **Reference port — NOT used by SongPlayer's one-frame-per-boundary repeat.**
/// SongPlayer stamps a starvation repeat with the SERVICED boundary itself
/// (§5.5, on-grid and strictly increasing like any emit), not with a
/// backward-decreasing burst-backfill stamp. This decreasing-stamp helper covers
/// camera-box's burst backfill, which SongPlayer's pacer does not do; whether it
/// applies to a one-frame-per-boundary repeat at all is camera-box open
/// question 11 (camera-box#1294). Kept + tested so the two projects stay
/// diffable.
pub fn starvation_repeat_timecode_100ns(
    base_timecode_100ns: i64,
    repeat_index: i64,
    fps: i64,
) -> i64 {
    if fps <= 0 {
        return base_timecode_100ns;
    }
    let frame_interval_100ns = UNITS_PER_SECOND / fps;
    base_timecode_100ns - repeat_index * frame_interval_100ns
}

/// Paced-audio grid math (#148): `samples_per_boundary`. WASM-safe pure
/// integer math, mirroring camera-box#1294 §6.
#[path = "genlock_audio.rs"]
pub mod audio;

/// Genlock lock-state vocabulary (#149): the `LockState` three-state enum
/// (LOCKED / DEGRADED / UNLOCKED) + its pure [`lock_state::derive`], shared with
/// the API health snapshot, the per-minute log line, and (#150) the dashboard
/// badge.
#[path = "genlock_lock_state.rs"]
pub mod lock_state;

/// Burn-id payload + QR corner geometry (#151): the `P{run}.{frame}.{ts}.{crc}`
/// wire format, CRC-32, and bottom-right burn placement, ported 1:1 from
/// camera-box `src/probe/payload.rs` + `vendor/distroav/src/burn-geom.hpp`. Pure
/// numbers only — the QR encode + NV12 compositing live in `sp-server`.
#[path = "genlock_burn.rs"]
pub mod burn;

#[cfg(test)]
#[path = "genlock_tests.rs"]
mod genlock_tests;

#[cfg(test)]
#[path = "genlock_burn_tests.rs"]
mod genlock_burn_tests;

#[cfg(test)]
#[path = "genlock_lock_state_tests.rs"]
mod genlock_lock_state_tests;

#[cfg(test)]
#[path = "genlock_global_tests.rs"]
mod genlock_global_tests;

#[cfg(test)]
#[path = "genlock_tests_mutants.rs"]
mod genlock_tests_mutants;

#[cfg(test)]
#[path = "genlock_lock_state_tests_mutants.rs"]
mod genlock_lock_state_tests_mutants;
