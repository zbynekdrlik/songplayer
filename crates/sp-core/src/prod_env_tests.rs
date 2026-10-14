//! #233: the production environment's codes, Slovak texts, status wire shape
//! and health segment.

use super::*;
use crate::health::HealthTone;
use crate::prod_env::audio::{AudioProfile, MmcssSetting, NicSetting, PowerView, SoundGridState};

/// Every problem, one of each kind and profile.
fn every_problem() -> Vec<EnvProblem> {
    let mut all = vec![
        EnvProblem::EditionNotAllowed {
            edition: "Professional".to_string(),
            build: Some(22631),
        },
        EnvProblem::EditionNotAllowed {
            edition: "IoTEnterprise".to_string(),
            build: None,
        },
        EnvProblem::DefenderRealtimeOn,
        EnvProblem::DefenderPassive("EDR Block Mode".to_string()),
        EnvProblem::AntivirusOn("ESET Security".to_string()),
    ];
    for p in FirewallProfile::ALL {
        all.push(EnvProblem::FirewallOn(p));
    }
    all.push(EnvProblem::Unreadable(Unreadable::Edition));
    all.push(EnvProblem::Unreadable(Unreadable::Build));
    all.push(EnvProblem::Unreadable(Unreadable::Defender));
    all.push(EnvProblem::Unreadable(Unreadable::Antivirus));
    for p in FirewallProfile::ALL {
        all.push(EnvProblem::Unreadable(Unreadable::Firewall(p)));
    }
    all
}

/// #255: every audio-profile problem, one of each kind.
fn every_audio_problem() -> Vec<EnvProblem> {
    let nic = || "soundgrid-pcie".to_string();
    let mut all = vec![
        EnvProblem::SoundGridPortUnset,
        EnvProblem::SoundGridPortMissing {
            mac: "6c2408e59c91".to_string(),
        },
        EnvProblem::SoundGridNicNetAdapterCx { nic: nic() },
    ];
    for setting in NicSetting::ALL {
        all.push(EnvProblem::SoundGridNicSetting {
            nic: nic(),
            setting,
            value: Some("1".to_string()),
        });
    }
    all.push(EnvProblem::SoundGridNicSetting {
        nic: nic(),
        setting: NicSetting::FlowControl,
        value: None,
    });
    all.push(EnvProblem::SoundGridNicUnreadable { nic: nic() });
    all.push(EnvProblem::AutoUpdateOn);
    all.push(EnvProblem::DriversNotExcluded);
    for setting in MmcssSetting::ALL {
        all.push(EnvProblem::Mmcss {
            setting,
            value: Some("20".to_string()),
        });
    }
    all.push(EnvProblem::Mmcss {
        setting: MmcssSetting::ProAudioPriority,
        value: None,
    });
    all.push(EnvProblem::Unreadable(Unreadable::SoundGrid));
    all.push(EnvProblem::Unreadable(Unreadable::WindowsUpdate));
    all.push(EnvProblem::Unreadable(Unreadable::Mmcss));
    all
}

#[test]
fn every_audio_problem_has_its_stable_code() {
    let codes: Vec<String> = every_audio_problem().iter().map(EnvProblem::code).collect();
    assert_eq!(
        codes,
        [
            "soundgrid_port_unset",
            "soundgrid_port_missing",
            "soundgrid_nic_netadaptercx",
            "soundgrid_nic_setting:interrupt_moderation",
            "soundgrid_nic_setting:flow_control",
            "soundgrid_nic_setting:eee",
            "soundgrid_nic_setting:green_ethernet",
            "soundgrid_nic_setting:power_saving",
            "soundgrid_nic_setting:gigalite",
            "soundgrid_nic_setting:power_management",
            "soundgrid_nic_setting:flow_control",
            "unreadable:soundgrid_nic",
            "windows_auto_update_on",
            "windows_update_drivers_on",
            "mmcss:system_responsiveness",
            "mmcss:network_throttling_index",
            "mmcss:pro_audio_priority",
            "mmcss:pro_audio_scheduling_category",
            "mmcss:pro_audio_priority",
            "unreadable:soundgrid",
            "unreadable:windows_update",
            "unreadable:mmcss",
        ]
    );
}

