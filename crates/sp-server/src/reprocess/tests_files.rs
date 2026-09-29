//! #136 reopen (29.9.2026): a metadata upgrade renames a song's COMPLETE file
//! set — the video, the audio, and every file named after the audio (the
//! karaoke stems `stems::stem_paths` derives, the dub track + transcripts
//! `stems::dub_path` / `dub_transcripts_path` derive).
//!
//! The box, after the repair of ~99 songs: video 326 `IYAOosrh7HY` had
//! `Gods Not Dead_Enjoy Worship_IYAOosrh7HY_normalized_audio.flac` (renamed) next
//! to `…_normalized_gf_audio_vocals.flac` (left behind), so the lyrics isolation
//! waited for stems forever and the stem mixer found none at playback.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;
use crate::db;
use crate::metadata::{MetadataError, MetadataProvider, ProviderChain};
use async_trait::async_trait;
use sp_core::metadata::{MetadataSource, VideoMetadata};
use sqlx::{Row, SqlitePool};

const ID: &str = "IYAOosrh7HY";

/// Names video 326 the way the repair did.
struct NamesTheSong;

#[async_trait]
impl MetadataProvider for NamesTheSong {
    async fn extract(&self, _video_id: &str, _title: &str) -> Result<VideoMetadata, MetadataError> {
        Ok(VideoMetadata {
            song: "Gods Not Dead".into(),
            artist: "Enjoy Worship".into(),
            source: MetadataSource::Gemini,
            gemini_failed: false,
        })
    }

    fn name(&self) -> &str {
        "claude"
    }
}

/// Every file of one song under one base name.
struct SongSet {
    video: PathBuf,
    audio: PathBuf,
    vocals: PathBuf,
    instrumental: PathBuf,
    dub: PathBuf,
    transcripts: PathBuf,
}

impl SongSet {
    fn under(dir: &Path, base: &str) -> Self {
        Self {
            video: dir.join(format!("{base}_video.mp4")),
            audio: dir.join(format!("{base}_audio.flac")),
            vocals: dir.join(format!("{base}_audio_vocals.flac")),
            instrumental: dir.join(format!("{base}_audio_instrumental.flac")),
            dub: dir.join(format!("{base}_dub.flac")),
            transcripts: dir.join(format!("{base}_dub_transcripts.json")),
        }
    }

    fn all(&self) -> [&PathBuf; 6] {
        [
            &self.video,
            &self.audio,
            &self.vocals,
            &self.instrumental,
            &self.dub,
            &self.transcripts,
        ]
    }

    fn write_all(&self) {
        for p in self.all() {
            std::fs::write(p, p.to_string_lossy().as_bytes()).unwrap();
        }
    }
}

fn old_set(dir: &Path) -> SongSet {
    SongSet::under(dir, &format!("Old Song_Old Artist_{ID}_normalized_gf"))
}

fn new_set(dir: &Path) -> SongSet {
    SongSet::under(dir, &format!("Gods Not Dead_Enjoy Worship_{ID}_normalized"))
}

fn text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// A parser-named (`_gf`) row whose stems are done and whose dub is ready, all
/// recorded under the old name.
async fn pool_with_row(old: &SongSet) -> (SqlitePool, i64) {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'ytalex', 'url')")
        .execute(&pool)
        .await
        .unwrap();
    let id: i64 = sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, song, artist, gemini_failed,
                             normalized, file_path, audio_file_path, metadata_source,
                             stem_status, vocals_file_path, instrumental_file_path,
                             dub_status, dub_file_path)
         VALUES (1, ?, 't', 'Old Song', 'Old Artist', 1, 1, ?, ?, 'regex',
                 'done', ?, ?, 'ready', ?)
         RETURNING id",
    )
    .bind(ID)
    .bind(text(&old.video))
    .bind(text(&old.audio))
    .bind(text(&old.vocals))
    .bind(text(&old.instrumental))
    .bind(text(&old.dub))
    .fetch_one(&pool)
    .await
    .unwrap()
    .get("id");
    (pool, id)
}

