//! #136 (ROZHODNUTÉ 5908227964 item 2): a download of a corrected video keeps
//! the operator's title and never asks the provider chain over it. The title
//! spread of item 1 (`apply_to_video`) is driven through the real router in
//! `api/routes_tests_patch_metadata.rs`.
//! Wired via `#[cfg(test)] #[path = "manual_tests.rs"] mod tests;`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use sp_core::metadata::{MetadataSource, VideoMetadata};
use sqlx::SqlitePool;

use super::*;
use crate::metadata::{MetadataError, MetadataProvider, ProviderChain};

/// A provider that names every video "Another Song" and counts its calls.
struct Counting {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl MetadataProvider for Counting {
    async fn extract(&self, _video_id: &str, _title: &str) -> Result<VideoMetadata, MetadataError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(VideoMetadata {
            song: "Another Song".into(),
            artist: "Another Artist".into(),
            source: MetadataSource::Gemini,
            gemini_failed: false,
        })
    }

    fn name(&self) -> &str {
        "counting"
    }
}

/// The chain of one [`Counting`] provider, and its call counter.
fn chain() -> (ProviderChain, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Counting {
        calls: Arc::clone(&calls),
    };
    (ProviderChain::new(vec![Box::new(provider)]), calls)
}

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u'), (2, 'q', 'v')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// A row waiting for a (re-)download (`normalized = 0`) with this title.
async fn row(pool: &SqlitePool, id: i64, song: Option<&str>, artist: Option<&str>, source: &str) {
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, title, song, artist, \
                             metadata_source, gemini_failed, normalized) \
         VALUES (?, 1, ?, 'Break! - planetboom (Live)', ?, ?, ?, 0, 0)",
    )
    .bind(id)
    .bind(format!("DOWNLOAD{id:03}"))
    .bind(song)
    .bind(artist)
    .bind(source)
    .execute(pool)
    .await
    .unwrap();
}

/// The title a download of row `id`'s video names it after.
async fn title_of(pool: &SqlitePool, chain: &ProviderChain, id: i64) -> DownloadTitle {
    let youtube_id = format!("DOWNLOAD{id:03}");
    download_title(pool, chain, &youtube_id, "Break! - planetboom (Live)").await
}

