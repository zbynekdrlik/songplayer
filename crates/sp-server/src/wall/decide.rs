//! #259: the wall report's decision — the facts read ([`WallFacts`]) → the
//! wall's state and what keeps it from showing Arena ([`decide`]), plus what
//! the status shows of the facts. Pure, Linux-tested.
//!
//! The owner's rule: the only reference is Arena's OWN saved output setup
//! (the displays its advanced output wants); SongPlayer keeps no layout,
//! grid or port order of its own, and nothing here acts on the box.
//!
//! The order, each step reading only the facts it needs (a fact a step
//! needs that could not be read is `unknown`; only SongPlayer's own reads
//! failing are, never the operator's setup):
//!
//! 1. no process `Arena.exe` → `arena_off`;
//! 2. Arena's simple output → `simple_output`; an advanced output with no
//!    preset saved (no `AdvancedOutput.xml`, no `presetFile`, no such
//!    preset file) → `no_preset`; a preset that drives no physical display
//!    → `no_display_output`;
//! 3. each display Arena wants, matched to a Windows display by its device
//!    key (Arena's `deviceId`, ASCII case ignored). NVAPI is read only now,
//!    and only when one is gone or has another size ([`needs_nvidia`]):
//!    - none: no NVIDIA driver, or NVAPI reports no screen connected →
//!      screens off; NVIDIA screens connected → missing (`not_joined`
//!      when NVAPI's grids were read and none holds more than one screen);
//!    - another size → missing (`not_joined` likewise);
//!
//!    any missing → `display_missing` (the missing displays listed first),
//!    else any off → `screens_off`;
//! 4. a fullscreen display Arena wants that no visible Arena window titled
//!    exactly `Display` covers → `arena_not_drawing`;
//! 5. else `ok`.

use sp_core::wall::{
    ArenaWindow, Display, NvidiaRead, WallFact, WallProblem, WallRead, WallState, WantedDisplay,
};

use super::guard::GuardFacts;
use super::read::{NvidiaFacts, WallFacts};

/// The title of Arena's output window (an Advanced Output screen's window;
/// observed on SNV: `Display`, over the span).
pub const ARENA_OUTPUT_TITLE: &str = "Display";

/// The check's verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub state: WallState,
    pub problems: Vec<WallProblem>,
}

/// A rectangle on the desktop, physical pixels (right and bottom exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// A state with nothing to name.
fn calm(state: WallState) -> Verdict {
    Verdict {
        state,
        problems: Vec::new(),
    }
}

/// A fact the next step needs could not be read.
fn unknown(fact: WallFact) -> Verdict {
    Verdict {
        state: WallState::Unknown,
        problems: vec![WallProblem::Unreadable(fact)],
    }
}

/// The Windows display Arena saved as `wanted` (its device key).
pub fn find<'a>(wanted: &WantedDisplay, displays: &'a [Display]) -> Option<&'a Display> {
    displays
        .iter()
        .find(|d| d.device_key.eq_ignore_ascii_case(&wanted.device_id))
}

/// `wanted` is there with its size.
pub fn fits(wanted: &WantedDisplay, displays: &[Display]) -> bool {
    find(wanted, displays).is_some_and(|d| d.width == wanted.width && d.height == wanted.height)
}

/// The decision needs NVAPI: Arena runs an advanced output whose preset
/// wants a display that Windows' displays (read) do not show with its size.
/// Otherwise the driver is not asked at all.
pub fn needs_nvidia(facts: &WallFacts) -> bool {
    let (Ok(arena), Ok(output), Ok(displays)) =
        (&facts.arena, &facts.arena_output, &facts.displays)
    else {
        return false;
    };
    arena.running && output.advanced && output.displays.iter().any(|wanted| !fits(wanted, displays))
}

/// NVAPI shows screens connected but in no grid of more than one screen
/// (the Surround span is dissolved). Told only when both reads answered.
pub fn not_joined(nvidia: &NvidiaFacts) -> bool {
    match nvidia {
        NvidiaFacts::Read {
            connected: Ok(connected),
            grids: Ok(grids),
        } => !connected.is_empty() && !grids.iter().any(|g| g.displays.len() > 1),
        _ => false,
    }
}

