//! Cache scanning and cleanup — manages normalized video files on disk.

use regex::Regex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Metadata for a cached, normalized video file.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedVideo {
    pub video_id: String,
    pub file_path: PathBuf,
    pub song: String,
    pub artist: String,
    pub gemini_failed: bool,
}

/// Regex for YouTube video IDs: exactly 11 characters of `[a-zA-Z0-9_-]`.
static VIDEO_ID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_-]{11}$").unwrap());

/// Pattern: `{song}_{artist}_{videoId}_normalized[_gf].mp4`
///
/// The video ID is always the 11-char segment immediately before `_normalized`.
static CACHE_FILENAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+)_(.+)_([a-zA-Z0-9_-]{11})_normalized(_gf)?\.mp4$").unwrap());

/// Scan the cache directory for normalized video files.
///
/// Returns a [`CachedVideo`] for each file matching the naming convention.
pub fn scan_cache(cache_dir: &Path) -> Vec<CachedVideo> {
    let entries = match std::fs::read_dir(cache_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("cannot read cache dir {}: {e}", cache_dir.display());
            return Vec::new();
        }
    };

    let mut result = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(caps) = CACHE_FILENAME_RE.captures(filename) {
            result.push(CachedVideo {
                song: caps[1].to_string(),
                artist: caps[2].to_string(),
                video_id: caps[3].to_string(),
                gemini_failed: caps.get(4).is_some(),
                file_path: path,
            });
        }
    }
    result
}

/// Delete cached files whose video ID is not in `active_ids`.
///
/// The currently playing video (`playing_id`) is never deleted even if it
/// is not in the active set.
pub fn cleanup_removed(cache_dir: &Path, active_ids: &HashSet<String>, playing_id: Option<&str>) {
    let cached = scan_cache(cache_dir);
    for video in cached {
        if active_ids.contains(&video.video_id) {
            continue;
        }
        if playing_id == Some(video.video_id.as_str()) {
            tracing::debug!(
                "skipping currently playing video {} during cleanup",
                video.video_id
            );
            continue;
        }
        tracing::info!(
            "removing cached file for removed video {}: {}",
            video.video_id,
            video.file_path.display()
        );
        if let Err(e) = std::fs::remove_file(&video.file_path) {
            tracing::warn!("failed to remove {}: {e}", video.file_path.display());
        }
    }
}

/// Generate the output filename for a normalized video.
pub fn normalized_filename(
    song: &str,
    artist: &str,
    video_id: &str,
    gemini_failed: bool,
) -> String {
    let safe_song = sanitize_filename(song);
    let safe_artist = sanitize_filename(artist);
    let gf_suffix = if gemini_failed { "_gf" } else { "" };
    format!("{safe_song}_{safe_artist}_{video_id}_normalized{gf_suffix}.mp4")
}

/// Sanitize a string for use in filenames.
///
/// - Replaces non-alphanumeric characters (except spaces, hyphens) with empty string
/// - Collapses whitespace to a single space
/// - Trims and limits to 50 characters
pub fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-')
        .collect();

    // Collapse whitespace.
    let collapsed: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");

    // Trim to 50 chars (at a char boundary).
    let truncated = if collapsed.len() > 50 {
        let mut end = 50;
        while end > 0 && !collapsed.is_char_boundary(end) {
            end -= 1;
        }
        &collapsed[..end]
    } else {
        &collapsed
    };

    truncated.trim().to_string()
}

