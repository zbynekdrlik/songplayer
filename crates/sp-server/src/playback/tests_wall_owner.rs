//! #221, release 0.69.0 review 🟡 2: ONE wall owner. Only the owner the
//! playback authority published (`program_on_air::wall_owner`,
//! `OnAirPlaylists::may_write_wall`) writes the shared wall outputs: the
//! `ShowSubtitles` line, the Presenter, the song-end clear, the title timers
//! and a re-sync's title and lines. Since #221 B4 step 6 the authority
//! publishes one playlist at most (SP-program's), so these tests publish a
//! two-member set DIRECTLY: they pin the gate itself, which reads only the
//! published owner, and the re-syncs at an owner change. A child module of
//! `tests_scene_change.rs` (the 1000-line cap): it reuses that module's
//! engine rig (`test_engine`, `play`, `sent`, `resyncs`, `Window`).

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
use crate::playback::state::PlayState;
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

/// Whether a stage-display push body is the cleared display (the six empty
/// strings of `PresenterPayload::empty`).
fn is_cleared(body: &str) -> bool {
    body.contains(r#""currentText":"""#) && body.contains(r#""currentSong":"""#)
}

/// Review rounds 1-2: a new owner's ON — 9 owns ({7, 9} published, 9
/// owns). 7 writes nothing any more, so 9's ON re-syncs the whole wall at
/// once: its
/// title (the scene-on's ONE `Resync`), its line — one `HideSubtitles` when
/// 9 has none (it was played off program by hand, with no lyrics) — and the
/// Presenter — cleared when 9 has no line. Never 7's line frozen on
/// `#sp-subs` or on the stage display.
#[tokio::test]
async fn the_new_owner_s_on_re_syncs_the_wall_s_line() {
    let (mut engine, mut rx) = two_on_air(7).await;
    let stage = presenter(&mut engine).await;
    engine.dispatch_lyrics_if_changed(7, 60_000);
    assert_eq!(subtitle_lines(&sent(&mut rx)), ["gamma"], "7's line is up");
    assert!(pushes(&stage, 1).await[0].contains("gamma"));
    let nine = engine.pipelines.get_mut(&9).unwrap();
    nine.lyrics_state = None;
    nine.scene_active
        .store(false, std::sync::atomic::Ordering::Release);
    engine.on_air.publish(on_air(&[7, 9]), Some(9));

    engine.handle_scene_change(9, true).await;
    let cmds = sent(&mut rx);
    assert_eq!(
        count(&cmds, is_hide_subtitles),
        1,
        "7's line leaves: {cmds:?}"
    );
    assert!(subtitle_lines(&cmds).is_empty(), "{cmds:?}");
    let later = Some("Later - Artist".to_string());
    assert_eq!(
        resyncs(&cmds),
        std::slice::from_ref(&later),
        "9's title: {cmds:?}"
    );
    let bodies = pushes(&stage, 2).await;
    assert!(
        is_cleared(&bodies[1]),
        "7's line leaves the stage: {bodies:?}"
    );
    assert!(engine.pipelines[&9].last_presenter_text.is_none());

    // The same ON with a line (a re-kick of 9): 9's line goes up at once.
    engine.pipelines.get_mut(&9).unwrap().lyrics_state = Some(one_line("delta."));
    engine.handle_scene_change(9, true).await;
    let cmds = sent(&mut rx);
    assert_eq!(subtitle_lines(&cmds), ["delta"], "{cmds:?}");
    assert_eq!(count(&cmds, is_hide_subtitles), 0, "{cmds:?}");
    assert_eq!(resyncs(&cmds), [later], "one title Resync: {cmds:?}");
    let bodies = pushes(&stage, 3).await;
    assert!(bodies[2].contains("delta"), "{bodies:?}");
    assert!(engine.pipelines[&9].last_presenter_text.is_some());

    // An ON of a member that does not own the wall touches no line.
    engine.handle_scene_change(7, true).await;
    let cmds = sent(&mut rx);
    assert!(subtitle_lines(&cmds).is_empty(), "{cmds:?}");
    assert_eq!(count(&cmds, is_hide_subtitles), 0, "{cmds:?}");
}

/// Review round 2: a new owner that plays nothing (no playable song, so its
/// `SelectAndPlay` finds none and it stays `WaitingForScene`) takes the old
/// owner's title down (a `Resync` naming none), its line and its stage-
/// display line: the old owner's hide timer, song-end clear and Presenter
/// pushes no longer reach the wall.
#[tokio::test]
async fn a_new_owner_that_plays_nothing_takes_the_old_owner_s_title_down() {
    let (mut engine, mut rx) = two_on_air(7).await;
    let stage = presenter(&mut engine).await;
    engine.dispatch_lyrics_if_changed(7, 60_000);
    assert!(pushes(&stage, 1).await[0].contains("gamma"));
    sqlx::query("UPDATE videos SET normalized = 0 WHERE id = 44")
        .execute(&engine.pool)
        .await
        .unwrap();
    let nine = engine.pipelines.get_mut(&9).unwrap();
    nine.state = PlayState::WaitingForScene;
    nine.current_video_id = None;
    nine.lyrics_state = None;
    nine.scene_active
        .store(false, std::sync::atomic::Ordering::Release);
    engine.on_air.publish(on_air(&[7, 9]), Some(9));
    sent(&mut rx);

    engine.handle_scene_change(9, true).await;

    assert_eq!(engine.pipelines[&9].state, PlayState::WaitingForScene);
    let cmds = sent(&mut rx);
    assert_eq!(resyncs(&cmds), [None], "7's title leaves: {cmds:?}");
    assert_eq!(count(&cmds, is_hide_subtitles), 1, "{cmds:?}");
    assert!(subtitle_lines(&cmds).is_empty(), "{cmds:?}");
    let bodies = pushes(&stage, 2).await;
    assert!(is_cleared(&bodies[1]), "{bodies:?}");
}

/// Review round 3: an OFF re-syncs the stage display to the wall owner
/// still on program ({7, 9} with 9 owning → {7} with 7 owning, and only 9's
/// OFF handled), as its ON would: cleared here, 7 having no line — never
/// 9's line left there, nor re-pushed.
#[tokio::test]
async fn an_owner_change_by_an_off_re_syncs_the_stage_display() {
    let (mut engine, mut rx) = two_on_air(9).await;
    let stage = presenter(&mut engine).await;
    engine.dispatch_lyrics_if_changed(9, 60_000);
    assert!(pushes(&stage, 1).await[0].contains("delta"));
    engine.pipelines.get_mut(&7).unwrap().lyrics_state = None;
    engine.on_air.publish(on_air(&[7]), Some(7));
    sent(&mut rx);

    engine.handle_scene_change(9, false).await;

    let bodies = pushes(&stage, 2).await;
    assert!(
        is_cleared(&bodies[1]),
        "9's line leaves the stage: {bodies:?}"
    );
    assert!(engine.pipelines[&7].last_presenter_text.is_none());
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

// -- ROZHODNUTÉ 6002459249 (3): no wall owner = nobody writes ---------------

/// 7 plays its line ("gamma.") with nothing on air: "OBS manuál" is on
/// program, so the authority published no playlist and no wall owner.
async fn nothing_on_air() -> (PlaybackEngine, mpsc::Receiver<ResolumeCommand>) {
    let (mut engine, mut rx) = test_engine(&SONGS).await;
    play(&mut engine, 7, 42, Window::Due);
    engine.pipelines.get_mut(&7).unwrap().lyrics_state = Some(one_line("gamma."));
    engine.on_air.publish(on_air(&[]), None);
    sent(&mut rx);
    (engine, rx)
}

/// With "OBS manuál" on program no playlist owns the wall, and nobody
/// writes it. A playlist played off program by hand (a ▶,
/// `PlayEvent::Start`) feeds its own karaoke WS only: no Presenter push.
#[tokio::test]
async fn with_no_wall_owner_a_playlist_played_off_program_feeds_no_presenter() {
    let (mut engine, mut rx) = nothing_on_air().await;
    let _stage = presenter(&mut engine).await;
    engine.set_scene_active_for_test(7, false); // played off program by hand

    engine.dispatch_lyrics_if_changed(7, 60_000);

    assert!(subtitle_lines(&sent(&mut rx)).is_empty(), "no wall line");
    assert!(
        engine.pipelines[&7].last_presenter_text.is_none(),
        "7 pushed nothing to the Presenter"
    );
}

/// The same with its scene still on program: the authority took it off the
/// air (a cut to "OBS manuál") and its OFF is still queued. Its line, its
/// song-end clear and its title timers write nothing, and a recovery names
/// no title and clears the lines.
#[tokio::test]
async fn with_no_wall_owner_a_playlist_still_on_program_writes_nothing() {
    let (mut engine, mut rx) = nothing_on_air().await;

    engine.dispatch_lyrics_if_changed(7, 60_000);
    assert!(subtitle_lines(&sent(&mut rx)).is_empty(), "no wall line");
    engine.clear_lyrics_display(7);
    assert_eq!(count(&sent(&mut rx), is_hide_subtitles), 0, "no clear");
    let cmds = timer_fires(&mut engine, &mut rx, 7, true).await;
    assert_eq!(count(&cmds, is_show_title), 0, "no title shown: {cmds:?}");
    let cmds = timer_fires(&mut engine, &mut rx, 7, false).await;
    assert_eq!(count(&cmds, is_hide_title), 0, "no title hidden: {cmds:?}");

    engine.handle_resolume_recovery("127.0.0.1").await;
    let cmds = sent(&mut rx);
    assert_eq!(resyncs(&cmds), [None], "no title: {cmds:?}");
    assert!(subtitle_lines(&cmds).is_empty(), "{cmds:?}");
    assert_eq!(count(&cmds, is_hide_subtitles), 1, "{cmds:?}");
}

/// The owner's OFF with no owner left (a cut to "OBS manuál") blanks the
/// stage display like the wall's line and title: nobody writes it any
/// more, so the old owner's last line would stay there for good.
#[tokio::test]
async fn the_last_owner_s_off_blanks_the_stage_display() {
    let (mut engine, mut rx) = test_engine(&SONGS).await;
    play(&mut engine, 7, 42, Window::Due); // 7 on air, the owner
    engine.pipelines.get_mut(&7).unwrap().lyrics_state = Some(one_line("gamma."));
    let stage = presenter(&mut engine).await;
    engine.dispatch_lyrics_if_changed(7, 60_000);
    assert!(pushes(&stage, 1).await[0].contains("gamma"));
    engine.on_air.publish(on_air(&[]), None); // the cut to "OBS manuál"
    sent(&mut rx);

    engine.handle_scene_change(7, false).await;

    let cmds = sent(&mut rx);
    assert_eq!(count(&cmds, is_hide_title), 1, "{cmds:?}");
    assert_eq!(count(&cmds, is_hide_subtitles), 1, "{cmds:?}");
    let bodies = pushes(&stage, 2).await;
    assert!(
        is_cleared(&bodies[1]),
        "7's line leaves the stage: {bodies:?}"
    );
}

/// A press from 7 to 9: 9 owns the wall before its ON, which is queued
/// behind 7's OFF. That OFF leaves the stage display to 9's ON, with no
/// blank flash in between (no push within a bounded window: correct code
/// can never fail it).
#[tokio::test]
async fn an_off_with_the_next_owner_on_its_way_leaves_the_stage_display() {
    let (mut engine, mut rx) = test_engine(&SONGS).await;
    play(&mut engine, 7, 42, Window::Due);
    engine.pipelines.get_mut(&7).unwrap().lyrics_state = Some(one_line("gamma."));
    let stage = presenter(&mut engine).await;
    engine.dispatch_lyrics_if_changed(7, 60_000);
    assert!(pushes(&stage, 1).await[0].contains("gamma"));
    engine.put_on_air_for_test(9); // the press: 9 on air, its ON queued
    sent(&mut rx);

    engine.handle_scene_change(7, false).await;

    tokio::time::sleep(Duration::from_millis(300)).await;
    let got = stage.received_requests().await.unwrap_or_default();
    assert_eq!(got.len(), 1, "no stage push at 7's OFF");
}
