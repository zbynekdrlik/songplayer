//! #217 addendum 3 — the driver-owned title state: the pure decision table,
//! the state's clips, and a `Resync` superseding the queued title commands.

use tokio::sync::mpsc;

use super::*;
use crate::resolume::ResolumeCommand;
use crate::resolume::driver::ClipInfo;

fn shown(text: &str) -> TitleState {
    TitleState::Shown(text.to_string())
}

fn clip(clip_id: i64, text_param_id: i64) -> ClipInfo {
    ClipInfo {
        clip_id,
        text_param_id,
    }
}

// -- plan: act only on a difference -------------------------------------------

#[test]
fn a_show_fades_in_unless_that_title_is_already_up() {
    assert_eq!(
        shown("A - B").plan(TitleIntent::Show("A - B")),
        TitleAction::Nothing,
        "no second fade for the title that is up"
    );
    for state in [
        TitleState::Unknown,
        TitleState::Hidden,
        shown("Other - Song"),
        TitleState::FadingIn("A - B".into()),
        TitleState::FadingOut,
    ] {
        assert_eq!(
            state.plan(TitleIntent::Show("A - B")),
            TitleAction::FadeIn("A - B"),
            "{state:?}: the title is not fully up, so it fades in"
        );
    }
    assert_eq!(
        TitleState::Hidden.plan(TitleIntent::Show("")),
        TitleAction::Nothing,
        "an empty title shows nothing"
    );
}

#[test]
fn a_hide_fades_only_a_title_known_to_be_up() {
    assert_eq!(
        TitleState::Hidden.plan(TitleIntent::Hide),
        TitleAction::Nothing,
        "nothing to hide"
    );
    assert_eq!(
        shown("A - B").plan(TitleIntent::Hide),
        TitleAction::FadeOut,
        "a title known to be up fades out"
    );
    for state in [
        TitleState::Unknown,
        TitleState::FadingIn("A - B".into()),
        TitleState::FadingOut,
    ] {
        assert_eq!(
            state.plan(TitleIntent::Hide),
            TitleAction::HideNow,
            "{state:?}: a fade from full opacity could flash the clip, so it hides at once"
        );
    }
}

#[test]
fn a_resync_naming_a_title_shows_it_only_when_it_is_not_up() {
    assert_eq!(
        shown("A - B").plan(TitleIntent::Resync(Some("A - B"))),
        TitleAction::Nothing,
        "the title the engine wants is up: nothing"
    );
    for state in [
        TitleState::Unknown,
        TitleState::Hidden,
        shown("Other - Song"),
        TitleState::FadingIn("A - B".into()),
        TitleState::FadingOut,
    ] {
        assert_eq!(
            state.plan(TitleIntent::Resync(Some("A - B"))),
            TitleAction::FadeIn("A - B"),
            "{state:?}: the wanted title fades in"
        );
    }
}

#[test]
fn a_resync_naming_no_title_hides_at_once_unless_hidden() {
    assert_eq!(
        TitleState::Hidden.plan(TitleIntent::Resync(None)),
        TitleAction::Nothing,
        "already hidden"
    );
    for state in [
        TitleState::Unknown,
        shown("A - B"),
        TitleState::FadingIn("A - B".into()),
        TitleState::FadingOut,
    ] {
        assert_eq!(
            state.plan(TitleIntent::Resync(None)),
            TitleAction::HideNow,
            "{state:?}: a title that must not be up is hidden at once, never faded"
        );
    }
    assert_eq!(
        shown("A - B").plan(TitleIntent::Resync(Some(""))),
        TitleAction::HideNow,
        "an empty title is no title"
    );
    assert_eq!(
        TitleState::Hidden.plan(TitleIntent::Resync(Some(""))),
        TitleAction::Nothing
    );
}

// -- WallTitle: the state and the clips it was reached on ---------------------

#[test]
fn a_finished_action_reaches_its_state_and_a_failed_one_stays_between() {
    let mut title = WallTitle::new();
    assert_eq!(title.state(), &TitleState::Unknown, "startup: not known");

    title.begin(TitleAction::FadeIn("A - B"), vec![clip(100, 900)]);
    assert_eq!(title.state(), &TitleState::FadingIn("A - B".into()));
    title.finish(true);
    assert_eq!(title.state(), &shown("A - B"), "the fade-in finished");

    title.begin(TitleAction::FadeOut, vec![clip(100, 900)]);
    title.finish(false);
    assert_eq!(
        title.state(),
        &TitleState::FadingOut,
        "a failed hide leaves the title partly up"
    );
    title.begin(TitleAction::HideNow, vec![clip(100, 900)]);
    title.finish(true);
    assert_eq!(
        title.state(),
        &TitleState::Hidden,
        "the instant hide finished"
    );

    title.begin(TitleAction::FadeIn("C - D"), vec![clip(100, 900)]);
    title.finish(false);
    assert_eq!(
        title.state(),
        &TitleState::FadingIn("C - D".into()),
        "a failed show leaves the title partly up"
    );
}

