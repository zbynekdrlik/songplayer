//! Genius.com lyrics source. Uses the **documented** Genius API
//! (`https://api.genius.com/search`) with an operator-provided Bearer
//! token — the token is a Genius Client Access Token created at
//! <https://genius.com/api-clients>.
//!
//! Flow per <https://docs.genius.com/#search-h2>:
//!   1. `GET /search?q=<artist song>` with `Authorization: Bearer <token>`
//!      returns `response.hits[]` each with `type` ("song") and
//!      `result.url` (the public genius.com lyrics page).
//!   2. Fetch that page — Genius exposes lyrics only via HTML, not the
//!      REST API, and every third-party client scrapes the same markers.
//!      The body is inside `<div data-lyrics-container="true">` regions;
//!      `<br>` separates lines; section labels in `[brackets]` are
//!      dropped.
//!
//! This module never panics, never guesses URLs, and returns `Ok(None)`
//! whenever the token is missing, the search fails, or no song hit
//! matches the requested artist. That lets the caller fall through to
//! the next gather source without error noise.

use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use sp_core::lyrics::{LyricsLine, LyricsTrack};
use tracing::{debug, warn};

const GENIUS_SEARCH_URL: &str = "https://api.genius.com/search";
const REQUEST_TIMEOUT_SECS: u64 = 10;

/// Genius's Cloudflare layer serves a challenge page to bare `curl/*` and
/// "github.com/..." user agents for the public lyrics pages, even when the
/// API itself accepts any UA. Using a common Chrome UA for both calls
/// keeps both paths on the happy flow; Genius's ToS permits reading
/// public pages programmatically.
#[cfg_attr(test, mutants::skip)]
fn user_agent() -> &'static str {
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
}

// ---------------------------------------------------------------------------
// Documented Genius API response shape (only the fields we read).
// Reference: https://docs.genius.com/#search-h2
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SearchResponse {
    response: SearchResponseInner,
}

#[derive(Debug, Deserialize)]
struct SearchResponseInner {
    hits: Vec<SearchHit>,
}

#[derive(Debug, Deserialize)]
struct SearchHit {
    /// Always "song" today. Present for forward compat; we filter on it.
    #[serde(rename = "type", default)]
    hit_type: String,
    result: HitResult,
}

#[derive(Debug, Deserialize)]
struct HitResult {
    /// Public genius.com lyrics page URL. Documented, stable.
    url: String,
    #[serde(default)]
    primary_artist: Option<ArtistRef>,
}

#[derive(Debug, Deserialize)]
struct ArtistRef {
    #[serde(default)]
    name: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Fetch plain-text lyrics lines from Genius for the given artist + song.
/// Returns `None` when:
///   - `access_token` is empty (caller hasn't configured the setting)
///   - the `/search` call fails or returns no song hits
///   - the public lyrics page yields no recognisable lyric regions
#[cfg_attr(test, mutants::skip)]
pub async fn fetch_lyrics(
    client: &Client,
    access_token: &str,
    artist: &str,
    song: &str,
) -> Result<Option<LyricsTrack>> {
    if access_token.trim().is_empty() {
        debug!("Genius: no access token configured — skipping");
        return Ok(None);
    }
    if artist.trim().is_empty() || song.trim().is_empty() {
        return Ok(None);
    }

    let q = format!("{} {}", artist.trim(), song.trim());
    let url = format!("{}?q={}", GENIUS_SEARCH_URL, urlencoding::encode(&q));
    debug!(%url, "Genius search request");

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", access_token.trim()))
        .header("User-Agent", user_agent())
        .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .send()
        .await?;

    if !resp.status().is_success() {
        warn!(
            status = %resp.status(),
            artist,
            song,
            "Genius search non-success"
        );
        return Ok(None);
    }

    let body: SearchResponse = match resp.json().await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "Genius search JSON parse failed");
            return Ok(None);
        }
    };

    let Some(hit_url) = pick_song_url(&body, artist) else {
        debug!(artist, song, "Genius: no song hit matched");
        return Ok(None);
    };

    debug!(%hit_url, "Genius: fetching lyrics page");
    let page_resp = client
        .get(&hit_url)
        .header("User-Agent", user_agent())
        .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .send()
        .await?;

    if !page_resp.status().is_success() {
        warn!(status = %page_resp.status(), "Genius lyrics page non-success");
        return Ok(None);
    }

    let html = page_resp.text().await?;
    Ok(extract_lyrics_from_html(&html))
}

