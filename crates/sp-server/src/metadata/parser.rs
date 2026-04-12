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
/// Normalize exotic delimiters to standard `|` or `-` before parsing.
///
/// Converts `//`, `||`, em-dash (`—`), and en-dash (`–`) to standard
/// delimiters so the existing pipe/dash regexes can match them.
/// Only the first exotic delimiter is kept; trailing segments are dropped.
fn normalize_title(title: &str) -> String {
    // Split on // — keep first two parts only (song | artist), drop the rest
    let s = if let Some(pos) = title.find("//") {
        let (left, right) = title.split_at(pos);
        let right = &right[2..]; // skip the "//"
        // Drop anything after a second "//"
        let right = right.split("//").next().unwrap_or(right);
        format!("{} | {}", left.trim(), right.trim())
    } else {
        title.to_string()
    };

    // Split on || — keep first two parts only
    let s = if let Some(pos) = s.find("||") {
        let (left, right) = s.split_at(pos);
        let right = &right[2..];
        let right = right.split("||").next().unwrap_or(right);
        format!("{} | {}", left.trim(), right.trim())
    } else {
        s
    };

    // Em-dash and en-dash act as song|artist (not artist-song), so normalize to pipe
    s.replace(" — ", " | ").replace(" – ", " | ")
}

/// Strip "Official Music Video", "Worship Together Session", etc. from an artist string.
fn clean_artist_suffix(artist: &str) -> String {
    let mut cleaned = artist.to_string();
    for re in TRAILING_PATTERNS.iter() {
        cleaned = re.replace_all(&cleaned, "").to_string();
    }
    // Additional artist-specific suffixes not in TRAILING_PATTERNS
    static ARTIST_EXTRA_SUFFIXES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        [
            r"(?i)\s*worship\s+together\s+session\s*$",
            r"(?i)\s*lyric\s*video\s*$",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("artist suffix pattern must compile"))
        .collect()
    });
    for re in ARTIST_EXTRA_SUFFIXES.iter() {
        cleaned = re.replace_all(&cleaned, "").to_string();
    }
    // Strip "Official Planetshakers" → "Planetshakers" etc.
    static OFFICIAL_PREFIX_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^official\s+").expect("compile"));
    cleaned = OFFICIAL_PREFIX_RE.replace(&cleaned, "").to_string();
    // Remove bracket content
    cleaned = BRACKET_ROUND_RE.replace_all(&cleaned, "").to_string();
    cleaned = BRACKET_SQUARE_RE.replace_all(&cleaned, "").to_string();
    // Strip trailing lone opening paren (from regex captures that stop at "(")
    cleaned = cleaned.trim_end_matches('(').to_string();
    cleaned = WHITESPACE_RE.replace_all(&cleaned, " ").to_string();
    cleaned = TRAILING_JUNK_RE.replace_all(&cleaned, "").to_string();
    cleaned.trim().to_string()
}

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

    let title = &normalize_title(title);

    // Multi-pipe: titles with 3+ pipe segments use "Song | Middle... | Artist [suffix]"
    let pipe_segments: Vec<&str> = title.split('|').collect();
    if pipe_segments.len() >= 3 {
        let song_raw = pipe_segments[0].trim().to_string();
        let last = pipe_segments.last().unwrap().trim();
        // Clean the last segment (strip "Official Music Video", "Worship Together Session", etc.).
        // If cleaning leaves nothing useful (pure junk), fall back to second-to-last segment.
        let artist = clean_artist_suffix(last);
        let artist = if artist.len() > 2 {
            artist
        } else {
            clean_artist_suffix(pipe_segments[pipe_segments.len() - 2].trim())
        };
        if artist.len() > 2 {
            let song = clean_song_title(&song_raw);
            return VideoMetadata {
                song,
                artist: shorten_artist(&artist),
                source: MetadataSource::Regex,
                gemini_failed: false,
            };
        }
    }

    // Pattern 1: "Song | Artist" (artist_first = false)
    if let Some(caps) = PIPE_RE.captures(title) {
        let song_raw = caps[1].trim().to_string();
        let artist_raw = caps[2].trim().to_string();
        let artist = clean_artist_suffix(&artist_raw);
        if artist.len() > 2 {
            let song = clean_song_title(&song_raw);
            return VideoMetadata {
                song,
                artist: shorten_artist(&artist),
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
                artist: shorten_artist(&artist),
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

/// Words that indicate a band/group name — never abbreviate these artists.
/// Includes common articles/prepositions that appear in band names but not personal names.
const BAND_INDICATORS: &[&str] = &[
    "worship",
    "music",
    "church",
    "choir",
    "band",
    "team",
    "united",
    "collective",
    "community",
    "ministry",
    "ministries",
    "ensemble",
    "orchestra",
    "rhythm",
    "heights",
    "city",
    "sons",
    "house",
    // Common band-name words that aren't personal names
    "young",
    "free",
    "voice",
    "college",
    "grupo",
    "sound",
    "room",
    "hill",
    "hillsong",
    "upperroom",
    "elevation",
    // Articles/prepositions — personal names don't contain these
    "of",
    "the",
    "and",
    "for",
    "in",
    "on",
    "at",
    "by",
    "from",
    "with",
    "y",
    "x", // Spanish "and", collaboration separator
];

/// Shorten personal artist names to initials (e.g. "Michael Bethany" → "M. Bethany").
/// Band/group names are never abbreviated. Comma-separated lists are handled per-segment.
pub fn shorten_artist(artist: &str) -> String {
    if artist.contains(',') {
        return artist
            .split(',')
            .map(|s| shorten_single_artist(s.trim()))
            .collect::<Vec<_>>()
            .join(", ");
    }
    if artist.contains('&') {
        return artist
            .split('&')
            .map(|s| shorten_single_artist(s.trim()))
            .collect::<Vec<_>>()
            .join(" & ");
    }
    shorten_single_artist(artist)
}

/// Shorten a single artist name (no commas/ampersands).
fn shorten_single_artist(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();

    // Single word or empty — never abbreviate
    if words.len() <= 1 {
        return name.to_string();
    }

    // Check if any word is a band indicator
    if words
        .iter()
        .any(|w| BAND_INDICATORS.iter().any(|b| w.to_lowercase() == *b))
    {
        return name.to_string();
    }

    // More than 3 words without a band indicator is ambiguous — don't abbreviate
    if words.len() > 3 {
        return name.to_string();
    }

    // Abbreviate all words except the last
    let mut parts: Vec<String> = Vec::new();
    for (i, word) in words.iter().enumerate() {
        if i < words.len() - 1 {
            let initial = word
                .chars()
                .next()
                .map(|c| format!("{}.", c.to_uppercase().next().unwrap_or(c)))
                .unwrap_or_default();
            parts.push(initial);
        } else {
            parts.push(word.to_string());
        }
    }
    parts.join(" ")
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
        assert_eq!(m.artist, "G. Artist");
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
        assert_eq!(m.artist, "A. Name");
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

    #[test]
    fn double_slash_delimiter_parsed_as_pipe() {
        let m = parse_title("Lamb of God // Church of the City // Worship Together Session");
        assert_eq!(m.song, "Lamb of God");
        assert_eq!(m.artist, "Church of the City");
    }

    #[test]
    fn double_pipe_delimiter_parsed() {
        let m = parse_title("Joy || IBC LIVE 2025");
        assert_eq!(m.song, "Joy");
        assert_eq!(m.artist, "IBC");
    }

    #[test]
    fn em_dash_delimiter_parsed_as_dash() {
        let m = parse_title("Shelter In — VOUS Worship");
        assert_eq!(m.song, "Shelter In");
        assert_eq!(m.artist, "VOUS Worship");
    }

    #[test]
    fn en_dash_delimiter_parsed_as_dash() {
        let m = parse_title("IMAGEN – Genock Gabriel");
        assert_eq!(m.song, "IMAGEN");
        assert_eq!(m.artist, "G. Gabriel");
    }

    #[test]
    fn three_segment_pipe_takes_last_as_artist() {
        let m = parse_title(
            "Supernatural Love | Show Me Your Glory - Live At Chapel | Planetshakers Official Music Video",
        );
        assert_eq!(m.song, "Supernatural Love");
        assert_eq!(m.artist, "Planetshakers");
    }

    #[test]
    fn three_segment_pipe_planetshakers_pattern() {
        let m = parse_title("Free Indeed | REVIVAL | Planetshakers Official Music Video");
        assert_eq!(m.song, "Free Indeed");
        assert_eq!(m.artist, "Planetshakers");
    }

    #[test]
    fn worship_together_session_pattern() {
        let m = parse_title("My Father's World | Chris Tomlin | Worship Together Session");
        assert_eq!(m.song, "My Father's World");
        assert_eq!(m.artist, "C. Tomlin");
    }

    #[test]
    fn pipe_artist_with_feat_paren_is_cleaned() {
        let m = parse_title("Keep On | Elevation Worship (feat. Davide Mutendji)");
        assert_eq!(m.song, "Keep On");
        assert_eq!(m.artist, "Elevation Worship");
    }

    #[test]
    fn pipe_artist_with_live_paren_is_cleaned() {
        let m = parse_title("Get This Party Started | Planetshakers (Live)");
        assert_eq!(m.song, "Get This Party Started");
        assert_eq!(m.artist, "Planetshakers");
    }

    // ---- multi-pipe mutant-killing tests ----

    /// Kills the `> 2` → `>= 2` mutant on the inner artist length check:
    /// a 2-char cleaned last segment must be rejected, falling to second-to-last.
    #[test]
    fn multi_pipe_two_char_last_segment_falls_to_second() {
        // "XY" is 2 chars → rejected → falls to "Planetshakers" (second-to-last)
        let m = parse_title("My Song | Planetshakers | XY");
        assert_eq!(m.song, "My Song");
        assert_eq!(m.artist, "Planetshakers");
    }

    /// Kills the `- 2` → `/ 2` mutant on the second-to-last index:
    /// with 5 segments where the last cleans to ≤2 chars, the second-to-last
    /// (index 3) must be picked, not index 5/2=2.
    #[test]
    fn multi_pipe_five_segments_picks_second_to_last() {
        // 5 segments: ["Song", "A", "B", "Planetshakers", "XY"]
        // Last "XY" is 2 chars → rejected → second-to-last (index 3) = "Planetshakers"
        // With / 2 mutant: index 5/2=2 = "B" (wrong)
        let m = parse_title("Song | A | B | Planetshakers | XY");
        assert_eq!(m.song, "Song");
        assert_eq!(m.artist, "Planetshakers");
    }

    /// Kills the `> 2` → `>= 2` mutant on the outer artist length check:
    /// both last AND second-to-last clean to ≤2 chars → must fall through.
    #[test]
    fn multi_pipe_both_segments_too_short_falls_through() {
        let m = parse_title("Some Song | AB | CD");
        assert_eq!(m.artist, "Unknown Artist");
    }

    // ---- shorten_artist tests ----

    #[test]
    fn shorten_personal_name_two_words() {
        assert_eq!(shorten_artist("Michael Bethany"), "M. Bethany");
    }

    #[test]
    fn shorten_personal_name_three_words() {
        assert_eq!(shorten_artist("Martin W Smith"), "M. W. Smith");
    }

    #[test]
    fn shorten_does_not_abbreviate_band_with_worship() {
        assert_eq!(shorten_artist("Elevation Worship"), "Elevation Worship");
    }

    #[test]
    fn shorten_does_not_abbreviate_single_word() {
        assert_eq!(shorten_artist("Planetshakers"), "Planetshakers");
    }

    #[test]
    fn shorten_does_not_abbreviate_band_with_music() {
        assert_eq!(shorten_artist("Maverick City Music"), "Maverick City Music");
    }

    #[test]
    fn shorten_does_not_abbreviate_vous_worship() {
        assert_eq!(shorten_artist("VOUS Worship"), "VOUS Worship");
    }

    #[test]
    fn shorten_handles_comma_separated_artists() {
        assert_eq!(
            shorten_artist("SEU Worship, Roosevelt Stewart, Grace Shuffitt"),
            "SEU Worship, R. Stewart, G. Shuffitt"
        );
    }

    #[test]
    fn shorten_does_not_abbreviate_ampersand_band() {
        assert_eq!(
            shorten_artist("Bethel Music & Kristene DiMarco"),
            "Bethel Music & K. DiMarco"
        );
    }

    #[test]
    fn shorten_does_not_touch_all_caps_acronym() {
        assert_eq!(shorten_artist("TAYA"), "TAYA");
    }

    #[test]
    fn shorten_handles_personal_name() {
        assert_eq!(shorten_artist("Pat Barrett"), "P. Barrett");
    }

    #[test]
    fn shorten_does_not_abbreviate_hillsong_young_free() {
        assert_eq!(
            shorten_artist("Hillsong Young & Free"),
            "Hillsong Young & Free"
        );
    }

    #[test]
    fn shorten_does_not_abbreviate_one_voice() {
        assert_eq!(shorten_artist("One Voice"), "One Voice");
    }

    #[test]
    fn shorten_does_not_abbreviate_spanish_y() {
        assert_eq!(shorten_artist("Johan y Sofi"), "Johan y Sofi");
    }

    #[test]
    fn shorten_does_not_abbreviate_grupo_grace() {
        assert_eq!(shorten_artist("Grupo Grace"), "Grupo Grace");
    }

    #[test]
    fn parser_output_shortens_personal_artist() {
        let m = parse_title("Pat Barrett - Count On You (Live)");
        assert_eq!(m.song, "Count On You");
        assert_eq!(m.artist, "P. Barrett");
    }

    #[test]
    fn parser_output_does_not_shorten_band() {
        let m = parse_title("The Blessing | Elevation Worship");
        assert_eq!(m.song, "The Blessing");
        assert_eq!(m.artist, "Elevation Worship");
    }
}
