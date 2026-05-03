//! Phase 1 primary algorithm for description/override merge: Claude-driven
//! word→line mapping. Each WhisperX audio word gets assigned to a reference
//! line index (or null = skip) under a strict monotonic-line-order constraint.
//!
//! Why Claude here. The Needleman-Wunsch DP fallback (`match_ref_to_asr`) is
//! mathematically deterministic and globally optimal under its weighted
//! edit-cost model — but the cost weights are global constants, not aware of
//! semantic phrasing. On highly repetitive worship lyrics ("Holy holy holy",
//! "Forever and ever amen") the DP can converge on a sparse alignment that
//! attaches an early ASR word to a much-later ref line, producing a 0.8 s
//! flash on the LED wall. Claude reads the actual phrasing, knows worship-song
//! structure, and groups semantically — id=132 wall verification 2026-05-03
//! drove this design.
//!
//! On any failure (network error, schema parse, validation, refusal) the
//! caller falls back to the deterministic NW DP. Claude is best-effort
//! intelligence on top of a guaranteed-correct floor.
//!
//! Output schema: `{"l": [<int|null>, ...]}` — array length must equal the
//! ASR word count. Validated for monotonic non-decreasing line indices and
//! in-range values before use.

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::ai::client::AiClient;

use super::{AsrWord, LineEmit};

#[derive(Debug, Deserialize)]
struct ClaudeMappingResponse {
    l: Vec<Option<usize>>,
}

pub(super) async fn claude_map_words_to_lines(
    ai_client: &AiClient,
    ref_lines: &[String],
    asr_words: &[AsrWord],
) -> Result<Vec<Option<usize>>, anyhow::Error> {
    if ref_lines.is_empty() || asr_words.is_empty() {
        anyhow::bail!("empty input to claude line-mapping");
    }
    let prompt = build_mapping_prompt(ref_lines, asr_words);
    let raw = ai_client.chat("", &prompt).await?;
    let parsed: ClaudeMappingResponse = parse_first_json_object(&raw)?;
    validate_mapping(&parsed.l, ref_lines.len(), asr_words.len())?;
    Ok(parsed.l)
}

fn build_mapping_prompt(ref_lines: &[String], asr_words: &[AsrWord]) -> String {
    let ref_repr = ref_lines
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{i}: {t}"))
        .collect::<Vec<_>>()
        .join("\n");
    let asr_repr = asr_words
        .iter()
        .enumerate()
        .map(|(i, w)| format!("{i}: {} ({}-{}ms)", w.norm, w.start_ms, w.end_ms))
        .collect::<Vec<_>>()
        .join("\n");
    let n = asr_words.len();
    format!(
        r#"You receive a worship-song reference text (clean lines from a YouTube description) and a WhisperX audio transcription word stream (with mishearings and possible filler).

TASK: assign each ASR word to a reference line index, or null = skip.

CONSTRAINTS:
- Line indices MUST be monotonic non-decreasing across the ASR stream — once you advance past line K, no later ASR word maps to a line < K.
- Skip filler / mishearings / instrumental noise / silence with null.
- Words that don't fit any reference line (ad-libs, "yeah", "oh") → null.
- For chorus REPEATS in audio, assign repeated words to the FIRST occurrence in the reference and emit null for the rest. A separate pass handles repeats.
- Use ASR start_ms timing to pick natural phrase boundaries when ambiguous.
- A reference line MAY legitimately receive ZERO ASR words (singer dropped it) — that's fine.

REFERENCE LINES (numbered):
{ref_repr}

ASR WORD STREAM (numbered, with timing):
{asr_repr}

OUTPUT: ONLY a JSON object. Schema: {{"l": [<int|null>, ...]}}

The array length MUST equal {n} (the ASR word count). Each element is the reference line index for that ASR word, or null to skip.

First char of response = `{{`. No prose, no fences, no markdown."#
    )
}

pub(super) fn validate_mapping(
    map: &[Option<usize>],
    n_ref: usize,
    n_asr: usize,
) -> Result<(), anyhow::Error> {
    if map.len() != n_asr {
        anyhow::bail!("claude mapping length {} != asr count {}", map.len(), n_asr);
    }
    let mut last: Option<usize> = None;
    for (i, m) in map.iter().enumerate() {
        if let Some(idx) = m {
            if *idx >= n_ref {
                anyhow::bail!("line idx {} out of range [0..{})", idx, n_ref);
            }
            if let Some(prev) = last {
                if *idx < prev {
                    anyhow::bail!(
                        "non-monotonic at asr[{}]: line {} after line {}",
                        i,
                        idx,
                        prev
                    );
                }
            }
            last = Some(*idx);
        }
    }
    Ok(())
}

pub(super) fn emits_from_mapping(map: &[Option<usize>], ref_lines: &[String]) -> Vec<LineEmit> {
    let mut indices_per_line: Vec<Vec<usize>> = vec![Vec::new(); ref_lines.len()];
    for (i, m) in map.iter().enumerate() {
        if let Some(li) = m {
            if *li < ref_lines.len() {
                indices_per_line[*li].push(i);
            }
        }
    }
    ref_lines
        .iter()
        .zip(indices_per_line)
        .map(|(text, indices)| LineEmit {
            text: text.clone(),
            asr_word_indices: indices,
        })
        .collect()
}

