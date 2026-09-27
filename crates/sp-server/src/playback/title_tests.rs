//! Tests for `playback/title.rs`: the formatter, the song's title clock
//! (#217 addendum 3) and the title resync.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{TitleClock, format_title_text, resync_title};
use crate::obs::ObsCommand;
use crate::resolume::ResolumeCommand;

#[test]
fn formats_song_and_artist() {
    assert_eq!(format_title_text("Song", "Artist"), "Song - Artist");
}

#[test]
fn empty_artist_yields_song_only() {
    assert_eq!(format_title_text("Song", ""), "Song");
}

#[test]
fn empty_song_yields_artist_only() {
    assert_eq!(format_title_text("", "Artist"), "Artist");
}

#[test]
fn both_empty_yields_empty() {
    assert_eq!(format_title_text("", ""), "");
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// The show and hide timers' instants: 1.5 s after `Started`, and 3.5 s
/// before the end of a 180 s song.
#[test]
fn a_title_clock_shows_1_5_s_after_the_start_and_hides_3_5_s_before_the_end() {
    let base = Instant::now();
    let clock = TitleClock::new(42, base, 180_000);
    assert_eq!(clock.video_id, 42);
    assert_eq!(clock.show_at, base + ms(1_500));
    assert_eq!(clock.hide_at, Some(base + ms(176_500)));
}

/// A song of 5 s or less has no hide timer, nor has an unknown 0 duration:
/// its title stays to the end. 5001 ms is the shortest song with one, at
/// 1501 ms.
#[test]
fn a_song_too_short_for_a_hide_timer_has_no_hide_point() {
    let base = Instant::now();
    assert_eq!(TitleClock::new(42, base, 5_000).hide_at, None);
    assert_eq!(TitleClock::new(42, base, 0).hide_at, None);
    assert_eq!(
        TitleClock::new(42, base, 5_001).hide_at,
        Some(base + ms(1_501))
    );
}

/// The window is `[show_at, hide_at)`: both sides of each boundary.
#[test]
fn the_title_window_is_open_from_the_show_point_until_the_hide_point() {
    let base = Instant::now();
    let clock = TitleClock::new(42, base, 180_000);
    assert!(!clock.open_at(base + ms(1_499)), "before the show point");
    assert!(clock.open_at(base + ms(1_500)), "from the show point");
    assert!(clock.open_at(base + ms(176_499)), "until the hide point");
    assert!(!clock.open_at(base + ms(176_500)), "from the hide point on");

    let short = TitleClock::new(42, base, 5_000);
    assert!(
        short.open_at(base + ms(600_000)),
        "no hide point: open to the end"
    );
    assert!(!short.open_at(base + ms(1_499)));
}

async fn song_pool() -> sqlx::SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (7, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist) \
         VALUES (42, 7, 'yt42', 'Song', 'Artist')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// The OBS text source follows the resync like the wall: the title when one
/// is due, cleared when none is (as the song-end hide timer clears it).
#[tokio::test]
async fn a_resync_sets_or_clears_the_obs_title_text_too() {
    let pool = song_pool().await;
    let (obs_tx, mut obs_rx) = mpsc::channel(8);
    let (resolume_tx, mut resolume_rx) = mpsc::channel(8);

    for (due, expected) in [(Some(42), "Song - Artist"), (None, "")] {
        let sent = resync_title(&pool, Some(&obs_tx), &resolume_tx, due).await;

        match obs_rx.try_recv() {
            Ok(ObsCommand::SetTextSource { source_name, text }) => {
                assert_eq!(source_name, "#sp-title");
                assert_eq!(text, expected, "due {due:?}");
            }
            other => panic!("expected one SetTextSource, got {other:?}"),
        }
        match resolume_rx.try_recv() {
            Ok(ResolumeCommand::Resync { title }) => {
                assert_eq!(title, sent, "the Resync names the title returned");
                assert_eq!(title.as_deref().unwrap_or(""), expected);
            }
            other => panic!("expected one Resync, got {other:?}"),
        }
    }
}
