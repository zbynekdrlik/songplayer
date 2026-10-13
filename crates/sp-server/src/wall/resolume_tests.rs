//! #259: reading Resolume Arena's saved output setup — its files' places,
//! the bounded read, the advanced-output switch, the preset's name and the
//! displays a preset drives.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sp_core::wall::WantedDisplay;

use super::*;

/// SNV's Display 1 device key (the span, 13.10.2026).
const SNV_KEY: &str = r"\Registry\Machine\System\CurrentControlSet\Control\Video\{49D71F75-AF5A-11EE-BACA-806E6F6E6963}\0000";

/// A preset in Arena's shape: a screen on the span, a Spout screen and a
/// virtual one.
fn snv_preset() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>

<XmlState name="snv surround 7680">
  <ScreenSetup name="ScreenSetup">
    <screens>
      <Screen name="LG wall" uniqueId="1">
        <OutputDevice>
          <OutputDeviceDisplay name="Display 1" deviceId="{SNV_KEY}" idHash="1029384756" fullscreen="1" width="7680" height="1080"/>
        </OutputDevice>
      </Screen>
      <Screen name="Spout">
        <OutputDevice>
          <OutputDeviceSpout name="Arena" width="2580" height="360"/>
        </OutputDevice>
      </Screen>
      <Screen name="Test">
        <OutputDevice>
          <OutputDeviceVirtual name="Virtual Output" width="800" height="600"/>
        </OutputDevice>
      </Screen>
    </screens>
  </ScreenSetup>
</XmlState>
"#
    )
}

fn snv_display() -> WantedDisplay {
    WantedDisplay {
        name: "Display 1".to_string(),
        device_id: SNV_KEY.to_string(),
        width: 7680,
        height: 1080,
        fullscreen: true,
    }
}

/// One `<OutputDeviceDisplay>` with `attributes` in a preset.
fn preset_with(attributes: &str) -> String {
    format!("<XmlState><Screen><OutputDeviceDisplay {attributes}/></Screen></XmlState>")
}

/// The installer ships THIRD-PARTY-NOTICES.txt; it must carry the MIT
/// notice of the roxmltree this crate pins (re-copy it when the pin moves).
#[test]
fn the_installer_notice_carries_the_pinned_roxmltrees_license() {
    const NOTICE: &str = include_str!("../../../../src-tauri/resources/THIRD-PARTY-NOTICES.txt");
    const MANIFEST: &str = include_str!("../../Cargo.toml");
    let notice = NOTICE.replace("\r\n", "\n");
    assert!(MANIFEST.contains("roxmltree = \"=0.21.1\""), "the pin");
    assert!(
        notice.contains("roxmltree 0.21.1"),
        "the notice names the pinned version"
    );
    assert!(notice.contains("Copyright (c) 2018 Yevhenii Reizner"));
}

#[test]
fn the_files_lie_in_arenas_documents_folder() {
    let docs = PathBuf::from("Users").join("Resolume").join("Documents");
    let arena = docs.join("Resolume Arena");
    assert_eq!(
        simple_output_file(&docs),
        arena.join("Preferences").join("SimpleOutput.xml")
    );
    assert_eq!(
        advanced_output_file(&docs),
        arena.join("Preferences").join("AdvancedOutput.xml")
    );
    assert_eq!(
        preset_file(&docs, "snv surround 7680"),
        arena
            .join("Presets")
            .join("Advanced Output")
            .join("snv surround 7680.xml")
    );
}

#[test]
fn the_documents_folder_is_the_users_profile_s() {
    assert_eq!(
        documents(Some(OsString::from("profile"))),
        Ok(Path::new("profile").join("Documents"))
    );
    for unset in [None, Some(OsString::new())] {
        assert_eq!(
            documents(unset),
            Err("USERPROFILE is not set: no Documents folder to read".to_string())
        );
    }
}

#[test]
fn a_file_is_read_whole_up_to_its_bound() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(read_bounded(&dir.path().join("absent.xml")), Ok(None));
    let small = dir.path().join("small.xml");
    std::fs::write(&small, "<SimpleSetup/>").unwrap();
    assert_eq!(read_bounded(&small), Ok(Some("<SimpleSetup/>".to_string())));
    let at_bound = dir.path().join("at_bound.xml");
    std::fs::write(&at_bound, "a".repeat(MAX_FILE_BYTES as usize)).unwrap();
    assert_eq!(
        read_bounded(&at_bound).unwrap().map(|text| text.len()),
        Some(MAX_FILE_BYTES as usize)
    );
    let over = dir.path().join("over.xml");
    std::fs::write(&over, "a".repeat(MAX_FILE_BYTES as usize + 1)).unwrap();
    assert_eq!(
        read_bounded(&over),
        Err(format!("{}: over 1048576 bytes", over.display()))
    );
    let binary = dir.path().join("binary.xml");
    std::fs::write(&binary, [0xFF_u8, 0xFE, 0x00]).unwrap();
    let refused = read_bounded(&binary).unwrap_err();
    assert!(refused.contains("not UTF-8"), "{refused}");
}

/// A path through a regular file fails to open for another reason than
/// "not found" (Linux: not a directory), and is refused, not taken as
/// absent. (Windows answers "not found" there: Unix only.)
#[cfg(unix)]
#[test]
fn a_file_that_cannot_be_opened_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    std::fs::write(&file, "x").unwrap();
    let through = file.join("SimpleOutput.xml");
    let refused = read_bounded(&through).unwrap_err();
    assert!(
        refused.starts_with(&format!("{}: ", through.display())),
        "{refused}"
    );
}

