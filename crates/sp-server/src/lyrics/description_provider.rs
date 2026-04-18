//! YouTube description lyrics provider.
//!
//! Fetches the raw description via yt-dlp, pipes it through a narrow Claude
//! prompt, and emits a `CandidateText { source: "description" }` for the
//! ensemble text-merge step. Caches both the raw description and the
//! extracted lyrics JSON on disk so reprocesses reuse the work.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use crate::ai::client::AiClient;

/// Build the Claude extraction prompt for a single video description.
///
/// The system prompt is intentionally specific: request a JSON object with
/// one `lines` key (array of strings or null), strip section markers, keep
/// non-English as-is, and refuse to fabricate. Returns `(system, user)`.
pub fn build_description_extraction_prompt(
    title: &str,
    artist: &str,
    description: &str,
) -> (String, String) {
    let system = String::from(
        "You are a lyrics extractor. Given a YouTube video description, return the song's lyrics \
         as a JSON object with exactly one key, \"lines\", whose value is either:\n\
           - an array of strings (one per lyric line, in reading order, in the song's original language), OR\n\
           - null, when the description contains NO lyrics.\n\
         \n\
         Rules:\n\
         1. Strip section markers (\"Verse 1:\", \"Chorus:\", \"Bridge:\", etc.), keep the line text.\n\
         2. Preserve non-English lyrics as-is. Do NOT translate.\n\
         3. Ignore: artist bio, social links, streaming/buy links, copyright notices, producer/\n\
            writer credits, album promo, tour dates, comment/like/subscribe prompts.\n\
         4. If multiple languages appear (e.g., English + Spanish side-by-side or verse/translation \
            blocks), include ALL lines in reading order — downstream reconciliation handles dedupe.\n\
         5. Do not fabricate lyrics. If you are not confident the text is the song's lyrics, \
            return {\"lines\": null}.\n\
         6. Output ONLY the JSON object. No preamble, no markdown fences, no commentary.",
    );
    let user =
        format!("Video title: {title}\nArtist: {artist}\n\nDescription:\n---\n{description}\n---");
    (system, user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_has_rule_about_null_when_no_lyrics() {
        let (system, _user) =
            build_description_extraction_prompt("Song", "Artist", "some description");
        assert!(
            system.contains("null"),
            "system prompt must mention the null case: {system}"
        );
        assert!(
            system.contains("\"lines\""),
            "system prompt must name the JSON key: {system}"
        );
    }

    #[test]
    fn prompt_includes_title_artist_and_description_in_user_message() {
        let (_system, user) = build_description_extraction_prompt(
            "How Great Thou Art",
            "Planetshakers",
            "Here are the lyrics:\nHow great thou art",
        );
        assert!(user.contains("How Great Thou Art"), "title missing: {user}");
        assert!(user.contains("Planetshakers"), "artist missing: {user}");
        assert!(
            user.contains("How great thou art"),
            "description body missing: {user}"
        );
    }

    #[test]
    fn prompt_forbids_fabrication() {
        let (system, _user) = build_description_extraction_prompt("S", "A", "desc");
        assert!(
            system.contains("fabricate") || system.contains("not confident"),
            "system prompt must warn against fabrication: {system}"
        );
    }

    #[test]
    fn prompt_requires_original_language() {
        let (system, _user) = build_description_extraction_prompt("S", "A", "desc");
        assert!(
            system.contains("Preserve") && system.contains("translate"),
            "system prompt must require original-language preservation: {system}"
        );
    }
}
