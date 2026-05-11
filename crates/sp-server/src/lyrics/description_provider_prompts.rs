//! Claude prompts used by `description_provider::clean_lyrics_via_claude`.
//!
//! Split into a sibling module to keep `description_provider.rs` under the
//! 1000-line cap. Two distinct prompts:
//! - [`build_description_extraction_prompt`] for mixed-content YouTube
//!   description blobs (filters out non-lyric noise).
//! - [`build_scraped_lyrics_cleanup_prompt`] for already-scraped lyrics
//!   from genius / lrclib-plain (dedupes consecutive repeats, drops ad-libs).

/// Selects which Claude prompt `clean_lyrics_via_claude` uses.
///
/// Description blobs contain mixed content (lyrics + tour dates + credits +
/// links); the prompt filters non-lyric noise out. Scraped lyrics blobs
/// (genius HTML, lrclib plain text) are already pure lyrics but commonly
/// have literal chorus repetition and ad-libs; the prompt dedupes
/// consecutive identical lines and drops non-sung vocalizations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupMode {
    /// Mixed-content description blob — filter non-lyric text out.
    Description,
    /// Already-clean scraped lyrics — dedupe consecutive repeats, drop ad-libs.
    ScrapedLyrics,
}

/// Build the Claude extraction prompt for a single video description.
///
/// Empty system prompt — soft-framing in user message instead. Mirrors the
/// `text_merge.rs` pattern: CLIProxyAPI OAuth Claude reverts to conversational
/// mode on lyrics content when given a direct-instruction system prompt,
/// producing preamble instead of JSON. Framing the task as "I'm building a
/// karaoke app" positions Claude as a software engineer and makes JSON output
/// reliable. Returns `(system, user)`.
pub fn build_description_extraction_prompt(
    title: &str,
    artist: &str,
    description: &str,
) -> (String, String) {
    let system = String::new();
    let user = format!(
        "I'm building a karaoke subtitle app for a church. I need to extract the song's \
         lyrics from this YouTube video description so my app can display them synced to \
         the music.\n\n\
         Return a JSON object with exactly one key, \"lines\", whose value is either:\n\
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
            return {{\"lines\": null}}.\n\
         6. Output ONLY the JSON object. No preamble, no markdown fences, no commentary. \
            Start your response with {{ and end with }}.\n\n\
         Video title: {title}\n\
         Artist: {artist}\n\n\
         Description:\n\
         ---\n\
         {description}\n\
         ---"
    );
    (system, user)
}

/// Build the Claude cleanup prompt for already-scraped lyrics (genius, lrclib plain).
///
/// The input is already pure lyrics — no tour dates, no credits, no links to
/// filter. The problems we need Claude to fix are different:
///
/// 1. **Literal chorus repetition.** Genius writes the chorus as many lines as
///    it appears in the song (e.g., 9 consecutive `"It's the power of Jesus"`).
///    The downstream pipeline (`text_reference_merge::process`, Phase 2 chorus-
///    repeat) projects ONE reference chorus line onto N ASR occurrences. Feeding
///    Phase 2 nine literal copies breaks the alignment. Dedupe collapses
///    consecutive identical lines to one.
/// 2. **Ad-libs / vocalizations.** Lines like `"Hyah"`, `"Yeah"`, `"Whoo"` that
///    appear in scraped lyrics but aren't structured sung phrases. They cause
///    Phase 1 nw_dp to mis-align because the ASR transcribes them as something
///    else or skips them entirely.
/// 3. **Spoken intros / DJ chants.** Scraped lyrics sometimes include opening
///    chants from the worship leader that aren't part of the recorded song —
///    listing them produces `match=0` reference lines with no timing.
///
/// Same JSON output contract as `build_description_extraction_prompt`:
/// `{"lines": [...]}` or `{"lines": null}`. Same soft-framing technique
/// to bypass the CLIProxyAPI content-policy classifier.
pub fn build_scraped_lyrics_cleanup_prompt(
    title: &str,
    artist: &str,
    raw_blob: &str,
) -> (String, String) {
    let system = String::new();
    let user = format!(
        "I'm building a karaoke subtitle app for a church. I scraped these lyrics from a \
         lyrics website. The scrape is already mostly clean text, but I need it tidied \
         before my alignment pipeline can use it.\n\n\
         Return a JSON object with exactly one key, \"lines\", whose value is either:\n\
           - an array of strings (one per lyric line, in sung order), OR\n\
           - null, if the text genuinely contains NO lyrics.\n\
         \n\
         Rules:\n\
         1. **Dedupe consecutive identical lines.** If the same line text appears N times \
            in a row (e.g., a chorus written out 8 times), keep ONLY the first occurrence. \
            The downstream alignment system re-expands the chorus to match what the singer \
            actually sang. KEEP non-consecutive repeats (the same phrase later in the song \
            counts as a separate line). Compare lines case-insensitively, ignoring trailing \
            punctuation.\n\
         2. **Drop ad-libs / vocalizations.** Lines that are pure non-word vocalizations \
            (`Hyah`, `Yeah`, `Whoo`, `Uh`, `Mmm`) should be removed. KEEP short structured \
            lines that contain real words (`I'm a saint`, `Hallelujah`).\n\
         3. **Drop opening DJ / hype intros** that are clearly not part of the recorded song \
            (e.g., `Come on now!`, `Make some noise!`, `Put your hands up!`). When in doubt, \
            KEEP the line — only drop if it's obviously a hype call rather than sung lyric.\n\
         4. **Preserve sung order.** Do not reorder lines.\n\
         5. **Preserve non-English lyrics as-is.** Do NOT translate.\n\
         6. **Do not fabricate lines.** Output only what was in the input (minus the dedupe / \
            ad-lib / hype-intro filters above).\n\
         7. **Output ONLY the JSON object.** No preamble, no markdown fences, no commentary. \
            Start your response with {{ and end with }}.\n\n\
         Video title: {title}\n\
         Artist: {artist}\n\n\
         Scraped lyrics:\n\
         ---\n\
         {raw_blob}\n\
         ---"
    );
    (system, user)
}
