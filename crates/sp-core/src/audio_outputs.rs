//! #233: SongPlayer's program audio outputs — ONE list (`audio_outputs`
//! setting) of destinations, each in the transport it supports, and the audio
//! network's sample rate (`audio_network_rate`). WASM-safe: the dashboard
//! edits these types and runs the same validation the server runs on a
//! settings PATCH (`sp-server` `playback/audio_out_config.rs`).
//!
//! An entry is built with [`OutputEntry::vban`] / [`OutputEntry::asio`] or by
//! the server's parser, never by a struct literal elsewhere, so a new
//! transport's field touches only those places.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::DEFAULT_VBAN_STREAM_NAME;

/// The rates an output may run at, Hz (the VBAN rate indexes SongPlayer sends).
pub const SUPPORTED_RATES: [u32; 5] = [44_100, 48_000, 88_200, 96_000, 192_000];
/// The program's own rate (media is made 48 kHz offline, `normalize.rs`).
pub const PROGRAM_RATE: u32 = 48_000;
/// `audio_network_rate` when unset or unreadable.
pub const DEFAULT_NETWORK_RATE: u32 = 48_000;
pub const MAX_OUTPUTS: usize = 16;
/// VBAN entries: each is one paced MMCSS thread (#210's `VBAN_MAX_TARGETS`).
pub const MAX_VBAN_OUTPUTS: usize = 8;
/// ASIO entries: each owns one driver on its own worker thread.
pub const MAX_ASIO_OUTPUTS: usize = 4;
/// An ASIO driver's registry description, characters.
pub const MAX_DRIVER_NAME_LEN: usize = 128;
/// The highest ASIO output channel an entry names (0-based; shown 1-based).
pub const MAX_ASIO_CHANNEL: u32 = 511;
pub const MAX_DELAY_MS: u32 = 2_000;
pub const MAX_ID_LEN: usize = 32;
pub const MAX_NAME_LEN: usize = 64;
pub const MAX_HOST_LEN: usize = 253;
pub const MAX_STREAM_NAME_LEN: usize = 16;
pub const DEFAULT_VBAN_PORT: u16 = 6980;

/// The transport of an output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputType {
    Vban,
    Asio,
}

impl OutputType {
    /// The values `type` accepts, for an error text.
    pub const NAMES: &'static str = "vban or asio";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vban => "vban",
            Self::Asio => "asio",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "vban" => Some(Self::Vban),
            "asio" => Some(Self::Asio),
            _ => None,
        }
    }
}

/// An output's rate: the network's (`audio_network_rate`) or a fixed one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RateChoice {
    #[default]
    Network,
    Fixed(u32),
}

impl Serialize for RateChoice {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Network => s.serialize_str("network"),
            Self::Fixed(hz) => s.serialize_u32(*hz),
        }
    }
}

impl<'de> Deserialize<'de> for RateChoice {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(RateVisitor)
    }
}

/// `"network"` or a whole number of Hz; anything else is refused.
struct RateVisitor;

impl Visitor<'_> for RateVisitor {
    type Value = RateChoice;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"network\" or a rate in Hz")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<RateChoice, E> {
        match v {
            "network" => Ok(RateChoice::Network),
            _ => Err(E::custom("a rate is \"network\" or a number")),
        }
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<RateChoice, E> {
        u32::try_from(v)
            .map(RateChoice::Fixed)
            .map_err(|_| E::custom("a rate is at most 4294967295"))
    }
}

/// A VBAN destination's sample format (VBAN spec rev. 13, p. 9).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VbanSampleFormat {
    Int16,
    #[default]
    Int24,
    Float32,
}

impl VbanSampleFormat {
    /// The values `vban.format` accepts, for an error text.
    pub const NAMES: &'static str = "int16, int24 or float32";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Int16 => "int16",
            Self::Int24 => "int24",
            Self::Float32 => "float32",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "int16" => Some(Self::Int16),
            "int24" => Some(Self::Int24),
            "float32" => Some(Self::Float32),
            _ => None,
        }
    }
}

fn default_stream_name() -> String {
    DEFAULT_VBAN_STREAM_NAME.to_string()
}

fn yes() -> bool {
    true
}

