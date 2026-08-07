//! First-boot self-healing routines: cache reconciliation + legacy
//! playlist sync parity with the original Python implementation.
//!
//! On first boot of a new version, [`self_heal_cache`] walks the cache
//! directory, deletes any legacy single-file `.mp4` left over from the
//! pre-FLAC pipeline, deletes any orphan half-sidecars from a crashed
//! mid-download, and re-links any complete video+audio pairs back to the
//! DB row that owns them.
//!
//! [`startup_sync_active_playlists`] replicates the behavior of the
//! legacy Python `tools.py::trigger_startup_sync` that was missed in the
//! initial Rust rewrite: after tools setup completes, fire a one-shot
//! [`SyncRequest`] for every `is_active = 1` playlist so the download
//! worker has fresh video IDs to process.

use std::path::Path;

use sqlx::{Row, SqlitePool};

use crate::SyncRequest;
use crate::downloader::cache;

/// Ensures the single pre-created `ytlive` custom playlist exists.
/// Idempotent: a no-op when the row is already present.
///
/// Previously this row was seeded inside migration V13, but that caused
/// pre-existing tests (which call `run_migrations` on an empty pool
/// and count rows or hard-code playlist_id=1) to regress. Seeding in
/// startup keeps migrations pure while guaranteeing the row exists
/// whenever the server is actually running.
pub async fn ensure_live_playlist_exists(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO playlists
            (name, youtube_url, ndi_output_name, playback_mode, is_active, kind)
         SELECT 'ytlive', '', 'SP-live', 'continuous', 1, 'custom'
         WHERE NOT EXISTS (SELECT 1 FROM playlists WHERE name = 'ytlive')",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Walk the cache directory, categorise every file, and:
///
/// * delete legacy single-file `.mp4`s (from before the FLAC migration),
/// * delete orphan half-sidecars (debris from a crashed download),
/// * re-link complete video+audio pairs to their DB row.
#[cfg_attr(test, mutants::skip)]
pub async fn self_heal_cache(pool: &SqlitePool, cache_dir: &Path) -> Result<(), sqlx::Error> {
    let scan = cache::scan_cache(cache_dir);
    tracing::info!(
        songs = scan.songs.len(),
        legacy = scan.legacy.len(),
        orphans = scan.orphans.len(),
        lyrics = scan.lyrics_files.len(),
        "self-heal cache scan"
    );

    // Delete legacy AAC single-file .mp4s — unusable under the new pipeline.
    cache::cleanup_legacy(&scan.legacy);

    // Delete orphan half-sidecars (mid-download crash debris).
    for orphan in &scan.orphans {
        tracing::info!(
            "removing orphan sidecar for {}: {}",
            orphan.video_id,
            orphan.path.display()
        );
        if let Err(e) = std::fs::remove_file(&orphan.path) {
            tracing::warn!("failed to remove orphan {}: {e}", orphan.path.display());
        }
    }

    // Re-link complete pairs back to their DB row.
    for song in &scan.songs {
        let v = song.video_path.to_string_lossy().to_string();
        let a = song.audio_path.to_string_lossy().to_string();
        sqlx::query(
            "UPDATE videos SET file_path = ?, audio_file_path = ?, normalized = 1
             WHERE youtube_id = ?",
        )
        .bind(&v)
        .bind(&a)
        .bind(&song.video_id)
        .execute(pool)
        .await?;
    }

    // Detect DB/disk mismatch: rows marked has_lyrics=1 but JSON file is gone.
    // This was originally a wholesale delete-all-lyrics-and-reset loop from
    // PR #24's migration, but that caused Gemini quota burn on every restart
    // (N songs × 1 Gemini translation call per restart). Now we only reset
    // rows where the file is genuinely missing — a truly idempotent self-heal.
    let claimed_rows = sqlx::query("SELECT youtube_id FROM videos WHERE has_lyrics = 1")
        .fetch_all(pool)
        .await?;
    let mut orphan_resets = 0usize;
    for row in claimed_rows {
        let youtube_id: String = row.get("youtube_id");
        let json_path = cache_dir.join(format!("{youtube_id}_lyrics.json"));
        if !json_path.exists() {
            sqlx::query(
                "UPDATE videos SET has_lyrics = 0, lyrics_source = NULL WHERE youtube_id = ?",
            )
            .bind(&youtube_id)
            .execute(pool)
            .await?;
            orphan_resets += 1;
        }
    }
    if orphan_resets > 0 {
        tracing::info!("reset {orphan_resets} DB rows claiming has_lyrics=1 but missing JSON file");
    }

    Ok(())
}

/// Re-run the emoji sanitizer (`metadata::sanitize::strip_emoji`) over every
/// stored `song` / `artist` value and UPDATE only the rows whose sanitized
/// text differs from what is stored (#135).
///
/// `metadata::get_metadata` sanitizes every NEW write, but a row written
/// before that choke point existed stays dirty forever unless something
/// re-visits it — and a row whose provider(s) keep failing (`gemini_failed
/// = 1` with no working provider) can NEVER be healed by
/// `ReprocessWorker`'s provider retry, since that path never falls back to
/// the regex parser. This pass fixes the DB directly, independent of
/// `gemini_failed` state, so it also cleans a `gemini_failed = 0` row that
/// simply has a dirty stored value from before the fix.
///
/// Idempotent: a row whose sanitized song/artist already matches the
/// stored value is left untouched (and NULL stays NULL — an absent value
/// is not "dirty"). Returns the number of rows healed.
pub async fn self_heal_emoji_metadata(pool: &SqlitePool) -> Result<usize, sqlx::Error> {
    let rows = sqlx::query("SELECT id, song, artist FROM videos")
        .fetch_all(pool)
        .await?;

    let mut healed = 0usize;
    for row in rows {
        let id: i64 = row.get("id");
        let song: Option<String> = row.get("song");
        let artist: Option<String> = row.get("artist");

        let clean_song = song.as_deref().map(crate::metadata::sanitize::strip_emoji);
        let clean_artist = artist
            .as_deref()
            .map(crate::metadata::sanitize::strip_emoji);

        if clean_song == song && clean_artist == artist {
            continue;
        }

        sqlx::query("UPDATE videos SET song = ?, artist = ? WHERE id = ?")
            .bind(clean_song)
            .bind(clean_artist)
            .bind(id)
            .execute(pool)
            .await?;
        healed += 1;
    }

    tracing::info!(
        healed,
        "self-heal: sanitized emoji from stored song/artist metadata"
    );

    Ok(healed)
}

/// Repair stored rows whose `song` was written empty/whitespace-only, or
/// left NULL despite processing having finished — re-derive song+artist
/// from the stored `title` via the regex title parser
/// (`metadata::parser::parse_title`).
///
/// Two distinct broken shapes are repaired:
///
/// * `song` is an empty/whitespace string (#136: five ytalex rows
///   shipped `song=""`, `artist=""`, `gemini_failed=false`).
/// * `song IS NULL` **and** `normalized = 1` — the download/metadata
///   pipeline ran to completion but never wrote a song value at all
///   (observed live post-#136: five catalog rows stuck with
///   `song=NULL`, `artist=NULL`, `normalized=1`, `gemini_failed=false`,
///   which the E2E gate's `gemini_failed=false but empty song` check
///   correctly flags).
///
/// A `song IS NULL` row with `normalized = 0` is deliberately NOT
/// touched — that row simply hasn't been processed yet, it is not
/// broken, and repairing it would race the download worker that is
/// about to write real metadata for it.
///
/// `metadata::get_metadata`'s provider-success path now re-checks for an
/// empty song after sanitization and falls back to the title parser
/// itself, and `db::models::mark_video_processed_pair` refuses to WRITE
/// an empty song — but a row already stored before those guards existed
/// stays broken forever unless something re-visits it. This pass fixes
/// the DB directly.
///
/// Every repaired row is stamped `gemini_failed = 1` — mirrors the
/// `get_metadata` fallback contract: a repaired row always means the
/// title parser produced the value, never a real provider result. If
/// the title parser ALSO yields an empty song for a row (e.g. an
/// empty/NULL title), the row is left untouched and logged at `warn!`
/// — this pass never writes an empty value, same invariant as the
/// write choke point.
///
/// Idempotent: a row whose `song` is already non-empty, or whose `song`
/// is NULL with `normalized = 0` (not yet processed, not "dirty"), is
/// left alone. Returns the number of rows healed.
pub async fn self_heal_empty_song_metadata(pool: &SqlitePool) -> Result<usize, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, youtube_id, title FROM videos
         WHERE (song IS NOT NULL AND TRIM(song) = '')
            OR (song IS NULL AND normalized = 1)",
    )
    .fetch_all(pool)
    .await?;

    let mut healed = 0usize;
    for row in rows {
        let id: i64 = row.get("id");
        let youtube_id: String = row.get("youtube_id");
        let title: Option<String> = row.get("title");
        let title = title.unwrap_or_default();

        let parsed = crate::metadata::parser::parse_title(&title);
        if parsed.song.trim().is_empty() {
            tracing::warn!(
                video_id = id,
                youtube_id = %youtube_id,
                title = %title,
                "self-heal: title parser also produced an empty song; leaving row untouched"
            );
            continue;
        }

        sqlx::query("UPDATE videos SET song = ?, artist = ?, gemini_failed = 1 WHERE id = ?")
            .bind(&parsed.song)
            .bind(&parsed.artist)
            .bind(id)
            .execute(pool)
            .await?;
        healed += 1;
    }

    tracing::info!(healed, "self-heal: repaired stored rows with empty song");
    Ok(healed)
}

