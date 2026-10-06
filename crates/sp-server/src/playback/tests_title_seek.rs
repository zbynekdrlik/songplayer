//! #217 (residual 5861473237): the song's title clock follows the song's
//! REAL position. A dashboard seek re-anchors it, and a resume whose seek
//! failed (the song plays from 0) counts from where `Started` says the song
//! really starts. A child of `tests_scene_change.rs`, reusing its rig.

use std::time::Duration;

use tokio::time::Instant;

use super::{SONG_MS, Window, play, resyncs, sent, test_engine, timers};
use crate::playback::pipeline::PipelineEvent;
use crate::resolume::ResolumeCommand;

/// Bounded wait for playlist 7's hide timer to run.
async fn hide_timer_ran(engine: &crate::playback::PlaybackEngine) {
    let timer = engine.pipelines[&7]
        .title_hide_abort
        .clone()
        .expect("the hide timer is armed for the song's new hide point");
    for _ in 0..400 {
        if timer.is_finished() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the hide timer never ran");
}

/// A seek to 3:00 of a 4:00 song: the title hides 3.5 s before the song's
/// end counted from the seek, 56.5 s later, not at the pre-seek schedule
/// (3:56.5 after the song's start). The show point stays: the title still
/// shows 1.5 s after the song started, and both timers are re-armed.
#[tokio::test]
async fn a_seek_moves_the_title_s_hide_point_to_the_song_s_new_position() {
    let (mut engine, _rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::OtherSong);
    engine
        .handle_pipeline_event(
            7,
            PipelineEvent::Started {
                duration_ms: 240_000,
                position_ms: 0,
            },
        )
        .await;
    let started = engine.pipelines[&7].title_clock.expect("the song's clock");

    let before = Instant::now();
    engine.seek(7, 180_000).await;
    let after = Instant::now();

    let clock = engine.pipelines[&7]
        .title_clock
        .expect("still the song's clock");
    assert_eq!(clock.video_id, 42);
    assert_eq!(
        clock.show_at, started.show_at,
        "the title still shows 1.5 s after the song started"
    );
    let left = Duration::from_millis(240_000 - 3_500 - 180_000);
    let hide = clock.hide_at.expect("a hide point");
    assert!(
        hide >= before + left && hide <= after + left,
        "the hide point is 56.5 s after the seek, not the pre-seek schedule \
         ({:?} after the song's start)",
        hide - started.show_at + Duration::from_millis(1_500)
    );
    assert_eq!(timers(&engine), (true, true), "both timers re-armed");
    engine.pipelines.get_mut(&7).unwrap().cancel_title_timers();
}

/// The hide TIMER follows the seek: a seek to 50 ms before the hide point of
/// a song whose title is up arms the hide timer for then, and it takes the
/// title down.
#[tokio::test]
async fn after_a_seek_the_hide_timer_fires_at_the_song_s_new_hide_point() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::Due);
    sent(&mut rx);

    engine.seek(7, SONG_MS - 3_500 - 50).await;

    hide_timer_ran(&engine).await;
    let cmds = sent(&mut rx);
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, ResolumeCommand::HideTitle)),
        "the title goes down at the song's new hide point, got {cmds:?}"
    );
}

/// A seek into the song's last 3.5 s: the title is no longer due, so the
/// wall's title is re-synced at once and goes down; no timer is left.
#[tokio::test]
async fn a_seek_into_the_last_3_5_s_takes_the_title_down_at_once() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::Due);
    sent(&mut rx);

    engine.seek(7, SONG_MS - 2_000).await;

    assert_eq!(
        resyncs(&sent(&mut rx)),
        [None::<String>],
        "the wall is re-synced, naming no title"
    );
    assert_eq!(timers(&engine), (false, false), "no timer is left");
}

/// A seek back from the song's last seconds reopens its title window: the
/// wall's title is re-synced to the song's title, and the hide timer is
/// armed for the new hide point.
#[tokio::test]
async fn a_seek_back_brings_the_title_back() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::AfterHide);
    sent(&mut rx);

    engine.seek(7, 60_000).await;

    assert_eq!(
        resyncs(&sent(&mut rx)),
        [Some("Song - Artist".to_string())],
        "the title is due again"
    );
    assert_eq!(timers(&engine), (false, true), "the hide timer only");
    engine.pipelines.get_mut(&7).unwrap().cancel_title_timers();
}

/// A paused song's clock and timers are left alone by a seek (its resume's
/// `Started` fixes a new clock), and nothing goes to the wall.
#[tokio::test]
async fn a_seek_of_a_paused_song_leaves_its_title_alone() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::Due);
    engine.pipelines.get_mut(&7).unwrap().paused_at = Some((42, 60_000));
    let clock = engine.pipelines[&7].title_clock;
    sent(&mut rx);

    engine.seek(7, 120_000).await;

    assert_eq!(engine.pipelines[&7].title_clock, clock, "the clock is kept");
    assert!(sent(&mut rx).is_empty(), "nothing goes to the wall");
    assert_eq!(timers(&engine), (false, false), "no timer armed");
}

/// A seek before the song's `Started` (no clock yet) arms nothing: the
/// pipeline applies it right after `Started` (the known corner, `seek.rs`).
#[tokio::test]
async fn a_seek_before_the_song_s_started_arms_nothing() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::NotStarted);
    sent(&mut rx);

    engine.seek(7, 120_000).await;

    assert_eq!(engine.pipelines[&7].title_clock, None, "still no clock");
    assert!(sent(&mut rx).is_empty(), "nothing goes to the wall");
    assert_eq!(timers(&engine), (false, false), "no timer armed");
}

/// A resume from 60 s whose seek FAILED plays the song from its start, and
/// its `Started` says so (position 0): the title hides 3.5 s before the end
/// of the whole song, not 60 s early.
#[tokio::test]
async fn a_resume_whose_seek_failed_hides_the_title_by_the_song_s_real_start() {
    let (mut engine, _rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::OtherSong);
    engine.handle_play_video(7, 42, Some(60_000)).await;

    engine
        .handle_pipeline_event(
            7,
            PipelineEvent::Started {
                duration_ms: SONG_MS,
                position_ms: 0,
            },
        )
        .await;

    let clock = engine.pipelines[&7].title_clock.expect("the song's clock");
    assert_eq!(
        clock.hide_at.expect("a hide point") - clock.show_at,
        Duration::from_millis(SONG_MS - 5_000),
        "shown 1.5 s after the start, hidden 3.5 s before the end of the whole song"
    );
    engine.pipelines.get_mut(&7).unwrap().cancel_title_timers();
}
