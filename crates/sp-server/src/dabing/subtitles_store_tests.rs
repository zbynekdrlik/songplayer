//! Tests for the dub subtitle store (#184 H5): the builder version stored with
//! the track and the startup rebuild of a track built by an older builder.

use std::path::PathBuf;

use sp_core::lyrics::LyricsTrack;

use super::*;
use crate::dabing::subtitles::DUB_SUBTITLES_BUILDER_VERSION;
use crate::db::{create_memory_pool, run_migrations};

// ── Pure: the stored version and the staleness rule ──────────────────────────

#[test]
fn a_stored_track_without_the_field_is_builder_version_1() {
    let json = br#"{"version": 22, "source": "gemini-live-translate", "lines": []}"#;
    assert_eq!(stored_builder_version(json), 1);
}

#[test]
fn the_stored_builder_version_is_read_from_the_track_json() {
    let json =
        br#"{"version": 22, "source": "x", "lines": [], "dub_subtitles_builder_version": 7}"#;
    assert_eq!(stored_builder_version(json), 7);
}

#[test]
fn an_unreadable_track_counts_as_version_1() {
    assert_eq!(stored_builder_version(b"not json"), 1);
}

#[test]
fn only_a_version_below_the_builder_is_stale() {
    assert!(is_stale(1));
    assert!(is_stale(DUB_SUBTITLES_BUILDER_VERSION - 1));
    assert!(!is_stale(DUB_SUBTITLES_BUILDER_VERSION));
    assert!(!is_stale(DUB_SUBTITLES_BUILDER_VERSION + 1));
}

// ── Startup backfill: a stale track is rebuilt, a current one is untouched ───

const TRANSCRIPTS: &str = r#"{"chunks": [{"start_ms": 0, "at_ms": 0, "tempo": 1.0,
    "en_timed": [{"t_ms": 1000, "text": "Hello."}],
    "sk_timed": [{"t_ms": 1000, "text": "Ahoj."}]}]}"#;

async fn setup() -> SqlitePool {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (7, 'Dabing', '')")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

/// A ready dub in `dir` with its saved transcripts JSON; `lyrics_source` is the
/// subtitle track's source when `has_track`, else NULL. Returns the path of its
/// `{youtube_id}_lyrics.json`.
async fn ready_dub(pool: &SqlitePool, dir: &Path, youtube_id: &str, has_track: bool) -> PathBuf {
    let audio = dir.join(format!("{youtube_id}_normalized_audio.flac"));
    std::fs::write(crate::stems::dub_transcripts_path(&audio), TRANSCRIPTS).unwrap();
    let source = has_track.then_some(subtitles::SOURCE_LIVE_TRANSLATE);
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, dub_requested, dub_status, \
         audio_file_path, lyrics_source, has_lyrics) VALUES (7, ?, 't', 1, 'ready', ?, ?, ?)",
    )
    .bind(youtube_id)
    .bind(audio.to_string_lossy().to_string())
    .bind(source)
    .bind(i64::from(has_track))
    .execute(pool)
    .await
    .unwrap();
    dir.join(format!("{youtube_id}_lyrics.json"))
}

/// A stored subtitle track whose one line says `marker`, stamped with
/// `builder_version` (`None` = a track stored before the field existed).
fn stored_track(marker: &str, builder_version: Option<u32>) -> String {
    let field = builder_version
        .map(|v| format!(r#", "dub_subtitles_builder_version": {v}"#))
        .unwrap_or_default();
    format!(
        r#"{{"version": 22, "source": "gemini-live-translate", "language_source": "en",
            "language_translation": "sk",
            "lines": [{{"start_ms": 0, "end_ms": 500, "en": "{marker}", "sk": "{marker}"}}]{field}}}"#
    )
}

fn read_track(path: &Path) -> (LyricsTrack, u32) {
    let bytes = std::fs::read(path).unwrap();
    let track: LyricsTrack = serde_json::from_slice(&bytes).unwrap();
    (track, stored_builder_version(&bytes))
}

#[tokio::test]
async fn backfill_rebuilds_a_stale_track_and_leaves_a_current_one_untouched() {
    let pool = setup().await;
    let dir = tempfile::tempdir().unwrap();

    let stale = ready_dub(&pool, dir.path(), "yt_stale", true).await;
    std::fs::write(&stale, stored_track("OLD", None)).unwrap();
    let older = ready_dub(&pool, dir.path(), "yt_older", true).await;
    std::fs::write(
        &older,
        stored_track("OLDER", Some(DUB_SUBTITLES_BUILDER_VERSION - 1)),
    )
    .unwrap();
    let current = ready_dub(&pool, dir.path(), "yt_current", true).await;
    let current_json = stored_track("KEEP", Some(DUB_SUBTITLES_BUILDER_VERSION));
    std::fs::write(&current, &current_json).unwrap();
    let missing = ready_dub(&pool, dir.path(), "yt_missing", false).await;

    backfill_missing_subtitles(&pool).await;

    // The stale tracks are rebuilt from their transcripts and stamped with the
    // current builder version; the stored JSON still reads as a LyricsTrack.
    for path in [&stale, &older, &missing] {
        let (track, version) = read_track(path);
        assert_eq!(version, DUB_SUBTITLES_BUILDER_VERSION, "{}", path.display());
        assert_eq!(track.source, subtitles::SOURCE_LIVE_TRANSLATE);
        assert_eq!(track.lines.len(), 1);
        assert_eq!(track.lines[0].sk.as_deref(), Some("Ahoj."));
        assert_eq!(track.lines[0].en, "Hello.");
    }
    // The current track is byte-for-byte untouched.
    assert_eq!(std::fs::read_to_string(&current).unwrap(), current_json);
}