/// Pick the best song-type hit from a Genius search response. Prefers a
/// hit whose `primary_artist.name` contains the expected artist; falls
/// back to the first song hit if no artist match.
fn pick_song_url(resp: &SearchResponse, artist: &str) -> Option<String> {
    let artist_lc = artist.trim().to_ascii_lowercase();
    let mut fallback: Option<&str> = None;
    for hit in &resp.response.hits {
        if hit.hit_type != "song" {
            continue;
        }
        if fallback.is_none() {
            fallback = Some(&hit.result.url);
        }
        if let Some(pa) = hit.result.primary_artist.as_ref()
            && let Some(name) = pa.name.as_ref()
            && name.to_ascii_lowercase().contains(&artist_lc)
        {
            return Some(hit.result.url.clone());
        }
    }
    fallback.map(|s| s.to_string())
}

/// Strip lyrics from the `data-lyrics-container="true"` regions of a
/// Genius song page.
pub fn extract_lyrics_from_html(html: &str) -> Option<LyricsTrack> {
    let mut joined = String::new();
    let mut search_from = 0;
    let marker = "data-lyrics-container=\"true\"";
    while let Some(rel) = html[search_from..].find(marker) {
        let abs = search_from + rel;
        let div_start = html[..abs].rfind("<div")?;
        let after_open = html[div_start..].find('>')? + div_start + 1;
        let close = html[after_open..].find("</div>")? + after_open;
        let block = &html[after_open..close];
        joined.push_str(block);
        joined.push('\n');
        search_from = close + "</div>".len();
    }
    if joined.is_empty() {
        return None;
    }

    let text = strip_html_preserving_breaks(&joined);
    let lines: Vec<LyricsLine> = text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !is_section_label(l) && !is_genius_banner(l))
        .map(|l| LyricsLine {
            start_ms: 0,
            end_ms: 0,
            en: l,
            sk: None,
            words: None,
        })
        .collect();
    if lines.is_empty() {
        None
    } else {
        Some(LyricsTrack {
            version: 2,
            source: "genius".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines,
        })
    }
}

fn strip_html_preserving_breaks(s: &str) -> String {
    let with_breaks = replace_case_insensitive_tag(s, "br", "\n");
    let with_breaks = replace_case_insensitive_close_tag(&with_breaks, "p", "\n");
    let mut out = String::with_capacity(with_breaks.len());
    let mut in_tag = false;
    for ch in with_breaks.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
}

