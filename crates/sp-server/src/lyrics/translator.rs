//! Gemini-based EN→SK lyrics translator with worship glossary.

use anyhow::{Result, anyhow};
use serde_json::Value;
use sp_core::lyrics::LyricsTrack;
use std::time::Duration;

/// Translate all EN lyrics lines to Slovak, modifying `track` in place.
/// Sets `track.language_translation = "sk"` on success.
#[cfg_attr(test, mutants::skip)]
pub async fn translate_lyrics(api_key: &str, model: &str, track: &mut LyricsTrack) -> Result<()> {
    if track.lines.is_empty() {
        track.language_translation = "sk".to_string();
        return Ok(());
    }

    let client = reqwest::Client::new();

    // Build numbered input text
    let numbered: String = track
        .lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{}: {}", i + 1, line.en))
        .collect::<Vec<_>>()
        .join("\n");

    let line_count = track.lines.len();
    let body = build_translation_body(model, &numbered, line_count);

    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        model, api_key
    );

    let resp = client
        .post(&url)
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| anyhow!("Gemini request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(anyhow!("Gemini HTTP {}", resp.status()));
    }

    let response_body: Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("Failed to parse Gemini response: {e}"))?;

    let text = response_body
        .pointer("/candidates/0/content/parts/0/text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Missing candidates[0].content.parts[0].text in response"))?;

    tracing::debug!("Translation response: {text}");

    let translations = parse_translation_response(text, line_count);

    for (line, sk_text) in track.lines.iter_mut().zip(translations.into_iter()) {
        line.sk = if sk_text.is_empty() {
            None
        } else {
            Some(sk_text)
        };
    }

    track.language_translation = "sk".to_string();
    Ok(())
}

/// Build the Gemini request body for EN→SK translation.
pub fn build_translation_body(_model: &str, numbered_lyrics: &str, line_count: usize) -> Value {
    let system_instruction = format!(
        "You are a Slovak worship lyrics translator.\n\
         \n\
         CRITICAL: Output EXACTLY {line_count} numbered lines, one per input line.\n\
         Format: N: Slovak text\n\
         \n\
         TRANSLATION RULES:\n\
         1. Preserve meaning and emotional tone of worship lyrics\n\
         2. Use natural Slovak phrasing — not word-for-word translation\n\
         3. Keep each line ≤45 characters for LED wall display\n\
         4. DO NOT translate these sacred words: Hallelujah, Hosanna, Amen, Selah, Maranatha, Emmanuel\n\
         5. NEVER produce Czech words. Use Slovak: pretože (not protože), tiež (not také), \
            hovorím (not říkám), iba (not pouze), každý (not každý stays same but watch for Czech patterns)\n\
         \n\
         WORSHIP GLOSSARY (use these exact translations):\n\
         - Jesus → Ježiš\n\
         - Christ → Kristus\n\
         - Lord → Pán\n\
         - God → Boh\n\
         - grace → milosť\n\
         - Holy Spirit → Duch Svätý\n\
         - Lamb of God → Baránok Boží\n\
         - salvation → spasenie\n\
         - faith → viera\n\
         - mercy → milosrdenstvo\n\
         - glory → sláva\n\
         - kingdom → kráľovstvo\n\
         - cross → kríž\n\
         - praise → chvála\n\
         - worship → uctievanie\n\
         - eternal life → večný život\n\
         - resurrection → vzkriesenie"
    );

    serde_json::json!({
        "system_instruction": {
            "parts": [{"text": system_instruction}]
        },
        "contents": [
            {"role": "user", "parts": [{"text": numbered_lyrics}]}
        ],
        "generationConfig": {
            "temperature": 0.3,
            "candidateCount": 1
        }
    })
}

