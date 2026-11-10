//! #255 item D (dstack, the owner of the two-network standard, 10.11.2026):
//! a PC with a Dante leg follows dstack's `docs/two-networks.md` §6 items
//! 1–3, checked with the rest of the production environment every minute
//! and gated at every deploy:
//!
//! 1. exactly one physical adapter turned on and connected; every other one
//!    disabled in the OS (a disconnected but enabled one is a problem too:
//!    "disabled in the OS, not just unplugged");
//! 2. exactly one default route (0.0.0.0/0), not through the Dante leg;
//! 3. the Dante leg holds the output's address, has no gateway, interface
//!    metric 900 (fixed, not automatic), no DNS registration, NetBIOS off
//!    (`NetbiosOptions` 2), File and Printer Sharing (`ms_server`) and
//!    Client for Microsoft Networks (`ms_msclient`) unbound.
//!
//! The Dante leg: the interface that holds the `ip` of an enabled `type:
//! dante` audio output (`dante_guard::dante_ips`, the same rule the LAN
//! services' guard uses). No such output (PP today) = nothing judged.
//!
//! Where each fact comes from (nothing spawns a process,
//! `prod_env_network_win.rs` only reads):
//!
//! - the interfaces, their addresses, the default routes and the IPv4
//!   interface metric from IP Helper (`GetIfTable2`,
//!   `GetUnicastIpAddressTable`, `GetIpForwardTable2`,
//!   `GetIpInterfaceTable`): what is live now;
//! - §6.1's physical adapters from WMI (`root\StandardCimv2`
//!   `MSFT_NetAdapter`, what `Get-NetAdapter -Physical` reads):
//!   `ConnectorPresent` and not `Virtual`, turned on = `State` not 3
//!   (disabled), connected = `InterfaceOperationalStatus` 1. Never IP
//!   Helper's rows: a REMOVED adapter keeps its `GetIfTable2` row with the
//!   connector flag, admin up and an `OperStatus` other than "not present"
//!   (resolume-snv's unplugged USB NIC "Dante", 10.11.2026), which
//!   `Get-NetAdapter` never lists;
//! - the leg's configured gateway, DNS registration and NetBIOS from the
//!   registry through [`RegistryRead`] (the values the adapter's TCP/IP
//!   and NetBT properties write: `DefaultGateway` / `DhcpDefaultGateway` by
//!   `EnableDHCP`, `RegistrationEnabled`, `NetbiosOptions`), so the walk is
//!   tested over the in-memory registry;
//! - the bindings from WMI (`root\StandardCimv2`
//!   `MSFT_NetAdapterBindingSettingData`, what `Get-NetAdapterBinding`
//!   reads), in the existing bounded WMI pass. Not the services'
//!   `Linkage\Bind`: that is NetCfg's private store, and #255 lane 1
//!   measured it listing adapters that no longer exist.

use std::net::Ipv4Addr;

use sp_core::prod_env::EnvProblem;
use sp_core::prod_env::network::{
    Binding, DANTE_LEG_METRIC, DanteLegView, NETBIOS_DISABLED, NetProblem, TwoNetworksView,
};

use super::audio::Reading;
use super::audio::read::{IF_FILTER, RegValue, RegistryRead};

/// The IPv4 interfaces' TCP/IP settings (`{GUID}`).
pub const TCPIP_INTERFACES: &str = r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces";
/// NetBT's per-interface settings (`Tcpip_{GUID}`).
pub const NETBT_INTERFACES: &str = r"SYSTEM\CurrentControlSet\Services\NetBT\Parameters\Interfaces";

/// `MIB_IF_ROW2.InterfaceAndOperStatusFlags`'s `ConnectorPresent` bit: what
/// `Get-NetAdapter -Physical` lists.
pub const IF_CONNECTOR_PRESENT: u8 = 0x04;
/// `IfOperStatusUp`.
pub const IF_OPER_UP: i32 = 1;
/// `IfOperStatusNotPresent`: a removed adapter keeps its row.
pub const IF_OPER_NOT_PRESENT: i32 = 6;
/// `NET_IF_ADMIN_STATUS_DOWN`: disabled in the OS.
pub const IF_ADMIN_DOWN: i32 = 2;

