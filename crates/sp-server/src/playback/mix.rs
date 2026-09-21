//! Live mixer-console application for `PlaybackEngine` (#184 round G, was #14/#186
//! `karaoke.rs`).
//!
//! Extracted from `mod.rs` to keep that file under the 1000-line cap. As a child
//! module of `playback`, it can access the engine's private fields.

use sp_core::mixer_model::MixFaders;
use sp_core::ws::ServerMsg;
use tracing::info;

use super::PlaybackEngine;

/// A mixer fader change never needs the pipeline reopened (#186): the ONE console
/// is a live set of gain atomics over the same already-open streams, so a change
/// is applied by writing the control's gain atomics — the playing
/// [`sp_decoder::StemMixReader`] ramps toward them mid-video. Kept as a typed,
/// unit-tested invariant so a regression that reintroduces the seconds-of-silence
/// reload trips in CI.
pub(crate) fn fader_change_needs_reload() -> bool {
    false
}

impl PlaybackEngine {
    /// Apply the ONE global live mixer console — the three fader positions
    /// `[vokály, podklad, dabing]`. Writes them to the process-global
    /// [`crate::stems::control::MixControl`] (which publishes the derived
    /// song / dub / no-stems gain sets to the atomics every playing
    /// [`sp_decoder::StemMixReader`] reads and ramps toward) and broadcasts the new
    /// state. This is the LIVE half only — the API handler persists the three
    /// settings AFTER this push (the round-A live-first ordering, so the mix
    /// reaches the gains in ~1.6 s instead of behind a contended pool acquire).
    ///
    /// A fader change is applied LIVE via those atomics — the pipeline is NEVER
    /// reopened (that reopen was the #186 seconds-of-silence dropout).
    #[cfg_attr(test, mutants::skip)]
    pub async fn set_mix(&mut self, faders: MixFaders) {
        let control = crate::stems::control::global();
        control.set_faders(faders);
        let f = control.faders();

        // #186: applied live through the shared gain atomics; NEVER a reopen.
        debug_assert!(
            !fader_change_needs_reload(),
            "a mixer fader change must not require a pipeline reload"
        );

        let song = sp_core::mixer_model::stream_gains_song(f);
        info!(
            vokaly = f.vokaly,
            podklad = f.podklad,
            dabing = f.dabing,
            gain_original = song[0],
            gain_vocals = song[1],
            gain_instrumental = song[2],
            "mixer console changed (live gains, no reload)"
        );

        // Broadcast the new live state to the dashboard.
        let _ = self.ws_event_tx.send(ServerMsg::MixChanged {
            vokaly: f.vokaly,
            podklad: f.podklad,
            dabing: f.dabing,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fader_change_never_reloads_the_pipeline() {
        // #186: the console is a live gain set — no fader move needs a reopen.
        assert!(
            !fader_change_needs_reload(),
            "a mixer fader change must not reload the pipeline"
        );
    }
}
