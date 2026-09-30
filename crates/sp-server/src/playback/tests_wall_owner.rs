//! #221, release 0.69.0 review 🟡 2: ONE wall owner. With two playlists on
//! air (a failed or late cg OBS mirror, a dashboard cut to "OBS manuál"),
//! only the owner the playback authority published
//! (`program_on_air::wall_owner`, `OnAirPlaylists::may_write_wall`) writes
//! the shared wall outputs: the `ShowSubtitles` line, the Presenter, the
//! song-end clear, the title timers and a re-sync's title and lines. A child
//! module of `tests_scene_change.rs` (the 1000-line cap): it reuses that
//! module's engine rig (`test_engine`, `play`, `sent`, `resyncs`, `Window`).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use sp_core::lyrics::{LyricsLine, LyricsTrack};
use tokio::sync::mpsc;
use tokio::time::Instant;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{Window, play, resyncs, sent, test_engine};
use crate::lyrics::renderer::LyricsState;
use crate::playback::PlaybackEngine;
use crate::playback::title::TitleClock;
use crate::presenter::PresenterClient;
use crate::resolume::ResolumeCommand;

/// Playlist 7 plays video 42 ("Song"), playlist 9 video 44 ("Later").
const SONGS: [(i64, i64, &str); 2] = [(7, 42, "Song"), (9, 44, "Later")];

/// A one-line track: `en` from 59 s to 62 s (a `Window::Due` song reports
/// 60 s). The #217 plan shows it whole; the wall strips its period.
fn one_line(en: &str) -> LyricsState {
    LyricsState::new(LyricsTrack {
        version: 22,
        source: "test".into(),
        language_source: "en".into(),
        language_translation: "sk".into(),
        lines: vec![LyricsLine {
            start_ms: 59_000,
            end_ms: 62_000,
            en: en.into(),
            sk: Some(format!("{en} sk")),
            words: None,
        }],
    })
}

/// 7 ("gamma.") and 9 ("delta.") both play their line on program; the
/// authority published both on air with `owner` owning the wall.
async fn two_on_air(owner: i64) -> (PlaybackEngine, mpsc::Receiver<ResolumeCommand>) {
    let (mut engine, mut rx) = test_engine(&SONGS).await;
    play(&mut engine, 7, 42, Window::Due);
    play(&mut engine, 9, 44, Window::Due);
    engine.pipelines.get_mut(&7).unwrap().lyrics_state = Some(one_line("gamma."));
    engine.pipelines.get_mut(&9).unwrap().lyrics_state = Some(one_line("delta."));
    engine.on_air.publish(on_air(&[7, 9]), Some(owner));
    sent(&mut rx);
    (engine, rx)
}

fn on_air(pids: &[i64]) -> BTreeSet<i64> {
    pids.iter().copied().collect()
}

/// The `en` of every `ShowSubtitles` in `cmds`.
fn subtitle_lines(cmds: &[ResolumeCommand]) -> Vec<String> {
    cmds.iter()
        .filter_map(|cmd| match cmd {
            ResolumeCommand::ShowSubtitles { en, .. } => Some(en.clone()),
            _ => None,
        })
        .collect()
}

fn count(cmds: &[ResolumeCommand], want: fn(&ResolumeCommand) -> bool) -> usize {
    cmds.iter().filter(|c| want(c)).count()
}

fn is_show_title(c: &ResolumeCommand) -> bool {
    matches!(c, ResolumeCommand::ShowTitle { .. })
}

fn is_hide_title(c: &ResolumeCommand) -> bool {
    matches!(c, ResolumeCommand::HideTitle)
}

fn is_hide_subtitles(c: &ResolumeCommand) -> bool {
    matches!(c, ResolumeCommand::HideSubtitles)
}

/// A Presenter stage display that accepts every push.
async fn presenter(engine: &mut PlaybackEngine) -> MockServer {
    let stage = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/stage"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&stage)
        .await;
    let url = format!("{}/api/stage", stage.uri());
    engine.presenter_client = Some(Arc::new(PresenterClient::new(url)));
    stage
}

