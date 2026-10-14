//! #259: a scripted [`WallReader`] for the tests, and SNV's box as observed
//! on 13.10.2026 (an observation of one moment, never a target: the
//! operator may set the wall up otherwise at any time):
//!
//! - Arena's advanced output on, preset `snv surround 7680`, one
//!   fullscreen `Display 1` 7680×1080 on the span's device key, plus a
//!   Spout screen;
//! - Windows: the VDD primary 3840×2160 at 0,0, the span `\\.\DISPLAY1`
//!   7680×1080 at x 3840;
//! - NVAPI (read only when a display Arena wants is gone or has another
//!   size, so never on this healthy box): three screens connected
//!   (`0x80061083` HDMI, `0x80061086` and `0x80061087` DisplayPort) joined
//!   into one 1×3 grid of 2560×1080;
//! - Arena's output window `Display` at 3840,0 – 11520,1080;
//! - #254: Arena answers, its CPU time read as 1098.7 s (the hung Arena of
//!   14.10.2026 stood still at that), no crash reporter running.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use sp_core::wall::{ArenaOutput, ArenaWindow, Connected, Display, DisplayId, Grid, WantedDisplay};

use super::guard::{ArenaProcess, ProcessTimes};
use super::nvapi::{NvGpuDisplayIds, NvMosaicGridTopoV2};
use super::read::{Arena, NvidiaFacts, NvidiaRaw, ProcessEntry, WallFacts, WallReader};
use super::resolume;

/// The span's device key (Arena's `deviceId` of Display 1).
pub const SPAN_KEY: &str = r"\Registry\Machine\System\CurrentControlSet\Control\Video\{49D71F75-AF5A-11EE-BACA-806E6F6E6963}\0000";

/// The VDD's device key.
pub const VDD_KEY: &str = r"\Registry\Machine\System\CurrentControlSet\Control\Video\{0A1B2C3D-4E5F-11EE-8000-806E6F6E6963}\0000";

/// The screens NVAPI reports.
pub const LEFT: DisplayId = DisplayId(0x8006_1086);
pub const MIDDLE: DisplayId = DisplayId(0x8006_1087);
pub const RIGHT: DisplayId = DisplayId(0x8006_1083);

/// Arena's process id in the fake.
pub const ARENA_PID: u32 = 5120;

/// #254: Arena's crash reporter's process id in the fake.
pub const MONITOR_PID: u32 = 9188;

/// #254: Arena's times: started 14.10.2026 08:05 local, 1098.7 s of CPU.
pub fn snv_times() -> ProcessTimes {
    ProcessTimes {
        created_100ns: 134_049_123_000_000_000,
        cpu_100ns: 10_987_000_000,
    }
}

/// #254: SNV's Arena as the guard reads it: it answers, no crash reporter.
pub fn snv_arena_process() -> ArenaProcess {
    ArenaProcess {
        pid: ARENA_PID,
        times: Ok(snv_times()),
        hung: Ok(false),
        monitors: Vec::new(),
        crash_report: Ok(false),
    }
}

/// #254: the crash reporter's window that waits for a click.
pub fn crash_report_window() -> ArenaWindow {
    ArenaWindow {
        title: "Crash Report".to_string(),
        visible: true,
        left: 1500,
        top: 800,
        right: 2100,
        bottom: 1250,
    }
}

/// The fake's Documents folder.
pub fn documents() -> PathBuf {
    PathBuf::from("profile").join("Documents")
}

/// The display Arena wants.
pub fn wanted() -> WantedDisplay {
    WantedDisplay {
        name: "Display 1".to_string(),
        device_id: SPAN_KEY.to_string(),
        width: 7680,
        height: 1080,
        fullscreen: true,
    }
}

/// Arena's saved output setup.
pub fn snv_output() -> ArenaOutput {
    ArenaOutput {
        advanced: true,
        preset: Some("snv surround 7680".to_string()),
        preset_found: true,
        displays: vec![wanted()],
    }
}

/// The grid NVAPI reports: 1×3, 2560×1080 @ 60 Hz.
pub fn span_grid() -> Grid {
    Grid {
        rows: 1,
        columns: 3,
        displays: vec![LEFT, MIDDLE, RIGHT],
        width: 2560,
        height: 1080,
        hz: 60,
    }
}

/// One screen alone, as NVAPI lists it when the span is dissolved.
pub fn single_grid(id: DisplayId) -> Grid {
    Grid {
        rows: 1,
        columns: 1,
        displays: vec![id],
        width: 2560,
        height: 1080,
        hz: 60,
    }
}

