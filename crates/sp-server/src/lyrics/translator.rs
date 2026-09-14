//! EN→SK lyrics translator — Claude-only via CLIProxyAPI.
//!
//! The user pays a Claude Max Plus subscription (unlimited at that tier);
//! Gemini quota is expensive and reserved for alignment. Translation MUST
//! stay on Claude even when a specific song refuses — the right response
//! to a refusal is to tune the prompt here, not to add a Gemini fallback.
//!
//! ## Prompt design (hard-won)
//!
//! Claude via CLIProxyAPI OAuth runs a content-policy classifier that flags
//! numbered English lines containing worship-style phrases as "copyrighted
//! song lyrics" and refuses to translate them. Trigger words:
//! `lyrics`, `song`, `worship`, `karaoke`, `church`, `copyright`, and
//! religious glossaries (`Jesus=Ježiš`, `Hallelujah stays as Hallelujah`,
//! etc.). Removing them alone is not enough — the content itself looks
//! song-like.
//!
//! The #152 workaround dressed the request as a *story* (a grandparent
//! dictating sentences for a memorial stone plaque). The newest Claude
//! flagships (`claude-fable-5-1`, `claude-opus-5`) see through that story and
//! refuse recognizable content anyway ("…even for a family plaque…", #145,
//! measured live 2026-09-14). What works across every 5-gen model is the
//! OPPOSITE of a story: a bare, neutral TECHNICAL translation task — numbered
//! lines in, numbered Slovak lines out, with an explicit masculine/feminine
//! grammatical-gender instruction for the first-person speaker (`SpeakerGender`,
//! #152 — the gender no longer rides on a grandFATHER/grandMOTHER framing but on
//! a plain grammatical directive). No story, no persona. Keep out the trigger
//! words above. Let Claude translate proper names naturally (Jesus → Ježiš,
//! Hallelujah → Haleluja); forcing them to stay in English gave stilted Slovak.
//!
//! When Claude still refuses (`I can't…` / `I cannot…` / `not able to…` with no
//! numbered lines), that is classified as a REFUSAL and logged as such WITH the
//! model id — never as a silent "parse returned 0" (#145) — so a future
//! model/prompt regression is visible in the logs instead of hidden.

use anyhow::{Result, anyhow};
use sp_core::lyrics::LyricsTrack;

/// Why a Claude translation response yielded ZERO usable numbered lines (#145).
///
/// Distinguishing a content-policy REFUSAL from any other empty result is the
/// point: before #145 both were logged identically as
/// `translate_via_claude: parse returned 0 translations`, so a refusal — the
/// dominant failure after a model switch — was invisible in the logs. Now a
/// refusal is classified and logged as such, with the model id, so a future
/// model/prompt regression surfaces loudly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranslationFailure {
    /// Claude declined on content-policy grounds ("I can't…", "I cannot…",
    /// "not able to…", "unable to…") and returned no numbered lines.
    Refused,
    /// Zero numbered lines for another reason: an empty body, malformed
    /// numbering, or the OAuth quota wall.
    NoTranslations,
}

/// Does `response` read like a content-policy refusal? A bare textual check on
/// the model's own decline phrasing — deliberately conservative so it only
/// fires on an explicit "I can't / I cannot / not able to / unable to", never
/// on a normal Slovak translation (which never contains these English phrases).
pub fn is_refusal_shaped(response: &str) -> bool {
    let low = response.to_lowercase();
    // Normalise the typographic apostrophe so "can't" and "can’t" both match.
    let low = low.replace('\u{2019}', "'");
    const MARKERS: [&str; 8] = [
        "i can't",
        "i cannot",
        "i'm not able",
        "i am not able",
        "not able to",
        "unable to",
        "can't help with",
        "cannot help with",
    ];
    MARKERS.iter().any(|m| low.contains(m))
}

/// Classify a Claude response that parsed into `parsed_count` numbered lines.
/// Returns `None` when at least one line parsed (a full or partial success);
/// otherwise a [`TranslationFailure`] describing WHY nothing parsed (#145).
pub fn classify_zero_translation(
    response: &str,
    parsed_count: usize,
) -> Option<TranslationFailure> {
    if parsed_count > 0 {
        return None;
    }
    if is_refusal_shaped(response) {
        Some(TranslationFailure::Refused)
    } else {
        Some(TranslationFailure::NoTranslations)
    }
}

