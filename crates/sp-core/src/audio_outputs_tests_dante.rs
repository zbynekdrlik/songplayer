//! The Dante entry of the output list (#285).

use super::*;

fn dante_entry(id: &str, device: &str, ip: &str) -> OutputEntry {
    OutputEntry::dante(
        id,
        "Dante",
        DanteDest {
            device: device.to_string(),
            ip: ip.to_string(),
        },
    )
}

fn field_problem(e: &OutputEntry) -> (&'static str, Problem) {
    let err = validate_entry(0, e).unwrap_err();
    (err.field, err.problem)
}

#[test]
fn a_dante_entry_names_its_device_and_address() {
    let e = dante_entry("out-1", "SongPlayer", "10.77.9.210");
    assert_eq!(e.kind, OutputType::Dante);
    assert!(e.enabled);
    assert_eq!(validate_entry(0, &e), Ok(()));
    assert_eq!(OutputType::parse("dante"), Some(OutputType::Dante));
    assert_eq!(OutputType::Dante.as_str(), "dante");
    assert_eq!(OutputType::NAMES, "vban, asio or dante");
    assert_eq!(asio_driver(&e), None);
    let text = serde_json::to_string(&e).unwrap();
    assert_eq!(serde_json::from_str::<OutputEntry>(&text).unwrap(), e);
}

#[test]
fn a_dante_device_name_follows_dantes_own_rule() {
    let long = "a".repeat(MAX_DANTE_NAME_LEN + 1);
    for (name, problem) in [
        ("", Problem::Empty),
        (long.as_str(), Problem::TooLong),
        ("-SongPlayer", Problem::BadCharacters),
        ("SongPlayer-", Problem::BadCharacters),
        ("Song Player", Problem::BadCharacters),
        ("Song_Player", Problem::BadCharacters),
    ] {
        let e = dante_entry("out-1", name, "10.77.9.210");
        assert_eq!(field_problem(&e), ("dante.device", problem), "{name}");
    }
    let full = "a".repeat(MAX_DANTE_NAME_LEN);
    assert_eq!(
        validate_entry(0, &dante_entry("out-1", &full, "10.0.0.1")),
        Ok(())
    );
    assert_eq!(
        validate_entry(0, &dante_entry("out-1", "sp-1", "10.0.0.1")),
        Ok(())
    );
}

#[test]
fn a_dante_address_is_one_ipv4_of_this_pc() {
    for ip in ["", "dante.lan", "10.0.0", "0.0.0.0", "::1"] {
        let e = dante_entry("out-1", "SongPlayer", ip);
        assert_eq!(field_problem(&e), ("dante.ip", Problem::BadAddress), "{ip}");
    }
}

#[test]
fn a_dante_entry_needs_its_block() {
    let mut e = dante_entry("out-1", "SongPlayer", "10.0.0.1");
    e.dante = None;
    assert_eq!(field_problem(&e), ("dante", Problem::Missing));
}

#[test]
fn the_list_holds_one_dante_device() {
    let list = vec![
        dante_entry("out-1", "SongPlayer", "10.0.0.1"),
        dante_entry("out-2", "SongPlayer2", "10.0.0.2"),
    ];
    assert_eq!(max_of_type(OutputType::Dante), MAX_DANTE_OUTPUTS);
    assert_eq!(MAX_DANTE_OUTPUTS, 1);
    assert_eq!(
        validate_list(&list),
        Err(ListError::TooManyOfType {
            kind: OutputType::Dante,
            count: 2,
            max: 1
        })
    );
}

#[test]
fn a_new_dante_entry_starts_switched_off_with_no_address() {
    let e = new_dante(&[]);
    assert_eq!(e.id, "out-1");
    assert!(!e.enabled, "the production gate (#285)");
    assert_eq!(e.dante.as_ref().unwrap().device, "SongPlayer");
    assert_eq!(field_problem(&e), ("dante.ip", Problem::BadAddress));
}
