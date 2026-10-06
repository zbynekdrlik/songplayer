//! Tests for `playback/title.rs`: the formatter, the song's title clock
//! (#217 addendum 3), the title timers' pushes and the title resync.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{TitleClock, format_title_text, push_hide, push_title, send_resync, title_text};
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
    let clock = TitleClock::new(42, base, 180_000, 0);
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
    assert_eq!(TitleClock::new(42, base, 5_000, 0).hide_at, None);
    assert_eq!(TitleClock::new(42, base, 0, 0).hide_at, None);
    assert_eq!(
        TitleClock::new(42, base, 5_001, 0).hide_at,
        Some(base + ms(1_501))
    );
}

/// Review round 4 (🔵): a resume hides 3.5 s before the song's REAL end,
/// wherever it starts. A resume 5 s or less before the end has no window at
/// all: its hide point is not after its show point (`shows`). Before, it had
/// no hide point, so its title showed for the song's last seconds and
/// stayed past its end. A song of 5 s or less still keeps its title to the
/// end.
#[test]
fn a_resume_hides_3_5_s_before_the_real_end_or_shows_no_title() {
    let base = Instant::now();
    let resumed = TitleClock::new(42, base, 180_000, 60_000);
    assert_eq!(resumed.hide_at, Some(base + ms(116_500)));
    assert!(resumed.shows());

    let last = TitleClock::new(42, base, 180_000, 174_999);
    assert_eq!(last.hide_at, Some(base + ms(1_501)), "the last 1 ms window");
    assert!(last.shows());

    for start in [175_000, 177_000, 200_000] {
        let clock = TitleClock::new(42, base, 180_000, start);
        assert!(!clock.shows(), "{start}: no title window");
        assert!(!clock.open_at(clock.show_at), "{start}: never due");
    }
    assert_eq!(
        TitleClock::new(42, base, 180_000, 177_000).hide_at,
        Some(base),
        "the hide point is already past at the start"
    );

    let short = TitleClock::new(42, base, 5_000, 1_000);
    assert_eq!(short.hide_at, None, "a 5 s song keeps its title to its end");
    assert!(short.shows());
}

/// #217: a seek moves the hide point to the song's new position (3.5 s
/// before its end, counted from the seek) and keeps the show point (1.5 s
/// after the song started). A seek into the last 3.5 s puts the hide point
/// at the seek; a seek back to the start puts it a whole song away; a song
/// of 5 s or less still keeps its title to the end.
#[test]
fn a_seek_moves_the_hide_point_and_keeps_the_show_point() {
    let base = Instant::now();
    let clock = TitleClock::new(42, base, 240_000, 0);
    let now = base + ms(10_000);
    let seeked = clock.seeked(now, 240_000, 180_000);
    assert_eq!(seeked.video_id, 42);
    assert_eq!(seeked.show_at, base + ms(1_500), "the show point stays");
    assert_eq!(seeked.hide_at, Some(now + ms(56_500)));
    assert_eq!(
        clock.seeked(now, 240_000, 237_000).hide_at,
        Some(now),
        "past the hide point: the title is no longer due"
    );
    assert_eq!(
        clock.seeked(now, 240_000, 0).hide_at,
        Some(now + ms(236_500)),
        "back to the start"
    );
    let short = TitleClock::new(42, base, 5_000, 0).seeked(now, 5_000, 1_000);
    assert_eq!(short.hide_at, None, "a 5 s song keeps its title to its end");
}

/// `shows` is strict: a hide point AT the show point leaves no window.
#[test]
fn a_clock_shows_a_title_only_when_its_hide_point_is_after_its_show_point() {
    let base = Instant::now();
    let clock = |hide_ms: Option<u64>| TitleClock {
        video_id: 42,
        show_at: base + ms(1_500),
        hide_at: hide_ms.map(|hide_ms| base + ms(hide_ms)),
    };
    assert!(clock(None).shows(), "no hide point: to the end");
    assert!(clock(Some(1_501)).shows(), "1 ms after the show point");
    assert!(!clock(Some(1_500)).shows(), "at the show point: no window");
    assert!(!clock(Some(0)).shows(), "before the show point: no window");
}

