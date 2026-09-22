//! Engine-command dispatch, extracted from `lib.rs` to keep it under the
//! 1000-line cap (#183 D4 — adding `SetDubMix` overflowed the inline `match`).
//!
//! One free async fn drives every [`EngineCommand`] against the owned
//! `PlaybackEngine`; the `lib.rs` command bridge just forwards each received
//! command here. Kept as a plain function (not a method) so the engine impl files
//! stay unchanged.

use crate::EngineCommand;
use crate::playback::PlaybackEngine;
use crate::playback::state::PlayEvent;

/// Apply one API/OBS/Resolume command to the playback engine.
pub(crate) async fn dispatch(engine: &mut PlaybackEngine, cmd: EngineCommand) {
    match cmd {
        EngineCommand::Play { playlist_id } => {
            // Manual /play from the dashboard. Engine dispatches
            // resume-vs-scene-on based on whether Pause captured a snapshot. #88.
            engine.handle_engine_play(playlist_id).await;
        }
        EngineCommand::Pause { playlist_id } => {
            engine
                .handle_command(playlist_id, PlayEvent::SceneOff)
                .await;
        }
        EngineCommand::Skip { playlist_id } => {
            engine.handle_command(playlist_id, PlayEvent::Skip).await;
        }
        EngineCommand::Previous { playlist_id } => {
            // Pops one entry off the per-playlist history stack and plays it. See
            // `PlaybackEngine::handle_previous` for the full contract.
            engine.handle_previous(playlist_id).await;
        }
        EngineCommand::SetMode { playlist_id, mode } => {
            engine
                .handle_command(playlist_id, PlayEvent::SetMode(mode))
                .await;
        }
        EngineCommand::PlayVideo {
            playlist_id,
            video_id,
            position_ms,
        } => {
            engine
                .handle_play_video(playlist_id, video_id, position_ms)
                .await;
        }
        EngineCommand::SceneChanged {
            playlist_id,
            on_program,
        } => {
            // VideosAvailable + SceneOn (on program) or SceneOff (off program)
            // are folded into handle_scene_change so every caller goes through the
            // same sequence.
            engine.handle_scene_change(playlist_id, on_program).await;
        }
        EngineCommand::Seek {
            playlist_id,
            position_ms,
        } => {
            engine.seek(playlist_id, position_ms);
        }
        EngineCommand::ResolumeRecovered { host } => {
            engine.handle_resolume_recovery(&host).await;
        }
        EngineCommand::EnsurePipeline { playlist_id } => {
            // #132: a playlist created/activated at runtime registers its pipeline
            // the same way startup does.
            engine.ensure_pipeline_for_playlist(playlist_id).await;
        }
        EngineCommand::RemovePipeline { playlist_id } => {
            // #132: a playlist deleted/deactivated at runtime tears its pipeline
            // down symmetrically.
            engine.remove_pipeline(playlist_id);
        }
        EngineCommand::SetMix { kind, faders } => {
            engine.set_mix(kind, faders).await; // #184 round G/G2
        }
        EngineCommand::TriggerNdiRecovery { playlist_id, step } => {
            // #173: operator/verification one-shot recovery rung.
            engine.trigger_ndi_recovery(playlist_id, step).await;
        }
    }
}
