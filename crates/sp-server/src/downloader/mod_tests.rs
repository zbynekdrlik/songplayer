//! The download worker's tests (moved out of mod.rs for the 1000-line cap, #229).

use super::*;

/// #136 T4: yt-dlp's `--dump-json` title text must arrive as UTF-8, so
/// every yt-dlp spawn gets `PYTHONUTF8=1` + `PYTHONIOENCODING=utf-8`.
/// Introspect the built Command's explicit env to pin both.
#[test]
fn apply_utf8_env_sets_pythonutf8_and_ioencoding() {
    use std::ffi::OsStr;
    let mut cmd = tokio::process::Command::new("yt-dlp");
    apply_utf8_env(&mut cmd);
    let mut utf8 = None;
    let mut ioenc = None;
    for (k, v) in cmd.as_std().get_envs() {
        if k == OsStr::new("PYTHONUTF8") {
            utf8 = v.map(|s| s.to_owned());
        }
        if k == OsStr::new("PYTHONIOENCODING") {
            ioenc = v.map(|s| s.to_owned());
        }
    }
    assert_eq!(
        utf8.as_deref(),
        Some(OsStr::new("1")),
        "PYTHONUTF8=1 must be set so frozen-Python yt-dlp emits UTF-8 stdout"
    );
    assert_eq!(
        ioenc.as_deref(),
        Some(OsStr::new("utf-8")),
        "PYTHONIOENCODING=utf-8 must be set for the redirected stdout pipe"
    );
}

#[test]
fn format_spec_orders_av1_then_hls_then_dash() {
    let spec = format_spec();
    // AV1 first — highest quality per byte, MF hardware-transform safe.
    let av1_pos = spec.find("vcodec^=av01").expect("AV1 alternative present");
    // HLS H.264 second — different encoder path from DASH, MF-compatible
    // for the THE-DEEP class of broken 1080p DASH encodes.
    let hls_pos = spec
        .find("protocol*=m3u8")
        .expect("HLS alternative present");
    // Unconstrained `bv*` last — plain bestvideo fallback.
    let fallback_pos = spec.rfind("bv*").expect("final fallback present");
    assert!(
        av1_pos < hls_pos,
        "AV1 must precede HLS in the fallback chain"
    );
    assert!(
        hls_pos < fallback_pos,
        "HLS must precede the unconstrained fallback"
    );
}

#[test]
fn format_spec_applies_max_resolution() {
    let spec = format_spec();
    let needle = format!("height<={MAX_RESOLUTION}");
    // Every alternative must cap at MAX_RESOLUTION so we never pull 4K.
    assert_eq!(
        spec.matches(&needle).count(),
        3,
        "each of the 3 alternatives must carry the height cap; spec: {spec}"
    );
}

/// RED (#141): yt-dlp answers every anonymous download with "Sign in
/// to confirm you're not a bot". A verified Netscape cookie file on
/// disk must be threaded through as `--cookies <path>`, right before
/// the URL, on both the video and audio yt-dlp invocations.
#[test]
fn ytdlp_video_args_appends_cookies_before_url_when_present() {
    let format_spec = "bv*[height<=1440]";
    let ffmpeg_dir = Path::new("/opt/ffmpeg");
    let output = Path::new("/cache/abc_video_temp.mp4");
    let url = "https://www.youtube.com/watch?v=abc";
    let cookies = Path::new("/data/cookies.txt");

    let args = ytdlp_video_args(format_spec, ffmpeg_dir, output, url, Some(cookies));

    let cookies_pos = args
        .iter()
        .position(|a| a.to_str() == Some("--cookies"))
        .expect("--cookies flag present when a cookie file is given");
    assert_eq!(
        args[cookies_pos + 1].to_str(),
        cookies.to_str(),
        "the element right after --cookies must be the cookie file path"
    );
    assert_eq!(
        args.last().unwrap().to_str(),
        Some(url),
        "the URL must remain the last argument even with --cookies inserted"
    );
}

#[test]
fn ytdlp_video_args_omits_cookies_when_absent() {
    let format_spec = "bv*[height<=1440]";
    let ffmpeg_dir = Path::new("/opt/ffmpeg");
    let output = Path::new("/cache/abc_video_temp.mp4");
    let url = "https://www.youtube.com/watch?v=abc";

    let args = ytdlp_video_args(format_spec, ffmpeg_dir, output, url, None);

    assert!(
        !args.iter().any(|a| a.to_str() == Some("--cookies")),
        "no --cookies flag when no cookie file exists"
    );
    assert_eq!(args.last().unwrap().to_str(), Some(url));
}