/// One network interface as judged here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetIf {
    /// `InterfaceIndex`.
    pub index: u32,
    /// `InterfaceGuid`, braced.
    pub guid: String,
    /// Its alias (the connection's name).
    pub alias: String,
    /// What `Get-NetAdapter -Physical` lists: a connector, no filter module,
    /// present.
    pub physical: bool,
    /// Turned on in the OS.
    pub enabled: bool,
    /// Connected (`OperStatus` up).
    pub up: bool,
}

impl NetIf {
    /// One `GetIfTable2` row: its flags, `OperStatus` and `AdminStatus`.
    pub fn from_row(
        index: u32,
        guid: &str,
        alias: &str,
        flags: u8,
        oper: i32,
        admin: i32,
    ) -> NetIf {
        let connector = flags & IF_CONNECTOR_PRESENT != 0;
        let filter = flags & IF_FILTER != 0;
        NetIf {
            index,
            guid: guid.to_string(),
            alias: alias.to_string(),
            physical: connector && !filter && oper != IF_OPER_NOT_PRESENT,
            enabled: admin != IF_ADMIN_DOWN,
            up: oper == IF_OPER_UP,
        }
    }
}

/// One IPv4 interface's metric (`GetIpInterfaceTable`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpIf {
    pub index: u32,
    pub metric: u32,
    /// `UseAutomaticMetric`.
    pub automatic: bool,
}

/// What IP Helper gave (each list, or its Win32 error).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetRead {
    pub interfaces: Result<Vec<NetIf>, u32>,
    /// Each IPv4 address and the index of the interface holding it.
    pub addresses: Result<Vec<(u32, Ipv4Addr)>, u32>,
    /// The interface index of each IPv4 default route.
    pub default_routes: Result<Vec<u32>, u32>,
    pub ip_interfaces: Result<Vec<IpIf>, u32>,
}

/// One row of `MSFT_NetAdapterBindingSettingData` (WMI).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BindingRow {
    /// The adapter's name (its alias).
    pub name: Option<String>,
    /// `ComponentID` (`ms_server`, `ms_msclient`).
    pub component: Option<String>,
    pub enabled: Option<bool>,
}

/// `MSFT_NetAdapter.State` of an adapter disabled in the OS.
pub const ADAPTER_DISABLED: u32 = 3;
/// `MSFT_NetAdapter.InterfaceOperationalStatus` up.
pub const ADAPTER_OPER_UP: u32 = 1;

/// One row of `MSFT_NetAdapter` (WMI, what `Get-NetAdapter` reads).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdapterRow {
    /// The adapter's name (its alias).
    pub name: Option<String>,
    pub connector_present: Option<bool>,
    /// `Virtual`.
    pub is_virtual: Option<bool>,
    /// `State` (2 started, 3 disabled).
    pub state: Option<u32>,
    /// `InterfaceOperationalStatus` (1 up, 2 down).
    pub oper: Option<u32>,
}

/// One physical adapter as §6.1 judges it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalNic {
    pub name: String,
    /// Turned on in the OS (`State` not disabled).
    pub enabled: bool,
    /// Connected (`InterfaceOperationalStatus` up).
    pub up: bool,
}

/// The physical adapters among `rows`: a connector and not virtual (`Err`:
/// a row that does not say whether it is physical, or a physical one that
/// does not say its name, state or link).
pub fn physical_nics(rows: &[AdapterRow]) -> Result<Vec<PhysicalNic>, String> {
    let mut nics = Vec::new();
    for row in rows {
        let name = row.name.as_deref().unwrap_or("?");
        let (Some(connector), Some(is_virtual)) = (row.connector_present, row.is_virtual) else {
            return Err(format!(
                "the network adapter {name} does not say whether it is physical"
            ));
        };
        if !connector || is_virtual {
            continue;
        }
        let (Some(name), Some(state), Some(oper)) = (row.name.clone(), row.state, row.oper) else {
            return Err(format!(
                "the physical network adapter {name} does not say its name, state or link"
            ));
        };
        nics.push(PhysicalNic {
            name,
            enabled: state != ADAPTER_DISABLED,
            up: oper == ADAPTER_OPER_UP,
        });
    }
    Ok(nics)
}

