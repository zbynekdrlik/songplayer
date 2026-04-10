//! Resolume title show/hide with parallel multi-clip opacity fade.

use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use tracing::debug;

use crate::resolume::TITLE_TOKEN;
use crate::resolume::driver::{ClipInfo, HostDriver};

/// Delay between writing the title text and starting the opacity fade.
/// Resolume Arena needs a brief moment to commit a parameter write before
/// the new text is reflected in the rendered clip; without this delay the
/// fade-in would briefly show the previous text. 35 ms is the empirical
/// minimum that avoids the flash on Resolume Arena 7.x.
const TEXT_SETTLE_MS: u64 = 35;

/// Total fade duration (in/out) in milliseconds.
const FADE_DURATION_MS: u64 = 1000;

/// Number of opacity steps in the fade. 20 steps × 50ms = 1000ms total,
/// matching the legacy Python title fade timing.
const FADE_STEPS: u32 = 20;

/// Format title text matching legacy Python behavior — clean `Song - Artist`.
/// No warning indicator (gemini_failed is no longer surfaced in titles).
pub fn format_title_text(song: &str, artist: &str) -> String {
    match (song.is_empty(), artist.is_empty()) {
        (false, false) => format!("{song} - {artist}"),
        (false, true) => song.to_string(),
        (true, false) => artist.to_string(),
        (true, true) => String::new(),
    }
}

/// Generate `n` evenly-spaced opacity values from `1/n` to `1.0` inclusive.
pub fn fade_steps(n: u32) -> Vec<f64> {
    (1..=n).map(|i| i as f64 / n as f64).collect()
}

/// Per-step delay for the fade loop (FADE_DURATION_MS / FADE_STEPS).
///
/// Skipped from mutation testing: the `/` → `*` mutant produces a 20s
/// per-step delay, which makes `show_title` / `hide_title` wiremock tests
/// take ~400s and exceed cargo-mutants' 300s test timeout. The math itself
/// is asserted by `fade_step_delay_is_50_milliseconds` below, so the
/// behavior is covered without needing the mutation operator.
#[cfg_attr(test, mutants::skip)]
pub fn fade_step_delay() -> Duration {
    Duration::from_millis(FADE_DURATION_MS / FADE_STEPS as u64)
}

/// Show title across all `#sp-title` clips in parallel.
pub async fn show_title(
    driver: &mut HostDriver,
    song: &str,
    artist: &str,
) -> Result<(), anyhow::Error> {
    let clips: Vec<ClipInfo> = match driver.clip_mapping.get(TITLE_TOKEN) {
        Some(v) if !v.is_empty() => v.clone(),
        _ => {
            debug!(
                token = TITLE_TOKEN,
                "no Resolume clips found, skipping show_title"
            );
            return Ok(());
        }
    };

    let text = format_title_text(song, artist);
    if text.is_empty() {
        return Ok(());
    }

    driver.ensure_endpoint().await?;
    let driver_ref: &HostDriver = driver;

    set_text_all(driver_ref, &clips, &text).await?;
    debug!(
        token = TITLE_TOKEN,
        count = clips.len(),
        %text,
        "set title text on all clips"
    );

    tokio::time::sleep(Duration::from_millis(TEXT_SETTLE_MS)).await;

    let step_delay = fade_step_delay();
    for opacity in fade_steps(FADE_STEPS) {
        set_opacity_all(driver_ref, &clips, opacity).await?;
        tokio::time::sleep(step_delay).await;
    }

    debug!(
        token = TITLE_TOKEN,
        count = clips.len(),
        "title fade-in complete"
    );
    Ok(())
}

/// Hide title across all `#sp-title` clips in parallel.
pub async fn hide_title(driver: &mut HostDriver) -> Result<(), anyhow::Error> {
    let clips: Vec<ClipInfo> = match driver.clip_mapping.get(TITLE_TOKEN) {
        Some(v) if !v.is_empty() => v.clone(),
        _ => {
            debug!(
                token = TITLE_TOKEN,
                "no Resolume clips found, skipping hide_title"
            );
            return Ok(());
        }
    };

    driver.ensure_endpoint().await?;
    let driver_ref: &HostDriver = driver;

    let step_delay = fade_step_delay();
    let steps: Vec<f64> = fade_steps(FADE_STEPS);
    for opacity in steps.iter().rev() {
        set_opacity_all(driver_ref, &clips, *opacity).await?;
        tokio::time::sleep(step_delay).await;
    }
    set_opacity_all(driver_ref, &clips, 0.0).await?;

    set_text_all(driver_ref, &clips, "").await?;

    debug!(
        token = TITLE_TOKEN,
        count = clips.len(),
        "title fade-out complete"
    );
    Ok(())
}

