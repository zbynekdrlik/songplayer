//! Cache scanning and cleanup — manages normalized song files on disk.
//!
//! The pipeline stores each processed song as two sidecar files that share
//! a common base name:
//!
//! ```text
//! {safe_song}_{safe_artist}_{video_id}_normalized[_gf]_video.mp4
//! {safe_song}_{safe_artist}_{video_id}_normalized[_gf]_audio.flac
//! ```
//!
//! `scan_cache` walks the directory and returns three disjoint sets:
//!
//! * [`ScanResult::songs`] — complete video+audio pairs.
//! * [`ScanResult::legacy`] — pre-migration single `.mp4` files (these are
//!   deleted by the self-healing startup scan).
//! * [`ScanResult::orphans`] — unpaired half-sidecars from a crashed mid
//!   download (these are deleted by `cleanup_removed`).

use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// A complete, processed song present in the cache.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedSong {
    pub video_id: String,
    pub song: String,
    pub artist: String,
    pub gemini_failed: bool,
    pub video_path: PathBuf,
    pub audio_path: PathBuf,
}

/// A single-file legacy `.mp4` from before the FLAC migration.
#[derive(Debug, Clone, PartialEq)]
pub struct LegacyFile {
    pub video_id: String,
    pub gemini_failed: bool,
    pub path: PathBuf,
}

/// An unpaired sidecar (video without audio, or audio without video).
#[derive(Debug, Clone, PartialEq)]
pub struct Orphan {
    pub video_id: String,
    pub path: PathBuf,
}

/// Result of walking the cache directory once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanResult {
    pub songs: Vec<CachedSong>,
    pub legacy: Vec<LegacyFile>,
    pub orphans: Vec<Orphan>,
    /// Lyrics sidecar files: `(youtube_id, path)`.
    pub lyrics_files: Vec<(String, PathBuf)>,
}

static VIDEO_ID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_-]{11}$").unwrap());

static SPLIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+)_(.+)_([a-zA-Z0-9_-]{11})_normalized(_gf)?_(video|audio)\.(mp4|flac)$")
        .unwrap()
});

static LEGACY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+)_(.+)_([a-zA-Z0-9_-]{11})_normalized(_gf)?\.mp4$").unwrap());

static LYRICS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-zA-Z0-9_-]{11})_lyrics\.json$").unwrap());

/// Build the output filename for the video sidecar.
pub fn video_filename(song: &str, artist: &str, video_id: &str, gemini_failed: bool) -> String {
    let safe_song = sanitize_filename(song);
    let safe_artist = sanitize_filename(artist);
    let gf = if gemini_failed { "_gf" } else { "" };
    format!("{safe_song}_{safe_artist}_{video_id}_normalized{gf}_video.mp4")
}

/// Build the output filename for the audio sidecar.
pub fn audio_filename(song: &str, artist: &str, video_id: &str, gemini_failed: bool) -> String {
    let safe_song = sanitize_filename(song);
    let safe_artist = sanitize_filename(artist);
    let gf = if gemini_failed { "_gf" } else { "" };
    format!("{safe_song}_{safe_artist}_{video_id}_normalized{gf}_audio.flac")
}

/// Walk the cache directory and categorise every matching file.
pub fn scan_cache(cache_dir: &Path) -> ScanResult {
    let entries = match std::fs::read_dir(cache_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("cannot read cache dir {}: {e}", cache_dir.display());
            return ScanResult::default();
        }
    };

    // Temporary buckets per video_id for pairing.
    let mut video_half: HashMap<String, (String, String, bool, PathBuf)> = HashMap::new();
    let mut audio_half: HashMap<String, (String, String, bool, PathBuf)> = HashMap::new();
    let mut legacy: Vec<LegacyFile> = Vec::new();
    let mut lyrics_files: Vec<(String, PathBuf)> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if let Some(caps) = SPLIT_RE.captures(filename) {
            let song = caps[1].to_string();
            let artist = caps[2].to_string();
            let vid = caps[3].to_string();
            let gf = caps.get(4).is_some();
            let kind = &caps[5];
            let slot = (song, artist, gf, path.clone());
            if kind == "video" {
                video_half.insert(vid, slot);
            } else {
                audio_half.insert(vid, slot);
            }
            continue;
        }

        if let Some(caps) = LEGACY_RE.captures(filename) {
            legacy.push(LegacyFile {
                video_id: caps[3].to_string(),
                gemini_failed: caps.get(4).is_some(),
                path,
            });
            continue;
        }

        if let Some(caps) = LYRICS_RE.captures(filename) {
            lyrics_files.push((caps[1].to_string(), path));
            continue;
        }
    }

    // Pair video + audio halves by video_id.
    let mut songs: Vec<CachedSong> = Vec::new();
    let mut orphans: Vec<Orphan> = Vec::new();

    let video_ids: HashSet<String> = video_half.keys().cloned().collect();
    let audio_ids: HashSet<String> = audio_half.keys().cloned().collect();

    for vid in video_ids.intersection(&audio_ids) {
        let (song, artist, gf, v_path) = video_half.remove(vid).unwrap();
        let (_, _, _, a_path) = audio_half.remove(vid).unwrap();
        songs.push(CachedSong {
            video_id: vid.clone(),
            song,
            artist,
            gemini_failed: gf,
            video_path: v_path,
            audio_path: a_path,
        });
    }

    for (vid, (_, _, _, path)) in video_half.into_iter().chain(audio_half) {
        orphans.push(Orphan {
            video_id: vid,
            path,
        });
    }

    ScanResult {
        songs,
        legacy,
        orphans,
        lyrics_files,
    }
}

