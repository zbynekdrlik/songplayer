//! #259: NVAPI's layouts against the C header, their versions, and the
//! decoding of a grid and a connected display (SNV's, 13.10.2026).

use std::cell::Cell;
use std::mem::offset_of;
use std::time::{Duration, Instant};

use sp_core::wall::{Connected, DisplayId, Grid};

use super::*;

#[test]
fn the_layouts_are_the_headers() {
    assert_eq!(size_of::<NvMosaicGridTopoDisplayV2>(), 28);
    assert_eq!(size_of::<NvMosaicDisplaySettingV1>(), 20);
    assert_eq!(size_of::<NvMosaicGridTopoV2>(), 1832);
    assert_eq!(size_of::<NvGpuDisplayIds>(), 16);
    assert_eq!(offset_of!(NvMosaicGridTopoV2, flags), 16);
    assert_eq!(offset_of!(NvMosaicGridTopoV2, displays), 20);
    assert_eq!(
        offset_of!(NvMosaicGridTopoV2, display_settings),
        20 + 64 * 28
    );
    assert_eq!(offset_of!(NvMosaicGridTopoDisplayV2, display_id), 4);
    assert_eq!(offset_of!(NvMosaicDisplaySettingV1, freq), 16);
    assert_eq!(offset_of!(NvGpuDisplayIds, display_id), 8);
    assert_eq!(offset_of!(NvGpuDisplayIds, flags), 12);
}

#[test]
fn each_structure_carries_its_version() {
    // MAKE_NVAPI_VERSION(NV_MOSAIC_GRID_TOPO_V2, 2) = 1832 | (2 << 16).
    assert_eq!(GRID_TOPO_VER, 0x0002_0728);
    // MAKE_NVAPI_VERSION(NV_GPU_DISPLAYIDS, 3) = 16 | (3 << 16).
    assert_eq!(DISPLAY_IDS_VER, 0x0003_0010);
    assert_eq!(nvapi_version(1832, 2), 1832 | (2 << 16));
    assert_eq!(nvapi_version(28, 1), 28 | (1 << 16));
    assert_eq!(NvMosaicGridTopoV2::for_query().version, 0x0002_0728);
    assert_eq!(NvMosaicGridTopoV2::for_query().display_count, 0);
    assert_eq!(NvGpuDisplayIds::for_query().version, 0x0003_0010);
    assert_eq!(NvGpuDisplayIds::for_query().display_id, 0);
}

/// Writes `value` little-endian at `offset` of `bytes`.
fn put(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// SNV's span as `EnumDisplayGrids` writes it, laid out by the C header's
/// offsets (independently of the Rust structure): 1×3, flags 0x12, the
/// displays A B C, 2560×1080 32 bpp 60 Hz.
fn snv_grid_bytes() -> Vec<u8> {
    let mut bytes = vec![0_u8; 1832];
    put(&mut bytes, 0, 0x0002_0728);
    put(&mut bytes, 4, 1); // rows
    put(&mut bytes, 8, 3); // columns
    put(&mut bytes, 12, 3); // displayCount
    put(&mut bytes, 16, 0x12); // the bit field word
    for (i, id) in [0x8006_1086_u32, 0x8006_1087, 0x8006_1083]
        .into_iter()
        .enumerate()
    {
        put(&mut bytes, 20 + 28 * i, 0x0002_001C); // the display's version
        put(&mut bytes, 20 + 28 * i + 4, id);
    }
    put(&mut bytes, 1812, 0x0001_0014); // displaySettings.version
    put(&mut bytes, 1816, 2560);
    put(&mut bytes, 1820, 1080);
    put(&mut bytes, 1824, 32);
    put(&mut bytes, 1828, 60);
    bytes
}

/// The structure the driver filled, read from its bytes.
fn topo_from(bytes: &[u8]) -> NvMosaicGridTopoV2 {
    assert_eq!(bytes.len(), size_of::<NvMosaicGridTopoV2>());
    // SAFETY: the slice holds exactly one structure's bytes, every bit
    // pattern is a valid value of its plain integer fields, and the read
    // is unaligned.
    unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<NvMosaicGridTopoV2>()) }
}

fn snv_grid() -> Grid {
    Grid {
        rows: 1,
        columns: 3,
        displays: vec![
            DisplayId(0x8006_1086),
            DisplayId(0x8006_1087),
            DisplayId(0x8006_1083),
        ],
        width: 2560,
        height: 1080,
        hz: 60,
    }
}

#[test]
fn snvs_span_decodes_from_its_bytes() {
    let topo = topo_from(&snv_grid_bytes());
    assert_eq!(topo.flags, 0x12);
    assert_eq!(topo.display_settings.bpp, 32);
    assert_eq!(grid(&topo), snv_grid());
}

#[test]
fn a_grid_keeps_only_the_displays_it_counts() {
    let mut topo = topo_from(&snv_grid_bytes());
    topo.display_count = 2;
    assert_eq!(
        grid(&topo).displays,
        [DisplayId(0x8006_1086), DisplayId(0x8006_1087)]
    );
    // A count past the array (a driver's bad answer) reads the whole array.
    topo.display_count = 1000;
    assert_eq!(grid(&topo).displays.len(), MOSAIC_MAX_DISPLAYS);
}

