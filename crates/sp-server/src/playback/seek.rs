//! The engine's seek (`EngineCommand::Seek`, the dashboard's seek bar):
//! the pipeline seeks the current song.

use super::PlaybackEngine;
use super::pipeline::PipelineCommand;

impl PlaybackEngine {
    /// Seek to `position_ms` within the currently-playing song on the given
    /// playlist. No-op when no pipeline exists for that playlist or when no
    /// song is loaded — the pipeline's own Seek handler ignores it.
    #[cfg_attr(test, mutants::skip)]
    pub async fn seek(&mut self, playlist_id: i64, position_ms: u64) {
        if let Some(pp) = self.pipelines.get(&playlist_id) {
            pp.pipeline.send(PipelineCommand::Seek { position_ms });
        }
    }
}
