//! #229: what this node has, from its own rows. The catalog and the hasher
//! read the same list ([`artifact_files`]); the catalog lists a file only once
//! the hasher holds its sha256, so building it reads no file.
//!
//! - A video's pair and stems: the representative row = the lowest id of the
//!   YouTube id with an audio (rows of one video share files, #136). Stems
//!   only when a row of the video with that audio has `stem_status =
//!   'done'`, named after the CURRENT audio (`stems::stem_paths`,
//!   `.claude/rules/song-files.md`).
//! - Lyrics: `{yt}_lyrics.json` at the highest pipeline version of the
//!   video's rows with lyrics, never when ANY row of the video is
//!   dub-requested or carries the Live-Translate track (that file is then the
//!   dub's subtitles, Review Focus 5).
//! - Metadata: the representative row with a song; its bytes are
//!   `PeerMetadata::to_bytes`.
//! - Jobs: every running job on the board and every queued one
//!   (`peer::queued`), one entry per kind ([`listed_jobs`]); queued too, the
//!   job of a file on disk the hasher has not reached yet ([`unhashed`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::Exchange;
use super::hash::sha256_hex;
use super::kind::{ArtifactKind, Job, MEDIA_VERSION, STEMS_VERSION};
use super::wire::{
    Artifact, Catalog, CatalogJob, JobState, PeerLyrics, PeerMetadata, PeerVideo, ms_to_rfc3339,
};
use crate::db::models_peer::{self, HashEntry};
use crate::downloader::cache::is_valid_video_id;

/// One file artifact this node's rows name (not checked on disk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactFile {
    pub youtube_id: String,
    pub kind: ArtifactKind,
    pub version: u32,
    pub path: PathBuf,
}

/// For the status: the files the rows name, how many of them the catalog
/// lists (hashed), and the queued job entries it lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogCounts {
    pub files: usize,
    pub listed: usize,
    pub queued: usize,
}

/// `?1` = an optional YouTube id filter. The 4th column: a row of the video
/// with the same audio has its stems done (rows share files, #136).
const MEDIA_ROWS: &str = "SELECT youtube_id, COALESCE(file_path, ''), audio_file_path, \
         EXISTS (SELECT 1 FROM videos s WHERE s.youtube_id = videos.youtube_id \
             AND s.audio_file_path = videos.audio_file_path AND s.stem_status = 'done') \
     FROM videos WHERE id IN (SELECT MIN(id) FROM videos WHERE normalized = 1 \
         AND audio_file_path IS NOT NULL AND audio_file_path != '' \
         AND (?1 IS NULL OR youtube_id = ?1) GROUP BY youtube_id) \
     ORDER BY youtube_id";

/// `?1` = an optional YouTube id filter, `?2` = the Live-Translate source.
/// A video is out when ANY of its rows is dub-requested or Live-Translate,
/// lyrics or not: the `{yt}_lyrics.json` is one file per video.
const LYRICS_ROWS: &str = "SELECT youtube_id, \
         MAX(CASE WHEN has_lyrics = 1 THEN lyrics_pipeline_version ELSE 0 END) \
     FROM videos WHERE (?1 IS NULL OR youtube_id = ?1) GROUP BY youtube_id \
     HAVING MAX(has_lyrics) = 1 \
         AND SUM(CASE WHEN dub_requested = 1 OR lyrics_source = ?2 THEN 1 ELSE 0 END) = 0 \
     ORDER BY youtube_id";

/// `?1` = an optional YouTube id filter.
const METADATA_ROWS: &str = "SELECT youtube_id, song, COALESCE(artist, ''), metadata_source, gemini_failed \
     FROM videos WHERE id IN (SELECT MIN(id) FROM videos WHERE TRIM(COALESCE(song, '')) != '' \
         AND (?1 IS NULL OR youtube_id = ?1) GROUP BY youtube_id) \
     ORDER BY youtube_id";

type MediaRow = (String, String, String, bool);
type MetadataRow = (String, String, String, Option<String>, i64);

