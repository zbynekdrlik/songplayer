//! #217 (residual 5861473237): the song's title clock follows the song's
//! REAL position. A dashboard seek goes to the pipeline, and the clock moves
//! when the pipeline reports where the song really plays from
//! (`PipelineEvent::Seeked`): the asked position, or where the decoder was
//! when it refused the seek. A resume whose seek failed (the song plays from
//! its start) counts from where `Started` says the song really starts. A
//! child of `tests_scene_change.rs`, reusing its rig.

use std::time::Duration;

use tokio::time::Instant;

use super::{SONG_MS, Window, play, resyncs, sent, test_engine, timers};
use crate::playback::PlaybackEngine;
use crate::playback::pipeline::{PipelineCommand, PipelineEvent, PlaybackPipeline};
use crate::resolume::ResolumeCommand;

/// Bounded wait for playlist 7's hide timer to run.
async fn hide_timer_ran(engine: &PlaybackEngine) {
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

/// Playlist 7's pipeline reports a `duration_ms` song started from 0.
async fn started(engine: &mut PlaybackEngine, duration_ms: u64) {
    engine
        .handle_pipeline_event(
            7,
            PipelineEvent::Started {
                duration_ms,
                position_ms: 0,
            },
        )
        .await;
}

/// Playlist 7's pipeline reports where its song plays from after a seek;
/// returns the instants around the report.
async fn seeked(engine: &mut PlaybackEngine, position_ms: u64) -> (Instant, Instant) {
    let before = Instant::now();
    engine
        .handle_pipeline_event(7, PipelineEvent::Seeked { position_ms })
        .await;
    (before, Instant::now())
}

/// The hide point of playlist 7's clock lies `left` after the report.
fn assert_hides_after(engine: &PlaybackEngine, (before, after): (Instant, Instant), left: u64) {
    let left = Duration::from_millis(left);
    let hide = engine.pipelines[&7]
        .title_clock
        .expect("the song's clock")
        .hide_at
        .expect("a hide point");
    assert!(
        hide >= before + left && hide <= after + left,
        "the hide point is {left:?} after the report"
    );
}

/// The dashboard's seek goes to the pipeline as a `Seek`, and nothing moves
/// yet: the clock waits for the pipeline's report of where the song really
/// plays from (the decoder may refuse the seek).
#[tokio::test]
async fn a_seek_goes_to_the_pipeline_and_the_clock_waits_for_its_report() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::Due);
    let (pipeline, cmds) = PlaybackPipeline::detached_for_test("SP-7");
    engine.pipelines.get_mut(&7).unwrap().pipeline = pipeline;
    let clock = engine.pipelines[&7].title_clock;
    sent(&mut rx);

    engine.seek(7, 120_000);

    match cmds.try_recv() {
        Ok(PipelineCommand::Seek { position_ms }) => assert_eq!(position_ms, 120_000),
        other => panic!("the pipeline gets the Seek, got {other:?}"),
    }
    assert_eq!(engine.pipelines[&7].title_clock, clock, "the clock is kept");
    assert!(sent(&mut rx).is_empty(), "nothing goes to the wall");
    assert_eq!(timers(&engine), (false, false), "no timer armed");
}

/// A seek to 3:00 of a 4:00 song, reported by the pipeline: the title hides
/// 3.5 s before the song's end counted from the report, 56.5 s later, not at
/// the pre-seek schedule (3:56.5 after the song's start). The show point
/// stays: the title still shows 1.5 s after the song started, and both timers
/// are re-armed.
#[tokio::test]
async fn a_seek_moves_the_title_s_hide_point_to_the_song_s_new_position() {
    let (mut engine, _rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::OtherSong);
    started(&mut engine, 240_000).await;
    let at_start = engine.pipelines[&7].title_clock.expect("the song's clock");

    engine.seek(7, 180_000);
    let report = seeked(&mut engine, 180_000).await;

    let clock = engine.pipelines[&7]
        .title_clock
        .expect("still the song's clock");
    assert_eq!(clock.video_id, 42);
    assert_eq!(
        clock.show_at, at_start.show_at,
        "the title still shows 1.5 s after the song started"
    );
    assert_hides_after(&engine, report, 240_000 - 3_500 - 180_000);
    assert_eq!(timers(&engine), (true, true), "both timers re-armed");
    engine.pipelines.get_mut(&7).unwrap().cancel_title_timers();
}

/// A seek the decoder REFUSES: the song plays on from where it was (1:00),
/// and the pipeline reports that, so the title hides 3.5 s before the end
/// counted from 1:00 — not from the asked 3:00, 2 minutes early.
#[tokio::test]
async fn a_refused_seek_hides_the_title_by_where_the_song_really_plays() {
    let (mut engine, _rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::OtherSong);
    started(&mut engine, 240_000).await;

    engine.seek(7, 180_000);
    let report = seeked(&mut engine, 60_000).await;

    assert_hides_after(&engine, report, 240_000 - 3_500 - 60_000);
    assert_eq!(timers(&engine), (true, true), "both timers re-armed");
    engine.pipelines.get_mut(&7).unwrap().cancel_title_timers();
}

