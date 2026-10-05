//! The wall after a playlist goes off program with another one still on
//! program (#221 L4b review rounds 3-5, `scene_off.rs::wall_after_scene_off`).
//! A child module of `tests_scene_change.rs` (the 1000-line cap): it reuses
//! that module's engine rig (`test_engine`, `play`, `sent`, `resyncs`,
//! `Window`).

use tokio::sync::mpsc;

use super::{Window, play, resyncs, sent, test_engine};
use crate::obs::ObsCommand;
use crate::playback::title::OBS_TITLE_SOURCE;
use crate::resolume::ResolumeCommand;

/// A one-line track: "delta." from 59 s to 62 s (a `Window::Due` song
/// reports 60 s, `position_for`).
fn a_line_at_sixty_seconds() -> crate::lyrics::renderer::LyricsState {
    let line = sp_core::lyrics::LyricsLine {
        start_ms: 59_000,
        end_ms: 62_000,
        en: "delta.".into(),
        sk: Some("delta sk.".into()),
        words: None,
    };
    crate::lyrics::renderer::LyricsState::new(sp_core::lyrics::LyricsTrack {
        version: 22,
        source: "test".into(),
        language_source: "en".into(),
        language_translation: "sk".into(),
        lines: vec![line],
    })
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

fn is_hide_title(c: &ResolumeCommand) -> bool {
    matches!(c, ResolumeCommand::HideTitle)
}

fn is_hide_subtitles(c: &ResolumeCommand) -> bool {
    matches!(c, ResolumeCommand::HideSubtitles)
}

/// #221 L4b review rounds 3-4: another playlist can already be on program
/// (and in the authority's diffed set), its title and line up, when a
/// playlist goes off — after a quick cut away and back, the stale events of
/// the cuts in between are dropped (#221 B4 step 6: the outgoing OFF no
/// longer waits for cg OBS). The wall is then re-synced to it: its due title
/// (a Resync, no HideTitle) and its current line re-sent at once (no
/// HideSubtitles, which would blank it until its next position report).
#[tokio::test]
async fn going_off_program_resyncs_the_wall_to_a_playlist_still_on_program() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song"), (9, 44, "Later")]).await;
    play(&mut engine, 7, 42, Window::Due); // the outgoing playlist
    play(&mut engine, 9, 44, Window::Due); // on program already, its title due
    engine.pipelines.get_mut(&9).unwrap().lyrics_state = Some(a_line_at_sixty_seconds());
    engine.put_on_air_for_test(9);
    sent(&mut rx);

    engine.handle_scene_change(7, false).await;

    let cmds = sent(&mut rx);
    assert_eq!(
        count(&cmds, is_hide_title),
        0,
        "9's title stays up: {cmds:?}"
    );
    assert_eq!(resyncs(&cmds), [Some("Later - Artist".to_string())]);
    assert_eq!(
        count(&cmds, is_hide_subtitles),
        0,
        "9's line stays: {cmds:?}"
    );
    assert_eq!(
        subtitle_lines(&cmds),
        ["delta"],
        "9's line, re-sent at once"
    );
}

/// Review round 4: the playlist still on program has no title due (it just
/// started its song, the usual press): the outgoing title is FADED out
/// (HideTitle, as before L4b), never cut by a `Resync(None)`; with no line
/// on program, the outgoing line goes.
#[tokio::test]
async fn going_off_program_fades_the_title_when_the_one_on_program_has_none_due() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song"), (9, 44, "Later")]).await;
    play(&mut engine, 7, 42, Window::Due);
    play(&mut engine, 9, 44, Window::BeforeShow);
    engine.put_on_air_for_test(9);
    sent(&mut rx);

    engine.handle_scene_change(7, false).await;

    let cmds = sent(&mut rx);
    assert_eq!(count(&cmds, is_hide_title), 1, "{cmds:?}");
    assert!(resyncs(&cmds).is_empty(), "no Resync: {cmds:?}");
    assert_eq!(count(&cmds, is_hide_subtitles), 1, "{cmds:?}");
}

/// Review round 4: a playlist still flagged on program but no longer in the
/// authority's diffed set is leaving too (its OFF is queued): it is not
/// "on program" for the wall, so nothing re-syncs to its title.
#[tokio::test]
async fn going_off_program_ignores_a_playlist_whose_off_is_queued_too() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song"), (9, 44, "Later")]).await;
    play(&mut engine, 7, 42, Window::Due);
    play(&mut engine, 9, 44, Window::Due);
    engine.on_air.replace(Default::default()); // nothing on air: 9's OFF is queued
    sent(&mut rx);

    engine.handle_scene_change(7, false).await;

    let cmds = sent(&mut rx);
    assert_eq!(count(&cmds, is_hide_title), 1, "{cmds:?}");
    assert!(resyncs(&cmds).is_empty(), "no Resync to 9: {cmds:?}");
    assert_eq!(count(&cmds, is_hide_subtitles), 1, "{cmds:?}");
}

