//! Test fixtures for the karaoke stems state (#136), shared by the
//! `models_stems` tests and the mixer API tests. Wired from `models_stems.rs`
//! via `#[path = "models_stems_fixtures.rs"]` under `#[cfg(test)]`.

use sqlx::SqlitePool;

/// Give a video REAL stem files: its audio sidecar moves into a temp cache and
/// both stems [`crate::stems::stem_paths`] derives from it are written there,
/// then the row is marked `done` with those paths. A stems-ready fixture must
/// hold the files a consumer opens (#136: a `done` row whose recorded paths name
/// no file is exactly the regression). Keep the returned dir alive.
pub(crate) async fn give_real_stems(pool: &SqlitePool, video_id: i64) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let audio = dir
        .path()
        .join(format!("S_A_{video_id}_normalized_audio.flac"));
    std::fs::write(&audio, b"a").unwrap();
    let (vocals, instrumental) = crate::stems::stem_paths(&audio);
    std::fs::write(&vocals, b"v").unwrap();
    std::fs::write(&instrumental, b"i").unwrap();
    sqlx::query("UPDATE videos SET audio_file_path = ? WHERE id = ?")
        .bind(audio.to_string_lossy().as_ref())
        .bind(video_id)
        .execute(pool)
        .await
        .unwrap();
    super::mark_stems_done(
        pool,
        video_id,
        &vocals.to_string_lossy(),
        &instrumental.to_string_lossy(),
    )
    .await
    .unwrap();
    dir
}