/// Probe sample rates of every `normalized = 1` row's `audio_file_path`
/// and flip any row whose audio is not at 48 kHz back to `normalized = 0`
/// so the download worker re-normalizes it.
///
/// Background: pre-PR-#38 cache files were sometimes 192 kHz (yt-dlp
/// produced FLAC at the source sample rate). `sp_decoder::SplitSyncedDecoder`
/// requires 48 kHz and rejects anything else with a hard error at
/// playback time — the operator sees the wall stay dark on what looks
/// like a fresh-cached song. This self-heal flips the DB row back to
/// `normalized = 0` so the download worker re-processes it under the
/// post-#38 pipeline that always passes `-ar 48000 -ac 2` to ffmpeg.
///
/// `probe` is injected so unit tests can drive arbitrary sample rates
/// without spawning ffprobe / opening real FLAC files. Production
/// callers pass [`probe_sample_rate_symphonia`].
///
/// Returns the number of rows flipped.
pub async fn flip_wrong_sample_rate_rows<F>(
    pool: &SqlitePool,
    probe: F,
) -> Result<usize, sqlx::Error>
where
    F: Fn(&Path) -> Option<u32>,
{
    let rows = sqlx::query(
        "SELECT id, youtube_id, audio_file_path FROM videos
         WHERE normalized = 1 AND audio_file_path IS NOT NULL AND audio_file_path <> ''",
    )
    .fetch_all(pool)
    .await?;
    let mut flipped = 0usize;
    for row in rows {
        let id: i64 = row.get("id");
        let yt: String = row.get("youtube_id");
        let path_str: String = row.get("audio_file_path");
        let path = Path::new(&path_str);
        match probe(path) {
            Some(48_000) => {}
            Some(other) => {
                tracing::warn!(
                    video_id = id,
                    youtube_id = %yt,
                    sample_rate = other,
                    path = %path.display(),
                    "self-heal: audio not at 48 kHz; flipping normalized=0 for re-normalize"
                );
                sqlx::query("UPDATE videos SET normalized = 0 WHERE id = ?")
                    .bind(id)
                    .execute(pool)
                    .await?;
                flipped += 1;
            }
            None => {
                tracing::warn!(
                    video_id = id,
                    youtube_id = %yt,
                    path = %path.display(),
                    "self-heal: sample-rate probe failed; leaving normalized state unchanged"
                );
            }
        }
    }
    tracing::info!(flipped, "self-heal: sample-rate sweep complete");
    Ok(flipped)
}

