//! #261 (no infinite archive): how much the node's data dir holds, measured
//! by sp-server's daily cache sweep and carried on `GET /api/v1/status`
//! ([`DataSize`], all null until the first measure, 5 min after a start),
//! and Nastavenia's one line about it ([`settings_line`]). Not on the
//! HealthBar: it is read rarely (the owner's usage-frequency rule).

use serde::{Deserialize, Serialize};

use crate::audio_faults::{LocalTime, dm_hm_sk};

/// The data dir's size as `GET /api/v1/status` carries it (its three fields
/// sit at the top level of the status). A status without them (an older
/// server) reads as not measured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataSize {
    /// The whole data dir (`C:\ProgramData\SongPlayer`), the cache included.
    #[serde(default)]
    pub data_dir_bytes: Option<u64>,
    /// The media cache (`…\cache`).
    #[serde(default)]
    pub cache_bytes: Option<u64>,
    /// When it was measured, UTC ms.
    #[serde(default)]
    pub data_dir_measured_ms: Option<i64>,
}

/// While the status is read.
pub const LINE_LOADING: &str = "Dáta na disku: načítavam…";

/// The status could not be read.
pub const LINE_UNAVAILABLE: &str = "Dáta na disku: nedostupné";

/// Measured nothing yet (the measure comes with the daily cache sweep: 5 min
/// after a start, later while the #230 background hold holds it).
pub const LINE_NOT_MEASURED: &str =
    "Dáta na disku: ešte nezmerané (meria sa pri dennom čistení cache)";

/// 1 MiB: Windows' "MB".
const MIB: u64 = 1_048_576;

/// MiB in a GiB: from this many the size reads in GB.
const MIB_PER_GIB: u64 = 1_024;

/// A size the way Windows shows it, in Slovak: `850 MB` (whole MiB, half
/// up) under a GiB, else `12,3 GB` (tenths of a GiB, half up, a decimal
/// comma).
pub fn size_sk(bytes: u64) -> String {
    let mib = (u128::from(bytes) + u128::from(MIB / 2)) / u128::from(MIB);
    if mib < u128::from(MIB_PER_GIB) {
        return format!("{mib} MB");
    }
    let gib = u128::from(MIB) * u128::from(MIB_PER_GIB);
    let tenths = (u128::from(bytes) * 10 + gib / 2) / gib;
    format!("{},{} GB", tenths / 10, tenths % 10)
}

/// Nastavenia's line: `Dáta na disku: 123,4 GB (cache 120,0 GB), merané
/// 17.10. 14:05` (`measured` = the measure's instant in the browser's
/// local time), or [`LINE_NOT_MEASURED`].
pub fn settings_line(size: &DataSize, measured: Option<LocalTime>) -> String {
    match (size.data_dir_bytes, measured) {
        (Some(data), Some(at)) => format!(
            "Dáta na disku: {} (cache {}), merané {}",
            size_sk(data),
            size.cache_bytes.map_or_else(|| "?".to_string(), size_sk),
            dm_hm_sk(at)
        ),
        _ => LINE_NOT_MEASURED.to_string(),
    }
}

#[cfg(test)]
#[path = "data_size_tests.rs"]
mod tests;
