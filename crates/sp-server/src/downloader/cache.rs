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
//!   download (these are deleted by the self-healing startup scan).

use regex::Regex;
use std::collections::HashMap;
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
    /// `(youtube_id, path)` for every preprocess-vocals output found in
    /// the cache directory. Persisted across alignment runs (see #41) so
    /// reprocess reuses Demucs output via aligner.rs cache-hit logic.
    pub vocals_files: Vec<(String, PathBuf)>,
    /// Complete pairs for a video id that already has a NEWER complete pair
    /// (a re-download under different metadata left the old pair behind).
    /// `songs` keeps the newest pair per id; these are removed by
    /// [`remove_duplicates`] at startup.
    pub duplicates: Vec<CachedSong>,
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

static VOCALS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-zA-Z0-9_-]{11})_vocals16k\.wav$").unwrap());

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

    // Temporary buckets per BASE name (`{song}_{artist}_{id}_normalized[_gf]`)
    // for pairing: a video pairs only with the audio of the same base, so two
    // downloads of one id under different metadata never cross-pair.
    let mut halves: HashMap<String, Halves> = HashMap::new();
    let mut legacy: Vec<LegacyFile> = Vec::new();
    let mut lyrics_files: Vec<(String, PathBuf)> = Vec::new();
    let mut vocals_files: Vec<(String, PathBuf)> = Vec::new();

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
            let base = filename
                .strip_suffix(if kind == "video" {
                    "_video.mp4"
                } else {
                    "_audio.flac"
                })
                .unwrap_or(filename)
                .to_string();
            let h = halves.entry(base).or_insert_with(|| Halves {
                video_id: vid,
                song,
                artist,
                gemini_failed: gf,
                video: None,
                audio: None,
            });
            if kind == "video" {
                h.video = Some(path.clone());
            } else {
                h.audio = Some(path.clone());
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

        if let Some(caps) = VOCALS_RE.captures(filename) {
            vocals_files.push((caps[1].to_string(), path));
            continue;
        }
    }

    // Pair video + audio halves of the same base; a lone half is an orphan.
    let mut complete: HashMap<String, Vec<CachedSong>> = HashMap::new();
    let mut orphans: Vec<Orphan> = Vec::new();
    for h in halves.into_values() {
        match (h.video, h.audio) {
            (Some(video_path), Some(audio_path)) => {
                complete
                    .entry(h.video_id.clone())
                    .or_default()
                    .push(CachedSong {
                        video_id: h.video_id,
                        song: h.song,
                        artist: h.artist,
                        gemini_failed: h.gemini_failed,
                        video_path,
                        audio_path,
                    });
            }
            (Some(path), None) | (None, Some(path)) => orphans.push(Orphan {
                video_id: h.video_id,
                path,
            }),
            (None, None) => {}
        }
    }

    // One keeper per id: the newest pair (by the video file's mtime); every
    // older complete pair of that id is a superseded duplicate.
    let mut songs: Vec<CachedSong> = Vec::new();
    let mut duplicates: Vec<CachedSong> = Vec::new();
    for (_, mut pairs) in complete {
        pairs.sort_by_key(|p| std::cmp::Reverse(modified(&p.video_path)));
        let mut pairs = pairs.into_iter();
        if let Some(keeper) = pairs.next() {
            songs.push(keeper);
        }
        duplicates.extend(pairs);
    }

    ScanResult {
        songs,
        legacy,
        orphans,
        lyrics_files,
        vocals_files,
        duplicates,
    }
}

/// The two halves of one base name seen by [`scan_cache`].
struct Halves {
    video_id: String,
    song: String,
    artist: String,
    gemini_failed: bool,
    video: Option<PathBuf>,
    audio: Option<PathBuf>,
}