/// Production sample-rate probe backed by Symphonia's FLAC reader.
/// Returns `None` if the file is missing, unreadable, or not a
/// recognised audio format — caller treats `None` as "leave row alone".
///
/// `cfg_attr(test, mutants::skip)`: the function is exercised by
/// `flip_wrong_sample_rate_rows`'s injectable probe in unit tests;
/// covering it directly would need a real FLAC fixture per branch and
/// mutation testing would add no signal — it's a thin Symphonia
/// wrapper.
#[cfg_attr(test, mutants::skip)]
pub fn probe_sample_rate_symphonia(audio_path: &Path) -> Option<u32> {
    match sp_decoder::SymphoniaAudioReader::open(audio_path) {
        Ok(reader) => {
            use sp_decoder::AudioStream;
            Some(reader.sample_rate())
        }
        Err(e) => {
            tracing::debug!(
                path = %audio_path.display(),
                "probe_sample_rate_symphonia: open failed: {e}"
            );
            None
        }
    }
}

/// Trigger a one-time playlist sync for every active playlist at startup.
/// Legacy Python parity with `tools.py::trigger_startup_sync`.
pub async fn startup_sync_active_playlists(
    pool: &SqlitePool,
    sync_tx: &tokio::sync::mpsc::Sender<SyncRequest>,
) -> Result<(), sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, youtube_url FROM playlists WHERE is_active = 1 AND kind = 'youtube'",
    )
    .fetch_all(pool)
    .await?;
    tracing::info!(
        count = rows.len(),
        "startup sync: enqueueing one SyncRequest per active playlist"
    );
    for row in rows {
        let playlist_id: i64 = row.get("id");
        let youtube_url: String = row.get("youtube_url");
        if let Err(e) = sync_tx
            .send(SyncRequest {
                playlist_id,
                youtube_url,
            })
            .await
        {
            tracing::warn!(playlist_id, "startup sync enqueue failed: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod empty_song_self_heal_tests {
    use super::*;
    use crate::db;

    async fn seed_pool() -> SqlitePool {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name)
             VALUES (1, 'p', 'u', 'n')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    /// RED: mirrors the live #136 production defect — a stored row whose
    /// `song` was written empty (`gemini_failed = 0`, so the provider
    /// retry worker never revisits it) must be repaired from its stored
    /// `title` via the regex parser, and flagged `gemini_failed = 1` so
    /// it is visibly not a real provider result.
    #[tokio::test]
    async fn heals_dirty_row_with_empty_song_from_title() {
        let pool = seed_pool().await;
        sqlx::query(
            "INSERT INTO videos
                (playlist_id, youtube_id, title, song, artist, gemini_failed, normalized)
             VALUES (1, 'q_T_-Lh8AFI', 'Elevation Worship - The Blessing', '', '', 0, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let healed = self_heal_empty_song_metadata(&pool).await.unwrap();
        assert_eq!(healed, 1, "exactly the dirty row should be healed");

        let row = sqlx::query(
            "SELECT song, artist, gemini_failed FROM videos WHERE youtube_id = 'q_T_-Lh8AFI'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("song"), "The Blessing");
        assert_eq!(row.get::<String, _>("artist"), "Elevation Worship");
        assert_eq!(
            row.get::<i64, _>("gemini_failed"),
            1,
            "a repaired row must be flagged gemini_failed so it reads as parser-derived"
        );
    }

    /// A whitespace-only stored song is just as dirty as `""`.
    #[tokio::test]
    async fn heals_whitespace_only_song() {
        let pool = seed_pool().await;
        sqlx::query(
            "INSERT INTO videos
                (playlist_id, youtube_id, title, song, artist, gemini_failed, normalized)
             VALUES (1, 'whitespace1', 'Pat Barrett - Count On You', '   ', NULL, 0, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let healed = self_heal_empty_song_metadata(&pool).await.unwrap();
        assert_eq!(healed, 1);

        let row = sqlx::query("SELECT song FROM videos WHERE youtube_id = 'whitespace1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>("song"), "Count On You");
    }

    /// When the title parser ALSO produces an empty song (e.g. an empty
    /// or NULL title), the row must be left completely untouched — never
    /// write an empty value, per the same invariant as the write choke
    /// point in `db::models::mark_video_processed_pair`.
    #[tokio::test]
    async fn leaves_row_untouched_when_parser_also_yields_empty_song() {
        let pool = seed_pool().await;
        sqlx::query(
            "INSERT INTO videos
                (playlist_id, youtube_id, title, song, artist, gemini_failed, normalized)
             VALUES (1, 'notitle', NULL, '', '', 0, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let healed = self_heal_empty_song_metadata(&pool).await.unwrap();
        assert_eq!(
            healed, 0,
            "a row the parser also can't derive a song for must not count as healed"
        );

        let row = sqlx::query("SELECT song FROM videos WHERE youtube_id = 'notitle'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let song: String = row.get("song");
        assert_eq!(song, "", "row must stay untouched, never coerced further");
    }

    /// A row that already has a real song must never be touched.
    #[tokio::test]
    async fn skips_already_populated_rows() {
        let pool = seed_pool().await;
        sqlx::query(
            "INSERT INTO videos
                (playlist_id, youtube_id, title, song, artist, gemini_failed, normalized)
             VALUES (1, 'clean1', 'irrelevant', 'The Blessing', 'Elevation Worship', 0, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let healed = self_heal_empty_song_metadata(&pool).await.unwrap();
        assert_eq!(healed, 0, "no row should be touched when already populated");

        let row = sqlx::query("SELECT song, artist FROM videos WHERE youtube_id = 'clean1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>("song"), "The Blessing");
        assert_eq!(row.get::<String, _>("artist"), "Elevation Worship");
    }

    /// A row with NULL song (not yet processed) is not "dirty" — it must
    /// never be touched by this pass, which only repairs rows that were
    /// actually written empty.
    #[tokio::test]
    async fn skips_null_song_rows() {
        let pool = seed_pool().await;
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, gemini_failed, normalized)
             VALUES (1, 'unprocessed', 'Some Title', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let healed = self_heal_empty_song_metadata(&pool).await.unwrap();
        assert_eq!(healed, 0, "a NULL song row is unprocessed, not dirty");

        let row = sqlx::query("SELECT song FROM videos WHERE youtube_id = 'unprocessed'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let song: Option<String> = row.get("song");
        assert_eq!(song, None, "song must stay NULL, never coerced to \"\"");
    }

    /// RED: mirrors the live post-deploy E2E production defect — a stored
    /// row whose `song` is NULL but `normalized = 1` (processing finished
    /// and produced nothing) must be repaired from its stored `title` via
    /// the regex title parser, exactly like the empty-string case. This
    /// is distinct from `skips_null_song_rows_when_unprocessed`: a NULL
    /// song with `normalized = 0` merely hasn't been processed yet, but
    /// NULL + `normalized = 1` means the pipeline ran and left the row
    /// broken — five real catalog rows (e.g. `q_T_-Lh8AFI`, "WELCOME
    /// HOME | ELEVATION RHYTHM & SEU Worship") were observed live in
    /// exactly this state.
    #[tokio::test]
    async fn heals_null_song_row_when_already_normalized() {
        let pool = seed_pool().await;
        sqlx::query(
            "INSERT INTO videos
                (playlist_id, youtube_id, title, gemini_failed, normalized)
             VALUES (1, 'q_T_-Lh8AFI',
                     'WELCOME HOME | ELEVATION RHYTHM & SEU Worship', 0, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let healed = self_heal_empty_song_metadata(&pool).await.unwrap();
        assert_eq!(
            healed, 1,
            "a NULL song row that already finished processing (normalized=1) must be repaired"
        );

        let row = sqlx::query(
            "SELECT song, artist, gemini_failed FROM videos WHERE youtube_id = 'q_T_-Lh8AFI'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("song"), "WELCOME HOME");
        assert_eq!(
            row.get::<String, _>("artist"),
            "ELEVATION RHYTHM & SEU Worship"
        );
        assert_eq!(
            row.get::<i64, _>("gemini_failed"),
            1,
            "a repaired row must be flagged gemini_failed so it reads as parser-derived"
        );
    }

    /// Idempotent: a second run over an already-repaired catalog heals
    /// nothing.
    #[tokio::test]
    async fn is_idempotent() {
        let pool = seed_pool().await;
        sqlx::query(
            "INSERT INTO videos
                (playlist_id, youtube_id, title, song, artist, gemini_failed, normalized)
             VALUES (1, 'dirty1', 'Elevation Worship - The Blessing', '', '', 0, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let first = self_heal_empty_song_metadata(&pool).await.unwrap();
        assert_eq!(first, 1);
        let second = self_heal_empty_song_metadata(&pool).await.unwrap();
        assert_eq!(second, 0, "second run must find nothing left to repair");
    }
}

#[cfg(test)]
mod sample_rate_self_heal_tests {
    use super::*;
    use crate::db;

    async fn seed_pool() -> SqlitePool {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        // Videos.playlist_id has a FK; insert a real playlist first.
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name)
             VALUES (1, 'p', 'u', 'n')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn seed_normalized_row(
        pool: &SqlitePool,
        youtube_id: &str,
        audio_path: Option<&str>,
        normalized: i64,
    ) -> i64 {
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, normalized, audio_file_path)
             VALUES (1, ?, 't', ?, ?)",
        )
        .bind(youtube_id)
        .bind(normalized)
        .bind(audio_path)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query_scalar::<_, i64>("SELECT id FROM videos WHERE youtube_id = ?")
            .bind(youtube_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn read_normalized(pool: &SqlitePool, id: i64) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT normalized FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn flips_192k_row_to_unnormalized_and_leaves_48k_alone() {
        let pool = seed_pool().await;

        let id_48k = seed_normalized_row(&pool, "y48k", Some("/cache/48k.flac"), 1).await;
        let id_192k = seed_normalized_row(&pool, "y192k", Some("/cache/192k.flac"), 1).await;

        let probe = |path: &Path| -> Option<u32> {
            match path.to_str() {
                Some("/cache/48k.flac") => Some(48_000),
                Some("/cache/192k.flac") => Some(192_000),
                _ => None,
            }
        };
        let flipped = flip_wrong_sample_rate_rows(&pool, probe).await.unwrap();
        assert_eq!(flipped, 1, "exactly the 192k row should be flipped");

        assert_eq!(read_normalized(&pool, id_48k).await, 1);
        assert_eq!(read_normalized(&pool, id_192k).await, 0);
    }

    #[tokio::test]
    async fn skips_rows_with_no_audio_path() {
        let pool = seed_pool().await;

        let id_none = seed_normalized_row(&pool, "noaudio", None, 1).await;
        // Probe should never run; if it does and returns 192k the row
        // would flip — assert it stayed at 1.
        let flipped = flip_wrong_sample_rate_rows(&pool, |_| Some(192_000))
            .await
            .unwrap();
        assert_eq!(flipped, 0);
        assert_eq!(read_normalized(&pool, id_none).await, 1);
    }

    #[tokio::test]
    async fn ignores_unnormalized_rows_even_at_wrong_sample_rate() {
        let pool = seed_pool().await;

        let id = seed_normalized_row(&pool, "raw", Some("/cache/raw.flac"), 0).await;
        let flipped = flip_wrong_sample_rate_rows(&pool, |_| Some(192_000))
            .await
            .unwrap();
        assert_eq!(flipped, 0);
        assert_eq!(read_normalized(&pool, id).await, 0);
    }

    #[tokio::test]
    async fn probe_returning_none_leaves_row_alone() {
        let pool = seed_pool().await;

        let id = seed_normalized_row(&pool, "ymissing", Some("/cache/gone.flac"), 1).await;
        // None means the probe failed (file gone, ffprobe missing, etc.).
        // Don't touch the row — we'd lose state on a transient I/O error.
        let flipped = flip_wrong_sample_rate_rows(&pool, |_| None).await.unwrap();
        assert_eq!(flipped, 0);
        assert_eq!(read_normalized(&pool, id).await, 1);
    }
}

#[cfg(test)]
mod sync_filter_tests {
    use super::*;
    use crate::db;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn startup_sync_skips_custom_playlists() {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        // Seed the ytlive custom playlist (normally done by ensure_live_playlist_exists
        // at server startup, not by migrations).
        ensure_live_playlist_exists(&pool).await.unwrap();

        // Insert one youtube playlist alongside the custom ytlive one.
        db::models::insert_playlist(&pool, "ytfast", "https://yt.com/fast")
            .await
            .unwrap();

        let (tx, mut rx) = mpsc::channel::<SyncRequest>(8);
        startup_sync_active_playlists(&pool, &tx).await.unwrap();
        drop(tx);

        let mut received_urls = Vec::new();
        while let Some(req) = rx.recv().await {
            received_urls.push(req.youtube_url);
        }

        assert_eq!(
            received_urls.len(),
            1,
            "only youtube playlists should be synced"
        );
        assert_eq!(received_urls[0], "https://yt.com/fast");
    }

    #[tokio::test]
    async fn ensure_live_playlist_is_idempotent_and_creates_ytlive() {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();

        // Before: no ytlive row.
        let before: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM playlists WHERE name = 'ytlive'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(before, 0);

        // First call inserts.
        ensure_live_playlist_exists(&pool).await.unwrap();
        let after_first: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM playlists WHERE name = 'ytlive'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(after_first, 1);

        // Second call is a no-op (idempotent).
        ensure_live_playlist_exists(&pool).await.unwrap();
        let after_second: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM playlists WHERE name = 'ytlive'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(after_second, 1);

        // Row has the expected kind + ndi_output_name.
        let row = sqlx::query(
            "SELECT kind, ndi_output_name, playback_mode FROM playlists WHERE name = 'ytlive'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        use sqlx::Row;
        assert_eq!(row.get::<String, _>("kind"), "custom");
        assert_eq!(row.get::<String, _>("ndi_output_name"), "SP-live");
        assert_eq!(row.get::<String, _>("playback_mode"), "continuous");
    }
}

#[cfg(test)]
mod emoji_self_heal_tests {
    use super::*;
    use crate::db;

    async fn seed_pool() -> SqlitePool {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name)
             VALUES (1, 'p', 'u', 'n')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    /// RED: mirrors the live #135 E2E failure — a stored row whose
    /// `gemini_failed = 1` (every provider keeps failing on this video, so
    /// it can never be healed by a provider retry) carries an emoji in
    /// `artist` written before the sanitizer choke point existed. The
    /// startup self-heal pass must clean it in place.
    #[tokio::test]
    async fn heals_dirty_stored_row_with_emoji() {
        let pool = seed_pool().await;
        sqlx::query(
            "INSERT INTO videos
                (playlist_id, youtube_id, title, song, artist, gemini_failed, normalized)
             VALUES (1, '0HQOYVf6-Yg', 't', 'Our God + The Blessing',
                     'Christian Afro House 2025 \u{1F525}', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let healed = self_heal_emoji_metadata(&pool).await.unwrap();
        assert_eq!(healed, 1, "exactly the dirty row should be healed");

        let row = sqlx::query("SELECT song, artist FROM videos WHERE youtube_id = '0HQOYVf6-Yg'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let song: String = row.get("song");
        let artist: String = row.get("artist");
        assert_eq!(song, "Our God + The Blessing");
        assert_eq!(
            artist, "Christian Afro House 2025",
            "self-heal must strip emoji from the stored artist"
        );
    }

    /// A row already clean (no emoji, `gemini_failed = 0`) must be left
    /// untouched — proves the pass doesn't rewrite every row on every boot.
    #[tokio::test]
    async fn skips_already_clean_rows() {
        let pool = seed_pool().await;
        sqlx::query(
            "INSERT INTO videos
                (playlist_id, youtube_id, title, song, artist, gemini_failed, normalized)
             VALUES (1, 'clean1', 't', 'The Blessing', 'Elevation Worship', 0, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let healed = self_heal_emoji_metadata(&pool).await.unwrap();
        assert_eq!(healed, 0, "no row should be touched when already clean");

        let row = sqlx::query("SELECT song, artist FROM videos WHERE youtube_id = 'clean1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>("song"), "The Blessing");
        assert_eq!(row.get::<String, _>("artist"), "Elevation Worship");
    }

    /// A row with NULL song/artist (never processed yet) must stay NULL —
    /// the sanitizer must never turn an absent value into `""`.
    #[tokio::test]
    async fn preserves_null_song_and_artist() {
        let pool = seed_pool().await;
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, gemini_failed, normalized)
             VALUES (1, 'unprocessed', 't', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let healed = self_heal_emoji_metadata(&pool).await.unwrap();
        assert_eq!(healed, 0, "a NULL song/artist row is not dirty");

        let row = sqlx::query("SELECT song, artist FROM videos WHERE youtube_id = 'unprocessed'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let song: Option<String> = row.get("song");
        let artist: Option<String> = row.get("artist");
        assert_eq!(song, None, "song must stay NULL, never coerced to \"\"");
        assert_eq!(artist, None, "artist must stay NULL, never coerced to \"\"");
    }
}
