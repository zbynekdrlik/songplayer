//! #255: SoundGrid's adapter from its driver's chosen port — the strict
//! reading of `SoundGrid.Driver.preferences` (PP's file verbatim) and the
//! walk over PP's layout.

use super::super::fake::*;
use super::*;

/// PP's preferences with its `LocalLanPort` element replaced by `port`.
fn with_port(port: &str) -> String {
    let element = "<LocalLanPort>6c:24:08:e5:9c:91</LocalLanPort>";
    assert_eq!(SG_PREFERENCES.matches(element).count(), 1);
    SG_PREFERENCES.replace(element, port)
}

// ---- one element ----

#[test]
fn an_element_s_text_is_read_strictly() {
    assert_eq!(element_text("<a>x</a>", "a"), Ok(Some("x")));
    assert_eq!(element_text("  <a> x </a> ", "a"), Ok(Some(" x ")));
    assert_eq!(element_text("<b>x</b>", "a"), Ok(None));
    assert_eq!(element_text("", "a"), Ok(None));
    assert_eq!(element_text("<a/>", "a"), Ok(Some("")));
    assert_eq!(element_text(" <a />", "a"), Ok(Some("")));
    assert_eq!(element_text("<a></a>", "a"), Ok(Some("")));
}

#[test]
fn an_element_of_another_form_is_an_error_naming_it() {
    for (text, error) in [
        ("<a>x", PortError::Unclosed("a")),
        ("</a>", PortError::Unclosed("a")),
        ("</a><a>x", PortError::Unclosed("a")),
        ("<a>x</a><a>y</a>", PortError::Twice("a")),
        ("<a>x<a>y</a>", PortError::Twice("a")),
        ("<a>x</a></a>", PortError::Twice("a")),
        ("<a><a>x</a></a>", PortError::Twice("a")),
        ("<a b=\"1\">x</a>", PortError::Malformed("a")),
        ("<a b>x", PortError::Malformed("a")),
        ("<ab>x</ab>", PortError::Malformed("a")),
        ("<a/></a>", PortError::Malformed("a")),
    ] {
        assert_eq!(element_text(text, "a"), Err(error), "{text:?}");
    }
}

// ---- the chosen port ----

#[test]
fn pp_s_preferences_name_soundgrid_pcie_s_mac() {
    assert_eq!(chosen_port(SG_PREFERENCES), Ok(Some(SG_MAC.to_string())));
    assert_eq!(
        chosen_port(&format!("\u{feff}{SG_PREFERENCES}")),
        Ok(Some(SG_MAC.to_string()))
    );
    assert_eq!(
        chosen_port(&with_port(
            "<LocalLanPort> 6C-24-08-E5-9C-91 </LocalLanPort>"
        )),
        Ok(Some(SG_MAC.to_string()))
    );
}

#[test]
fn no_port_chosen_reads_as_none() {
    for port in [
        "",
        "<LocalLanPort></LocalLanPort>",
        "<LocalLanPort>   </LocalLanPort>",
        "<LocalLanPort/>",
        "<LocalLanPort />",
    ] {
        assert_eq!(chosen_port(&with_port(port)), Ok(None), "{port:?}");
    }
    // A port outside <Configuration> is no choice of the driver's.
    let outside = "<Preferences><LocalLanPort>6c:24:08:e5:9c:91</LocalLanPort>\
                   <Configuration><IOMode>0</IOMode></Configuration></Preferences>";
    assert_eq!(chosen_port(outside), Ok(None));
    assert_eq!(
        chosen_port("<Preferences><Configuration/></Preferences>"),
        Ok(None)
    );
}

#[test]
fn preferences_that_do_not_read_say_what_is_wrong() {
    assert_eq!(
        chosen_port("<Preferences></Preferences>"),
        Err(PortError::NoConfiguration)
    );
    assert_eq!(chosen_port(""), Err(PortError::NoConfiguration));
    assert_eq!(
        chosen_port(&SG_PREFERENCES.replace("</Configuration>", "")),
        Err(PortError::Unclosed("Configuration"))
    );
    assert_eq!(
        chosen_port(&format!("{SG_PREFERENCES}<Configuration></Configuration>")),
        Err(PortError::Twice("Configuration"))
    );
    for (port, error) in [
        (
            "<LocalLanPort>6c:24:08:e5:9c:91</LocalLanPort><LocalLanPort>x</LocalLanPort>",
            PortError::Twice("LocalLanPort"),
        ),
        (
            "<LocalLanPort>6c:24:08:e5:9c:91",
            PortError::Unclosed("LocalLanPort"),
        ),
        (
            "<LocalLanPort id=\"1\">6c:24:08:e5:9c:91</LocalLanPort>",
            PortError::Malformed("LocalLanPort"),
        ),
        (
            "<!-- <LocalLanPort>6c:24:08:e5:9c:91</LocalLanPort> -->",
            PortError::Comment,
        ),
        (
            "<LocalLanPort>Ethernet 4</LocalLanPort>",
            PortError::NotAMac("Ethernet 4".to_string()),
        ),
    ] {
        assert_eq!(chosen_port(&with_port(port)), Err(error), "{port:?}");
    }
    // A long value is quoted by its start only.
    let long = format!("<LocalLanPort>{}</LocalLanPort>", "x".repeat(60));
    assert_eq!(
        chosen_port(&with_port(&long)),
        Err(PortError::NotAMac("x".repeat(QUOTED_CHARS)))
    );
    assert_eq!(QUOTED_CHARS, 40);
}