#[test]
fn new_title_clip_ids_make_the_state_unknown_and_the_same_ids_keep_it() {
    let mut title = WallTitle::at(shown("A - B"), vec![clip(100, 900)]);

    title.note_clips(Some(&vec![clip(100, 900)]));
    assert_eq!(
        title.state(),
        &shown("A - B"),
        "the same ids after an outage: the clip is as the driver left it"
    );
    title.note_clips(None);
    title.note_clips(Some(&Vec::new()));
    assert_eq!(
        title.state(),
        &shown("A - B"),
        "no title clips (a composition still loading) says nothing about them"
    );

    title.note_clips(Some(&vec![clip(200, 1900)]));
    assert_eq!(
        title.state(),
        &TitleState::Unknown,
        "new ids: Arena relaunched, its clip holds what it restored"
    );
}

#[test]
fn an_action_records_the_clips_it_ran_on() {
    let mut title = WallTitle::new();
    title.begin(TitleAction::FadeIn("A - B"), vec![clip(200, 1900)]);
    title.finish(true);

    title.note_clips(Some(&vec![clip(200, 1900)]));
    assert_eq!(
        title.state(),
        &shown("A - B"),
        "the clips the show ran on are the state's clips"
    );
}

// -- take_queued: a Resync supersedes the title commands queued before it ------

fn show(song: &str) -> ResolumeCommand {
    ResolumeCommand::ShowTitle {
        song: song.to_string(),
        artist: "Artist".to_string(),
    }
}

fn resync(title: Option<&str>) -> ResolumeCommand {
    ResolumeCommand::Resync {
        title: title.map(str::to_string),
    }
}

fn line(en: &str) -> ResolumeCommand {
    ResolumeCommand::ShowSubtitles {
        en: en.to_string(),
        next_en: String::new(),
        sk: None,
        next_sk: None,
        suppress_en: false,
    }
}

/// A short, comparable label per command.
fn labels(cmds: &[ResolumeCommand]) -> Vec<String> {
    cmds.iter()
        .map(|cmd| match cmd {
            ResolumeCommand::ShowTitle { song, .. } => format!("show {song}"),
            ResolumeCommand::HideTitle => "hide".to_string(),
            ResolumeCommand::Resync { title } => format!("resync {title:?}"),
            ResolumeCommand::ShowSubtitles { en, .. } => format!("line {en}"),
            ResolumeCommand::HideSubtitles => "clear".to_string(),
            ResolumeCommand::RefreshMapping => "refresh".to_string(),
            ResolumeCommand::Shutdown => "shutdown".to_string(),
        })
        .collect()
}

async fn queued(cmds: Vec<ResolumeCommand>) -> Vec<String> {
    let (tx, mut rx) = mpsc::channel(16);
    for cmd in cmds {
        tx.send(cmd).await.unwrap();
    }
    let first = rx.recv().await.unwrap();
    labels(&take_queued(first, &mut rx))
}

#[tokio::test]
async fn a_resync_drops_the_title_commands_queued_before_it_and_keeps_the_rest() {
    assert_eq!(
        queued(vec![
            show("Next"),
            line("one"),
            ResolumeCommand::HideTitle,
            resync(None),
            line("two"),
        ])
        .await,
        ["line one", "resync None", "line two"],
        "the queued show and hide would only flash on the way to the resync"
    );
    assert_eq!(
        queued(vec![resync(Some("A - B")), resync(None), show("Later")]).await,
        ["resync None", "show Later"],
        "only the last resync counts; a title command after it is newer and stays"
    );
}

#[tokio::test]
async fn without_a_resync_every_queued_command_runs_in_order() {
    assert_eq!(
        queued(vec![
            show("Song"),
            line("one"),
            ResolumeCommand::HideTitle,
            ResolumeCommand::HideSubtitles,
        ])
        .await,
        ["show Song", "line one", "hide", "clear"]
    );
}

#[tokio::test]
async fn a_lone_command_is_taken_alone() {
    assert_eq!(
        queued(vec![resync(Some("A - B"))]).await,
        ["resync Some(\"A - B\")"]
    );
}