/// Where a VBAN output sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VbanDest {
    pub host: String,
    pub port: u16,
    #[serde(default = "default_stream_name")]
    pub stream_name: String,
    #[serde(default)]
    pub format: VbanSampleFormat,
}

/// Where an ASIO output plays: a registered driver (its registry
/// description) and its two output channels, left and right (0-based; the
/// dashboard shows them 1-based). The driver's rate, buffer and sample type
/// are the driver's own: an ASIO entry's `rate` is kept as stored, unused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsioDest {
    pub driver: String,
    pub channels: [u32; 2],
}

/// One output of the list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputEntry {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: OutputType,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub rate: RateChoice,
    #[serde(default)]
    pub delay_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vban: Option<VbanDest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asio: Option<AsioDest>,
}

impl OutputEntry {
    /// An enabled VBAN entry at the network rate, no delay.
    pub fn vban(id: &str, name: &str, dest: VbanDest) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            kind: OutputType::Vban,
            enabled: true,
            rate: RateChoice::Network,
            delay_ms: 0,
            vban: Some(dest),
            asio: None,
        }
    }

    /// An enabled ASIO entry, no delay (its rate is the driver's).
    pub fn asio(id: &str, name: &str, dest: AsioDest) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            kind: OutputType::Asio,
            enabled: true,
            rate: RateChoice::Network,
            delay_ms: 0,
            vban: None,
            asio: Some(dest),
        }
    }
}

/// What is wrong with one field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    Empty,
    TooLong,
    BadCharacters,
    Duplicate,
    UnsupportedRate,
    TooLarge,
    Missing,
    BadPort,
    OutOfRange,
    SameChannel,
    DriverTaken,
}

impl Problem {
    fn text(self) -> &'static str {
        match self {
            Self::Empty => "is empty",
            Self::TooLong => "is too long",
            Self::BadCharacters => "has a character that is not allowed",
            Self::Duplicate => "is used by an earlier entry",
            Self::UnsupportedRate => "must be \"network\" or 44100, 48000, 88200, 96000 or 192000",
            Self::TooLarge => "is over 2000 ms",
            Self::Missing => "is missing",
            Self::BadPort => "must be 1-65535",
            Self::OutOfRange => "must be 0-511",
            Self::SameChannel => "must name two different channels",
            Self::DriverTaken => {
                "is already used by an earlier ASIO entry (a driver takes one client)"
            }
        }
    }

    fn sk(self) -> &'static str {
        match self {
            Self::Empty => "je prázdne",
            Self::TooLong => "je príliš dlhé",
            Self::BadCharacters => "obsahuje nepovolený znak",
            Self::Duplicate => "má rovnakú hodnotu ako iný výstup",
            Self::UnsupportedRate => {
                "musí byť podľa siete alebo 44100, 48000, 88200, 96000 či 192000 Hz"
            }
            Self::TooLarge => "má viac ako 2000 ms",
            Self::Missing => "chýba",
            Self::BadPort => "musí byť 1 až 65535",
            // The dashboard shows the channels 1-based.
            Self::OutOfRange => "musí byť 1 až 512",
            Self::SameChannel => "musí obsahovať dva rôzne kanály",
            Self::DriverTaken => "je už použité iným výstupom ASIO (ovládač berie jedného klienta)",
        }
    }
}

/// The Slovak name of a field, for the dashboard (shown as `pole „…“`, so
/// every problem text agrees with the neuter "pole"); an unknown field as it is.
fn field_sk(field: &'static str) -> &'static str {
    match field {
        "id" => "identifikátor",
        "name" => "názov",
        "rate" => "frekvencia",
        "delay_ms" => "oneskorenie",
        "vban" => "nastavenie VBAN",
        "vban.host" => "cieľ",
        "vban.port" => "port",
        "vban.stream_name" => "názov streamu",
        "asio" => "nastavenie ASIO",
        "asio.driver" => "ovládač",
        "asio.channels" => "kanály",
        _ => field,
    }
}

/// One entry's first problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryError {
    /// 0-based; shown 1-based.
    pub index: usize,
    pub id: String,
    pub field: &'static str,
    pub problem: Problem,
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "entry {} (id {}): {} {}",
            self.index + 1,
            shown_id(&self.id),
            self.field,
            self.problem.text()
        )
    }
}