/// The verdict on `facts`.
pub fn decide(facts: &WallFacts) -> Verdict {
    let Ok(arena) = &facts.arena else {
        return unknown(WallFact::Arena);
    };
    if !arena.running {
        return calm(WallState::ArenaOff);
    }
    let Ok(output) = &facts.arena_output else {
        return unknown(WallFact::ArenaOutput);
    };
    if !output.advanced {
        return calm(WallState::SimpleOutput);
    }
    if !output.preset_found {
        return calm(WallState::NoPreset);
    }
    if output.displays.is_empty() {
        return calm(WallState::NoDisplayOutput);
    }
    let Ok(displays) = &facts.displays else {
        return unknown(WallFact::Displays);
    };
    let mut missing = Vec::new();
    let mut off = Vec::new();
    for wanted in &output.displays {
        let (width, height) = (wanted.width, wanted.height);
        match find(wanted, displays) {
            None => {
                let connected = match &facts.nvidia {
                    NvidiaFacts::NoDriver => false,
                    NvidiaFacts::Read {
                        connected: Ok(connected),
                        ..
                    } => !connected.is_empty(),
                    NvidiaFacts::NotRead
                    | NvidiaFacts::Read {
                        connected: Err(_), ..
                    } => {
                        return unknown(WallFact::Nvapi);
                    }
                };
                if connected {
                    missing.push(WallProblem::DisplayAbsent {
                        name: wanted.name.clone(),
                        width,
                        height,
                        not_joined: not_joined(&facts.nvidia),
                    });
                } else {
                    off.push(WallProblem::ScreensOff {
                        name: wanted.name.clone(),
                        width,
                        height,
                    });
                }
            }
            Some(display) if display.width != width || display.height != height => {
                missing.push(WallProblem::DisplaySize {
                    name: wanted.name.clone(),
                    width,
                    height,
                    actual_width: display.width,
                    actual_height: display.height,
                    not_joined: not_joined(&facts.nvidia),
                });
            }
            Some(_) => {}
        }
    }
    if !missing.is_empty() {
        missing.extend(off);
        return Verdict {
            state: WallState::DisplayMissing,
            problems: missing,
        };
    }
    if !off.is_empty() {
        return Verdict {
            state: WallState::ScreensOff,
            problems: off,
        };
    }
    let not_drawn: Vec<WallProblem> = output
        .displays
        .iter()
        .filter(|wanted| wanted.fullscreen)
        .filter_map(|wanted| find(wanted, displays).map(|d| (wanted, rect(d))))
        .filter(|(_, r)| !arena.windows.iter().any(|w| draws_on(w, *r)))
        .map(|(wanted, r)| WallProblem::ArenaNotDrawing {
            name: wanted.name.clone(),
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        })
        .collect();
    if !not_drawn.is_empty() {
        return Verdict {
            state: WallState::ArenaNotDrawing,
            problems: not_drawn,
        };
    }
    calm(WallState::Ok)
}

/// A display's rectangle on the desktop.
pub fn rect(display: &Display) -> Rect {
    Rect {
        left: display.x,
        top: display.y,
        right: display.x.saturating_add_unsigned(display.width),
        bottom: display.y.saturating_add_unsigned(display.height),
    }
}

/// Arena draws on `area` with `window`: a visible window titled exactly
/// [`ARENA_OUTPUT_TITLE`] that covers all of it.
pub fn draws_on(window: &ArenaWindow, area: Rect) -> bool {
    window.visible
        && window.title == ARENA_OUTPUT_TITLE
        && window.left <= area.left
        && window.top <= area.top
        && window.right >= area.right
        && window.bottom >= area.bottom
}

/// What the status shows of the facts: each one read, `None` for one that
/// could not be (its reason in [`read_errors`]); NVAPI `not_read` when the
/// decision did not need it.
pub fn read_view(facts: &WallFacts) -> WallRead {
    let arena = facts.arena.as_ref().ok();
    let (nvidia, connected, grids) = match &facts.nvidia {
        NvidiaFacts::NotRead => (NvidiaRead::NotRead, None, None),
        NvidiaFacts::NoDriver => (NvidiaRead::NoDriver, None, None),
        NvidiaFacts::Read { connected, grids } => (
            NvidiaRead::Read,
            connected.as_ref().ok().cloned(),
            grids.as_ref().ok().cloned(),
        ),
    };
    WallRead {
        arena_running: arena.map(|a| a.running),
        arena_windows: arena.map(|a| a.windows.clone()),
        arena_output: facts.arena_output.as_ref().ok().cloned(),
        displays: facts.displays.as_ref().ok().cloned(),
        nvidia,
        connected,
        grids,
    }
}

/// Why facts could not be read, each reason once, in the facts' order
/// (NVAPI's connected displays, then its grids; #254: then the guard's).
pub fn read_errors(facts: &WallFacts) -> Vec<String> {
    let (connected, grids) = match &facts.nvidia {
        NvidiaFacts::Read { connected, grids } => (connected.as_ref().err(), grids.as_ref().err()),
        NvidiaFacts::NotRead | NvidiaFacts::NoDriver => (None, None),
    };
    let reasons = [
        facts.arena.as_ref().err(),
        facts.arena_output.as_ref().err(),
        facts.displays.as_ref().err(),
        connected,
        grids,
    ];
    let mut errors: Vec<String> = Vec::new();
    for why in reasons
        .into_iter()
        .flatten()
        .chain(guard_errors(&facts.guard))
    {
        if !errors.contains(why) {
            errors.push(why.clone());
        }
    }
    errors
}

/// #254: why the guard could not read: the process list, else each Arena's
/// times, hang and crash reporter.
fn guard_errors(guard: &GuardFacts) -> Vec<&String> {
    match guard {
        Err(why) => vec![why],
        Ok(arenas) => arenas
            .iter()
            .flat_map(|a| {
                [
                    a.times.as_ref().err(),
                    a.hung.as_ref().err(),
                    a.crash_report.as_ref().err(),
                ]
            })
            .flatten()
            .collect(),
    }
}

#[cfg(test)]
#[path = "decide_tests.rs"]
mod tests;