pub(super) fn parse_first_json_object<T: DeserializeOwned>(raw: &str) -> Result<T, anyhow::Error> {
    let s = raw.trim();
    let bytes = s.as_bytes();
    let mut depth: i32 = 0;
    let mut start: Option<usize> = None;
    let mut end: Option<usize> = None;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate() {
        if esc {
            esc = false;
            continue;
        }
        if in_str && b == b'\\' {
            esc = true;
            continue;
        }
        if b == b'"' {
            in_str = !in_str;
            continue;
        }
        if in_str {
            continue;
        }
        if b == b'{' {
            if start.is_none() {
                start = Some(i);
            }
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                end = Some(i + 1);
                break;
            }
        }
    }
    let (s_idx, e_idx) = match (start, end) {
        (Some(a), Some(b)) => (a, b),
        _ => return Err(anyhow::anyhow!("no balanced JSON object in response")),
    };
    let json_slice = &s[s_idx..e_idx];
    Ok(serde_json::from_str(json_slice)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_mapping_accepts_valid() {
        let m = vec![Some(0), None, Some(0), Some(1), None, Some(1)];
        validate_mapping(&m, 2, 6).unwrap();
    }

    #[test]
    fn validate_mapping_accepts_all_null() {
        let m = vec![None, None, None];
        validate_mapping(&m, 3, 3).unwrap();
    }

    #[test]
    fn validate_mapping_rejects_wrong_length() {
        let m = vec![Some(0), None];
        let err = validate_mapping(&m, 1, 5).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("length 2"), "{}", msg);
    }

    #[test]
    fn validate_mapping_rejects_out_of_range() {
        let m = vec![Some(0), Some(2)];
        let err = validate_mapping(&m, 1, 2).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("out of range"), "{}", msg);
    }

    #[test]
    fn validate_mapping_rejects_non_monotonic() {
        let m = vec![Some(1), Some(0)];
        let err = validate_mapping(&m, 2, 2).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("non-monotonic"), "{}", msg);
    }

    #[test]
    fn validate_mapping_allows_equal_consecutive() {
        // Multiple ASR words mapping to the same line — the common case.
        let m = vec![Some(0), Some(0), Some(0), Some(1), Some(1)];
        validate_mapping(&m, 2, 5).unwrap();
    }

    #[test]
    fn emits_from_mapping_groups_by_line() {
        let ref_lines = vec!["alpha".to_string(), "bravo".to_string()];
        let map = vec![Some(0), Some(0), None, Some(1), Some(1)];
        let emits = emits_from_mapping(&map, &ref_lines);
        assert_eq!(emits.len(), 2);
        assert_eq!(emits[0].text, "alpha");
        assert_eq!(emits[0].asr_word_indices, vec![0, 1]);
        assert_eq!(emits[1].text, "bravo");
        assert_eq!(emits[1].asr_word_indices, vec![3, 4]);
    }

    #[test]
    fn emits_from_mapping_empty_line_gets_no_words() {
        let ref_lines = vec!["a".into(), "b".into(), "c".into()];
        let map = vec![Some(0), Some(2)];
        let emits = emits_from_mapping(&map, &ref_lines);
        assert_eq!(emits[0].asr_word_indices, vec![0]);
        assert!(emits[1].asr_word_indices.is_empty());
        assert_eq!(emits[2].asr_word_indices, vec![1]);
    }

    #[test]
    fn emits_from_mapping_ignores_out_of_range_idx() {
        // Defensive: shouldn't happen post-validate, but emits_from_mapping is
        // pure data so guard anyway.
        let ref_lines = vec!["only".to_string()];
        let map = vec![Some(0), Some(99)];
        let emits = emits_from_mapping(&map, &ref_lines);
        assert_eq!(emits.len(), 1);
        assert_eq!(emits[0].asr_word_indices, vec![0]);
    }

    #[derive(Deserialize)]
    struct TestWrap {
        l: Vec<Option<usize>>,
    }

    #[test]
    fn parse_first_json_object_clean() {
        let raw = r#"{"l":[0,null,1]}"#;
        let p: TestWrap = parse_first_json_object(raw).unwrap();
        assert_eq!(p.l, vec![Some(0), None, Some(1)]);
    }

    #[test]
    fn parse_first_json_object_strips_prose() {
        let raw = "Sure! Here:\n```json\n{\"l\":[1,2]}\n```";
        let p: TestWrap = parse_first_json_object(raw).unwrap();
        assert_eq!(p.l, vec![Some(1), Some(2)]);
    }

    #[test]
    fn parse_first_json_object_handles_nested_braces() {
        let raw = r#"{"l":[0,1],"meta":{"nested":true}}"#;
        let p: TestWrap = parse_first_json_object(raw).unwrap();
        assert_eq!(p.l, vec![Some(0), Some(1)]);
    }

    #[test]
    fn parse_first_json_object_handles_braces_in_strings() {
        // Brace inside a string literal must NOT confuse depth tracking.
        let raw = r#"{"note":"has } brace","l":[0]}"#;
        let p: TestWrap = parse_first_json_object(raw).unwrap();
        assert_eq!(p.l, vec![Some(0)]);
    }

    #[test]
    fn parse_first_json_object_malformed_returns_err() {
        let raw = "no json here";
        let r: Result<TestWrap, _> = parse_first_json_object(raw);
        assert!(r.is_err());
    }
}
