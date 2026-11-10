//! #255 item D: the two-network problems' codes and Slovak texts, the
//! bindings' names, the status view on the wire.

use super::super::EnvProblem;
use super::{
    Binding, DANTE_LEG_METRIC, DanteLegView, NETBIOS_DISABLED, NetProblem, TwoNetworksView, list_sk,
};

fn s(v: &str) -> String {
    v.to_string()
}

/// Every problem, one of each kind, with its code and Slovak text.
fn every_problem() -> Vec<(NetProblem, &'static str, &'static str)> {
    vec![
        (
            NetProblem::PhysicalNics {
                enabled: vec![s("Ethernet 4"), s("Ethernet (odpojená)")],
                up: 1,
            },
            "two_networks_physical_nics",
            "počítač so sieťou Dante má mať zapnutú práve jednu fyzickú sieťovú kartu, \
             pripojenú, a ostatné vypnuté v systéme (nie len odpojené); zapnuté: Ethernet 4, \
             Ethernet (odpojená), pripojené: 1",
        ),
        (
            NetProblem::DefaultRoutes(vec![s("Ethernet 4"), s("Dante70")]),
            "two_networks_default_routes",
            "počítač so sieťou Dante má mať práve jednu predvolenú trasu (0.0.0.0/0), cez sieť \
             LAN; má ich 2: Ethernet 4, Dante70",
        ),
        (
            NetProblem::LegDefaultRoute { nic: s("Dante70") },
            "dante_leg_default_route",
            "predvolená trasa ide cez sieť Dante „Dante70“ (má ísť len cez LAN)",
        ),
        (
            NetProblem::LegMissing {
                ip: s("10.77.7.107"),
            },
            "dante_leg_missing",
            "adresu výstupu Dante 10.77.7.107 nemá žiadne sieťové rozhranie tohto počítača",
        ),
        (
            NetProblem::LegGateway {
                nic: s("Dante70"),
                gateways: vec![s("10.77.7.1")],
            },
            "dante_leg_gateway",
            "sieť Dante „Dante70“ má nastavenú bránu 10.77.7.1 (nemá mať žiadnu)",
        ),
        (
            NetProblem::LegMetric {
                nic: s("Dante70"),
                metric: 25,
                automatic: true,
            },
            "dante_leg_metric",
            "sieť Dante „Dante70“ má automatickú metriku rozhrania (25); má byť pevne 900",
        ),
        (
            NetProblem::LegMetric {
                nic: s("Dante70"),
                metric: 90,
                automatic: false,
            },
            "dante_leg_metric",
            "sieť Dante „Dante70“ má metriku rozhrania 90 (má byť pevne 900)",
        ),
        (
            NetProblem::LegDnsRegistration { nic: s("Dante70") },
            "dante_leg_dns_registration",
            "sieť Dante „Dante70“ registruje svoje adresy v DNS (má to byť vypnuté)",
        ),
        (
            NetProblem::LegNetbios {
                nic: s("Dante70"),
                value: Some(1),
            },
            "dante_leg_netbios",
            "sieť Dante „Dante70“ nemá vypnutý NetBIOS cez TCP/IP (NetbiosOptions 1, má byť 2)",
        ),
        (
            NetProblem::LegNetbios {
                nic: s("Dante70"),
                value: None,
            },
            "dante_leg_netbios",
            "sieť Dante „Dante70“ nemá vypnutý NetBIOS cez TCP/IP (NetbiosOptions nenastavené, \
             má byť 2)",
        ),
        (
            NetProblem::LegBinding {
                nic: s("Dante70"),
                binding: Binding::FileSharing,
            },
            "dante_leg_binding:ms_server",
            "na sieti Dante „Dante70“ je zapnuté „File and Printer Sharing for Microsoft \
             Networks“ (má byť vypnuté)",
        ),
        (
            NetProblem::LegBinding {
                nic: s("Dante70"),
                binding: Binding::Client,
            },
            "dante_leg_binding:ms_msclient",
            "na sieti Dante „Dante70“ je zapnuté „Client for Microsoft Networks“ (má byť \
             vypnuté)",
        ),
        (
            NetProblem::UnreadableDanteOutputs,
            "unreadable:dante_outputs",
            "zvukové výstupy sa nepodarilo prečítať (nevie sa, či má počítač sieť Dante)",
        ),
        (
            NetProblem::UnreadableInterfaces,
            "unreadable:network_interfaces",
            "sieťové rozhrania a ich adresy sa nepodarilo zistiť",
        ),
        (
            NetProblem::UnreadableRoutes,
            "unreadable:default_routes",
            "predvolené trasy sa nepodarilo zistiť",
        ),
        (
            NetProblem::UnreadableLeg { nic: s("Dante70") },
            "unreadable:dante_leg",
            "nastavenia siete Dante „Dante70“ sa nepodarilo zistiť",
        ),
        (
            NetProblem::UnreadableBindings { nic: s("Dante70") },
            "unreadable:dante_leg_bindings",
            "väzby siete Dante „Dante70“ (zdieľanie, klient) sa nepodarilo zistiť",
        ),
    ]
}

