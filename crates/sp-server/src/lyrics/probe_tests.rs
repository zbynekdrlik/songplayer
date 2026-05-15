//! Tests for the lyrics-source probe.
//!
//! Sibling file (not nested `#[cfg(test)] mod tests` inside probe.rs) per the
//! split-tests convention used elsewhere in the lyrics module — keeps probe.rs
//! under the file-size cap and lets each test file stay narrowly focused.

use super::probe::{ProbeReport, ProbeResult, probe_sources_impl};
use crate::ai::AiSettings;
use crate::ai::client::AiClient;
use crate::db::models::VideoLyricsRow;

fn fixture_row(song: &str, artist: &str, spotify_track_id: Option<&str>) -> VideoLyricsRow {
    VideoLyricsRow {
        id: 1,
        youtube_id: "ytid_test".into(),
        song: song.into(),
        artist: artist.into(),
        duration_ms: Some(180_000),
        audio_file_path: None,
        youtube_url: "https://www.youtube.com/playlist?list=X".into(),
        lyrics_override_text: None,
        lyrics_time_offset_ms: 0,
        spotify_track_id: spotify_track_id.map(String::from),
        spotify_resolved_at: None,
    }
}

#[tokio::test]
async fn probe_reports_any_text_source_false_when_all_providers_miss() {
    // All six probes miss: empty song/artist short-circuits the song-title
    // providers; no spotify_track_id; no genius token; bogus ytdlp path so
    // yt_subs + description return None.
    let report = probe_sources_impl(
        None, // ai_client: not used by probe (no Claude)
        std::path::Path::new("/nonexistent/ytdlp"),
        std::path::Path::new("/tmp"),
        &reqwest::Client::new(),
        &fixture_row("", "", None),
        "", // genius_access_token empty
    )
    .await;

    assert!(!report.any_text_source);
    assert_eq!(report.recommendation, "skip_no_text_source");
    assert!(report.probes.iter().all(|p| !p.available));
    let names: Vec<&str> = report.probes.iter().map(|p| p.provider.as_str()).collect();
    assert!(names.contains(&"yt_subs"));
    assert!(names.contains(&"description"));
    assert!(names.contains(&"lyrics_ovh"));
    assert!(names.contains(&"genius"));
    assert!(names.contains(&"lrclib"));
    assert!(names.contains(&"spotify"));
}

#[tokio::test]
async fn probe_reports_spotify_skipped_when_no_track_id() {
    let report = probe_sources_impl(
        None,
        std::path::Path::new("/nonexistent/ytdlp"),
        std::path::Path::new("/tmp"),
        &reqwest::Client::new(),
        &fixture_row("Song", "Artist", None),
        "",
    )
    .await;
    let spotify = report
        .probes
        .iter()
        .find(|p| p.provider == "spotify")
        .expect("spotify probe must be present");
    assert!(!spotify.available);
    assert!(spotify.note.to_lowercase().contains("no spotify_track_id"));
}

#[tokio::test]
async fn probe_reports_genius_skipped_when_no_token() {
    let report = probe_sources_impl(
        None,
        std::path::Path::new("/nonexistent/ytdlp"),
        std::path::Path::new("/tmp"),
        &reqwest::Client::new(),
        &fixture_row("Song", "Artist", None),
        "", // empty token
    )
    .await;
    let genius = report
        .probes
        .iter()
        .find(|p| p.provider == "genius")
        .expect("genius probe must be present");
    assert!(!genius.available);
    assert!(
        genius.note.to_lowercase().contains("no genius token")
            || genius.note.to_lowercase().contains("skipped")
    );
}

#[tokio::test]
async fn probe_reports_yt_subs_and_description_skipped_when_ytdlp_unavailable() {
    let report = probe_sources_impl(
        None,
        std::path::Path::new("/nonexistent/ytdlp"),
        std::path::Path::new("/tmp"),
        &reqwest::Client::new(),
        &fixture_row("Song", "Artist", None),
        "",
    )
    .await;
    let yt_subs = report
        .probes
        .iter()
        .find(|p| p.provider == "yt_subs")
        .unwrap();
    let descr = report
        .probes
        .iter()
        .find(|p| p.provider == "description")
        .unwrap();
    assert!(!yt_subs.available);
    assert!(!descr.available);
}