#[test]
fn every_reason_reads_in_english() {
    let reasons: Vec<String> = [
        PortError::NoConfiguration,
        PortError::Twice("LocalLanPort"),
        PortError::Unclosed("Configuration"),
        PortError::Malformed("LocalLanPort"),
        PortError::Comment,
        PortError::NotAMac("eth0".to_string()),
    ]
    .iter()
    .map(PortError::why)
    .collect();
    assert_eq!(
        reasons,
        [
            "they have no <Configuration>",
            "<LocalLanPort> is there more than once",
            "<Configuration> is not closed",
            "<LocalLanPort> is malformed",
            "<Configuration> holds a comment",
            "LocalLanPort \"eth0\" is no MAC",
        ]
    );
}

// ---- the walk ----

/// What the walk gives, and the read errors it notes.
fn walk(
    reg: &Fake,
    preferences: FileRead,
    adapters: Result<Vec<IfRow>, u32>,
) -> (SoundGridFacts, Vec<String>) {
    let mut errors = Vec::new();
    let facts = soundgrid(reg, &preferences, &adapters, &mut errors);
    (facts, errors)
}

fn text(s: &str) -> FileRead {
    FileRead::Text(s.to_string())
}

#[test]
fn pp_s_soundgrid_adapter_is_the_one_its_driver_chose() {
    assert_eq!(
        walk(&pp(), text(SG_PREFERENCES), interfaces()),
        (
            SoundGridFacts::Adapter {
                mac: SG_MAC.to_string(),
                nic: soundgrid_facts(),
            },
            Vec::new()
        )
    );
    // Whatever adapter the driver chose is the one walked: never by name,
    // never by a binding.
    let dante = with_port("<LocalLanPort>6c:1f:f7:73:c7:d9</LocalLanPort>");
    assert_eq!(
        walk(&pp(), text(&dante), interfaces()),
        (
            SoundGridFacts::Adapter {
                mac: DVS_MAC.to_string(),
                nic: dante_facts(),
            },
            Vec::new()
        )
    );
}

#[test]
fn soundgrid_not_installed_has_no_facts_whatever_its_preferences() {
    for code in [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND] {
        let mut reg = pp();
        reg.subkeys_of(SOUNDGRID_SERVICE, Err(code));
        for preferences in [
            text(SG_PREFERENCES),
            text("garbage"),
            FileRead::Missing,
            FileRead::Failed("%APPDATA% is not set".to_string()),
        ] {
            assert_eq!(
                walk(&reg, preferences.clone(), Err(87)),
                (SoundGridFacts::NotInstalled, Vec::new()),
                "{code} {preferences:?}"
            );
        }
    }
}

#[test]
fn soundgrid_with_no_port_chosen_is_unset_and_no_read_error() {
    for preferences in [
        FileRead::Missing,
        text(&with_port("<LocalLanPort></LocalLanPort>")),
        text(&with_port("")),
    ] {
        assert_eq!(
            walk(&pp(), preferences.clone(), interfaces()),
            (SoundGridFacts::PortUnset, Vec::new()),
            "{preferences:?}"
        );
    }
}

#[test]
fn a_chosen_port_no_adapter_has_is_missing() {
    let other = with_port("<LocalLanPort>02:00:00:00:00:01</LocalLanPort>");
    assert_eq!(
        walk(&pp(), text(&other), interfaces()),
        (
            SoundGridFacts::PortMissing {
                mac: "020000000001".to_string()
            },
            Vec::new()
        )
    );
}

/// A registry, the preferences, the interfaces and the reason.
type UnreadableCase = (Fake, FileRead, Result<Vec<IfRow>, u32>, &'static str);

#[test]
fn soundgrid_that_cannot_be_read_says_why() {
    let mut service = pp();
    service.subkeys_of(SOUNDGRID_SERVICE, Err(5));
    let two = Ok(vec![
        row(SG_GUID, SG_MAC, true, false),
        row("{B}", SG_MAC, true, false),
    ]);
    let cases: [UnreadableCase; 5] = [
        (
            service,
            text(SG_PREFERENCES),
            interfaces(),
            "the SoundGrid Protocol's service could not be read (error 5)",
        ),
        (
            pp(),
            FileRead::Failed("%APPDATA% is not set".to_string()),
            interfaces(),
            "the SoundGrid driver's preferences could not be read: %APPDATA% is not set",
        ),
        (
            pp(),
            text(&with_port("<LocalLanPort>6c:24:08:e5:9c:91")),
            interfaces(),
            "the SoundGrid driver's preferences do not read: <LocalLanPort> is not closed",
        ),
        (
            pp(),
            text(SG_PREFERENCES),
            Err(87),
            "the network interfaces could not be listed (error 87)",
        ),
        (
            pp(),
            text(SG_PREFERENCES),
            two,
            "the SoundGrid driver's port: 2 network adapters have the MAC 6c2408e59c91",
        ),
    ];
    for (reg, preferences, adapters, why) in cases {
        assert_eq!(
            walk(&reg, preferences, adapters),
            (SoundGridFacts::Unreadable, vec![why.to_string()]),
            "{why}"
        );
    }
}