#[test]
fn every_audio_problem_reads_in_slovak() {
    let texts: Vec<String> = every_audio_problem()
        .iter()
        .map(EnvProblem::text_sk)
        .collect();
    assert_eq!(
        texts,
        [
            "SoundGrid nemá zvolený sieťový port — nastav ho v SoundGrid Studio / ovládači",
            "sieťová karta s MAC 6c:24:08:e5:9c:91, ktorú má ovládač SoundGrid zvolenú ako port, \
             sa nenašla",
            "sieťová karta SoundGrid „soundgrid-pcie“ beží na ovládači NetAdapterCx (musí bežať \
             na ovládači NDIS)",
            "sieťová karta SoundGrid „soundgrid-pcie“: nastavenie „Interrupt Moderation \
             (moderovanie prerušení)“ nie je vypnuté (hodnota 1)",
            "sieťová karta SoundGrid „soundgrid-pcie“: nastavenie „Flow Control (riadenie toku)“ \
             nie je vypnuté (hodnota 1)",
            "sieťová karta SoundGrid „soundgrid-pcie“: nastavenie „Energy Efficient Ethernet“ \
             nie je vypnuté (hodnota 1)",
            "sieťová karta SoundGrid „soundgrid-pcie“: nastavenie „Green Ethernet“ nie je \
             vypnuté (hodnota 1)",
            "sieťová karta SoundGrid „soundgrid-pcie“: nastavenie „Power Saving Mode (úsporný \
             režim)“ nie je vypnuté (hodnota 1)",
            "sieťová karta SoundGrid „soundgrid-pcie“: nastavenie „GigaLite“ nie je vypnuté \
             (hodnota 1)",
            "sieťová karta SoundGrid „soundgrid-pcie“: nastavenie „vypínanie zariadenia na \
             úsporu energie“ nie je vypnuté (hodnota 1)",
            "sieťová karta SoundGrid „soundgrid-pcie“: nastavenie „Flow Control (riadenie toku)“ \
             chýba",
            "ovládač alebo nastavenia sieťovej karty SoundGrid „soundgrid-pcie“ sa nepodarilo \
             zistiť",
            "automatické aktualizácie Windows nie sú vypnuté (NoAutoUpdate)",
            "Windows Update nemá vylúčené ovládače (ExcludeWUDriversInQualityUpdate) — \
             aktualizácia môže vymeniť ovládač sieťovej karty",
            "MMCSS: SystemResponsiveness je 20 (má byť 0)",
            "MMCSS: NetworkThrottlingIndex je 20 (má byť 0xFFFFFFFF)",
            "MMCSS: Priority úlohy Pro Audio je 20 (má byť 6)",
            "MMCSS: Scheduling Category úlohy Pro Audio je 20 (má byť High)",
            "MMCSS: Priority úlohy Pro Audio chýba (má byť 6)",
            "väzby protokolu SoundGrid na sieťové karty sa nepodarilo zistiť",
            "nastavenia Windows Update sa nepodarilo zistiť",
            "nastavenia MMCSS sa nepodarilo zistiť",
        ]
    );
}

#[test]
fn every_problem_has_its_stable_code() {
    let codes: Vec<String> = every_problem().iter().map(EnvProblem::code).collect();
    assert_eq!(
        codes,
        [
            "edition_not_allowed",
            "edition_not_allowed",
            "defender_realtime_on",
            "defender_passive_mode",
            "antivirus_on",
            "firewall_on:domain",
            "firewall_on:private",
            "firewall_on:public",
            "unreadable:edition",
            "unreadable:build",
            "unreadable:defender",
            "unreadable:antivirus",
            "unreadable:firewall:domain",
            "unreadable:firewall:private",
            "unreadable:firewall:public",
        ]
    );
}