/// Delete song pairs whose video ID is not in `active_ids`, and always
/// preserve the currently playing video ID if supplied.
pub fn cleanup_removed(cache_dir: &Path, active_ids: &HashSet<String>, playing_id: Option<&str>) {
    let result = scan_cache(cache_dir);
    for song in result.songs {
        if active_ids.contains(&song.video_id) {
            continue;
        }
        if playing_id == Some(song.video_id.as_str()) {
            continue;
        }
        for path in [&song.video_path, &song.audio_path] {
            tracing::info!(
                "removing cached sidecar for removed video {}: {}",
                song.video_id,
                path.display()
            );
            if let Err(e) = std::fs::remove_file(path) {
                tracing::warn!("failed to remove {}: {e}", path.display());
            }
        }
    }
    // Orphans are always removed — they are debris from a crashed download.
    for orphan in result.orphans {
        tracing::info!(
            "removing orphan sidecar for {}: {}",
            orphan.video_id,
            orphan.path.display()
        );
        if let Err(e) = std::fs::remove_file(&orphan.path) {
            tracing::warn!("failed to remove orphan {}: {e}", orphan.path.display());
        }
    }
}

/// Delete every legacy single-file `.mp4` listed in `legacy`.
pub fn cleanup_legacy(legacy: &[LegacyFile]) {
    for item in legacy {
        tracing::info!(
            "deleting legacy AAC file for {}: {}",
            item.video_id,
            item.path.display()
        );
        if let Err(e) = std::fs::remove_file(&item.path) {
            tracing::warn!("failed to remove legacy file {}: {e}", item.path.display());
        }
    }
}