/// The bodies of every push the stage display received, once `n` arrived
/// (bounded: 10 s).
async fn pushes(stage: &MockServer, n: usize) -> Vec<String> {
    for _ in 0..200 {
        let got = stage.received_requests().await.unwrap_or_default();
        if got.len() >= n {
            return got
                .iter()
                .map(|req| String::from_utf8_lossy(&req.body).into_owned())
                .collect();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the stage display did not receive {n} push(es) within 10 s");
}

#[tokio::test]
async fn only_the_wall_owner_dispatches_its_line_and_pushes_the_presenter() {
    let (mut engine, mut rx) = two_on_air(9).await;
    let stage = presenter(&mut engine).await;

    engine.dispatch_lyrics_if_changed(7, 60_000);
    assert!(
        subtitle_lines(&sent(&mut rx)).is_empty(),
        "7 is on air but 9 owns the wall"
    );
    assert!(
        engine.pipelines[&7].last_presenter_text.is_none(),
        "7 pushed nothing to the Presenter"
    );

    engine.dispatch_lyrics_if_changed(9, 60_000);
    assert_eq!(subtitle_lines(&sent(&mut rx)), ["delta"]);
    assert!(engine.pipelines[&9].last_presenter_text.is_some());
    let bodies = pushes(&stage, 1).await;
    assert!(bodies.iter().all(|b| !b.contains("gamma")), "{bodies:?}");
    assert!(bodies.iter().any(|b| b.contains("delta")), "{bodies:?}");
}

/// A playlist that comes to own the wall sends its current line at once,
/// even the one it sent before another playlist owned the wall.
#[tokio::test]
async fn a_playlist_that_comes_to_own_the_wall_sends_its_line_at_once() {
    let (mut engine, mut rx) = two_on_air(7).await;
    engine.dispatch_lyrics_if_changed(7, 60_000);
    assert_eq!(subtitle_lines(&sent(&mut rx)), ["gamma"]);

    engine.on_air.publish(on_air(&[7, 9]), Some(9));
    engine.dispatch_lyrics_if_changed(7, 60_000);
    engine.dispatch_lyrics_if_changed(9, 60_000);
    assert_eq!(subtitle_lines(&sent(&mut rx)), ["delta"]);

    engine.on_air.publish(on_air(&[7, 9]), Some(7));
    engine.dispatch_lyrics_if_changed(7, 60_000);
    assert_eq!(
        subtitle_lines(&sent(&mut rx)),
        ["gamma"],
        "7's line replaces 9's on the wall"
    );
}

#[tokio::test]
async fn only_the_wall_owner_s_song_end_clears_the_shared_outputs() {
    let (mut engine, mut rx) = two_on_air(9).await;
    let stage = presenter(&mut engine).await;

    engine.clear_lyrics_display(7);
    assert_eq!(
        count(&sent(&mut rx), is_hide_subtitles),
        0,
        "7's song end leaves 9's line"
    );
    engine.clear_lyrics_display(9);
    assert_eq!(count(&sent(&mut rx), is_hide_subtitles), 1);
    assert_eq!(
        pushes(&stage, 1).await.len(),
        1,
        "only 9's clear reached the Presenter"
    );
}

/// Arm `playlist_id`'s title timers with `clock` (its video 42 or 44), wait
/// until the one that fires 50 ms ahead has run (bounded), and return what
/// it sent to the wall.
async fn timer_fires(
    engine: &mut PlaybackEngine,
    rx: &mut mpsc::Receiver<ResolumeCommand>,
    playlist_id: i64,
    show: bool,
) -> Vec<ResolumeCommand> {
    let now = Instant::now();
    let soon = now + Duration::from_millis(50);
    let later = now + Duration::from_secs(3600);
    let video_id = if playlist_id == 7 { 42 } else { 44 };
    // The show timer: shown in 50 ms, hidden in an hour. The hide timer:
    // shown already (no show timer), hidden in 50 ms.
    let (show_at, hide_at) = if show { (soon, later) } else { (now, soon) };
    engine.pipelines.get_mut(&playlist_id).unwrap().title_clock = Some(TitleClock {
        video_id,
        show_at,
        hide_at: Some(hide_at),
    });
    engine.arm_title_timers(playlist_id, now);
    let pp = &engine.pipelines[&playlist_id];
    let armed = if show {
        pp.title_show_abort.clone()
    } else {
        pp.title_hide_abort.clone()
    };
    let timer = armed.expect("the timer 50 ms ahead is armed");
    for _ in 0..400 {
        if timer.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(timer.is_finished(), "the timer ran");
    engine
        .pipelines
        .get_mut(&playlist_id)
        .unwrap()
        .cancel_title_timers();
    sent(rx)
}

/// The other member's hide timer never takes the owner's title down, and
/// its show timer never puts its own title over the owner's.
#[tokio::test]
async fn only_the_wall_owner_s_title_timers_show_or_hide_the_title() {
    let (mut engine, mut rx) = two_on_air(9).await;

    let cmds = timer_fires(&mut engine, &mut rx, 7, false).await;
    assert_eq!(count(&cmds, is_hide_title), 0, "7's hide timer: {cmds:?}");
    let cmds = timer_fires(&mut engine, &mut rx, 7, true).await;
    assert_eq!(count(&cmds, is_show_title), 0, "7's show timer: {cmds:?}");

    let cmds = timer_fires(&mut engine, &mut rx, 9, false).await;
    assert_eq!(count(&cmds, is_hide_title), 1, "9's hide timer: {cmds:?}");
    let cmds = timer_fires(&mut engine, &mut rx, 9, true).await;
    assert_eq!(count(&cmds, is_show_title), 1, "9's show timer: {cmds:?}");
}

/// A Resolume recovery re-syncs the owner's title and re-sends only its
/// line, whatever the playlist ids (before: the highest id's title, both
/// lines).
#[tokio::test]
async fn a_recovery_resyncs_only_the_wall_owner_s_title_and_line() {
    let (engine, mut rx) = two_on_air(7).await;

    engine.handle_resolume_recovery("127.0.0.1").await;

    let cmds = sent(&mut rx);
    assert_eq!(resyncs(&cmds), [Some("Song - Artist".to_string())]);
    assert_eq!(subtitle_lines(&cmds), ["gamma"], "{cmds:?}");
}
