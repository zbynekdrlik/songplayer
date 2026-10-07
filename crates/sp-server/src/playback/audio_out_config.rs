//! #233: the outputs' settings on the server. A PATCH of `audio_outputs` /
//! `audio_network_rate` is parsed strictly — every error names the entry, the
//! (sanitized) id and the field, never the value: serde's own error text can
//! quote the input — and stored normalized. The outputs task reads the stored
//! list leniently: an entry this version cannot read (a rollback, a hand-edited
//! row) is skipped and named in `problems`; the rest run. A stored value that
//! is no list changes nothing: what runs keeps running. Untrusted JSON goes
//! only through `Box<RawValue>` maps into typed fields, never into
//! `serde_json::Value` (`rust-workspace.md`).

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde_json::value::RawValue;
use sp_core::audio_outputs::{
    AsioDest, EntryError, MAX_ASIO_OUTPUTS, MAX_OUTPUTS, MAX_VBAN_OUTPUTS, OutputEntry, OutputType,
    Problem, RateChoice, SUPPORTED_RATES, VbanDest, VbanSampleFormat, driver_taken, max_of_type,
    shown_id, validate_entry, validate_list,
};
use sp_core::config::{
    DEFAULT_VBAN_STREAM_NAME, SETTING_AUDIO_NETWORK_RATE, SETTING_AUDIO_OUTPUTS, audio_network_rate,
};
use sqlx::SqlitePool;

// The stored read caps each type; their sum stays within the list's total,
// so a stored list never runs more than `MAX_OUTPUTS` outputs.
const _: () = assert!(MAX_VBAN_OUTPUTS + MAX_ASIO_OUTPUTS <= MAX_OUTPUTS);

type Fields = BTreeMap<String, Box<RawValue>>;

/// One JSON object's fields, read by name with errors that name them.
struct Reader<'a> {
    fields: Fields,
    at: &'a str,
    prefix: &'static str,
}

impl<'a> Reader<'a> {
    fn of(raw: &RawValue, at: &'a str, prefix: &'static str, what: &str) -> Result<Self, String> {
        let fields =
            serde_json::from_str(raw.get()).map_err(|_| format!("{what} is not a JSON object"))?;
        Ok(Self { fields, at, prefix })
    }

    fn opt<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, String> {
        self.fields
            .get(key)
            .map(|v| {
                serde_json::from_str(v.get())
                    .map_err(|_| format!("{}: {}{key} has the wrong type", self.at, self.prefix))
            })
            .transpose()
    }

    fn req<T: DeserializeOwned>(&self, key: &str) -> Result<T, String> {
        self.opt(key)?.ok_or_else(|| self.missing(key))
    }

    fn raw(&self, key: &str) -> Result<&RawValue, String> {
        self.fields
            .get(key)
            .map(|v| &**v)
            .ok_or_else(|| self.missing(key))
    }

    fn missing(&self, key: &str) -> String {
        format!("{}: {}{key} is missing", self.at, self.prefix)
    }
}

/// The value's items, or where it stopped being a JSON list (line and
/// column only: serde's text can quote the input).
fn list_items(raw: &str) -> Result<Vec<Box<RawValue>>, String> {
    serde_json::from_str(raw).map_err(|e| {
        format!(
            "audio_outputs is not a JSON list (line {}, column {})",
            e.line(),
            e.column()
        )
    })
}

fn vban_dest(raw: &RawValue, at: &str) -> Result<VbanDest, String> {
    let r = Reader::of(raw, at, "vban.", &format!("{at}: vban"))?;
    let format = match r.opt::<String>("format")? {
        None => VbanSampleFormat::default(),
        Some(text) => VbanSampleFormat::parse(&text)
            .ok_or_else(|| format!("{at}: vban.format must be {}", VbanSampleFormat::NAMES))?,
    };
    Ok(VbanDest {
        host: r.req("host")?,
        port: r.req("port")?,
        stream_name: r
            .opt("stream_name")?
            .unwrap_or_else(|| DEFAULT_VBAN_STREAM_NAME.to_string()),
        format,
    })
}

/// An ASIO entry's `asio` block (`[u32; 2]` refuses a third channel).
fn asio_dest(raw: &RawValue, at: &str) -> Result<AsioDest, String> {
    let r = Reader::of(raw, at, "asio.", &format!("{at}: asio"))?;
    Ok(AsioDest {
        driver: r.req("driver")?,
        channels: r.req("channels")?,
    })
}

/// Entry `index` read field by field (no validation of values yet). Each
/// type reads only its own block.
fn entry(index: usize, raw: &RawValue) -> Result<OutputEntry, String> {
    let first = format!("entry {}", index + 1);
    let head = Reader::of(raw, &first, "", &first)?;
    let id: String = head.req("id")?;
    let at = format!("{first} (id {})", shown_id(&id));
    // A new Reader, not `Reader { at: &at, ..head }`: functional update to a
    // different lifetime is the unstable type-changing struct update.
    let r = Reader {
        fields: head.fields,
        at: &at,
        prefix: "",
    };
    let kind_text: String = r.req("type")?;
    let kind = OutputType::parse(&kind_text)
        .ok_or_else(|| format!("{at}: type must be {}", OutputType::NAMES))?;
    let (vban, asio) = match kind {
        OutputType::Vban => (Some(vban_dest(r.raw("vban")?, &at)?), None),
        OutputType::Asio => (None, Some(asio_dest(r.raw("asio")?, &at)?)),
    };
    Ok(OutputEntry {
        id,
        name: r.req("name")?,
        kind,
        enabled: r.opt("enabled")?.unwrap_or(true),
        rate: r.opt::<RateChoice>("rate")?.unwrap_or_default(),
        delay_ms: r.opt("delay_ms")?.unwrap_or(0),
        vban,
        asio,
    })
}

