//! #261: Nastavenia's data-dir line and the status fields it reads.

use super::{DataSize, LINE_NOT_MEASURED, settings_line, size_sk};
use crate::audio_faults::LocalTime;

const MIB: u64 = 1_048_576;
const GIB: u64 = 1_073_741_824;

/// Whole MiB under a GiB, half up; tenths of a GiB from there, half up,
/// with a decimal comma.
#[test]
fn a_size_reads_as_windows_shows_it() {
    let table = [
        (0, "0 MB"),
        (MIB / 2 - 1, "0 MB"),
        (MIB / 2, "1 MB"),
        (850 * MIB, "850 MB"),
        (1_023 * MIB, "1023 MB"),
        (1_023 * MIB + MIB / 2 - 1, "1023 MB"),
        (1_023 * MIB + MIB / 2, "1,0 GB"),
        (GIB, "1,0 GB"),
        // 1.05 GiB is 1 127 428 915.2 bytes: the last byte under it stays 1,0
        (GIB + GIB / 20, "1,0 GB"),
        (GIB + GIB / 20 + 1, "1,1 GB"),
        (123 * GIB + 4 * GIB / 10, "123,4 GB"),
        (2_047 * GIB + 97 * GIB / 100, "2048,0 GB"),
        (u64::MAX, "17179869184,0 GB"),
    ];
    for (bytes, want) in table {
        assert_eq!(size_sk(bytes), want, "{bytes} bytes");
    }
}

fn at() -> LocalTime {
    LocalTime {
        day: 17,
        month: 10,
        hour: 14,
        minute: 5,
        second: 9,
    }
}

#[test]
fn a_measured_size_names_both_and_when() {
    let size = DataSize {
        data_dir_bytes: Some(123 * GIB + 4 * GIB / 10),
        cache_bytes: Some(120 * GIB),
        data_dir_measured_ms: Some(1),
    };
    assert_eq!(
        settings_line(&size, Some(at())),
        "Dáta na disku: 123,4 GB (cache 120,0 GB), merané 17.10. 14:05"
    );
    let no_cache = DataSize {
        cache_bytes: None,
        ..size
    };
    assert_eq!(
        settings_line(&no_cache, Some(at())),
        "Dáta na disku: 123,4 GB (cache ?), merané 17.10. 14:05"
    );
}

/// Nothing measured yet, or no time to name: the line says so.
#[test]
fn an_unmeasured_size_says_so() {
    assert_eq!(settings_line(&DataSize::default(), None), LINE_NOT_MEASURED);
    let no_time = DataSize {
        data_dir_bytes: Some(GIB),
        cache_bytes: Some(GIB),
        data_dir_measured_ms: Some(1),
    };
    assert_eq!(settings_line(&no_time, None), LINE_NOT_MEASURED);
    assert_eq!(
        settings_line(&DataSize::default(), Some(at())),
        LINE_NOT_MEASURED
    );
}

/// The three fields at the top level of the status, null until measured;
/// a status without them reads as not measured.
#[test]
fn the_status_fields_are_null_until_measured() {
    let json = serde_json::to_value(DataSize::default()).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "data_dir_bytes": null,
            "cache_bytes": null,
            "data_dir_measured_ms": null
        })
    );
    let older: DataSize = serde_json::from_str(r#"{"version":"0.1.0"}"#).unwrap();
    assert_eq!(older, DataSize::default());
    let read: DataSize =
        serde_json::from_str(r#"{"data_dir_bytes":5,"cache_bytes":4,"data_dir_measured_ms":3}"#)
            .unwrap();
    assert_eq!(
        read,
        DataSize {
            data_dir_bytes: Some(5),
            cache_bytes: Some(4),
            data_dir_measured_ms: Some(3)
        }
    );
}