/// Why a list is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListError {
    TooMany {
        count: usize,
    },
    TooManyOfType {
        kind: OutputType,
        count: usize,
        max: usize,
    },
    Entry(EntryError),
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooMany { count } => {
                write!(
                    f,
                    "audio_outputs has {count} entries (at most {MAX_OUTPUTS})"
                )
            }
            Self::TooManyOfType { kind, count, max } => write!(
                f,
                "audio_outputs has {count} {} entries (at most {max})",
                kind.as_str()
            ),
            Self::Entry(e) => fmt::Display::fmt(e, f),
        }
    }
}

impl ListError {
    /// The same refusal in Slovak, for the dashboard.
    pub fn sk(&self) -> String {
        match self {
            Self::TooMany { count } => {
                format!("Výstupov je {count}, najviac môže byť {MAX_OUTPUTS}")
            }
            Self::TooManyOfType { kind, count, max } => format!(
                "Výstupov {} je {count}, najviac môže byť {max}",
                kind.as_str().to_uppercase()
            ),
            Self::Entry(e) => format!(
                "Výstup {} ({}): pole „{}“ {}",
                e.index + 1,
                shown_id(&e.id),
                field_sk(e.field),
                e.problem.sk()
            ),
        }
    }
}

/// An id as an error shows it: at most [`MAX_ID_LEN`] characters, each one
/// outside a-z 0-9 - shown as `?` (an error never echoes junk input).
pub fn shown_id(id: &str) -> String {
    id.chars()
        .take(MAX_ID_LEN)
        .map(|c| if id_char(c) { c } else { '?' })
        .collect()
}

fn id_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
}

fn id_problem(id: &str) -> Option<Problem> {
    if id.is_empty() {
        Some(Problem::Empty)
    } else if id.len() > MAX_ID_LEN {
        Some(Problem::TooLong)
    } else if !id.chars().all(id_char) {
        Some(Problem::BadCharacters)
    } else {
        None
    }
}

fn name_problem(name: &str) -> Option<Problem> {
    if name.trim().is_empty() {
        Some(Problem::Empty)
    } else if name.chars().count() > MAX_NAME_LEN {
        Some(Problem::TooLong)
    } else if name.chars().any(char::is_control) {
        Some(Problem::BadCharacters)
    } else {
        None
    }
}

fn host_problem(host: &str) -> Option<Problem> {
    if host.is_empty() {
        Some(Problem::Empty)
    } else if host.len() > MAX_HOST_LEN {
        Some(Problem::TooLong)
    } else if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
    {
        Some(Problem::BadCharacters)
    } else {
        None
    }
}

fn driver_problem(driver: &str) -> Option<Problem> {
    if driver.trim().is_empty() {
        Some(Problem::Empty)
    } else if driver.chars().count() > MAX_DRIVER_NAME_LEN {
        Some(Problem::TooLong)
    } else if driver.chars().any(char::is_control) {
        Some(Problem::BadCharacters)
    } else {
        None
    }
}

fn channels_problem(channels: [u32; 2]) -> Option<Problem> {
    if channels.iter().any(|&c| c > MAX_ASIO_CHANNEL) {
        Some(Problem::OutOfRange)
    } else if channels[0] == channels[1] {
        Some(Problem::SameChannel)
    } else {
        None
    }
}

fn stream_problem(name: &str) -> Option<Problem> {
    if name.is_empty() {
        Some(Problem::Empty)
    } else if name.len() > MAX_STREAM_NAME_LEN {
        Some(Problem::TooLong)
    } else if !name.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        Some(Problem::BadCharacters)
    } else {
        None
    }
}