/// Parse a numbered translation response into a Vec of Slovak strings.
/// Returns a Vec of exactly `expected_count` strings (empty string for missing lines).
pub fn parse_translation_response(text: &str, expected_count: usize) -> Vec<String> {
    let mut result = vec![String::new(); expected_count];

    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Split on first colon only
        if let Some((num_part, rest)) = trimmed.split_once(':') {
            let num_trimmed = num_part.trim();
            if let Ok(n) = num_trimmed.parse::<usize>() {
                if n >= 1 && n <= expected_count {
                    result[n - 1] = rest.trim().to_string();
                }
            }
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_translation_response_basic() {
        let text = "1: Prvá riadka\n2: Druhá riadka\n3: Tretia riadka";
        let result = parse_translation_response(text, 3);
        assert_eq!(result, vec!["Prvá riadka", "Druhá riadka", "Tretia riadka"]);
    }

    #[test]
    fn parse_translation_response_with_colon_in_text() {
        // Translated text itself contains a colon — only first colon is used as delimiter
        let text = "1: Pán: môj pastier\n2: Druhá riadka";
        let result = parse_translation_response(text, 2);
        assert_eq!(result, vec!["Pán: môj pastier", "Druhá riadka"]);
    }

    #[test]
    fn parse_translation_response_missing_lines_filled_with_empty() {
        // Lines 2 and 4 are missing
        let text = "1: Prvá riadka\n3: Tretia riadka";
        let result = parse_translation_response(text, 4);
        assert_eq!(
            result,
            vec![
                "Prvá riadka".to_string(),
                String::new(),
                "Tretia riadka".to_string(),
                String::new(),
            ]
        );
    }

    #[test]
    fn parse_translation_response_extra_lines_ignored() {
        // Line 5 is out of range (expected_count = 3)
        let text = "1: Prvá\n2: Druhá\n3: Tretia\n5: Extra";
        let result = parse_translation_response(text, 3);
        assert_eq!(result, vec!["Prvá", "Druhá", "Tretia"]);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn parse_translation_response_empty_input() {
        let result = parse_translation_response("", 3);
        assert_eq!(result, vec![String::new(), String::new(), String::new()]);
    }

    #[test]
    fn build_translation_body_structure() {
        let body = build_translation_body("gemini-2.5-flash", "1: Amazing grace\n2: How sweet", 2);

        // System instruction contains glossary terms
        let sys = body["system_instruction"]["parts"][0]["text"]
            .as_str()
            .unwrap();
        assert!(sys.contains("Ježiš"), "glossary: Jesus→Ježiš");
        assert!(sys.contains("Kristus"), "glossary: Christ→Kristus");
        assert!(sys.contains("milosť"), "glossary: grace→milosť");
        assert!(
            sys.contains("Duch Svätý"),
            "glossary: Holy Spirit→Duch Svätý"
        );
        assert!(sys.contains("Baránok Boží"), "glossary: Lamb of God");
        assert!(sys.contains("spasenie"), "glossary: salvation");
        assert!(sys.contains("milosrdenstvo"), "glossary: mercy");
        assert!(sys.contains("sláva"), "glossary: glory");
        assert!(sys.contains("kráľovstvo"), "glossary: kingdom");
        assert!(sys.contains("vzkriesenie"), "glossary: resurrection");

        // Contains line count instruction
        assert!(sys.contains("EXACTLY 2"), "must specify exact line count");

        // Sacred words not translated
        assert!(sys.contains("Hallelujah"), "Hallelujah preserved");
        assert!(sys.contains("Hosanna"), "Hosanna preserved");

        // Czech prevention
        assert!(sys.contains("pretože"), "Czech prevention: pretože");
        assert!(sys.contains("protože"), "Czech prevention: not protože");

        // Temperature is 0.3
        assert_eq!(body["generationConfig"]["temperature"].as_f64(), Some(0.3));

        // No tools
        assert!(
            body.get("tools").is_none(),
            "translation must not use google_search tools"
        );

        // User content contains the numbered lyrics
        let user_text = body["contents"][0]["parts"][0]["text"].as_str().unwrap();
        assert!(user_text.contains("1: Amazing grace"));
        assert!(user_text.contains("2: How sweet"));
    }
}
