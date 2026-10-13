//! #259: what the wall report reads, and the walk that reads it over a
//! [`WallReader`] (the Windows glue `win.rs`, or a test's fake). The walk
//! finds Arena's process and windows, reads Arena's saved output setup
//! (`resolume.rs`) and Windows' displays, and asks NVAPI only when the
//! decision needs it (`decide::needs_nvidia`: a display Arena wants is gone
//! or has another size), so a healthy wall never calls the driver. It
//! decides nothing itself (`decide.rs` does).

use std::path::{Path, PathBuf};

use sp_core::wall::{ArenaOutput, ArenaWindow, Connected, Display, Grid};

use super::decide;
use super::nvapi::{self, NvGpuDisplayIds, NvMosaicGridTopoV2};
use super::resolume;

/// Resolume Arena's process image (its file name, any case).
pub const ARENA_IMAGE: &str = "Arena.exe";

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
    /// Every process: its id and its image's file name.
    fn processes(&self) -> Result<Vec<(u32, String)>, String>;
    /// The top-level windows of the processes `pids`.
    fn windows_of(&self, pids: &[u32]) -> Result<Vec<ArenaWindow>, String>;
    /// The Documents folder Arena keeps its setup in.
    fn documents(&self) -> Result<PathBuf, String>;
    /// A file's text, `None` when it does not exist.
    fn file(&self, path: &Path) -> Result<Option<String>, String>;
}

/// Reads every fact through `reader`; NVAPI only when the decision needs
/// it.
pub fn read_facts(reader: &dyn WallReader) -> WallFacts {
    let mut facts = WallFacts {
        arena: arena(reader),
        arena_output: arena_output(reader),
        displays: reader.displays(),
        nvidia: NvidiaFacts::NotRead,
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
    let pids: Vec<u32> = reader
        .processes()?
        .into_iter()
        .filter(|(_, image)| image.eq_ignore_ascii_case(ARENA_IMAGE))
        .map(|(pid, _)| pid)
        .collect();
    if pids.is_empty() {
        return Ok(Arena::default());
    }
    Ok(Arena {
        running: true,
        windows: reader.windows_of(&pids)?,
    })
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
