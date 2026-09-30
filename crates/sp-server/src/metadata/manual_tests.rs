//! #136 (ROZHODNUTÉ 5908227964 item 2): a re-download keeps an operator's
//! title correction and never asks the provider chain over it. The title
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
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
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

async fn title_of(pool: &SqlitePool, chain: &ProviderChain, id: i64) -> DownloadTitle {
    let youtube_id = format!("DOWNLOAD{id:03}");
    download_title(pool, chain, id, &youtube_id, "Break! - planetboom (Live)").await
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
        "no row"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

/// `DownloadWorker::process_next` downloads with yt-dlp, so no Linux test
/// drives it; its title step is pinned by its source: it takes the row's
/// title from `download_title` (never the chain directly) and records that
/// title's own `source`.
#[test]
fn the_download_worker_takes_the_title_from_download_title() {
    let src = include_str!("../downloader/mod.rs");
    assert!(
        src.contains(
            "download_title(&self.pool, &self.metadata, row.id, &row.youtube_id, &row.title)"
        ),
        "process_next asks download_title for the row's title"
    );
    assert!(
        !src.contains("crate::metadata::get_metadata("),
        "the chain is never asked directly"
    );
    assert!(
        src.contains("meta.source,"),
        "the title's own source is recorded"
    );
}
