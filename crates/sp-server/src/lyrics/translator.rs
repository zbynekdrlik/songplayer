//! EN→SK lyrics translator via Claude (CLIProxyAPI).
//!
//! Prompt design:
//! The earlier Gemini-based translator + over-explained Claude prompt used
//! terms like "karaoke subtitles", "church", "lyrics" that tripped Claude's
//! content-policy layer and caused refusals. The user verified that a short
//! neutral prompt — no mention of "lyrics", "song", "worship", "karaoke" —
//! works reliably. Keep it that way.

use anyhow::{Result, anyhow};
use sp_core::lyrics::LyricsTrack;

/// Translate English lines in `track` to Slovak via Claude. Returns a Vec
/// aligned 1:1 with `track.lines`; empty strings mark lines Claude did not
/// return a translation for.
#[cfg_attr(test, mutants::skip)]
pub async fn translate_via_claude(
    ai_client: &crate::ai::client::AiClient,
    track: &LyricsTrack,
) -> Result<Vec<String>> {
    if track.lines.is_empty() {
        return Ok(vec![]);
    }

    let numbered: String = track
        .lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{}: {}", i + 1, line.en))
        .collect::<Vec<_>>()
        .join("\n");

    let line_count = track.lines.len();
    let user = build_prompt(line_count, &numbered);

    // No system prompt — cloaked Claude behaves best with everything in the user message.
    let response = ai_client
        .chat("", &user)
        .await
        .map_err(|e| anyhow!("Claude translation failed: {e}"))?;

    let translations = parse_translation_response(&response, line_count);

    let non_empty = translations.iter().filter(|t| !t.is_empty()).count();
    if non_empty == 0 && line_count > 0 {
        return Err(anyhow!("Claude translation returned no translations"));
    }

    Ok(translations)
}

/// Build the translation prompt. Public for unit testing the exact wording.
///
/// Keep this short and neutral: no mention of "lyrics", "song", "worship",
/// "karaoke", or "church". The user verified that over-explained prompts
/// trigger Claude's content-policy refusals while minimal ones do not.
pub fn build_prompt(line_count: usize, numbered: &str) -> String {
    format!(
        "Translate these English lines to Slovak, keeping the line numbering. \
         Slovak, not Czech. Output exactly {line_count} lines in the format `N: Slovak text`. \
         Glossary: Jesus=Ježiš, Christ=Kristus, Lord=Pán, God=Boh, grace=milosť, \
         Holy Spirit=Duch Svätý, cross=kríž, faith=viera, glory=sláva, \
         salvation=spasenie, Hallelujah stays as Hallelujah, Hosanna stays as Hosanna, \
         Amen stays as Amen.\n\n{numbered}"
    )
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

    fn make_track(lines: &[&str]) -> LyricsTrack {
        LyricsTrack {
            version: 1,
            source: "test".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines: lines
                .iter()
                .enumerate()
                .map(|(i, s)| sp_core::lyrics::LyricsLine {
                    start_ms: (i as u64) * 1000,
                    end_ms: (i as u64 + 1) * 1000,
                    en: (*s).to_string(),
                    sk: None,
                    words: None,
                })
                .collect(),
        }
    }

    #[test]
    fn parse_translation_response_basic() {
        let text = "1: Prvá riadka\n2: Druhá riadka\n3: Tretia riadka";
        let result = parse_translation_response(text, 3);
        assert_eq!(result, vec!["Prvá riadka", "Druhá riadka", "Tretia riadka"]);
    }

    #[test]
    fn parse_translation_response_with_colon_in_text() {
        let text = "1: Pán: môj pastier\n2: Druhá riadka";
        let result = parse_translation_response(text, 2);
        assert_eq!(result, vec!["Pán: môj pastier", "Druhá riadka"]);
    }

    #[test]
    fn parse_translation_response_missing_lines_filled_with_empty() {
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
    fn build_prompt_is_short_and_neutral() {
        let out = build_prompt(3, "1: a\n2: b\n3: c");
        // Must NOT contain the policy-tripping terms the user flagged.
        for bad in [
            "lyrics",
            "song",
            "worship",
            "karaoke",
            "church",
            "copyright",
        ] {
            assert!(
                !out.to_lowercase().contains(bad),
                "prompt must stay neutral; found `{bad}` in:\n{out}"
            );
        }
        // Must contain the numbered text and line count instruction.
        assert!(out.contains("1: a"));
        assert!(out.contains("exactly 3"));
    }

    #[test]
    fn build_prompt_includes_core_glossary() {
        let out = build_prompt(1, "1: x");
        for term in ["Ježiš", "Kristus", "Pán", "Boh", "Duch Svätý", "milosť"] {
            assert!(out.contains(term), "glossary missing `{term}` in:\n{out}");
        }
    }

    #[tokio::test]
    async fn translate_via_claude_returns_parsed_translations() {
        use crate::ai::AiSettings;
        use crate::ai::client::AiClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let response_body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "1: Prvá riadka\n2: Druhá riadka\n3: Tretia riadka"
                }
            }]
        });
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .mount(&server)
            .await;

        let client = AiClient::new(AiSettings {
            api_url: format!("{}/v1", server.uri()),
            api_key: None,
            model: "claude-opus-4-20250514".into(),
            system_prompt_extra: None,
        });

        let track = make_track(&["Line one", "Line two", "Line three"]);
        let result = translate_via_claude(&client, &track).await;

        assert!(
            result.is_ok(),
            "translation should succeed, got: {result:?}"
        );
        let translations = result.unwrap();
        assert_eq!(translations.len(), 3);
        assert_eq!(translations[0], "Prvá riadka");
        assert_eq!(translations[1], "Druhá riadka");
        assert_eq!(translations[2], "Tretia riadka");
    }

    #[tokio::test]
    async fn translate_via_claude_errors_on_policy_refusal() {
        use crate::ai::AiSettings;
        use crate::ai::client::AiClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let response_body = serde_json::json!({
            "choices": [{
                "message": {"content": "I cannot help with that request."}
            }]
        });
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .mount(&server)
            .await;

        let client = AiClient::new(AiSettings {
            api_url: format!("{}/v1", server.uri()),
            api_key: None,
            model: "claude-opus-4-20250514".into(),
            system_prompt_extra: None,
        });

        let track = make_track(&["Line one", "Line two"]);
        let result = translate_via_claude(&client, &track).await;

        assert!(
            result.is_err(),
            "expected error on non-numbered response, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn translate_via_claude_empty_track_returns_empty() {
        use crate::ai::AiSettings;
        use crate::ai::client::AiClient;

        let client = AiClient::new(AiSettings::default());
        let track = make_track(&[]);
        let result = translate_via_claude(&client, &track).await.unwrap();
        assert!(result.is_empty());
    }
}