#[test]
fn the_advanced_output_switch_is_read() {
    let on = r#"<?xml version="1.0" encoding="UTF-8"?><SimpleSetup advancedModeEnabled="1"/>"#;
    assert_eq!(advanced_mode(on), Ok(true));
    let nested = r#"<XmlState><SimpleSetup width="1920" advancedModeEnabled="0"/></XmlState>"#;
    assert_eq!(advanced_mode(nested), Ok(false));
    // A UTF-8 BOM, as an editor may save it.
    assert_eq!(advanced_mode(&format!("\u{FEFF}{on}")), Ok(true));
    assert_eq!(
        advanced_mode(r#"<SimpleSetup advancedModeEnabled="yes"/>"#),
        Err(r#"<SimpleSetup> advancedModeEnabled is Some("yes"), not 0 or 1"#.to_string())
    );
    assert_eq!(
        advanced_mode("<SimpleSetup/>"),
        Err("<SimpleSetup> advancedModeEnabled is None, not 0 or 1".to_string())
    );
    assert_eq!(
        advanced_mode("<XmlState/>"),
        Err("no <SimpleSetup>".to_string())
    );
    assert!(
        advanced_mode("<SimpleSetup")
            .unwrap_err()
            .starts_with("not XML: ")
    );
}

#[test]
fn the_preset_name_is_presetfile_or_none_saved() {
    let setup = |name: &str| {
        format!(r#"<XmlState><ScreenSetup name="ScreenSetup" presetFile="{name}"/></XmlState>"#)
    };
    assert_eq!(
        preset_name(&setup("snv surround 7680")),
        Ok(Some("snv surround 7680".to_string()))
    );
    assert_eq!(preset_name(&setup("../x")), Ok(Some("../x".to_string())));
    // No preset saved: never an error.
    for none in [
        setup("").as_str(),
        "<XmlState><ScreenSetup/></XmlState>",
        "<XmlState/>",
    ] {
        assert_eq!(preset_name(none), Ok(None), "{none}");
    }
    // Only text that is no XML fails the read.
    assert!(
        preset_name("<ScreenSetup")
            .unwrap_err()
            .starts_with("not XML: ")
    );
}

#[test]
fn a_preset_name_is_used_only_as_a_plain_file_name() {
    for good in ["snv surround 7680", "a.b", "..a", "x."] {
        assert!(is_file_name(good), "{good}");
    }
    for bad in ["", ".", "..", "../x", r"..\x", "C:x", "a/b"] {
        assert!(!is_file_name(bad), "{bad}");
    }
}

#[test]
fn a_preset_names_the_physical_displays_it_drives() {
    assert_eq!(wanted_displays(&snv_preset()), Ok(vec![snv_display()]));
}

#[test]
fn every_display_counts_once_in_the_presets_order() {
    let second = r"\Registry\Machine\System\CurrentControlSet\Control\Video\{49D71F75-AF5A-11EE-BACA-806E6F6E6963}\0001";
    let preset = format!(
        r#"<XmlState>
  <Screen><OutputDeviceDisplay name="Display 2" deviceId="{second}" fullscreen="0" width="1920" height="1080"/></Screen>
  <Screen><OutputDeviceDisplay name="Display 1" deviceId="{SNV_KEY}" width=" 7680 " height="1080"/></Screen>
  <Screen><OutputDeviceDisplay name="Display 1" deviceId="{}" fullscreen="1" width="7680" height="1080"/></Screen>
</XmlState>"#,
        SNV_KEY.to_ascii_lowercase()
    );
    assert_eq!(
        wanted_displays(&preset),
        Ok(vec![
            WantedDisplay {
                name: "Display 2".to_string(),
                device_id: second.to_string(),
                width: 1920,
                height: 1080,
                fullscreen: false,
            },
            snv_display(),
        ])
    );
}

#[test]
fn a_preset_with_no_physical_display_wants_none() {
    let spout = r#"<XmlState><Screen><OutputDeviceSpout name="Arena"/></Screen></XmlState>"#;
    assert_eq!(wanted_displays(spout), Ok(Vec::new()));
}

#[test]
fn a_display_that_is_not_one_refuses_the_preset_by_name() {
    let key = format!(r#"deviceId="{SNV_KEY}""#);
    let refused = [
        (
            format!(r#"{key} width="7680" height="1080""#),
            "<OutputDeviceDisplay> has no name",
        ),
        (
            r#"name="Display 1" width="7680" height="1080""#.to_string(),
            "<OutputDeviceDisplay> has no deviceId",
        ),
        (
            format!(r#"name="Display 1" {key} height="1080""#),
            "<OutputDeviceDisplay> has no width",
        ),
        (
            format!(r#"name="Display 1" {key} width="7680""#),
            "<OutputDeviceDisplay> has no height",
        ),
        (
            format!(r#"name="Display 1" {key} fullscreen="2" width="7680" height="1080""#),
            r#"<OutputDeviceDisplay> fullscreen is "2", not 0 or 1"#,
        ),
    ];
    for (attributes, why) in refused {
        assert_eq!(
            wanted_displays(&preset_with(&attributes)),
            Err(why.to_string()),
            "{attributes}"
        );
    }
    let bad_width = wanted_displays(&preset_with(&format!(
        r#"name="Display 1" {key} width="wide" height="1080""#
    )))
    .unwrap_err();
    assert!(
        bad_width.starts_with("<OutputDeviceDisplay> width: "),
        "{bad_width}"
    );
    assert!(wanted_displays("<a>").unwrap_err().starts_with("not XML: "));
    // A DTD is refused (no entity is ever expanded).
    let dtd = r#"<!DOCTYPE x [<!ENTITY e "e">]><x>&e;</x>"#;
    assert!(wanted_displays(dtd).unwrap_err().starts_with("not XML: "));
}