/// A seek sent before the song's `Started` (no clock yet) is applied by the
/// pipeline right after `Started`, and its report then moves the new clock:
/// the title hides 3.5 s before the end counted from the reported position.
#[tokio::test]
async fn a_seek_sent_before_started_moves_the_clock_by_its_report() {
    let (mut engine, _rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::NotStarted);

    engine.seek(7, 120_000);
    assert_eq!(engine.pipelines[&7].title_clock, None, "no clock yet");
    started(&mut engine, SONG_MS).await;
    let at_start = engine.pipelines[&7].title_clock.expect("the song's clock");
    let report = seeked(&mut engine, 120_000).await;

    assert_eq!(
        engine.pipelines[&7].title_clock.map(|c| c.show_at),
        Some(at_start.show_at),
        "the show point stays"
    );
    assert_hides_after(&engine, report, SONG_MS - 3_500 - 120_000);
    engine.pipelines.get_mut(&7).unwrap().cancel_title_timers();
}

/// The hide TIMER follows the reported seek: a seek to 50 ms before the hide
/// point of a song whose title is up arms the hide timer for then, and it
/// takes the title down.
#[tokio::test]
async fn after_a_seek_the_hide_timer_fires_at_the_song_s_new_hide_point() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::Due);
    sent(&mut rx);

    engine.seek(7, SONG_MS - 3_500 - 50);
    seeked(&mut engine, SONG_MS - 3_500 - 50).await;

    hide_timer_ran(&engine).await;
    let cmds = sent(&mut rx);
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, ResolumeCommand::HideTitle)),
        "the title goes down at the song's new hide point, got {cmds:?}"
    );
}

/// A seek into the song's last 3.5 s: once reported, the title is no longer
/// due, so the wall's title is re-synced at once and goes down; no timer is
/// left.
#[tokio::test]
async fn a_seek_into_the_last_3_5_s_takes_the_title_down_at_once() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::Due);
    sent(&mut rx);

    engine.seek(7, SONG_MS - 2_000);
    seeked(&mut engine, SONG_MS - 2_000).await;

    assert_eq!(
        resyncs(&sent(&mut rx)),
        [None::<String>],
        "the wall is re-synced, naming no title"
    );
    assert_eq!(timers(&engine), (false, false), "no timer is left");
}

/// A seek back from the song's last seconds reopens its title window: once
/// reported, the wall's title is re-synced to the song's title, and the hide
/// timer is armed for the new hide point.
#[tokio::test]
async fn a_seek_back_brings_the_title_back() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::AfterHide);
    sent(&mut rx);

    engine.seek(7, 60_000);
    seeked(&mut engine, 60_000).await;

    assert_eq!(
        resyncs(&sent(&mut rx)),
        [Some("Song - Artist".to_string())],
        "the title is due again"
    );
    assert_eq!(timers(&engine), (false, true), "the hide timer only");
    engine.pipelines.get_mut(&7).unwrap().cancel_title_timers();
}

/// A paused song's clock and timers are left alone by a seek and its report
/// (its resume's `Started` fixes a new clock), and nothing goes to the wall.
#[tokio::test]
async fn a_seek_of_a_paused_song_leaves_its_title_alone() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::Due);
    engine.pipelines.get_mut(&7).unwrap().paused_at = Some((42, 60_000));
    let clock = engine.pipelines[&7].title_clock;
    sent(&mut rx);

    engine.seek(7, 120_000);
    seeked(&mut engine, 120_000).await;

    assert_eq!(engine.pipelines[&7].title_clock, clock, "the clock is kept");
    assert!(sent(&mut rx).is_empty(), "nothing goes to the wall");
    assert_eq!(timers(&engine), (false, false), "no timer armed");
}

/// The report of an earlier song's seek (a newer Play is under way, its
/// `Started` not come yet: the Play cleared the clock) moves nothing, arms
/// nothing and sends nothing to the wall.
#[tokio::test]
async fn the_seek_report_of_an_earlier_song_is_ignored() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::OtherSong);
    started(&mut engine, SONG_MS).await;
    engine.handle_play_video(7, 42, None).await;
    sent(&mut rx);

    seeked(&mut engine, 60_000).await;

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

    started(&mut engine, SONG_MS).await;

    let clock = engine.pipelines[&7].title_clock.expect("the song's clock");
    assert_eq!(
        clock.hide_at.expect("a hide point") - clock.show_at,
        Duration::from_millis(SONG_MS - 5_000),
        "shown 1.5 s after the start, hidden 3.5 s before the end of the whole song"
    );
    engine.pipelines.get_mut(&7).unwrap().cancel_title_timers();
}
