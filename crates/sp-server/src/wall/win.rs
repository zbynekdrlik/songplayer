//! #259: the wall's raw reads on Windows (`wall`'s glue):
//!
//! - NVAPI from `nvapi64.dll`, loaded at run time with `libloading` (as
//!   sp-ndi loads NDI: no NVIDIA SDK at build time), every entry point
//!   reached through its one export `nvapi_QueryInterface`; a box without
//!   the NVIDIA driver (no `System32\nvapi64.dll`) reads "no NVIDIA";
//! - Windows' displays (`EnumDisplayDevicesW` + `EnumDisplaySettingsW`),
//!   each with its device key (the id Arena saves for it);
//! - Arena's saved setup files in `%USERPROFILE%\Documents`
//!   (`resolume.rs`, bounded reads);
//! - the processes (a Toolhelp snapshot: their image file names, no handle
//!   to any process opened);
//! - Arena's top-level windows (`EnumWindows`).
//!
//! Nothing spawns a process and nothing on the box is changed. Every
//! decision on what was read is `read.rs`'s and `decide.rs`'s, tested on
//! Linux; this file only reads. Not compiled on the Linux runner, so every
//! fn is `mutants::skip`.
//!
//! Coordinates: SongPlayer's process is per-monitor DPI aware (the Tauri
//! shell's window library makes it so at its start), so `GetWindowRect` and
//! each display's `dmPosition` / `dmPelsWidth` / `dmPelsHeight` are
//! physical pixels on one virtual desktop, and Arena's window and the span
//! are compared as read. A DPI-unaware process would read scaled rectangles
//! on a scaled display (not the case on SNV: the span runs at 100 %).

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sp_core::wall::{ArenaWindow, Display};
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};

use super::nvapi::{self, NvGpuDisplayIds, NvMosaicGridTopoV2};
use super::read::{NvidiaRaw, WallReader};
use super::resolume;

type QueryInterface = unsafe extern "C" fn(u32) -> *mut c_void;
type Initialize = unsafe extern "C" fn() -> i32;
type EnumPhysicalGpus = unsafe extern "C" fn(*mut *mut c_void, *mut u32) -> i32;
type GetConnectedDisplayIds =
    unsafe extern "C" fn(*mut c_void, *mut NvGpuDisplayIds, *mut u32, u32) -> i32;
type EnumDisplayGrids = unsafe extern "C" fn(*mut NvMosaicGridTopoV2, *mut u32) -> i32;

/// The most displays one GPU's list, or the grid list, is read for.
const MAX_LISTED: usize = 64;

/// The most display devices enumerated.
const MAX_DEVICES: u32 = 64;

/// The most processes listed.
const MAX_PROCESSES: usize = 16_384;

/// The longest window title read, in UTF-16 units.
const TITLE_UNITS: usize = 512;

/// The NVAPI entry points the check calls, from one loaded and initialised
/// `nvapi64.dll`.
struct Nvapi {
    /// Keeps the DLL, and so every entry point below, loaded.
    #[allow(dead_code)] // held for its Drop only
    library: libloading::Library,
    enum_gpus: EnumPhysicalGpus,
    connected_ids: GetConnectedDisplayIds,
    enum_grids: EnumDisplayGrids,
}

/// The process's NVAPI, loaded by the first read that finds it; a load or
/// `NvAPI_Initialize` that fails is tried again at the next read.
static NVAPI: Mutex<Option<Nvapi>> = Mutex::new(None);

