//! Genlock lock-state vocabulary (#149, Lane 1, contract §7 A7.3).
//!
//! The three-state lock enum + its pure derivation, shared (WASM-safe) between
//! the `/api/v1/ndi/health` snapshot, the per-minute structured log line, and
//! (#150) the WASM dashboard badge — one definition of the LOCKED / DEGRADED /
//! UNLOCKED vocabulary the OBS indicator also speaks (camera-box#1298).
//!
//! The window counts fed to [`derive`] are produced engine-side by the
//! sp-server `playback::lock_state::EventWindow` (a 60 s ring of cumulative
//! pacing counters); this module owns only the state machine, so its rules are
//! testable with plain integers and no clock.

use serde::{Deserialize, Serialize};

/// The genlock lock state exposed on the health snapshot / log / UI badge.
/// Serialised as the exact camera-box#1298 tokens.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone, Copy)]
pub enum LockState {
    #[serde(rename = "LOCKED")]
    Locked,
    #[serde(rename = "DEGRADED")]
    Degraded,
    #[serde(rename = "UNLOCKED")]
    Unlocked,
}

impl LockState {
    /// The wire/log token for this state (`"LOCKED" | "DEGRADED" | "UNLOCKED"`),
    /// matching the serde rename so the log line and the JSON agree.
    pub fn as_str(&self) -> &'static str {
        match self {
            LockState::Locked => "LOCKED",
            LockState::Degraded => "DEGRADED",
            LockState::Unlocked => "UNLOCKED",
        }
    }
}

/// Rate-normalised lock inputs (#168 round 6, #149 calibration). Everything
/// [`derive`] needs to classify ONE output's genlock lock state, with the 60 s
/// window event counts normalised against the emitted slots so a structural
/// frame-rate conversion (24/25-fps content on the 30-fps grid) reads LOCKED,
/// not DEGRADED. WASM-safe: the rule below is pure integer permille arithmetic;
/// the only float is `source_fps`, folded to permille exactly once inside
/// [`expected_repeat_permille`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LockInputs {
    /// Box clock locked (dantesync LOCK/NANO).
    pub clock_ok: bool,
    /// Boundary pacing enabled for this output (`genlock_pacing`).
    pub pacing_enabled: bool,
    /// NDI receivers currently attached.
    pub connections: u32,
    /// Late emits in the 60 s window (a submit > 2 ms past its boundary).
    pub late_w: u64,
    /// Last-frame repeats in the 60 s window (the fps-conversion + any starvation).
    pub repeats_w: u64,
    /// Grid resyncs in the 60 s window.
    pub resyncs_w: u64,
    /// Boundaries serviced (slots emitted) in the 60 s window — the rate base.
    pub slots_w: u64,
    /// Nominal source frame rate of the playing file (e.g. `24.0`).
    pub source_fps: f32,
    /// The fixed integer grid rate the pacer runs (`GENLOCK_GRID_FPS = 30`).
    pub grid_fps: u32,
    /// The output is decoding content — its RAW pipeline transport is
    /// `TransportState::Playing` (#150). A paused / idle output under pacing
    /// keeps servicing the grid with STANDBY frames (the frozen last frame is
    /// a repeat on every slot, `repeats_w ≈ slots_w` by design), so the
    /// repeat-rate rule only applies while decoding.
    pub decoding: bool,
}

/// Late-fraction threshold: DEGRADED once late emits exceed 25 % (250 ‰) of the
/// emitted slots in the 60 s window. Calibrated 22.9.2026 (SP-slow, 24 fps
/// content on the 30 fps grid, 1 800 slots/min): a clean grid ran late ≤ 6 % of
/// slots (0–100/min, stems child resident); a sender-side STALL ran late ≈ 42 %
/// of slots (W1: ~750/min, 30–105 ms). 25 % sits well above clean, well below
/// stalled.
pub const LATE_DEGRADED_PERMILLE: u64 = 250;