/// Check if a string looks like a valid YouTube video ID.
pub fn is_valid_video_id(s: &str) -> bool {
    VIDEO_ID_RE.is_match(s)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn sanitize_removes_special_chars() {
        assert_eq!(sanitize_filename("Hello World!"), "Hello World");
        assert_eq!(sanitize_filename("AC/DC"), "ACDC");
        assert_eq!(sanitize_filename("test@#$%^&*()file"), "testfile");
    }

    #[test]
    fn sanitize_collapses_whitespace() {
        assert_eq!(sanitize_filename("  hello   world  "), "hello world");
    }

    #[test]
    fn sanitize_limits_length() {
        let long = "a".repeat(100);
        let result = sanitize_filename(&long);
        assert!(result.len() <= 50);
    }

    #[test]
    fn sanitize_preserves_hyphens() {
        assert_eq!(sanitize_filename("hip-hop"), "hip-hop");
    }

    #[test]
    fn normalized_filename_without_gf() {
        let name = normalized_filename("Amazing Grace", "Chris Tomlin", "dQw4w9WgXcQ", false);
        assert_eq!(
            name,
            "Amazing Grace_Chris Tomlin_dQw4w9WgXcQ_normalized.mp4"
        );
    }

    #[test]
    fn normalized_filename_with_gf() {
        let name = normalized_filename("Song", "Artist", "dQw4w9WgXcQ", true);
        assert_eq!(name, "Song_Artist_dQw4w9WgXcQ_normalized_gf.mp4");
    }

    #[test]
    fn scan_cache_finds_matching_files() {
        let dir = tempfile::tempdir().unwrap();

        // Create test files.
        fs::write(
            dir.path()
                .join("Amazing Grace_Chris Tomlin_dQw4w9WgXcQ_normalized.mp4"),
            "fake video",
        )
        .unwrap();
        fs::write(
            dir.path().join("Song_Artist_xxxxxxxxxxx_normalized_gf.mp4"),
            "fake video",
        )
        .unwrap();
        // Non-matching files should be skipped.
        fs::write(dir.path().join("random_file.txt"), "not a video").unwrap();
        fs::write(dir.path().join("abc123_temp.mp4"), "temp download").unwrap();

        let cached = scan_cache(dir.path());
        assert_eq!(cached.len(), 2);

        let ids: HashSet<String> = cached.iter().map(|v| v.video_id.clone()).collect();
        assert!(ids.contains("dQw4w9WgXcQ"));
        assert!(ids.contains("xxxxxxxxxxx"));

        // Check gemini_failed flag.
        let gf_video = cached.iter().find(|v| v.video_id == "xxxxxxxxxxx").unwrap();
        assert!(gf_video.gemini_failed);

        let ok_video = cached.iter().find(|v| v.video_id == "dQw4w9WgXcQ").unwrap();
        assert!(!ok_video.gemini_failed);
    }

    #[test]
    fn scan_cache_extracts_song_and_artist() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path()
                .join("The Blessing_Elevation Worship_dQw4w9WgXcQ_normalized.mp4"),
            "",
        )
        .unwrap();

        let cached = scan_cache(dir.path());
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].song, "The Blessing");
        assert_eq!(cached[0].artist, "Elevation Worship");
    }

    #[test]
    fn scan_cache_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan_cache(dir.path()).is_empty());
    }

    #[test]
    fn cleanup_removes_inactive_files() {
        let dir = tempfile::tempdir().unwrap();

        let keep = "Keep_Song_dQw4w9WgXcQ_normalized.mp4";
        let remove = "Remove_Song_xxxxxxxxxxx_normalized.mp4";

        fs::write(dir.path().join(keep), "keep").unwrap();
        fs::write(dir.path().join(remove), "remove").unwrap();

        let active: HashSet<String> = ["dQw4w9WgXcQ".to_string()].into_iter().collect();
        cleanup_removed(dir.path(), &active, None);

        assert!(dir.path().join(keep).exists());
        assert!(!dir.path().join(remove).exists());
    }

    #[test]
    fn cleanup_skips_currently_playing() {
        let dir = tempfile::tempdir().unwrap();

        let playing = "Playing_Song_xxxxxxxxxxx_normalized.mp4";
        fs::write(dir.path().join(playing), "playing").unwrap();

        let active: HashSet<String> = HashSet::new(); // not in active set
        cleanup_removed(dir.path(), &active, Some("xxxxxxxxxxx"));

        // Should NOT be deleted because it is currently playing.
        assert!(dir.path().join(playing).exists());
    }

    #[test]
    fn is_valid_video_id_accepts_valid() {
        assert!(is_valid_video_id("dQw4w9WgXcQ"));
        assert!(is_valid_video_id("xxxxxxxxxxx"));
        assert!(is_valid_video_id("abc-def_123"));
    }

    #[test]
    fn is_valid_video_id_rejects_invalid() {
        assert!(!is_valid_video_id("short"));
        assert!(!is_valid_video_id("toolongstring123"));
        assert!(!is_valid_video_id("hello world"));
        assert!(!is_valid_video_id("abc!def@123"));
    }
}
