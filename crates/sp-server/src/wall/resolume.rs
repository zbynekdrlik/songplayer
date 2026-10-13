//! #259: Resolume Arena's own saved output setup — the ONE thing the wall
//! report compares the box with (the owner's rule: whatever the operator
//! set up in Arena is the truth; SongPlayer keeps no layout of its own).
//! Parsed with roxmltree (a read-only XML tree; a DTD is refused, so no
//! entity expansion) into typed values; every file is read bounded
//! ([`read_bounded`]). Pure and Linux-tested.
//!
//! Arena keeps its setup in the Documents folder of the Windows user it
//! runs as. SongPlayer reads its OWN user's Documents (`%USERPROFILE%`):
//! on SNV both run as the same user. A box where they do not reads files
//! that are not Arena's (or none: then the simple output is assumed).
//!
//! - `Resolume Arena\Preferences\SimpleOutput.xml`: `<SimpleSetup
//!   advancedModeEnabled="1">` = the advanced output is on; no file =
//!   Arena's default, the simple output;
//! - `Resolume Arena\Preferences\AdvancedOutput.xml`: `<ScreenSetup
//!   presetFile="NAME">`, the preset the advanced output was saved as;
//! - `Resolume Arena\Presets\Advanced Output\NAME.xml`: every
//!   `<OutputDeviceDisplay name="Display 1"
//!   deviceId="\Registry\Machine\System\CurrentControlSet\Control\Video\{GUID}\0000"
//!   fullscreen="1" width="7680" height="1080">` is a physical display it
//!   drives (its Spout, NDI, virtual and DeckLink outputs are other
//!   elements, never read). `deviceId` is the Windows display's device key.

use std::ffi::OsString;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use roxmltree::{Document, Node};
use sp_core::wall::WantedDisplay;

/// The largest Arena file read, bytes (a preset is a few kB).
pub const MAX_FILE_BYTES: u64 = 1_048_576;

/// The Documents folder of SongPlayer's user: `%USERPROFILE%\Documents`.
pub fn documents(user_profile: Option<OsString>) -> Result<PathBuf, String> {
    match user_profile {
        Some(profile) if !profile.is_empty() => Ok(PathBuf::from(profile).join("Documents")),
        _ => Err("USERPROFILE is not set: no Documents folder to read".to_string()),
    }
}

/// Arena's folder in `documents`.
fn arena_dir(documents: &Path) -> PathBuf {
    documents.join("Resolume Arena")
}

/// `Resolume Arena\Preferences\SimpleOutput.xml`.
pub fn simple_output_file(documents: &Path) -> PathBuf {
    arena_dir(documents)
        .join("Preferences")
        .join("SimpleOutput.xml")
}

/// `Resolume Arena\Preferences\AdvancedOutput.xml`.
pub fn advanced_output_file(documents: &Path) -> PathBuf {
    arena_dir(documents)
        .join("Preferences")
        .join("AdvancedOutput.xml")
}

/// `Resolume Arena\Presets\Advanced Output\<preset>.xml`.
pub fn preset_file(documents: &Path, preset: &str) -> PathBuf {
    arena_dir(documents)
        .join("Presets")
        .join("Advanced Output")
        .join(format!("{preset}.xml"))
}

/// A file's text, `None` when it does not exist; one over
/// [`MAX_FILE_BYTES`] or that is not UTF-8 is refused.
pub fn read_bounded(path: &Path) -> Result<Option<String>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!("{}: over {MAX_FILE_BYTES} bytes", path.display()));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|e| format!("{}: not UTF-8 ({e})", path.display()))
}

/// The XML tree of `xml` (a UTF-8 BOM is skipped by the parser).
fn parse(xml: &str) -> Result<Document<'_>, String> {
    Document::parse(xml).map_err(|e| format!("not XML: {e}"))
}

/// The first element named `tag`, anywhere in the tree.
fn element<'a, 'input>(doc: &'a Document<'input>, tag: &str) -> Option<Node<'a, 'input>> {
    doc.descendants().find(|node| node.has_tag_name(tag))
}

/// `SimpleOutput.xml`: Arena's advanced output is on
/// (`advancedModeEnabled` `1`) or off (`0`).
pub fn advanced_mode(xml: &str) -> Result<bool, String> {
    let doc = parse(xml)?;
    let setup = element(&doc, "SimpleSetup").ok_or("no <SimpleSetup>")?;
    match setup.attribute("advancedModeEnabled") {
        Some("1") => Ok(true),
        Some("0") => Ok(false),
        other => Err(format!(
            "<SimpleSetup> advancedModeEnabled is {other:?}, not 0 or 1"
        )),
    }
}

/// `AdvancedOutput.xml`: the preset the advanced output was saved as — a
/// plain file name (never a path out of the presets folder).
pub fn preset_name(xml: &str) -> Result<String, String> {
    let doc = parse(xml)?;
    let setup = element(&doc, "ScreenSetup").ok_or("no <ScreenSetup>")?;
    let name = setup
        .attribute("presetFile")
        .ok_or("<ScreenSetup> has no presetFile")?;
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(|c| matches!(c, '/' | '\\' | ':'))
    {
        return Err(format!("presetFile {name:?} is no file name"));
    }
    Ok(name.to_string())
}

/// One `<OutputDeviceDisplay>`: `name`, `deviceId`, `width` and `height`
/// required; `fullscreen` `1` (or absent, Arena's default) or `0`.
fn wanted_display(node: &Node<'_, '_>) -> Result<WantedDisplay, String> {
    let attribute = |name: &str| {
        node.attribute(name)
            .ok_or_else(|| format!("<OutputDeviceDisplay> has no {name}"))
    };
    let number = |name: &str| {
        attribute(name)?
            .trim()
            .parse::<u32>()
            .map_err(|e| format!("<OutputDeviceDisplay> {name}: {e}"))
    };
    let fullscreen = match node.attribute("fullscreen") {
        None | Some("1") => true,
        Some("0") => false,
        Some(other) => {
            return Err(format!(
                "<OutputDeviceDisplay> fullscreen is {other:?}, not 0 or 1"
            ));
        }
    };
    Ok(WantedDisplay {
        name: attribute("name")?.to_string(),
        device_id: attribute("deviceId")?.to_string(),
        width: number("width")?,
        height: number("height")?,
        fullscreen,
    })
}

/// A preset: every physical display it drives, each once (by device id,
/// ASCII case ignored), in the preset's order.
pub fn wanted_displays(xml: &str) -> Result<Vec<WantedDisplay>, String> {
    let doc = parse(xml)?;
    let mut wanted: Vec<WantedDisplay> = Vec::new();
    for node in doc
        .descendants()
        .filter(|node| node.has_tag_name("OutputDeviceDisplay"))
    {
        let display = wanted_display(&node)?;
        if !wanted
            .iter()
            .any(|w| w.device_id.eq_ignore_ascii_case(&display.device_id))
        {
            wanted.push(display);
        }
    }
    Ok(wanted)
}

#[cfg(test)]
#[path = "resolume_tests.rs"]
mod tests;
