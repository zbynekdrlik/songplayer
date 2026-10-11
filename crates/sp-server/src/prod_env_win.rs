//! #233: the production environment's facts on Windows (`prod_env`'s glue).
//!
//! Nothing here spawns a process (a launched process is exactly what
//! Defender scanned on PP): the edition and the firewall come from the
//! registry (`RegGetValueW`), Defender's real-time protection from WMI
//! (`root\Microsoft\Windows\Defender`, `MSFT_MpComputerStatus`) over COM.
//! Every decision on what was read (the firewall policy over the local
//! value, a missing Defender namespace) is `prod_env`'s, tested on Linux;
//! this file only reads. Not compiled on the Linux runner, so every fn is
//! `mutants::skip`.

use super::{EnvFacts, RegDword, defender_realtime, firewall_on};
use sp_core::prod_env::FirewallProfile;

/// The Windows version key (`EditionID`, `ProductName`).
const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

/// The group-policy firewall keys, by profile (domain, private, public).
const POLICY_FIREWALL: [&str; 3] = [
    r"SOFTWARE\Policies\Microsoft\WindowsFirewall\DomainProfile",
    r"SOFTWARE\Policies\Microsoft\WindowsFirewall\PrivateProfile",
    r"SOFTWARE\Policies\Microsoft\WindowsFirewall\PublicProfile",
];

/// The local firewall keys, by profile (the private profile is `Standard`).
const LOCAL_FIREWALL: [&str; 3] = [
    r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\DomainProfile",
    r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\StandardProfile",
    r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\PublicProfile",
];

/// How long WMI may take to hand over Defender's status, ms.
const WMI_NEXT_TIMEOUT_MS: i32 = 10_000;

/// Reads every fact. Runs on the blocking pool (`prod_env::Reader`).
#[cfg_attr(test, mutants::skip)] // Windows glue, not compiled on the Linux runner
pub fn read_facts() -> EnvFacts {
    let mut read_errors = Vec::new();
    let edition_id = string_fact(&mut read_errors, "EditionID");
    let product_name = string_fact(&mut read_errors, "ProductName");
    let mut firewall = [None; 3];
    for (i, profile) in FirewallProfile::ALL.into_iter().enumerate() {
        let policy = reg_dword(POLICY_FIREWALL[i], "EnableFirewall");
        let local = reg_dword(LOCAL_FIREWALL[i], "EnableFirewall");
        firewall[i] = match firewall_on(profile, policy, local) {
            Ok(on) => Some(on),
            Err(why) => {
                read_errors.push(why);
                None
            }
        };
    }
    let defender = match defender_realtime(read_defender()) {
        Ok(on) => Some(on),
        Err(why) => {
            read_errors.push(why);
            None
        }
    };
    EnvFacts {
        edition_id,
        product_name,
        defender_realtime: defender,
        firewall,
        read_errors,
    }
}

/// A string of the Windows version key, or `None` with why.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn string_fact(errors: &mut Vec<String>, value: &str) -> Option<String> {
    match reg_string(CURRENT_VERSION, value) {
        Ok(Some(s)) => Some(s),
        Ok(None) => {
            errors.push(format!("{value} is missing"));
            None
        }
        Err(code) => {
            errors.push(format!("{value} could not be read (error {code})"));
            None
        }
    }
}

/// `s` as a NUL-terminated UTF-16 string.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A `REG_SZ` value under HKLM: `Ok(None)` when the key or value is absent.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn reg_string(subkey: &str, value: &str) -> Result<Option<String>, u32> {
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
    };
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};

    let key = wide(subkey);
    let name = wide(value);
    let mut bytes: u32 = 0;
    // SAFETY: both names are NUL-terminated UTF-16 buffers that outlive the
    // call; with a null data pointer the call only writes the size it needs
    // into `bytes`.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    match rc {
        ERROR_SUCCESS => {}
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => return Ok(None),
        other => return Err(other),
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2) + 1];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: `buf` holds `size` bytes and outlives the call, which writes at
    // most `size` bytes into it and the written size into `size`.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    match rc {
        ERROR_SUCCESS => {}
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => return Ok(None),
        other => return Err(other),
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(Some(String::from_utf16_lossy(&buf[..len])))
}

