//! #242: a playlist's own sound in its row (V33): `audio_gain_db` and
//! `audio_eq` (the `sp_core::audio_fx::EqBand` list as JSON). The API writes
//! only validated values (`api/playlist_audio.rs`); a stored value that does
//! not read or validate came from outside it and plays untouched, with a
//! WARN naming the playlist.

use sp_core::audio_fx::{EqBand, PlaylistFx, validate};
use sqlx::SqlitePool;
use tracing::warn;

/// The sound a row's two columns describe; an unreadable or out-of-limits
/// value is the default (untouched audio), with a WARN.
pub fn row_fx(playlist_id: i64, gain_db: f64, eq_json: &str) -> PlaylistFx {
    let eq = match serde_json::from_str::<Vec<EqBand>>(eq_json) {
        Ok(eq) => eq,
        Err(e) => {
            warn!(playlist_id, %e, "playlist audio_eq does not read — the playlist plays untouched");
            return PlaylistFx::default();
        }
    };
    let fx = PlaylistFx { gain_db, eq };
    match validate(&fx) {
        Ok(()) => fx,
        Err(e) => {
            warn!(playlist_id, %e, "playlist audio is out of limits — the playlist plays untouched");
            PlaylistFx::default()
        }
    }
}

/// One playlist's sound; `None` when no such playlist.
pub async fn get_playlist_fx(
    pool: &SqlitePool,
    playlist_id: i64,
) -> Result<Option<PlaylistFx>, sqlx::Error> {
    let row: Option<(f64, String)> =
        sqlx::query_as("SELECT audio_gain_db, audio_eq FROM playlists WHERE id = ?")
            .bind(playlist_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(gain, eq)| row_fx(playlist_id, gain, &eq)))
}

/// Every playlist's sound, by id ascending.
pub async fn all_playlist_fx(pool: &SqlitePool) -> Result<Vec<(i64, PlaylistFx)>, sqlx::Error> {
    let rows: Vec<(i64, f64, String)> =
        sqlx::query_as("SELECT id, audio_gain_db, audio_eq FROM playlists ORDER BY id")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(id, gain, eq)| (id, row_fx(id, gain, &eq)))
        .collect())
}

/// Write a playlist's sound (the caller validated it); returns whether the
/// playlist exists.
pub async fn set_playlist_fx(
    pool: &SqlitePool,
    playlist_id: i64,
    fx: &PlaylistFx,
) -> Result<bool, sqlx::Error> {
    let eq = serde_json::to_string(&fx.eq).unwrap_or_else(|_| "[]".to_string());
    let done = sqlx::query(
        "UPDATE playlists SET audio_gain_db = ?, audio_eq = ?, updated_at = datetime('now') \
         WHERE id = ?",
    )
    .bind(fx.gain_db)
    .bind(eq)
    .bind(playlist_id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

#[cfg(test)]
#[path = "models_playlist_fx_tests.rs"]
mod tests;