#[test]
fn every_problem_reads_in_slovak() {
    let texts: Vec<String> = every_problem().iter().map(EnvProblem::text_sk).collect();
    assert_eq!(
        texts,
        [
            "edícia Windows „Professional“ (zostava 22631) nie je povolená (povolené sú len \
             Windows LTSC a Windows 11 IoT Enterprise)",
            "edícia Windows „IoTEnterprise“ nie je povolená (povolené sú len Windows LTSC a \
             Windows 11 IoT Enterprise)",
            "ochrana Windows Defender v reálnom čase je zapnutá",
            "Windows Defender beží v režime „EDR Block Mode“ a súbory stále kontroluje",
            "antivírus „ESET Security“ je zapnutý",
            "brána firewall je zapnutá (doménový profil)",
            "brána firewall je zapnutá (súkromný profil)",
            "brána firewall je zapnutá (verejný profil)",
            "edíciu Windows sa nepodarilo zistiť",
            "zostavu Windows sa nepodarilo zistiť",
            "stav ochrany Windows Defender v reálnom čase sa nepodarilo zistiť",
            "zoznam antivírusových programov sa nepodarilo zistiť",
            "stav brány firewall (doménový profil) sa nepodarilo zistiť",
            "stav brány firewall (súkromný profil) sa nepodarilo zistiť",
            "stav brány firewall (verejný profil) sa nepodarilo zistiť",
        ]
    );
}

#[test]
fn a_problem_is_shown_by_its_code_and_text() {
    let p = EnvProblem::FirewallOn(FirewallProfile::Public);
    assert_eq!(
        p.view(),
        ProblemView {
            code: "firewall_on:public".to_string(),
            text_sk: "brána firewall je zapnutá (verejný profil)".to_string(),
        }
    );
}

#[test]
fn the_profiles_are_listed_domain_private_public() {
    let names: Vec<&str> = FirewallProfile::ALL.iter().map(|p| p.name()).collect();
    assert_eq!(names, ["domain", "private", "public"]);
}

/// The wire shape both routes answer, every field by name.
#[test]
fn the_status_serializes_every_field_by_name() {
    let status = EnvironmentStatus {
        checked: true,
        ok: false,
        note: None,
        edition_id: Some("IoTEnterpriseS".to_string()),
        product_name: Some("Windows 10 IoT Enterprise LTSC".to_string()),
        build_number: Some(26100),
        ltsc: Some(true),
        edition_allowed: Some(true),
        defender_realtime: Some(true),
        defender_passive: Some(false),
        defender_mode: Some("Normal".to_string()),
        defender_antivirus_enabled: Some(true),
        antivirus_on: Some(vec!["ESET Security".to_string()]),
        firewall: FirewallState {
            domain: Some(false),
            private: None,
            public: Some(true),
        },
        audio: AudioProfile {
            soundgrid: Some(SoundGridState::NotInstalled),
            notes: vec!["parkovanie jadier je zapnuté (minimum 50 % jadier)".to_string()],
            ..AudioProfile::default()
        },
        problems: vec![EnvProblem::DefenderRealtimeOn.view()],
        read_errors: vec!["firewall private: error 5".to_string()],
        checked_at_ms: Some(1_791_608_681_366),
    };
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "checked": true,
            "ok": false,
            "note": null,
            "edition_id": "IoTEnterpriseS",
            "product_name": "Windows 10 IoT Enterprise LTSC",
            "build_number": 26100,
            "ltsc": true,
            "edition_allowed": true,
            "defender_realtime": true,
            "defender_passive": false,
            "defender_mode": "Normal",
            "defender_antivirus_enabled": true,
            "antivirus_on": ["ESET Security"],
            "firewall": {"domain": false, "private": null, "public": true},
            "audio": {
                "soundgrid": "not_installed",
                "soundgrid_port_mac": null,
                "soundgrid_nic": null,
                "dante": {
                    "installed": null,
                    "interface_address": null,
                    "interface_name": null,
                    "nic": null,
                },
                "windows_update": {"no_auto_update": null, "drivers_excluded": null},
                "mmcss": {
                    "system_responsiveness": null,
                    "network_throttling_index": null,
                    "pro_audio_priority": null,
                    "pro_audio_scheduling_category": null,
                },
                "power": {
                    "scheme_guid": null,
                    "scheme_name": null,
                    "core_parking_min_cores_pct": null,
                },
                "notes": ["parkovanie jadier je zapnuté (minimum 50 % jadier)"],
            },
            "problems": [{
                "code": "defender_realtime_on",
                "text_sk": "ochrana Windows Defender v reálnom čase je zapnutá",
            }],
            "read_errors": ["firewall private: error 5"],
            "checked_at_ms": 1_791_608_681_366_i64,
        })
    );
    let back: EnvironmentStatus = serde_json::from_value(json).unwrap();
    assert_eq!(back, status);
}