#[tokio::test]
async fn probe_description_unavailable_when_no_ai_client_present() {
    // Without ai_client the probe cannot Claude-validate description text;
    // YouTube descriptions are mostly promo/social copy with zero lyrics,
    // so the safe default is "unavailable" rather than the old "raw lines
    // count > 0 ⇒ available" heuristic that lied for id=175 Jesus Saves.
    let report = probe_sources_impl(
        None,
        std::path::Path::new("/nonexistent/ytdlp"),
        std::path::Path::new("/tmp"),
        &reqwest::Client::new(),
        &fixture_row("Song", "Artist", None),
        "",
    )
    .await;
    let descr = report
        .probes
        .iter()
        .find(|p| p.provider == "description")
        .unwrap();
    assert!(!descr.available);
    assert!(
        descr.note.to_lowercase().contains("no ai_client")
            || descr.note.to_lowercase().contains("claude-validate"),
        "expected note about missing Claude validation, got: {}",
        descr.note
    );
}

#[tokio::test]
async fn probe_description_reports_error_when_fetch_fails() {
    // #93 follow-up: cover the `Err(e)` arm in probe.rs:141-147.
    //
    // The Err path in `fetch_description_lyrics` fires when the
    // description cache write fails. Reproduce that by:
    //   1. Pre-seeding `{id}_description.txt` with empty content so
    //      `fetch_raw_description` hits the cache (no subprocess needed).
    //   2. Pre-creating `{id}_description_lyrics.json` as a *directory* so
    //      that the empty-description `write_lyrics_cache(None).await?`
    //      branch fails with "Is a directory" and propagates Err.
    //
    // The existing tests cover the `None` (no ai_client) and the
    // implicit `Ok(_)` (no lyrics) skip paths — this test pins the
    // third outcome that's been silently uncovered.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let cache_dir = tmp.path();
    let youtube_id = "ytid_test";

    // (1) Cached description, empty → fetch_raw_description returns
    // Ok(Some("")) → fetch_description_lyrics enters the
    // `description.trim().is_empty()` branch.
    tokio::fs::write(
        cache_dir.join(format!("{youtube_id}_description.txt")),
        "",
    )
    .await
    .unwrap();

    // (2) Sabotage the lyrics-cache JSON write by occupying its path
    // with a directory.
    tokio::fs::create_dir(
        cache_dir.join(format!("{youtube_id}_description_lyrics.json")),
    )
    .await
    .unwrap();

    let ai = AiClient::new(AiSettings::default());
    let report = probe_sources_impl(
        Some(&ai),
        std::path::Path::new("/nonexistent/ytdlp"),
        cache_dir,
        &reqwest::Client::new(),
        &fixture_row("Song", "Artist", None),
        "",
    )
    .await;

    let descr = report
        .probes
        .iter()
        .find(|p| p.provider == "description")
        .expect("description probe must be present");
    assert!(!descr.available);
    assert_eq!(descr.line_count, 0);
    assert!(
        descr.note.starts_with("error:"),
        "expected `error:` prefix when fetch_description_lyrics returns Err, got: {}",
        descr.note
    );
}

#[tokio::test]
async fn probe_emits_provider_url_for_every_provider() {
    let report = probe_sources_impl(
        None,
        std::path::Path::new("/nonexistent/ytdlp"),
        std::path::Path::new("/tmp"),
        &reqwest::Client::new(),
        &fixture_row("Jesus Saves", "Chris Tomlin", Some("4abcDeFg")),
        "",
    )
    .await;
    for p in &report.probes {
        assert!(
            !p.provider_url.is_empty(),
            "provider {} has empty provider_url",
            p.provider
        );
    }
    let url = |name: &str| {
        report
            .probes
            .iter()
            .find(|p| p.provider == name)
            .unwrap()
            .provider_url
            .clone()
    };
    assert!(url("yt_subs").starts_with("https://youtube.com/watch?v="));
    assert!(url("description").starts_with("https://youtube.com/watch?v="));
    assert!(
        url("lyrics_ovh").contains("api.lyrics.ovh")
            || url("lyrics_ovh") == "https://www.lyrics.ovh/"
    );
    assert!(url("genius").starts_with("https://genius.com/"));
    assert!(url("lrclib").starts_with("https://lrclib.net/"));
    assert!(url("spotify").starts_with("https://open.spotify.com/"));
}
