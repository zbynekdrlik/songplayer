//! Title-based metadata parser — port of Python `parse_title_smart()`.

use regex::Regex;
use sp_core::metadata::{MetadataSource, VideoMetadata};
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Compiled regexes (compiled once, reused)
// ---------------------------------------------------------------------------

/// Pattern 1: "Song | Artist" format.
static PIPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^([^|]+)\s*\|\s*([^|]+?)(?:\s*(?:Official|Music|Video|Live|feat\.|ft\.)|$)")
        .expect("PIPE_RE must compile")
});

/// Pattern 2: "Artist - Song" format.
static DASH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([^-]+?)\s*-\s*([^-]+?)(?:\s*\(|\s*\[|$)").expect("DASH_RE must compile")
});

// Cleaning regexes
static BRACKET_ROUND_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\([^)]*\)").expect("compile"));
static BRACKET_SQUARE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[[^\]]*\]").expect("compile"));
static BRACKET_CURLY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{[^}]*\}").expect("compile"));

static TRAILING_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)\s+feat\.?\s+.*$",
        r"(?i)\s+ft\.?\s+.*$",
        r"(?i)\s+featuring\s+.*$",
        r"(?i)\s+official\s*(?:music\s*)?video\s*$",
        r"(?i)\s+official\s*audio\s*$",
        r"(?i)\s+music\s*video\s*$",
        r"(?i)\s+live\s*$",
        r"(?i)\s+acoustic\s*$",
        r"(?i)\s+hd\s*$",
        r"(?i)\s+4k\s*$",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("trailing pattern must compile"))
    .collect()
});

static WHITESPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s{2,}").expect("compile"));
static TRAILING_JUNK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[,\-|\s]+$").expect("compile"));

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse a YouTube video title into song/artist metadata.
///
/// Tries two patterns in order (pipe-separated, then dash-separated).
/// Falls back to returning the full title as the song with "Unknown Artist".
pub fn parse_title(title: &str) -> VideoMetadata {
    let title = title.trim();
    if title.is_empty() {
        return VideoMetadata {
            song: String::new(),
            artist: "Unknown Artist".into(),
            source: MetadataSource::Regex,
            gemini_failed: false,
        };
    }

    // Pattern 1: "Song | Artist" (artist_first = false)
    if let Some(caps) = PIPE_RE.captures(title) {
        let song_raw = caps[1].trim().to_string();
        let artist = caps[2].trim().to_string();
        if artist.len() > 2 {
            let song = clean_song_title(&song_raw);
            return VideoMetadata {
                song,
                artist,
                source: MetadataSource::Regex,
                gemini_failed: false,
            };
        }
    }

    // Pattern 2: "Artist - Song" (artist_first = true)
    if let Some(caps) = DASH_RE.captures(title) {
        let artist = caps[1].trim().to_string();
        let song_raw = caps[2].trim().to_string();
        if artist.len() > 2 {
            let song = clean_song_title(&song_raw);
            return VideoMetadata {
                song,
                artist,
                source: MetadataSource::Regex,
                gemini_failed: false,
            };
        }
    }

    // No match — use full title as song name.
    VideoMetadata {
        song: title.to_string(),
        artist: "Unknown Artist".into(),
        source: MetadataSource::Regex,
        gemini_failed: false,
    }
}

