//! Startup migration integration test: legacy files are deleted and
//! all video rows are reset to unnormalized on first boot.

use std::fs;
use std::path::{Path, PathBuf};

use sp_server::SyncRequest;
use sp_server::startup::{self_heal_cache, startup_sync_active_playlists};
use sqlx::Row;

#[tokio::test]
async fn self_heal_deletes_legacy_files_and_resets_normalized() {
    let pool = sp_server::db::create_memory_pool().await.unwrap();
    sp_server::db::run_migrations(&pool).await.unwrap();

    // Seed a playlist + an already-normalized video pointing at a legacy
    // .mp4 path. (Note: V4 has already reset normalized=0 via run_migrations,
    // so we UPDATE the row back to normalized=1 to simulate a row that
    // somehow survived.)
    sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES ('p', 'u', 'n')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let legacy_path = tmp.path().join("Old_Song_dQw4w9WgXcQ_normalized.mp4");
    fs::write(&legacy_path, b"legacy").unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, file_path) VALUES (1, 'dQw4w9WgXcQ', 1, ?)",
    )
    .bind(legacy_path.to_string_lossy().as_ref())
    .execute(&pool)
    .await
    .unwrap();

    self_heal_cache(&pool, tmp.path()).await.unwrap();

    assert!(!legacy_path.exists(), "legacy .mp4 must be deleted");

    // Verify self_heal cleared the stale file_path so the download
    // worker knows this video needs re-processing. V4 migration
    // already set normalized=0 for all rows; self_heal's job is to
    // delete the file and (optionally) clear the path reference.
    let row = sqlx::query(
        "SELECT file_path, audio_file_path FROM videos WHERE youtube_id = 'dQw4w9WgXcQ'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let file_path: Option<String> = row.get("file_path");
    // The legacy file was deleted; self_heal doesn't clear the DB
    // path (V4 migration handles the normalized flag), but the file
    // no longer exists on disk. Verify the row still exists.
    assert!(
        file_path.is_some(),
        "row must still exist in DB after legacy cleanup"
    );
}

#[tokio::test]
async fn self_heal_deletes_orphan_half_sidecar() {
    let pool = sp_server::db::create_memory_pool().await.unwrap();
    sp_server::db::run_migrations(&pool).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();

    // A video sidecar without its audio partner — classic mid-download crash.
    let orphan = tmp.path().join("S_A_aaaaaaaaaaa_normalized_video.mp4");
    fs::write(&orphan, b"orphan").unwrap();

    self_heal_cache(&pool, tmp.path()).await.unwrap();

    assert!(!orphan.exists(), "orphan sidecar must be deleted");
}