/// Which interface holds a Dante output's address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Holder {
    /// The addresses or the interfaces could not be read.
    Unread,
    /// No interface holds it.
    Nobody,
    Interface(NetIf),
}

/// One Dante leg as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegFacts {
    pub ip: Ipv4Addr,
    pub holder: Holder,
    /// The gateways configured on it (`EnableDHCP` 1: `DhcpDefaultGateway`,
    /// else `DefaultGateway`).
    pub gateways: Reading<Vec<String>>,
    /// Its metric and whether it is automatic.
    pub metric: Reading<(u32, bool)>,
    /// It registers its addresses in DNS (`RegistrationEnabled` not 0).
    pub dns_registration: Reading<bool>,
    /// `NetbiosOptions` (absent: not set).
    pub netbios: Reading<u32>,
}

/// The two-network facts of one check. The default is a PC with no Dante
/// leg.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkFacts {
    /// The enabled Dante outputs' addresses, each once (`None`: the outputs
    /// could not be read).
    pub dante_ips: Option<Vec<Ipv4Addr>>,
    pub interfaces: Option<Vec<NetIf>>,
    /// §6.1's physical adapters (WMI; `None`: not read).
    pub physical: Option<Vec<PhysicalNic>>,
    pub default_routes: Option<Vec<u32>>,
    pub legs: Vec<LegFacts>,
    /// The bindings (WMI; `None`: not read).
    pub bindings: Option<Vec<BindingRow>>,
    /// Why a fact could not be read.
    pub errors: Vec<String>,
}

impl Default for NetworkFacts {
    fn default() -> Self {
        Self {
            dante_ips: Some(Vec::new()),
            interfaces: None,
            physical: None,
            default_routes: None,
            legs: Vec::new(),
            bindings: None,
            errors: Vec::new(),
        }
    }
}

/// A DWORD reading (another type is unread).
fn dword(value: RegValue, what: &str, errors: &mut Vec<String>) -> Reading<u32> {
    match value {
        RegValue::Dword(n) => Reading::Value(n),
        RegValue::Absent => Reading::Absent,
        RegValue::Failed(code) => {
            errors.push(format!("{what} could not be read (error {code})"));
            Reading::Unread
        }
        RegValue::Text(_) | RegValue::Multi(_) | RegValue::Other(_) => {
            errors.push(format!("{what} is no DWORD"));
            Reading::Unread
        }
    }
}

/// The gateways a value lists (blank entries skipped; absent = none).
fn gateway_list(value: RegValue, what: &str, errors: &mut Vec<String>) -> Reading<Vec<String>> {
    let list = match value {
        RegValue::Absent => Vec::new(),
        RegValue::Multi(list) => list,
        RegValue::Text(one) => vec![one],
        RegValue::Failed(code) => {
            errors.push(format!("{what} could not be read (error {code})"));
            return Reading::Unread;
        }
        RegValue::Dword(_) | RegValue::Other(_) => {
            errors.push(format!("{what} is no list"));
            return Reading::Unread;
        }
    };
    Reading::Value(
        list.into_iter()
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty())
            .collect(),
    )
}