async fn set_text_all(
    driver: &HostDriver,
    clips: &[ClipInfo],
    text: &str,
) -> Result<(), anyhow::Error> {
    let mut futs = FuturesUnordered::new();
    for clip in clips {
        futs.push(driver.set_text(clip.text_param_id, text));
    }
    while let Some(res) = futs.next().await {
        res?;
    }
    Ok(())
}

async fn set_opacity_all(
    driver: &HostDriver,
    clips: &[ClipInfo],
    opacity: f64,
) -> Result<(), anyhow::Error> {
    let mut futs = FuturesUnordered::new();
    for clip in clips {
        futs.push(driver.set_clip_opacity(clip.clip_id, opacity));
    }
    while let Some(res) = futs.next().await {
        res?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_title_text_song_and_artist() {
        assert_eq!(format_title_text("My Song", "Artist"), "My Song - Artist");
    }

    #[test]
    fn format_title_text_song_only() {
        assert_eq!(format_title_text("My Song", ""), "My Song");
    }

    #[test]
    fn format_title_text_artist_only() {
        assert_eq!(format_title_text("", "Artist"), "Artist");
    }

    #[test]
    fn format_title_text_empty() {
        assert_eq!(format_title_text("", ""), "");
    }

    #[test]
    fn format_title_text_no_warning_indicator_anywhere() {
        let result = format_title_text("Song", "Artist");
        assert!(!result.contains('\u{26A0}'));
        assert!(!result.contains('⚠'));
    }

    #[test]
    fn fade_steps_20_steps_over_1s() {
        let steps = fade_steps(20);
        assert_eq!(steps.len(), 20);
        assert!((steps[0] - 0.05).abs() < 0.001);
        assert!((steps[19] - 1.0).abs() < 0.001);
    }

    #[test]
    fn fade_steps_values_are_monotonically_increasing() {
        let steps = fade_steps(20);
        for i in 1..steps.len() {
            assert!(steps[i] > steps[i - 1]);
        }
    }

    /// Verify the per-step delay is 50ms (1000 / 20).
    /// Kills `/` → `%` mutant in `fade_step_delay` (which would yield
    /// 1000 % 20 = 0ms instead of 50ms).
    #[test]
    fn fade_step_delay_is_50_milliseconds() {
        assert_eq!(fade_step_delay(), Duration::from_millis(50));
    }

    // -----------------------------------------------------------------------
    // Wiremock-based HTTP integration tests
    // -----------------------------------------------------------------------

    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    /// Spawn a mock Resolume server with the given clip mapping prepopulated
    /// in the driver. Returns the wiremock URL and a HostDriver pointed at it.
    async fn spawn_mock_driver_with_clips(clips: Vec<ClipInfo>) -> (MockServer, HostDriver) {
        let server = MockServer::start().await;
        // Parse the wiremock URL into host:port
        let url = server.uri();
        let stripped = url.trim_start_matches("http://");
        let parts: Vec<&str> = stripped.split(':').collect();
        let host = parts[0].to_string();
        let port: u16 = parts[1].parse().unwrap();

        let mut driver = HostDriver::new(host, port);
        // Pre-populate the mapping (bypasses refresh_mapping which would
        // otherwise need a /api/v1/composition mock)
        driver.clip_mapping.insert(TITLE_TOKEN.to_string(), clips);
        driver.ensure_endpoint().await.unwrap();
        (server, driver)
    }

    #[tokio::test]
    async fn show_title_sets_text_then_fades_in_on_single_clip() {
        let clips = vec![ClipInfo {
            clip_id: 100,
            text_param_id: 200,
        }];
        let (server, mut driver) = spawn_mock_driver_with_clips(clips).await;

        // Mock the text param PUT — capture the body to verify clean text.
        Mock::given(method("PUT"))
            .and(path("/api/v1/parameter/by-id/200"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        // Mock the clip opacity PUT (matches any opacity value).
        Mock::given(method("PUT"))
            .and(path("/api/v1/composition/clips/by-id/100"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        show_title(&mut driver, "My Song", "Artist Name")
            .await
            .expect("show_title should succeed");

        // Verify the text was set with clean format (no warning).
        let received = server.received_requests().await.unwrap();
        let text_req = received
            .iter()
            .find(|r| r.url.path() == "/api/v1/parameter/by-id/200")
            .expect("text PUT must be sent");
        let body: serde_json::Value = serde_json::from_slice(&text_req.body).unwrap();
        assert_eq!(body["value"], "My Song - Artist Name");
        let body_str = body["value"].as_str().unwrap();
        assert!(!body_str.contains('\u{26A0}'), "title must not contain ⚠");

        // Verify 20 opacity PUTs (one per fade step) were sent.
        let opacity_count = received
            .iter()
            .filter(|r| r.url.path() == "/api/v1/composition/clips/by-id/100")
            .count();
        assert_eq!(
            opacity_count, 20,
            "expected 20 opacity PUTs for 20-step fade-in, got {opacity_count}"
        );
    }

    #[tokio::test]
    async fn show_title_updates_all_clips_in_parallel() {
        let clips = vec![
            ClipInfo {
                clip_id: 100,
                text_param_id: 200,
            },
            ClipInfo {
                clip_id: 101,
                text_param_id: 201,
            },
            ClipInfo {
                clip_id: 102,
                text_param_id: 202,
            },
        ];
        let (server, mut driver) = spawn_mock_driver_with_clips(clips).await;

        // Match all parameter and clip-by-id requests with regex.
        Mock::given(method("PUT"))
            .and(path_regex(r"^/api/v1/parameter/by-id/\d+$"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/api/v1/composition/clips/by-id/\d+$"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        show_title(&mut driver, "Song", "Artist").await.unwrap();

        let received = server.received_requests().await.unwrap();

        // Each text param (200, 201, 202) should have been written once.
        for param in &[200, 201, 202] {
            let count = received
                .iter()
                .filter(|r| r.url.path() == format!("/api/v1/parameter/by-id/{param}"))
                .count();
            assert_eq!(count, 1, "text param {param} should be set once");
        }

        // Each clip (100, 101, 102) should have 20 opacity writes.
        for clip in &[100, 101, 102] {
            let count = received
                .iter()
                .filter(|r| r.url.path() == format!("/api/v1/composition/clips/by-id/{clip}"))
                .count();
            assert_eq!(count, 20, "clip {clip} should have 20 opacity steps");
        }
    }

    #[tokio::test]
    async fn hide_title_fades_out_then_clears_text() {
        let clips = vec![ClipInfo {
            clip_id: 100,
            text_param_id: 200,
        }];
        let (server, mut driver) = spawn_mock_driver_with_clips(clips).await;

        Mock::given(method("PUT"))
            .and(path("/api/v1/parameter/by-id/200"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/composition/clips/by-id/100"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        hide_title(&mut driver).await.unwrap();

        let received = server.received_requests().await.unwrap();

        // Opacity: 20 fade steps + 1 final 0.0 = 21 PUTs.
        let opacity_count = received
            .iter()
            .filter(|r| r.url.path() == "/api/v1/composition/clips/by-id/100")
            .count();
        assert_eq!(
            opacity_count, 21,
            "expected 21 opacity PUTs (20 fade + 1 final zero), got {opacity_count}"
        );

        // Text should be cleared to empty string at the end.
        let text_reqs: Vec<&Request> = received
            .iter()
            .filter(|r| r.url.path() == "/api/v1/parameter/by-id/200")
            .collect();
        assert_eq!(text_reqs.len(), 1, "text should be cleared exactly once");
        let body: serde_json::Value = serde_json::from_slice(&text_reqs[0].body).unwrap();
        assert_eq!(body["value"], "");
    }

    #[tokio::test]
    async fn show_title_with_no_clips_is_no_op() {
        let (server, mut driver) = spawn_mock_driver_with_clips(vec![]).await;

        // No mocks needed - we expect zero requests.
        // ALSO: verify the function returns quickly (early-return path), not
        // after running the full ~1-second fade loop on an empty clip list.
        // This kills mutants that turn the empty-Vec guard into `true` (which
        // would proceed through the fade loop with empty data, taking ~1s).
        let start = std::time::Instant::now();
        show_title(&mut driver, "Song", "Artist").await.unwrap();
        let elapsed = start.elapsed();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 0, "no requests should be sent");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "show_title with no clips must early-return, not run the fade loop. Took: {elapsed:?}"
        );
    }

    /// Same timing-based mutation kill for `hide_title`'s empty-Vec guard.
    #[tokio::test]
    async fn hide_title_with_no_clips_is_no_op() {
        let (server, mut driver) = spawn_mock_driver_with_clips(vec![]).await;

        let start = std::time::Instant::now();
        hide_title(&mut driver).await.unwrap();
        let elapsed = start.elapsed();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 0, "no requests should be sent");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "hide_title with no clips must early-return, not run the fade loop. Took: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn show_title_with_empty_text_is_no_op() {
        let clips = vec![ClipInfo {
            clip_id: 100,
            text_param_id: 200,
        }];
        let (server, mut driver) = spawn_mock_driver_with_clips(clips).await;

        // No mocks - empty text should produce no requests.
        show_title(&mut driver, "", "").await.unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 0, "empty text should send no requests");
    }

    #[tokio::test]
    async fn set_text_without_ensure_endpoint_returns_error() {
        // Create driver but DON'T call ensure_endpoint.
        let driver = HostDriver::new("127.0.0.1".to_string(), 1);

        let result = driver.set_text(123, "test").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("endpoint cache empty"),
            "error should mention endpoint cache, got: {err}"
        );
    }

    #[tokio::test]
    async fn set_clip_opacity_without_ensure_endpoint_returns_error() {
        let driver = HostDriver::new("127.0.0.1".to_string(), 1);

        let result = driver.set_clip_opacity(123, 0.5).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("endpoint cache empty"));
    }
}
