//! #324: who holds a UDP port, on Windows: the IP Helper's UDP table with
//! its owners (`GetExtendedUdpTable`, `UDP_TABLE_OWNER_PID`) and the owner's
//! image name (`QueryFullProcessImageNameW`). No process is spawned. Not
//! compiled on the Linux runner, so every fn is `mutants::skip`; the row
//! decoding and the choice of the holder are `dante_slots.rs`'s, tested on
//! Linux.

use std::net::Ipv4Addr;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedUdpTable, MIB_UDPROW_OWNER_PID, MIB_UDPTABLE_OWNER_PID, UDP_TABLE_OWNER_PID,
};
use windows_sys::Win32::Networking::WinSock::AF_INET;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

use super::dante_slots::{UdpRow, holder_of, image_name, row_addr, row_port};

/// The longest image path read, in UTF-16 units.
const PATH_UNITS: usize = 1024;

/// The holder of UDP `port` against a device on `ip`: its pid and image
/// name (`"pid N"`'s image unknown when the process cannot be opened).
#[cfg_attr(test, mutants::skip)] // Windows glue
pub(super) fn port_holder(ip: Ipv4Addr, port: u16) -> Option<(u32, String)> {
    let rows = udp_rows()?;
    let pid = holder_of(&rows, ip, port)?;
    let image = image_of(pid).unwrap_or_else(|| "an unknown process".to_string());
    Some((pid, image))
}

/// Every IPv4 UDP endpoint of the PC with its owner; `None` when the table
/// cannot be read.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn udp_rows() -> Option<Vec<UdpRow>> {
    let mut size: u32 = 0;
    let mut buf: Vec<u64> = Vec::new();
    // Three tries at most: the size asked, then the table (a table that grew in
    // between is asked once more).
    for _ in 0..3 {
        // SAFETY: `buf` holds at least `size` bytes (u64s keep the table's
        // alignment); the call writes at most that many and sets `size`.
        let rc = unsafe {
            GetExtendedUdpTable(
                buf.as_mut_ptr().cast(),
                &mut size,
                0,
                u32::from(AF_INET),
                UDP_TABLE_OWNER_PID,
                0,
            )
        };
        if rc == ERROR_INSUFFICIENT_BUFFER {
            buf = vec![0_u64; (size as usize).div_ceil(8)];
            continue;
        }
        if rc != NO_ERROR || buf.is_empty() {
            return None;
        }
        let table = buf.as_ptr().cast::<MIB_UDPTABLE_OWNER_PID>();
        // SAFETY: the call wrote a table of `dwNumEntries` rows from its
        // `table` field on (a C flexible array) inside `buf`.
        let rows = unsafe {
            let count = (*table).dwNumEntries as usize;
            let first = std::ptr::addr_of!((*table).table).cast::<MIB_UDPROW_OWNER_PID>();
            std::slice::from_raw_parts(first, count)
        };
        return Some(
            rows.iter()
                .map(|r| UdpRow {
                    addr: row_addr(r.dwLocalAddr),
                    port: row_port(r.dwLocalPort),
                    pid: r.dwOwningPid,
                })
                .collect(),
        );
    }
    None
}

/// The image file name of process `pid`, when it can be opened.
#[cfg_attr(test, mutants::skip)] // Windows glue
fn image_of(pid: u32) -> Option<String> {
    // SAFETY: opens the process for a query only; closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut path = [0_u16; PATH_UNITS];
    let mut len = PATH_UNITS as u32;
    // SAFETY: `path` holds `len` units; the call writes at most that many.
    let read = unsafe {
        QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut len)
    };
    // SAFETY: closes the handle opened above, once.
    unsafe { CloseHandle(handle) };
    if read == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&path[..(len as usize).min(PATH_UNITS)]);
    Some(image_name(&path).to_string())
}
