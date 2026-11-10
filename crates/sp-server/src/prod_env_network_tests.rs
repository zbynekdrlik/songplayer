//! #255 item D: dstack's two-network standard §6 items 1–3 over
//! resolume-snv's layout as read on 10.11.2026 (physical `Ethernet 4` up on
//! the LAN, `Ethernet` and `Wi-Fi` disabled, the virtual `Dante70` holding
//! 10.77.7.107 with no gateway, metric 900, no DNS registration, NetBIOS off,
//! sharing and client unbound; `Bluetooth Network Connection` virtual and
//! disconnected), each departure from it, and what cannot be read.

use std::net::Ipv4Addr;

use sp_core::prod_env::EnvProblem;
use sp_core::prod_env::network::{Binding, DanteLegView, NetProblem, TwoNetworksView};

use super::super::audio::Reading;
use super::super::audio::read::RegValue;
use super::super::audio::read::fake::Fake;
use super::{
    ADAPTER_DISABLED, ADAPTER_OPER_UP, AdapterRow, BindingRow, Holder, IF_ADMIN_DOWN,
    IF_CONNECTOR_PRESENT, IF_OPER_NOT_PRESENT, IF_OPER_UP, IpIf, LegFacts, NETBT_INTERFACES, NetIf,
    NetRead, NetworkFacts, PhysicalNic, TCPIP_INTERFACES, bound, physical_nics, problems, read,
    snv_adapter_rows, view, with_adapters, with_bindings,
};

const DANTE: Ipv4Addr = Ipv4Addr::new(10, 77, 7, 107);
const LAN: Ipv4Addr = Ipv4Addr::new(10, 77, 9, 201);
const DANTE_GUID: &str = "{7A1B0000-0000-4000-8000-000000000070}";
const LAN_GUID: &str = "{E4000000-0000-4000-8000-000000000004}";

/// Hardware + connector (what a physical adapter's row has).
const PHYSICAL: u8 = 0x01 | IF_CONNECTOR_PRESENT;
/// Hardware, no connector (Realtek's VLAN adapter, Bluetooth PAN).
const VIRTUAL: u8 = 0x01;
/// `IfOperStatusDown`, `IfOperStatusLowerLayerDown`.
const OPER_DOWN: i32 = 2;
const OPER_LOWER_DOWN: i32 = 7;
/// `NET_IF_ADMIN_STATUS_UP`.
const ADMIN_UP: i32 = 1;

fn nic(index: u32, guid: &str, alias: &str, flags: u8, oper: i32, admin: i32) -> NetIf {
    NetIf::from_row(index, guid, alias, flags, oper, admin)
}

/// resolume-snv's interfaces (with `Ethernet 4`'s filter module row and two
/// removed adapters that keep their rows: "Ethernet 2", not present, and the
/// unplugged USB NIC "Dante", `USB\VID_0BDA&PID_8153\0013000001`, whose
/// row still has the connector flag, admin up and `OperStatus` down).
fn snv_interfaces() -> Vec<NetIf> {
    vec![
        nic(12, LAN_GUID, "Ethernet 4", PHYSICAL, IF_OPER_UP, ADMIN_UP),
        nic(
            13,
            "{F0000000-0000-4000-8000-000000000012}",
            "Ethernet 4-WFP Native MAC Layer LightWeight Filter-0000",
            PHYSICAL | 0x02,
            IF_OPER_UP,
            ADMIN_UP,
        ),
        nic(
            8,
            "{18000000-0000-4000-8000-000000000008}",
            "Ethernet",
            PHYSICAL,
            OPER_DOWN,
            IF_ADMIN_DOWN,
        ),
        nic(
            15,
            "{15000000-0000-4000-8000-000000000015}",
            "Wi-Fi",
            PHYSICAL,
            OPER_DOWN,
            IF_ADMIN_DOWN,
        ),
        nic(30, DANTE_GUID, "Dante70", VIRTUAL, IF_OPER_UP, ADMIN_UP),
        nic(
            20,
            "{20000000-0000-4000-8000-000000000020}",
            "Bluetooth Network Connection",
            VIRTUAL,
            OPER_LOWER_DOWN,
            ADMIN_UP,
        ),
        nic(
            9,
            "{90000000-0000-4000-8000-000000000009}",
            "Ethernet 2",
            PHYSICAL,
            IF_OPER_NOT_PRESENT,
            ADMIN_UP,
        ),
        ghost_dante(),
    ]
}

/// The removed USB NIC "Dante"'s stale `GetIfTable2` row.
fn ghost_dante() -> NetIf {
    nic(
        33,
        "{D4000000-0000-4000-8000-000000000033}",
        "Dante",
        PHYSICAL,
        OPER_DOWN,
        ADMIN_UP,
    )
}