/// Repeat-fraction MARGIN above the structural fps-conversion rate: DEGRADED
/// once repeats exceed `expected_repeat_permille + 10 %` (100 ‰) of the slots.
/// The conversion itself is exact and constant (24/30 → 200 ‰ = 20 % of slots
/// in EVERY window on 22.9.2026), so the margin only fires on genuine
/// starvation (repeats ABOVE the conversion), never on the by-design
/// conversion. Resyncs were 0 everywhere in the calibration, so they stay a
/// hard event below rather than a rate.
pub const REPEAT_MARGIN_PERMILLE: u64 = 100;

/// The structural repeat permille for `source_fps` frames delivered onto a
/// `grid_fps` grid: `1000 − (source/grid × 1000)`, saturating at 0 once the
/// source meets or exceeds the grid. Each grid slot with no fresh source frame
/// repeats the last one, so the fraction of repeated slots is `1 − source/grid`.
/// The one `source_fps` fold to permille happens here, so [`derive`]'s
/// comparison stays pure integer.
///
/// Examples (22.9.2026 grid): `24 → 200`, `25 → 167`, `30 → 0`, `60 → 0`.
pub fn expected_repeat_permille(source_fps: f32, grid_fps: u32) -> u64 {
    if grid_fps == 0 || source_fps <= 0.0 {
        return 0;
    }
    // Fold source_fps to permille once, then integer-only: 1000 − source/grid,
    // saturating so a source at or above the grid clamps to 0 (no underflow).
    let source_permille = (source_fps * 1000.0) as u64;
    1000u64.saturating_sub(source_permille / grid_fps as u64)
}

/// Derive the lock state + a static reason from the clock/pacing/receiver inputs
/// and the rate-normalised 60 s window counts (contract §7 A7.3, calibrated
/// #168 round 6 / #149). Checks in strict precedence — the FIRST match wins:
///
/// 1. `!clock_ok`                           → `(Unlocked, "clock not ok")`
/// 2. `!pacing_enabled`                     → `(Unlocked, "pacing disabled")`
/// 3. `connections == 0`                    → `(Degraded, "no receiver")`
/// 4. `resyncs_w > 0`                       → `(Degraded, "resync in 60 s")`
/// 5. `slots_w == 0`                        → `(Locked,   "locked")`
/// 6. `late_w > 25 %` of slots             → `(Degraded, "late > 25 % of slots in 60 s")`
/// 7. `repeats_w > fps-conversion + 10 %`  → `(Degraded, "repeats above the fps conversion in 60 s")`
/// 8. otherwise                             → `(Locked,   "locked")`
///
/// So clock beats pacing beats receiver beats resync; a paused/idle output
/// (`slots_w == 0`, nothing emitted — no grid to break) reads LOCKED; then the
/// two rate-normalised checks. Resyncs stay a hard event (0 in the calibration).
pub fn derive(inputs: &LockInputs) -> (LockState, &'static str) {
    if !inputs.clock_ok {
        return (LockState::Unlocked, "clock not ok");
    }
    if !inputs.pacing_enabled {
        return (LockState::Unlocked, "pacing disabled");
    }
    if inputs.connections == 0 {
        return (LockState::Degraded, "no receiver");
    }
    if inputs.resyncs_w > 0 {
        return (LockState::Degraded, "resync in 60 s");
    }
    if inputs.slots_w == 0 {
        return (LockState::Locked, "locked");
    }
    if inputs.late_w * 1000 > LATE_DEGRADED_PERMILLE * inputs.slots_w {
        return (LockState::Degraded, "late > 25 % of slots in 60 s");
    }
    let expected = expected_repeat_permille(inputs.source_fps, inputs.grid_fps);
    if inputs.repeats_w * 1000 > (expected + REPEAT_MARGIN_PERMILLE) * inputs.slots_w {
        return (
            LockState::Degraded,
            "repeats above the fps conversion in 60 s",
        );
    }
    (LockState::Locked, "locked")
}

