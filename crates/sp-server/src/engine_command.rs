//! Commands the API/OBS/Resolume layers send to the playback engine — moved out of lib.rs for the 1000-line cap.

use sp_core::mixer_model::{MixFaders, MixKind};
use sp_core::playback::PlaybackMode;

/// Commands sent from the API layer to the playback engine.
#[derive(Debug, Clone)]
pub enum EngineCommand {
    SceneChanged {
        playlist_id: i64,
        on_program: bool,
    },
    Play {
        playlist_id: i64,
    },
    Pause {
        playlist_id: i64,
    },
    Skip {
        playlist_id: i64,
    },
    /// Go back to the previous track. Pops the most recent entry off
    /// the per-playlist history stack maintained by `PlaybackEngine`
    /// and plays it. No-op if the history is empty.
    Previous {
        playlist_id: i64,
    },
    SetMode {
        playlist_id: i64,
        mode: PlaybackMode,
    },
    /// Jump to a specific video within a playlist and start playing it
    /// immediately. For custom playlists, also updates
    /// `playlists.current_position` so subsequent Skip advances from the
    /// new position. For youtube playlists it behaves like Previous
    /// (plays the given video but does not affect the random-unplayed
    /// selector; the next Skip will pick a fresh random video).
    ///
    /// When `position_ms` is `Some(ms)`, the pipeline seeks to that
    /// offset before starting frame submission — atomic play-from-position
    /// that eliminates the race in the old play-video + delayed seek dance
    /// (see issue #88).
    PlayVideo {
        playlist_id: i64,
        video_id: i64,
        position_ms: Option<u64>,
    },
    /// Seek the currently-playing song on the given playlist to `position_ms`.
    /// No-op when no pipeline exists or no song is loaded.
    Seek {
        playlist_id: i64,
        position_ms: u64,
    },
    /// Re-emit current title + subtitle state after a Resolume host recovered.
    ResolumeRecovered {
        host: String,
    },
    /// #132: Register a playback pipeline for a playlist created or activated
    /// at runtime via the API, so scene detection can start it without a
    /// process restart. The engine reconciles from the DB (creates only when
    /// the playlist is active and has a non-empty NDI name); idempotent and
    /// safe to over-send.
    EnsurePipeline {
        playlist_id: i64,
    },
    /// #132: Tear down a playlist's pipeline after a runtime delete or
    /// deactivate. No-op if the engine has no pipeline for it.
    RemovePipeline {
        playlist_id: i64,
    },
    /// #173: Operator/verification trigger for a single NDI dark-wall recovery
    /// rung on one playlist, over the healthy OBS WebSocket. Backs the admin
    /// `POST /api/v1/ndi/recover/{playlist_id}?step=...` endpoint. The engine
    /// resolves the playlist's `ndi_output_name` and forwards
    /// `ObsCommand::NudgeNdiReceiver`. Does NOT touch the automatic ladder state.
    TriggerNdiRecovery {
        playlist_id: i64,
        step: crate::obs::ndi_recovery::RecoveryStep,
    },
    /// #184 round G/G2: set ONE memory of the live mixer console — the three fader
    /// positions `[vokály, podklad, dabing]` for `kind` (song or dub). The engine
    /// writes them to the process-global `MixControl`, which republishes ONLY that
    /// kind's reader family's gains with NO pipeline reopen (the #186 seam), and
    /// broadcasts `MixChanged`. The persist is done by the API handler after this
    /// live push. Supersedes `SetKaraoke` + `SetDubMix`.
    SetMix {
        kind: MixKind,
        faders: MixFaders,
    },
}