/// Replace every opening (or self-closing) tag of the given name,
/// case-insensitive, with `replacement`. Example: `replace_case_insensitive_tag(s, "br", "\n")`
/// turns `<br>`, `<BR/>`, `<Br />` into newlines.
fn replace_case_insensitive_tag(src: &str, tag: &str, replacement: &str) -> String {
    let lower = src.to_ascii_lowercase();
    let needle = format!("<{tag}");
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < src.len() {
        if lower[i..].starts_with(&needle)
            && let Some(close_rel) = lower[i..].find('>')
        {
            out.push_str(replacement);
            i += close_rel + 1;
            continue;
        }
        // Multi-byte safe: push the char at `i`, advance by its UTF-8 len.
        let c = src[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

fn replace_case_insensitive_close_tag(src: &str, tag: &str, replacement: &str) -> String {
    let lower = src.to_ascii_lowercase();
    let needle = format!("</{tag}");
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < src.len() {
        if lower[i..].starts_with(&needle)
            && let Some(close_rel) = lower[i..].find('>')
        {
            out.push_str(replacement);
            i += close_rel + 1;
            continue;
        }
        let c = src[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

fn is_section_label(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('[') && t.ends_with(']')
}

/// The first `data-lyrics-container` div on every Genius song page is
/// prefixed with a banner like `"3 ContributorsTHE DEEP Lyrics"` —
/// Contributor count + song title + the word "Lyrics" — rendered before
/// the actual first lyric line. Drop any line that carries that
/// signature so the worker doesn't feed it to Gemini as a lyric.
fn is_genius_banner(line: &str) -> bool {
    let t = line.trim();
    // Banners always mention "Contributor" (or "Translation") and end in
    // "Lyrics". Filter on the conjunction — neither alone is specific
    // enough (a real lyric can legitimately end in "lyrics").
    let lower = t.to_ascii_lowercase();
    (lower.contains("contributor") || lower.contains("translation")) && lower.ends_with("lyrics")
}

// ---------------------------------------------------------------------------
// Tests — pure data transforms; network paths exercised end-to-end by the
// deployed worker and by manual verification against real Genius pages.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_lyrics_from_basic_container() {
        let html = r#"
        <html><body>
        <div data-lyrics-container="true" class="Lyrics__Container">
            [Verse 1]<br/>Line one<br>Line two<br/>
        </div>
        </body></html>
        "#;
        let track = extract_lyrics_from_html(html).expect("found lyrics");
        assert_eq!(track.source, "genius");
        let lines: Vec<&str> = track.lines.iter().map(|l| l.en.as_str()).collect();
        assert_eq!(lines, vec!["Line one", "Line two"]);
    }

    #[test]
    fn extract_handles_multiple_containers_and_entities() {
        let html = r#"
        <div data-lyrics-container="true">Can&#39;t stop<br>the feeling</div>
        <div data-lyrics-container="true">[Chorus]<br/>Sing it &amp; mean it</div>
        "#;
        let track = extract_lyrics_from_html(html).expect("found lyrics");
        let lines: Vec<&str> = track.lines.iter().map(|l| l.en.as_str()).collect();
        assert_eq!(
            lines,
            vec!["Can't stop", "the feeling", "Sing it & mean it"]
        );
    }

    #[test]
    fn extract_returns_none_when_no_container() {
        let html = "<html><body>no lyrics markers here</body></html>";
        assert!(extract_lyrics_from_html(html).is_none());
    }

    #[test]
    fn section_label_detection() {
        assert!(is_section_label("[Verse 1]"));
        assert!(is_section_label("[Chorus]"));
        assert!(is_section_label("[Pre-Chorus: Artist]"));
        assert!(!is_section_label("Verse 1"));
        assert!(!is_section_label("Just a lyric line"));
    }

    #[test]
    fn genius_banner_detection() {
        // Real banners observed on Genius pages (2026-04-23 live fetch
        // of https://genius.com/Youth-alive-the-deep-lyrics).
        assert!(is_genius_banner("1 ContributorThe Deep Lyrics"));
        assert!(is_genius_banner("37 ContributorsJesus Be The Name Lyrics"));
        assert!(is_genius_banner("Translations Türkçe Français Lyrics"));
        // False positives we must avoid:
        assert!(!is_genius_banner("Just a lyric")); // no contributor
        assert!(!is_genius_banner("Contributor")); // not ending in lyrics
        assert!(!is_genius_banner("I wrote these lyrics for you")); // real lyric
    }

    #[test]
    fn extract_strips_contributor_banner() {
        let html = r#"
        <div data-lyrics-container="true">3 ContributorsTHE DEEP Lyrics<br/>I can't comprehend<br/>How You love</div>
        "#;
        let track = extract_lyrics_from_html(html).expect("found lyrics");
        let lines: Vec<&str> = track.lines.iter().map(|l| l.en.as_str()).collect();
        assert_eq!(lines, vec!["I can't comprehend", "How You love"]);
    }

    #[test]
    fn strip_html_preserves_line_breaks() {
        assert_eq!(strip_html_preserving_breaks("a<br/>b<br>c"), "a\nb\nc");
        assert_eq!(strip_html_preserving_breaks("<p>hi</p>"), "hi\n");
    }

    #[test]
    fn pick_song_url_prefers_matching_artist() {
        let resp = SearchResponse {
            response: SearchResponseInner {
                hits: vec![
                    SearchHit {
                        hit_type: "song".into(),
                        result: HitResult {
                            url: "https://genius.com/wrong-artist-song".into(),
                            primary_artist: Some(ArtistRef {
                                name: Some("Wrong Artist".into()),
                            }),
                        },
                    },
                    SearchHit {
                        hit_type: "song".into(),
                        result: HitResult {
                            url: "https://genius.com/right-artist-song".into(),
                            primary_artist: Some(ArtistRef {
                                name: Some("Right Artist".into()),
                            }),
                        },
                    },
                ],
            },
        };
        assert_eq!(
            pick_song_url(&resp, "right artist").as_deref(),
            Some("https://genius.com/right-artist-song")
        );
    }

    #[test]
    fn pick_song_url_falls_back_to_first_hit_when_no_artist_match() {
        let resp = SearchResponse {
            response: SearchResponseInner {
                hits: vec![SearchHit {
                    hit_type: "song".into(),
                    result: HitResult {
                        url: "https://genius.com/first-hit".into(),
                        primary_artist: None,
                    },
                }],
            },
        };
        assert_eq!(
            pick_song_url(&resp, "unknown").as_deref(),
            Some("https://genius.com/first-hit")
        );
    }

    /// Kills the `+=` → `-=`, `+=` → `*=` TIMEOUT mutants on line 257, and
    /// the `+` → `*` MISSED mutant on `close_rel + 1` (line 257:28). Real
    /// output is `"foo\nbar\nbaz"`; any arithmetic change either hangs
    /// (TIMEOUT) or produces a different string (off-by-one leaves `>`
    /// characters, or miscounts `close_rel + 1`).
    #[test]
    fn replace_case_insensitive_tag_handles_mixed_case_br() {
        let input = "foo<br>bar<BR>baz<Br/>end";
        let out = replace_case_insensitive_tag(input, "br", "\n");
        assert_eq!(out, "foo\nbar\nbaz\nend");
    }

    /// Kills `+=` → TIMEOUT mutants on line 263 (the non-tag char-advance
    /// path). Exercised by any input that contains non-`<br` characters,
    /// but we assert the exact output so the `+` → `*` MISSED mutant
    /// on `close_rel + 1` is also killed on inputs with interleaved tags.
    ///
    /// Multi-byte UTF-8 exercises `c.len_utf8()` — Slovak `á` is 2 bytes,
    /// so mutations to the char-advance arithmetic would corrupt the
    /// string slice or infinite-loop.
    #[test]
    fn replace_case_insensitive_tag_preserves_utf8_between_tags() {
        let input = "náš<br>dom";
        let out = replace_case_insensitive_tag(input, "br", "\n");
        assert_eq!(out, "náš\ndom");
    }

    /// Kills the three mutants on `replace_case_insensitive_close_tag`
    /// (line 278: `+=` → `-=`/`*=` + `+` → `*`; line 283: `+=` → `*=`).
    /// Real output is `"a\nb\nc"`; any mutation either hangs or produces
    /// garbage output.
    #[test]
    fn replace_case_insensitive_close_tag_handles_closing_paragraph() {
        let input = "a</p>b</P>c";
        let out = replace_case_insensitive_close_tag(input, "p", "\n");
        assert_eq!(out, "a\nb\nc");
    }

    /// UTF-8 variant for the close-tag function — guards line 283
    /// (`i += c.len_utf8()`) against mutants that would corrupt
    /// multi-byte char boundaries.
    #[test]
    fn replace_case_insensitive_close_tag_preserves_utf8() {
        let input = "ľúto</p>sme";
        let out = replace_case_insensitive_close_tag(input, "p", "\n");
        assert_eq!(out, "ľúto\nsme");
    }

    /// Kills `&&` → `||` on `is_section_label` line 290. Under `||`,
    /// any line starting with `[` OR ending with `]` would be labelled
    /// as a section (and filtered out of lyrics). This test covers
    /// inputs that match exactly one of the two conditions — they
    /// must NOT be labelled.
    #[test]
    fn is_section_label_requires_both_brackets() {
        // Starts with `[` but does not end with `]` — NOT a section label.
        assert!(
            !is_section_label("[Verse 1"),
            "missing closing `]` must not be a section label"
        );
        // Ends with `]` but does not start with `[` — NOT a section label.
        assert!(
            !is_section_label("Verse 1]"),
            "missing opening `[` must not be a section label"
        );
        // Neither — obvious non-label.
        assert!(!is_section_label("just a lyric line"));
        // Both — correctly labelled.
        assert!(is_section_label("[Chorus]"));
    }

    /// Kills the line 190:29 `+` → `-` mutant on
    /// `search_from = close + "</div>".len()`. With `-`, `search_from`
    /// moves BACKWARD, which either infinite-loops or re-processes the
    /// same div. Two-container input with distinct content lets us
    /// assert the exact line order.
    #[test]
    fn extract_lyrics_from_two_containers_advances_correctly() {
        let html = r#"
        <div data-lyrics-container="true">alpha<br>bravo</div>
        <div data-lyrics-container="true">charlie<br>delta</div>
        "#;
        let track = extract_lyrics_from_html(html).expect("found lyrics");
        let lines: Vec<&str> = track.lines.iter().map(|l| l.en.as_str()).collect();
        assert_eq!(
            lines,
            vec!["alpha", "bravo", "charlie", "delta"],
            "two containers must be extracted in order with no duplicates"
        );
    }

    #[test]
    fn pick_song_url_skips_non_song_hit_types() {
        // Genius returns other hit types (e.g. "lyric", "album"); we must
        // ignore them so we don't try to scrape a non-lyrics page.
        let resp = SearchResponse {
            response: SearchResponseInner {
                hits: vec![
                    SearchHit {
                        hit_type: "album".into(),
                        result: HitResult {
                            url: "https://genius.com/album-not-song".into(),
                            primary_artist: Some(ArtistRef {
                                name: Some("Artist".into()),
                            }),
                        },
                    },
                    SearchHit {
                        hit_type: "song".into(),
                        result: HitResult {
                            url: "https://genius.com/actual-song".into(),
                            primary_artist: Some(ArtistRef {
                                name: Some("Artist".into()),
                            }),
                        },
                    },
                ],
            },
        };
        assert_eq!(
            pick_song_url(&resp, "artist").as_deref(),
            Some("https://genius.com/actual-song")
        );
    }
}