/// Loads `nvapi64.dll`, resolves the entry points and initialises NVAPI.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn load() -> Result<Nvapi, String> {
    // SAFETY: nvapi64.dll is the NVIDIA driver's own library; loading it
    // runs only its initialisation.
    let library = unsafe { libloading::Library::new("nvapi64.dll") }
        .map_err(|e| format!("nvapi64.dll could not be loaded (no NVIDIA driver?): {e}"))?;
    // SAFETY: `nvapi_QueryInterface` is NVAPI's one export, `unsigned int`
    // in, the entry point (or null) out.
    let query: QueryInterface = unsafe {
        *library
            .get::<QueryInterface>(b"nvapi_QueryInterface\0")
            .map_err(|e| format!("nvapi64.dll has no nvapi_QueryInterface: {e}"))?
    };
    let entry = |id: u32, name: &str| -> Result<*mut c_void, String> {
        // SAFETY: `query` takes any id and answers null for one it lacks.
        let pointer = unsafe { query(id) };
        if pointer.is_null() {
            Err(format!("nvapi64.dll has no {name}"))
        } else {
            Ok(pointer)
        }
    };
    // SAFETY (each transmute): NVAPI answers an id with the entry point of
    // that id, whose C signature (nvapi.h) is the type it is turned into.
    let initialize = unsafe {
        std::mem::transmute::<*mut c_void, Initialize>(entry(
            nvapi::NVAPI_INITIALIZE,
            "NvAPI_Initialize",
        )?)
    };
    let enum_gpus = unsafe {
        std::mem::transmute::<*mut c_void, EnumPhysicalGpus>(entry(
            nvapi::NVAPI_ENUM_PHYSICAL_GPUS,
            "NvAPI_EnumPhysicalGPUs",
        )?)
    };
    let connected_ids = unsafe {
        std::mem::transmute::<*mut c_void, GetConnectedDisplayIds>(entry(
            nvapi::NVAPI_GPU_GET_CONNECTED_DISPLAY_IDS,
            "NvAPI_GPU_GetConnectedDisplayIds",
        )?)
    };
    let enum_grids = unsafe {
        std::mem::transmute::<*mut c_void, EnumDisplayGrids>(entry(
            nvapi::NVAPI_MOSAIC_ENUM_DISPLAY_GRIDS,
            "NvAPI_Mosaic_EnumDisplayGrids",
        )?)
    };
    // SAFETY: NvAPI_Initialize takes nothing; it may be called again.
    nvapi::status(unsafe { initialize() }, "NvAPI_Initialize")?;
    Ok(Nvapi {
        library,
        enum_gpus,
        connected_ids,
        enum_grids,
    })
}

/// `read` with the process's NVAPI, loading it first when it is not yet.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn with_nvapi<T>(read: impl FnOnce(&Nvapi) -> Result<T, String>) -> Result<T, String> {
    let mut held = NVAPI.lock().unwrap_or_else(|e| e.into_inner());
    if held.is_none() {
        *held = Some(load()?);
    }
    match held.as_ref() {
        Some(nvapi) => read(nvapi),
        None => Err("NVAPI is not loaded".to_string()),
    }
}

/// Every NVIDIA GPU's handle.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn gpus(nvapi: &Nvapi) -> Result<Vec<*mut c_void>, String> {
    let mut handles = [std::ptr::null_mut::<c_void>(); nvapi::MAX_PHYSICAL_GPUS];
    let mut count = 0_u32;
    // SAFETY: the array holds NVAPI_MAX_PHYSICAL_GPUS handles, as the call
    // expects; both pointers outlive the call.
    let code = unsafe { (nvapi.enum_gpus)(handles.as_mut_ptr(), &mut count) };
    nvapi::status(code, "NvAPI_EnumPhysicalGPUs")?;
    let count = (count as usize).min(handles.len());
    Ok(handles[..count].to_vec())
}

/// `units` up to its first NUL, as a string.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn wide_string(units: &[u16]) -> String {
    let len = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..len])
}

/// `EnumWindows`' callback: every top-level window into the list `lparam`
/// points to.
#[cfg_attr(test, mutants::skip)] // Windows glue
unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the `&mut Vec<HWND>` `windows_of` passed; it lives
    // for the whole `EnumWindows` call, which calls this on its own thread.
    let windows = unsafe { &mut *(lparam as *mut Vec<HWND>) };
    windows.push(hwnd);
    1
}

/// The NVIDIA driver is installed: its `nvapi64.dll` is in System32 (a box
/// without it has no NVIDIA screen to ask about: no error).
#[cfg_attr(test, mutants::skip)] // Windows glue
fn nvapi_installed() -> bool {
    std::env::var_os("SystemRoot").is_some_and(|root| {
        std::path::Path::new(&root)
            .join("System32")
            .join("nvapi64.dll")
            .is_file()
    })
}

