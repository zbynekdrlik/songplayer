//! Gemini chunk prompt builder (pure function, no I/O).

/// Build the Gemini chunked-transcription prompt.
pub fn build_prompt(
    reference_lyrics: &str,
    chunk_start_ms: u64,
    chunk_end_ms: u64,
    full_duration_ms: u64,
) -> String {
    format!(
        "You are a precise sung-lyrics transcription assistant. Your only output format is timed lines in this exact schema, one per line, nothing else:\n\
         (MM:SS.x --> MM:SS.x) text\n\n\
         Transcribe the sung vocals in the attached audio.\n\n\
         Rules:\n\n\
         1. Timestamps are LOCAL to this audio chunk, starting at 00:00. Do NOT offset.\n\n\
         2. COVERAGE — Output a timed line for EVERY sung phrase. Do NOT skip or collapse repeated choruses or refrains. If a phrase is sung 5 times, output 5 separate lines. Do not summarize.\n\n\
         3. SHORT LINES — Break long phrases into short, separately timed lines.\n\
            - Break at every comma, semicolon, or breath pause.\n\
            - Example: \"To know Your heart, oh it's the goal of my life, it's the aim of my life\" MUST be 3 separate lines:\n\
              (07:23.0 --> 07:25.5) To know Your heart\n\
              (07:26.0 --> 07:30.0) Oh it's the goal of my life\n\
              (07:31.0 --> 07:34.0) It's the aim of my life\n\
            - Aim for <= 8 words per output line where the phrasing allows.\n\n\
         4. PRECISION — Line start_time = the exact moment the first syllable BEGINS being sung (not the breath before, not a preceding beat). Line end_time = the last syllable finishes, before the next silence.\n\n\
         5. SILENCE — If the chunk has no vocals (instrumental only, or pre-roll silence), output exactly: # no vocals\n\n\
         6. OUTPUT FORMAT — Output ONLY timed lines. No intro text, no commentary, no markdown fences, no summary at the end.\n\n\
         7. DO NOT HALLUCINATE — Only transcribe what you actually hear. If you hear a word not matching the reference lyrics below, still write what you hear. If the reference has a line that doesn't appear in this audio chunk, do NOT include it.\n\n\
         Reference lyrics for this song (extracted from YouTube description — may be out of order, missing chorus repeats, or contain extra phrases not in this chunk):\n\
         {reference_lyrics}\n\n\
         This chunk covers audio from {start:.1}s to {end:.1}s of the full song ({total:.1}s total). The chunk may start or end mid-phrase.\n",
        start = chunk_start_ms as f64 / 1000.0,
        end = chunk_end_ms as f64 / 1000.0,
        total = full_duration_ms as f64 / 1000.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_chunk_time_window_in_seconds() {
        let p = build_prompt("Hello world", 17_160, 77_160, 659_980);
        assert!(
            p.contains("from 17.2s to 77.2s"),
            "expected chunk window in seconds, got:\n{p}"
        );
        assert!(
            p.contains("659.9s total") || p.contains("659.98s total") || p.contains("660.0s total"),
            "expected full duration in seconds, got:\n{p}"
        );
    }

    #[test]
    fn prompt_includes_reference_lyrics_verbatim() {
        let p = build_prompt(
            "I could search all this world\nI still find",
            0,
            60_000,
            180_000,
        );
        assert!(p.contains("I could search all this world"));
        assert!(p.contains("I still find"));
    }

    #[test]
    fn prompt_contains_required_rules() {
        let p = build_prompt("ref", 0, 60_000, 180_000);
        assert!(p.contains("Timestamps are LOCAL"));
        assert!(p.contains("COVERAGE"));
        assert!(p.contains("SHORT LINES"));
        assert!(p.contains("PRECISION"));
        assert!(p.contains("# no vocals"));
        assert!(p.contains("DO NOT HALLUCINATE"));
    }
}