fn snv_raw() -> NetRead {
    NetRead {
        interfaces: Ok(snv_interfaces()),
        addresses: Ok(vec![(1, Ipv4Addr::LOCALHOST), (12, LAN), (30, DANTE)]),
        default_routes: Ok(vec![12]),
        ip_interfaces: Ok(vec![
            IpIf {
                index: 12,
                metric: 25,
                automatic: true,
            },
            IpIf {
                index: 30,
                metric: 900,
                automatic: false,
            },
        ]),
    }
}

fn tcpip(guid: &str) -> String {
    format!(r"{TCPIP_INTERFACES}\{guid}")
}

fn netbt(guid: &str) -> String {
    format!(r"{NETBT_INTERFACES}\Tcpip_{guid}")
}

/// resolume-snv's registry for its two legs: `Dante70` static, no gateway,
/// no DNS registration, NetBIOS off; `Ethernet 4` on DHCP with its gateway.
fn snv_registry() -> Fake {
    let mut reg = Fake::default();
    reg.set(&tcpip(DANTE_GUID), "EnableDHCP", RegValue::Dword(0));
    reg.set(
        &tcpip(DANTE_GUID),
        "DefaultGateway",
        RegValue::Multi(Vec::new()),
    );
    // A DHCP gateway left from before the leg was static is not used.
    reg.set(
        &tcpip(DANTE_GUID),
        "DhcpDefaultGateway",
        RegValue::Multi(vec!["10.77.7.1".to_string()]),
    );
    reg.set(
        &tcpip(DANTE_GUID),
        "RegistrationEnabled",
        RegValue::Dword(0),
    );
    reg.set(&netbt(DANTE_GUID), "NetbiosOptions", RegValue::Dword(2));
    reg.set(&tcpip(LAN_GUID), "EnableDHCP", RegValue::Dword(1));
    reg.set(
        &tcpip(LAN_GUID),
        "DhcpDefaultGateway",
        RegValue::Multi(vec!["10.77.8.1".to_string()]),
    );
    reg
}

fn binding(name: &str, component: &str, enabled: Option<bool>) -> BindingRow {
    BindingRow {
        name: Some(name.to_string()),
        component: Some(component.to_string()),
        enabled,
    }
}

/// dstack unbound sharing and the client on `Dante70` only.
fn snv_bindings() -> Vec<BindingRow> {
    vec![
        binding("Ethernet 4", "ms_server", Some(true)),
        binding("Ethernet 4", "ms_msclient", Some(true)),
        binding("Dante70", "ms_server", Some(false)),
        binding("Dante70", "ms_msclient", Some(false)),
    ]
}

/// The facts of a check on `raw` + `reg` with SNV's bindings and adapters,
/// and the errors.
fn check(reg: &Fake, raw: &NetRead, ips: &[Ipv4Addr]) -> (NetworkFacts, Vec<String>) {
    check_with(reg, raw, ips, snv_adapter_rows())
}

/// The same with these `MSFT_NetAdapter` rows.
fn check_with(
    reg: &Fake,
    raw: &NetRead,
    ips: &[Ipv4Addr],
    adapters: Vec<AdapterRow>,
) -> (NetworkFacts, Vec<String>) {
    let mut errors = Vec::new();
    let facts = read(reg, raw, ips, &mut errors);
    let facts = with_bindings(&facts, Ok(snv_bindings()), &mut errors);
    let facts = with_adapters(&facts, Ok(adapters), &mut errors);
    (facts, errors)
}

/// One `MSFT_NetAdapter` row of a physical adapter.
fn adapter(name: &str, state: u32, oper: u32) -> AdapterRow {
    AdapterRow {
        name: Some(name.to_string()),
        connector_present: Some(true),
        is_virtual: Some(false),
        state: Some(state),
        oper: Some(oper),
    }
}

/// `MSFT_NetAdapter.State` started, `InterfaceOperationalStatus` down.
const STARTED: u32 = 2;
const DOWN: u32 = 2;

fn snv() -> NetworkFacts {
    let (facts, errors) = check(&snv_registry(), &snv_raw(), &[DANTE]);
    assert!(errors.is_empty(), "{errors:?}");
    facts
}

fn codes(facts: &NetworkFacts) -> Vec<String> {
    problems(facts).iter().map(EnvProblem::code).collect()
}

fn net(problems: Vec<EnvProblem>) -> Vec<NetProblem> {
    problems
        .into_iter()
        .map(|p| match p {
            EnvProblem::Network(p) => p,
            other => panic!("not a two-network problem: {other:?}"),
        })
        .collect()
}

fn s(v: &str) -> String {
    v.to_string()
}

