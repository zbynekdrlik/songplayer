//! Process-global now-playing registry (#177).
//!
//! Holds, per playlist, the video id its pipeline is currently playing. Written
//! by the playback engine (set when a song starts, cleared on Stop) and read by
//! the karaoke API (`GET /api/v1/karaoke` → `now_playing[]`) WITHOUT going
//! through the engine command channel — the same decoupling the NDI health
//! registry uses. A process-global (like `stems::control::global()`) so neither
//! the engine struct nor `AppState` needs a new field.
//!
//! `set` on a `Started`/replay, `clear` on Stop. A PAUSE deliberately KEEPS the
//! entry — a paused song is still the panel's current song (resumable via
//! `paused_at`), so the karaoke controls still apply to it. Because the wall
//! plays continuously, each new song's `Started` overwrites the entry, so the
//! snapshot stays fresh; `get_karaoke` also skips any entry whose row vanished.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::RwLock;

/// playlist_id → currently-playing video_id.
#[derive(Debug, Default)]
pub struct NowPlayingRegistry {
    inner: RwLock<HashMap<i64, i64>>,
}

impl NowPlayingRegistry {
    /// Record `video_id` as the song now playing on `playlist_id`.
    pub fn set(&self, playlist_id: i64, video_id: i64) {
        if let Ok(mut g) = self.inner.write() {
            g.insert(playlist_id, video_id);
        }
    }

    /// Drop the entry for `playlist_id` (the pipeline stopped).
    pub fn clear(&self, playlist_id: i64) {
        if let Ok(mut g) = self.inner.write() {
            g.remove(&playlist_id);
        }
    }

    /// Snapshot of `(playlist_id, video_id)` pairs currently playing, sorted by
    /// playlist id for a stable payload order.
    pub fn snapshot(&self) -> Vec<(i64, i64)> {
        let mut v: Vec<(i64, i64)> = match self.inner.read() {
            Ok(g) => g.iter().map(|(&p, &vid)| (p, vid)).collect(),
            Err(_) => Vec::new(),
        };
        v.sort_unstable();
        v
    }
}

/// The process-global now-playing registry.
pub fn global() -> &'static NowPlayingRegistry {
    static REG: OnceLock<NowPlayingRegistry> = OnceLock::new();
    REG.get_or_init(NowPlayingRegistry::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_clear_and_snapshot() {
        let reg = NowPlayingRegistry::default();
        reg.set(2, 20);
        reg.set(1, 10);
        assert_eq!(reg.snapshot(), vec![(1, 10), (2, 20)]);
        reg.set(1, 11); // a new song on playlist 1 overwrites
        assert_eq!(reg.snapshot(), vec![(1, 11), (2, 20)]);
        reg.clear(1);
        assert_eq!(reg.snapshot(), vec![(2, 20)]);
    }
}