#[test]
fn ytdlp_video_args_keeps_existing_fixed_flags() {
    let format_spec = "bv*[height<=1440]";
    let ffmpeg_dir = Path::new("/opt/ffmpeg");
    let output = Path::new("/cache/abc_video_temp.mp4");
    let url = "https://www.youtube.com/watch?v=abc";

    let args = ytdlp_video_args(format_spec, ffmpeg_dir, output, url, None);

    for flag in ["-f", "--no-part", "--remux-video"] {
        assert!(
            args.iter().any(|a| a.to_str() == Some(flag)),
            "existing flag {flag} must still be present"
        );
    }
    // The JS-runtime flag moved OUT of the per-call arg builder into
    // `ytdlp_command` (#189) — and it must never be the old `node` value.
    assert!(
        !args.iter().any(|a| a.to_str() == Some("--js-runtimes")),
        "the runtime flag now lives in ytdlp_command, not the arg builder"
    );
    assert!(!args.iter().any(|a| a.to_str() == Some("node")));
}

#[test]
fn ytdlp_audio_args_appends_cookies_before_url_when_present() {
    let ffmpeg_dir = Path::new("/opt/ffmpeg");
    let output_template = "/cache/abc_audio_temp.%(ext)s";
    let url = "https://www.youtube.com/watch?v=abc";
    let cookies = Path::new("/data/cookies.txt");

    let args = ytdlp_audio_args(ffmpeg_dir, output_template, url, Some(cookies));

    let cookies_pos = args
        .iter()
        .position(|a| a.to_str() == Some("--cookies"))
        .expect("--cookies flag present when a cookie file is given");
    assert_eq!(args[cookies_pos + 1].to_str(), cookies.to_str());
    assert_eq!(args.last().unwrap().to_str(), Some(url));
}

#[test]
fn ytdlp_audio_args_omits_cookies_when_absent() {
    let ffmpeg_dir = Path::new("/opt/ffmpeg");
    let output_template = "/cache/abc_audio_temp.%(ext)s";
    let url = "https://www.youtube.com/watch?v=abc";

    let args = ytdlp_audio_args(ffmpeg_dir, output_template, url, None);

    assert!(!args.iter().any(|a| a.to_str() == Some("--cookies")));
    assert_eq!(args.last().unwrap().to_str(), Some(url));
}

#[test]
fn ytdlp_audio_args_keeps_existing_fixed_flags() {
    let ffmpeg_dir = Path::new("/opt/ffmpeg");
    let output_template = "/cache/abc_audio_temp.%(ext)s";
    let url = "https://www.youtube.com/watch?v=abc";

    let args = ytdlp_audio_args(ffmpeg_dir, output_template, url, None);

    for flag in [
        "-f",
        "bestaudio",
        "--no-part",
        "--print",
        "after_move:filepath",
    ] {
        assert!(
            args.iter().any(|a| a.to_str() == Some(flag)),
            "existing flag {flag} must still be present"
        );
    }
    // The JS-runtime flag moved OUT of the per-call arg builder into
    // `ytdlp_command` (#189) — and it must never be the old `node` value.
    assert!(
        !args.iter().any(|a| a.to_str() == Some("--js-runtimes")),
        "the runtime flag now lives in ytdlp_command, not the arg builder"
    );
    assert!(!args.iter().any(|a| a.to_str() == Some("node")));
}

// -----------------------------------------------------------------
// RED (#140): a failed row must back off instead of blocking the
// whole queue behind it forever.
// -----------------------------------------------------------------

#[test]
fn retry_backoff_is_5_min_at_attempt_1() {
    assert_eq!(retry_backoff(1), std::time::Duration::from_secs(5 * 60));
}

#[test]
fn retry_backoff_is_10_min_at_attempt_2() {
    assert_eq!(retry_backoff(2), std::time::Duration::from_secs(10 * 60));
}

#[test]
fn retry_backoff_is_40_min_at_attempt_4() {
    assert_eq!(retry_backoff(4), std::time::Duration::from_secs(40 * 60));
}

#[test]
fn retry_backoff_caps_at_24h_by_attempt_20() {
    assert_eq!(
        retry_backoff(20),
        std::time::Duration::from_secs(24 * 60 * 60)
    );
}

#[test]
fn retry_backoff_caps_at_24h_without_overflow_at_u32_max() {
    assert_eq!(
        retry_backoff(u32::MAX),
        std::time::Duration::from_secs(24 * 60 * 60)
    );
}