/// Every display each NVIDIA GPU reports connected.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn connected_ids(nvapi: &Nvapi) -> Result<Vec<NvGpuDisplayIds>, String> {
    let mut all = Vec::new();
    for gpu in gpus(nvapi)? {
        let mut count = 0_u32;
        // SAFETY: a null array asks only for the count.
        let code = unsafe { (nvapi.connected_ids)(gpu, std::ptr::null_mut(), &mut count, 0) };
        nvapi::status(code, "NvAPI_GPU_GetConnectedDisplayIds (count)")?;
        let wanted = (count as usize).min(MAX_LISTED);
        if wanted == 0 {
            continue;
        }
        let mut ids = vec![NvGpuDisplayIds::for_query(); wanted];
        let mut count = wanted as u32;
        // SAFETY: `ids` holds `count` elements, each with its version set;
        // both pointers outlive the call.
        let code = unsafe { (nvapi.connected_ids)(gpu, ids.as_mut_ptr(), &mut count, 0) };
        nvapi::status(code, "NvAPI_GPU_GetConnectedDisplayIds")?;
        ids.truncate((count as usize).min(wanted));
        all.extend(ids);
    }
    Ok(all)
}

/// The Mosaic (Surround) grids.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn grids(nvapi: &Nvapi) -> Result<Vec<NvMosaicGridTopoV2>, String> {
    let mut count = 0_u32;
    // SAFETY: a null array asks only for the count.
    let code = unsafe { (nvapi.enum_grids)(std::ptr::null_mut(), &mut count) };
    nvapi::status(code, "NvAPI_Mosaic_EnumDisplayGrids (count)")?;
    let wanted = (count as usize).min(MAX_LISTED);
    if wanted == 0 {
        return Ok(Vec::new());
    }
    let mut grids = vec![NvMosaicGridTopoV2::for_query(); wanted];
    let mut count = wanted as u32;
    // SAFETY: `grids` holds `count` elements, each with its version set;
    // both pointers outlive the call.
    let code = unsafe { (nvapi.enum_grids)(grids.as_mut_ptr(), &mut count) };
    nvapi::status(code, "NvAPI_Mosaic_EnumDisplayGrids")?;
    grids.truncate((count as usize).min(wanted));
    Ok(grids)
}

/// The box's reads on Windows.
pub struct WinReader;

impl WallReader for WinReader {
    #[cfg_attr(test, mutants::skip)] // Windows glue
    fn nvidia(&self) -> Result<Option<NvidiaRaw>, String> {
        if !nvapi_installed() {
            return Ok(None);
        }
        with_nvapi(|nvapi| {
            Ok(Some(NvidiaRaw {
                connected: connected_ids(nvapi)?,
                grids: grids(nvapi)?,
            }))
        })
    }

    #[cfg_attr(test, mutants::skip)] // Windows glue
    fn documents(&self) -> Result<PathBuf, String> {
        resolume::documents(std::env::var_os("USERPROFILE"))
    }

    #[cfg_attr(test, mutants::skip)] // Windows glue
    fn file(&self, path: &Path) -> Result<Option<String>, String> {
        resolume::read_bounded(path)
    }