/// The registry values of the leg on `nic`.
fn leg_settings(
    reg: &dyn RegistryRead,
    nic: &NetIf,
    ip: Ipv4Addr,
    errors: &mut Vec<String>,
) -> LegFacts {
    let key = format!(r"{TCPIP_INTERFACES}\{}", nic.guid);
    let name = &nic.alias;
    let dhcp = dword(
        reg.value(&key, "EnableDHCP"),
        &format!("the Dante leg {name}'s EnableDHCP"),
        errors,
    );
    let gateways = match dhcp {
        Reading::Unread => Reading::Unread,
        Reading::Value(1) => gateway_list(
            reg.value(&key, "DhcpDefaultGateway"),
            &format!("the Dante leg {name}'s DhcpDefaultGateway"),
            errors,
        ),
        Reading::Value(_) | Reading::Absent => gateway_list(
            reg.value(&key, "DefaultGateway"),
            &format!("the Dante leg {name}'s DefaultGateway"),
            errors,
        ),
    };
    let dns_registration = match dword(
        reg.value(&key, "RegistrationEnabled"),
        &format!("the Dante leg {name}'s RegistrationEnabled"),
        errors,
    ) {
        Reading::Unread => Reading::Unread,
        Reading::Absent => Reading::Value(true),
        Reading::Value(n) => Reading::Value(n != 0),
    };
    let netbios = dword(
        reg.value(
            &format!(r"{NETBT_INTERFACES}\Tcpip_{}", nic.guid),
            "NetbiosOptions",
        ),
        &format!("the Dante leg {name}'s NetbiosOptions"),
        errors,
    );
    LegFacts {
        ip,
        holder: Holder::Interface(nic.clone()),
        gateways,
        metric: Reading::Unread,
        dns_registration,
        netbios,
    }
}

/// A leg whose interface is not known: nothing of it read.
fn leg_unknown(ip: Ipv4Addr, holder: Holder) -> LegFacts {
    LegFacts {
        ip,
        holder,
        gateways: Reading::Unread,
        metric: Reading::Unread,
        dns_registration: Reading::Unread,
        netbios: Reading::Unread,
    }
}

/// One Dante output's address: the interface that holds it and its values.
fn leg(reg: &dyn RegistryRead, raw: &NetRead, ip: Ipv4Addr, errors: &mut Vec<String>) -> LegFacts {
    let Ok(addresses) = &raw.addresses else {
        return leg_unknown(ip, Holder::Unread);
    };
    let Some(&(index, _)) = addresses.iter().find(|(_, a)| *a == ip) else {
        return leg_unknown(ip, Holder::Nobody);
    };
    let Ok(interfaces) = &raw.interfaces else {
        return leg_unknown(ip, Holder::Unread);
    };
    let Some(nic) = interfaces.iter().find(|i| i.index == index) else {
        errors.push(format!(
            "the interface {index} holding the Dante address {ip} is not listed"
        ));
        return leg_unknown(ip, Holder::Unread);
    };
    let mut leg = leg_settings(reg, nic, ip, errors);
    leg.metric = match &raw.ip_interfaces {
        Ok(rows) => match rows.iter().find(|r| r.index == index) {
            Some(row) => Reading::Value((row.metric, row.automatic)),
            None => {
                errors.push(format!(
                    "the Dante leg {}'s IPv4 interface is not listed",
                    nic.alias
                ));
                Reading::Unread
            }
        },
        Err(_) => Reading::Unread,
    };
    leg
}

/// The two-network facts: the Dante outputs' addresses (each once), the
/// interfaces, the default routes and each leg.
pub fn read(
    reg: &dyn RegistryRead,
    raw: &NetRead,
    dante_ips: &[Ipv4Addr],
    errors: &mut Vec<String>,
) -> NetworkFacts {
    let mut ips: Vec<Ipv4Addr> = Vec::new();
    for ip in dante_ips {
        if !ips.contains(ip) {
            ips.push(*ip);
        }
    }
    let lists = [
        ("the network interfaces", raw.interfaces.as_ref().err()),
        ("the IPv4 addresses", raw.addresses.as_ref().err()),
        ("the routing table", raw.default_routes.as_ref().err()),
        ("the IPv4 interfaces", raw.ip_interfaces.as_ref().err()),
    ];
    for (what, code) in lists {
        if let Some(code) = code {
            errors.push(format!("{what} could not be listed (error {code})"));
        }
    }
    let legs = ips.iter().map(|&ip| leg(reg, raw, ip, errors)).collect();
    NetworkFacts {
        dante_ips: Some(ips),
        interfaces: raw.interfaces.clone().ok(),
        physical: None,
        default_routes: raw.default_routes.clone().ok(),
        legs,
        bindings: None,
        errors: Vec::new(),
    }
}