/// The first problem of entry `index` on its own (the list rules — counts,
/// duplicate ids — are [`validate_list`]'s).
pub fn validate_entry(index: usize, e: &OutputEntry) -> Result<(), EntryError> {
    let err = |field: &'static str, problem: Problem| EntryError {
        index,
        id: e.id.clone(),
        field,
        problem,
    };
    if let Some(p) = id_problem(&e.id) {
        return Err(err("id", p));
    }
    if let Some(p) = name_problem(&e.name) {
        return Err(err("name", p));
    }
    if let RateChoice::Fixed(hz) = e.rate
        && !SUPPORTED_RATES.contains(&hz)
    {
        return Err(err("rate", Problem::UnsupportedRate));
    }
    if e.delay_ms > MAX_DELAY_MS {
        return Err(err("delay_ms", Problem::TooLarge));
    }
    match e.kind {
        OutputType::Vban => {
            let Some(v) = &e.vban else {
                return Err(err("vban", Problem::Missing));
            };
            if let Some(p) = host_problem(&v.host) {
                return Err(err("vban.host", p));
            }
            if v.port == 0 {
                return Err(err("vban.port", Problem::BadPort));
            }
            if let Some(p) = stream_problem(&v.stream_name) {
                return Err(err("vban.stream_name", p));
            }
        }
        OutputType::Asio => {
            let Some(a) = &e.asio else {
                return Err(err("asio", Problem::Missing));
            };
            if let Some(p) = driver_problem(&a.driver) {
                return Err(err("asio.driver", p));
            }
            if let Some(p) = channels_problem(a.channels) {
                return Err(err("asio.channels", p));
            }
        }
    }
    Ok(())
}

/// The driver of an ASIO entry (`None` for any other type).
pub fn asio_driver(e: &OutputEntry) -> Option<&str> {
    match e.kind {
        OutputType::Asio => e.asio.as_ref().map(|a| a.driver.as_str()),
        OutputType::Vban => None,
    }
}

/// `e` names the driver of an ASIO entry in `earlier` (a driver takes one
/// client; a switched-off entry still names its driver).
pub fn driver_taken(earlier: &[OutputEntry], e: &OutputEntry) -> bool {
    asio_driver(e).is_some_and(|d| earlier.iter().any(|p| asio_driver(p) == Some(d)))
}

/// How many entries of a type the list may hold.
pub fn max_of_type(kind: OutputType) -> usize {
    match kind {
        OutputType::Vban => MAX_VBAN_OUTPUTS,
        OutputType::Asio => MAX_ASIO_OUTPUTS,
    }
}

/// The whole list: at most [`MAX_OUTPUTS`] entries, [`MAX_VBAN_OUTPUTS`]
/// VBAN and [`MAX_ASIO_OUTPUTS`] ASIO ones, every entry valid, ids unique,
/// each ASIO driver named once ([`driver_taken`]).
pub fn validate_list(entries: &[OutputEntry]) -> Result<(), ListError> {
    if entries.len() > MAX_OUTPUTS {
        return Err(ListError::TooMany {
            count: entries.len(),
        });
    }
    for kind in [OutputType::Vban, OutputType::Asio] {
        let count = entries.iter().filter(|e| e.kind == kind).count();
        let max = max_of_type(kind);
        if count > max {
            return Err(ListError::TooManyOfType { kind, count, max });
        }
    }
    for (i, e) in entries.iter().enumerate() {
        validate_entry(i, e).map_err(ListError::Entry)?;
        let earlier = &entries[..i];
        let problem = if earlier.iter().any(|p| p.id == e.id) {
            Some(("id", Problem::Duplicate))
        } else if driver_taken(earlier, e) {
            Some(("asio.driver", Problem::DriverTaken))
        } else {
            None
        };
        if let Some((field, problem)) = problem {
            return Err(ListError::Entry(EntryError {
                index: i,
                id: e.id.clone(),
                field,
                problem,
            }));
        }
    }
    Ok(())
}

/// The rate an output with `rate` runs at on a network at `network` Hz.
pub fn effective_rate(rate: RateChoice, network: u32) -> u32 {
    match rate {
        RateChoice::Network => network,
        RateChoice::Fixed(hz) => hz,
    }
}

fn id_number(id: &str) -> Option<u64> {
    id.strip_prefix("out-")?.parse().ok()
}

/// The next free `out-N`: one above the highest N in use (a hand-edited id
/// at the top of the range saturates; validation then names the duplicate).
pub fn next_id(entries: &[OutputEntry]) -> String {
    let max = entries
        .iter()
        .filter_map(|e| id_number(&e.id))
        .max()
        .unwrap_or(0);
    format!("out-{}", max.saturating_add(1))
}

