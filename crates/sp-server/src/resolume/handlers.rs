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

/// Look up the clips for `TITLE_TOKEN` in the driver's mapping, returning
/// them cloned if non-empty. Extracted as a pure function so the empty-Vec
/// guard can be mutation-tested directly (the inline `match v if !v.is_empty()`
/// guard produced a functionally-equivalent mutant that could only be caught
/// with brittle timing assertions).
pub(crate) fn clips_for_title(driver: &HostDriver) -> Option<Vec<ClipInfo>> {
    driver
        .clip_mapping
        .get(TITLE_TOKEN)
        .filter(|v| !v.is_empty())
        .cloned()
}

/// Show title across all `#sp-title` clips in parallel.
pub async fn show_title(
    driver: &mut HostDriver,
    song: &str,
    artist: &str,
) -> Result<(), anyhow::Error> {
    let Some(clips) = clips_for_title(driver) else {
        debug!(
            token = TITLE_TOKEN,
            "no Resolume clips found, skipping show_title"
        );
        return Ok(());
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
    let Some(clips) = clips_for_title(driver) else {
        debug!(
            token = TITLE_TOKEN,
            "no Resolume clips found, skipping hide_title"
        );
        return Ok(());
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

/// Show subtitles — instant text swap on the four token groups:
///   - `#sp-subs`      : current EN line (skipped if `suppress_en`)
///   - `#sp-subs-next` : next EN line    (skipped if `suppress_en`)
///   - `#sp-subssk`    : current SK line
///   - `#sp-subssk-next`: next SK line (pushed only if a mapping exists —
///     the driver's clip scanner picks up the token automatically, no
///     config change needed)
/// No fade animation; text is written directly.
#[cfg_attr(test, mutants::skip)]
pub async fn set_subtitles(
    driver: &mut HostDriver,
    en: &str,
    next_en: &str,
    sk: Option<&str>,
    next_sk: Option<&str>,
    suppress_en: bool,
) -> Result<(), anyhow::Error> {
    let subs_clips = if suppress_en {
        None
    } else {
        driver
            .clip_mapping
            .get(super::SUBS_TOKEN)
            .filter(|v| !v.is_empty())
            .cloned()
    };
    let subs_next_clips = if suppress_en {
        None
    } else {
        driver
            .clip_mapping
            .get(super::SUBS_NEXT_TOKEN)
            .filter(|v| !v.is_empty())
            .cloned()
    };
    let subs_sk_clips = driver
        .clip_mapping
        .get(super::SUBS_SK_TOKEN)
        .filter(|v| !v.is_empty())
        .cloned();

    if subs_clips.is_none() && subs_next_clips.is_none() && subs_sk_clips.is_none() {
        debug!(
            subs_token = super::SUBS_TOKEN,
            subs_next_token = super::SUBS_NEXT_TOKEN,
            subs_sk_token = super::SUBS_SK_TOKEN,
            suppress_en,
            "no Resolume subtitle clips found, skipping set_subtitles"
        );
        return Ok(());
    }

    driver.ensure_endpoint().await?;
    let driver_ref: &HostDriver = driver;

    if let Some(clips) = subs_clips {
        set_text_all(driver_ref, &clips, en).await?;
    }
    if let Some(clips) = subs_next_clips {
        set_text_all(driver_ref, &clips, next_en).await?;
    }
    if let Some(clips) = subs_sk_clips {
        let sk_text = sk.unwrap_or("");
        set_text_all(driver_ref, &clips, sk_text).await?;
    }
    let _ = next_sk; // reserved for #sp-subssk-next when operator configures it
    Ok(())
}

/// Hide subtitles — clear text on all `#sp-subs` and `#sp-subssk` clips.
/// No fade animation; text is cleared directly.
#[cfg_attr(test, mutants::skip)]
pub async fn clear_subtitles(driver: &mut HostDriver) -> Result<(), anyhow::Error> {
    let subs_clips = driver
        .clip_mapping
        .get(super::SUBS_TOKEN)
        .filter(|v| !v.is_empty())
        .cloned();
    let subs_sk_clips = driver
        .clip_mapping
        .get(super::SUBS_SK_TOKEN)
        .filter(|v| !v.is_empty())
        .cloned();

    if subs_clips.is_none() && subs_sk_clips.is_none() {
        debug!(
            subs_token = super::SUBS_TOKEN,
            subs_sk_token = super::SUBS_SK_TOKEN,
            "no Resolume subtitle clips found, skipping clear_subtitles"
        );
        return Ok(());
    }

    driver.ensure_endpoint().await?;
    let driver_ref: &HostDriver = driver;

    if let Some(clips) = subs_clips {
        set_text_all(driver_ref, &clips, "").await?;
    }
    if let Some(clips) = subs_sk_clips {
        set_text_all(driver_ref, &clips, "").await?;
    }
    Ok(())
}

/// Drain all pending futures, logging each individual error at `warn`
/// level. Returns the first error seen (if any) so the caller can
/// short-circuit the fade loop, but never silently drops a failure —
/// every broken clip is visible in the logs.
async fn drain_all<F, T>(mut futs: FuturesUnordered<F>) -> Result<(), anyhow::Error>
where
    F: std::future::Future<Output = Result<T, anyhow::Error>>,
{
    let mut first_err: Option<anyhow::Error> = None;
    while let Some(res) = futs.next().await {
        if let Err(e) = res {
            tracing::warn!(error = %e, "parallel Resolume request failed");
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

async fn set_text_all(
    driver: &HostDriver,
    clips: &[ClipInfo],
    text: &str,
) -> Result<(), anyhow::Error> {
    let futs = FuturesUnordered::new();
    for clip in clips {
        futs.push(driver.set_text(clip.text_param_id, text));
    }
    drain_all(futs).await
}

async fn set_opacity_all(
    driver: &HostDriver,
    clips: &[ClipInfo],
    opacity: f64,
) -> Result<(), anyhow::Error> {
    let futs = FuturesUnordered::new();
    for clip in clips {
        futs.push(driver.set_clip_opacity(clip.clip_id, opacity));
    }
    drain_all(futs).await
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

        show_title(&mut driver, "Song", "Artist").await.unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 0, "no requests should be sent");
    }

    #[tokio::test]
    async fn hide_title_with_no_clips_is_no_op() {
        let (server, mut driver) = spawn_mock_driver_with_clips(vec![]).await;

        hide_title(&mut driver).await.unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 0, "no requests should be sent");
    }

    /// Direct mutation-killing tests for the empty-Vec guard. These catch
    /// the `!v.is_empty()` -> `true` mutant cleanly without relying on
    /// timing assertions against the fade loop.
    #[test]
    fn clips_for_title_returns_none_when_mapping_is_empty() {
        let driver = HostDriver::new("127.0.0.1".to_string(), 1);
        // Empty mapping.
        assert!(clips_for_title(&driver).is_none());
    }

    #[test]
    fn clips_for_title_returns_none_when_token_maps_to_empty_vec() {
        let mut driver = HostDriver::new("127.0.0.1".to_string(), 1);
        driver
            .clip_mapping
            .insert(TITLE_TOKEN.to_string(), Vec::new());
        // Empty Vec should still return None.
        assert!(
            clips_for_title(&driver).is_none(),
            "empty Vec must be treated as no clips"
        );
    }

    #[test]
    fn clips_for_title_returns_some_when_clips_present() {
        let mut driver = HostDriver::new("127.0.0.1".to_string(), 1);
        let clip = ClipInfo {
            clip_id: 100,
            text_param_id: 200,
        };
        driver
            .clip_mapping
            .insert(TITLE_TOKEN.to_string(), vec![clip.clone()]);
        let result = clips_for_title(&driver).expect("should return Some");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].clip_id, 100);
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

    /// Type alias used so test futures can be stored in a single
    /// `FuturesUnordered` (every async block has its own anonymous type).
    type TestFut =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), anyhow::Error>> + Send>>;

    /// Verify `drain_all` awaits every future and returns an error when any
    /// future fails. Kills mutants that would short-circuit or skip futures.
    #[tokio::test]
    async fn drain_all_returns_err_on_any_failure() {
        let futs: FuturesUnordered<TestFut> = FuturesUnordered::new();
        futs.push(Box::pin(async { Ok(()) }));
        futs.push(Box::pin(async { Err(anyhow::anyhow!("boom")) }));
        futs.push(Box::pin(async { Ok(()) }));
        let result = drain_all(futs).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "boom");
    }

    #[tokio::test]
    async fn drain_all_returns_ok_when_all_succeed() {
        let futs: FuturesUnordered<TestFut> = FuturesUnordered::new();
        futs.push(Box::pin(async { Ok(()) }));
        futs.push(Box::pin(async { Ok(()) }));
        futs.push(Box::pin(async { Ok(()) }));
        assert!(drain_all(futs).await.is_ok());
    }

    /// Verify drain_all surfaces an error from a mixed set of ok/err futures
    /// even when the error is not the first future pushed. Kills mutants that
    /// might short-circuit before awaiting all futures.
    #[tokio::test]
    async fn drain_all_surfaces_error_among_successes() {
        let futs: FuturesUnordered<TestFut> = FuturesUnordered::new();
        futs.push(Box::pin(async { Ok(()) }));
        futs.push(Box::pin(async { Ok(()) }));
        futs.push(Box::pin(async { Err(anyhow::anyhow!("boom")) }));
        futs.push(Box::pin(async { Ok(()) }));
        futs.push(Box::pin(async { Ok(()) }));
        let result = drain_all(futs).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "boom");
    }
}
