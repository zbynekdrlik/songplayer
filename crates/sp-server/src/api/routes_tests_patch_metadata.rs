//! `PATCH /api/v1/videos/{id}` song + artist correction tests (#136 T1).
//! Included via `#[path] #[cfg(test)] mod tests_patch_metadata;` from
//! routes.rs; shares `test_state`/`app` with `routes_tests.rs` via
//! `super::tests`. Kept in its own sibling file so `routes_tests.rs` stays
//! under the 1000-line file-size gate.
//!
//! Operators correct wall metadata that the Gemini-failed regex fallback
//! wrote wrong (swapped / initial-shortened song+artist, mojibake) — the
//! deterministic R4-rejected lever. The handler must sanitize through the
//! central `metadata::sanitize::strip_emoji` choke point and reject a
//! whitespace-only `song` with 400.

#![allow(unused_imports)]

use super::tests::{app, test_state};
use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

/// Seed one playlist + one video with the given song/artist; the caller
/// clones `state.pool` for post-PATCH assertions.
async fn seed_video(state: &crate::AppState, id: i64, song: &str, artist: &str) {
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&state.pool)
        .await
        .ok(); // playlist may already exist across multi-seed tests
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, song, artist) \
         VALUES (?, 1, ?, 1, ?, ?)",
    )
    .bind(id)
    .bind(format!("yt-{id}"))
    .bind(song)
    .bind(artist)
    .execute(&state.pool)
    .await
    .unwrap();
}

