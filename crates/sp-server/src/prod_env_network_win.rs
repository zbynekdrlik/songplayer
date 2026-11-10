//! #255 item D: the two-network facts' raw reads on Windows (IP Helper:
//! `GetIfTable2`, `GetUnicastIpAddressTable`, `GetIpForwardTable2`,
//! `GetIpInterfaceTable`, all IPv4). Nothing here spawns a process; every
//! decision is `prod_env_network.rs`'s, tested on Linux. Not compiled on the
//! Linux runner, so every fn is `mutants::skip`.

use std::net::Ipv4Addr;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, GetIpForwardTable2, GetIpInterfaceTable, GetUnicastIpAddressTable,
    MIB_IF_ROW2, MIB_IF_TABLE2, MIB_IPFORWARD_ROW2, MIB_IPFORWARD_TABLE2, MIB_IPINTERFACE_ROW,
    MIB_IPINTERFACE_TABLE, MIB_UNICASTIPADDRESS_ROW, MIB_UNICASTIPADDRESS_TABLE,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_INET};

use super::super::audio::read::guid_text;
use super::super::audio::win::WinRegistry;
use super::{IpIf, NetIf, NetRead, NetworkFacts, read};

/// The two-network facts of one check, for the Dante outputs' addresses
/// `dante_ips` (`prod_env::read_direct`).
#[cfg_attr(test, mutants::skip)] // Windows glue, not compiled on the Linux runner
pub fn read_network(dante_ips: &[Ipv4Addr]) -> NetworkFacts {
    let raw = NetRead {
        interfaces: interfaces(),
        addresses: addresses(),
        default_routes: default_routes(),
        ip_interfaces: ip_interfaces(),
    };
    let mut errors = Vec::new();
    let mut facts = read(&WinRegistry, &raw, dante_ips, &mut errors);
    facts.errors = errors;
    facts
}

/// The rows of an IP Helper table `$table` (a `*mut` to a
/// `{ NumEntries, Table: [$row; 1] }` IP Helper allocated), each copied out
/// by `$each`, then the table freed once.
macro_rules! table_rows {
    ($table:expr, $row:ty, $each:expr) => {{
        let table = $table;
        if table.is_null() {
            Vec::new()
        } else {
            // SAFETY: the table holds `NumEntries` rows from its `Table`
            // field on (a C flexible array); they are copied out before the
            // table is freed.
            let rows: Vec<_> = unsafe {
                let count = (*table).NumEntries as usize;
                let first = std::ptr::addr_of!((*table).Table).cast::<$row>();
                std::slice::from_raw_parts(first, count)
                    .iter()
                    .map($each)
                    .collect()
            };
            // SAFETY: frees the table IP Helper allocated, once.
            unsafe { FreeMibTable(table as *const core::ffi::c_void) };
            rows
        }
    }};
}

/// An IPv4 `SOCKADDR_INET` as an address (`None` for another family).
#[cfg_attr(test, mutants::skip)] // Windows glue
fn ipv4(addr: &SOCKADDR_INET) -> Option<Ipv4Addr> {
    // SAFETY: `si_family` overlays the family field of every member.
    if unsafe { addr.si_family } != AF_INET {
        return None;
    }
    // SAFETY: the family says the `Ipv4` member is the one written; its
    // address is in network order, so its bytes are a.b.c.d.
    let raw = unsafe { addr.Ipv4.sin_addr.S_un.S_addr };
    Some(Ipv4Addr::from(raw.to_ne_bytes()))
}

/// Every interface (`GetIfTable2`).
#[cfg_attr(test, mutants::skip)] // Windows glue
fn interfaces() -> Result<Vec<NetIf>, u32> {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    // SAFETY: the call allocates the table and writes its address.
    let rc = unsafe { GetIfTable2(&mut table) };
    if rc != ERROR_SUCCESS {
        return Err(rc);
    }
    Ok(table_rows!(table, MIB_IF_ROW2, |row: &MIB_IF_ROW2| {
        let g = row.InterfaceGuid;
        let alias_len = row
            .Alias
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(row.Alias.len());
        NetIf::from_row(
            row.InterfaceIndex,
            &format!(
                "{{{}}}",
                guid_text(g.data1, g.data2, g.data3, g.data4).to_ascii_uppercase()
            ),
            &String::from_utf16_lossy(&row.Alias[..alias_len]),
            row.InterfaceAndOperStatusFlags._bitfield,
            row.OperStatus,
            row.AdminStatus,
        )
    }))
}

/// Every IPv4 unicast address and its interface (`GetUnicastIpAddressTable`).
#[cfg_attr(test, mutants::skip)] // Windows glue
fn addresses() -> Result<Vec<(u32, Ipv4Addr)>, u32> {
    let mut table: *mut MIB_UNICASTIPADDRESS_TABLE = std::ptr::null_mut();
    // SAFETY: the call allocates the table and writes its address.
    let rc = unsafe { GetUnicastIpAddressTable(AF_INET, &mut table) };
    if rc != ERROR_SUCCESS {
        return Err(rc);
    }
    let rows: Vec<Option<(u32, Ipv4Addr)>> = table_rows!(
        table,
        MIB_UNICASTIPADDRESS_ROW,
        |row: &MIB_UNICASTIPADDRESS_ROW| { ipv4(&row.Address).map(|ip| (row.InterfaceIndex, ip)) }
    );
    Ok(rows.into_iter().flatten().collect())
}

/// The interface of every IPv4 default route (`GetIpForwardTable2`, prefix
/// length 0).
#[cfg_attr(test, mutants::skip)] // Windows glue
fn default_routes() -> Result<Vec<u32>, u32> {
    let mut table: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
    // SAFETY: the call allocates the table and writes its address.
    let rc = unsafe { GetIpForwardTable2(AF_INET, &mut table) };
    if rc != ERROR_SUCCESS {
        return Err(rc);
    }
    let rows: Vec<Option<u32>> =
        table_rows!(table, MIB_IPFORWARD_ROW2, |row: &MIB_IPFORWARD_ROW2| {
            (row.DestinationPrefix.PrefixLength == 0
                && ipv4(&row.DestinationPrefix.Prefix) == Some(Ipv4Addr::UNSPECIFIED))
            .then_some(row.InterfaceIndex)
        });
    Ok(rows.into_iter().flatten().collect())
}

/// Every IPv4 interface's metric (`GetIpInterfaceTable`).
#[cfg_attr(test, mutants::skip)] // Windows glue
fn ip_interfaces() -> Result<Vec<IpIf>, u32> {
    let mut table: *mut MIB_IPINTERFACE_TABLE = std::ptr::null_mut();
    // SAFETY: the call allocates the table and writes its address.
    let rc = unsafe { GetIpInterfaceTable(AF_INET, &mut table) };
    if rc != ERROR_SUCCESS {
        return Err(rc);
    }
    Ok(table_rows!(
        table,
        MIB_IPINTERFACE_ROW,
        |row: &MIB_IPINTERFACE_ROW| IpIf {
            index: row.InterfaceIndex,
            metric: row.Metric,
            automatic: row.UseAutomaticMetric != 0,
        }
    ))
}
