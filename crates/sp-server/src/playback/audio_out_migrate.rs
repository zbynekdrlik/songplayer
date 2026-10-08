//! #233: the first start of the output list moves #210's three VBAN keys into
//! it — one entry per target, `rate: 48000` (fixed, never "network"), INT24 and
//! the stream name exactly as #210 put it on the wire — so FOH hears what it
//! heard before. It acts only while NO list is stored and an old key exists,
//! and it KEEPS the old keys (main-session ruling 4, 7.10.2026): the new code
//! ignores them once the list exists, and a rollback to ≤ 0.73.0 (the last
//! release without the list) still finds them, so FOH keeps its sound. A
//! dashboard edit of a migrated entry (FOH's) is NOT copied back to the old
//! keys: a rollback sends what they held when the migration ran. A later
//! lane deletes them once the list has run a main release. The list is written only if still absent (`INSERT OR IGNORE`), so
//! a list a settings PATCH stored meanwhile is never replaced.

use sp_core::audio_outputs::{
    MAX_NAME_LEN, OutputEntry, PROGRAM_RATE, RateChoice, VbanDest, VbanSampleFormat,
    validate_entry, wire_stream_name,
};
use sp_core::config::{
    DEFAULT_VBAN_STREAM_NAME, SETTING_AUDIO_OUTPUTS, SETTING_VBAN_ENABLED,
    SETTING_VBAN_STREAM_NAME, SETTING_VBAN_TARGETS,
};
use sqlx::SqlitePool;

/// #210's `VBAN_MAX_TARGETS`: the targets its sender used.
const OLD_MAX_TARGETS: usize = 8;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Migrated {
    pub entries: Vec<OutputEntry>,
    /// The targets not migrated, each with the reason.
    pub skipped: Vec<String>,
}

/// `host:port` at the last colon, a port 1..=65535, read as #210's
/// `ToSocketAddrs` read it (the caller trims the whole target): no space next
/// to the colon is dropped, so a target #210 never resolved is not taken.
pub fn split_target(spec: &str) -> Option<(String, u16)> {
    let (host, port) = spec.rsplit_once(':')?;
    let port = port.parse::<u16>().ok().filter(|p| *p != 0)?;
    (!host.is_empty()).then(|| (host.to_string(), port))
}

/// #210's settings as entries (pure): the first 8 non-empty targets (the ones
/// #210 used), each `out-N` in order, named after its `host:port`.
pub fn entries_from_vban(enabled: bool, stream_name: &str, targets: &str) -> Migrated {
    let name = stream_name.trim();
    let wire = wire_stream_name(if name.is_empty() {
        DEFAULT_VBAN_STREAM_NAME
    } else {
        name
    });
    let mut out = Migrated::default();
    let specs = targets.split(',').map(str::trim).filter(|s| !s.is_empty());
    for (i, spec) in specs.enumerate() {
        if i >= OLD_MAX_TARGETS {
            out.skipped
                .push(format!("{spec}: over the 8 targets #210 used"));
            continue;
        }
        let Some((host, port)) = split_target(spec) else {
            out.skipped.push(format!("{spec}: not host:port"));
            continue;
        };
        let id = format!("out-{}", out.entries.len() + 1);
        let label: String = spec.chars().take(MAX_NAME_LEN).collect();
        let dest = VbanDest {
            host,
            port,
            stream_name: wire.clone(),
            format: VbanSampleFormat::Int24,
        };
        let mut entry = OutputEntry::vban(&id, &label, dest);
        entry.enabled = enabled;
        entry.rate = RateChoice::Fixed(PROGRAM_RATE);
        match validate_entry(out.entries.len(), &entry) {
            Ok(()) => out.entries.push(entry),
            Err(e) => out.skipped.push(format!("{spec}: {e}")),
        }
    }
    out
}

#[derive(Debug, PartialEq)]
pub enum MigrationOutcome {
    /// A list is stored, or no old key exists: nothing to do (every start
    /// after the first, and a box that never had VBAN).
    Nothing,
    /// The list was written from #210's keys (which stay).
    Migrated(Migrated),
}

/// Write `text` as the list unless one is stored (a settings PATCH may have
/// written one since it was read). Whether it was written.
pub async fn store_list_if_absent(pool: &SqlitePool, text: &str) -> Result<bool, sqlx::Error> {
    let done = sqlx::query("INSERT OR IGNORE INTO settings (key, value) VALUES (?, ?)")
        .bind(SETTING_AUDIO_OUTPUTS)
        .bind(text)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// The migration (see the module doc); the outputs task runs it at its start.
pub async fn migrate_vban_settings(pool: &SqlitePool) -> Result<MigrationOutcome, sqlx::Error> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM settings WHERE key IN (?, ?, ?, ?)")
            .bind(SETTING_VBAN_ENABLED)
            .bind(SETTING_VBAN_STREAM_NAME)
            .bind(SETTING_VBAN_TARGETS)
            .bind(SETTING_AUDIO_OUTPUTS)
            .fetch_all(pool)
            .await?;
    let get = |k: &str| {
        rows.iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    if get(SETTING_AUDIO_OUTPUTS).is_some() {
        return Ok(MigrationOutcome::Nothing);
    }
    let old = [
        SETTING_VBAN_ENABLED,
        SETTING_VBAN_STREAM_NAME,
        SETTING_VBAN_TARGETS,
    ]
    .into_iter()
    .any(|k| get(k).is_some());
    if !old {
        return Ok(MigrationOutcome::Nothing);
    }
    let m = entries_from_vban(
        get(SETTING_VBAN_ENABLED).is_some_and(|v| v.trim() == "true"),
        get(SETTING_VBAN_STREAM_NAME).unwrap_or(""),
        get(SETTING_VBAN_TARGETS).unwrap_or(""),
    );
    let text =
        serde_json::to_string(&m.entries).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    if store_list_if_absent(pool, &text).await? {
        Ok(MigrationOutcome::Migrated(m))
    } else {
        Ok(MigrationOutcome::Nothing)
    }
}

#[cfg(test)]
#[path = "audio_out_migrate_tests.rs"]
mod tests;