/// The NVAPI structure of `grid`, as the driver fills it.
pub fn topo(grid: &Grid) -> NvMosaicGridTopoV2 {
    let mut topo = NvMosaicGridTopoV2::for_query();
    topo.rows = grid.rows;
    topo.columns = grid.columns;
    topo.display_count = grid.displays.len() as u32;
    topo.flags = 0x12;
    for (slot, id) in topo.displays.iter_mut().zip(&grid.displays) {
        slot.display_id = id.0;
    }
    topo.display_settings.width = grid.width;
    topo.display_settings.height = grid.height;
    topo.display_settings.bpp = 32;
    topo.display_settings.freq = grid.hz;
    topo
}

/// The NVAPI structure of a connected display.
pub fn display_ids(id: DisplayId, connector: i32) -> NvGpuDisplayIds {
    NvGpuDisplayIds {
        connector_type: connector,
        display_id: id.0,
        flags: 0x0002_0054,
        ..NvGpuDisplayIds::for_query()
    }
}

/// A connected screen as the check keeps it.
pub fn connected(id: DisplayId, connector: &str) -> Connected {
    Connected {
        id,
        connector: connector.to_string(),
        flags: 0x0002_0054,
    }
}

/// The three screens, connected.
pub fn screens() -> Vec<Connected> {
    vec![
        connected(RIGHT, "HDMI"),
        connected(LEFT, "DisplayPort"),
        connected(MIDDLE, "DisplayPort"),
    ]
}

/// The VDD: the primary display, 3840×2160 at 0,0.
pub fn vdd() -> Display {
    Display {
        name: r"\\.\DISPLAY5".to_string(),
        device_key: VDD_KEY.to_string(),
        primary: true,
        x: 0,
        y: 0,
        width: 3840,
        height: 2160,
        hz: 60,
    }
}

/// The span: 7680×1080 at x 3840.
pub fn span() -> Display {
    Display {
        name: r"\\.\DISPLAY1".to_string(),
        device_key: SPAN_KEY.to_string(),
        primary: false,
        x: 3840,
        y: 0,
        width: 7680,
        height: 1080,
        hz: 60,
    }
}

/// Arena's main window, on the VDD.
pub fn arena_main() -> ArenaWindow {
    ArenaWindow {
        title: "Resolume Arena - snv".to_string(),
        visible: true,
        left: 0,
        top: 0,
        right: 3840,
        bottom: 2112,
    }
}

/// Arena's output window over the span.
pub fn arena_output() -> ArenaWindow {
    ArenaWindow {
        title: "Display".to_string(),
        visible: true,
        left: 3840,
        top: 0,
        right: 11520,
        bottom: 1080,
    }
}

/// NVAPI read, each read answering `connected` and `grids`.
pub fn nvidia_read(
    connected: Result<Vec<Connected>, String>,
    grids: Result<Vec<Grid>, String>,
) -> NvidiaFacts {
    NvidiaFacts::Read { connected, grids }
}

/// SNV's NVAPI, read: the three screens joined into the span's grid.
pub fn snv_nvidia() -> NvidiaFacts {
    nvidia_read(Ok(screens()), Ok(vec![span_grid()]))
}

/// SNV's NVAPI structures, as the driver fills them.
pub fn snv_nvidia_raw() -> NvidiaRaw {
    NvidiaRaw {
        connected: Ok(vec![
            display_ids(RIGHT, 4),
            display_ids(LEFT, 7),
            display_ids(MIDDLE, 7),
        ]),
        grids: Ok(vec![topo(&span_grid())]),
    }
}

/// SNV's facts, as the walk reads them from [`FakeReader::snv`] (NVAPI not
/// read: every display Arena wants is there).
pub fn snv_facts() -> WallFacts {
    WallFacts {
        arena: Ok(Arena {
            running: true,
            windows: vec![arena_main(), arena_output()],
        }),
        arena_output: Ok(snv_output()),
        displays: Ok(vec![vdd(), span()]),
        nvidia: NvidiaFacts::NotRead,
        guard: Ok(vec![snv_arena_process()]),
    }
}

/// SNV's `SimpleOutput.xml`.
pub const SIMPLE_ON: &str =
    r#"<?xml version="1.0" encoding="UTF-8"?><SimpleSetup advancedModeEnabled="1"/>"#;

/// SNV's `AdvancedOutput.xml`.
pub const ADVANCED: &str =
    r#"<XmlState><ScreenSetup name="ScreenSetup" presetFile="snv surround 7680"/></XmlState>"#;

/// SNV's preset: the span and a Spout screen.
pub fn preset_xml() -> String {
    format!(
        r#"<XmlState name="snv surround 7680"><ScreenSetup><screens>
<Screen name="LG wall"><OutputDevice><OutputDeviceDisplay name="Display 1" deviceId="{SPAN_KEY}" fullscreen="1" width="7680" height="1080"/></OutputDevice></Screen>
<Screen name="Spout"><OutputDevice><OutputDeviceSpout name="Arena"/></OutputDevice></Screen>
</screens></ScreenSetup></XmlState>"#
    )
}

