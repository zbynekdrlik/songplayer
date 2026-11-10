//! #255 item D (dstack, the owner of the two-network standard, 10.11.2026):
//! a PC with a Dante leg is checked against dstack's `docs/two-networks.md`
//! §6 items 1–3 — exactly one physical network adapter turned on and
//! connected (every other one disabled in the OS, not just unplugged),
//! exactly one default route and not through the Dante leg, and the Dante
//! leg itself: its address, no gateway, interface metric 900 (fixed), no DNS
//! registration, NetBIOS off, File and Printer Sharing and Client for
//! Microsoft Networks unbound.
//!
//! A PC has a Dante leg when an enabled `type: dante` audio output names an
//! `ip`: the leg is the interface that holds it. With no such output (PP
//! today) nothing here is judged. It replaces the report-only note on Dante
//! Virtual Soundcard's adapter: SongPlayer's Dante output is a native device
//! in its own process, DVS is not used.
//!
//! This module names the problems and the status view; sp-server's
//! `prod_env::network` reads the facts and decides.

use serde::{Deserialize, Serialize};

/// The Dante leg's interface metric (§3: "interface metric 900, fixed").
pub const DANTE_LEG_METRIC: u32 = 900;

/// NetBT's `NetbiosOptions` with NetBIOS over TCP/IP off (§3).
pub const NETBIOS_DISABLED: u32 = 2;

/// A network component the Dante leg must have unbound (§3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    /// File and Printer Sharing for Microsoft Networks (`ms_server`).
    FileSharing,
    /// Client for Microsoft Networks (`ms_msclient`).
    Client,
}

impl Binding {
    /// Both, in the order they are judged.
    pub const ALL: [Binding; 2] = [Self::FileSharing, Self::Client];

    /// Its component id (`Get-NetAdapterBinding`'s `ComponentID`), also the
    /// problem code's suffix.
    pub fn component(self) -> &'static str {
        match self {
            Self::FileSharing => "ms_server",
            Self::Client => "ms_msclient",
        }
    }

    /// Its name in Windows' adapter properties.
    pub fn name(self) -> &'static str {
        match self {
            Self::FileSharing => "File and Printer Sharing for Microsoft Networks",
            Self::Client => "Client for Microsoft Networks",
        }
    }
}

/// One way a PC with a Dante leg does not follow §6 items 1–3.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetProblem {
    /// §6.1: not exactly one physical adapter turned on, or that one is not
    /// connected. `enabled`: the physical adapters turned on (a disconnected
    /// one marked so); `up`: how many of them are connected.
    PhysicalNics { enabled: Vec<String>, up: usize },
    /// §6.2: not exactly one default route (the interfaces holding one).
    DefaultRoutes(Vec<String>),
    /// §6.2: a default route goes through the Dante leg.
    LegDefaultRoute { nic: String },
    /// §6.3: no interface holds the Dante output's address.
    LegMissing { ip: String },
    /// §6.3: the leg has a gateway configured.
    LegGateway { nic: String, gateways: Vec<String> },
    /// §6.3: the leg's metric is automatic, or not 900.
    LegMetric {
        nic: String,
        metric: u32,
        automatic: bool,
    },
    /// §6.3: the leg registers its addresses in DNS.
    LegDnsRegistration { nic: String },
    /// §6.3: NetBIOS over TCP/IP is not off on the leg (`NetbiosOptions`
    /// as read; `None`: not set, i.e. the DHCP default).
    LegNetbios { nic: String, value: Option<u32> },
    /// §6.3: a component is bound to the leg.
    LegBinding { nic: String, binding: Binding },
    /// The audio outputs could not be read: whether the PC has a Dante leg
    /// is unknown.
    UnreadableDanteOutputs,
    /// The network interfaces (or their addresses) could not be listed.
    UnreadableInterfaces,
    /// The routing table could not be read.
    UnreadableRoutes,
    /// A value of the Dante leg could not be read (why: the read errors).
    UnreadableLeg { nic: String },
    /// The leg's bindings could not be read.
    UnreadableBindings { nic: String },
}

impl NetProblem {
    /// The stable code.
    pub fn code(&self) -> String {
        match self {
            Self::PhysicalNics { .. } => "two_networks_physical_nics".to_string(),
            Self::DefaultRoutes(_) => "two_networks_default_routes".to_string(),
            Self::LegDefaultRoute { .. } => "dante_leg_default_route".to_string(),
            Self::LegMissing { .. } => "dante_leg_missing".to_string(),
            Self::LegGateway { .. } => "dante_leg_gateway".to_string(),
            Self::LegMetric { .. } => "dante_leg_metric".to_string(),
            Self::LegDnsRegistration { .. } => "dante_leg_dns_registration".to_string(),
            Self::LegNetbios { .. } => "dante_leg_netbios".to_string(),
            Self::LegBinding { binding, .. } => {
                format!("dante_leg_binding:{}", binding.component())
            }
            Self::UnreadableDanteOutputs => "unreadable:dante_outputs".to_string(),
            Self::UnreadableInterfaces => "unreadable:network_interfaces".to_string(),
            Self::UnreadableRoutes => "unreadable:default_routes".to_string(),
            Self::UnreadableLeg { .. } => "unreadable:dante_leg".to_string(),
            Self::UnreadableBindings { .. } => "unreadable:dante_leg_bindings".to_string(),
        }
    }