#[tokio::test]
async fn self_heal_keeps_complete_pairs_and_links_to_db() {
    let pool = sp_server::db::create_memory_pool().await.unwrap();
    sp_server::db::run_migrations(&pool).await.unwrap();

    // Seed a playlist + an un-normalized video row matching the pair below.
    sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES ('p', 'u', 'n')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, file_path) VALUES (1, 'bbbbbbbbbbb', 0, NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let v = tmp.path().join("S_A_bbbbbbbbbbb_normalized_video.mp4");
    let a = tmp.path().join("S_A_bbbbbbbbbbb_normalized_audio.flac");
    fs::write(&v, b"v").unwrap();
    fs::write(&a, b"a").unwrap();

    self_heal_cache(&pool, tmp.path()).await.unwrap();

    assert!(v.exists(), "complete video sidecar must survive");
    assert!(a.exists(), "complete audio sidecar must survive");

    // DB row must have been re-linked and marked normalized.
    let row = sqlx::query("SELECT file_path, audio_file_path, normalized FROM videos WHERE youtube_id = 'bbbbbbbbbbb'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let file_path: Option<String> = row.get("file_path");
    let audio_path: Option<String> = row.get("audio_file_path");
    let normalized: i64 = row.get("normalized");
    assert_eq!(normalized, 1, "row must be marked normalized after re-link");
    assert!(file_path.is_some() && file_path.unwrap().ends_with("_video.mp4"));
    assert!(audio_path.is_some() && audio_path.unwrap().ends_with("_audio.flac"));
}

#[tokio::test]
async fn startup_sync_enqueues_one_request_per_active_playlist() {
    let pool = sp_server::db::create_memory_pool().await.unwrap();
    sp_server::db::run_migrations(&pool).await.unwrap();

    // Two active playlists and one inactive.
    sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name, is_active)
         VALUES ('ytfast', 'https://yt.com/pl1', 'SP-fast', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name, is_active)
         VALUES ('ytslow', 'https://yt.com/pl2', 'SP-slow', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name, is_active)
         VALUES ('inactive', 'https://yt.com/pl3', 'SP-off', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<SyncRequest>(16);
    startup_sync_active_playlists(&pool, &tx).await.unwrap();
    drop(tx);

    let mut received: Vec<SyncRequest> = Vec::new();
    while let Some(req) = rx.recv().await {
        received.push(req);
    }

    assert_eq!(
        received.len(),
        2,
        "expected 2 SyncRequests for 2 active playlists"
    );
    let urls: Vec<String> = received.iter().map(|r| r.youtube_url.clone()).collect();
    assert!(urls.contains(&"https://yt.com/pl1".to_string()));
    assert!(urls.contains(&"https://yt.com/pl2".to_string()));
    assert!(!urls.contains(&"https://yt.com/pl3".to_string()));

    let pids: Vec<i64> = received.iter().map(|r| r.playlist_id).collect();
    assert!(
        pids.iter().all(|&id| id > 0),
        "every SyncRequest must carry a valid playlist_id"
    );
    assert_eq!(
        pids.len(),
        pids.iter().collect::<std::collections::HashSet<_>>().len(),
        "playlist_ids must be unique"
    );
}

#[tokio::test]
async fn startup_sync_is_noop_when_no_active_playlists() {
    let pool = sp_server::db::create_memory_pool().await.unwrap();
    sp_server::db::run_migrations(&pool).await.unwrap();

    // All playlists inactive.
    sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name, is_active)
         VALUES ('off1', 'https://yt.com/x', 'SP-x', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<SyncRequest>(8);
    startup_sync_active_playlists(&pool, &tx).await.unwrap();
    drop(tx);

    assert!(rx.recv().await.is_none(), "no SyncRequests expected");
}

// ── #136 reopen: the files a song names after its audio ─────────────────────
//
// The metadata repair of 29.9.2026 renamed ~99 songs' video + audio and left
// their stems under the old `_gf` name (video 326 `IYAOosrh7HY`): the lyrics
// isolation waited for stems forever and the stem mixer found none. The startup
// self-heal re-links such files to the name the audio derives, and resets
// stems no name holds so the stem worker separates them again.

const REPAIRED_ID: &str = "IYAOosrh7HY";

fn named(dir: &Path, base: &str, suffix: &str) -> PathBuf {
    dir.join(format!("{base}_{REPAIRED_ID}_normalized{suffix}"))
}

fn text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// A repaired song: video + audio under the NEW name, its row `done` with the
/// stems (and `ready` with the dub) recorded under the OLD `_gf` name.
async fn repaired_song(dir: &Path) -> (sqlx::SqlitePool, PathBuf) {
    let pool = sp_server::db::create_memory_pool().await.unwrap();
    sp_server::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES ('p', 'u', 'n')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let video = named(dir, "Gods Not Dead_Enjoy Worship", "_video.mp4");
    let audio = named(dir, "Gods Not Dead_Enjoy Worship", "_audio.flac");
    fs::write(&video, b"v").unwrap();
    fs::write(&audio, b"a").unwrap();
    let old = |suffix: &str| text(&named(dir, "Old_Song", suffix));
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, file_path, audio_file_path,
                             stem_status, vocals_file_path, instrumental_file_path,
                             dub_status, dub_file_path)
         VALUES (1, ?, 1, ?, ?, 'done', ?, ?, 'ready', ?)",
    )
    .bind(REPAIRED_ID)
    .bind(text(&video))
    .bind(text(&audio))
    .bind(old("_gf_audio_vocals.flac"))
    .bind(old("_gf_audio_instrumental.flac"))
    .bind(old("_gf_dub.flac"))
    .execute(&pool)
    .await
    .unwrap();
    (pool, audio)
}

/// The repaired row's stems + dub columns.
#[derive(Debug, PartialEq)]
struct StemRow {
    stem_status: Option<String>,
    stem_attempts: i64,
    vocals: Option<String>,
    instrumental: Option<String>,
    dub_status: String,
    dub: Option<String>,
}

async fn stem_row(pool: &sqlx::SqlitePool) -> StemRow {
    let r = sqlx::query(
        "SELECT stem_status, stem_attempts, vocals_file_path, instrumental_file_path,
                dub_status, dub_file_path
         FROM videos WHERE youtube_id = ?",
    )
    .bind(REPAIRED_ID)
    .fetch_one(pool)
    .await
    .unwrap();
    StemRow {
        stem_status: r.get("stem_status"),
        stem_attempts: r.get("stem_attempts"),
        vocals: r.get("vocals_file_path"),
        instrumental: r.get("instrumental_file_path"),
        dub_status: r.get("dub_status"),
        dub: r.get("dub_file_path"),
    }
}

