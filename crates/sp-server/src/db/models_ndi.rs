//! #196: per-output last-known NDI receiver count, persisted in the `settings`
//! table (one row per output, keyed `ndi_last_receivers_<playlist_id>`), so a
//! SongPlayer restart can tell which outputs HAD a receiver before it — the
//! baseline the post-restart receiver self-check
//! (`sp_core::health::no_receiver_after_restart`) compares against.
//!
//! In its own sibling module because `db/models.rs` is at the 1000-line cap.

use std::collections::HashMap;

use sqlx::{Row, SqlitePool};

/// Settings-key prefix for the per-output last-known receiver count.
const KEY_PREFIX: &str = "ndi_last_receivers_";

/// The `settings` key that stores `playlist_id`'s last-known receiver count.
/// Pure so it is unit-tested (mutation-scored).
pub fn key_for(playlist_id: i64) -> String {
    format!("{KEY_PREFIX}{playlist_id}")
}

/// Parse one `settings` row into `(playlist_id, count)`, or `None` when the key
/// is not one of ours or the value is not an integer. Pure so the prefix / parse
/// branches are unit-tested (mutation-scored).
pub fn parse_count_row(key: &str, value: &str) -> Option<(i64, i32)> {
    let id_str = key.strip_prefix(KEY_PREFIX)?;
    let id = id_str.parse::<i64>().ok()?;
    let count = value.parse::<i32>().ok()?;
    Some((id, count))
}

/// Persist `count` as `playlist_id`'s last-known receiver count (upsert). Used
/// on each health poll when the count changes.
///
/// mutants::skip — a settings-table write; the key it uses (`key_for`) is pure +
/// unit-tested.
#[cfg_attr(test, mutants::skip)]
pub async fn set_last_receiver_count(
    pool: &SqlitePool,
    playlist_id: i64,
    count: i32,
) -> Result<(), sqlx::Error> {
    crate::db::models::set_setting(pool, &key_for(playlist_id), &count.to_string()).await
}

/// Read EVERY persisted pre-restart receiver count (`playlist_id -> count`) from
/// the `settings` table — the self-check baseline, read once at startup. A DB
/// error or a malformed row is skipped (an absent baseline just means "no
/// output is known to have had a receiver", the safe direction).
///
/// mutants::skip — a settings-table read; the per-row decoding (`parse_count_row`)
/// is pure + unit-tested.
#[cfg_attr(test, mutants::skip)]
pub async fn all_last_receiver_counts(pool: &SqlitePool) -> HashMap<i64, i32> {
    let rows = match sqlx::query("SELECT key, value FROM settings WHERE key LIKE ?")
        .bind(format!("{KEY_PREFIX}%"))
        .fetch_all(pool)
        .await
    {
        Ok(r) => r,
        Err(_) => return HashMap::new(),
    };
    let mut out = HashMap::new();
    for row in rows {
        let key: String = row.get("key");
        let value: String = row.get("value");
        if let Some((id, count)) = parse_count_row(&key, &value) {
            out.insert(id, count);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_for_formats_prefix_and_id() {
        assert_eq!(key_for(4), "ndi_last_receivers_4");
        assert_eq!(key_for(0), "ndi_last_receivers_0");
    }

    #[test]
    fn parse_row_round_trips_a_written_key() {
        // The key `key_for` writes must parse back to the same id.
        let key = key_for(7);
        assert_eq!(parse_count_row(&key, "3"), Some((7, 3)));
    }

    #[test]
    fn parse_row_rejects_a_foreign_key() {
        assert_eq!(parse_count_row("genlock_pacing", "2"), None);
        assert_eq!(parse_count_row("ndi_other_5", "2"), None);
    }

    #[test]
    fn parse_row_rejects_non_integer_id_or_value() {
        assert_eq!(parse_count_row("ndi_last_receivers_abc", "2"), None);
        assert_eq!(parse_count_row("ndi_last_receivers_4", "two"), None);
    }

    #[test]
    fn parse_row_accepts_zero_and_negative_counts() {
        // 0 = "had no receiver", -1 = "never polled" — both valid baselines.
        assert_eq!(parse_count_row("ndi_last_receivers_4", "0"), Some((4, 0)));
        assert_eq!(parse_count_row("ndi_last_receivers_4", "-1"), Some((4, -1)));
    }
}