#[test]
fn every_problem_has_its_code_and_slovak_text() {
    for (problem, code, text) in every_problem() {
        assert_eq!(problem.code(), code, "{problem:?}");
        assert_eq!(problem.text_sk(), text, "{problem:?}");
        let env = EnvProblem::Network(problem.clone());
        assert_eq!(env.code(), code);
        assert_eq!(env.text_sk(), text);
        assert_eq!(env.view().code, code);
        assert_eq!(env.view().text_sk, text);
    }
}

#[test]
fn the_standard_values_and_the_bindings() {
    assert_eq!(DANTE_LEG_METRIC, 900);
    assert_eq!(NETBIOS_DISABLED, 2);
    let components: Vec<&str> = Binding::ALL.iter().map(|b| b.component()).collect();
    assert_eq!(components, ["ms_server", "ms_msclient"]);
    assert_eq!(
        Binding::FileSharing.name(),
        "File and Printer Sharing for Microsoft Networks"
    );
    assert_eq!(Binding::Client.name(), "Client for Microsoft Networks");
}

#[test]
fn a_list_names_each_or_says_none() {
    assert_eq!(list_sk(&[]), "žiadne");
    assert_eq!(list_sk(&[s("a")]), "a");
    assert_eq!(list_sk(&[s("a"), s("b")]), "a, b");
}

/// resolume-snv's leg as the standard wants it, every field by name.
#[test]
fn the_view_serializes_every_field_by_name() {
    let view = TwoNetworksView {
        dante_ips: Some(vec![s("10.77.7.107")]),
        physical_enabled: Some(vec![s("Ethernet 4")]),
        physical_up: Some(vec![s("Ethernet 4")]),
        default_routes: Some(vec![s("Ethernet 4")]),
        legs: vec![DanteLegView {
            ip: s("10.77.7.107"),
            nic: Some(s("Dante70")),
            gateways: Some(vec![]),
            metric: Some(900),
            automatic_metric: Some(false),
            dns_registration: Some(false),
            netbios_options: Some(2),
            file_sharing_bound: Some(false),
            client_bound: Some(false),
        }],
    };
    let json = serde_json::to_value(&view).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "dante_ips": ["10.77.7.107"],
            "physical_enabled": ["Ethernet 4"],
            "physical_up": ["Ethernet 4"],
            "default_routes": ["Ethernet 4"],
            "legs": [{
                "ip": "10.77.7.107",
                "nic": "Dante70",
                "gateways": [],
                "metric": 900,
                "automatic_metric": false,
                "dns_registration": false,
                "netbios_options": 2,
                "file_sharing_bound": false,
                "client_bound": false,
            }],
        })
    );
    let back: TwoNetworksView = serde_json::from_value(json).unwrap();
    assert_eq!(back, view);
    let empty: TwoNetworksView = serde_json::from_str("{}").unwrap();
    assert_eq!(empty, TwoNetworksView::default());
}
