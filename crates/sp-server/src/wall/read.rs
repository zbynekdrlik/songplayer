//! #259: what the wall report reads, and the walk that reads it over a
//! [`WallReader`] (the Windows glue `win.rs`, or a test's fake). The walk
//! finds Arena's process and windows, reads Arena's saved output setup
//! (`resolume.rs`) and Windows' displays, and asks NVAPI only when the
//! decision needs it (`decide::needs_nvidia`: a display Arena wants is gone
//! or has another size), so a healthy wall never calls the driver. It
//! decides nothing itself (`decide.rs` does).
//!
//! #254: the same walk reads what the guard needs (`guard.rs`): each Arena
//! process's start and CPU time, whether a visible window of it does not
//! answer, and whether its own crash reporter (a `BugSplatMonitor.exe` it
//! started) shows its "Crash Report" window.

use std::path::{Path, PathBuf};

use sp_core::wall::{ArenaOutput, ArenaWindow, Connected, Display, Grid};

use super::decide;
use super::guard::{
    ArenaProcess, CRASH_MONITOR_IMAGE, CRASH_REPORT_TITLE, GuardFacts, ProcessKey, ProcessTimes,
};
use super::nvapi::{self, NvGpuDisplayIds, NvMosaicGridTopoV2};
use super::resolume;

/// Resolume Arena's process image (its file name, any case).
pub const ARENA_IMAGE: &str = "Arena.exe";

/// One process of the list: its id, its parent's id and its image's file
/// name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessEntry {
    pub pid: u32,
    pub parent: u32,
    pub image: String,
}

/// Resolume Arena: whether its process runs, and its top-level windows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Arena {
    pub running: bool,
    pub windows: Vec<ArenaWindow>,
}

/// What NVAPI said. Its two reads are independent: a failed grid read never
/// discards the connected displays.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum NvidiaFacts {
    /// Not needed: every display Arena wants is there with its size.
    #[default]
    NotRead,
    /// No NVIDIA driver on the box.
    NoDriver,
    /// Asked: the connected displays and the Mosaic (Surround) grids, each
    /// with why it could not be read.
    Read {
        connected: Result<Vec<Connected>, String>,
        grids: Result<Vec<Grid>, String>,
    },
}

/// NVAPI's structures as the driver filled them, each read on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NvidiaRaw {
    pub connected: Result<Vec<NvGpuDisplayIds>, String>,
    pub grids: Result<Vec<NvMosaicGridTopoV2>, String>,
}

/// Every fact one check read, each with why it could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WallFacts {
    pub arena: Result<Arena, String>,
    /// Arena's saved output setup (`resolume.rs`).
    pub arena_output: Result<ArenaOutput, String>,
    /// Windows' displays attached to the desktop.
    pub displays: Result<Vec<Display>, String>,
    pub nvidia: NvidiaFacts,
    /// #254: what the guard reads of each Arena process.
    pub guard: GuardFacts,
}

impl WallFacts {
    /// Nothing could be read, for `why` (a read past its bound, not
    /// Windows); NVAPI was not reached.
    pub fn unread(why: &str) -> Self {
        Self {
            arena: Err(why.to_string()),
            arena_output: Err(why.to_string()),
            displays: Err(why.to_string()),
            nvidia: NvidiaFacts::NotRead,
            guard: Err(why.to_string()),
        }
    }
}

/// The box's raw reads. Each answers why it failed; none changes anything.
pub trait WallReader {
    /// NVAPI: every display each NVIDIA GPU reports connected, and the
    /// Mosaic grids, each read on its own; `None` when the NVIDIA driver is
    /// not installed.
    fn nvidia(&self) -> Option<NvidiaRaw>;
    /// Windows' displays attached to the desktop, with their current mode.
    fn displays(&self) -> Result<Vec<Display>, String>;
    /// Every process: its id, its parent's id and its image's file name.
    fn processes(&self) -> Result<Vec<ProcessEntry>, String>;
    /// The top-level windows of the processes `pids`.
    fn windows_of(&self, pids: &[u32]) -> Result<Vec<ArenaWindow>, String>;
    /// The Documents folder Arena keeps its setup in.
    fn documents(&self) -> Result<PathBuf, String>;
    /// A file's text, `None` when it does not exist.
    fn file(&self, path: &Path) -> Result<Option<String>, String>;
    /// #254: process `pid`'s start and CPU time.
    fn times(&self, pid: u32) -> Result<ProcessTimes, String>;
    /// #254: a visible top-level window of process `pid` does not answer
    /// (`IsHungAppWindow`).
    fn hung(&self, pid: u32) -> Result<bool, String>;
}

/// Reads every fact through `reader`; NVAPI only when the decision needs
/// it.
pub fn read_facts(reader: &dyn WallReader) -> WallFacts {
    let processes = reader.processes();
    let mut facts = WallFacts {
        arena: arena_of(reader, &processes),
        arena_output: arena_output(reader),
        displays: reader.displays(),
        nvidia: NvidiaFacts::NotRead,
        guard: processes.map(|list| guard_facts(reader, &list)),
    };
    if decide::needs_nvidia(&facts) {
        facts.nvidia = nvidia(reader);
    }
    facts
}

/// NVAPI through `reader`: no driver, or each read decoded on its own.
pub fn nvidia(reader: &dyn WallReader) -> NvidiaFacts {
    match reader.nvidia() {
        None => NvidiaFacts::NoDriver,
        Some(raw) => NvidiaFacts::Read {
            connected: raw
                .connected
                .map(|ids| ids.iter().map(nvapi::connected).collect()),
            grids: raw
                .grids
                .map(|grids| grids.iter().map(nvapi::grid).collect()),
        },
    }
}