#[test]
fn an_interface_row_is_physical_by_its_connector_and_never_a_filter_s() {
    assert_eq!(
        (
            IF_CONNECTOR_PRESENT,
            IF_OPER_UP,
            IF_OPER_NOT_PRESENT,
            IF_ADMIN_DOWN
        ),
        (0x04, 1, 6, 2)
    );
    let row = |flags, oper, admin| nic(1, "{G}", "x", flags, oper, admin);
    let up = row(PHYSICAL, IF_OPER_UP, ADMIN_UP);
    assert_eq!(
        (up.physical, up.enabled, up.up),
        (true, true, true),
        "a connected physical adapter"
    );
    assert_eq!(
        (up.index, up.guid.as_str(), up.alias.as_str()),
        (1, "{G}", "x")
    );
    assert!(row(IF_CONNECTOR_PRESENT, IF_OPER_UP, ADMIN_UP).physical);
    assert!(!row(VIRTUAL, IF_OPER_UP, ADMIN_UP).physical, "no connector");
    assert!(!row(0, IF_OPER_UP, ADMIN_UP).physical, "no flag at all");
    assert!(
        !row(PHYSICAL | 0x02, IF_OPER_UP, ADMIN_UP).physical,
        "a filter"
    );
    assert!(
        !row(PHYSICAL, IF_OPER_NOT_PRESENT, ADMIN_UP).physical,
        "a removed adapter"
    );
    let disabled = row(PHYSICAL, OPER_DOWN, IF_ADMIN_DOWN);
    assert_eq!(
        (disabled.physical, disabled.enabled, disabled.up),
        (true, false, false)
    );
    let unplugged = row(PHYSICAL, OPER_DOWN, ADMIN_UP);
    assert_eq!((unplugged.enabled, unplugged.up), (true, false));
    assert!(!row(PHYSICAL, OPER_LOWER_DOWN, ADMIN_UP).up);
}

#[test]
fn snv_as_dstack_set_it_up_holds() {
    let facts = snv();
    assert_eq!(facts.dante_ips, Some(vec![DANTE]));
    assert_eq!(facts.default_routes, Some(vec![12]));
    assert_eq!(facts.legs.len(), 1);
    let leg = &facts.legs[0];
    assert_eq!(
        *leg,
        LegFacts {
            ip: DANTE,
            holder: Holder::Interface(snv_interfaces()[4].clone()),
            gateways: Reading::Value(Vec::new()),
            metric: Reading::Value((900, false)),
            dns_registration: Reading::Value(false),
            netbios: Reading::Value(2),
        }
    );
    assert_eq!(problems(&facts), Vec::<EnvProblem>::new());
}

/// resolume-snv, 10.11.2026: the gate read `two_networks_physical_nics`
/// (enabled: "Dante (odpojená)", "Ethernet 4") off the removed USB NIC's
/// stale `GetIfTable2` row (connector flag, admin up, `OperStatus` down).
/// Windows' own view, `MSFT_NetAdapter` / `Get-NetAdapter -Physical`, has
/// no such adapter: `Ethernet 4` is the one physical adapter on.
#[test]
fn a_removed_nic_s_stale_interface_row_is_no_enabled_physical_nic() {
    let raw = snv_raw();
    assert!(
        raw.interfaces.as_ref().unwrap().contains(&ghost_dante()),
        "the box's interfaces hold the ghost's row"
    );
    let (facts, errors) = check(&snv_registry(), &raw, &[DANTE]);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(problems(&facts), Vec::<EnvProblem>::new());
    let shown = view(&facts);
    assert_eq!(shown.physical_enabled, Some(vec![s("Ethernet 4")]));
    assert_eq!(shown.physical_up, Some(vec![s("Ethernet 4")]));
}

#[test]
fn snv_s_view_names_every_fact() {
    assert_eq!(
        view(&snv()),
        TwoNetworksView {
            dante_ips: Some(vec![s("10.77.7.107")]),
            physical_enabled: Some(vec![s("Ethernet 4")]),
            physical_up: Some(vec![s("Ethernet 4")]),
            default_routes: Some(vec![s("Ethernet 4")]),
            legs: vec![DanteLegView {
                ip: s("10.77.7.107"),
                nic: Some(s("Dante70")),
                gateways: Some(Vec::new()),
                metric: Some(900),
                automatic_metric: Some(false),
                dns_registration: Some(false),
                netbios_options: Some(2),
                file_sharing_bound: Some(false),
                client_bound: Some(false),
            }],
        }
    );
}

/// PP today: no enabled Dante output, so nothing is judged, whatever its
/// adapters and routes are; the view still lists them.
#[test]
fn a_pc_with_no_dante_leg_is_not_judged() {
    let mut raw = snv_raw();
    raw.default_routes = Ok(vec![12, 30]);
    let (facts, errors) = check(&Fake::default(), &raw, &[]);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(facts.dante_ips, Some(Vec::new()));
    assert!(facts.legs.is_empty());
    assert_eq!(problems(&facts), Vec::<EnvProblem>::new());
    let shown = view(&facts);
    assert_eq!(shown.dante_ips, Some(Vec::new()));
    assert_eq!(
        shown.default_routes,
        Some(vec![s("Ethernet 4"), s("Dante70")])
    );
    assert!(shown.legs.is_empty());
    // The default facts are such a PC.
    assert_eq!(problems(&NetworkFacts::default()), Vec::<EnvProblem>::new());
    // A bindings read that failed is said only when there is a leg.
    let mut errors = Vec::new();
    let no_bindings = with_bindings(&facts, Err(s("WMI down")), &mut errors);
    assert_eq!(no_bindings.bindings, None);
    assert!(errors.is_empty());
}