async fn seed_pool_with_three_videos() -> (SqlitePool, i64, i64, i64) {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();

    let now = chrono::Utc::now();
    let future = (now + chrono::Duration::hours(1)).to_rfc3339();
    let past = (now - chrono::Duration::hours(1)).to_rfc3339();

    // A: due 1h from now — must never be picked while B/C are eligible.
    let id_a: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, next_attempt_at) \
         VALUES (1, 'video_a', 'A', ?) RETURNING id",
    )
    .bind(&future)
    .fetch_one(&pool)
    .await
    .unwrap();

    // B: never failed (NULL next_attempt_at) — eligible immediately.
    let id_b: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title) \
         VALUES (1, 'video_b', 'B') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // C: due 1h ago — eligible, but behind B in id order.
    let id_c: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, next_attempt_at) \
         VALUES (1, 'video_c', 'C', ?) RETURNING id",
    )
    .bind(&past)
    .fetch_one(&pool)
    .await
    .unwrap();

    (pool, id_a, id_b, id_c)
}

#[tokio::test]
async fn fetch_next_unprocessed_skips_rows_not_yet_due_for_retry() {
    let (pool, id_a, id_b, id_c) = seed_pool_with_three_videos().await;

    let row = fetch_next_unprocessed(&pool)
        .await
        .unwrap()
        .expect("B is eligible immediately");
    assert_eq!(
        row.id, id_b,
        "B (NULL next_attempt_at, lowest eligible id) must be picked first"
    );
    assert_ne!(row.id, id_a, "A is not due yet");

    sqlx::query("UPDATE videos SET normalized = 1 WHERE id = ?")
        .bind(id_b)
        .execute(&pool)
        .await
        .unwrap();

    let row = fetch_next_unprocessed(&pool)
        .await
        .unwrap()
        .expect("C became due an hour ago");
    assert_eq!(row.id, id_c, "C (due 1h ago) must be picked next");
    assert_ne!(
        row.id, id_a,
        "A (due 1h from now) must never be picked while C is eligible"
    );
}

async fn seed_single_video() -> (SqlitePool, i64) {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    let video_id: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title) \
         VALUES (1, 'vid', 't') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    (pool, video_id)
}