/// Arena: running when a process's image is [`ARENA_IMAGE`], then the
/// windows of every such process (none read while it does not run).
pub fn arena(reader: &dyn WallReader) -> Result<Arena, String> {
    arena_of(reader, &reader.processes())
}

/// The process ids whose image is `image` (ASCII case ignored).
fn pids_of(processes: &[ProcessEntry], image: &str) -> Vec<u32> {
    processes
        .iter()
        .filter(|p| p.image.eq_ignore_ascii_case(image))
        .map(|p| p.pid)
        .collect()
}

/// [`arena`] over a process list already read.
fn arena_of(
    reader: &dyn WallReader,
    processes: &Result<Vec<ProcessEntry>, String>,
) -> Result<Arena, String> {
    let pids = pids_of(processes.as_ref().map_err(Clone::clone)?, ARENA_IMAGE);
    if pids.is_empty() {
        return Ok(Arena::default());
    }
    // #259: only the visible windows (no decision reads a hidden one, and
    // Arena owns ~30 hidden helpers).
    let windows = reader.windows_of(&pids)?;
    Ok(Arena {
        running: true,
        windows: windows.into_iter().filter(|w| w.visible).collect(),
    })
}

/// #254: what the guard needs of each Arena process in `processes`: its
/// times, whether a window of it hangs, its crash reporter's processes
/// ([`reporters_of`]; none while Arena's own start cannot be read) and
/// whether one of them shows a visible window titled exactly
/// [`CRASH_REPORT_TITLE`].
pub fn guard_facts(reader: &dyn WallReader, processes: &[ProcessEntry]) -> Vec<ArenaProcess> {
    pids_of(processes, ARENA_IMAGE)
        .into_iter()
        .map(|pid| {
            let times = reader.times(pid);
            let monitors = match &times {
                Ok(arena) => reporters_of(reader, processes, pid, arena.created_100ns),
                Err(_) => Vec::new(),
            };
            let crash_report = if monitors.is_empty() {
                Ok(false)
            } else {
                let pids: Vec<u32> = monitors.iter().map(|m| m.pid).collect();
                reader.windows_of(&pids).map(|windows| {
                    windows
                        .iter()
                        .any(|w| w.visible && w.title == CRASH_REPORT_TITLE)
                })
            };
            ArenaProcess {
                pid,
                times,
                hung: reader.hung(pid),
                monitors,
                crash_report,
            }
        })
        .collect()
}

/// #254: Arena `pid`'s crash reporters: each `BugSplatMonitor.exe` whose
/// parent id is `pid` AND that started no earlier than Arena
/// (`arena_created_100ns`). A parent id is never updated and outlives its
/// parent, and ids are reused: a reporter another program left up (PP's
/// Arena-Bridge.exe uses BugSplat too) whose id Arena now has started
/// before Arena, and is never Arena's. A reporter whose start cannot be
/// read is not counted.
fn reporters_of(
    reader: &dyn WallReader,
    processes: &[ProcessEntry],
    pid: u32,
    arena_created_100ns: u64,
) -> Vec<ProcessKey> {
    processes
        .iter()
        .filter(|p| p.parent == pid && p.image.eq_ignore_ascii_case(CRASH_MONITOR_IMAGE))
        .filter_map(|p| {
            let times = reader.times(p.pid).ok()?;
            Some(ProcessKey {
                pid: p.pid,
                created_100ns: times.created_100ns,
            })
        })
        .filter(|m| m.created_100ns >= arena_created_100ns)
        .collect()
}

/// Arena's saved output setup, as the operator left it:
///
/// - the simple output: no `SimpleOutput.xml`, or its advanced output off;
/// - the advanced output with no preset (`preset_found` false): no
///   `AdvancedOutput.xml`, no or an empty `presetFile`, a name that is no
///   plain file name, or no such preset file — the operator's setup, never
///   a failed read;
/// - else the preset and the physical displays it drives.
///
/// Only a read that fails (an existing file that cannot be read, or is no
/// XML Arena writes) is an error; it names its file.
pub fn arena_output(reader: &dyn WallReader) -> Result<ArenaOutput, String> {
    let documents = reader.documents()?;
    let simple = resolume::simple_output_file(&documents);
    let in_file = |path: &Path| {
        let path = path.display().to_string();
        move |why: String| format!("{path}: {why}")
    };
    let advanced = match reader.file(&simple)? {
        None => false,
        Some(xml) => resolume::advanced_mode(&xml).map_err(in_file(simple.as_path()))?,
    };
    if !advanced {
        return Ok(ArenaOutput::default());
    }
    let no_preset = |preset: Option<String>| ArenaOutput {
        advanced: true,
        preset,
        ..ArenaOutput::default()
    };
    let setup = resolume::advanced_output_file(&documents);
    let Some(xml) = reader.file(&setup)? else {
        return Ok(no_preset(None));
    };
    let Some(preset) = resolume::preset_name(&xml).map_err(in_file(setup.as_path()))? else {
        return Ok(no_preset(None));
    };
    if !resolume::is_file_name(&preset) {
        return Ok(no_preset(Some(preset)));
    }
    let file = resolume::preset_file(&documents, &preset);
    let Some(xml) = reader.file(&file)? else {
        return Ok(no_preset(Some(preset)));
    };
    let displays = resolume::wanted_displays(&xml).map_err(in_file(file.as_path()))?;
    Ok(ArenaOutput {
        advanced: true,
        preset: Some(preset),
        preset_found: true,
        displays,
    })
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