/// One output's lock state as fed to [`summarize`] (#150): its NDI name, the
/// derived [`LockState`], whether the output is LIVE on the wall
/// (`state == Playing`), and whether the box clock is ok. WASM-safe and pure.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputLock {
    pub name: String,
    pub state: LockState,
    pub live: bool,
    pub clock_ok: bool,
}

/// The reduced whole-box genlock lock, rendered as the dashboard's global
/// badge (#150): the summarised [`LockState`], the worst output's name (when
/// the box is not fully LOCKED), and how many outputs are live.
#[derive(Clone, Debug, PartialEq)]
pub struct LockSummary {
    pub state: LockState,
    pub worst: Option<String>,
    pub live_count: usize,
}

/// Rank a state for "which output is worst" — UNLOCKED beats DEGRADED beats
/// LOCKED (higher = worse).
fn severity(state: LockState) -> u8 {
    match state {
        LockState::Locked => 0,
        LockState::Degraded => 1,
        LockState::Unlocked => 2,
    }
}

/// The output's effective state for the summary: a LOCKED output whose clock
/// is not ok is demoted to UNLOCKED. Defensive — the engine never emits LOCKED
/// with `clock_ok == false` (see [`derive`], which returns UNLOCKED first) —
/// but the summary must never report LOCKED off a clock-broken output.
fn effective_state(o: &OutputLock) -> LockState {
    if o.state == LockState::Locked && !o.clock_ok {
        LockState::Unlocked
    } else {
        o.state
    }
}

/// Reduce the per-output lock states into ONE whole-box summary for the
/// dashboard header badge (#150), sharing the LOCKED / DEGRADED / UNLOCKED
/// vocabulary of [`LockState`] and [`derive`].
///
/// Rules (contract §7, camera-box#1298):
/// - **LOCKED** iff every LIVE output is LOCKED and the box clock is ok
///   (`worst = None`).
/// - Otherwise the **worst live state** (UNLOCKED > DEGRADED > LOCKED),
///   naming the worst output in `worst`.
/// - **No live output** → the state comes from the clock only: LOCKED (with
///   `worst = None`, rendered "no live output") when every output reports
///   `clock_ok`, else UNLOCKED. An empty slice is treated as clock-ok
///   (vacuously LOCKED / no live output).
pub fn summarize(outputs: &[OutputLock]) -> LockSummary {
    let live_count = outputs.iter().filter(|o| o.live).count();

    if live_count == 0 {
        let clock_ok = outputs.iter().all(|o| o.clock_ok);
        return LockSummary {
            state: if clock_ok {
                LockState::Locked
            } else {
                LockState::Unlocked
            },
            worst: None,
            live_count: 0,
        };
    }

    // The worst live output by effective severity. `max_by_key` keeps the last
    // maximum on ties, which is fine — any worst-tied output is a valid name.
    let worst = outputs
        .iter()
        .filter(|o| o.live)
        .max_by_key(|o| severity(effective_state(o)))
        .expect("live_count > 0 guarantees a live output");

    let worst_state = effective_state(worst);
    if worst_state == LockState::Locked {
        LockSummary {
            state: LockState::Locked,
            worst: None,
            live_count,
        }
    } else {
        LockSummary {
            state: worst_state,
            worst: Some(worst.name.clone()),
            live_count,
        }
    }
}

// ── #176: always-visible whole-box GLOBAL genlock indicator, incl. OFF ────────

/// The dashboard's always-visible global genlock state (#176). Adds a fourth
/// state `Off` to the LOCKED / DEGRADED / UNLOCKED lock vocabulary: `Off` means
/// NO output has boundary pacing enabled (`genlock_pacing=false`, the production
/// default #147) — the box free-runs on the NDI SDK clock, a deliberate
/// configuration, not a fault. Rendered grey; the other three keep their
/// green / amber / red colours (camera-box#1298). This is the #176 revision of
/// #164's "hide the badge entirely while pacing is off".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobalLock {
    Off,
    Locked,
    Degraded,
    Unlocked,
}

