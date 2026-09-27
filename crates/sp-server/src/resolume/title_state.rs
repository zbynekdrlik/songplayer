//! The wall title's on-air state, owned by the Resolume driver (#217
//! addendum 3).
//!
//! The engine only declares what SHOULD be on the wall: a `ShowTitle` /
//! `HideTitle` from its song timers, a `Resync` after a Resolume recovery or
//! an OBS scene-on. It cannot see what is already queued at the driver, so a
//! title shown twice ran the fade twice (a blink) and a hide on a relaunched
//! clip faded restored text from full opacity (a flash). The driver compares
//! each command with what it last did to the `#sp-title` clips and acts only
//! on a difference.

use tokio::sync::mpsc;

use crate::resolume::ResolumeCommand;
use crate::resolume::driver::ClipInfo;

/// What the driver last did to the `#sp-title` clips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TitleState {
    /// Not known: at startup, and after the `#sp-title` clip ids changed.
    /// Arena re-ids every clip on relaunch, and the relaunched clip holds
    /// whatever its saved composition restored.
    Unknown,
    /// The last hide finished: opacity 0, no text.
    Hidden,
    /// The last show finished: this text at full opacity.
    Shown(String),
    /// A show started and did not finish (a request failed): partly up.
    FadingIn(String),
    /// A hide started and did not finish (a request failed): partly up.
    FadingOut,
}

/// A title command from the engine.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TitleIntent<'a> {
    /// `ShowTitle` (the song's 1.5 s show timer): this text fades in.
    Show(&'a str),
    /// `HideTitle` (the song-end timer, a scene-off): the title fades out.
    Hide,
    /// `Resync`: this title, or none, SHOULD be up now.
    Resync(Option<&'a str>),
}

/// What the driver does about a [`TitleIntent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TitleAction<'a> {
    Nothing,
    /// `handlers::show_title`: set the text, then fade in from 5 %.
    FadeIn(&'a str),
    /// `handlers::hide_title`: fade out from full opacity, clear the text.
    FadeOut,
    /// `handlers::hide_title_now`: opacity 0 at once, clear the text.
    HideNow,
}

impl TitleState {
    /// Act only on a difference.
    ///
    /// - A title (a Show, or a Resync naming one) that is already `Shown`:
    ///   nothing, as a second fade from 5 % is a visible blink. Otherwise it
    ///   fades in. An empty Show shows nothing (`show_title`'s old no-op); an
    ///   empty Resync title is no title.
    /// - A Hide: nothing while `Hidden`. It fades only a title known to be up
    ///   (`Shown`); any other state hides at once, because `hide_title` fades
    ///   from FULL opacity and would flash a clip that is not.
    /// - A Resync naming no title: nothing while `Hidden`, else hide at once.
    pub(crate) fn plan<'a>(&self, intent: TitleIntent<'a>) -> TitleAction<'a> {
        let wanted = match intent {
            TitleIntent::Show("") => return TitleAction::Nothing,
            TitleIntent::Show(text) => Some(text),
            TitleIntent::Resync(title) => title.filter(|text| !text.is_empty()),
            TitleIntent::Hide => {
                return match self {
                    TitleState::Hidden => TitleAction::Nothing,
                    TitleState::Shown(_) => TitleAction::FadeOut,
                    _ => TitleAction::HideNow,
                };
            }
        };
        match wanted {
            Some(text) if self.shows(text) => TitleAction::Nothing,
            Some(text) => TitleAction::FadeIn(text),
            None if *self == TitleState::Hidden => TitleAction::Nothing,
            None => TitleAction::HideNow,
        }
    }

    /// Whether `text` is fully up.
    fn shows(&self, text: &str) -> bool {
        matches!(self, TitleState::Shown(shown) if shown == text)
    }
}

/// The driver's [`TitleState`] and the `#sp-title` clips it was reached on.
#[derive(Debug)]
pub(crate) struct WallTitle {
    state: TitleState,
    clips: Vec<ClipInfo>,
}