#[test]
fn the_audio_outputs_unread_is_one_problem() {
    let facts = NetworkFacts {
        dante_ips: None,
        ..snv()
    };
    assert_eq!(codes(&facts), ["unreadable:dante_outputs"]);
    assert_eq!(view(&facts).dante_ips, None);
}

#[test]
fn an_output_listed_twice_is_one_leg() {
    let (facts, _) = check(&snv_registry(), &snv_raw(), &[DANTE, DANTE]);
    assert_eq!(facts.dante_ips, Some(vec![DANTE]));
    assert_eq!(facts.legs.len(), 1);
}

/// §6.1 (from `MSFT_NetAdapter`): a second physical adapter turned on, even
/// unplugged, fails; so does the only one when it is not connected, and
/// none at all. Turned on = `State` not 3, whatever the link.
#[test]
fn exactly_one_physical_adapter_is_on_and_connected() {
    let with = |second: AdapterRow| {
        let mut rows = snv_adapter_rows();
        rows[1] = second;
        check_with(&snv_registry(), &snv_raw(), &[DANTE], rows).0
    };
    let facts = with(adapter("Ethernet", STARTED, DOWN));
    assert_eq!(
        net(problems(&facts)),
        [NetProblem::PhysicalNics {
            enabled: vec![s("Ethernet 4"), s("Ethernet (odpojená)")],
            up: 1,
        }]
    );
    assert_eq!(
        view(&facts).physical_enabled,
        Some(vec![s("Ethernet 4"), s("Ethernet")])
    );
    assert_eq!(view(&facts).physical_up, Some(vec![s("Ethernet 4")]));
    // Unknown (0) and present (1) count as on.
    for state in [0, 1] {
        let facts = with(adapter("Ethernet", state, DOWN));
        assert_eq!(
            view(&facts).physical_enabled,
            Some(vec![s("Ethernet 4"), s("Ethernet")]),
            "State {state}"
        );
    }
    // Two connected.
    let facts = with(adapter("Ethernet", STARTED, ADAPTER_OPER_UP));
    assert_eq!(
        net(problems(&facts)),
        [NetProblem::PhysicalNics {
            enabled: vec![s("Ethernet 4"), s("Ethernet")],
            up: 2,
        }]
    );
    // A disabled one that still reads up is off.
    let facts = with(adapter("Ethernet", ADAPTER_DISABLED, ADAPTER_OPER_UP));
    assert_eq!(problems(&facts), Vec::<EnvProblem>::new());
    assert_eq!(view(&facts).physical_up, Some(vec![s("Ethernet 4")]));
    // The only one on is unplugged.
    let mut alone = snv_adapter_rows();
    alone[0] = adapter("Ethernet 4", STARTED, DOWN);
    let (facts, _) = check_with(&snv_registry(), &snv_raw(), &[DANTE], alone);
    assert_eq!(
        net(problems(&facts))[0],
        NetProblem::PhysicalNics {
            enabled: vec![s("Ethernet 4 (odpojená)")],
            up: 0,
        }
    );
    assert_eq!(view(&facts).physical_up, Some(Vec::new()));
    // None on.
    let mut none = snv_adapter_rows();
    none[0] = adapter("Ethernet 4", ADAPTER_DISABLED, DOWN);
    let (facts, _) = check_with(&snv_registry(), &snv_raw(), &[DANTE], none);
    assert_eq!(
        net(problems(&facts))[0],
        NetProblem::PhysicalNics {
            enabled: Vec::new(),
            up: 0,
        }
    );
}