/// A box whose every read is scripted.
pub struct FakeReader {
    /// NVAPI's answer; `None` = no NVIDIA driver.
    pub nvidia: Option<NvidiaRaw>,
    /// How many times NVAPI was asked.
    pub nvidia_asked: Cell<u32>,
    pub displays: Result<Vec<Display>, String>,
    pub processes: Result<Vec<(u32, String)>, String>,
    /// #254: each process's parent (`(pid, parent)`; a process not listed
    /// has parent 0).
    pub parents: Vec<(u32, u32)>,
    /// #254: each process's times (a process not listed cannot be opened).
    pub times: Vec<(u32, Result<ProcessTimes, String>)>,
    /// #254: whether a window of each process hangs (not listed: no).
    pub hung: Vec<(u32, Result<bool, String>)>,
    /// Every top-level window, with its process id.
    pub windows: Result<Vec<(u32, ArenaWindow)>, String>,
    pub documents: Result<PathBuf, String>,
    /// Each file's text (a file not listed does not exist), or why it
    /// cannot be read.
    pub files: Vec<(PathBuf, Result<String, String>)>,
    /// The process ids each `windows_of` was asked for.
    pub asked: RefCell<Vec<Vec<u32>>>,
}

impl FakeReader {
    /// SNV's box, the wall showing Arena's output.
    pub fn snv() -> Self {
        let docs = documents();
        Self {
            nvidia: Some(snv_nvidia_raw()),
            nvidia_asked: Cell::new(0),
            displays: Ok(vec![vdd(), span()]),
            processes: Ok(vec![
                (4, "System".to_string()),
                (ARENA_PID, "Arena.exe".to_string()),
                (777, "SongPlayer.exe".to_string()),
            ]),
            parents: Vec::new(),
            times: vec![(ARENA_PID, Ok(snv_times()))],
            hung: Vec::new(),
            windows: Ok(vec![
                (ARENA_PID, arena_main()),
                (
                    777,
                    ArenaWindow {
                        title: "Display".to_string(),
                        visible: true,
                        left: 3840,
                        top: 0,
                        right: 11520,
                        bottom: 1080,
                    },
                ),
                (ARENA_PID, arena_output()),
            ]),
            documents: Ok(docs.clone()),
            files: vec![
                (
                    resolume::simple_output_file(&docs),
                    Ok(SIMPLE_ON.to_string()),
                ),
                (
                    resolume::advanced_output_file(&docs),
                    Ok(ADVANCED.to_string()),
                ),
                (
                    resolume::preset_file(&docs, "snv surround 7680"),
                    Ok(preset_xml()),
                ),
            ],
            asked: RefCell::new(Vec::new()),
        }
    }

    /// The same box with `path`'s text replaced (or the file gone: `None`).
    pub fn with_file(mut self, path: PathBuf, text: Option<Result<String, String>>) -> Self {
        self.files.retain(|(listed, _)| *listed != path);
        if let Some(text) = text {
            self.files.push((path, text));
        }
        self
    }
}

impl WallReader for FakeReader {
    fn nvidia(&self) -> Option<NvidiaRaw> {
        self.nvidia_asked.set(self.nvidia_asked.get() + 1);
        self.nvidia.clone()
    }

    fn displays(&self) -> Result<Vec<Display>, String> {
        self.displays.clone()
    }

    fn processes(&self) -> Result<Vec<ProcessEntry>, String> {
        let parent = |pid: u32| {
            self.parents
                .iter()
                .find(|(child, _)| *child == pid)
                .map_or(0, |(_, parent)| *parent)
        };
        Ok(self
            .processes
            .clone()?
            .into_iter()
            .map(|(pid, image)| ProcessEntry {
                pid,
                parent: parent(pid),
                image,
            })
            .collect())
    }

    fn windows_of(&self, pids: &[u32]) -> Result<Vec<ArenaWindow>, String> {
        self.asked.borrow_mut().push(pids.to_vec());
        let windows = self.windows.clone()?;
        Ok(windows
            .into_iter()
            .filter(|(pid, _)| pids.contains(pid))
            .map(|(_, window)| window)
            .collect())
    }

    fn documents(&self) -> Result<PathBuf, String> {
        self.documents.clone()
    }

    fn file(&self, path: &Path) -> Result<Option<String>, String> {
        match self.files.iter().find(|(listed, _)| listed == path) {
            None => Ok(None),
            Some((_, text)) => text.clone().map(Some),
        }
    }

    fn times(&self, pid: u32) -> Result<ProcessTimes, String> {
        match self.times.iter().find(|(listed, _)| *listed == pid) {
            None => Err(format!("OpenProcess({pid}) failed (error 87)")),
            Some((_, times)) => times.clone(),
        }
    }

    fn hung(&self, pid: u32) -> Result<bool, String> {
        self.hung
            .iter()
            .find(|(listed, _)| *listed == pid)
            .map_or(Ok(false), |(_, hung)| hung.clone())
    }
}
