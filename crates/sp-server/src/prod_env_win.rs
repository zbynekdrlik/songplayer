//! #233: the production environment's raw reads on Windows (`prod_env`'s
//! glue).
//!
//! Nothing here spawns a process (a launched process is exactly what
//! Defender scanned on PP):
//!
//! - [`read_direct`], at each check: the edition, the build and the
//!   firewall's policy and local values from the registry (`RegGetValueW`),
//!   the WinDefend service's state from the service manager, and the #255
//!   audio profile (`prod_env_audio_win.rs`);
//! - [`read_wmi`], behind the bounded reader: Defender's status
//!   (`root\Microsoft\Windows\Defender` `MSFT_MpComputerStatus`), the
//!   firewall's effective state (`root\StandardCimv2`
//!   `MSFT_NetFirewallProfile` in the active policy store) and the registered
//!   antivirus products (`root\SecurityCenter2` `AntiVirusProduct`), over
//!   COM on a thread of its own.
//!
//! Every decision on what was read is `prod_env_read.rs`'s, tested on Linux;
//! this file only reads. Not compiled on the Linux runner, so every fn is
//! `mutants::skip`.

use super::read::{
    AvProduct, DefenderRow, DirectRead, FirewallRow, RegDword, RegString, ServiceRead, WmiRead,
};

/// The Windows version key (`EditionID`, `ProductName`, `CurrentBuildNumber`).
const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

/// The group-policy firewall keys, by profile (domain, private, public).
const POLICY_FIREWALL: [&str; 3] = [
    r"SOFTWARE\Policies\Microsoft\WindowsFirewall\DomainProfile",
    r"SOFTWARE\Policies\Microsoft\WindowsFirewall\PrivateProfile",
    r"SOFTWARE\Policies\Microsoft\WindowsFirewall\PublicProfile",
];

/// The older Administrative-Templates firewall policy, for the private and
/// public profiles.
const LEGACY_POLICY_FIREWALL: &str = r"SOFTWARE\Policies\Microsoft\WindowsFirewall\StandardProfile";

/// The local firewall keys, by profile (the private profile is `Standard`).
const LOCAL_FIREWALL: [&str; 3] = [
    r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\DomainProfile",
    r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\StandardProfile",
    r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\PublicProfile",
];

/// How long WMI may take to hand over the next row, ms.
const WMI_NEXT_TIMEOUT_MS: i32 = 10_000;

/// The most rows a query is read for (a box has 3 firewall profiles and a
/// handful of antivirus products).
const MAX_ROWS: usize = 64;

/// E_FAIL: the WMI thread could not be started or ended in a panic.
const E_FAIL: i32 = 0x8000_4005_u32 as i32;

/// The registry and the service manager, read at each check.
#[cfg_attr(test, mutants::skip)] // Windows glue, not compiled on the Linux runner
pub fn read_direct() -> DirectRead {
    DirectRead {
        edition_id: reg_string(CURRENT_VERSION, "EditionID"),
        product_name: reg_string(CURRENT_VERSION, "ProductName"),
        build: reg_string(CURRENT_VERSION, "CurrentBuildNumber"),
        firewall_policy: POLICY_FIREWALL.map(|key| reg_dword(key, "EnableFirewall")),
        firewall_legacy_policy: reg_dword(LEGACY_POLICY_FIREWALL, "EnableFirewall"),
        firewall_local: LOCAL_FIREWALL.map(|key| reg_dword(key, "EnableFirewall")),
        defender_service: read_service("WinDefend"),
        audio: super::audio::win::read_audio(),
    }
}

/// `s` as a NUL-terminated UTF-16 string.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A `REG_SZ` value under HKLM: `Ok(None)` when the key or value is absent.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn reg_string(subkey: &str, value: &str) -> RegString {
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

/// A service's current state (`SERVICE_STOPPED` = 1, …) from the service
/// manager, or the Win32 error of the first call that failed (a service that
/// does not exist: `ERROR_SERVICE_DOES_NOT_EXIST`).
#[cfg_attr(test, mutants::skip)] // Windows glue
fn read_service(name: &str) -> ServiceRead {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_MANAGER_CONNECT,
        SC_STATUS_PROCESS_INFO, SERVICE_QUERY_STATUS, SERVICE_STATUS_PROCESS,
    };

    let service_name = wide(name);
    // SAFETY: every handle is checked before use and closed once; the name is
    // a NUL-terminated UTF-16 buffer that outlives the calls; `status` is a
    // zeroed POD of the size handed over, written by the query alone.
    unsafe {
        let manager = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if manager.is_null() {
            return ServiceRead::Failed(GetLastError());
        }
        let service = OpenServiceW(manager, service_name.as_ptr(), SERVICE_QUERY_STATUS);
        if service.is_null() {
            let code = GetLastError();
            CloseServiceHandle(manager);
            return ServiceRead::Failed(code);
        }
        let mut status: SERVICE_STATUS_PROCESS = std::mem::zeroed();
        let mut needed: u32 = 0;
        let ok = QueryServiceStatusEx(
            service,
            SC_STATUS_PROCESS_INFO,
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
            std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut needed,
        );
        let read = if ok == 0 {
            ServiceRead::Failed(GetLastError())
        } else {
            ServiceRead::State(status.dwCurrentState)
        };
        CloseServiceHandle(service);
        CloseServiceHandle(manager);
        read
    }
}