/// Physical = a connector and not virtual; a row that does not say so, or a
/// physical one that does not say its name, state or link, is unread.
#[test]
fn an_adapter_is_physical_by_its_connector_and_not_virtual() {
    assert_eq!((ADAPTER_DISABLED, ADAPTER_OPER_UP), (3, 1));
    let row = |connector, is_virtual| AdapterRow {
        connector_present: Some(connector),
        is_virtual: Some(is_virtual),
        ..adapter("x", STARTED, ADAPTER_OPER_UP)
    };
    let one = PhysicalNic {
        name: s("x"),
        enabled: true,
        up: true,
    };
    assert_eq!(physical_nics(&[row(true, false)]), Ok(vec![one]));
    assert_eq!(physical_nics(&[row(true, true)]), Ok(Vec::new()));
    assert_eq!(physical_nics(&[row(false, false)]), Ok(Vec::new()));
    assert_eq!(physical_nics(&[row(false, true)]), Ok(Vec::new()));
    assert_eq!(
        physical_nics(&snv_adapter_rows()),
        Ok(vec![
            PhysicalNic {
                name: s("Ethernet 4"),
                enabled: true,
                up: true,
            },
            PhysicalNic {
                name: s("Ethernet"),
                enabled: false,
                up: false,
            },
            PhysicalNic {
                name: s("Wi-Fi"),
                enabled: false,
                up: false,
            },
        ])
    );
    let says_not = |row: AdapterRow| physical_nics(&[adapter("Ethernet 4", 2, 1), row]);
    assert_eq!(
        says_not(AdapterRow {
            connector_present: None,
            ..adapter("y", STARTED, DOWN)
        }),
        Err(s(
            "the network adapter y does not say whether it is physical"
        ))
    );
    assert_eq!(
        says_not(AdapterRow {
            is_virtual: None,
            name: None,
            ..adapter("y", STARTED, DOWN)
        }),
        Err(s(
            "the network adapter ? does not say whether it is physical"
        ))
    );
    for row in [
        AdapterRow {
            name: None,
            ..adapter("y", STARTED, DOWN)
        },
        AdapterRow {
            state: None,
            ..adapter("y", STARTED, DOWN)
        },
        AdapterRow {
            oper: None,
            ..adapter("y", STARTED, DOWN)
        },
    ] {
        let name = row.name.clone().unwrap_or(s("?"));
        assert_eq!(
            says_not(row),
            Err(format!(
                "the physical network adapter {name} does not say its name, state or link"
            ))
        );
    }
    // A virtual row needs no name, state or link.
    assert_eq!(
        says_not(AdapterRow {
            is_virtual: Some(true),
            name: None,
            state: None,
            oper: None,
            ..adapter("y", STARTED, DOWN)
        })
        .map(|nics| nics.len()),
        Ok(1)
    );
}

/// The adapters not read (WMI, or a row that says too little): §6.1 is
/// unreadable and the reason is said, only with a leg to judge.
#[test]
fn adapters_that_cannot_be_read_leave_6_1_unreadable() {
    let (facts, _) = check(&snv_registry(), &snv_raw(), &[DANTE]);
    let mut errors = Vec::new();
    let failed = with_adapters(&facts, Err(s("WMI down")), &mut errors);
    assert_eq!(failed.physical, None);
    assert_eq!(errors, ["WMI down"]);
    assert_eq!(codes(&failed), ["unreadable:network_interfaces"]);
    let shown = view(&failed);
    assert_eq!((shown.physical_enabled, shown.physical_up), (None, None));
    // A row that says too little.
    let mut errors = Vec::new();
    let vague = with_adapters(
        &facts,
        Ok(vec![AdapterRow {
            state: None,
            ..adapter("Ethernet 4", STARTED, ADAPTER_OPER_UP)
        }]),
        &mut errors,
    );
    assert_eq!(vague.physical, None);
    assert_eq!(
        errors,
        ["the physical network adapter Ethernet 4 does not say its name, state or link"]
    );
    assert_eq!(codes(&vague), ["unreadable:network_interfaces"]);
    // No leg: nothing said, nothing judged.
    let (pp, _) = check(&snv_registry(), &snv_raw(), &[]);
    let mut errors = Vec::new();
    let pp = with_adapters(&pp, Err(s("WMI down")), &mut errors);
    assert!(errors.is_empty());
    assert_eq!(problems(&pp), Vec::<EnvProblem>::new());
}

/// §6.2: exactly one default route, never through the Dante leg.
#[test]
fn exactly_one_default_route_and_not_through_the_dante_leg() {
    let mut raw = snv_raw();
    raw.default_routes = Ok(vec![12, 30]);
    let (facts, _) = check(&snv_registry(), &raw, &[DANTE]);
    assert_eq!(
        net(problems(&facts)),
        [
            NetProblem::DefaultRoutes(vec![s("Ethernet 4"), s("Dante70")]),
            NetProblem::LegDefaultRoute { nic: s("Dante70") },
        ]
    );
    // The only one goes through the leg.
    raw.default_routes = Ok(vec![30]);
    let (facts, _) = check(&snv_registry(), &raw, &[DANTE]);
    assert_eq!(
        net(problems(&facts)),
        [NetProblem::LegDefaultRoute { nic: s("Dante70") }]
    );
    // None; an interface not listed is named by its index.
    raw.default_routes = Ok(Vec::new());
    let (facts, _) = check(&snv_registry(), &raw, &[DANTE]);
    assert_eq!(
        net(problems(&facts)),
        [NetProblem::DefaultRoutes(Vec::new())]
    );
    raw.default_routes = Ok(vec![12, 99]);
    let (facts, _) = check(&snv_registry(), &raw, &[DANTE]);
    assert_eq!(
        net(problems(&facts)),
        [NetProblem::DefaultRoutes(vec![
            s("Ethernet 4"),
            s("rozhranie 99")
        ])]
    );
    assert_eq!(
        view(&facts).default_routes,
        Some(vec![s("Ethernet 4"), s("rozhranie 99")])
    );
}