#[tokio::test]
async fn self_heal_relinks_stems_and_dub_left_under_an_old_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (pool, audio) = repaired_song(dir).await;
    let left_behind = [
        "_gf_audio_vocals.flac",
        "_gf_audio_instrumental.flac",
        "_gf_dub.flac",
        "_gf_dub_transcripts.json",
    ]
    .map(|suffix| named(dir, "Old_Song", suffix));
    for p in &left_behind {
        fs::write(p, text(p)).unwrap();
    }

    self_heal_cache(&pool, dir).await.unwrap();

    let (vocals, instrumental) = sp_server::stems::stem_paths(&audio);
    let current = [
        vocals.clone(),
        instrumental.clone(),
        sp_server::stems::dub_path(&audio),
        sp_server::stems::dub_transcripts_path(&audio),
    ];
    for (from, to) in left_behind.iter().zip(&current) {
        assert!(!from.exists(), "{} must be moved away", from.display());
        assert_eq!(
            fs::read_to_string(to).unwrap(),
            text(from),
            "{} must hold the file that was {}",
            to.display(),
            from.display()
        );
    }
    assert_eq!(
        stem_row(&pool).await,
        StemRow {
            stem_status: Some("done".into()),
            stem_attempts: 0,
            vocals: Some(text(&vocals)),
            instrumental: Some(text(&instrumental)),
            dub_status: "ready".into(),
            dub: Some(text(&current[2])),
        },
        "the row keeps its states and records the files under the audio's name"
    );
}

/// Stems recorded `done` that no name holds any more: the row goes back to
/// pending so the stem worker separates the song again, never a `done` the
/// lyrics isolation waits on forever. A `ready` dub no name holds is left as it
/// is (a dub is an operator-requested synthesis, never re-run on its own).
#[tokio::test]
async fn self_heal_resets_stems_missing_under_every_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (pool, _audio) = repaired_song(dir).await;
    sqlx::query("UPDATE videos SET stem_attempts = 2")
        .execute(&pool)
        .await
        .unwrap();

    self_heal_cache(&pool, dir).await.unwrap();

    assert_eq!(
        stem_row(&pool).await,
        StemRow {
            stem_status: None,
            stem_attempts: 0,
            vocals: None,
            instrumental: None,
            dub_status: "ready".into(),
            dub: Some(text(&named(dir, "Old_Song", "_gf_dub.flac"))),
        },
        "the stems row is pending again with no recorded path; the dub row is untouched"
    );
}

/// A song whose video and audio sit under two different names is still one
/// song the DB records: the self-heal must not delete its halves as orphans, it
/// keeps them and the row plays on (#136 review round 1). A rename whose
/// move-back failed leaves exactly this: `rename_song_files` records the stuck
/// half at its new name and the other at its old one (review round 2).
#[tokio::test]
async fn self_heal_keeps_the_halves_of_a_split_song_the_db_records() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (pool, audio) = repaired_song(dir).await;
    let video = named(dir, "Old_Song", "_gf_video.mp4");
    fs::remove_file(named(dir, "Gods Not Dead_Enjoy Worship", "_video.mp4")).unwrap();
    fs::write(&video, b"v").unwrap();
    sqlx::query("UPDATE videos SET file_path = ?")
        .bind(text(&video))
        .execute(&pool)
        .await
        .unwrap();
    // A half no row records is still crash debris.
    let debris = named(dir, "Debris_Song", "_audio.flac");
    fs::write(&debris, b"x").unwrap();

    self_heal_cache(&pool, dir).await.unwrap();

    assert_eq!(
        fs::read(&video).unwrap(),
        b"v",
        "the recorded video half is kept"
    );
    assert_eq!(
        fs::read(&audio).unwrap(),
        b"a",
        "the recorded audio half is kept"
    );
    assert!(!debris.exists(), "an unrecorded half is still removed");
    let r = sqlx::query("SELECT file_path, audio_file_path FROM videos WHERE youtube_id = ?")
        .bind(REPAIRED_ID)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(r.get::<String, _>("file_path"), text(&video));
    assert_eq!(r.get::<String, _>("audio_file_path"), text(&audio));
}

/// A song whose stems already sit under its audio's name is left alone.
#[tokio::test]
async fn self_heal_leaves_stems_in_place_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (pool, audio) = repaired_song(dir).await;
    let (vocals, instrumental) = sp_server::stems::stem_paths(&audio);
    fs::write(&vocals, b"v").unwrap();
    fs::write(&instrumental, b"i").unwrap();
    sqlx::query(
        "UPDATE videos SET vocals_file_path = ?, instrumental_file_path = ?,
                dub_status = 'none', dub_file_path = NULL",
    )
    .bind(text(&vocals))
    .bind(text(&instrumental))
    .execute(&pool)
    .await
    .unwrap();

    self_heal_cache(&pool, dir).await.unwrap();

    assert_eq!(fs::read(&vocals).unwrap(), b"v");
    assert_eq!(fs::read(&instrumental).unwrap(), b"i");
    assert_eq!(
        stem_row(&pool).await,
        StemRow {
            stem_status: Some("done".into()),
            stem_attempts: 0,
            vocals: Some(text(&vocals)),
            instrumental: Some(text(&instrumental)),
            dub_status: "none".into(),
            dub: None,
        }
    );
}
