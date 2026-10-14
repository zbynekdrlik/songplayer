//! #255: SoundGrid's adapter is the one its driver uses, from the driver's
//! own port choice (pure, Linux-tested; the glue only reads the file).
//!
//! Measured on PP (13.10.2026, #255 comment 6138104246): the SoundGrid
//! Protocol's `Linkage\Bind` lists 14 `\Device\{GUID}` — the SoundGrid
//! adapter, two Wi-Fi Direct virtual adapters, the not-present kernel
//! debugger adapter and about ten adapters that no longer exist — so the
//! bindings name no adapter (gating them would fail PP on adapters SoundGrid
//! never uses). The SoundGrid ASIO driver keeps its chosen port in
//! `%APPDATA%\Waves Audio\Preferences\SoundGrid.Driver.preferences`
//! (`<Configuration><LocalLanPort>6c:24:08:e5:9c:91</LocalLanPort>`): the
//! file of the user SongPlayer and its ASIO host run as, so the very file
//! the driver they host reads. The adapter with that MAC is SoundGrid's.

use super::{
    ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, FileRead, IfRow, MacMiss, RegistryRead,
    SOUNDGRID_SERVICE, adapter_for_mac, nic, normalize_mac,
};
use crate::prod_env::audio::SoundGridFacts;

/// The most of a `LocalLanPort` that is not a MAC that a read error quotes.
pub const QUOTED_CHARS: usize = 40;

/// Why the driver's preferences do not tell a port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortError {
    /// No `<Configuration>` element.
    NoConfiguration,
    /// An element there more than once.
    Twice(&'static str),
    /// An element opened and not closed, or closed and not opened.
    Unclosed(&'static str),
    /// An element with attributes, or of another form.
    Malformed(&'static str),
    /// A comment inside `<Configuration>`: what it hides cannot be told.
    Comment,
    /// `LocalLanPort` holds no MAC (its start, at most [`QUOTED_CHARS`]).
    NotAMac(String),
}

impl PortError {
    /// The reason in English (the read errors).
    pub fn why(&self) -> String {
        match self {
            Self::NoConfiguration => "they have no <Configuration>".to_string(),
            Self::Twice(name) => format!("<{name}> is there more than once"),
            Self::Unclosed(name) => format!("<{name}> is not closed"),
            Self::Malformed(name) => format!("<{name}> is malformed"),
            Self::Comment => "<Configuration> holds a comment".to_string(),
            Self::NotAMac(text) => format!("LocalLanPort {text:?} is no MAC"),
        }
    }
}

/// The text of the one element `name` in `text`: `Ok(None)` when there is
/// none, `Ok(Some(""))` for an empty one (`<name/>`, `<name />`). An
/// element there twice, not closed, with attributes, or a name that only
/// starts with `name` is an error (a strict reading of a tiny file, never
/// a search that takes garbage).
pub fn element_text<'a>(text: &'a str, name: &'static str) -> Result<Option<&'a str>, PortError> {
    let opening = format!("<{name}");
    let closing = format!("</{name}>");
    let opens = text.matches(opening.as_str()).count();
    let closes = text.matches(closing.as_str()).count();
    if opens == 0 {
        return if closes == 0 {
            Ok(None)
        } else {
            Err(PortError::Unclosed(name))
        };
    }
    if opens > 1 || closes > 1 {
        return Err(PortError::Twice(name));
    }
    let Some(start) = text.find(opening.as_str()) else {
        return Ok(None);
    };
    let after = &text[start + opening.len()..];
    if let Some(inside) = after.strip_prefix('>') {
        return match inside.find(closing.as_str()) {
            Some(end) => Ok(Some(&inside[..end])),
            None => Err(PortError::Unclosed(name)),
        };
    }
    if after.trim_start().starts_with("/>") && closes == 0 {
        return Ok(Some(""));
    }
    Err(PortError::Malformed(name))
}

/// The port the driver's preferences choose: `<LocalLanPort>` inside
/// `<Configuration>` (a leading BOM skipped), normalized as a MAC
/// (`normalize_mac`); `Ok(None)` when there is no `LocalLanPort` or it is
/// empty (no port chosen).
pub fn chosen_port(text: &str) -> Result<Option<String>, PortError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(configuration) = element_text(text, "Configuration")? else {
        return Err(PortError::NoConfiguration);
    };
    if configuration.contains("<!--") {
        return Err(PortError::Comment);
    }
    let Some(port) = element_text(configuration, "LocalLanPort")? else {
        return Ok(None);
    };
    let port = port.trim();
    if port.is_empty() {
        return Ok(None);
    }
    match normalize_mac(port) {
        Some(mac) => Ok(Some(mac)),
        None => Err(PortError::NotAMac(
            port.chars().take(QUOTED_CHARS).collect(),
        )),
    }
}

/// SoundGrid on this box: not installed (no SoundGrid Protocol service,
/// whatever its preferences say), its port unset (no preferences file, no
/// or an empty `LocalLanPort`), the port's MAC on no adapter, or the
/// adapter with that MAC walked like any other (`nic`). What cannot be read
/// says why.
pub fn soundgrid(
    reg: &dyn RegistryRead,
    preferences: &FileRead,
    adapters: &Result<Vec<IfRow>, u32>,
    errors: &mut Vec<String>,
) -> SoundGridFacts {
    match reg.subkeys(SOUNDGRID_SERVICE) {
        Ok(_) => {}
        Err(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => return SoundGridFacts::NotInstalled,
        Err(code) => {
            errors.push(format!(
                "the SoundGrid Protocol's service could not be read (error {code})"
            ));
            return SoundGridFacts::Unreadable;
        }
    }
    let text = match preferences {
        FileRead::Missing => return SoundGridFacts::PortUnset,
        FileRead::Failed(why) => {
            errors.push(format!(
                "the SoundGrid driver's preferences could not be read: {why}"
            ));
            return SoundGridFacts::Unreadable;
        }
        FileRead::Text(text) => text,
    };
    let mac = match chosen_port(text) {
        Ok(Some(mac)) => mac,
        Ok(None) => return SoundGridFacts::PortUnset,
        Err(e) => {
            errors.push(format!(
                "the SoundGrid driver's preferences do not read: {}",
                e.why()
            ));
            return SoundGridFacts::Unreadable;
        }
    };
    let rows = match adapters {
        Ok(rows) => rows,
        Err(code) => {
            errors.push(format!(
                "the network interfaces could not be listed (error {code})"
            ));
            return SoundGridFacts::Unreadable;
        }
    };
    match adapter_for_mac(rows, &mac) {
        Ok(row) => SoundGridFacts::Adapter {
            nic: nic(reg, &row.guid, errors),
            mac,
        },
        Err(MacMiss::NoneHas) => SoundGridFacts::PortMissing { mac },
        Err(miss) => {
            errors.push(format!("the SoundGrid driver's port: {}", miss.why(&mac)));
            SoundGridFacts::Unreadable
        }
    }
}

#[cfg(test)]
#[path = "prod_env_audio_soundgrid_tests.rs"]
mod tests;