#[test]
fn a_dante_address_no_interface_holds_is_named() {
    let other = Ipv4Addr::new(10, 77, 7, 108);
    let (facts, errors) = check(&snv_registry(), &snv_raw(), &[other]);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(facts.legs[0].holder, Holder::Nobody);
    assert_eq!(
        net(problems(&facts)),
        [NetProblem::LegMissing {
            ip: s("10.77.7.108")
        }]
    );
    assert_eq!(view(&facts).legs[0].nic, None);
}

/// §6.3: each value of the leg that departs from the standard, in order.
#[test]
fn every_leg_value_off_the_standard_is_named_in_order() {
    let mut reg = snv_registry();
    reg.set(
        &tcpip(DANTE_GUID),
        "DefaultGateway",
        RegValue::Multi(vec![s(" 10.77.7.1 "), s(""), s("10.77.7.2")]),
    );
    reg.set(
        &tcpip(DANTE_GUID),
        "RegistrationEnabled",
        RegValue::Dword(1),
    );
    reg.set(&netbt(DANTE_GUID), "NetbiosOptions", RegValue::Dword(0));
    let mut raw = snv_raw();
    raw.ip_interfaces = Ok(vec![IpIf {
        index: 30,
        metric: 900,
        automatic: true,
    }]);
    let (mut facts, _) = check(&reg, &raw, &[DANTE]);
    facts.bindings = Some(vec![
        binding("dante70", " MS_SERVER ", Some(true)),
        binding("Dante70", "ms_msclient", Some(true)),
    ]);
    let d = || s("Dante70");
    assert_eq!(
        net(problems(&facts)),
        [
            NetProblem::LegGateway {
                nic: d(),
                gateways: vec![s("10.77.7.1"), s("10.77.7.2")],
            },
            NetProblem::LegMetric {
                nic: d(),
                metric: 900,
                automatic: true,
            },
            NetProblem::LegDnsRegistration { nic: d() },
            NetProblem::LegNetbios {
                nic: d(),
                value: Some(0),
            },
            NetProblem::LegBinding {
                nic: d(),
                binding: Binding::FileSharing,
            },
            NetProblem::LegBinding {
                nic: d(),
                binding: Binding::Client,
            },
        ]
    );
    let leg = &view(&facts).legs[0];
    assert_eq!(
        (
            leg.automatic_metric,
            leg.dns_registration,
            leg.netbios_options
        ),
        (Some(true), Some(true), Some(0))
    );
    assert_eq!(
        (leg.file_sharing_bound, leg.client_bound),
        (Some(true), Some(true))
    );
}

#[test]
fn the_metric_is_900_and_fixed() {
    for (metric, automatic, holds) in [
        (900, false, true),
        (900, true, false),
        (90, false, false),
        (901, false, false),
    ] {
        let mut raw = snv_raw();
        raw.ip_interfaces = Ok(vec![IpIf {
            index: 30,
            metric,
            automatic,
        }]);
        let (facts, _) = check(&snv_registry(), &raw, &[DANTE]);
        let expected = if holds {
            Vec::new()
        } else {
            vec![NetProblem::LegMetric {
                nic: s("Dante70"),
                metric,
                automatic,
            }]
        };
        assert_eq!(net(problems(&facts)), expected, "{metric} {automatic}");
    }
}

/// DHCP's gateway counts on a DHCP leg; a single string is one gateway; a
/// missing value is none.
#[test]
fn the_gateway_is_the_one_the_leg_s_addressing_uses() {
    let mut reg = snv_registry();
    reg.set(&tcpip(DANTE_GUID), "EnableDHCP", RegValue::Dword(1));
    let (facts, _) = check(&reg, &snv_raw(), &[DANTE]);
    assert_eq!(facts.legs[0].gateways, Reading::Value(vec![s("10.77.7.1")]));
    reg.set(
        &tcpip(DANTE_GUID),
        "DhcpDefaultGateway",
        RegValue::Text(s("10.77.7.3")),
    );
    let (facts, _) = check(&reg, &snv_raw(), &[DANTE]);
    assert_eq!(facts.legs[0].gateways, Reading::Value(vec![s("10.77.7.3")]));
    // No EnableDHCP: a static leg.
    let mut reg = snv_registry();
    reg.set(&tcpip(DANTE_GUID), "EnableDHCP", RegValue::Absent);
    reg.set(&tcpip(DANTE_GUID), "DefaultGateway", RegValue::Absent);
    let (facts, _) = check(&reg, &snv_raw(), &[DANTE]);
    assert_eq!(facts.legs[0].gateways, Reading::Value(Vec::new()));
    assert!(problems(&facts).is_empty());
}