/// A `REG_DWORD` value under HKLM.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn reg_dword(subkey: &str, value: &str) -> RegDword {
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW,
    };

    let key = wide(subkey);
    let name = wide(value);
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: both names are NUL-terminated UTF-16 buffers and `data` is a
    // u32 of `size` bytes; all outlive the call, which writes one DWORD.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    match rc {
        ERROR_SUCCESS => RegDword::Value(data),
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => RegDword::Absent,
        other => RegDword::Failed(other),
    }
}

/// Defender's real-time protection through WMI, on a thread of its own (its
/// own COM apartment, never the blocking pool's): the property, `Ok(None)`
/// with no instance or no value, or the failing HRESULT.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn read_defender() -> Result<Option<bool>, i32> {
    /// E_FAIL: the thread could not be started or ended in a panic.
    const E_FAIL: i32 = 0x8000_4005_u32 as i32;
    match std::thread::Builder::new()
        .name("env-defender".to_string())
        .spawn(defender_in_own_apartment)
    {
        Ok(thread) => thread.join().unwrap_or(Err(E_FAIL)),
        Err(_) => Err(E_FAIL),
    }
}

/// Uninitializes the thread's COM when dropped (after every COM object).
struct ComApartment;

impl Drop for ComApartment {
    #[cfg_attr(test, mutants::skip)] // Windows glue
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx of this thread.
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
}

/// COM in a multithreaded apartment on this thread, then the query.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn defender_in_own_apartment() -> Result<Option<bool>, i32> {
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    // SAFETY: a fresh thread, initialized once; `ComApartment` balances it.
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if init.is_err() {
        return Err(init.0);
    }
    let _apartment = ComApartment;
    // Every COM object of the query is dropped when it returns, before the
    // apartment is.
    query_defender().map_err(|e| e.code().0)
}

/// `SELECT RealTimeProtectionEnabled FROM MSFT_MpComputerStatus` in
/// `root\Microsoft\Windows\Defender`. The process's COM security is left
/// alone (no `CoInitializeSecurity`: other COM users share the process);
/// the services proxy is set to impersonate, the documented way to query
/// WMI without it.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn query_defender() -> windows::core::Result<Option<bool>> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, CoCreateInstance, CoSetProxyBlanket, EOAC_NONE,
        RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
    };
    use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
    use windows::Win32::System::Wmi::{
        IWbemClassObject, IWbemLocator, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY,
        WBEM_S_TIMEDOUT, WbemLocator,
    };
    use windows::core::{BSTR, HRESULT, PCWSTR, VARIANT, w};

    // SAFETY: COM is initialized on this thread (`defender_in_own_apartment`);
    // every pointer handed over is a live local or a null the API accepts.
    unsafe {
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
        let services = locator.ConnectServer(
            &BSTR::from(r"ROOT\Microsoft\Windows\Defender"),
            None,
            None,
            None,
            0,
            None,
            None,
        )?;
        CoSetProxyBlanket(
            &services,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            PCWSTR::null(),
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )?;
        let rows = services.ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from("SELECT RealTimeProtectionEnabled FROM MSFT_MpComputerStatus"),
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )?;
        let mut row: [Option<IWbemClassObject>; 1] = [None];
        let mut returned: u32 = 0;
        let next = rows.Next(WMI_NEXT_TIMEOUT_MS, &mut row, &mut returned);
        next.ok()?;
        if next == HRESULT(WBEM_S_TIMEDOUT.0) {
            return Err(windows::core::Error::from(HRESULT(WBEM_S_TIMEDOUT.0)));
        }
        let Some(row) = &row[0] else {
            return Ok(None);
        };
        let mut value = VARIANT::default();
        row.Get(w!("RealTimeProtectionEnabled"), 0, &mut value, None, None)?;
        Ok(bool::try_from(&value).ok())
    }
}
