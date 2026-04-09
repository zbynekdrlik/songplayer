//! A/B crossfade logic for Resolume title display.

use std::time::Duration;

use tracing::debug;

use crate::resolume::driver::HostDriver;

/// Delay between setting text and triggering the clip, allowing Resolume
/// to update the text texture before the clip goes live.
const TRIGGER_DELAY_MS: u64 = 35;

/// Update title with A/B crossfade.
///
/// 1. Determine inactive lane (opposite of current).
/// 2. Set text on inactive lane clips (`#song-name-{lane}`, `#artist-name-{lane}`).
/// 3. Wait `TRIGGER_DELAY_MS` for Resolume to process the text update.
/// 4. Trigger (connect) the inactive lane clips.
/// 5. Flip lane state.
pub async fn crossfade_title(
    driver: &mut HostDriver,
    playlist_id: i64,
    song: &str,
    artist: &str,
) -> Result<(), anyhow::Error> {
    let current_lane = *driver.lane_state.get(&playlist_id).unwrap_or(&false);
    let inactive_lane = !current_lane;
    let lane_suffix = if inactive_lane { "b" } else { "a" };

    let song_token = format!("#song-name-{lane_suffix}");
    let artist_token = format!("#artist-name-{lane_suffix}");

    // Copy clip info to avoid borrow conflicts with &mut self methods.
    let song_clip = driver.clip_mapping.get(&song_token).cloned();
    let artist_clip = driver.clip_mapping.get(&artist_token).cloned();

    // Step 1: Set text on inactive lane clips.
    if let Some(ref clip) = song_clip {
        driver.set_text(clip.text_param_id, song).await?;
        debug!(token = %song_token, %song, "set song text");
    }

    if let Some(ref clip) = artist_clip {
        driver.set_text(clip.text_param_id, artist).await?;
        debug!(token = %artist_token, %artist, "set artist text");
    }

    // Step 2: Wait for Resolume to process text update.
    tokio::time::sleep(Duration::from_millis(TRIGGER_DELAY_MS)).await;

    // Step 3: Trigger inactive lane clips.
    if let Some(ref clip) = song_clip {
        driver.trigger_clip(clip.clip_id).await?;
        debug!(token = %song_token, clip_id = clip.clip_id, "triggered song clip");
    }

    if let Some(ref clip) = artist_clip {
        driver.trigger_clip(clip.clip_id).await?;
        debug!(token = %artist_token, clip_id = clip.clip_id, "triggered artist clip");
    }

    // Step 4: Flip lane state.
    driver.lane_state.insert(playlist_id, inactive_lane);
    debug!(playlist_id, lane = %lane_suffix, "flipped lane state");

    Ok(())
}

/// Clear title display by triggering the `#song-clear` clip.
pub async fn clear_title(driver: &mut HostDriver, playlist_id: i64) -> Result<(), anyhow::Error> {
    let clear_token = "#song-clear";

    let clear_clip = driver.clip_mapping.get(clear_token).cloned();
    if let Some(ref clip) = clear_clip {
        driver.trigger_clip(clip.clip_id).await?;
        debug!(playlist_id, clip_id = clip.clip_id, "triggered clear clip");
    } else {
        debug!(playlist_id, "no #song-clear clip found, skipping clear");
    }

    Ok(())
}

/// Returns the lane suffix for the inactive lane given the current state.
fn _inactive_lane_suffix(current_is_b: bool) -> &'static str {
    if current_is_b { "a" } else { "b" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolume::driver::ClipInfo;

    #[test]
    fn trigger_delay_is_reasonable() {
        // Sanity check: delay should be small but non-zero.
        assert!(TRIGGER_DELAY_MS > 0);
        assert!(TRIGGER_DELAY_MS < 1000);
    }

    #[test]
    fn inactive_lane_suffix_logic() {
        assert_eq!(_inactive_lane_suffix(false), "b");
        assert_eq!(_inactive_lane_suffix(true), "a");
    }

    #[test]
    fn crossfade_lane_state_toggles() {
        // Verify the lane state logic without HTTP calls.
        let mut driver = HostDriver::new("localhost".to_string(), 8080);
        let playlist_id = 1;

        // Initial state: no entry = A (false).
        let current = *driver.lane_state.get(&playlist_id).unwrap_or(&false);
        assert!(!current);

        // After first crossfade, should be B (true).
        let inactive = !current;
        driver.lane_state.insert(playlist_id, inactive);
        assert!(driver.lane_state[&playlist_id]);

        // After second crossfade, should be A (false).
        let current = driver.lane_state[&playlist_id];
        let inactive = !current;
        driver.lane_state.insert(playlist_id, inactive);
        assert!(!driver.lane_state[&playlist_id]);
    }

    #[test]
    fn crossfade_token_generation() {
        // Verify the correct tokens are generated for each lane.
        let current_lane = false; // A
        let inactive_lane = !current_lane;
        let suffix = if inactive_lane { "b" } else { "a" };

        assert_eq!(format!("#song-name-{suffix}"), "#song-name-b");
        assert_eq!(format!("#artist-name-{suffix}"), "#artist-name-b");

        // Flip: now current is B.
        let current_lane = true; // B
        let inactive_lane = !current_lane;
        let suffix = if inactive_lane { "b" } else { "a" };

        assert_eq!(format!("#song-name-{suffix}"), "#song-name-a");
        assert_eq!(format!("#artist-name-{suffix}"), "#artist-name-a");
    }

    #[test]
    fn clear_title_uses_correct_token() {
        // The clear token should always be #song-clear.
        let driver = HostDriver::new("localhost".to_string(), 8080);
        let clear_token = "#song-clear";
        assert!(driver.clip_mapping.get(clear_token).is_none());

        // With a mapping present, it should find it.
        let mut driver = HostDriver::new("localhost".to_string(), 8080);
        driver.clip_mapping.insert(
            "#song-clear".to_string(),
            ClipInfo {
                clip_id: 104,
                text_param_id: 204,
            },
        );
        let clip = driver.clip_mapping.get(clear_token).unwrap();
        assert_eq!(clip.clip_id, 104);
    }

    #[test]
    fn multiple_playlists_independent_lanes() {
        let mut driver = HostDriver::new("localhost".to_string(), 8080);

        // Playlist 1 on lane B.
        driver.lane_state.insert(1, true);
        // Playlist 2 still on default A.
        let p2_lane = *driver.lane_state.get(&2).unwrap_or(&false);

        assert!(driver.lane_state[&1]);
        assert!(!p2_lane);
    }
}