/// The facts with the bindings WMI read (or why not; said only when there
/// is a leg to judge them for).
pub fn with_bindings(
    facts: &NetworkFacts,
    bindings: Result<Vec<BindingRow>, String>,
    errors: &mut Vec<String>,
) -> NetworkFacts {
    let mut facts = facts.clone();
    facts.bindings = match bindings {
        Ok(rows) => Some(rows),
        Err(why) => {
            if !facts.legs.is_empty() {
                errors.push(why);
            }
            None
        }
    };
    facts
}

/// The facts with §6.1's physical adapters from WMI's `MSFT_NetAdapter`
/// rows (or why not; said only when there is a leg to judge them for).
pub fn with_adapters(
    facts: &NetworkFacts,
    adapters: Result<Vec<AdapterRow>, String>,
    errors: &mut Vec<String>,
) -> NetworkFacts {
    let mut facts = facts.clone();
    facts.physical = match adapters.and_then(|rows| physical_nics(&rows)) {
        Ok(nics) => Some(nics),
        Err(why) => {
            if !facts.legs.is_empty() {
                errors.push(why);
            }
            None
        }
    };
    facts
}

/// An interface's name by its index.
fn name_of(interfaces: Option<&Vec<NetIf>>, index: u32) -> String {
    interfaces
        .and_then(|ifs| ifs.iter().find(|i| i.index == index))
        .map_or_else(|| format!("rozhranie {index}"), |i| i.alias.clone())
}

/// Whether `binding` is bound to the adapter `alias` (`None`: no row says).
pub fn bound(bindings: &[BindingRow], alias: &str, binding: Binding) -> Option<bool> {
    bindings
        .iter()
        .find(|row| {
            row.name
                .as_deref()
                .is_some_and(|n| n.trim().eq_ignore_ascii_case(alias.trim()))
                && row
                    .component
                    .as_deref()
                    .is_some_and(|c| c.trim().eq_ignore_ascii_case(binding.component()))
        })
        .and_then(|row| row.enabled)
}

/// §6.1: exactly one physical adapter turned on, and it is connected.
fn physical_problem(nics: &[NetIf]) -> Option<NetProblem> {
    let enabled: Vec<&NetIf> = nics.iter().filter(|i| i.physical && i.enabled).collect();
    let up = enabled.iter().filter(|i| i.up).count();
    if enabled.len() == 1 && up == 1 {
        return None;
    }
    let enabled = enabled
        .iter()
        .map(|i| {
            if i.up {
                i.alias.clone()
            } else {
                format!("{} (odpojená)", i.alias)
            }
        })
        .collect();
    Some(NetProblem::PhysicalNics { enabled, up })
}