impl WallTitle {
    pub(crate) fn new() -> Self {
        Self {
            state: TitleState::Unknown,
            clips: Vec::new(),
        }
    }

    /// A given state on given clips (the driver tests start from one).
    #[cfg(test)]
    pub(crate) fn at(state: TitleState, clips: Vec<ClipInfo>) -> Self {
        Self { state, clips }
    }

    pub(crate) fn state(&self) -> &TitleState {
        &self.state
    }

    pub(crate) fn plan<'a>(&self, intent: TitleIntent<'a>) -> TitleAction<'a> {
        self.state.plan(intent)
    }

    /// A refresh mapped `clips` for `#sp-title`. A state holds only for the
    /// clips it was reached on: other ids mean Arena relaunched (it re-ids
    /// every clip), and the relaunched clip shows whatever Arena restored, so
    /// the state becomes `Unknown`. The same ids after an outage keep it: the
    /// clip is as the driver left it. No clips (a composition still loading)
    /// says nothing about them.
    pub(crate) fn note_clips(&mut self, clips: Option<&Vec<ClipInfo>>) {
        if let Some(clips) = clips.filter(|clips| !clips.is_empty())
            && *clips != self.clips
        {
            self.state = TitleState::Unknown;
        }
    }

    /// A title action starts on `clips`. Until `finish`, the clips are
    /// between two states.
    pub(crate) fn begin(&mut self, action: TitleAction<'_>, clips: Vec<ClipInfo>) {
        self.state = match action {
            TitleAction::FadeIn(text) => TitleState::FadingIn(text.to_string()),
            TitleAction::FadeOut | TitleAction::HideNow => TitleState::FadingOut,
            TitleAction::Nothing => return,
        };
        self.clips = clips;
    }

    /// The action ended; `ok` = every request of it answered. A failed one
    /// leaves `FadingIn` / `FadingOut`, which the next Resync converges.
    pub(crate) fn finish(&mut self, ok: bool) {
        if !ok {
            return;
        }
        self.state = match std::mem::replace(&mut self.state, TitleState::Unknown) {
            TitleState::FadingIn(text) => TitleState::Shown(text),
            TitleState::FadingOut => TitleState::Hidden,
            other => other,
        };
    }
}

/// The commands to run now: `first` and every command already queued behind
/// it. The driver runs one command at a time, and a fade blocks it for ~1 s,
/// so commands queue behind a slow step (a refresh, a fade, a timeout). A
/// `Resync` is the engine's LATER statement of what the wall should show, so
/// the title commands queued before it are dropped: running them would only
/// flash the title on the way (a queued ShowTitle fading in, then the
/// `Resync(None)` hiding it). Everything else keeps its order.
pub(crate) fn take_queued(
    first: ResolumeCommand,
    rx: &mut mpsc::Receiver<ResolumeCommand>,
) -> Vec<ResolumeCommand> {
    let mut batch = vec![first];
    while let Ok(cmd) = rx.try_recv() {
        batch.push(cmd);
    }
    supersede_by_resync(batch)
}

/// Drop the title commands before the last `Resync` of `batch`.
fn supersede_by_resync(batch: Vec<ResolumeCommand>) -> Vec<ResolumeCommand> {
    let Some(last) = batch
        .iter()
        .rposition(|cmd| matches!(cmd, ResolumeCommand::Resync { .. }))
    else {
        return batch;
    };
    batch
        .into_iter()
        .enumerate()
        .filter(|(i, cmd)| *i >= last || !is_title_command(cmd))
        .map(|(_, cmd)| cmd)
        .collect()
}

fn is_title_command(cmd: &ResolumeCommand) -> bool {
    matches!(
        cmd,
        ResolumeCommand::ShowTitle { .. }
            | ResolumeCommand::HideTitle
            | ResolumeCommand::Resync { .. }
    )
}

#[cfg(test)]
#[path = "title_state_tests.rs"]
mod tests;