/// A DNS registration value that is not set is Windows' default: on. A
/// NetBIOS value that is not set is the DHCP default: not off.
#[test]
fn unset_dns_and_netbios_values_are_windows_defaults() {
    let mut reg = snv_registry();
    reg.set(&tcpip(DANTE_GUID), "RegistrationEnabled", RegValue::Absent);
    reg.set(&netbt(DANTE_GUID), "NetbiosOptions", RegValue::Absent);
    let (facts, _) = check(&reg, &snv_raw(), &[DANTE]);
    assert_eq!(facts.legs[0].dns_registration, Reading::Value(true));
    assert_eq!(facts.legs[0].netbios, Reading::Absent);
    assert_eq!(
        net(problems(&facts)),
        [
            NetProblem::LegDnsRegistration { nic: s("Dante70") },
            NetProblem::LegNetbios {
                nic: s("Dante70"),
                value: None,
            },
        ]
    );
    assert_eq!(view(&facts).legs[0].netbios_options, None);
    reg.set(&netbt(DANTE_GUID), "NetbiosOptions", RegValue::Dword(1));
    let (facts, _) = check(&reg, &snv_raw(), &[DANTE]);
    assert_eq!(
        net(problems(&facts))[1],
        NetProblem::LegNetbios {
            nic: s("Dante70"),
            value: Some(1),
        }
    );
}

#[test]
fn a_binding_is_found_by_the_adapter_s_name_and_component() {
    let rows = snv_bindings();
    assert_eq!(bound(&rows, "Dante70", Binding::FileSharing), Some(false));
    assert_eq!(bound(&rows, " dante70 ", Binding::Client), Some(false));
    assert_eq!(bound(&rows, "Ethernet 4", Binding::Client), Some(true));
    assert_eq!(bound(&rows, "Wi-Fi", Binding::Client), None);
    let partial = [
        BindingRow {
            name: None,
            component: Some(s("ms_server")),
            enabled: Some(true),
        },
        BindingRow {
            name: Some(s("Dante70")),
            component: None,
            enabled: Some(true),
        },
        binding("Dante70", "ms_msclient", None),
    ];
    assert_eq!(bound(&partial, "Dante70", Binding::FileSharing), None);
    assert_eq!(bound(&partial, "Dante70", Binding::Client), None);
}

/// A leg whose bindings are not all known reads one "cannot be read".
#[test]
fn bindings_that_cannot_be_read_are_one_problem() {
    let (facts, _) = check(&snv_registry(), &snv_raw(), &[DANTE]);
    let unread = NetworkFacts {
        bindings: None,
        ..facts.clone()
    };
    assert_eq!(codes(&unread), ["unreadable:dante_leg_bindings"]);
    let shown = &view(&unread).legs[0];
    assert_eq!((shown.file_sharing_bound, shown.client_bound), (None, None));
    let half = NetworkFacts {
        bindings: Some(vec![binding("Dante70", "ms_server", Some(false))]),
        ..facts.clone()
    };
    assert_eq!(codes(&half), ["unreadable:dante_leg_bindings"]);
    let one_on = NetworkFacts {
        bindings: Some(vec![binding("Dante70", "ms_server", Some(true))]),
        ..facts
    };
    assert_eq!(
        codes(&one_on),
        [
            "dante_leg_binding:ms_server",
            "unreadable:dante_leg_bindings"
        ]
    );
    // The bindings' error is said when there is a leg.
    let mut errors = Vec::new();
    let (facts, _) = check(&snv_registry(), &snv_raw(), &[DANTE]);
    let failed = with_bindings(&facts, Err(s("WMI down")), &mut errors);
    assert_eq!(failed.bindings, None);
    assert_eq!(errors, ["WMI down"]);
}