/// A file's modification time (the epoch when unreadable, so it loses).
fn modified(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// Delete every superseded duplicate pair: its video, its audio and the
/// karaoke stems derived from that audio.
pub fn remove_duplicates(duplicates: &[CachedSong]) {
    for dup in duplicates {
        let (vocals, instrumental) = crate::stems::stem_paths(&dup.audio_path);
        tracing::info!(
            video_id = %dup.video_id,
            video = %dup.video_path.display(),
            "removing superseded duplicate cache pair (a newer pair of this id is kept)"
        );
        for path in [&dup.video_path, &dup.audio_path, &vocals, &instrumental] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("failed to remove duplicate {}: {e}", path.display()),
            }
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
    use std::collections::HashSet;
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

    #[test]
    fn scan_cache_picks_up_vocals_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("dQw4w9WgXcQ_vocals16k.wav"), "fake vocals").unwrap();
        fs::write(dir.path().join("aBcDeFgHiJk_vocals16k.wav"), "fake").unwrap();
        let result = scan_cache(dir.path());
        assert_eq!(result.vocals_files.len(), 2);
        let ids: HashSet<&str> = result
            .vocals_files
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert!(ids.contains("dQw4w9WgXcQ"));
        assert!(ids.contains("aBcDeFgHiJk"));
    }

    fn touch_at(path: &Path, secs_ago: u64) {
        fs::write(path, b"x").unwrap();
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    /// Box, 27.9.2026: `TwzfEwsTfag` had two complete pairs in the cache — an
    /// April pair under the artist "Indiana Bible College" and an August `_gf`
    /// pair under "Worthy" (with stems). The scan kept whichever video/audio
    /// half it met last per id, so it could even pair one base's video with
    /// the other base's audio, and the stale pair stayed forever (the A/V
    /// gate refused the ambiguity).
    #[test]
    fn two_complete_pairs_for_one_id_keep_the_newest_and_list_the_other() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let old_v =
            d.join("Never Lost Champion_Indiana Bible College_TwzfEwsTfag_normalized_video.mp4");
        let old_a =
            d.join("Never Lost Champion_Indiana Bible College_TwzfEwsTfag_normalized_audio.flac");
        let new_v = d.join("Never Lost Champion_Worthy_TwzfEwsTfag_normalized_gf_video.mp4");
        let new_a = d.join("Never Lost Champion_Worthy_TwzfEwsTfag_normalized_gf_audio.flac");
        touch_at(&old_v, 400_000);
        touch_at(&old_a, 400_000);
        touch_at(&new_v, 1_000);
        touch_at(&new_a, 1_000);

        let r = scan_cache(d);
        assert_eq!(r.songs.len(), 1, "one keeper per id");
        assert_eq!(r.songs[0].video_path, new_v, "the newest pair is kept");
        assert_eq!(r.songs[0].audio_path, new_a, "never a cross-paired half");
        assert_eq!(r.songs[0].artist, "Worthy");
        assert!(r.songs[0].gemini_failed);
        assert_eq!(r.duplicates.len(), 1);
        assert_eq!(r.duplicates[0].video_path, old_v);
        assert_eq!(r.duplicates[0].audio_path, old_a);
        assert_eq!(r.duplicates[0].video_id, "TwzfEwsTfag");
        assert!(r.orphans.is_empty(), "both pairs are complete: no orphan");
    }

    #[test]
    fn a_half_of_another_base_is_an_orphan_not_a_pair_partner() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let v = d.join("Song_ArtistA_dQw4w9WgXcQ_normalized_video.mp4");
        let a = d.join("Song_ArtistB_dQw4w9WgXcQ_normalized_audio.flac");
        touch_at(&v, 10);
        touch_at(&a, 10);
        let r = scan_cache(d);
        assert!(r.songs.is_empty(), "different bases never pair");
        assert_eq!(r.orphans.len(), 2);
        assert!(r.duplicates.is_empty());
    }

    #[test]
    fn remove_duplicates_deletes_the_pair_and_its_stems_only() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let dup_v = d.join("S_Old_TwzfEwsTfag_normalized_video.mp4");
        let dup_a = d.join("S_Old_TwzfEwsTfag_normalized_audio.flac");
        let dup_voc = d.join("S_Old_TwzfEwsTfag_normalized_audio_vocals.flac");
        let dup_ins = d.join("S_Old_TwzfEwsTfag_normalized_audio_instrumental.flac");
        let keep_v = d.join("S_New_TwzfEwsTfag_normalized_gf_video.mp4");
        let keep_a = d.join("S_New_TwzfEwsTfag_normalized_gf_audio.flac");
        let keep_voc = d.join("S_New_TwzfEwsTfag_normalized_gf_audio_vocals.flac");
        for p in [
            &dup_v, &dup_a, &dup_voc, &dup_ins, &keep_v, &keep_a, &keep_voc,
        ] {
            fs::write(p, b"x").unwrap();
        }
        remove_duplicates(&[CachedSong {
            video_id: "TwzfEwsTfag".into(),
            song: "S".into(),
            artist: "Old".into(),
            gemini_failed: false,
            video_path: dup_v.clone(),
            audio_path: dup_a.clone(),
        }]);
        for p in [&dup_v, &dup_a, &dup_voc, &dup_ins] {
            assert!(!p.exists(), "{} removed", p.display());
        }
        for p in [&keep_v, &keep_a, &keep_voc] {
            assert!(p.exists(), "{} kept", p.display());
        }
    }
}
