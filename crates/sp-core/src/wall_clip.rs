//! #275 (SNV, 14.10.2026): whether SongPlayer's picture runs on the wall.
//!
//! Arena crashed and its relaunch script brought it back at 11:51Z. After a
//! relaunch no clip runs until the operator presses a Stream Deck scene, so
//! the wall was black for 5.5 h while songs played on SongPlayer's program,
//! and the wall report read `ok`: it reads WHERE Arena draws (`wall`),
//! never WHAT. sp-server's Resolume host driver reads each SongPlayer clip's
//! `connected` state and whether its layer, its layer group and the
//! composition show (`resolume/sp_clips.rs`); this module names that fact
//! ([`SongplayerClip`], the `songplayer_clip` of `GET /api/v1/wall`) and the
//! health bar's note.
//!
//! The owner's rules hold: SongPlayer never triggers, connects or "restores"
//! a clip (clips run only through the operator's Stream Deck), and never
//! enforces a setup. The operator may show something else on purpose
//! (slides during a sermon), so the note is calm (amber, never red), shown
//! only while a song plays on program, and it is never a fault of the wall.
//!
//! A clip is SongPlayer's when its `video.description` (Arena's name of the
//! clip's source) is exactly one of SongPlayer's Spout senders
//! ([`SPOUT_SOURCES`]). An NDI clip's description is only `NDI Source`:
//! Arena's REST does not name the NDI source, so a clip of `SP-program`
//! over NDI (PP's fallback) cannot be told from another NDI clip. The fact
//! lists the visible NDI clips by name, and the note says "Spout".

use serde::{Deserialize, Serialize};

use crate::audio_faults::duration_sk;
use crate::playback::PlaybackState;

/// SongPlayer's Spout senders as Arena names a clip's source
/// (`video.description`): the 4K wall feed and the FHD program (#223,
/// #239).
pub const SPOUT_SOURCES: [&str; 2] = ["SP-program-MAX", "SP-program"];

/// An NDI clip's `video.description`: Arena gives no source name.
pub const NDI_SOURCE: &str = "NDI Source";

/// Whether a SongPlayer clip runs on the wall.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipState {
    /// Arena's composition is not read (not loaded yet, Arena does not
    /// answer, no Resolume host) or a value did not read: no claim.
    #[default]
    Unknown,
    /// At least one SongPlayer clip is connected on a layer that shows.
    Running,
    /// No SongPlayer clip shows (also when the composition has none).
    #[serde(rename = "none")]
    NotRunning,
}

impl ClipState {
    /// The state's name on the wire (`data-clip`, the gate's log).
    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Running => "running",
            Self::NotRunning => "none",
        }
    }
}

/// One clip Arena shows: its layer's name, its own name and its source.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipView {
    #[serde(default)]
    pub layer: String,
    #[serde(default)]
    pub clip: String,
    /// Arena's `video.description` (`SP-program-MAX`, `NDI Source`).
    #[serde(default)]
    pub source: String,
}

impl ClipView {
    /// `L# Fill — Yt music`.
    pub fn text(&self) -> String {
        format!("{} — {}", self.layer, self.clip)
    }
}

/// Each clip as `layer — clip (source)`, joined by `; `.
fn with_sources(clips: &[ClipView]) -> String {
    let each: Vec<String> = clips
        .iter()
        .map(|c| format!("{} ({})", c.text(), c.source))
        .collect();
    each.join("; ")
}

/// Each clip as `layer — clip`, joined by `; `.
fn names(clips: &[ClipView]) -> String {
    let each: Vec<String> = clips.iter().map(ClipView::text).collect();
    each.join("; ")
}

/// The segment's words while no SongPlayer clip runs, after `Stena: `.
pub const NOTE_SK: &str = "nebeží Spout klip SongPlayeru";

/// SongPlayer's clip on the wall as `GET /api/v1/wall` carries it
/// (`songplayer_clip`). The default (and an older server's absent field) is
/// `unknown`, read never.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SongplayerClip {
    #[serde(default)]
    pub state: ClipState,
    /// SongPlayer's clips that show (empty unless `running`).
    #[serde(default)]
    pub clips: Vec<ClipView>,
    /// The NDI clips that show: Arena does not name their source, so one may
    /// be SongPlayer's `SP-program` over NDI.
    #[serde(default)]
    pub ndi_clips: Vec<ClipView>,
    /// When the state began (UTC unix ms; `None` before the first read). A
    /// `running` or `none` counts from its start across `unknown` gaps (an
    /// unread poll is no change of what runs).
    #[serde(default)]
    pub since_ms: Option<i64>,
    /// The server's clock when it answered: how long the state holds is read
    /// against it, never against the browser's clock (another machine).
    #[serde(default)]
    pub now_ms: Option<i64>,
    /// Why a host's fact is unknown (English, for the log and the gate);
    /// `None` when every host read.
    #[serde(default)]
    pub read_error: Option<String>,
    /// SongPlayer's own reader cannot read Arena's answer (a param with no
    /// id, a value of another shape, a by-id answer that does not decode):
    /// the post-deploy gate fails on it. Arena not answering, or its
    /// composition not loaded yet, is no reader fault.
    #[serde(default)]
    pub reader_fault: bool,
}

impl SongplayerClip {
    /// The health bar names it: a song plays on program and no SongPlayer
    /// clip runs.
    pub fn missing(&self, song_on_program: bool) -> bool {
        song_on_program && self.state == ClipState::NotRunning
    }

    /// How long the state holds, on the server's clock: `12 min`; `None`
    /// while either instant is unknown.
    pub fn held_for_sk(&self) -> Option<String> {
        let (since, now) = (self.since_ms?, self.now_ms?);
        Some(duration_sk(now - since))
    }

    /// The note after `Stena: `: `nebeží Spout klip SongPlayeru (12 min)`.
    pub fn short_sk(&self) -> String {
        match self.held_for_sk() {
            Some(held) => format!("{NOTE_SK} ({held})"),
            None => NOTE_SK.to_string(),
        }
    }

    /// The tooltip's sentences: which SongPlayer clips run, or that none
    /// does (for how long, that only the operator starts clips, and the
    /// NDI clips SongPlayer cannot tell); nothing while unknown.
    pub fn text_sk(&self) -> Option<String> {
        match self.state {
            ClipState::Unknown => None,
            ClipState::Running => Some(format!(
                "Na stene beží klip SongPlayeru: {}.",
                with_sources(&self.clips)
            )),
            ClipState::NotRunning => {
                let held = self
                    .held_for_sk()
                    .map(|held| format!(" už {held}"))
                    .unwrap_or_default();
                let ndi = if self.ndi_clips.is_empty() {
                    String::new()
                } else {
                    format!("; viditeľné NDI klipy: {}", names(&self.ndi_clips))
                };
                Some(format!(
                    "Na stene{held} nebeží žiadny Spout klip SongPlayeru ({}). Klipy spúšťa len \
                     operátor zo Stream Decku, SongPlayer žiadny nespúšťa. Klip so zdrojom NDI \
                     SongPlayer nerozozná (Resolume nepovie, aký NDI zdroj má){ndi}.",
                    SPOUT_SOURCES.join(" ani ")
                ))
            }
        }
    }
}

/// A song plays on program: a playlist the server told (`state_known`) is
/// on program (`Playing`, the Player badge's own WS state, #225) with a
/// song.
pub fn song_on_program(state_known: bool, state: PlaybackState, has_song: bool) -> bool {
    state_known && state == PlaybackState::Playing && has_song
}

#[cfg(test)]
#[path = "wall_clip_tests.rs"]
mod tests;