/// A value of the leg that cannot be read: one "cannot be read" first, the
/// other values still judged; each reason in the errors.
#[test]
fn a_leg_value_that_cannot_be_read_says_why() {
    let mut reg = snv_registry();
    reg.set(
        &tcpip(DANTE_GUID),
        "RegistrationEnabled",
        RegValue::Failed(5),
    );
    reg.set(&netbt(DANTE_GUID), "NetbiosOptions", RegValue::Text(s("2")));
    reg.set(&tcpip(DANTE_GUID), "EnableDHCP", RegValue::Dword(0));
    reg.set(&tcpip(DANTE_GUID), "DefaultGateway", RegValue::Dword(1));
    let mut raw = snv_raw();
    raw.ip_interfaces = Ok(vec![IpIf {
        index: 30,
        metric: 25,
        automatic: true,
    }]);
    let (facts, errors) = check(&reg, &raw, &[DANTE]);
    assert_eq!(
        errors,
        [
            "the Dante leg Dante70's DefaultGateway is no list",
            "the Dante leg Dante70's RegistrationEnabled could not be read (error 5)",
            "the Dante leg Dante70's NetbiosOptions is no DWORD",
        ]
    );
    assert_eq!(codes(&facts), ["unreadable:dante_leg", "dante_leg_metric"]);
    // Each value alone.
    for (key, name, value, why) in [
        (
            tcpip(DANTE_GUID),
            "EnableDHCP",
            RegValue::Failed(5),
            "the Dante leg Dante70's EnableDHCP could not be read (error 5)",
        ),
        (
            tcpip(DANTE_GUID),
            "EnableDHCP",
            RegValue::Text(s("1")),
            "the Dante leg Dante70's EnableDHCP is no DWORD",
        ),
        (
            tcpip(DANTE_GUID),
            "DefaultGateway",
            RegValue::Failed(2),
            "the Dante leg Dante70's DefaultGateway could not be read (error 2)",
        ),
        (
            tcpip(DANTE_GUID),
            "DefaultGateway",
            RegValue::Other(3),
            "the Dante leg Dante70's DefaultGateway is no list",
        ),
        (
            tcpip(DANTE_GUID),
            "RegistrationEnabled",
            RegValue::Multi(Vec::new()),
            "the Dante leg Dante70's RegistrationEnabled is no DWORD",
        ),
        (
            netbt(DANTE_GUID),
            "NetbiosOptions",
            RegValue::Failed(5),
            "the Dante leg Dante70's NetbiosOptions could not be read (error 5)",
        ),
        (
            netbt(DANTE_GUID),
            "NetbiosOptions",
            RegValue::Other(3),
            "the Dante leg Dante70's NetbiosOptions is no DWORD",
        ),
    ] {
        let mut reg = snv_registry();
        reg.set(&key, name, value);
        let (facts, errors) = check(&reg, &snv_raw(), &[DANTE]);
        assert_eq!(errors, [why]);
        assert_eq!(codes(&facts), ["unreadable:dante_leg"], "{why}");
    }
    // The leg's IPv4 interface not listed.
    let mut raw = snv_raw();
    raw.ip_interfaces = Ok(Vec::new());
    let (facts, errors) = check(&snv_registry(), &raw, &[DANTE]);
    assert_eq!(
        errors,
        ["the Dante leg Dante70's IPv4 interface is not listed"]
    );
    assert_eq!(codes(&facts), ["unreadable:dante_leg"]);
    assert_eq!(view(&facts).legs[0].metric, None);
}

/// A list IP Helper could not give: said once, judged as unreadable.
#[test]
fn a_list_that_cannot_be_read_says_why() {
    let mut raw = snv_raw();
    raw.interfaces = Err(8);
    raw.default_routes = Err(87);
    raw.ip_interfaces = Err(5);
    let (facts, errors) = check(&snv_registry(), &raw, &[DANTE]);
    assert_eq!(
        errors,
        [
            "the network interfaces could not be listed (error 8)",
            "the routing table could not be listed (error 87)",
            "the IPv4 interfaces could not be listed (error 5)",
        ]
    );
    assert_eq!(facts.legs[0].holder, Holder::Unread);
    // §6.1 is judged from WMI's adapters, so the leg's unknown interface
    // comes after §6.2.
    assert_eq!(
        codes(&facts),
        ["unreadable:default_routes", "unreadable:network_interfaces"]
    );
    let shown = view(&facts);
    // §6.1's adapters come from WMI: still read.
    assert_eq!(
        (
            shown.physical_enabled,
            shown.physical_up,
            shown.default_routes
        ),
        (
            Some(vec![s("Ethernet 4")]),
            Some(vec![s("Ethernet 4")]),
            None
        )
    );
    // The addresses not read: the leg is unknown.
    let mut raw = snv_raw();
    raw.addresses = Err(1);
    let (facts, errors) = check(&snv_registry(), &raw, &[DANTE]);
    assert_eq!(errors, ["the IPv4 addresses could not be listed (error 1)"]);
    assert_eq!(facts.legs[0].holder, Holder::Unread);
    assert_eq!(codes(&facts), ["unreadable:network_interfaces"]);
    // The interface holding the address is not in the interface list.
    let mut raw = snv_raw();
    raw.addresses = Ok(vec![(31, DANTE)]);
    let (facts, errors) = check(&snv_registry(), &raw, &[DANTE]);
    assert_eq!(
        errors,
        ["the interface 31 holding the Dante address 10.77.7.107 is not listed"]
    );
    assert_eq!(codes(&facts), ["unreadable:network_interfaces"]);
    // The IPv4 interfaces not read: the metric is unknown.
    let mut raw = snv_raw();
    raw.ip_interfaces = Err(5);
    let (facts, _) = check(&snv_registry(), &raw, &[DANTE]);
    assert_eq!(facts.legs[0].metric, Reading::Unread);
    assert_eq!(codes(&facts), ["unreadable:dante_leg"]);
}