#[test]
fn a_connected_display_decodes_from_its_bytes() {
    let mut bytes = [0_u8; 16];
    put(&mut bytes, 0, 0x0003_0010);
    put(&mut bytes, 4, 4); // NV_MONITOR_CONN_TYPE_HDMI
    put(&mut bytes, 8, 0x8006_1083);
    put(&mut bytes, 12, 0x0002_0054);
    // SAFETY: as `topo_from`: one structure's bytes, plain integers.
    let ids = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<NvGpuDisplayIds>()) };
    assert_eq!(
        connected(&ids),
        Connected {
            id: DisplayId(0x8006_1083),
            connector: "HDMI".to_string(),
            flags: 0x0002_0054,
        }
    );
}

#[test]
fn connectors_are_named() {
    assert_eq!(connector_name(4), "HDMI");
    assert_eq!(connector_name(7), "DisplayPort");
    assert_eq!(connector_name(5), "type 5");
    assert_eq!(connector_name(-1), "type -1");
}

#[test]
fn a_status_other_than_ok_names_the_call() {
    assert_eq!(status(0, "NvAPI_Initialize"), Ok(()));
    assert_eq!(
        status(-6, "NvAPI_EnumPhysicalGPUs"),
        Err("NvAPI_EnumPhysicalGPUs answered NVAPI status -6".to_string())
    );
}

/// A session's set-up calls, counted.
#[derive(Default)]
struct Calls {
    loads: Cell<u32>,
    inits: Cell<u32>,
}

impl Calls {
    /// `session.ready` at `now`, the load answering `load` and the
    /// initialise `init`; the loaded value is the load's number.
    fn ready(
        &self,
        session: &mut Session<u32>,
        now: Instant,
        load: Result<(), &str>,
        init: Result<(), &str>,
    ) -> Result<u32, String> {
        session
            .ready(
                now,
                || {
                    self.loads.set(self.loads.get() + 1);
                    load.map(|()| self.loads.get()).map_err(str::to_string)
                },
                |_| {
                    self.inits.set(self.inits.get() + 1);
                    init.map_err(str::to_string)
                },
            )
            .copied()
    }

    fn counts(&self) -> (u32, u32) {
        (self.loads.get(), self.inits.get())
    }
}

#[test]
fn nvapi_is_loaded_once_and_initialised_until_a_read_fails() {
    let calls = Calls::default();
    let mut session = Session::unloaded();
    let t0 = Instant::now();
    assert_eq!(calls.ready(&mut session, t0, Ok(()), Ok(())), Ok(1));
    assert_eq!(calls.ready(&mut session, t0, Ok(()), Ok(())), Ok(1));
    assert_eq!(calls.counts(), (1, 1));
    // A read answered a non-OK status: initialised again, the DLL kept.
    session.reset();
    assert_eq!(calls.ready(&mut session, t0, Ok(()), Ok(())), Ok(1));
    assert_eq!(calls.counts(), (1, 2));
}

#[test]
fn a_failed_load_is_answered_from_memory_for_a_minute() {
    let calls = Calls::default();
    let mut session = Session::unloaded();
    let t0 = Instant::now();
    let why = Err("nvapi64.dll could not be loaded".to_string());
    let failing = Err("nvapi64.dll could not be loaded");
    assert_eq!(calls.ready(&mut session, t0, failing, Ok(())), why);
    let almost = t0 + SET_UP_RETRY - Duration::from_millis(1);
    assert_eq!(calls.ready(&mut session, almost, Ok(()), Ok(())), why);
    assert_eq!(calls.counts(), (1, 0));
    // A minute on, tried again (a load that failed kept nothing).
    let later = t0 + SET_UP_RETRY;
    assert_eq!(calls.ready(&mut session, later, Ok(()), Ok(())), Ok(2));
    assert_eq!(calls.counts(), (2, 1));
    assert_eq!(SET_UP_RETRY, Duration::from_secs(60));
}

#[test]
fn a_failed_initialise_keeps_the_dll_and_waits_a_minute() {
    let calls = Calls::default();
    let mut session = Session::unloaded();
    let t0 = Instant::now();
    let why = Err("NvAPI_Initialize answered NVAPI status -6".to_string());
    let failing = Err("NvAPI_Initialize answered NVAPI status -6");
    assert_eq!(calls.ready(&mut session, t0, Ok(()), failing), why);
    let soon = t0 + Duration::from_secs(5);
    assert_eq!(calls.ready(&mut session, soon, Ok(()), Ok(())), why);
    assert_eq!(calls.counts(), (1, 1));
    let later = t0 + SET_UP_RETRY;
    assert_eq!(calls.ready(&mut session, later, Ok(()), Ok(())), Ok(1));
    assert_eq!(calls.counts(), (1, 2));
    // A read failing after a minute's success initialises at once.
    session.reset();
    let next = later + Duration::from_secs(5);
    assert_eq!(calls.ready(&mut session, next, Ok(()), Ok(())), Ok(1));
    assert_eq!(calls.counts(), (1, 3));
}