/// An older server (or the mock) without the field reads as not checked.
#[test]
fn a_missing_status_reads_as_not_checked() {
    let s: EnvironmentStatus = serde_json::from_str("{}").unwrap();
    assert_eq!(s, EnvironmentStatus::default());
    assert!(!s.checked);
    assert!(!s.ok);
    assert!(s.problems.is_empty());
}

fn status_with(problems: &[EnvProblem]) -> EnvironmentStatus {
    EnvironmentStatus {
        checked: true,
        ok: problems.is_empty(),
        problems: problems.iter().map(EnvProblem::view).collect(),
        ..EnvironmentStatus::default()
    }
}

#[test]
fn the_marker_says_unchecked_ok_or_violated() {
    assert_eq!(marker(&EnvironmentStatus::default()), "unchecked");
    assert_eq!(marker(&status_with(&[])), "ok");
    assert_eq!(
        marker(&status_with(&[EnvProblem::DefenderRealtimeOn])),
        "violated"
    );
    // Not ok with no problem named, or ok beside a problem: never "ok".
    let not_ok = EnvironmentStatus {
        checked: true,
        ok: false,
        ..EnvironmentStatus::default()
    };
    assert_eq!(marker(&not_ok), "violated");
    let ok_with_a_problem = EnvironmentStatus {
        ok: true,
        ..status_with(&[EnvProblem::DefenderRealtimeOn])
    };
    assert_eq!(marker(&ok_with_a_problem), "violated");
    // Not checked wins over anything else.
    let unchecked = EnvironmentStatus {
        checked: false,
        ..status_with(&[EnvProblem::DefenderRealtimeOn])
    };
    assert_eq!(marker(&unchecked), "unchecked");
}

#[test]
fn no_segment_before_the_first_check_or_while_the_box_holds() {
    assert_eq!(label(&EnvironmentStatus::default()), None);
    assert_eq!(label(&status_with(&[])), None);
}

#[test]
fn one_problem_is_the_red_segment_and_the_tooltip_names_the_rule() {
    let (tone, text, tip) = label(&status_with(&[EnvProblem::DefenderRealtimeOn])).unwrap();
    assert_eq!(tone, HealthTone::Error);
    assert_eq!(
        text,
        "Prostredie: ochrana Windows Defender v reálnom čase je zapnutá"
    );
    assert_eq!(
        tip,
        format!("{RULE_SK} Zistené: ochrana Windows Defender v reálnom čase je zapnutá.")
    );
}