/// §6.2 and §6.3 for one leg, in order: its address, a default route
/// through it, its gateway, metric, DNS registration, NetBIOS, bindings.
fn leg_problems(leg: &LegFacts, f: &NetworkFacts) -> Vec<NetProblem> {
    let nic = match &leg.holder {
        Holder::Unread => return vec![NetProblem::UnreadableInterfaces],
        Holder::Nobody => {
            return vec![NetProblem::LegMissing {
                ip: leg.ip.to_string(),
            }];
        }
        Holder::Interface(nic) => nic,
    };
    let name = nic.alias.clone();
    let mut problems = Vec::new();
    let mut unread = false;
    if f.default_routes
        .as_ref()
        .is_some_and(|routes| routes.contains(&nic.index))
    {
        problems.push(NetProblem::LegDefaultRoute { nic: name.clone() });
    }
    match &leg.gateways {
        Reading::Value(gateways) if !gateways.is_empty() => {
            problems.push(NetProblem::LegGateway {
                nic: name.clone(),
                gateways: gateways.clone(),
            });
        }
        Reading::Value(_) | Reading::Absent => {}
        Reading::Unread => unread = true,
    }
    match leg.metric {
        Reading::Value((metric, automatic)) if automatic || metric != DANTE_LEG_METRIC => {
            problems.push(NetProblem::LegMetric {
                nic: name.clone(),
                metric,
                automatic,
            });
        }
        Reading::Value(_) | Reading::Absent => {}
        Reading::Unread => unread = true,
    }
    match leg.dns_registration {
        Reading::Value(true) => problems.push(NetProblem::LegDnsRegistration { nic: name.clone() }),
        Reading::Value(false) | Reading::Absent => {}
        Reading::Unread => unread = true,
    }
    match leg.netbios {
        Reading::Value(NETBIOS_DISABLED) => {}
        Reading::Value(n) => problems.push(NetProblem::LegNetbios {
            nic: name.clone(),
            value: Some(n),
        }),
        Reading::Absent => problems.push(NetProblem::LegNetbios {
            nic: name.clone(),
            value: None,
        }),
        Reading::Unread => unread = true,
    }
    if unread {
        problems.insert(0, NetProblem::UnreadableLeg { nic: name.clone() });
    }
    let mut bindings_unread = f.bindings.is_none();
    for binding in Binding::ALL {
        match f
            .bindings
            .as_deref()
            .and_then(|rows| bound(rows, &name, binding))
        {
            Some(true) => problems.push(NetProblem::LegBinding {
                nic: name.clone(),
                binding,
            }),
            Some(false) => {}
            None => bindings_unread = true,
        }
    }
    if bindings_unread {
        problems.push(NetProblem::UnreadableBindings { nic: name });
    }
    problems
}

/// Every two-network problem, in order (each once): the outputs unread;
/// else nothing with no Dante leg; else §6.1, §6.2, then each leg.
pub fn problems(f: &NetworkFacts) -> Vec<EnvProblem> {
    let Some(ips) = &f.dante_ips else {
        return vec![EnvProblem::Network(NetProblem::UnreadableDanteOutputs)];
    };
    if ips.is_empty() {
        return Vec::new();
    }
    let mut all = Vec::new();
    match &f.interfaces {
        None => all.push(NetProblem::UnreadableInterfaces),
        Some(nics) => all.extend(physical_problem(nics)),
    }
    match &f.default_routes {
        None => all.push(NetProblem::UnreadableRoutes),
        Some(routes) if routes.len() != 1 => all.push(NetProblem::DefaultRoutes(
            routes
                .iter()
                .map(|&i| name_of(f.interfaces.as_ref(), i))
                .collect(),
        )),
        Some(_) => {}
    }
    for leg in &f.legs {
        all.extend(leg_problems(leg, f));
    }
    let mut once: Vec<NetProblem> = Vec::new();
    for problem in all {
        if !once.contains(&problem) {
            once.push(problem);
        }
    }
    once.into_iter().map(EnvProblem::Network).collect()
}

/// The facts on the status.
pub fn view(f: &NetworkFacts) -> TwoNetworksView {
    let names = |pick: fn(&NetIf) -> bool| {
        f.interfaces.as_ref().map(|nics| {
            nics.iter()
                .filter(|i| pick(i))
                .map(|i| i.alias.clone())
                .collect()
        })
    };
    let legs = f
        .legs
        .iter()
        .map(|leg| {
            let nic = match &leg.holder {
                Holder::Interface(nic) => Some(nic.alias.clone()),
                Holder::Unread | Holder::Nobody => None,
            };
            let binding = |b: Binding| {
                let alias = nic.as_deref()?;
                bound(f.bindings.as_deref()?, alias, b)
            };
            DanteLegView {
                ip: leg.ip.to_string(),
                gateways: leg.gateways.value(),
                metric: leg.metric.value().map(|(m, _)| m),
                automatic_metric: leg.metric.value().map(|(_, a)| a),
                dns_registration: leg.dns_registration.value(),
                netbios_options: leg.netbios.value(),
                file_sharing_bound: binding(Binding::FileSharing),
                client_bound: binding(Binding::Client),
                nic,
            }
        })
        .collect();
    TwoNetworksView {
        dante_ips: f
            .dante_ips
            .as_ref()
            .map(|ips| ips.iter().map(Ipv4Addr::to_string).collect()),
        physical_enabled: names(|i| i.physical && i.enabled),
        physical_up: names(|i| i.physical && i.enabled && i.up),
        default_routes: f.default_routes.as_ref().map(|routes| {
            routes
                .iter()
                .map(|&i| name_of(f.interfaces.as_ref(), i))
                .collect()
        }),
        legs,
    }
}