/// Every WMI read, on a thread of its own (its own COM apartment, never the
/// blocking pool's). Runs behind the bounded reader (`prod_env::Reader`).
#[cfg_attr(test, mutants::skip)] // Windows glue
pub fn read_wmi() -> WmiRead {
    match std::thread::Builder::new()
        .name("env-wmi".to_string())
        .spawn(wmi_in_own_apartment)
    {
        Ok(thread) => thread.join().unwrap_or_else(|_| WmiRead::failed(E_FAIL)),
        Err(_) => WmiRead::failed(E_FAIL),
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

/// COM in a multithreaded apartment on this thread, then the three queries.
/// The process's COM security is left alone (no `CoInitializeSecurity`: other
/// COM users share the process); each proxy is set to impersonate, the
/// documented way to query WMI without it.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn wmi_in_own_apartment() -> WmiRead {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::System::Wmi::{IWbemLocator, WbemLocator};

    // SAFETY: a fresh thread, initialized once; `ComApartment` balances it.
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if init.is_err() {
        return WmiRead::failed(init.0);
    }
    let _apartment = ComApartment;
    // SAFETY: COM is initialized on this thread.
    let locator: IWbemLocator =
        match unsafe { CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER) } {
            Ok(locator) => locator,
            Err(e) => return WmiRead::failed(e.code().0),
        };
    // Every COM object of a query is dropped when it returns, and the
    // locator before the apartment (declared after it).
    WmiRead {
        defender: query_defender(&locator).map_err(|e| e.code().0),
        firewall: query_firewall(&locator).map_err(|e| e.code().0),
        antivirus: query_antivirus(&locator).map_err(|e| e.code().0),
    }
}

/// Sets `proxy` to impersonate the caller.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn impersonate<P>(proxy: P) -> windows::core::Result<()>
where
    P: windows::core::Param<windows::core::IUnknown>,
{
    use windows::Win32::System::Com::{
        CoSetProxyBlanket, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
    };
    use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
    use windows::core::PCWSTR;

    // SAFETY: `proxy` is a live COM interface; the other arguments are
    // constants or null.
    unsafe {
        CoSetProxyBlanket(
            proxy,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            PCWSTR::null(),
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
    }
}

/// A namespace's services, connected with the bounded wait
/// (`WBEM_FLAG_CONNECT_USE_MAX_WAIT`: ConnectServer gives up after 2 min
/// instead of waiting forever) and set to impersonate.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn connect(
    locator: &windows::Win32::System::Wmi::IWbemLocator,
    namespace: &str,
) -> windows::core::Result<windows::Win32::System::Wmi::IWbemServices> {
    use windows::Win32::System::Wmi::WBEM_FLAG_CONNECT_USE_MAX_WAIT;
    use windows::core::BSTR;

    // SAFETY: COM is initialized on this thread; every argument is a live
    // BSTR or null.
    let services = unsafe {
        locator.ConnectServer(
            &BSTR::from(namespace),
            None,
            None,
            None,
            WBEM_FLAG_CONNECT_USE_MAX_WAIT.0,
            None,
            None,
        )?
    };
    impersonate(&services)?;
    Ok(services)
}

/// Every row of `query` (WQL) in `services`, with `context` (at most
/// `MAX_ROWS`; a row WMI does not hand over within `WMI_NEXT_TIMEOUT_MS` is
/// an error).
#[cfg_attr(test, mutants::skip)] // Windows glue
fn rows(
    services: &windows::Win32::System::Wmi::IWbemServices,
    query: &str,
    context: Option<&windows::Win32::System::Wmi::IWbemContext>,
) -> windows::core::Result<Vec<windows::Win32::System::Wmi::IWbemClassObject>> {
    use windows::Win32::System::Wmi::{
        IWbemClassObject, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_S_TIMEDOUT,
    };
    use windows::core::{BSTR, HRESULT};

    // SAFETY: COM is initialized on this thread; every pointer handed over
    // is a live local.
    unsafe {
        let found = services.ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from(query),
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            context,
        )?;
        // The enumerator is a proxy of its own: impersonate there too. Not
        // every enumerator is a proxy (E_NOINTERFACE then), so a refusal
        // here is no failure.
        let _ = impersonate(&found);
        let mut all = Vec::new();
        for _ in 0..MAX_ROWS {
            let mut row: [Option<IWbemClassObject>; 1] = [None];
            let mut returned: u32 = 0;
            let next = found.Next(WMI_NEXT_TIMEOUT_MS, &mut row, &mut returned);
            next.ok()?;
            if next == HRESULT(WBEM_S_TIMEDOUT.0) {
                return Err(windows::core::Error::from(HRESULT(WBEM_S_TIMEDOUT.0)));
            }
            match row[0].take() {
                Some(object) => all.push(object),
                None => break,
            }
        }
        Ok(all)
    }
}