/// `(file_path, audio_file_path, vocals_file_path, instrumental_file_path,
/// dub_file_path)` of the row.
async fn recorded(pool: &SqlitePool, id: i64) -> [Option<String>; 5] {
    let r = sqlx::query(
        "SELECT file_path, audio_file_path, vocals_file_path, instrumental_file_path,
                dub_file_path
         FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    [
        r.get("file_path"),
        r.get("audio_file_path"),
        r.get("vocals_file_path"),
        r.get("instrumental_file_path"),
        r.get("dub_file_path"),
    ]
}

async fn song_and_flag(pool: &SqlitePool, id: i64) -> (String, String, i64) {
    let r = sqlx::query("SELECT song, artist, gemini_failed FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    (r.get("song"), r.get("artist"), r.get("gemini_failed"))
}

fn worker(pool: &SqlitePool, cache: &Path) -> ReprocessWorker {
    let chain = Arc::new(ProviderChain::new(vec![Box::new(NamesTheSong)]));
    ReprocessWorker::new(pool.clone(), chain, cache.to_path_buf())
}

#[tokio::test]
async fn a_metadata_upgrade_moves_the_stems_and_the_dub_with_the_audio() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = (old_set(dir.path()), new_set(dir.path()));
    old.write_all();
    let (pool, id) = pool_with_row(&old).await;

    assert_eq!(worker(&pool, dir.path()).process_all().await.unwrap(), 1);

    for (from, to) in old.all().into_iter().zip(new.all()) {
        assert!(!from.exists(), "{} must be moved away", from.display());
        assert_eq!(
            std::fs::read_to_string(to).unwrap(),
            text(from),
            "{} must hold the file that was {}",
            to.display(),
            from.display()
        );
    }
    assert_eq!(
        recorded(&pool, id).await,
        [
            Some(text(&new.video)),
            Some(text(&new.audio)),
            Some(text(&new.vocals)),
            Some(text(&new.instrumental)),
            Some(text(&new.dub)),
        ],
        "the DB records every file under the new name"
    );
    assert_eq!(
        song_and_flag(&pool, id).await,
        ("Gods Not Dead".into(), "Enjoy Worship".into(), 0)
    );
}

/// A move that fails (on the box: Windows refuses to rename a file another
/// process holds open) must never leave the song split across two names: the
/// files already moved go back, the DB keeps the old names, and the upgraded
/// song/artist are still stored.
#[tokio::test]
async fn a_failed_move_keeps_the_whole_set_under_the_old_name() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = (old_set(dir.path()), new_set(dir.path()));
    old.write_all();
    // The video moves last; a non-empty directory at its new name refuses it on
    // Linux and on Windows alike.
    std::fs::create_dir(&new.video).unwrap();
    std::fs::write(new.video.join("blocker"), b"x").unwrap();
    let (pool, id) = pool_with_row(&old).await;

    assert_eq!(worker(&pool, dir.path()).process_all().await.unwrap(), 1);

    for from in old.all() {
        assert_eq!(
            std::fs::read_to_string(from).unwrap(),
            text(from),
            "{} must be back under the old name",
            from.display()
        );
    }
    for to in [
        &new.audio,
        &new.vocals,
        &new.instrumental,
        &new.dub,
        &new.transcripts,
    ] {
        assert!(!to.exists(), "{} must not exist", to.display());
    }
    assert_eq!(
        recorded(&pool, id).await,
        [
            Some(text(&old.video)),
            Some(text(&old.audio)),
            Some(text(&old.vocals)),
            Some(text(&old.instrumental)),
            Some(text(&old.dub)),
        ],
        "the DB keeps the whole set under the old name"
    );
    assert_eq!(
        song_and_flag(&pool, id).await,
        ("Gods Not Dead".into(), "Enjoy Worship".into(), 0),
        "the upgraded metadata is stored even though the files keep their names"
    );
}

/// A song with no stems and no dub yet: only the video and the audio move, and
/// the empty path columns stay empty (a NULL must never become a path).
#[tokio::test]
async fn a_song_without_stems_moves_its_video_and_audio_only() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = (old_set(dir.path()), new_set(dir.path()));
    std::fs::write(&old.video, b"v").unwrap();
    std::fs::write(&old.audio, b"a").unwrap();
    let (pool, id) = pool_with_row(&old).await;
    sqlx::query(
        "UPDATE videos SET stem_status = NULL, vocals_file_path = NULL,
                instrumental_file_path = NULL, dub_status = 'none', dub_file_path = NULL
         WHERE id = ?",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(worker(&pool, dir.path()).process_all().await.unwrap(), 1);

    assert!(new.video.exists() && new.audio.exists());
    assert!(!old.video.exists() && !old.audio.exists());
    assert_eq!(
        recorded(&pool, id).await,
        [
            Some(text(&new.video)),
            Some(text(&new.audio)),
            None,
            None,
            None
        ]
    );
}
