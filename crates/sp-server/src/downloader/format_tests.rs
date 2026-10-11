//! #223 S9a: the format selector and the `max_resolution` cap (`format.rs`).

use super::*;

const DASH: &str = "[dynamic_range=SDR][protocol=https][vcodec!^=avc1]";
const HLS: &str = "[dynamic_range=SDR][protocol*=m3u8][vcodec^=avc1]";

/// At 1440: the 1440, 1080 and 720 tiers (DASH before HLS in each), then
/// the four with no lower bound — exactly the selector checked through the
/// box's yt-dlp (#223 comment 6099757719).
#[test]
fn the_selector_walks_the_tiers_from_the_cap_down() {
    let expected = [
        format!("bv*[height<=1440][height>=1440]{DASH}"),
        format!("bv*[height<=1440][height>=1440]{HLS}"),
        format!("bv*[height<=1440][height>=1080]{DASH}"),
        format!("bv*[height<=1440][height>=1080]{HLS}"),
        format!("bv*[height<=1440][height>=720]{DASH}"),
        format!("bv*[height<=1440][height>=720]{HLS}"),
        format!("bv*[height<=1440]{DASH}"),
        format!("bv*[height<=1440]{HLS}"),
        "bv*[height<=1440][dynamic_range=SDR]".to_string(),
        "bv*[height<=1440]".to_string(),
    ]
    .join("/");
    assert_eq!(format_spec(1440), expected);
}

/// THE DEEP (`xrhVLX6vwPk`): VP9 only at 360p, H.264 1080p over HLS. The
/// 1080 tier's HLS alternative comes before any alternative with no lower
/// bound, so its 360p VP9 never wins (D8's untiered selector picked it).
#[test]
fn a_higher_h264_over_hls_beats_a_lower_vp9() {
    let spec = format_spec(1440);
    let hls_1080 = spec
        .find(&format!("[height>=1080]{HLS}"))
        .expect("the 1080 tier's HLS alternative");
    let dash_any = spec
        .find(&format!("bv*[height<=1440]{DASH}"))
        .expect("the DASH alternative with no floor");
    assert!(hls_1080 < dash_any, "{spec}");
}

/// #223 S10b: at 2160, any picture taller than 1440 rows at 25 fps or less
/// first, then the 1440 selector exactly, every later alternative capped at
/// 1440 rows (comment 6102705701).
#[test]
fn at_2160_a_taller_picture_than_1440_rows_is_taken_only_at_25_fps_or_less() {
    let tall = "[height>1440][fps<=25]";
    let expected = [
        format!("bv*[height<=2160]{tall}{DASH}"),
        format!("bv*[height<=2160]{tall}{HLS}"),
        format_spec(1440),
    ]
    .join("/");
    assert_eq!(format_spec(2160), expected);
}

/// A 4K60 format matches no alternative: the tall ones ask 25 fps or less,
/// every other one at most 1440 rows (the ceiling was the cap before S10b,
/// so the 1440 tier took 4K60).
#[test]
fn no_alternative_takes_a_taller_picture_than_1440_rows_at_any_rate() {
    for cap in [1441, 1800, 2160] {
        let spec = format_spec(cap);
        let alternatives: Vec<&str> = spec.split('/').collect();
        assert_eq!(alternatives.len(), 12, "{spec}");
        for (i, alternative) in alternatives.iter().enumerate() {
            if i < 2 {
                assert!(
                    alternative.starts_with(&format!("bv*[height<={cap}][height>1440][fps<=25]")),
                    "{alternative}"
                );
            } else {
                assert!(
                    alternative.starts_with("bv*[height<=1440]"),
                    "{alternative}"
                );
            }
        }
    }
}

#[test]
fn only_the_tiers_under_the_cap_are_walked() {
    let at_1080 = format_spec(1080);
    assert!(!at_1080.contains("[height>=1440]") && !at_1080.contains("fps"));
    assert!(at_1080.starts_with(&format!("bv*[height<=1080][height>=1080]{DASH}/")));
    let at_480 = format_spec(480);
    assert!(!at_480.contains("[height>="), "{at_480}");
    assert_eq!(at_480.split('/').count(), 4, "{at_480}");
}

#[test]
fn h264_is_only_ever_asked_over_hls() {
    for alternative in format_spec(2160).split('/') {
        if alternative.contains("vcodec^=avc1") {
            assert!(alternative.contains("protocol*=m3u8"), "{alternative}");
        }
    }
}

/// #223 S10b: unset or unreadable = 2160 with hardware decode, 1440
/// without; a stored cap wins either way (comment 6102693285).
#[test]
fn the_cap_is_the_setting_clamped_or_the_decode_paths_default() {
    for raw in [None, Some(""), Some("abc")] {
        assert_eq!(max_resolution(raw, true), 2160, "{raw:?}");
        assert_eq!(max_resolution(raw, false), 1440, "{raw:?}");
    }
    for hw in [true, false] {
        assert_eq!(max_resolution(Some("2160"), hw), 2160);
        assert_eq!(max_resolution(Some(" 1080 "), hw), 1080);
        assert_eq!(max_resolution(Some("4320"), hw), 2160);
        assert_eq!(max_resolution(Some("100"), hw), 480);
        assert_eq!(max_resolution(Some("480"), hw), 480);
    }
}