/// Review round 4: the line of a playlist whose OFF is queued too (out of
/// the authority's diffed set) is not re-sent: only the lines on program
/// are, and with none the outgoing line goes.
#[tokio::test]
async fn going_off_program_re_sends_only_the_lines_still_on_program() {
    let songs = [(7, 42, "Song"), (9, 44, "Later"), (11, 46, "Third")];
    let (mut engine, mut rx) = test_engine(&songs).await;
    play(&mut engine, 7, 42, Window::Due);
    play(&mut engine, 9, 44, Window::Due); // on program, its title due, no line
    play(&mut engine, 11, 46, Window::OtherSong); // leaving too, a line at 60 s
    engine.pipelines.get_mut(&11).unwrap().lyrics_state = Some(a_line_at_sixty_seconds());
    engine.put_on_air_for_test(9);
    sent(&mut rx);

    engine.handle_scene_change(7, false).await;

    let cmds = sent(&mut rx);
    assert_eq!(resyncs(&cmds), [Some("Later - Artist".to_string())]);
    assert!(
        subtitle_lines(&cmds).is_empty(),
        "11's line is not re-sent: {cmds:?}"
    );
    assert_eq!(count(&cmds, is_hide_subtitles), 1, "{cmds:?}");
}

/// Review round 5: with no title due on program, the outgoing title fades
/// AND cg OBS's `#sp-title` text is cleared (`title::push_hide`), as the
/// pre-L4b press's Play re-sync cleared it: the OBS text follows the wall.
#[tokio::test]
async fn going_off_program_clears_the_obs_title_text_when_none_is_due() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song"), (9, 44, "Later")]).await;
    let (obs_tx, mut obs_rx) = mpsc::channel(16);
    engine.obs_cmd_tx = Some(obs_tx);
    play(&mut engine, 7, 42, Window::Due);
    play(&mut engine, 9, 44, Window::BeforeShow);
    engine.put_on_air_for_test(9);
    sent(&mut rx);

    engine.handle_scene_change(7, false).await;

    let cmds = sent(&mut rx);
    assert_eq!(count(&cmds, is_hide_title), 1, "{cmds:?}");
    assert_eq!(
        obs_texts(&mut obs_rx),
        [(OBS_TITLE_SOURCE.to_string(), String::new())]
    );
}

/// Every `(source, text)` the engine set on cg OBS.
fn obs_texts(obs_rx: &mut mpsc::Receiver<ObsCommand>) -> Vec<(String, String)> {
    let mut texts = Vec::new();
    while let Ok(cmd) = obs_rx.try_recv() {
        if let ObsCommand::SetTextSource { source_name, text } = cmd {
            texts.push((source_name, text));
        }
    }
    texts
}

/// Review round 6: with no playlist left on program the outgoing title
/// fades and cg OBS's `#sp-title` text is cleared too (`title::push_hide`):
/// the OFF cancelled the song's hide timer, whose own hide clears it.
#[tokio::test]
async fn going_off_program_alone_clears_the_obs_title_text_too() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    let (obs_tx, mut obs_rx) = mpsc::channel(16);
    engine.obs_cmd_tx = Some(obs_tx);
    play(&mut engine, 7, 42, Window::Due);
    sent(&mut rx);

    engine.handle_scene_change(7, false).await;

    let cmds = sent(&mut rx);
    assert_eq!(count(&cmds, is_hide_title), 1, "{cmds:?}");
    assert_eq!(count(&cmds, is_hide_subtitles), 1, "{cmds:?}");
    assert_eq!(
        obs_texts(&mut obs_rx),
        [(OBS_TITLE_SOURCE.to_string(), String::new())]
    );
}

/// Review round 5: a failed read of the due title of the playlist still on
/// program sends no title command (a transient DB error never takes a title
/// down mid-song, `decide_wall_title`); its line is still re-sent.
#[tokio::test]
async fn going_off_program_sends_no_title_when_the_due_title_s_read_fails() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song"), (9, 44, "Later")]).await;
    play(&mut engine, 7, 42, Window::Due);
    play(&mut engine, 9, 44, Window::Due);
    engine.pipelines.get_mut(&9).unwrap().lyrics_state = Some(a_line_at_sixty_seconds());
    engine.put_on_air_for_test(9);
    sent(&mut rx);
    engine.pool.close().await;

    engine.handle_scene_change(7, false).await;

    let cmds = sent(&mut rx);
    assert_eq!(count(&cmds, is_hide_title), 0, "no fade: {cmds:?}");
    assert!(resyncs(&cmds).is_empty(), "no Resync: {cmds:?}");
    assert_eq!(subtitle_lines(&cmds), ["delta"], "9's line, re-sent");
}
