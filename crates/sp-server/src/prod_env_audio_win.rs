//! #255: the audio profile's raw reads on Windows (`prod_env::audio`'s
//! glue). Nothing here spawns a process (a launched process is what
//! Defender scanned on PP, #233):
//!
//! - the registry under HKLM through [`WinRegistry`] (`RegGetValueW`,
//!   `RegOpenKeyExW` / `RegEnumKeyExW`): the walk itself is
//!   `prod_env_audio_read.rs`'s;
//! - the network interfaces through `GetIfTable2` (iphlpapi);
//! - the active power scheme through the power API (`PowerGetActiveScheme`,
//!   `PowerReadFriendlyName`, `PowerReadACValueIndex`: what `powercfg`
//!   calls, defaults and custom schemes included);
//! - Dante Virtual Soundcard's `manager.json` and the SoundGrid driver's
//!   `SoundGrid.Driver.preferences` (under this process's own `%APPDATA%`:
//!   SongPlayer and its ASIO host run as the user whose driver they host)
//!   with `std::fs`.
//!
//! Every decision on what was read is `prod_env_audio_read.rs`'s and
//! `prod_env_audio.rs`'s, tested on Linux; this file only reads. Not
//! compiled on the Linux runner, so every fn is `mutants::skip`.

use std::path::PathBuf;

use super::AudioFacts;
use super::read::{
    DVS_MANAGER_JSON, IfRow, PowerRead, RegValue, RegistryRead, SOUNDGRID_PREFERENCES, guid_text,
    if_flags, mac_hex, read_audio as walk, read_file, read_under, reg_value_from,
};

/// The processor settings' power subgroup (`SUB_PROCESSOR`).
const SUB_PROCESSOR: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0x54533251_82be_4824_96c1_47b60b740d00);

/// The core-parking minimum (`CPMINCORES`).
const CPMINCORES: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0x0cc5b647_c1df_4637_891a_dec35c318583);

/// The most subkeys a key is read for (the network class has a few dozen).
const MAX_SUBKEYS: u32 = 4096;

/// The registry under HKLM.
pub struct WinRegistry;

impl RegistryRead for WinRegistry {
    #[cfg_attr(test, mutants::skip)] // Windows glue
    fn value(&self, key: &str, name: &str) -> RegValue {
        reg_value(key, name)
    }

    #[cfg_attr(test, mutants::skip)] // Windows glue
    fn subkeys(&self, key: &str) -> Result<Vec<String>, u32> {
        reg_subkeys(key)
    }
}

/// Every audio-profile fact of one check (`prod_env::read_direct`).
#[cfg_attr(test, mutants::skip)] // Windows glue, not compiled on the Linux runner
pub fn read_audio() -> AudioFacts {
    let dvs = read_file(&dvs_manager_path());
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let soundgrid = read_under(appdata.as_deref(), "APPDATA", SOUNDGRID_PREFERENCES);
    walk(&WinRegistry, &if_table(), &dvs, &soundgrid, &power())
}

/// `%ProgramData%\Audinate\Dante Virtual Soundcard\manager.json`.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn dvs_manager_path() -> PathBuf {
    let data = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
    PathBuf::from(data).join(DVS_MANAGER_JSON)
}

/// `s` as a NUL-terminated UTF-16 string.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// One value under HKLM, any type, as `RegGetValueW` gives it
/// (`REG_EXPAND_SZ` not expanded).
#[cfg_attr(test, mutants::skip)] // Windows glue
pub(super) fn reg_value(subkey: &str, value: &str) -> RegValue {
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_NOEXPAND, RRF_RT_ANY, RegGetValueW,
    };

    let key = wide(subkey);
    let name = wide(value);
    let flags = RRF_RT_ANY | RRF_NOEXPAND;
    let mut kind: u32 = 0;
    let mut size: u32 = 0;
    // SAFETY: both names are NUL-terminated UTF-16 buffers that outlive the
    // call; with a null data pointer the call only writes the type and the
    // size it needs.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            flags,
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    match rc {
        ERROR_SUCCESS => {}
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => return RegValue::Absent,
        other => return RegValue::Failed(other),
    }
    // Two spare UTF-16 units: a string stored with no terminator gets one.
    let mut buf = vec![0u8; size as usize + 4];
    let mut size = buf.len() as u32;
    // SAFETY: `buf` holds `size` bytes and outlives the call, which writes
    // at most `size` bytes into it and the written size into `size`.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            flags,
            &mut kind,
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    match rc {
        ERROR_SUCCESS => reg_value_from(kind, &buf[..(size as usize).min(buf.len())]),
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => RegValue::Absent,
        other => RegValue::Failed(other),
    }
}