    /// The problem in Slovak.
    pub fn text_sk(&self) -> String {
        match self {
            Self::PhysicalNics { enabled, up } => format!(
                "počítač so sieťou Dante má mať zapnutú práve jednu fyzickú sieťovú kartu, \
                 pripojenú, a ostatné vypnuté v systéme (nie len odpojené); zapnuté: {}, \
                 pripojené: {up}",
                list_sk(enabled)
            ),
            Self::DefaultRoutes(nics) => format!(
                "počítač so sieťou Dante má mať práve jednu predvolenú trasu (0.0.0.0/0), cez \
                 sieť LAN; má ich {}: {}",
                nics.len(),
                list_sk(nics)
            ),
            Self::LegDefaultRoute { nic } => {
                format!("predvolená trasa ide cez sieť Dante „{nic}“ (má ísť len cez LAN)")
            }
            Self::LegMissing { ip } => {
                format!("adresu výstupu Dante {ip} nemá žiadne sieťové rozhranie tohto počítača")
            }
            Self::LegGateway { nic, gateways } => format!(
                "sieť Dante „{nic}“ má nastavenú bránu {} (nemá mať žiadnu)",
                list_sk(gateways)
            ),
            Self::LegMetric {
                nic,
                metric,
                automatic: true,
            } => format!(
                "sieť Dante „{nic}“ má automatickú metriku rozhrania ({metric}); má byť pevne \
                 {DANTE_LEG_METRIC}"
            ),
            Self::LegMetric { nic, metric, .. } => format!(
                "sieť Dante „{nic}“ má metriku rozhrania {metric} (má byť pevne \
                 {DANTE_LEG_METRIC})"
            ),
            Self::LegDnsRegistration { nic } => {
                format!("sieť Dante „{nic}“ registruje svoje adresy v DNS (má to byť vypnuté)")
            }
            Self::LegNetbios { nic, value } => {
                let value = value.map_or("nenastavené".to_string(), |v| v.to_string());
                format!(
                    "sieť Dante „{nic}“ nemá vypnutý NetBIOS cez TCP/IP (NetbiosOptions \
                     {value}, má byť {NETBIOS_DISABLED})"
                )
            }
            Self::LegBinding { nic, binding } => format!(
                "na sieti Dante „{nic}“ je zapnuté „{}“ (má byť vypnuté)",
                binding.name()
            ),
            Self::UnreadableDanteOutputs => {
                "zvukové výstupy sa nepodarilo prečítať (nevie sa, či má počítač sieť Dante)"
                    .to_string()
            }
            Self::UnreadableInterfaces => {
                "sieťové rozhrania a ich adresy sa nepodarilo zistiť".to_string()
            }
            Self::UnreadableRoutes => "predvolené trasy sa nepodarilo zistiť".to_string(),
            Self::UnreadableLeg { nic } => {
                format!("nastavenia siete Dante „{nic}“ sa nepodarilo zistiť")
            }
            Self::UnreadableBindings { nic } => {
                format!("väzby siete Dante „{nic}“ (zdieľanie, klient) sa nepodarilo zistiť")
            }
        }
    }
}

/// A list of names for a Slovak text (`žiadne` for none).
pub fn list_sk(names: &[String]) -> String {
    if names.is_empty() {
        "žiadne".to_string()
    } else {
        names.join(", ")
    }
}

/// One Dante leg on the status (`None`: not read, or no interface holds the
/// address).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DanteLegView {
    /// The Dante output's address.
    #[serde(default)]
    pub ip: String,
    /// The interface that holds it (its alias).
    #[serde(default)]
    pub nic: Option<String>,
    /// The gateways configured on it (`[]`: none).
    #[serde(default)]
    pub gateways: Option<Vec<String>>,
    /// Its IPv4 interface metric.
    #[serde(default)]
    pub metric: Option<u32>,
    /// The metric is automatic.
    #[serde(default)]
    pub automatic_metric: Option<bool>,
    /// It registers its addresses in DNS.
    #[serde(default)]
    pub dns_registration: Option<bool>,
    /// NetBT's `NetbiosOptions` (`None`: not set or not read).
    #[serde(default)]
    pub netbios_options: Option<u32>,
    /// File and Printer Sharing is bound to it.
    #[serde(default)]
    pub file_sharing_bound: Option<bool>,
    /// Client for Microsoft Networks is bound to it.
    #[serde(default)]
    pub client_bound: Option<bool>,
}

/// The two-network facts on the status (#255 item D).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TwoNetworksView {
    /// The enabled Dante outputs' addresses (`[]`: no Dante leg, nothing
    /// judged; `None`: the outputs could not be read).
    #[serde(default)]
    pub dante_ips: Option<Vec<String>>,
    /// The physical adapters turned on (`None`: not listed).
    #[serde(default)]
    pub physical_enabled: Option<Vec<String>>,
    /// The physical adapters connected.
    #[serde(default)]
    pub physical_up: Option<Vec<String>>,
    /// The interface of each default route (`None`: not read).
    #[serde(default)]
    pub default_routes: Option<Vec<String>>,
    /// Each Dante leg, in the order of the outputs.
    #[serde(default)]
    pub legs: Vec<DanteLegView>,
}

#[cfg(test)]
#[path = "prod_env_network_tests.rs"]
mod tests;