/// Sanitize a string for use inside a filename.
pub fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-')
        .collect();
    let collapsed: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
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
    fn video_filename_without_gf() {
        let name = video_filename("Amazing Grace", "Chris Tomlin", "dQw4w9WgXcQ", false);
        assert_eq!(
            name,
            "Amazing Grace_Chris Tomlin_dQw4w9WgXcQ_normalized_video.mp4"
        );
    }

    #[test]
    fn video_filename_with_gf() {
        let name = video_filename("Song", "Artist", "dQw4w9WgXcQ", true);
        assert_eq!(name, "Song_Artist_dQw4w9WgXcQ_normalized_gf_video.mp4");
    }

    #[test]
    fn audio_filename_without_gf() {
        let name = audio_filename("Amazing Grace", "Chris Tomlin", "dQw4w9WgXcQ", false);
        assert_eq!(
            name,
            "Amazing Grace_Chris Tomlin_dQw4w9WgXcQ_normalized_audio.flac"
        );
    }

    #[test]
    fn audio_filename_with_gf() {
        let name = audio_filename("Song", "Artist", "dQw4w9WgXcQ", true);
        assert_eq!(name, "Song_Artist_dQw4w9WgXcQ_normalized_gf_audio.flac");
    }

    #[test]
    fn scan_cache_pairs_video_and_audio() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        fs::write(
            base.join("Amazing Grace_Chris Tomlin_dQw4w9WgXcQ_normalized_video.mp4"),
            "fake video",
        )
        .unwrap();
        fs::write(
            base.join("Amazing Grace_Chris Tomlin_dQw4w9WgXcQ_normalized_audio.flac"),
            "fake audio",
        )
        .unwrap();

        let result = scan_cache(base);
        assert_eq!(result.songs.len(), 1);
        assert!(result.legacy.is_empty());
        assert!(result.orphans.is_empty());

        let song = &result.songs[0];
        assert_eq!(song.video_id, "dQw4w9WgXcQ");
        assert!(!song.gemini_failed);
        assert_eq!(song.song, "Amazing Grace");
        assert_eq!(song.artist, "Chris Tomlin");
    }

    #[test]
    fn scan_cache_flags_legacy_single_mp4() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path()
                .join("Old Song_Old Artist_xxxxxxxxxxx_normalized.mp4"),
            "legacy",
        )
        .unwrap();

        let result = scan_cache(dir.path());
        assert!(result.songs.is_empty());
        assert_eq!(result.legacy.len(), 1);
        assert_eq!(result.legacy[0].video_id, "xxxxxxxxxxx");
    }

    #[test]
    fn scan_cache_flags_legacy_gf_single_mp4() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Old_Song_xxxxxxxxxxx_normalized_gf.mp4"),
            "legacy gf",
        )
        .unwrap();

        let result = scan_cache(dir.path());
        assert_eq!(result.legacy.len(), 1);
        assert!(result.legacy[0].gemini_failed);
    }

    #[test]
    fn scan_cache_orphan_video_without_audio() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("S_A_aaaaaaaaaaa_normalized_video.mp4"), "v").unwrap();

        let result = scan_cache(dir.path());
        assert!(result.songs.is_empty());
        assert_eq!(result.orphans.len(), 1);
    }

    #[test]
    fn scan_cache_orphan_audio_without_video() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("S_A_bbbbbbbbbbb_normalized_audio.flac"),
            "a",
        )
        .unwrap();

        let result = scan_cache(dir.path());
        assert!(result.songs.is_empty());
        assert_eq!(result.orphans.len(), 1);
    }

    #[test]
    fn scan_cache_ignores_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.txt"), "ignore me").unwrap();
        fs::write(dir.path().join("xxxxxxxxxxx_temp.mp4"), "temp").unwrap();

        let result = scan_cache(dir.path());
        assert!(result.songs.is_empty());
        assert!(result.legacy.is_empty());
        assert!(result.orphans.is_empty());
    }

    #[test]
    fn cleanup_removed_deletes_both_files_of_a_pair() {
        let dir = tempfile::tempdir().unwrap();
        let v = dir.path().join("S_A_dQw4w9WgXcQ_normalized_video.mp4");
        let a = dir.path().join("S_A_dQw4w9WgXcQ_normalized_audio.flac");
        fs::write(&v, "v").unwrap();
        fs::write(&a, "a").unwrap();

        let active: HashSet<String> = HashSet::new();
        cleanup_removed(dir.path(), &active, None);
        assert!(!v.exists());
        assert!(!a.exists());
    }

    #[test]
    fn cleanup_removed_skips_currently_playing() {
        let dir = tempfile::tempdir().unwrap();
        let v = dir.path().join("S_A_xxxxxxxxxxx_normalized_video.mp4");
        let a = dir.path().join("S_A_xxxxxxxxxxx_normalized_audio.flac");
        fs::write(&v, "v").unwrap();
        fs::write(&a, "a").unwrap();

        let active: HashSet<String> = HashSet::new();
        cleanup_removed(dir.path(), &active, Some("xxxxxxxxxxx"));
        assert!(v.exists());
        assert!(a.exists());
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

    #[test]
    fn scan_cache_detects_lyrics_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("dQw4w9WgXcQ_lyrics.json"),
            r#"{"lines":[]}"#,
        )
        .unwrap();

        let result = scan_cache(dir.path());
        assert_eq!(result.lyrics_files.len(), 1);
        assert_eq!(result.lyrics_files[0].0, "dQw4w9WgXcQ");
        assert!(result.songs.is_empty());
        assert!(result.legacy.is_empty());
        assert!(result.orphans.is_empty());
    }

    #[test]
    fn scan_cache_ignores_non_matching_json() {
        let dir = tempfile::tempdir().unwrap();
        // Wrong suffix
        fs::write(dir.path().join("dQw4w9WgXcQ_meta.json"), "{}").unwrap();
        // Too long video id
        fs::write(dir.path().join("dQw4w9WgXcQXXX_lyrics.json"), "{}").unwrap();

        let result = scan_cache(dir.path());
        assert!(result.lyrics_files.is_empty());
    }
}