/// The file artifacts this node's rows name (all, or one video's).
pub async fn artifact_files(
    pool: &SqlitePool,
    cache_dir: &Path,
    youtube_id: Option<&str>,
) -> Result<Vec<ArtifactFile>, sqlx::Error> {
    let mut files = Vec::new();
    let media: Vec<MediaRow> = sqlx::query_as(MEDIA_ROWS)
        .bind(youtube_id)
        .fetch_all(pool)
        .await?;
    for (yt, video, audio, stems_done) in media {
        if !is_valid_video_id(&yt) {
            continue;
        }
        let audio = PathBuf::from(audio);
        if !video.is_empty() {
            files.push(file(
                &yt,
                ArtifactKind::Video,
                MEDIA_VERSION,
                PathBuf::from(video),
            ));
        }
        if stems_done {
            let (vocals, instrumental) = crate::stems::stem_paths(&audio);
            files.push(file(&yt, ArtifactKind::StemVocals, STEMS_VERSION, vocals));
            files.push(file(
                &yt,
                ArtifactKind::StemInstrumental,
                STEMS_VERSION,
                instrumental,
            ));
        }
        files.push(file(&yt, ArtifactKind::Audio, MEDIA_VERSION, audio));
    }
    let lyrics: Vec<(String, i64)> = sqlx::query_as(LYRICS_ROWS)
        .bind(youtube_id)
        .bind(crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE)
        .fetch_all(pool)
        .await?;
    for (yt, version) in lyrics {
        if !is_valid_video_id(&yt) {
            continue;
        }
        let path = cache_dir.join(format!("{yt}_lyrics.json"));
        let version = u32::try_from(version).unwrap_or(0);
        files.push(file(&yt, ArtifactKind::Lyrics, version, path));
    }
    Ok(files)
}

fn file(youtube_id: &str, kind: ArtifactKind, version: u32, path: PathBuf) -> ArtifactFile {
    ArtifactFile {
        youtube_id: youtube_id.to_string(),
        kind,
        version,
        path,
    }
}

/// The titles this node holds (all videos, or one).
pub async fn metadata_for(
    pool: &SqlitePool,
    youtube_id: Option<&str>,
) -> Result<Vec<PeerMetadata>, sqlx::Error> {
    let rows: Vec<MetadataRow> = sqlx::query_as(METADATA_ROWS)
        .bind(youtube_id)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .filter(|row| is_valid_video_id(&row.0))
        .map(
            |(youtube_id, song, artist, metadata_source, gemini_failed)| PeerMetadata {
                youtube_id,
                song,
                artist,
                metadata_source,
                gemini_failed: gemini_failed != 0,
            },
        )
        .collect())
}

/// The jobs a catalog of `node` lists: every `running` entry (the board's)
/// and one `queued` entry per kind each queued job makes. An `(id, kind)`
/// already listed is never listed again: a job both queued and running (a
/// download in progress is still a `normalized = 0` row) is listed once, as
/// running. Sorted by YouTube id, then by the kind's wire name.
pub fn listed_jobs(
    running: Vec<CatalogJob>,
    queued: &[(String, Job)],
    node: &str,
) -> Vec<CatalogJob> {
    let mut listed: HashSet<(String, ArtifactKind)> = running
        .iter()
        .map(|j| (j.youtube_id.clone(), j.kind))
        .collect();
    let mut jobs = running;
    for (youtube_id, job) in queued {
        for kind in job.makes() {
            if listed.insert((youtube_id.clone(), *kind)) {
                jobs.push(CatalogJob {
                    youtube_id: youtube_id.clone(),
                    kind: *kind,
                    node: node.to_string(),
                    state: JobState::Queued,
                    started_at: None,
                });
            }
        }
    }
    jobs.sort_by(|a, b| {
        (a.youtube_id.as_str(), a.kind.as_str()).cmp(&(b.youtube_id.as_str(), b.kind.as_str()))
    });
    jobs
}

/// This node's jobs as `node` lists them ([`listed_jobs`]): the board's
/// running ones, the queued ones from its rows, and, queued too, the job of
/// each of `files` not hashed yet ([`unhashed`]).
pub async fn jobs(
    ex: &Exchange,
    node: &str,
    files: &[ArtifactFile],
    hashes: &HashMap<String, HashEntry>,
) -> Result<Vec<CatalogJob>, sqlx::Error> {
    let mut queued = super::queued::queued(&ex.pool).await?;
    queued.extend(unhashed(files, hashes).await);
    Ok(listed_jobs(ex.board.snapshot(node), &queued, node))
}

/// The job of each of `files` that is on disk but not in `hashes` yet: its
/// output is this node's, listed after the hasher's next pass (every 60 s).
/// Announced as queued, a peer waits for it instead of making it itself in
/// that window (lanes 7-9, #229 finding 6036287850). A file missing from disk
/// is no job (a peer would wait the full 2 h for nothing). Only these files
/// are stat'ed, so in steady state a catalog stats nothing.
pub async fn unhashed(
    files: &[ArtifactFile],
    hashes: &HashMap<String, HashEntry>,
) -> Vec<(String, Job)> {
    let mut jobs = Vec::new();
    for f in files {
        if hashes.contains_key(&path_key(&f.path)) {
            continue;
        }
        if let Some(job) = Job::making(f.kind)
            && tokio::fs::try_exists(&f.path).await.unwrap_or(false)
        {
            jobs.push((f.youtube_id.clone(), job));
        }
    }
    jobs
}