#[tokio::test]
async fn a_re_download_keeps_the_operators_title_and_never_asks_the_chain() {
    let pool = pool().await;
    row(&pool, 1, Some("Break!"), Some("planetboom"), "manual").await;
    row(&pool, 2, Some("Solo Song"), None, "manual").await;
    let (chain, calls) = chain();

    assert_eq!(
        title_of(&pool, &chain, 1).await,
        DownloadTitle {
            song: "Break!".into(),
            artist: "planetboom".into(),
            source: MANUAL_SOURCE,
            gemini_failed: false,
        }
    );
    assert_eq!(
        title_of(&pool, &chain, 2).await,
        DownloadTitle {
            song: "Solo Song".into(),
            artist: String::new(),
            source: MANUAL_SOURCE,
            gemini_failed: false,
        },
        "a correction with no artist"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the chain is never asked");
    assert_eq!(MANUAL_SOURCE, "manual");
}

/// The correction belongs to the VIDEO: the same video added to another
/// playlist after it was corrected is downloaded under the corrected title.
#[tokio::test]
async fn a_new_row_of_a_corrected_video_takes_its_title() {
    let pool = pool().await;
    row(&pool, 6, Some("Break!"), Some("planetboom"), "manual").await;
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, title, song, artist, normalized) \
         VALUES (7, 2, 'DOWNLOAD006', 'Break! - planetboom (Live)', NULL, NULL, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (chain, calls) = chain();

    let title = download_title(&pool, &chain, "DOWNLOAD006", "Break! - planetboom (Live)").await;
    assert_eq!(
        (title.song.as_str(), title.artist.as_str(), title.source),
        ("Break!", "planetboom", MANUAL_SOURCE)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn any_other_row_asks_the_chain() {
    let pool = pool().await;
    row(&pool, 3, Some("Old Song"), Some("Old Artist"), "gemini").await;
    row(&pool, 4, None, None, "manual").await; // no song to keep
    row(&pool, 5, Some("  "), None, "manual").await;
    let (chain, calls) = chain();

    for id in [3, 4, 5] {
        assert_eq!(
            title_of(&pool, &chain, id).await,
            DownloadTitle {
                song: "Another Song".into(),
                artist: "Another Artist".into(),
                source: "gemini",
                gemini_failed: false,
            },
            "row {id}"
        );
    }
    assert_eq!(
        title_of(&pool, &chain, 99).await.song,
        "Another Song",
        "no row of that video"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

/// `DownloadWorker::process_next` downloads with yt-dlp, so no Linux test
/// drives it; its title step is pinned by its source: it takes the video's
/// title from `download_title` (never the chain directly) and records the
/// download through `record_download` (never `mark_video_processed_pair`
/// directly).
#[test]
fn the_download_worker_takes_the_title_from_download_title() {
    // The worker's code, before its test module.
    let src = include_str!("../downloader/mod.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert!(
        src.contains("download_title(&self.pool, &self.metadata, &row.youtube_id, &row.title)"),
        "process_next asks download_title for the video's title"
    );
    assert!(
        !src.contains("crate::metadata::get_metadata("),
        "the chain is never asked directly"
    );
    assert!(
        src.contains("crate::metadata::manual::record_download("),
        "the download is recorded through record_download"
    );
    assert!(
        !src.contains("mark_video_processed_pair("),
        "never recorded around it"
    );
}

/// The pair a download of row `id`'s video wrote under `title` in `dir`.
fn fresh_pair(dir: &std::path::Path, id: i64, title: &DownloadTitle) -> [std::path::PathBuf; 2] {
    let youtube_id = format!("DOWNLOAD{id:03}");
    let names = [
        crate::downloader::cache::video_filename(&title.song, &title.artist, &youtube_id, false),
        crate::downloader::cache::audio_filename(&title.song, &title.artist, &youtube_id, false),
    ];
    names.map(|name| {
        let path = dir.join(name);
        std::fs::write(&path, b"x").unwrap();
        path
    })
}

/// `(song, artist, metadata_source, gemini_failed, normalized, file_path,
/// audio_file_path)` of row `id`.
type Recorded = (String, String, String, i64, i64, String, String);

async fn recorded(pool: &SqlitePool, id: i64) -> Recorded {
    sqlx::query_as(
        "SELECT song, artist, metadata_source, gemini_failed, normalized, file_path, \
                audio_file_path FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Review round 1: `download_title` is read before the download and the
/// loudnorm (about a minute). A correction made in between (a PATCH of this
/// row or of another row of the video) is final: the download is recorded
/// under the correction and its fresh pair renamed after it, never the
/// chain's title over `'manual'`.
#[tokio::test]
async fn a_correction_made_during_the_download_names_the_fresh_pair() {
    let dir = tempfile::tempdir().unwrap();
    let pool = pool().await;
    row(&pool, 8, None, None, "gemini").await;
    let (chain, _) = chain();
    let title = title_of(&pool, &chain, 8).await; // the chain's "Another Song"
    let [video, audio] = fresh_pair(dir.path(), 8, &title);
    // The operator corrects the video while it downloads.
    sqlx::query(
        "UPDATE videos SET song = 'Break!', artist = 'planetboom', \
         metadata_source = 'manual', gemini_failed = 0 WHERE id = 8",
    )
    .execute(&pool)
    .await
    .unwrap();

    let written = record_download(&pool, dir.path(), 8, "DOWNLOAD008", &title, &video, &audio)
        .await
        .unwrap();

    let corrected = DownloadTitle {
        song: "Break!".into(),
        artist: "planetboom".into(),
        source: MANUAL_SOURCE,
        gemini_failed: false,
    };
    let [new_video, new_audio] = [
        dir.path().join(crate::downloader::cache::video_filename(
            "Break!",
            "planetboom",
            "DOWNLOAD008",
            false,
        )),
        dir.path().join(crate::downloader::cache::audio_filename(
            "Break!",
            "planetboom",
            "DOWNLOAD008",
            false,
        )),
    ];
    assert_eq!(
        recorded(&pool, 8).await,
        (
            corrected.song.clone(),
            corrected.artist.clone(),
            MANUAL_SOURCE.to_string(),
            0,
            1,
            new_video.to_string_lossy().into_owned(),
            new_audio.to_string_lossy().into_owned(),
        )
    );
    assert!(
        new_video.exists() && new_audio.exists(),
        "the pair was renamed"
    );
    assert!(
        !video.exists() && !audio.exists(),
        "no pair is left under the chain's name"
    );
    assert_eq!(written, corrected, "it answers the title it recorded");
}

/// Pin: with no correction the download records the title it was named
/// after, and its pair stays where the download wrote it.
#[tokio::test]
async fn a_download_with_no_correction_records_its_own_title() {
    let dir = tempfile::tempdir().unwrap();
    let pool = pool().await;
    row(&pool, 9, None, None, "gemini").await;
    let (chain, _) = chain();
    let title = title_of(&pool, &chain, 9).await;
    let [video, audio] = fresh_pair(dir.path(), 9, &title);

    let written = record_download(&pool, dir.path(), 9, "DOWNLOAD009", &title, &video, &audio)
        .await
        .unwrap();
    assert_eq!(written, title);

    assert_eq!(
        recorded(&pool, 9).await,
        (
            "Another Song".to_string(),
            "Another Artist".to_string(),
            "gemini".to_string(),
            0,
            1,
            video.to_string_lossy().into_owned(),
            audio.to_string_lossy().into_owned(),
        )
    );
    assert!(video.exists() && audio.exists());
}

/// #229 item C: while paid AI is off a download names the song by the title
/// parser, marked for the repair, and asks no provider; once it is on the
/// chain names it.
#[tokio::test]
async fn paid_ai_off_names_a_download_by_the_parser_for_the_repair() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let (chain, calls) = chain();
    let chain = chain.gated(pool.clone());
    crate::db::models::set_setting(&pool, "paid_ai_enabled", "false")
        .await
        .unwrap();
    let off = download_title(&pool, &chain, "pa1d0ff0001", "Sinach - Way Maker").await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(off.source, MetadataSource::Regex.as_str());
    assert!(off.gemini_failed, "marked for the repair");
    assert!(!off.song.is_empty());
    crate::db::models::set_setting(&pool, "paid_ai_enabled", "true")
        .await
        .unwrap();
    let on = download_title(&pool, &chain, "pa1d0ff0001", "Sinach - Way Maker").await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        (on.song.as_str(), on.gemini_failed),
        ("Another Song", false)
    );
}