/// The window is `[show_at, hide_at)`: both sides of each boundary.
#[test]
fn the_title_window_is_open_from_the_show_point_until_the_hide_point() {
    let base = Instant::now();
    let clock = TitleClock::new(42, base, 180_000, 0);
    assert!(!clock.open_at(base + ms(1_499)), "before the show point");
    assert!(clock.open_at(base + ms(1_500)), "from the show point");
    assert!(clock.open_at(base + ms(176_499)), "until the hide point");
    assert!(!clock.open_at(base + ms(176_500)), "from the hide point on");

    let short = TitleClock::new(42, base, 5_000, 0);
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
         VALUES (42, 7, 'yt42', 'Song', 'Artist'), (43, 7, 'yt43', '', '')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// The resync's title: the formatted song title; none for a video without a
/// row, or with neither a song nor an artist.
#[tokio::test]
async fn the_title_text_is_the_formatted_title_or_none() {
    let pool = song_pool().await;
    assert_eq!(
        title_text(&pool, 42).await.unwrap(),
        Some("Song - Artist".to_string())
    );
    assert_eq!(
        title_text(&pool, 43).await.unwrap(),
        None,
        "no song, no artist"
    );
    assert_eq!(title_text(&pool, 99).await.unwrap(), None, "no row");
}

/// cg OBS's command queue, full: one command in a channel of one. cg OBS
/// drains it only while it is connected. Keep the receiver alive, or a send
/// fails at once instead of waiting.
fn full_obs_queue() -> (mpsc::Sender<ObsCommand>, mpsc::Receiver<ObsCommand>) {
    let (obs_tx, obs_rx) = mpsc::channel(1);
    obs_tx
        .try_send(ObsCommand::SetTextSource {
            source_name: "#other".to_string(),
            text: String::new(),
        })
        .unwrap();
    (obs_tx, obs_rx)
}

/// The OBS title text waiting on `rx`, if any.
fn obs_title_text(rx: &mut mpsc::Receiver<ObsCommand>) -> Option<String> {
    match rx.try_recv() {
        Ok(ObsCommand::SetTextSource { source_name, text }) => {
            assert_eq!(source_name, "#sp-title");
            Some(text)
        }
        Ok(other) => panic!("expected the OBS title text, got {other:?}"),
        Err(_) => None,
    }
}

/// Review round 3 (🔵): the resync runs on the engine loop, and an awaited
/// OBS send parked the whole engine behind a full cg OBS queue (the Resync
/// itself already went first). The OBS text is the fallback display: it is
/// dropped when the queue is full, never awaited.
#[tokio::test]
async fn a_resync_never_waits_on_a_full_obs_queue() {
    let (obs_tx, _obs_rx) = full_obs_queue();
    let (resolume_tx, mut resolume_rx) = mpsc::channel(8);

    tokio::time::timeout(
        Duration::from_secs(5),
        send_resync(Some(&obs_tx), &resolume_tx, Some("Song - Artist".into())),
    )
    .await
    .expect("the resync does not wait for cg OBS");

    match resolume_rx.try_recv() {
        Ok(ResolumeCommand::Resync { title }) => {
            assert_eq!(title.as_deref(), Some("Song - Artist"));
        }
        other => panic!("expected the Resync, got {other:?}"),
    }
}

/// The Resolume Resync goes first, and the OBS text source follows it like
/// the wall: the title when one is due, cleared when none is (as the
/// song-end hide timer clears it).
#[tokio::test]
async fn a_resync_sets_or_clears_the_obs_title_text_too() {
    let (obs_tx, mut obs_rx) = mpsc::channel(8);
    let (resolume_tx, mut resolume_rx) = mpsc::channel(8);

    for (title, expected) in [(Some("Song - Artist"), "Song - Artist"), (None, "")] {
        send_resync(Some(&obs_tx), &resolume_tx, title.map(str::to_string)).await;

        match resolume_rx.try_recv() {
            Ok(ResolumeCommand::Resync { title: sent }) => {
                assert_eq!(sent.as_deref(), title, "the Resync names the title");
            }
            other => panic!("expected one Resync, got {other:?}"),
        }
        match obs_rx.try_recv() {
            Ok(ObsCommand::SetTextSource { source_name, text }) => {
                assert_eq!(source_name, "#sp-title");
                assert_eq!(text, expected, "title {title:?}");
            }
            other => panic!("expected one SetTextSource, got {other:?}"),
        }
    }
}

/// Review round 4 (🟡): the show timer awaited the OBS text before the
/// ShowTitle, and cg OBS's queue drains only while it is connected. With cg
/// OBS away, the title never reached the wall. Resolume goes first, and the
/// OBS text is dropped, never awaited.
#[tokio::test]
async fn a_full_obs_queue_never_holds_the_show_timer_s_title_back() {
    let pool = song_pool().await;
    let (obs_tx, _obs_rx) = full_obs_queue();
    let (resolume_tx, mut resolume_rx) = mpsc::channel(8);

    let pushed = tokio::time::timeout(
        Duration::from_secs(5),
        push_title(&pool, Some(&obs_tx), &resolume_tx, 42),
    )
    .await
    .expect("the show timer does not wait for cg OBS");

    assert!(pushed, "video 42 has a title");
    match resolume_rx.try_recv() {
        Ok(ResolumeCommand::ShowTitle { song, artist }) => {
            assert_eq!((song.as_str(), artist.as_str()), ("Song", "Artist"));
        }
        other => panic!("expected the ShowTitle, got {other:?}"),
    }
}

/// Review round 4 (🟡): the same for the hide timer. It awaited the OBS text
/// before the HideTitle, so with cg OBS away the title stayed into the next
/// song.
#[tokio::test]
async fn a_full_obs_queue_never_holds_the_hide_timer_s_hide_back() {
    let (obs_tx, _obs_rx) = full_obs_queue();
    let (resolume_tx, mut resolume_rx) = mpsc::channel(8);

    tokio::time::timeout(
        Duration::from_secs(5),
        push_hide(Some(&obs_tx), &resolume_tx),
    )
    .await
    .expect("the hide timer does not wait for cg OBS");

    assert!(
        matches!(resolume_rx.try_recv(), Ok(ResolumeCommand::HideTitle)),
        "the HideTitle went out"
    );
}

/// With room in cg OBS's queue, the show timer sets OBS's title text and the
/// hide timer clears it, each after Resolume's command. A video with no row
/// pushes nothing.
#[tokio::test]
async fn the_title_timers_set_and_clear_the_obs_title_text() {
    let pool = song_pool().await;
    let (obs_tx, mut obs_rx) = mpsc::channel(8);
    let (resolume_tx, mut resolume_rx) = mpsc::channel(8);

    assert!(push_title(&pool, Some(&obs_tx), &resolume_tx, 42).await);
    assert!(matches!(
        resolume_rx.try_recv(),
        Ok(ResolumeCommand::ShowTitle { .. })
    ));
    assert_eq!(
        obs_title_text(&mut obs_rx).as_deref(),
        Some("Song - Artist")
    );

    push_hide(Some(&obs_tx), &resolume_tx).await;
    assert!(matches!(
        resolume_rx.try_recv(),
        Ok(ResolumeCommand::HideTitle)
    ));
    assert_eq!(obs_title_text(&mut obs_rx).as_deref(), Some(""));

    assert!(
        !push_title(&pool, Some(&obs_tx), &resolume_tx, 99).await,
        "no row: no title"
    );
    assert!(resolume_rx.try_recv().is_err(), "nothing sent to Resolume");
    assert_eq!(obs_title_text(&mut obs_rx), None, "nothing sent to OBS");
}
