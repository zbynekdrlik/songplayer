//! #225: playlist queries for the dashboard WebSocket, and a playlist row's
//! playback mode (own module: `db/models.rs` is at the 1000-line cap).

use sp_core::playback::PlaybackMode;
use sqlx::SqlitePool;
use tracing::warn;

/// The playback mode a playlist's row holds (#225 unit 2: the row is the
/// mode's one persisted truth, and every pipeline starts in it). The API
/// writes only the three known names (`api/routes_mode.rs`), so an unknown
/// value came from outside it: it plays the default, with a WARN naming the
/// playlist.
pub fn row_mode(playlist_id: i64, name: &str, stored: &str) -> PlaybackMode {
    PlaybackMode::parse(stored).unwrap_or_else(|| {
        warn!(
            playlist_id,
            playlist_name = name,
            stored,
            "unknown playback mode in the playlist row — it plays the default"
        );
        PlaybackMode::default()
    })
}

/// Every playlist's id and row mode, ascending — active or not, since the
/// dashboard lists them all. The WS on-connect replay tells each one's
/// state; one the engine has not told about, in its row's mode. An unknown
/// stored value is the default here too, but quietly: this runs on every
/// dashboard connect, and the WARN belongs to the pipeline's start
/// ([`row_mode`]).
pub async fn all_playlist_modes(
    pool: &SqlitePool,
) -> Result<Vec<(i64, PlaybackMode)>, sqlx::Error> {
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, playback_mode FROM playlists ORDER BY id")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(id, stored)| (id, PlaybackMode::from_str_lossy(&stored)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_playlist_and_its_row_mode_ascending_inactive_ones_too() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        let seeded = all_playlist_modes(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, is_active, playback_mode) \
             VALUES (22521, 'b', 'u-b', 0, 'loop'), (22520, 'a', 'u-a', 1, 'single'), \
                    (22522, 'c', 'u-c', 1, 'shuffle')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut expected = seeded;
        expected.extend([
            (22_520, PlaybackMode::Single),
            (22_521, PlaybackMode::Loop),
            // An unknown stored value plays the default.
            (22_522, PlaybackMode::Continuous),
        ]);
        expected.sort_unstable_by_key(|(id, _)| *id);
        assert_eq!(all_playlist_modes(&pool).await.unwrap(), expected);
    }

    #[test]
    fn a_row_mode_is_its_stored_name_and_an_unknown_one_the_default() {
        assert_eq!(row_mode(1, "p", "single"), PlaybackMode::Single);
        assert_eq!(row_mode(1, "p", "LOOP"), PlaybackMode::Loop);
        assert_eq!(row_mode(1, "p", "continuous"), PlaybackMode::Continuous);
        assert_eq!(row_mode(1, "p", "shuffle"), PlaybackMode::Continuous);
        assert_eq!(row_mode(1, "p", ""), PlaybackMode::Continuous);
    }
}