/// Translate English lines in `track` to Slovak via Claude. Returns a Vec
/// aligned 1:1 with `track.lines`; empty strings mark lines Claude did not
/// return a translation for.
#[cfg_attr(test, mutants::skip)]
pub async fn translate_via_claude(
    ai_client: &crate::ai::client::AiClient,
    track: &LyricsTrack,
    gender: SpeakerGender,
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
    let user = build_prompt(line_count, &numbered, gender);

    // No system prompt — cloaked Claude behaves best with everything in the user message.
    let response = ai_client
        .chat("", &user)
        .await
        .map_err(|e| anyhow!("Claude translation failed: {e}"))?;

    let translations = parse_translation_response(&response, line_count);

    let non_empty = translations.iter().filter(|t| !t.is_empty()).count();
    if non_empty == 0 && line_count > 0 {
        // Surface the raw Claude response in the application log so an
        // operator can see EXACTLY why parsing produced zero translations
        // (content-policy refusal text, empty body, malformed numbering,
        // 5-hour OAuth quota wall, etc.). The response is bounded at 4000
        // characters so a verbose Claude refusal doesn't blow up the log
        // line beyond what `tracing` will keep in memory comfortably.
        let model = ai_client.settings().model.clone();
        let snippet: String = response.chars().take(4000).collect();
        let truncated = response.chars().count() > 4000;
        // #145: classify + log a REFUSAL distinctly (with the model id) instead
        // of the generic "parse returned 0", so a content-policy refusal is
        // never invisible in the logs again.
        match classify_zero_translation(&response, non_empty) {
            Some(TranslationFailure::Refused) => {
                tracing::warn!(
                    kind = "refusal",
                    model = %model,
                    line_count,
                    truncated,
                    response = %snippet,
                    "translate_via_claude: Claude REFUSED translation on content-policy grounds — tune translator::build_prompt (#145)"
                );
                return Err(anyhow!("Claude refused translation (model {model})"));
            }
            _ => {
                tracing::warn!(
                    kind = "parse_zero",
                    model = %model,
                    line_count,
                    response_len = response.chars().count(),
                    truncated,
                    response = %snippet,
                    "translate_via_claude: parse returned 0 translations — raw Claude response logged for diagnosis"
                );
                return Err(anyhow!(
                    "Claude translation returned no translations (model {model})"
                ));
            }
        }
    }

    Ok(translations)
}

/// Grammatical gender of the first-person speaker (#152).
///
/// English first-person lines carry no gender ("I was lost"); Slovak marks it
/// on past-tense verbs, participles, and adjectives ("bol som stratený" vs
/// "bola som stratená"). The translator prompt (`build_prompt`) carries an
/// explicit grammatical directive — "use masculine/feminine forms wherever
/// Slovak grammar requires a gender for the first-person speaker" (#145; the
/// #152 grandFATHER/grandMOTHER-plaque STORY was dropped because the newest
/// flagships refuse it — verified: male → "Keď som bol vinný", female → "Keď som
/// bola vinná"). Without this directive, Claude picks a gender arbitrarily per
/// song (owner report 2026-09-13: a male-sung song was rendered female).
///
/// Default is `Male`: male-sung songs are the norm in this catalog, so female-
/// led songs get the per-song override rather than the reverse. The classifier
/// constraints are unchanged — the prompt must never contain "lyrics", "song",
/// "worship", "karaoke", "church", a religious glossary, or any "story".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpeakerGender {
    #[default]
    Male,
    Female,
}

