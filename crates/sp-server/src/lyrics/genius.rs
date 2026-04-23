//! Genius.com lyrics source. Uses the public (unauthenticated) search
//! endpoint to find a song match, then fetches the song's public page and
//! extracts lyrics from the `data-lyrics-container="true"` div regions.
//!
//! Genius's REST API (https://docs.genius.com/) returns song metadata +
//! the URL of the public lyrics page, but NOT the lyrics body itself —
//! scraping the page is the expected flow and is what every third-party
//! client does. The page HTML is stable enough (one div per lyric
//! region) that a simple regex strip works.

use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use sp_core::lyrics::{LyricsLine, LyricsTrack};
use tracing::{debug, warn};

const GENIUS_SEARCH_URL: &str = "https://genius.com/api/search/multi";
const REQUEST_TIMEOUT_SECS: u64 = 10;

#[cfg_attr(test, mutants::skip)]
fn user_agent() -> &'static str {
    "Mozilla/5.0 (compatible; SongPlayer/1.0; +https://github.com/zbynekdrlik/songplayer)"
}

// ---------------------------------------------------------------------------
// Genius search API shape (only the fields we read)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SearchResponse {
    response: SearchResponseInner,
}

#[derive(Debug, Deserialize)]
struct SearchResponseInner {
    sections: Vec<SearchSection>,
}

#[derive(Debug, Deserialize)]
struct SearchSection {
    #[serde(rename = "type")]
    section_type: String,
    hits: Vec<SearchHit>,
}

#[derive(Debug, Deserialize)]
struct SearchHit {
    result: HitResult,
}