#[test]
fn more_problems_show_the_first_and_count_the_rest() {
    let two = [
        EnvProblem::DefenderRealtimeOn,
        EnvProblem::FirewallOn(FirewallProfile::Domain),
    ];
    let (_, text, tip) = label(&status_with(&two)).unwrap();
    assert_eq!(
        text,
        "Prostredie: ochrana Windows Defender v reálnom čase je zapnutá (+1)"
    );
    assert_eq!(
        tip,
        format!(
            "{RULE_SK} Zistené: ochrana Windows Defender v reálnom čase je zapnutá; brána \
             firewall je zapnutá (doménový profil)."
        )
    );
    let three = [
        EnvProblem::Unreadable(Unreadable::Edition),
        EnvProblem::DefenderRealtimeOn,
        EnvProblem::FirewallOn(FirewallProfile::Public),
    ];
    let (_, text, _) = label(&status_with(&three)).unwrap();
    assert_eq!(text, "Prostredie: edíciu Windows sa nepodarilo zistiť (+2)");
}

/// #255: the rule names the audio profile after the #233 part.
#[test]
fn the_rule_names_the_audio_profile() {
    assert!(RULE_SK.starts_with(
        "Produkčný počítač musí bežať na Windows LTSC alebo Windows 11 IoT Enterprise, s \
         vypnutou ochranou Windows Defender v reálnom čase (ani v pasívnom režime), bez iného \
         antivírusu a s vypnutou bránou firewall"
    ));
    assert!(RULE_SK.ends_with(
        "programy, ktoré SongPlayer spúšťa. Sieťová karta, ktorú má ovládač SoundGrid zvolenú \
         ako port, beží na ovládači NDIS s vypnutým moderovaním prerušení, riadením toku a \
         úsporou energie, automatické aktualizácie Windows sú vypnuté a MMCSS je nastavené pre \
         Pro Audio."
    ));
}

fn with_notes(notes: &[&str], power: PowerView) -> EnvironmentStatus {
    EnvironmentStatus {
        checked: true,
        ok: true,
        audio: AudioProfile {
            notes: notes.iter().map(|n| n.to_string()).collect(),
            power,
            ..AudioProfile::default()
        },
        ..EnvironmentStatus::default()
    }
}

/// #255: report-only notes are a grey segment, never a problem; none, none.
#[test]
fn the_notes_segment_shows_only_while_there_are_notes() {
    assert_eq!(notes_label(&EnvironmentStatus::default()), None);
    assert_eq!(notes_label(&with_notes(&[], PowerView::default())), None);
    let one = with_notes(&["a"], PowerView::default());
    assert_eq!(
        notes_label(&one),
        Some((
            HealthTone::Off,
            "Profil PC: 1 poznámka".to_string(),
            format!("{NOTES_SK} a."),
        ))
    );
    // The box still holds: the marker and the problem segment ignore notes.
    assert_eq!(marker(&one), "ok");
    assert_eq!(label(&one), None);
}

#[test]
fn the_notes_segment_counts_in_slovak_and_names_the_power_scheme() {
    let power = PowerView {
        scheme_guid: Some("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c".to_string()),
        scheme_name: Some("High performance".to_string()),
        core_parking_min_cores_pct: Some(100),
    };
    let (_, text, tip) = notes_label(&with_notes(&["a", "b"], power.clone())).unwrap();
    assert_eq!(text, "Profil PC: 2 poznámky");
    assert_eq!(
        tip,
        format!(
            "{NOTES_SK} a; b. Plán napájania: High performance \
             (8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c)."
        )
    );
    let unnamed = PowerView {
        scheme_name: None,
        ..power
    };
    let (_, text, tip) = notes_label(&with_notes(&["a", "b", "c", "d", "e"], unnamed)).unwrap();
    assert_eq!(text, "Profil PC: 5 poznámok");
    assert!(
        tip.ends_with(" Plán napájania: bez názvu (8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c)."),
        "{tip}"
    );
}

#[test]
fn notes_are_counted_like_slovak_counts_them() {
    assert_eq!(notes_word(0), "poznámok");
    assert_eq!(notes_word(1), "poznámka");
    assert_eq!(notes_word(2), "poznámky");
    assert_eq!(notes_word(4), "poznámky");
    assert_eq!(notes_word(5), "poznámok");
    assert_eq!(notes_word(11), "poznámok");
}