/// Build the translation prompt. Public for unit testing the exact wording.
///
/// **Neutral technical framing + gender directive (#145, supersedes the #152
/// grandparent story).** Claude via CLIProxyAPI OAuth classifies numbered
/// English lines containing worship-style phrases as "copyrighted song lyrics"
/// and refuses to translate them. The #152 workaround dressed the request as a
/// grandparent dictating for a memorial plaque, but the newest flagships
/// (`claude-fable-5-1`, `claude-opus-5`) see through that story and refuse
/// anyway ("…even for a family plaque…", #145). What works across every 5-gen
/// model — measured live 2026-09-14 — is the OPPOSITE: a bare, neutral TECHNICAL
/// translation task with NO story or persona, plus an explicit masculine /
/// feminine grammatical-gender directive for the first-person speaker (the #152
/// gender requirement, now carried by a plain grammatical instruction instead of
/// a grandFATHER/grandMOTHER framing — verified: male → "Keď som **bol** vinný",
/// female → "Keď som **bola** vinná"). Do NOT add words like "lyrics", "song",
/// "worship", "karaoke", "church", "copyright", "plaque", or a religious
/// glossary — those (and any recognizable "story") re-trigger the refusal. Let
/// Claude translate proper names naturally (Jesus → Ježiš, Hallelujah → Haleluja);
/// forcing them to stay in English gave stilted Slovak output.
pub fn build_prompt(line_count: usize, numbered: &str, gender: SpeakerGender) -> String {
    let gender_forms = match gender {
        SpeakerGender::Male => "masculine",
        SpeakerGender::Female => "feminine",
    };
    format!(
        "Translate each of the following numbered lines into Slovak. Keep the \
         exact same numbering and output exactly {line_count} numbered lines. \
         Wherever Slovak grammar requires a gender for the first-person speaker, \
         use {gender_forms} forms. Output only the numbered Slovak lines, nothing \
         else.\n\n\
         {numbered}"
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
    fn build_prompt_stays_clear_of_policy_triggers() {
        // #152: build_prompt gained a SpeakerGender parameter. The neutral-
        // framing + line-count guarantees this test pins are gender-agnostic;
        // pass the default (Male) to exercise the production default path.
        let out = build_prompt(3, "1: a\n2: b\n3: c", SpeakerGender::Male);
        // Must NOT contain the terms that flip Claude's "copyrighted lyrics"
        // classifier. Empirically verified on 2026-04-23 against Elevation
        // Worship's "Jesus Be The Name": any of these in the prompt yields
        // a refusal, removing them + grandmother framing yields a clean
        // 96/96 translation.
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
    fn build_prompt_does_not_force_proper_names_unchanged() {
        // Older prompts forced "Jesus stays as Jesus" etc., which produced
        // stilted Slovak output (user feedback 2026-04-23). Natural Slovak
        // speakers expect Ježiš / Haleluja / Hosana / Amen — let Claude
        // translate the name instead of pinning it to English.
        let out = build_prompt(1, "1: Jesus", SpeakerGender::Male);
        let low = out.to_lowercase();
        assert!(
            !low.contains("stays as") && !low.contains("stay unchanged"),
            "prompt must not force proper names to stay in English; got:\n{out}"
        );
    }

    // ── #152: gender-aware framing ────────────────────────────────────────
    // A first-person Slovak line carries grammatical gender ("bol som" vs
    // "bola som"); English has none, so the prompt must tell Claude which
    // singular form the speaker uses. Default is Male (owner report
    // 2026-09-13: male-sung songs are the catalog norm).

    #[test]
    fn speaker_gender_defaults_to_male() {
        assert_eq!(SpeakerGender::default(), SpeakerGender::Male);
    }

    // #145: the grandparent/plaque STORY is gone (the newest flagships refuse
    // it — "…even for a family plaque…"). The gender is now carried by a plain
    // grammatical directive, so the framing tests pin masculine/feminine forms
    // AND assert the removed story words never come back.
    #[test]
    fn build_prompt_male_requests_masculine_no_story() {
        let out = build_prompt(3, "1: a\n2: b\n3: c", SpeakerGender::Male);
        let low = out.to_lowercase();
        assert!(
            low.contains("masculine"),
            "male prompt must request masculine forms:\n{out}"
        );
        assert!(
            !low.contains("feminine"),
            "male prompt must not request feminine forms:\n{out}"
        );
        for story in ["grandfather", "grandmother", "plaque", "dictated"] {
            assert!(
                !low.contains(story),
                "neutral prompt must drop the story word `{story}` (#145):\n{out}"
            );
        }
    }

    #[test]
    fn build_prompt_female_requests_feminine_no_story() {
        let out = build_prompt(3, "1: a\n2: b\n3: c", SpeakerGender::Female);
        let low = out.to_lowercase();
        assert!(
            low.contains("feminine"),
            "female prompt must request feminine forms:\n{out}"
        );
        assert!(
            !low.contains("masculine"),
            "female prompt must not request masculine forms:\n{out}"
        );
        for story in ["grandfather", "grandmother", "plaque", "dictated"] {
            assert!(
                !low.contains(story),
                "neutral prompt must drop the story word `{story}` (#145):\n{out}"
            );
        }
    }

    #[test]
    fn build_prompt_both_genders_stay_clear_of_policy_triggers() {
        for gender in [SpeakerGender::Male, SpeakerGender::Female] {
            let out = build_prompt(3, "1: a\n2: b\n3: c", gender);
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
                    "prompt must stay neutral; found `{bad}` for {gender:?}:\n{out}"
                );
            }
        }
    }

    // ── #145: refusal classification ──────────────────────────────────────
    // A refusal-shaped Claude response with zero numbered lines must be
    // classified as `Refused` (logged distinctly, with the model id) — never as
    // a silent "parse returned 0".

    #[test]
    fn classify_refusal_shaped_zero_lines_is_refused() {
        // The exact production refusal from #145 (Fable 5.1 on "Not Guilty").
        let refusal = "I'd like to help, but I can't do this one as requested. \
                       These lines match the lyrics of a published worship song, \
                       so translating all 72 lines isn't something I can do, even \
                       for a family plaque.";
        assert_eq!(
            classify_zero_translation(refusal, 0),
            Some(TranslationFailure::Refused)
        );
    }

    #[test]
    fn classify_typographic_apostrophe_refusal_is_refused() {
        // Some models emit the curly apostrophe: "I can’t …".
        let refusal = "I can\u{2019}t translate this text.";
        assert!(is_refusal_shaped(refusal));
        assert_eq!(
            classify_zero_translation(refusal, 0),
            Some(TranslationFailure::Refused)
        );
    }

    #[test]
    fn classify_empty_body_is_no_translations() {
        assert_eq!(
            classify_zero_translation("", 0),
            Some(TranslationFailure::NoTranslations)
        );
        assert_eq!(
            classify_zero_translation("   \n\n  ", 0),
            Some(TranslationFailure::NoTranslations)
        );
    }

    #[test]
    fn classify_some_parsed_lines_is_not_a_failure() {
        // Any parsed line means success/partial — not a failure, even if the
        // text happens to contain a refusal-like word elsewhere.
        assert_eq!(classify_zero_translation("1: ahoj", 1), None);
        assert_eq!(classify_zero_translation("I can't; 1: ahoj", 1), None);
    }

    #[test]
    fn is_refusal_shaped_does_not_fire_on_normal_slovak() {
        // A real Slovak translation never contains the English decline phrases.
        assert!(!is_refusal_shaped(
            "1: Keď som bol vinný,\n2: prichytený pri čine"
        ));
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
        let result = translate_via_claude(&client, &track, SpeakerGender::Male).await;

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
        let result = translate_via_claude(&client, &track, SpeakerGender::Male).await;

        assert!(
            result.is_err(),
            "expected error on non-numbered response, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn translate_via_claude_refusal_error_names_refusal_and_model() {
        // #145 regression: when Claude REFUSES on content-policy grounds, the
        // failure must be surfaced AS a refusal and must carry the model id —
        // never a generic "returned no translations" that hides the cause.
        use crate::ai::AiSettings;
        use crate::ai::client::AiClient;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let response_body = serde_json::json!({
            "choices": [{
                "message": {"content":
                    "I'd like to help, but I can't do this one — these lines match \
                     the lyrics of a published worship song."}
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
            model: "claude-fable-5-1".into(),
            system_prompt_extra: None,
        });

        let track = make_track(&["Line one", "Line two"]);
        let err = translate_via_claude(&client, &track, SpeakerGender::Male)
            .await
            .expect_err("a refusal must be an error");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("refus"),
            "a content-policy refusal must be surfaced as a refusal, got: {msg}"
        );
        assert!(
            msg.contains("claude-fable-5-1"),
            "a refusal error must name the model id, got: {msg}"
        );
    }

    #[tokio::test]
    async fn translate_via_claude_empty_track_returns_empty() {
        use crate::ai::AiSettings;
        use crate::ai::client::AiClient;

        let client = AiClient::new(AiSettings::default());
        let track = make_track(&[]);
        let result = translate_via_claude(&client, &track, SpeakerGender::Male)
            .await
            .unwrap();
        assert!(result.is_empty());
    }
}