/// The names of a key's subkeys under HKLM.
#[cfg_attr(test, mutants::skip)] // Windows glue
pub(super) fn reg_subkeys(subkey: &str) -> Result<Vec<String>, u32> {
    use windows_sys::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
    };

    let key = wide(subkey);
    let mut handle: HKEY = std::ptr::null_mut();
    // SAFETY: the name is a NUL-terminated UTF-16 buffer that outlives the
    // call, which writes the opened key into `handle`.
    let rc = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, key.as_ptr(), 0, KEY_READ, &mut handle) };
    if rc != ERROR_SUCCESS {
        return Err(rc);
    }
    let mut names = Vec::new();
    let mut result = Ok(());
    for index in 0..MAX_SUBKEYS {
        // A key name is at most 255 characters.
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        // SAFETY: `handle` is open; `name` holds `len` UTF-16 units and
        // outlives the call, which writes the name and its length.
        let rc = unsafe {
            RegEnumKeyExW(
                handle,
                index,
                name.as_mut_ptr(),
                &mut len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        match rc {
            ERROR_SUCCESS => {
                let len = (len as usize).min(name.len());
                names.push(String::from_utf16_lossy(&name[..len]));
            }
            ERROR_NO_MORE_ITEMS => break,
            other => {
                result = Err(other);
                break;
            }
        }
    }
    // SAFETY: closes the key opened above, once.
    unsafe { RegCloseKey(handle) };
    result.map(|()| names)
}

/// The network interfaces (`GetIfTable2`).
#[cfg_attr(test, mutants::skip)] // Windows glue
pub(super) fn if_table() -> Result<Vec<IfRow>, u32> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2,
    };

    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    // SAFETY: the call allocates the table and writes its address.
    let rc = unsafe { GetIfTable2(&mut table) };
    if rc != ERROR_SUCCESS {
        return Err(rc);
    }
    if table.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: the table holds `NumEntries` rows from its `Table` field on
    // (a C flexible array); it stays alive until `FreeMibTable` below, and
    // the rows are copied out before that.
    let rows = unsafe {
        let count = (*table).NumEntries as usize;
        let first = std::ptr::addr_of!((*table).Table).cast::<MIB_IF_ROW2>();
        std::slice::from_raw_parts(first, count)
            .iter()
            .map(|row| {
                let len = (row.PhysicalAddressLength as usize).min(row.PhysicalAddress.len());
                let g = row.InterfaceGuid;
                let (hardware, filter) = if_flags(row.InterfaceAndOperStatusFlags._bitfield);
                IfRow {
                    guid: format!(
                        "{{{}}}",
                        guid_text(g.data1, g.data2, g.data3, g.data4).to_ascii_uppercase()
                    ),
                    mac: mac_hex(&row.PhysicalAddress[..len]),
                    permanent_mac: mac_hex(&row.PermanentPhysicalAddress[..len]),
                    hardware,
                    filter,
                }
            })
            .collect()
    };
    // SAFETY: frees the table `GetIfTable2` allocated, once.
    unsafe { FreeMibTable(table as *const core::ffi::c_void) };
    Ok(rows)
}

/// The active power scheme, its name and its core-parking minimum (AC).
#[cfg_attr(test, mutants::skip)] // Windows glue
pub(super) fn power() -> PowerRead {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::System::Power::{
        PowerGetActiveScheme, PowerReadACValueIndex, PowerReadFriendlyName,
    };

    let mut active: *mut windows_sys::core::GUID = std::ptr::null_mut();
    // SAFETY: the call allocates the GUID (LocalAlloc) and writes its
    // address; it is copied and freed below.
    let rc = unsafe { PowerGetActiveScheme(std::ptr::null_mut(), &mut active) };
    if rc != ERROR_SUCCESS || active.is_null() {
        let code = if rc == ERROR_SUCCESS {
            ERROR_FILE_NOT_FOUND
        } else {
            rc
        };
        return PowerRead {
            scheme: Err(code),
            name: None,
            min_cores: Err(code),
        };
    }
    // SAFETY: `active` is the GUID the call allocated; it is read once and
    // freed once.
    let scheme = unsafe {
        let scheme = *active;
        LocalFree(active.cast());
        scheme
    };
    let text = guid_text(scheme.data1, scheme.data2, scheme.data3, scheme.data4);
    let mut size: u32 = 0;
    // SAFETY: with a null buffer the call only writes the size it needs.
    let rc = unsafe {
        PowerReadFriendlyName(
            std::ptr::null_mut(),
            &scheme,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    let name = if rc == ERROR_SUCCESS && size > 0 {
        let mut buf = vec![0u8; size as usize];
        // SAFETY: `buf` holds `size` bytes and outlives the call, which
        // writes the NUL-terminated UTF-16 name into it.
        let rc = unsafe {
            PowerReadFriendlyName(
                std::ptr::null_mut(),
                &scheme,
                std::ptr::null(),
                std::ptr::null(),
                buf.as_mut_ptr(),
                &mut size,
            )
        };
        match reg_value_from(super::read::REG_SZ, &buf) {
            RegValue::Text(name) if rc == ERROR_SUCCESS => Some(name),
            _ => None,
        }
    } else {
        None
    };
    let mut index: u32 = 0;
    // SAFETY: every GUID outlives the call, which writes one DWORD.
    let rc = unsafe {
        PowerReadACValueIndex(
            std::ptr::null_mut(),
            &scheme,
            &SUB_PROCESSOR,
            &CPMINCORES,
            &mut index,
        )
    };
    let min_cores = match rc {
        ERROR_SUCCESS => Ok(Some(index)),
        ERROR_FILE_NOT_FOUND => Ok(None),
        other => Err(other),
    };
    PowerRead {
        scheme: Ok(text),
        name,
        min_cores,
    }
}
