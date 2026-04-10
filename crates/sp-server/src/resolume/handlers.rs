//! Resolume title show/hide with opacity fade.

use std::time::Duration;

use tracing::debug;

use crate::resolume::driver::HostDriver;

const TEXT_SETTLE_MS: u64 = 35;
const FADE_DURATION_MS: u64 = 1000;
const FADE_STEPS: u32 = 20;

/// Format title text matching legacy Python behavior.
pub fn format_title_text(song: &str, artist: &str, gemini_failed: bool) -> String {
    let text = match (song.is_empty(), artist.is_empty()) {
        (false, false) => format!("{song} - {artist}"),
        (false, true) => song.to_string(),
        (true, false) => artist.to_string(),
        (true, true) => String::new(),
    };
    if !text.is_empty() && gemini_failed {
        format!("{text} \u{26A0}")
    } else {
        text
    }
}

/// Generate n evenly-spaced opacity values from step/n to 1.0.
pub fn fade_steps(n: u32) -> Vec<f64> {
    (1..=n).map(|i| i as f64 / n as f64).collect()
}

/// Show title: set text, wait for texture, fade opacity 0->1.
pub async fn show_title(
    driver: &mut HostDriver,
    resolume_title_token: &str,
    song: &str,
    artist: &str,
    gemini_failed: bool,
) -> Result<(), anyhow::Error> {
    let clip = driver
        .clip_mapping
        .get(resolume_title_token)
        .and_then(|v| v.first())
        .ok_or_else(|| anyhow::anyhow!("no clip found for token {resolume_title_token}"))?
        .clone();

    let text = format_title_text(song, artist, gemini_failed);
    if text.is_empty() {
        return Ok(());
    }

    driver.set_text(clip.text_param_id, &text).await?;
    debug!(token = %resolume_title_token, %text, "set title text");

    tokio::time::sleep(Duration::from_millis(TEXT_SETTLE_MS)).await;

    let step_delay = Duration::from_millis(FADE_DURATION_MS / FADE_STEPS as u64);
    for opacity in fade_steps(FADE_STEPS) {
        driver.set_clip_opacity(clip.clip_id, opacity).await?;
        tokio::time::sleep(step_delay).await;
    }

    debug!(token = %resolume_title_token, "title fade-in complete");
    Ok(())
}

/// Hide title: fade opacity 1->0, then clear text.
pub async fn hide_title(
    driver: &mut HostDriver,
    resolume_title_token: &str,
) -> Result<(), anyhow::Error> {
    let clip = driver
        .clip_mapping
        .get(resolume_title_token)
        .and_then(|v| v.first())
        .ok_or_else(|| anyhow::anyhow!("no clip found for token {resolume_title_token}"))?
        .clone();

    let step_delay = Duration::from_millis(FADE_DURATION_MS / FADE_STEPS as u64);
    let steps = fade_steps(FADE_STEPS);
    for opacity in steps.iter().rev() {
        driver.set_clip_opacity(clip.clip_id, *opacity).await?;
        tokio::time::sleep(step_delay).await;
    }
    driver.set_clip_opacity(clip.clip_id, 0.0).await?;

    driver.set_text(clip.text_param_id, "").await?;

    debug!(token = %resolume_title_token, "title fade-out complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_title_text_song_and_artist() {
        assert_eq!(
            format_title_text("My Song", "Artist", false),
            "My Song - Artist"
        );
    }

    #[test]
    fn format_title_text_song_only() {
        assert_eq!(format_title_text("My Song", "", false), "My Song");
    }

    #[test]
    fn format_title_text_artist_only() {
        assert_eq!(format_title_text("", "Artist", false), "Artist");
    }

    #[test]
    fn format_title_text_empty() {
        assert_eq!(format_title_text("", "", false), "");
    }

    #[test]
    fn format_title_text_gemini_failed() {
        assert_eq!(
            format_title_text("My Song", "Artist", true),
            "My Song - Artist \u{26A0}"
        );
    }

    #[test]
    fn format_title_text_gemini_failed_empty_no_warning() {
        assert_eq!(format_title_text("", "", true), "");
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
}
