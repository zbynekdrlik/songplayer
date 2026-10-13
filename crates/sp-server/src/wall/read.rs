//! #259: what the wall report reads, and the walk that reads it over a
//! [`WallReader`] (the Windows glue `win.rs`, or a test's fake). The walk
//! decodes NVAPI's structures, finds Arena's process and windows, and reads
//! Arena's saved output setup (`resolume.rs`); it decides nothing
//! (`decide.rs` does).

use std::path::{Path, PathBuf};

use sp_core::wall::{ArenaOutput, ArenaWindow, Connected, Display, Grid};

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

/// What NVAPI says: the connected displays and the Mosaic (Surround) grids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Nvidia {
    pub connected: Vec<Connected>,
    pub grids: Vec<Grid>,
}

/// NVAPI's structures as the driver filled them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NvidiaRaw {
    pub connected: Vec<NvGpuDisplayIds>,
    pub grids: Vec<NvMosaicGridTopoV2>,
}

/// Every fact one check read, each with why it could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WallFacts {
    pub arena: Result<Arena, String>,
    /// Arena's saved output setup (`resolume.rs`).
    pub arena_output: Result<ArenaOutput, String>,
    /// Windows' displays attached to the desktop.
    pub displays: Result<Vec<Display>, String>,
    /// NVAPI; `None` = no NVIDIA driver on the box.
    pub nvidia: Result<Option<Nvidia>, String>,
}

impl WallFacts {
    /// Nothing could be read, for `why` (a read past its bound, not Windows).
    pub fn unread(why: &str) -> Self {
        Self {
            arena: Err(why.to_string()),
            arena_output: Err(why.to_string()),
            displays: Err(why.to_string()),
            nvidia: Err(why.to_string()),
        }
    }
}

/// The box's raw reads. Each answers why it failed; none changes anything.
pub trait WallReader {
    /// NVAPI: every display each NVIDIA GPU reports connected, and the
    /// Mosaic grids; `None` when the NVIDIA driver is not installed.
    fn nvidia(&self) -> Result<Option<NvidiaRaw>, String>;
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

/// Reads every fact through `reader`.
pub fn read_facts(reader: &dyn WallReader) -> WallFacts {
    WallFacts {
        arena: arena(reader),
        arena_output: arena_output(reader),
        displays: reader.displays(),
        nvidia: reader.nvidia().map(|raw| raw.map(|raw| nvidia(&raw))),
    }
}

/// What the check keeps of NVAPI's structures.
pub fn nvidia(raw: &NvidiaRaw) -> Nvidia {
    Nvidia {
        connected: raw.connected.iter().map(nvapi::connected).collect(),
        grids: raw.grids.iter().map(nvapi::grid).collect(),
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

/// A file Arena must have: its text, or why not (missing, unreadable).
fn required(reader: &dyn WallReader, path: &Path) -> Result<String, String> {
    reader
        .file(path)?
        .ok_or_else(|| format!("{} does not exist", path.display()))
}

/// Arena's saved output setup: the simple output (no `SimpleOutput.xml`,
/// or its advanced output off), else the preset its advanced output was
/// saved as and the physical displays that preset drives. A parse error
/// names its file.
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
    let setup = resolume::advanced_output_file(&documents);
    let preset =
        resolume::preset_name(&required(reader, &setup)?).map_err(in_file(setup.as_path()))?;
    let file = resolume::preset_file(&documents, &preset);
    let displays =
        resolume::wanted_displays(&required(reader, &file)?).map_err(in_file(file.as_path()))?;
    Ok(ArenaOutput {
        advanced: true,
        preset: Some(preset),
        displays,
    })
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