#[derive(Debug, Deserialize)]
struct HitResult {
    url: String,
    #[serde(default)]
    primary_artist: Option<ArtistRef>,
    #[serde(default)]
    title: Option<String>,
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
/// Returns `None` when no search hit is found or the lyrics page yields
/// no recognisable lyric regions.
#[cfg_attr(test, mutants::skip)]
pub async fn fetch_lyrics(
    client: &Client,
    artist: &str,
    song: &str,
) -> Result<Option<LyricsTrack>> {
    if artist.trim().is_empty() || song.trim().is_empty() {
        return Ok(None);
    }

    let q = format!("{} {}", artist.trim(), song.trim());
    let url = format!(
        "{}?per_page=5&q={}",
        GENIUS_SEARCH_URL,
        urlencoding::encode(&q)
    );
    debug!(%url, "Genius search request");

    let resp = client
        .get(&url)
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

    // The search response groups hits by type. "song" is the one we want.
    let Some(hit_url) = pick_song_url(&body, artist, song) else {
        debug!(artist, song, "Genius: no song hit");
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
        warn!(status = %page_resp.status(), "Genius page non-success");
        return Ok(None);
    }

    let html = page_resp.text().await?;
    Ok(extract_lyrics_from_html(&html))
}

/// Pick the best song-type hit from a Genius search response. Prefers a
/// hit whose `primary_artist.name` contains the expected artist.
fn pick_song_url(resp: &SearchResponse, artist: &str, _song: &str) -> Option<String> {
    let artist_lc = artist.trim().to_ascii_lowercase();
    // Look at all sections; any with a usable `url` is fine, but prefer an
    // artist-name match when available.
    let mut fallback: Option<&str> = None;
    for section in &resp.response.sections {
        if section.section_type != "song" && section.section_type != "top_hit" {
            continue;
        }
        for hit in &section.hits {
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
    }
    fallback.map(|s| s.to_string())
}

/// Strip lyrics from the `data-lyrics-container="true"` regions of a
/// Genius song page.
///
/// Genius wraps every lyric region in `<div data-lyrics-container="true">`
/// with `<br/>` for line breaks and occasional inline `<a>` annotations.
/// We convert `<br>` to `\n`, drop every other tag, decode HTML entities,
/// skip section labels like `[Verse 1]`, and return a `LyricsTrack` with
/// blank timings (callers treat this as text-only context).
pub fn extract_lyrics_from_html(html: &str) -> Option<LyricsTrack> {
    // Find every `<div data-lyrics-container="true"...>...</div>` block.
    // Genius uses multiple such divs per page, all concatenated.
    let mut joined = String::new();
    let mut search_from = 0;
    let marker = "data-lyrics-container=\"true\"";
    while let Some(rel) = html[search_from..].find(marker) {
        let abs = search_from + rel;
        // Walk back to the opening `<div`.
        let div_start = html[..abs].rfind("<div")?;
        // Find the matching closing `</div>` — Genius never nests another
        // `<div>` inside the container, so a simple find() is safe.
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
        .filter(|l| !l.is_empty() && !is_section_label(l))
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

/// Convert a fragment of HTML into plain text, preserving line boundaries
/// defined by `<br>` and `<p>` tags. Decodes common HTML entities.
fn strip_html_preserving_breaks(s: &str) -> String {
    // Replace any <br ...> / <br/> / <p> with newlines before stripping.
    let with_breaks = regex_replace_case_insensitive(s, r"<br\s*/?>", "\n");
    let with_breaks = regex_replace_case_insensitive(&with_breaks, r"</p\s*>", "\n");
    // Drop all remaining tags.
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
    // Decode the handful of entities Genius uses.
    out = out
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ");
    out
}

/// Minimal case-insensitive replace. Not a full regex — just literal
/// marker-based replacement with "\s*" and "\s*/?" accepted.
fn regex_replace_case_insensitive(src: &str, pattern: &str, replacement: &str) -> String {
    // Only support the two patterns we actually need; anything else panics
    // so we notice during development.
    match pattern {
        r"<br\s*/?>" => {
            let lower = src.to_ascii_lowercase();
            let mut out = String::with_capacity(src.len());
            let mut i = 0;
            while i < src.len() {
                let rest = &lower[i..];
                if rest.starts_with("<br") {
                    // Find closing '>'
                    if let Some(close_rel) = rest.find('>') {
                        out.push_str(replacement);
                        i += close_rel + 1;
                        continue;
                    }
                }
                out.push(src.as_bytes()[i] as char);
                i += 1;
            }
            out
        }
        r"</p\s*>" => {
            let lower = src.to_ascii_lowercase();
            let mut out = String::with_capacity(src.len());
            let mut i = 0;
            while i < src.len() {
                let rest = &lower[i..];
                if rest.starts_with("</p") {
                    if let Some(close_rel) = rest.find('>') {
                        out.push_str(replacement);
                        i += close_rel + 1;
                        continue;
                    }
                }
                out.push(src.as_bytes()[i] as char);
                i += 1;
            }
            out
        }
        other => panic!("regex_replace_case_insensitive: unsupported pattern {other}"),
    }
}

fn is_section_label(line: &str) -> bool {
    // Genius annotates sections as `[Verse 1]`, `[Chorus]`, `[Pre-Chorus]`.
    let t = line.trim();
    t.starts_with('[') && t.ends_with(']')
}

// ---------------------------------------------------------------------------
// Tests
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
    fn strip_html_preserves_line_breaks() {
        let s = "a<br/>b<br>c";
        assert_eq!(strip_html_preserving_breaks(s), "a\nb\nc");
    }

    #[test]
    fn pick_song_url_prefers_matching_artist() {
        let resp = SearchResponse {
            response: SearchResponseInner {
                sections: vec![SearchSection {
                    section_type: "song".into(),
                    hits: vec![
                        SearchHit {
                            result: HitResult {
                                url: "https://genius.com/wrong-artist-song".into(),
                                primary_artist: Some(ArtistRef {
                                    name: Some("Wrong Artist".into()),
                                }),
                                title: Some("Song".into()),
                            },
                        },
                        SearchHit {
                            result: HitResult {
                                url: "https://genius.com/right-artist-song".into(),
                                primary_artist: Some(ArtistRef {
                                    name: Some("Right Artist".into()),
                                }),
                                title: Some("Song".into()),
                            },
                        },
                    ],
                }],
            },
        };
        let picked = pick_song_url(&resp, "right artist", "song");
        assert_eq!(
            picked.as_deref(),
            Some("https://genius.com/right-artist-song")
        );
    }

    #[test]
    fn pick_song_url_falls_back_to_first_hit_when_no_artist_match() {
        let resp = SearchResponse {
            response: SearchResponseInner {
                sections: vec![SearchSection {
                    section_type: "song".into(),
                    hits: vec![SearchHit {
                        result: HitResult {
                            url: "https://genius.com/first-hit".into(),
                            primary_artist: None,
                            title: None,
                        },
                    }],
                }],
            },
        };
        let picked = pick_song_url(&resp, "unknown", "song");
        assert_eq!(picked.as_deref(), Some("https://genius.com/first-hit"));
    }
}
