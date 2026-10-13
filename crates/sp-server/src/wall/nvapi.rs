//! #259: the NVAPI structures the wall check reads, their versions, and what
//! the check takes from them. Pure and Linux-tested; the calls through
//! `nvapi64.dll` are `win.rs`'s.
//!
//! NVAPI is NVIDIA's documented driver API (the open headers NVIDIA/nvapi,
//! `nvapi.h` + `nvapi_interface.h`). Every entry point is reached through the
//! one export `nvapi_QueryInterface(id)`; the ids below were verified against
//! `nvapi_interface.h`. Measured on SNV (RTX 3070 Ti, GeForce, 13.10.2026):
//! `EnumDisplayGrids` answers the 1×3 span, 2560×1080 32 bpp 60 Hz, flags
//! 0x12, displays left to right `0x80061086`, `0x80061087`, `0x80061083`;
//! `GetConnectedDisplayIds` names `0x80061083` HDMI (connector 4) and the
//! other two DisplayPort (connector 7).

use std::time::{Duration, Instant};

use sp_core::wall::{Connected, DisplayId, Grid};

/// `NvAPI_Initialize`.
pub const NVAPI_INITIALIZE: u32 = 0x0150_E828;
/// `NvAPI_EnumPhysicalGPUs`.
pub const NVAPI_ENUM_PHYSICAL_GPUS: u32 = 0xE5AC_921F;
/// `NvAPI_GPU_GetConnectedDisplayIds`.
pub const NVAPI_GPU_GET_CONNECTED_DISPLAY_IDS: u32 = 0x0078_DBA2;
/// `NvAPI_Mosaic_EnumDisplayGrids`.
pub const NVAPI_MOSAIC_ENUM_DISPLAY_GRIDS: u32 = 0xDF28_87AF;

/// `NVAPI_OK`: every entry point answers an `NvAPI_Status`, 0 = done.
pub const NVAPI_OK: i32 = 0;
/// `NVAPI_MAX_PHYSICAL_GPUS`: the size of `EnumPhysicalGPUs`' handle array.
pub const MAX_PHYSICAL_GPUS: usize = 64;
/// `NV_MOSAIC_MAX_DISPLAYS`: the displays a grid's array holds.
pub const MOSAIC_MAX_DISPLAYS: usize = 64;

/// `MAKE_NVAPI_VERSION(T, v)`: the structure's size in the low 16 bits, its
/// version above them. Written as a sum: it is the header's `|` while the
/// size fits 16 bits, and a `|` of disjoint bits would leave an equivalent
/// mutant.
pub const fn nvapi_version(size: usize, version: u32) -> u32 {
    size as u32 + (version << 16)
}

/// `NV_MOSAIC_GRID_TOPO_DISPLAY_V2`: one display of a grid (28 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NvMosaicGridTopoDisplayV2 {
    pub version: u32,
    pub display_id: u32,
    pub overlap_x: i32,
    pub overlap_y: i32,
    /// `NV_ROTATE`.
    pub rotation: i32,
    pub clone_group: u32,
    /// `NV_PIXEL_SHIFT_TYPE`.
    pub pixel_shift_type: i32,
}

/// `NV_MOSAIC_DISPLAY_SETTING_V1`: every display's mode in a grid (20 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NvMosaicDisplaySettingV1 {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    /// The refresh rate, Hz.
    pub freq: u32,
}

/// `NV_MOSAIC_GRID_TOPO_V2` (1832 bytes): one Mosaic grid as
/// `NvAPI_Mosaic_EnumDisplayGrids` fills it. `flags` is the header's bit
/// field word (`applyWithBezelCorrect`, `immersiveGaming`, `baseMosaic`,
/// `driverReloadAllowed`, `acceleratePrimaryDisplay`, `pixelShift`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NvMosaicGridTopoV2 {
    pub version: u32,
    pub rows: u32,
    pub columns: u32,
    pub display_count: u32,
    pub flags: u32,
    /// Row-major: left to right on a one-row grid.
    pub displays: [NvMosaicGridTopoDisplayV2; MOSAIC_MAX_DISPLAYS],
    pub display_settings: NvMosaicDisplaySettingV1,
}

/// `NV_MOSAIC_GRID_TOPO_VER2`.
pub const GRID_TOPO_VER: u32 = nvapi_version(std::mem::size_of::<NvMosaicGridTopoV2>(), 2);

impl NvMosaicGridTopoV2 {
    /// An element of `EnumDisplayGrids`' array: its version set, the rest
    /// for the driver to fill.
    pub fn for_query() -> Self {
        Self {
            version: GRID_TOPO_VER,
            rows: 0,
            columns: 0,
            display_count: 0,
            flags: 0,
            displays: [NvMosaicGridTopoDisplayV2::default(); MOSAIC_MAX_DISPLAYS],
            display_settings: NvMosaicDisplaySettingV1::default(),
        }
    }
}