/// One output's inputs to [`global_lock_summary`] (#176): its pacing-enabled
/// flag (`Off` is derived from this across ALL outputs), whether it is LIVE on
/// the wall, its derived [`LockState`], its box-clock-ok flag, and its reason
/// string. WASM-safe and pure — the sp-ui `GlobalLockBadge` builds these from
/// the `/api/v1/ndi/health` snapshot and renders the returned state (sp-ui has
/// no unit-test job, so the logic + its tests live here).
#[derive(Clone, Debug, PartialEq)]
pub struct GlobalLockInput {
    pub pacing_enabled: bool,
    pub live: bool,
    pub state: LockState,
    pub clock_ok: bool,
    pub reason: String,
}

/// The whole-box global genlock summary (#176): the [`GlobalLock`] state plus a
/// short reason string for the badge tooltip.
#[derive(Clone, Debug, PartialEq)]
pub struct GlobalLockSummary {
    pub state: GlobalLock,
    pub reason: String,
}

/// This output's effective [`LockState`] for the global summary: a LOCKED output
/// whose box clock is not ok is demoted to UNLOCKED (mirrors [`summarize`] /
/// [`effective_state`]).
fn effective_global(o: &GlobalLockInput) -> LockState {
    if o.state == LockState::Locked && !o.clock_ok {
        LockState::Unlocked
    } else {
        o.state
    }
}

/// The reason shown in the `● GENLOCK OFF` tooltip: the box free-runs on the
/// NDI SDK clock because boundary pacing is off (`genlock_pacing=false`).
const OFF_REASON: &str = "pacing vypnuté";

/// Reduce the per-output genlock inputs into ONE whole-box GLOBAL state for the
/// always-visible dashboard badge (#176):
///
/// - **No output has pacing enabled** → [`GlobalLock::Off`] (reason
///   `"pacing vypnuté"`) — the production default; grey, never hidden.
/// - **Pacing-enabled LIVE outputs** → the worst effective state over them
///   (UNLOCKED > DEGRADED > LOCKED); a LOCKED-but-clock-not-ok output is demoted
///   to UNLOCKED. The reason is the worst live output's reason (or `"locked"`).
/// - **Pacing enabled but nothing live** → derived from the enabled set's clock
///   only: LOCKED iff every enabled output's clock is ok, else UNLOCKED
///   (mirrors [`summarize`]'s no-live branch).
pub fn global_lock_summary(outputs: &[GlobalLockInput]) -> GlobalLockSummary {
    let enabled: Vec<&GlobalLockInput> = outputs.iter().filter(|o| o.pacing_enabled).collect();
    if enabled.is_empty() {
        return GlobalLockSummary {
            state: GlobalLock::Off,
            reason: OFF_REASON.to_string(),
        };
    }

    let live: Vec<&GlobalLockInput> = enabled.iter().copied().filter(|o| o.live).collect();
    if live.is_empty() {
        let clock_ok = enabled.iter().all(|o| o.clock_ok);
        return if clock_ok {
            GlobalLockSummary {
                state: GlobalLock::Locked,
                reason: "no live output".to_string(),
            }
        } else {
            GlobalLockSummary {
                state: GlobalLock::Unlocked,
                reason: "clock not ok".to_string(),
            }
        };
    }

    let worst = live
        .iter()
        .copied()
        .max_by_key(|o| severity(effective_global(o)))
        .expect("live is non-empty");
    let (state, reason) = match effective_global(worst) {
        LockState::Locked => (GlobalLock::Locked, "locked".to_string()),
        LockState::Degraded => (GlobalLock::Degraded, worst.reason.clone()),
        LockState::Unlocked => (GlobalLock::Unlocked, worst.reason.clone()),
    };
    GlobalLockSummary { state, reason }
}