/// Remove brackets, feat., "Official Video", etc. from a song title.
pub fn clean_song_title(song: &str) -> String {
    let original = song.trim().to_string();

    // Remove bracket content
    let mut cleaned = BRACKET_ROUND_RE.replace_all(&original, "").to_string();
    cleaned = BRACKET_SQUARE_RE.replace_all(&cleaned, "").to_string();
    cleaned = BRACKET_CURLY_RE.replace_all(&cleaned, "").to_string();

    // Remove trailing patterns
    for re in TRAILING_PATTERNS.iter() {
        cleaned = re.replace_all(&cleaned, "").to_string();
    }

    // Collapse whitespace
    cleaned = WHITESPACE_RE.replace_all(&cleaned, " ").to_string();

    // Strip trailing junk characters
    cleaned = TRAILING_JUNK_RE.replace_all(&cleaned, "").to_string();

    let cleaned = cleaned.trim().to_string();

    // If cleaning removed everything, return original
    if cleaned.is_empty() {
        original
    } else {
        cleaned
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_title tests ----

    #[test]
    fn pipe_format_basic() {
        let m = parse_title("HOLYGHOST | Sons Of Sunday");
        assert_eq!(m.song, "HOLYGHOST");
        assert_eq!(m.artist, "Sons Of Sunday");
        assert_eq!(m.source, MetadataSource::Regex);
        assert!(!m.gemini_failed);
    }

    #[test]
    fn dash_format_basic() {
        let m = parse_title("Elevation Worship - The Blessing");
        assert_eq!(m.song, "The Blessing");
        assert_eq!(m.artist, "Elevation Worship");
    }

    #[test]
    fn pipe_format_with_live_bracket() {
        let m = parse_title("The Blessing (Live) | Elevation Worship");
        assert_eq!(m.song, "The Blessing");
        assert_eq!(m.artist, "Elevation Worship");
    }

    #[test]
    fn pipe_format_with_official_suffix() {
        let m = parse_title("Amazing Song | Great Artist Official Music Video");
        assert_eq!(m.song, "Amazing Song");
        assert_eq!(m.artist, "Great Artist");
    }

    #[test]
    fn unknown_format_returns_full_title() {
        let m = parse_title("Unknown format title");
        assert_eq!(m.song, "Unknown format title");
        assert_eq!(m.artist, "Unknown Artist");
        assert_eq!(m.source, MetadataSource::Regex);
    }

    #[test]
    fn empty_title() {
        let m = parse_title("");
        assert_eq!(m.song, "");
        assert_eq!(m.artist, "Unknown Artist");
    }

    #[test]
    fn whitespace_only_title() {
        let m = parse_title("   ");
        assert_eq!(m.song, "");
        assert_eq!(m.artist, "Unknown Artist");
    }

    #[test]
    fn artist_too_short_falls_through() {
        // Artist "AB" is only 2 chars — should not match (need > 2).
        let m = parse_title("Song | AB");
        assert_eq!(m.artist, "Unknown Artist");
    }

    #[test]
    fn dash_format_with_bracket_stops_song() {
        let m = parse_title("Artist Name - Song Title (Official Video)");
        assert_eq!(m.song, "Song Title");
        assert_eq!(m.artist, "Artist Name");
    }

    // ---- clean_song_title tests ----

    #[test]
    fn clean_removes_round_brackets() {
        assert_eq!(clean_song_title("Song (Live)"), "Song");
    }

    #[test]
    fn clean_removes_square_brackets() {
        assert_eq!(clean_song_title("Song [Official]"), "Song");
    }

    #[test]
    fn clean_removes_curly_brackets() {
        assert_eq!(clean_song_title("Song {Remix}"), "Song");
    }

    #[test]
    fn clean_removes_feat() {
        assert_eq!(
            clean_song_title("Some Song feat. Other Artist"),
            "Some Song"
        );
    }

    #[test]
    fn clean_removes_ft() {
        assert_eq!(clean_song_title("Some Song ft. Other Artist"), "Some Song");
    }

    #[test]
    fn clean_removes_featuring() {
        assert_eq!(
            clean_song_title("Some Song featuring Other Artist"),
            "Some Song"
        );
    }

    #[test]
    fn clean_removes_official_music_video() {
        assert_eq!(clean_song_title("Song Official Music Video"), "Song");
    }

    #[test]
    fn clean_removes_official_video() {
        assert_eq!(clean_song_title("Song Official Video"), "Song");
    }

    #[test]
    fn clean_removes_official_audio() {
        assert_eq!(clean_song_title("Song Official Audio"), "Song");
    }

    #[test]
    fn clean_removes_music_video() {
        assert_eq!(clean_song_title("Song Music Video"), "Song");
    }

    #[test]
    fn clean_removes_live() {
        assert_eq!(clean_song_title("Song Live"), "Song");
    }

    #[test]
    fn clean_removes_acoustic() {
        assert_eq!(clean_song_title("Song Acoustic"), "Song");
    }

    #[test]
    fn clean_removes_hd() {
        assert_eq!(clean_song_title("Song HD"), "Song");
    }

    #[test]
    fn clean_removes_4k() {
        assert_eq!(clean_song_title("Song 4K"), "Song");
    }

    #[test]
    fn clean_collapses_whitespace() {
        assert_eq!(clean_song_title("Song   Title"), "Song Title");
    }

    #[test]
    fn clean_strips_trailing_junk() {
        assert_eq!(clean_song_title("Song -"), "Song");
        assert_eq!(clean_song_title("Song |"), "Song");
        assert_eq!(clean_song_title("Song ,"), "Song");
    }

    #[test]
    fn clean_returns_original_if_empty_after_cleaning() {
        // A title that is entirely bracket content should return original.
        assert_eq!(clean_song_title("(Live)"), "(Live)");
    }

    #[test]
    fn clean_combined() {
        assert_eq!(
            clean_song_title("Song (Remix) feat. Artist Official Video"),
            "Song"
        );
    }
}