/// `NV_GPU_DISPLAYIDS` (16 bytes): one display a GPU reports. `flags` is the
/// header's bit field word (bit 2 `isActive`, 4 `isOSVisible`, 6
/// `isConnected`, 17 `isPhysicallyConnected`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NvGpuDisplayIds {
    pub version: u32,
    /// `NV_MONITOR_CONN_TYPE`.
    pub connector_type: i32,
    pub display_id: u32,
    pub flags: u32,
}

/// `NV_GPU_DISPLAYIDS_VER` (version 3).
pub const DISPLAY_IDS_VER: u32 = nvapi_version(std::mem::size_of::<NvGpuDisplayIds>(), 3);

impl NvGpuDisplayIds {
    /// An element of `GetConnectedDisplayIds`' array: its version set.
    pub fn for_query() -> Self {
        Self {
            version: DISPLAY_IDS_VER,
            ..Self::default()
        }
    }
}

/// An `NV_MONITOR_CONN_TYPE` as the wall check names it: `HDMI` (4),
/// `DisplayPort` (7), else `type N`.
pub fn connector_name(kind: i32) -> String {
    match kind {
        4 => "HDMI".to_string(),
        7 => "DisplayPort".to_string(),
        other => format!("type {other}"),
    }
}

/// A connected display as the check keeps it.
pub fn connected(ids: &NvGpuDisplayIds) -> Connected {
    Connected {
        id: DisplayId(ids.display_id),
        connector: connector_name(ids.connector_type),
        flags: ids.flags,
    }
}

/// A grid as the check keeps it: its first `display_count` displays (at
/// most the array's 64), each display's mode.
pub fn grid(topo: &NvMosaicGridTopoV2) -> Grid {
    let count = (topo.display_count as usize).min(MOSAIC_MAX_DISPLAYS);
    Grid {
        rows: topo.rows,
        columns: topo.columns,
        displays: topo.displays[..count]
            .iter()
            .map(|d| DisplayId(d.display_id))
            .collect(),
        width: topo.display_settings.width,
        height: topo.display_settings.height,
        hz: topo.display_settings.freq,
    }
}

/// An entry point's answer: `NVAPI_OK`, else why (`call` and the status).
pub fn status(code: i32, call: &str) -> Result<(), String> {
    if code == NVAPI_OK {
        Ok(())
    } else {
        Err(format!("{call} answered NVAPI status {code}"))
    }
}

/// How long a failed load of `nvapi64.dll` or a failed `NvAPI_Initialize`
/// is answered from memory before NVAPI is set up again.
pub const SET_UP_RETRY: Duration = Duration::from_secs(60);

/// The process's NVAPI session, the decisions only (`win.rs` holds one with
/// the loaded DLL as `L`):
///
/// - loaded once and kept (never a DLL loaded and freed at every check);
/// - initialised before a read, and again after a read that answered any
///   non-OK status ([`Session::reset`]);
/// - a failed load or initialise is answered from memory, with its reason,
///   for [`SET_UP_RETRY`].
#[derive(Debug)]
pub struct Session<L> {
    loaded: Option<L>,
    initialised: bool,
    failed: Option<(Instant, String)>,
}

impl<L> Session<L> {
    /// Nothing loaded yet.
    pub const fn unloaded() -> Self {
        Self {
            loaded: None,
            initialised: false,
            failed: None,
        }
    }

    /// The loaded, initialised NVAPI at `now`: `load` when nothing is
    /// loaded, then `initialise` when not yet (or reset); or the last
    /// set-up failure's reason while it is younger than [`SET_UP_RETRY`].
    pub fn ready(
        &mut self,
        now: Instant,
        load: impl FnOnce() -> Result<L, String>,
        initialise: impl FnOnce(&L) -> Result<(), String>,
    ) -> Result<&L, String> {
        if !self.initialised {
            let recent = self
                .failed
                .as_ref()
                .filter(|(at, _)| now.saturating_duration_since(*at) < SET_UP_RETRY);
            if let Some((_, why)) = recent {
                return Err(why.clone());
            }
            if let Err(why) = self.set_up(load, initialise) {
                self.failed = Some((now, why.clone()));
                return Err(why);
            }
            self.initialised = true;
        }
        self.loaded
            .as_ref()
            .ok_or_else(|| "NVAPI is not loaded".to_string())
    }

    /// Loads when nothing is loaded (a failed load keeps nothing), then
    /// initialises (a failed initialise keeps the DLL loaded).
    fn set_up(
        &mut self,
        load: impl FnOnce() -> Result<L, String>,
        initialise: impl FnOnce(&L) -> Result<(), String>,
    ) -> Result<(), String> {
        let loaded = match self.loaded.take() {
            Some(loaded) => loaded,
            None => load()?,
        };
        initialise(self.loaded.insert(loaded))
    }

    /// A read answered a non-OK status: initialise again before the next
    /// read (the DLL stays loaded).
    pub fn reset(&mut self) {
        self.initialised = false;
    }
}

#[cfg(test)]
#[path = "nvapi_tests.rs"]
mod tests;