async fn patch(state: crate::AppState, id: i64, body: serde_json::Value) -> StatusCode {
    app(state)
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/videos/{id}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// The core lever: PATCH song + artist writes both columns and replies 204.
#[tokio::test]
async fn patch_video_sets_song_and_artist() {
    let state = test_state().await;
    // A Gemini-failed swap: artist holds the initialized band, song the wrong half.
    seed_video(&state, 10, "planetboom", "P. Break!").await;
    let pool = state.pool.clone();

    let status = patch(
        state,
        10,
        serde_json::json!({ "song": "Break!", "artist": "planetboom" }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (song, artist): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT song, artist FROM videos WHERE id = 10")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        song.as_deref(),
        Some("Break!"),
        "song column must be corrected"
    );
    assert_eq!(
        artist.as_deref(),
        Some("planetboom"),
        "artist column must be corrected"
    );
}

/// A whitespace-only `song` is a clear operator error — reject with 400 and
/// leave the stored song untouched.
#[tokio::test]
async fn patch_video_rejects_whitespace_only_song() {
    let state = test_state().await;
    seed_video(&state, 11, "Real Song", "Real Artist").await;
    let pool = state.pool.clone();

    let status = patch(
        state,
        11,
        serde_json::json!({ "song": "   ", "artist": "Real Artist" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let song: Option<String> = sqlx::query_scalar("SELECT song FROM videos WHERE id = 11")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        song.as_deref(),
        Some("Real Song"),
        "a rejected whitespace song must not overwrite the stored song"
    );
}

/// Operator input runs through the same central sanitizer as ingested
/// titles — emoji / high-plane junk is stripped before it reaches the DB.
#[tokio::test]
async fn patch_video_sanitizes_emoji_from_song_and_artist() {
    let state = test_state().await;
    seed_video(&state, 12, "old", "old").await;
    let pool = state.pool.clone();

    let status = patch(
        state,
        12,
        serde_json::json!({ "song": "Way Maker \u{1F525}", "artist": "Sinach \u{2728}" }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (song, artist): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT song, artist FROM videos WHERE id = 12")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        song.as_deref(),
        Some("Way Maker"),
        "emoji stripped from song"
    );
    assert_eq!(
        artist.as_deref(),
        Some("Sinach"),
        "emoji stripped from artist"
    );
}

/// An empty artist clears the column to NULL (some songs have no artist),
/// mirroring the `lyrics_override_text` empty->NULL convention.
#[tokio::test]
async fn patch_video_clears_artist_to_null_on_empty() {
    let state = test_state().await;
    seed_video(&state, 13, "Song", "Delete Me").await;
    let pool = state.pool.clone();

    let status = patch(state, 13, serde_json::json!({ "artist": "" })).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let artist: Option<String> = sqlx::query_scalar("SELECT artist FROM videos WHERE id = 13")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(artist, None, "empty artist must clear to NULL");
}

/// A song-only PATCH must not clobber the untouched artist column (dynamic
/// UPDATE touches only provided fields).
#[tokio::test]
async fn patch_video_song_only_leaves_artist_untouched() {
    let state = test_state().await;
    seed_video(&state, 14, "Wrong", "Keep This Artist").await;
    let pool = state.pool.clone();

    let status = patch(state, 14, serde_json::json!({ "song": "Right" })).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (song, artist): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT song, artist FROM videos WHERE id = 14")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(song.as_deref(), Some("Right"));
    assert_eq!(
        artist.as_deref(),
        Some("Keep This Artist"),
        "artist must survive a song-only PATCH"
    );
}

// ── #136: an operator's title correction is final ──────────────────────────

/// A provider that would name the video differently — what the metadata repair
/// would write over a correction it is allowed to reach.
struct NamesAnotherSong;

#[async_trait::async_trait]
impl crate::metadata::MetadataProvider for NamesAnotherSong {
    async fn extract(
        &self,
        _video_id: &str,
        _title: &str,
    ) -> Result<sp_core::metadata::VideoMetadata, crate::metadata::MetadataError> {
        Ok(sp_core::metadata::VideoMetadata {
            song: "Another Song".into(),
            artist: "Another Artist".into(),
            source: sp_core::metadata::MetadataSource::Gemini,
            gemini_failed: false,
        })
    }

    fn name(&self) -> &str {
        "claude"
    }
}

/// A parser-named row (`gemini_failed = 1`, `metadata_source = 'regex'`) with
/// its video + audio under the parser's `_gf` name in `dir`. Returns the two
/// file paths.
async fn seed_parser_row(state: &crate::AppState, id: i64, dir: &std::path::Path) -> [String; 2] {
    let base = format!("Old Song_Old Artist_PATCHED{id:04}_normalized_gf");
    let video = dir.join(format!("{base}_video.mp4"));
    let audio = dir.join(format!("{base}_audio.flac"));
    std::fs::write(&video, b"v").unwrap();
    std::fs::write(&audio, b"a").unwrap();
    let files = [
        video.to_string_lossy().into_owned(),
        audio.to_string_lossy().into_owned(),
    ];
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&state.pool)
        .await
        .ok();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, title, song, artist, gemini_failed, \
                             normalized, metadata_source, file_path, audio_file_path) \
         VALUES (?, 1, ?, 'Break! - planetboom (Live)', 'Old Song', 'Old Artist', 1, 1, \
                 'regex', ?, ?)",
    )
    .bind(id)
    .bind(format!("PATCHED{id:04}"))
    .bind(&files[0])
    .bind(&files[1])
    .execute(&state.pool)
    .await
    .unwrap();
    files
}

/// `(song, artist, gemini_failed, metadata_source, file_path, audio_file_path)`.
async fn metadata_row(
    pool: &sqlx::SqlitePool,
    id: i64,
) -> (String, String, i64, Option<String>, String, String) {
    sqlx::query_as(
        "SELECT song, artist, gemini_failed, metadata_source, file_path, audio_file_path \
         FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// #136 (release 0.69.0 review 🟡 3): the operator corrects a parser title
/// on the dashboard, then the metadata repair runs (5 s after start, then
/// every 30 min). The correction, and the files it names, must survive: the
/// row left the repair queue when it was patched.
#[tokio::test]
async fn a_patched_title_survives_the_metadata_repair() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state().await;
    let files = seed_parser_row(&state, 21, dir.path()).await;
    let pool = state.pool.clone();

    let status = patch(
        state,
        21,
        serde_json::json!({ "song": "Break!", "artist": "planetboom" }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let chain = std::sync::Arc::new(crate::metadata::ProviderChain::new(vec![Box::new(
        NamesAnotherSong,
    )]));
    let mut repair =
        crate::reprocess::ReprocessWorker::new(pool.clone(), chain, dir.path().to_path_buf());
    assert_eq!(
        repair.process_all().await.unwrap(),
        0,
        "the repair must not touch a row the operator corrected"
    );

    let (song, artist, gemini_failed, source, file_path, audio_file_path) =
        metadata_row(&pool, 21).await;
    assert_eq!((song.as_str(), artist.as_str()), ("Break!", "planetboom"));
    assert_eq!(
        gemini_failed, 0,
        "a corrected row is no longer parser-named"
    );
    assert_eq!(source.as_deref(), Some("manual"));
    assert_eq!(
        [file_path, audio_file_path],
        files,
        "the files keep their names"
    );
    for f in &files {
        assert!(std::path::Path::new(f).exists(), "{f} must not be moved");
    }
    assert_eq!(
        crate::metadata::health::failed_videos(&pool).await.unwrap(),
        0,
        "status.metadata no longer counts the corrected row"
    );
}

/// An artist-only correction is a correction too.
#[tokio::test]
async fn an_artist_only_patch_marks_the_metadata_manual() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state().await;
    seed_parser_row(&state, 22, dir.path()).await;
    let pool = state.pool.clone();

    let status = patch(state, 22, serde_json::json!({ "artist": "planetboom" })).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (song, artist, gemini_failed, source, _, _) = metadata_row(&pool, 22).await;
    assert_eq!((song.as_str(), artist.as_str()), ("Old Song", "planetboom"));
    assert_eq!((gemini_failed, source.as_deref()), (0, Some("manual")));
}

/// Pin: a PATCH of only the other fields (the EN suppress flag, the lyrics
/// override) is no title correction — the row stays in the repair queue.
#[tokio::test]
async fn a_patch_without_song_or_artist_leaves_the_repair_queue_alone() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state().await;
    seed_parser_row(&state, 23, dir.path()).await;
    let pool = state.pool.clone();

    let status = patch(
        state,
        23,
        serde_json::json!({ "suppress_resolume_en": true, "lyrics_override_text": "a\nb" }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, _, gemini_failed, source, _, _) = metadata_row(&pool, 23).await;
    assert_eq!((gemini_failed, source.as_deref()), (1, Some("regex")));
    assert_eq!(
        crate::metadata::health::failed_videos(&pool).await.unwrap(),
        1
    );
}

/// A provider that is slow enough for the operator: while the repair batch
/// waits for its answer, the operator corrects the row on the dashboard (the
/// real `PATCH` through the router), then the provider names it differently.
struct OperatorCorrectsDuringTheCall {
    state: crate::AppState,
    id: i64,
}

#[async_trait::async_trait]
impl crate::metadata::MetadataProvider for OperatorCorrectsDuringTheCall {
    async fn extract(
        &self,
        video_id: &str,
        title: &str,
    ) -> Result<sp_core::metadata::VideoMetadata, crate::metadata::MetadataError> {
        let status = patch(
            self.state.clone(),
            self.id,
            serde_json::json!({ "song": "Break!", "artist": "planetboom" }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "the operator's PATCH");
        crate::metadata::MetadataProvider::extract(&NamesAnotherSong, video_id, title).await
    }

    fn name(&self) -> &str {
        "claude"
    }
}

/// #136 review round 2: `process_all` reads the repair queue once, then each
/// row waits for the providers. A correction that lands while the batch runs
/// (the batch starts 5 s after every deploy, when the operator looks at the
/// `_gf` titles) must survive too: the row left the queue, so the repair
/// neither renames its files nor writes its metadata.
#[tokio::test]
async fn a_correction_made_while_the_repair_batch_runs_survives() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state().await;
    let files = seed_parser_row(&state, 24, dir.path()).await;
    let pool = state.pool.clone();

    let chain = std::sync::Arc::new(crate::metadata::ProviderChain::new(vec![Box::new(
        OperatorCorrectsDuringTheCall { state, id: 24 },
    )]));
    let mut repair =
        crate::reprocess::ReprocessWorker::new(pool.clone(), chain, dir.path().to_path_buf());
    assert_eq!(
        repair.process_all().await.unwrap(),
        0,
        "the row left the queue during the call: not repaired"
    );

    let (song, artist, gemini_failed, source, file_path, audio_file_path) =
        metadata_row(&pool, 24).await;
    assert_eq!((song.as_str(), artist.as_str()), ("Break!", "planetboom"));
    assert_eq!((gemini_failed, source.as_deref()), (0, Some("manual")));
    assert_eq!(
        [file_path, audio_file_path],
        files,
        "the files keep their names"
    );
    for f in &files {
        assert!(std::path::Path::new(f).exists(), "{f} must not be moved");
    }
}