/// The entry the dashboard's "add VBAN" creates: its host is left for the
/// operator (validation refuses it empty).
pub fn new_vban(entries: &[OutputEntry]) -> OutputEntry {
    let id = next_id(entries);
    let n = id_number(&id).unwrap_or(1);
    OutputEntry::vban(
        &id,
        &format!("VBAN {n}"),
        VbanDest {
            host: String::new(),
            port: DEFAULT_VBAN_PORT,
            stream_name: default_stream_name(),
            format: VbanSampleFormat::Int24,
        },
    )
}

/// The entry the dashboard's "add ASIO" creates: the driver it is given
/// (the first one the box lists, or none, which validation refuses) on
/// channels 1 and 2.
pub fn new_asio(entries: &[OutputEntry], driver: &str) -> OutputEntry {
    let id = next_id(entries);
    let n = id_number(&id).unwrap_or(1);
    OutputEntry::asio(
        &id,
        &format!("ASIO {n}"),
        AsioDest {
            driver: driver.to_string(),
            channels: [0, 1],
        },
    )
}

/// The stored (0-based) ASIO channel of the dashboard's 1-based `shown`
/// one. A shown 0 is no channel: it wraps to `u32::MAX`, which validation
/// refuses ("musí byť 1 až 512"), and [`asio_channel_shown`] shows it as 0
/// again (#233 review round 1: it read as channel 1 without a word).
pub fn asio_channel_index(shown: u32) -> u32 {
    shown.wrapping_sub(1)
}

/// The dashboard's 1-based form of a stored ASIO channel (the inverse of
/// [`asio_channel_index`] for every `u32`).
pub fn asio_channel_shown(index: u32) -> u32 {
    index.wrapping_add(1)
}

/// Why the dashboard's "Pridať výstup ASIO" is off, or `None` when an ASIO
/// output can be added: the driver list is still loading (an empty text),
/// its read failed, or the box lists no driver (#233 review round 2: a row
/// with no driver to pick would only be refused).
pub fn asio_add_refusal(listed: Option<&[String]>, read_failed: bool) -> Option<&'static str> {
    match listed {
        Some([]) => Some("V systéme nie je žiadny ovládač ASIO"),
        Some(_) => None,
        None if read_failed => Some("Zoznam ovládačov ASIO sa nenačítal"),
        None => Some(""),
    }
}

/// The dashboard's driver choices for an ASIO entry, `(value, label)`: the
/// box's listed drivers, then the entry's own driver when the list lacks
/// it. That driver is marked "(nenájdený)" only when the list is KNOWN
/// (`Some`); while it is unknown (not read yet, or its read failed) it is
/// offered unmarked — the dashboard claims only what it was told (#225,
/// #233 review round 1). An entry with no driver adds nothing.
pub fn asio_driver_options(listed: Option<&[String]>, current: &str) -> Vec<(String, String)> {
    let mut options: Vec<(String, String)> = listed
        .unwrap_or_default()
        .iter()
        .map(|n| (n.clone(), n.clone()))
        .collect();
    if !current.is_empty() && !options.iter().any(|(value, _)| value == current) {
        let label = match listed {
            Some(_) => format!("{current} (nenájdený)"),
            None => current.to_string(),
        };
        options.push((current.to_string(), label));
    }
    options
}

/// The Slovak reason of a waiting ASIO output, by the server's stable code
/// (`sp-server` `asio_state::Reason::code`; its English text is the
/// tooltip). An unknown code (a later version's) reads as such.
pub fn asio_reason_sk(code: &str) -> &'static str {
    match code {
        "not_found" => "ovládač nie je v systéme",
        "busy" => "ovládač používa iný program",
        "refused" => "ovládač sa nedá použiť",
        "failed" => "chyba ovládača",
        "reset" => "ovládač sa reštartuje",
        "rate_changed" => "ovládač zmenil frekvenciu",
        "clock_lost" => "ovládač stratil hodinový signál",
        "stalled" => "ovládač neodpovedá",
        "windows_only" => "ASIO funguje len vo Windows",
        "held" => "predchádzajúci výstup ešte neuvoľnil ovládač",
        _ => "neznámy dôvod",
    }
}

/// The stream name as #210 put it on the wire (`vban_packet::stream_name_bytes`):
/// its first 16 characters, each non-ASCII or control character as `_`.
pub fn wire_stream_name(name: &str) -> String {
    name.chars()
        .take(MAX_STREAM_NAME_LEN)
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "audio_outputs_tests.rs"]
mod tests;