/// A PATCH's list: every entry read, then the shared validation.
pub fn parse_list(raw: &str) -> Result<Vec<OutputEntry>, String> {
    let mut entries = Vec::new();
    for (i, item) in list_items(raw)?.iter().enumerate() {
        entries.push(entry(i, item)?);
    }
    validate_list(&entries).map_err(|e| e.to_string())?;
    Ok(entries)
}

/// The stored list as the outputs task runs it.
#[derive(Debug, Default, PartialEq)]
pub struct Stored {
    pub entries: Vec<OutputEntry>,
    pub problems: Vec<String>,
    /// The stored value is no list: the task changes nothing (Review Focus
    /// 3: what runs keeps running, never "all outputs off").
    pub not_a_list: bool,
}

impl Stored {
    /// Keep entry `index` unless it breaks a rule the kept ones set: its own
    /// validation, a duplicate id, the per-type cap (whose sum stays within
    /// `MAX_OUTPUTS`, the const assert above), an ASIO driver a kept entry
    /// already names.
    fn keep(&mut self, index: usize, e: OutputEntry) {
        let of_type = self.entries.iter().filter(|k| k.kind == e.kind).count();
        let max = max_of_type(e.kind);
        let entry_error = |field: &'static str, problem: Problem| {
            EntryError {
                index,
                id: e.id.clone(),
                field,
                problem,
            }
            .to_string()
        };
        let refusal = if let Err(err) = validate_entry(index, &e) {
            Some(err.to_string())
        } else if self.entries.iter().any(|k| k.id == e.id) {
            Some(entry_error("id", Problem::Duplicate))
        } else if of_type >= max {
            Some(format!(
                "entry {} (id {}): over the {max} {} entries",
                index + 1,
                shown_id(&e.id),
                e.kind.as_str()
            ))
        } else if driver_taken(&self.entries, &e) {
            Some(entry_error("asio.driver", Problem::DriverTaken))
        } else {
            None
        };
        match refusal {
            Some(problem) => self.problems.push(problem),
            None => self.entries.push(e),
        }
    }
}

/// The stored value, leniently: each unreadable or invalid entry is skipped
/// and named; a value that is no list is named and flagged (`not_a_list`).
pub fn parse_stored(raw: Option<&str>) -> Stored {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return Stored::default();
    };
    let items = match list_items(raw) {
        Ok(items) => items,
        Err(problem) => {
            return Stored {
                entries: Vec::new(),
                problems: vec![problem],
                not_a_list: true,
            };
        }
    };
    let mut out = Stored::default();
    for (i, item) in items.iter().enumerate() {
        match entry(i, item) {
            Ok(e) => out.keep(i, e),
            Err(problem) => out.problems.push(problem),
        }
    }
    out
}

/// The supported rates as an error lists them.
pub fn rates_text() -> String {
    SUPPORTED_RATES.map(|r| r.to_string()).join(", ")
}

/// The settings PATCH check of one key: the two output settings are refused
/// with the reason or normalized; every other key passes unchanged.
pub fn checked(key: &str, value: &str) -> Result<String, String> {
    match key {
        SETTING_AUDIO_OUTPUTS => {
            let entries = if value.trim().is_empty() {
                Vec::new()
            } else {
                parse_list(value)?
            };
            serde_json::to_string(&entries)
                .map_err(|_| "audio_outputs could not be written".to_string())
        }
        SETTING_AUDIO_NETWORK_RATE => match value.trim().parse::<u32>() {
            Ok(rate) if SUPPORTED_RATES.contains(&rate) => Ok(rate.to_string()),
            _ => Err(format!(
                "audio_network_rate must be one of {}",
                rates_text()
            )),
        },
        _ => Ok(value.to_string()),
    }
}

/// What the outputs task runs: the stored list (leniently), the network rate.
#[derive(Debug, Default, PartialEq)]
pub struct OutputsSettings {
    pub entries: Vec<OutputEntry>,
    pub network_rate: u32,
    pub problems: Vec<String>,
    /// [`Stored::not_a_list`]: apply nothing but the problem.
    pub not_a_list: bool,
}

/// Read the two settings (the outputs task, every 5 s).
pub async fn load(pool: &SqlitePool) -> Result<OutputsSettings, sqlx::Error> {
    use crate::db::models::get_setting;
    let stored = parse_stored(get_setting(pool, SETTING_AUDIO_OUTPUTS).await?.as_deref());
    let rate = get_setting(pool, SETTING_AUDIO_NETWORK_RATE).await?;
    Ok(OutputsSettings {
        entries: stored.entries,
        network_rate: audio_network_rate(rate.as_deref()),
        problems: stored.problems,
        not_a_list: stored.not_a_list,
    })
}

#[cfg(test)]
#[path = "audio_out_config_tests.rs"]
mod tests;