/// One property of a row (`VT_NULL` when the instance has no value).
#[cfg_attr(test, mutants::skip)] // Windows glue
fn property(
    row: &windows::Win32::System::Wmi::IWbemClassObject,
    name: &str,
) -> windows::core::Result<windows::core::VARIANT> {
    use windows::core::{HSTRING, VARIANT};

    let mut value = VARIANT::default();
    // SAFETY: `row` is a live object; the name and `value` outlive the call.
    unsafe { row.Get(&HSTRING::from(name), 0, &mut value, None, None)? };
    Ok(value)
}

/// A boolean property, `None` with no value.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn bool_of(row: &windows::Win32::System::Wmi::IWbemClassObject, name: &str) -> Option<bool> {
    property(row, name)
        .ok()
        .and_then(|v| bool::try_from(&v).ok())
}

/// An unsigned property, `None` with no value.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn u32_of(row: &windows::Win32::System::Wmi::IWbemClassObject, name: &str) -> Option<u32> {
    property(row, name)
        .ok()
        .and_then(|v| u32::try_from(&v).ok())
}

/// A string property, `None` with no value or an empty one.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn string_of(row: &windows::Win32::System::Wmi::IWbemClassObject, name: &str) -> Option<String> {
    property(row, name)
        .ok()
        .and_then(|v| windows::core::BSTR::try_from(&v).ok())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// Defender's status: its first instance, `None` with none.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn query_defender(
    locator: &windows::Win32::System::Wmi::IWbemLocator,
) -> windows::core::Result<Option<DefenderRow>> {
    let services = connect(locator, r"ROOT\Microsoft\Windows\Defender")?;
    let found = rows(
        &services,
        "SELECT RealTimeProtectionEnabled, AMRunningMode, AntivirusEnabled \
         FROM MSFT_MpComputerStatus",
        None,
    )?;
    Ok(found.first().map(|row| DefenderRow {
        realtime: bool_of(row, "RealTimeProtectionEnabled"),
        mode: string_of(row, "AMRunningMode"),
        antivirus_enabled: bool_of(row, "AntivirusEnabled"),
    }))
}

/// The firewall profiles as they apply now: the active policy store (the
/// local settings merged with every group policy), through the
/// `PolicyStore` context value the NetSecurity provider reads.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn query_firewall(
    locator: &windows::Win32::System::Wmi::IWbemLocator,
) -> windows::core::Result<Vec<FirewallRow>> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::Win32::System::Wmi::{IWbemContext, WbemContext};
    use windows::core::{VARIANT, w};

    let services = connect(locator, r"ROOT\StandardCimv2")?;
    // SAFETY: COM is initialized on this thread; the value outlives the call.
    let context: IWbemContext = unsafe {
        let context: IWbemContext = CoCreateInstance(&WbemContext, None, CLSCTX_INPROC_SERVER)?;
        context.SetValue(w!("PolicyStore"), 0, &VARIANT::from("ActiveStore"))?;
        context
    };
    let found = rows(
        &services,
        "SELECT Name, Enabled FROM MSFT_NetFirewallProfile",
        Some(&context),
    )?;
    Ok(found
        .iter()
        .map(|row| FirewallRow {
            name: string_of(row, "Name").unwrap_or_default(),
            enabled: u32_of(row, "Enabled"),
        })
        .collect())
}

/// The antivirus products registered with the Security Center (a client
/// SKU's; a server has no such namespace).
#[cfg_attr(test, mutants::skip)] // Windows glue
fn query_antivirus(
    locator: &windows::Win32::System::Wmi::IWbemLocator,
) -> windows::core::Result<Vec<AvProduct>> {
    let services = connect(locator, r"ROOT\SecurityCenter2")?;
    let found = rows(
        &services,
        "SELECT displayName, instanceGuid, productState FROM AntiVirusProduct",
        None,
    )?;
    Ok(found
        .iter()
        .map(|row| AvProduct {
            name: string_of(row, "displayName").unwrap_or_default(),
            guid: string_of(row, "instanceGuid").unwrap_or_default(),
            state: u32_of(row, "productState"),
        })
        .collect())
}