#[tokio::test]
async fn record_download_failure_increments_attempts_and_schedules_retry_then_success_resets() {
    let (pool, video_id) = seed_single_video().await;
    let before = chrono::Utc::now();

    record_download_failure(&pool, video_id, "yt-dlp exited with 1: boom")
        .await
        .unwrap();

    let attempts: i64 = sqlx::query_scalar("SELECT download_attempts FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let last_error: Option<String> =
        sqlx::query_scalar("SELECT last_download_error FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let next_attempt_at: Option<String> =
        sqlx::query_scalar("SELECT next_attempt_at FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, 1, "first failure -> 1 attempt");
    assert_eq!(last_error.as_deref(), Some("yt-dlp exited with 1: boom"));
    let next_attempt_at = next_attempt_at.expect("next_attempt_at must be set on failure");
    let parsed = chrono::DateTime::parse_from_rfc3339(&next_attempt_at).expect("valid RFC3339");
    assert!(
        parsed.to_utc() > before,
        "next_attempt_at must be scheduled in the future"
    );

    record_download_failure(&pool, video_id, "second failure")
        .await
        .unwrap();
    let attempts: i64 = sqlx::query_scalar("SELECT download_attempts FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 2, "second failure -> 2 attempts");

    // The reset path: the same UPDATE mark_video_processed_pair issues
    // on success zeroes the three bookkeeping columns back out.
    crate::db::models::mark_video_processed_pair(
        &pool, video_id, "Song", "Artist", "test", false, "/v.mp4", "/a.flac",
    )
    .await
    .unwrap();

    let attempts: i64 = sqlx::query_scalar("SELECT download_attempts FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let last_error: Option<String> =
        sqlx::query_scalar("SELECT last_download_error FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let next_attempt_at: Option<String> =
        sqlx::query_scalar("SELECT next_attempt_at FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, 0, "success resets download_attempts to 0");
    assert!(last_error.is_none(), "success resets last_download_error");
    assert!(next_attempt_at.is_none(), "success resets next_attempt_at");
}

#[tokio::test]
async fn record_download_failure_truncates_error_to_last_300_chars() {
    let (pool, video_id) = seed_single_video().await;
    let long_error = "x".repeat(500);

    record_download_failure(&pool, video_id, &long_error)
        .await
        .unwrap();

    let last_error: Option<String> =
        sqlx::query_scalar("SELECT last_download_error FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        last_error.unwrap().len(),
        300,
        "last_download_error must be truncated to the last 300 chars"
    );
}

const SHARED_YT: &str = "aaaaaaaaaaa";

/// Two playlists, each with a row of [`SHARED_YT`] that records `video` and
/// `audio`, or no file at all (`None`): rows of one video share files by name
/// (#136).
async fn pool_with_two_rows(pair: Option<(&Path, &Path)>) -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p1', 'u1'), (2, 'p2', 'u2')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let video = pair.map(|(v, _)| v.to_string_lossy().into_owned());
    let audio = pair.map(|(_, a)| a.to_string_lossy().into_owned());
    for playlist in [1, 2] {
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, normalized, file_path, \
             audio_file_path) VALUES (?, ?, 't', ?, ?, ?)",
        )
        .bind(playlist)
        .bind(SHARED_YT)
        .bind(pair.is_some())
        .bind(&video)
        .bind(&audio)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool
}

/// The local download's names for [`SHARED_YT`] in `dir`: the video temp, the
/// final video and the final (normalized) audio, the temp and the audio
/// written. With `video_held`, a non-empty directory holds the final video's
/// name: a rename onto it fails on Linux and on Windows.
fn download_files(dir: &Path, video_held: bool) -> (PathBuf, PathBuf, PathBuf) {
    let temp = dir.join(format!("{SHARED_YT}_video_temp.mp4"));
    let video = dir.join(cache::video_filename("Song", "Artist", SHARED_YT, false));
    let audio = dir.join(cache::audio_filename("Song", "Artist", SHARED_YT, false));
    std::fs::write(&temp, b"the fresh video").unwrap();
    std::fs::write(&audio, b"the normalized audio").unwrap();
    if video_held {
        std::fs::create_dir(&video).unwrap();
        std::fs::write(video.join("held"), b"in the way").unwrap();
    }
    (temp, video, audio)
}

/// #229 follow-up (data loss): the local download normalizes into the audio
/// name every row of the video records (#136). When the video then cannot
/// take its final name, that audio is still the one two rows play: it stays.
#[tokio::test]
async fn a_failed_video_rename_keeps_the_audio_two_rows_record() {
    let dir = tempfile::tempdir().unwrap();
    let (temp, video, audio) = download_files(dir.path(), true);
    let pool = pool_with_two_rows(Some((&video, &audio))).await;
    assert!(place_video(&pool, &temp, &video, &audio).await.is_err());
    assert_eq!(
        std::fs::read(&audio).unwrap(),
        b"the normalized audio",
        "the audio two rows record survives the failed attempt"
    );
    assert!(!temp.exists(), "the attempt's video temp goes");
}

/// An audio no row records is this attempt's alone: it goes with the failed
/// attempt, never left behind as an orphan.
#[tokio::test]
async fn a_failed_video_rename_drops_an_audio_no_row_records() {
    let dir = tempfile::tempdir().unwrap();
    let (temp, video, audio) = download_files(dir.path(), true);
    let pool = pool_with_two_rows(None).await;
    assert!(place_video(&pool, &temp, &video, &audio).await.is_err());
    assert!(
        !audio.exists(),
        "an unrecorded audio is this attempt's debris"
    );
    assert!(!temp.exists());
}

/// When the rows cannot be read, nobody can say the audio is debris: it
/// stays (WARNed).
#[tokio::test]
async fn a_failed_video_rename_keeps_the_audio_when_the_rows_cannot_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let (temp, video, audio) = download_files(dir.path(), true);
    let pool = pool_with_two_rows(None).await;
    pool.close().await;
    assert!(place_video(&pool, &temp, &video, &audio).await.is_err());
    assert_eq!(std::fs::read(&audio).unwrap(), b"the normalized audio");
    assert!(!temp.exists());
}

#[tokio::test]
async fn a_placed_video_keeps_its_audio() {
    let dir = tempfile::tempdir().unwrap();
    let (temp, video, audio) = download_files(dir.path(), false);
    let pool = pool_with_two_rows(None).await;
    place_video(&pool, &temp, &video, &audio).await.unwrap();
    assert_eq!(std::fs::read(&video).unwrap(), b"the fresh video");
    assert_eq!(std::fs::read(&audio).unwrap(), b"the normalized audio");
    assert!(!temp.exists());
}