/// This node's catalog as `node`: every hashed file (hashed after `since_ms`
/// when given), every title, every running and queued job.
pub async fn build(
    ex: &Exchange,
    node: &str,
    since_ms: Option<i64>,
) -> Result<Catalog, sqlx::Error> {
    let files = artifact_files(&ex.pool, &ex.cache_dir, None).await?;
    let hashes = models_peer::all_hashes(&ex.pool).await?;
    let mut artifacts = Vec::new();
    for f in &files {
        let Some(h) = hashes.get(&path_key(&f.path)) else {
            continue;
        };
        if since_ms.is_some_and(|since| h.hashed_at_ms <= since) {
            continue;
        }
        artifacts.push(Artifact {
            youtube_id: f.youtube_id.clone(),
            kind: f.kind,
            version: f.version,
            size: u64::try_from(h.size).unwrap_or(0),
            sha256: h.sha256.clone(),
            updated_at: Some(ms_to_rfc3339(h.hashed_at_ms)),
        });
    }
    for m in metadata_for(&ex.pool, None).await? {
        let bytes = m.to_bytes();
        artifacts.push(Artifact {
            youtube_id: m.youtube_id.clone(),
            kind: ArtifactKind::Metadata,
            version: m.version(),
            size: bytes.len() as u64,
            sha256: sha256_hex(&bytes),
            updated_at: None,
        });
    }
    Ok(Catalog {
        node: node.to_string(),
        artifacts,
        jobs: jobs(ex, node, &files, &hashes).await?,
    })
}

/// The files the rows name, how many of them are hashed (listed), and the
/// queued job entries the catalog lists.
pub async fn counts(ex: &Exchange) -> Result<CatalogCounts, sqlx::Error> {
    let files = artifact_files(&ex.pool, &ex.cache_dir, None).await?;
    let hashes = models_peer::all_hashes(&ex.pool).await?;
    let listed = files
        .iter()
        .filter(|f| hashes.contains_key(&path_key(&f.path)))
        .count();
    let queued = jobs(ex, "", &files, &hashes)
        .await?
        .iter()
        .filter(|j| j.state == JobState::Queued)
        .count();
    Ok(CatalogCounts {
        files: files.len(),
        listed,
        queued,
    })
}

/// `?1` = the YouTube id, `?2` = the Live-Translate source: the same dub rule
/// as [`LYRICS_ROWS`] (no row of the video dub-requested or Live-Translate).
const LYRICS_ROW: &str = "SELECT lyrics_source, lyrics_pipeline_version, lyrics_alignment_model, \
       lyrics_reference, lyrics_translation_version, lyrics_translation_gender \
     FROM videos WHERE youtube_id = ?1 AND has_lyrics = 1 AND lyrics_source IS NOT NULL \
       AND NOT EXISTS (SELECT 1 FROM videos d WHERE d.youtube_id = ?1 \
                       AND (d.dub_requested = 1 OR d.lyrics_source = ?2)) \
     ORDER BY lyrics_pipeline_version DESC, lyrics_processed_at DESC LIMIT 1";

type LyricsRow = (String, i64, Option<String>, i64, i64, Option<String>);

/// The lyrics row of `youtube_id` this node serves, if any.
pub async fn peer_lyrics(
    pool: &SqlitePool,
    youtube_id: &str,
) -> Result<Option<PeerLyrics>, sqlx::Error> {
    let row: Option<LyricsRow> = sqlx::query_as(LYRICS_ROW)
        .bind(youtube_id)
        .bind(crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(
        |(source, version, alignment_model, reference, translation, gender)| PeerLyrics {
            source,
            pipeline_version: u32::try_from(version).unwrap_or(0),
            alignment_model,
            reference: reference != 0,
            translation_version: u32::try_from(translation).unwrap_or(0),
            translation_gender: gender,
        },
    ))
}

/// `GET /api/v1/peer/videos/{youtube_id}`'s answer; `None` = no titled row.
pub async fn peer_video(
    pool: &SqlitePool,
    youtube_id: &str,
) -> Result<Option<PeerVideo>, sqlx::Error> {
    let Some(metadata) = metadata_for(pool, Some(youtube_id)).await?.pop() else {
        return Ok(None);
    };
    let duration: Option<i64> = sqlx::query_scalar(
        "SELECT duration_ms FROM videos WHERE youtube_id = ? AND duration_ms IS NOT NULL \
         ORDER BY id LIMIT 1",
    )
    .bind(youtube_id)
    .fetch_optional(pool)
    .await?;
    let lyrics = peer_lyrics(pool, youtube_id).await?;
    Ok(Some(PeerVideo {
        metadata,
        duration_ms: duration,
        lyrics,
    }))
}

/// A path as the hash cache keys it.
pub fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