/// The download's and the probe's cap read from the database: both
/// settings, each read as `max_resolution` reads it.
#[tokio::test]
async fn the_live_cap_follows_the_stored_settings() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let set = |key: &'static str, value: &'static str| {
        let pool = pool.clone();
        async move {
            crate::db::models::set_setting(&pool, key, value)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        live_cap(&pool).await,
        1440,
        "nothing stored: hardware decode off"
    );
    set("video_hw_decode", "true").await;
    assert_eq!(live_cap(&pool).await, 2160);
    set("max_resolution", "1080").await;
    assert_eq!(live_cap(&pool).await, 1080, "a stored cap wins");
    set("video_hw_decode", "false").await;
    set("max_resolution", "2160").await;
    assert_eq!(
        live_cap(&pool).await,
        2160,
        "a stored cap wins with decode off too"
    );
    set("max_resolution", "").await;
    assert_eq!(live_cap(&pool).await, 1440);
}

#[test]
fn a_patch_of_max_resolution_takes_480_to_2160_or_empty() {
    assert_eq!(checked("max_resolution", " 2160 "), Ok("2160".to_string()));
    assert_eq!(checked("max_resolution", "480"), Ok("480".to_string()));
    assert_eq!(checked("max_resolution", ""), Ok(String::new()));
    for refused in ["479", "2161", "abc", "-1", "1440.5"] {
        let reason = checked("max_resolution", refused).unwrap_err();
        assert!(
            reason.contains("480") && reason.contains("2160"),
            "{reason}"
        );
    }
    assert_eq!(checked("gemini_model", "abc"), Ok("abc".to_string()));
}

/// The format line among yt-dlp's progress lines (its last one counts).
#[test]
fn the_fetched_format_is_read_from_the_marked_line() {
    let stdout = "[download] 100% of 52.31MiB\n\
                  SPFMT|401|av01.0.12M.08|3840|2160|25\n\
                  [download] done\n";
    assert_eq!(
        parse_downloaded_format(stdout),
        Some(DownloadedFormat {
            format_id: "401".into(),
            codec: Some("av01.0.12M.08".into()),
            width: Some(3840),
            height: Some(2160),
            fps: Some(25.0),
        })
    );
    let twice = "SPFMT|270|avc1.640028|1920|1080|25.0\nSPFMT|271|vp9|2560|1440|29.97\n";
    let last = parse_downloaded_format(twice).unwrap();
    assert_eq!(
        (last.format_id.as_str(), last.height, last.fps),
        ("271", Some(1440), Some(29.97))
    );
}

#[test]
fn an_unknown_field_reads_none_and_no_line_reads_none() {
    let f = parse_downloaded_format("SPFMT|313|vp9|NA|NA|NA").unwrap();
    assert_eq!((f.width, f.height, f.fps), (None, None, None));
    assert_eq!(f.codec.as_deref(), Some("vp9"));
    let unknown_codec = parse_downloaded_format("SPFMT|313|NA|3840|2160|25").unwrap();
    assert_eq!(
        (unknown_codec.codec, unknown_codec.height),
        (None, Some(2160))
    );
    assert_eq!(
        parse_downloaded_format("SPFMT|NA|vp9|1|2|3"),
        None,
        "no format id"
    );
    assert_eq!(
        parse_downloaded_format("SPFMT|401|av01|3840|2160"),
        None,
        "a field short"
    );
    assert_eq!(parse_downloaded_format("[download] 100%\n"), None);
    assert!(FORMAT_PRINT.starts_with("after_move:SPFMT|"));
}

#[tokio::test]
async fn the_format_is_recorded_on_every_row_of_the_video() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'a', 'u1'), (2, 'b', 'u2')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id) VALUES \
         (10, 1, 'PySFfTurafA'), (11, 2, 'PySFfTurafA'), (12, 1, 'otherotheri')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let format = DownloadedFormat {
        format_id: "400".into(),
        codec: Some("av01.0.12M.08".into()),
        width: Some(2560),
        height: Some(1440),
        fps: Some(24.0),
    };
    record(&pool, 10, Some(&format)).await.unwrap();
    let rows: Vec<(i64, Option<String>, Option<i64>)> =
        sqlx::query_as("SELECT id, video_format_id, video_height FROM videos ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        [
            (10, Some("400".to_string()), Some(1440)),
            (11, Some("400".to_string()), Some(1440)),
            (12, None, None),
        ]
    );
}

/// A row's id and four of its V34 columns.
type FormatRow = (
    i64,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<f64>,
);

/// No format known (no line, or a peer's copy): every column of the video
/// goes NULL, so no row keeps the format of files that were replaced.
#[tokio::test]
async fn an_unknown_format_clears_every_column_of_the_video() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'a', 'u1'), (2, 'b', 'u2')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, video_format_id, video_codec, \
         video_width, video_height, video_fps) VALUES \
         (10, 1, 'PySFfTurafA', '400', 'av01', 2560, 1440, 24.0), \
         (11, 2, 'PySFfTurafA', '400', 'av01', 2560, 1440, 24.0), \
         (12, 1, 'otherotheri', '248', 'vp9', 1920, 1080, 25.0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    record(&pool, 11, None).await.unwrap();
    let rows: Vec<FormatRow> = sqlx::query_as(
        "SELECT id, video_format_id, video_codec, video_height, video_fps \
             FROM videos ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            (10, None, None, None, None),
            (11, None, None, None, None),
            (
                12,
                Some("248".into()),
                Some("vp9".into()),
                Some(1080),
                Some(25.0)
            ),
        ]
    );
}