/// resolume-snv as dstack set it up on 10.11.2026 (the tests' fixture):
/// `Ethernet 4` the one physical adapter on, the only default route; the
/// virtual `Dante70` holding 10.77.7.107 with no gateway, metric 900, no
/// DNS registration, NetBIOS off, sharing and client unbound; `Ethernet` and
/// `Wi-Fi` disabled.
#[cfg(test)]
pub fn snv_example() -> NetworkFacts {
    let nic = |index, guid: &str, alias: &str, physical, enabled, up| NetIf {
        index,
        guid: guid.to_string(),
        alias: alias.to_string(),
        physical,
        enabled,
        up,
    };
    let dante = nic(
        30,
        "{7A1B0000-0000-4000-8000-000000000070}",
        "Dante70",
        false,
        true,
        true,
    );
    let binding = |name: &str, component: &str, enabled| BindingRow {
        name: Some(name.to_string()),
        component: Some(component.to_string()),
        enabled: Some(enabled),
    };
    NetworkFacts {
        dante_ips: Some(vec![Ipv4Addr::new(10, 77, 7, 107)]),
        interfaces: Some(vec![
            nic(
                12,
                "{E4000000-0000-4000-8000-000000000004}",
                "Ethernet 4",
                true,
                true,
                true,
            ),
            nic(
                8,
                "{18000000-0000-4000-8000-000000000008}",
                "Ethernet",
                true,
                false,
                false,
            ),
            nic(
                15,
                "{15000000-0000-4000-8000-000000000015}",
                "Wi-Fi",
                true,
                false,
                false,
            ),
            dante.clone(),
        ]),
        physical: Some(physical_nics(&snv_adapter_rows()).expect("SNV's rows say it all")),
        default_routes: Some(vec![12]),
        legs: vec![LegFacts {
            ip: Ipv4Addr::new(10, 77, 7, 107),
            holder: Holder::Interface(dante),
            gateways: Reading::Value(Vec::new()),
            metric: Reading::Value((DANTE_LEG_METRIC, false)),
            dns_registration: Reading::Value(false),
            netbios: Reading::Value(NETBIOS_DISABLED),
        }],
        bindings: Some(vec![
            binding("Ethernet 4", "ms_server", true),
            binding("Ethernet 4", "ms_msclient", true),
            binding("Dante70", "ms_server", false),
            binding("Dante70", "ms_msclient", false),
        ]),
        errors: Vec::new(),
    }
}

/// resolume-snv's `MSFT_NetAdapter` rows as read live on 10.11.2026
/// (`Name`, `ConnectorPresent`, `Virtual`, `State`,
/// `InterfaceOperationalStatus`): `Ethernet 4` started and up, `Ethernet`
/// and `Wi-Fi` disabled, `Dante70` and Bluetooth PAN virtual. The removed
/// USB NIC "Dante" has no row.
#[cfg(test)]
pub fn snv_adapter_rows() -> Vec<AdapterRow> {
    let row = |name: &str, connector, is_virtual, state, oper| AdapterRow {
        name: Some(name.to_string()),
        connector_present: Some(connector),
        is_virtual: Some(is_virtual),
        state: Some(state),
        oper: Some(oper),
    };
    vec![
        row("Ethernet 4", true, false, 2, 1),
        row("Ethernet", true, false, 3, 2),
        row("Dante70", false, true, 2, 1),
        row("Wi-Fi", true, false, 3, 2),
        row("Bluetooth Network Connection", false, true, 2, 2),
    ]
}

#[cfg(windows)]
#[path = "prod_env_network_win.rs"]
pub(super) mod win;

#[cfg(test)]
#[path = "prod_env_network_tests.rs"]
mod tests;
