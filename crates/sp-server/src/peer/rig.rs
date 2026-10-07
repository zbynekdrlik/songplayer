//! #229 tests: a real node of the exchange — its own in-memory DB, cache dir
//! and HTTP port serving `peer::router` — so two nodes talk over real HTTP.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sp_core::config::{SETTING_NODE_NAME, SETTING_PEER_API_KEY, SETTING_PEERS};
use sqlx::SqlitePool;

use super::Exchange;
use super::config::PeerConfig;
use crate::downloader::cache::{audio_filename, video_filename};

/// SNV's peer key in the tests (at least 32 characters, never a real one).
pub(crate) const SNV_KEY: &str = "snv-example-peer-key-0123456789abcdef";
/// Another node's key: not SNV's.
pub(crate) const OTHER_KEY: &str = "other-example-peer-key-0123456789abc";

/// `len` deterministic bytes; `seed` tells two files apart.
pub(crate) fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

pub(crate) struct TestNode {
    pub(crate) name: String,
    pub(crate) ex: Arc<Exchange>,
    pub(crate) base_url: String,
    dir: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
}

impl TestNode {
    /// A node named `name` (serving with `serve_key` when given) with two
    /// active playlists (ids 1 and 2), listening on a free 127.0.0.1 port.
    pub(crate) async fn start(name: &str, serve_key: Option<&str>) -> Self {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        set(&pool, SETTING_NODE_NAME, name).await;
        if let Some(key) = serve_key {
            set(&pool, SETTING_PEER_API_KEY, key).await;
        }
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (1, 'p1', 'u1', 'SP-p1', 1), (2, 'p2', 'u2', 'SP-p2', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ex = Exchange::new(pool, dir.path().to_path_buf());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let app = super::router(ex.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            name: name.to_string(),
            ex,
            base_url,
            dir,
            server,
        }
    }

    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.ex.pool
    }

    pub(crate) fn cache(&self) -> &Path {
        self.dir.path()
    }

    /// This node as another node's peer, sending `key`.
    pub(crate) fn as_peer(&self, key: &str) -> PeerConfig {
        PeerConfig {
            name: self.name.clone(),
            base_url: self.base_url.clone(),
            key: key.into(),
            cf_client_id: None,
            cf_client_secret: None,
        }
    }

    pub(crate) async fn set_peers(&self, peers: &[PeerConfig]) {
        let json = serde_json::to_string(peers).unwrap();
        set(self.pool(), SETTING_PEERS, &json).await;
    }

    /// A row of `youtube_id` in playlist 1, not downloaded yet.
    pub(crate) async fn add_video(&self, youtube_id: &str) -> i64 {
        self.add_video_to(1, youtube_id).await
    }

    pub(crate) async fn add_video_to(&self, playlist_id: i64, youtube_id: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO videos (playlist_id, youtube_id, title, normalized) \
             VALUES (?, ?, 'A YouTube title', 0) RETURNING id",
        )
        .bind(playlist_id)
        .bind(youtube_id)
        .fetch_one(self.pool())
        .await
        .unwrap()
    }

    /// Row `id` downloaded as `song` / `artist` (source `gemini`): a 2 000-byte
    /// video and a 3 000-byte audio under the download worker's names.
    pub(crate) async fn give_song(
        &self,
        id: i64,
        youtube_id: &str,
        song: &str,
        artist: &str,
    ) -> (PathBuf, PathBuf) {
        let video = self
            .cache()
            .join(video_filename(song, artist, youtube_id, false));
        let audio = self
            .cache()
            .join(audio_filename(song, artist, youtube_id, false));
        std::fs::write(&video, bytes(2_000, 1)).unwrap();
        std::fs::write(&audio, bytes(3_000, 2)).unwrap();
        crate::db::models::mark_video_processed_pair(
            self.pool(),
            id,
            song,
            artist,
            "gemini",
            false,
            &video.to_string_lossy(),
            &audio.to_string_lossy(),
        )
        .await
        .unwrap();
        (video, audio)
    }

    /// Row `id`'s stems, done, named after its current audio.
    pub(crate) async fn give_stems(&self, id: i64) -> (PathBuf, PathBuf) {
        let audio: String = sqlx::query_scalar("SELECT audio_file_path FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(self.pool())
            .await
            .unwrap();
        let (vocals, instrumental) = crate::stems::stem_paths(Path::new(&audio));
        std::fs::write(&vocals, bytes(1_500, 3)).unwrap();
        std::fs::write(&instrumental, bytes(1_700, 4)).unwrap();
        crate::db::models_stems::mark_stems_done(
            self.pool(),
            id,
            &vocals.to_string_lossy(),
            &instrumental.to_string_lossy(),
        )
        .await
        .unwrap();
        (vocals, instrumental)
    }

    /// Row `id`'s lyrics at the current pipeline version, from `source`;
    /// returns the `{yt}_lyrics.json` bytes.
    pub(crate) async fn give_lyrics(&self, id: i64, youtube_id: &str, source: &str) -> Vec<u8> {
        let track = sp_core::lyrics::LyricsTrack {
            version: 1,
            source: source.into(),
            language_source: "en".into(),
            language_translation: "sk".into(),
            lines: vec![sp_core::lyrics::LyricsLine {
                start_ms: 1_000,
                end_ms: 4_000,
                en: "Way maker".into(),
                sk: Some("Cestu robíš".into()),
                words: None,
            }],
        };
        let json = serde_json::to_vec(&track).unwrap();
        let path = self.cache().join(format!("{youtube_id}_lyrics.json"));
        std::fs::write(path, &json).unwrap();
        crate::db::models::mark_video_lyrics_complete(
            self.pool(),
            id,
            source,
            crate::lyrics::LYRICS_PIPELINE_VERSION,
            None,
            None,
        )
        .await
        .unwrap();
        json
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Store a setting of `pool`.
pub(crate) async fn set(pool: &SqlitePool, key: &str, value: &str) {
    crate::db::models::set_setting(pool, key, value)
        .await
        .unwrap();
}