    #[cfg_attr(test, mutants::skip)] // Windows glue
    fn displays(&self) -> Result<Vec<Display>, String> {
        use windows_sys::Win32::Graphics::Gdi::{
            DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_PRIMARY_DEVICE,
            DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW, EnumDisplaySettingsW,
        };
        let mut displays = Vec::new();
        for index in 0..MAX_DEVICES {
            // SAFETY: DISPLAY_DEVICEW is plain data; all zeroes is a value.
            let mut device: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
            device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            // SAFETY: `device` is a DISPLAY_DEVICEW whose `cb` is its size;
            // a null device name lists the adapters' outputs.
            if unsafe { EnumDisplayDevicesW(std::ptr::null(), index, &mut device, 0) } == 0 {
                break;
            }
            if device.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP == 0 {
                continue;
            }
            let name = wide_string(&device.DeviceName);
            // SAFETY: DEVMODEW is plain data; all zeroes is a value.
            let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
            mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            // SAFETY: `DeviceName` is NUL-terminated (the call above wrote
            // it) and `mode` a DEVMODEW whose `dmSize` is its size.
            let read = unsafe {
                EnumDisplaySettingsW(device.DeviceName.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode)
            };
            if read == 0 {
                return Err(format!("EnumDisplaySettingsW failed for {name}"));
            }
            // SAFETY: for a display device the union holds the display's
            // fields (its position).
            let position = unsafe { mode.Anonymous1.Anonymous2.dmPosition };
            displays.push(Display {
                name,
                // The display's registry key, the one Arena saves as its
                // `deviceId` (`\Registry\Machine\…\Video\{GUID}\000N`).
                device_key: wide_string(&device.DeviceKey),
                primary: device.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE != 0,
                x: position.x,
                y: position.y,
                width: mode.dmPelsWidth,
                height: mode.dmPelsHeight,
                hz: mode.dmDisplayFrequency,
            });
        }
        Ok(displays)
    }

    #[cfg_attr(test, mutants::skip)] // Windows glue
    fn processes(&self) -> Result<Vec<(u32, String)>, String> {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        };
        // SAFETY: a snapshot of the process list; closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            // SAFETY: reads this thread's last error.
            let error = unsafe { GetLastError() };
            return Err(format!("CreateToolhelp32Snapshot failed (error {error})"));
        }
        let mut processes = Vec::new();
        // SAFETY: PROCESSENTRY32W is plain data; all zeroes is a value.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        // SAFETY: `snapshot` is open and `entry` a PROCESSENTRY32W whose
        // `dwSize` is its size (each call below likewise).
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while more && processes.len() < MAX_PROCESSES {
            processes.push((entry.th32ProcessID, wide_string(&entry.szExeFile)));
            // SAFETY: as above.
            more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        // SAFETY: the snapshot is closed once, here.
        unsafe { CloseHandle(snapshot) };
        Ok(processes)
    }

    #[cfg_attr(test, mutants::skip)] // Windows glue
    fn windows_of(&self, pids: &[u32]) -> Result<Vec<ArenaWindow>, String> {
        use windows_sys::Win32::Foundation::{GetLastError, RECT};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
        };
        let mut handles: Vec<HWND> = Vec::new();
        // SAFETY: `collect_window` only pushes onto `handles`, which outlives
        // the call.
        let listed = unsafe {
            EnumWindows(
                Some(collect_window),
                &mut handles as *mut Vec<HWND> as LPARAM,
            )
        };
        if listed == 0 {
            // SAFETY: reads this thread's last error.
            let error = unsafe { GetLastError() };
            return Err(format!("EnumWindows failed (error {error})"));
        }
        let mut windows = Vec::new();
        for hwnd in handles {
            let mut pid = 0_u32;
            // SAFETY: `hwnd` came from EnumWindows (a window closed since
            // answers 0); `pid` outlives the call.
            unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
            if !pids.contains(&pid) {
                continue;
            }
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            // SAFETY: as above; `rect` outlives the call.
            if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
                continue; // the window closed meanwhile
            }
            let mut title = [0_u16; TITLE_UNITS];
            // SAFETY: `title` holds TITLE_UNITS units; the call writes at
            // most that many, its NUL included, and answers the length. For
            // another process's window it reads the caption and sends no
            // message to that (perhaps hung) process.
            let len = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), TITLE_UNITS as i32) };
            let len = usize::try_from(len).unwrap_or(0).min(TITLE_UNITS);
            windows.push(ArenaWindow {
                title: String::from_utf16_lossy(&title[..len]),
                // SAFETY: as above.
                visible: unsafe { IsWindowVisible(hwnd) } != 0,
                left: rect.left,
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom,
            });
        }
        Ok(windows)
    }
}

#[cfg(test)]
#[path = "win_tests.rs"]
mod tests;
