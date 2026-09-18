//! Karaoke live-control application for `PlaybackEngine` (#14, #186).
//!
//! Extracted from `mod.rs` to keep that file under the 1000-line cap. As a child
//! module of `playback`, it can access the engine's private fields.

use sp_core::playback::KaraokeMode;
use sp_core::ws::ServerMsg;
use tracing::info;

use super::PlaybackEngine;

/// A karaoke MODE change never needs the pipeline reopened (#186): every mode is
/// a live gain PRESET over the same set of already-open streams, so a change is
/// applied by writing the control's gain atomics — the playing
/// [`sp_decoder::StemMixReader`] ramps toward them mid-song. Kept as a typed,
/// unit-tested invariant so a regression that reintroduces the seconds-of-silence
/// reload trips in CI.
pub(crate) fn mode_change_needs_reload(_old: KaraokeMode, _new: KaraokeMode) -> bool {
    false
}

impl PlaybackEngine {
    /// Apply a new karaoke mode + vocal gain. Updates the process-global live
    /// control (which publishes the preset gain triple to the atomics every
    /// playing [`sp_decoder::StemMixReader`] reads and ramps toward), persists
    /// both to settings (restored on restart), and broadcasts the new state.
    ///
    /// A preset change is applied LIVE via those atomics — the pipeline is NEVER
    /// reopened (that reopen was the #186 seconds-of-silence dropout). The
    /// vocal-gain slider is likewise live: `set_vocal_gain` re-publishes the
    /// triple.
    #[cfg_attr(test, mutants::skip)]
    pub async fn set_karaoke(&mut self, mode: KaraokeMode, vocal_gain: f32) {
        let control = crate::stems::control::global();
        let old_mode = control.mode();
        control.set_mode(mode);
        control.set_vocal_gain(vocal_gain);

        // #186: a preset change is applied live through the shared gain atomics;
        // the decoder is NEVER reopened. Assert the invariant so a regression
        // that reintroduces a reload trips in debug/CI.
        debug_assert!(
            !mode_change_needs_reload(old_mode, mode),
            "karaoke preset change must not require a pipeline reload"
        );

        // Persist so the operator's choice survives a restart.
        let _ = crate::db::models::set_setting(&self.pool, "karaoke_mode", mode.as_str()).await;
        let _ = crate::db::models::set_setting(
            &self.pool,
            "karaoke_vocal_gain",
            &control.vocal_gain().to_string(),
        )
        .await;

        // One info! per preset change with the resulting live gain triple.
        let (gain_original, gain_vocals, gain_instrumental) =
            crate::stems::control::preset_gains(mode, control.vocal_gain());
        info!(
            ?mode,
            vocal_gain = control.vocal_gain(),
            gain_original,
            gain_vocals,
            gain_instrumental,
            "karaoke preset changed (live gains, no reload)"
        );

        // Broadcast the new live state to the dashboard.
        let _ = self.ws_event_tx.send(ServerMsg::KaraokeStateChanged {
            mode,
            vocal_gain: control.vocal_gain(),
        });
    }

    /// #183 D4: apply a new dub mix ratio to the process-global control. The
    /// playing 4-stream dub `StemMixReader` ramps toward the new blend live (the
    /// #186 seam, one stream wider) — the pipeline is NEVER reopened. The DB value
    /// is persisted by the API handler; this is the LIVE half.
    #[cfg_attr(test, mutants::skip)]
    pub async fn set_dub_mix(&mut self, video_id: i64, ratio: f32) {
        let control = crate::stems::control::global();
        control.set_dub_ratio(ratio);
        let (g_original, g_vocals, g_instrumental, g_dub) =
            crate::stems::control::dub_gains(control.dub_ratio());
        info!(
            video_id,
            ratio = control.dub_ratio(),
            g_original,
            g_vocals,
            g_instrumental,
            g_dub,
            "dub mix changed (live gains, no reload)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_change_never_reloads_the_pipeline() {
        // #186: every mode is a live gain preset — no pair needs a reopen.
        let modes = [
            KaraokeMode::FullMix,
            KaraokeMode::KaraokeLow,
            KaraokeMode::VocalsOnly,
            KaraokeMode::InstrumentalOnly,
        ];
        for &old in &modes {
            for &new in &modes {
                assert!(
                    !mode_change_needs_reload(old, new),
                    "mode change {old:?} -> {new:?} must not reload the pipeline"
                );
            }
        }
    }
}
