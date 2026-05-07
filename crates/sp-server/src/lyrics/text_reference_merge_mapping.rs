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
//! Output schema: sparse pairs — `{"assignments": [{"a": <asr_idx>, "l":
//! <ref_idx>}, ...]}`. Claude lists only words it wants to map; ASR words not
//! in the list default to "skip". The first iteration used a dense
//! `[<int|null>, ...]` array of length == asr_count, but Claude reliably
//! miscounts arrays of 250+ elements (off by 5-15 entries on the id=132 test
//! song). The sparse format sidesteps the counting problem entirely.
//!
//! Validation rules: assignments' `a` strictly increasing, `l` monotonic
//! non-decreasing, both within their respective bounds. Output rejected (and
//! NW DP fallback runs) if any rule violated.

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::ai::client::AiClient;

use super::{AsrWord, LineEmit};

#[derive(Debug, Deserialize)]
struct ClaudeMappingResponse {
    assignments: Vec<Assignment>,
    /// Per spec 2026-05-07: Claude may return additional reference lines
    /// for runs of unmatched ASR words ≥ 5 s. `default` so older Claude
    /// responses (no `added_ref_lines` key) still parse.
    #[serde(default)]
    added_ref_lines: Vec<AddedRefLine>,
}

#[derive(Debug, Deserialize)]
struct Assignment {
    /// ASR word index — into the flattened ASR word stream.
    a: usize,
    /// Reference line index — into the ORIGINAL description's line list.
    /// Pipeline rewrites these into expanded indices after augmentation.
    l: usize,
}

/// Reference line added by Claude to fill a missing-section gap.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AddedRefLine {
    pub after_line: usize,
    pub text: String,
}

/// Phase 1 Claude output: dense mapping + added reference lines.
pub(crate) struct MappingResult {
    pub mapping: Vec<Option<usize>>,
    pub added: Vec<AddedRefLine>,
}

pub(super) async fn claude_map_words_to_lines(
    ai_client: &AiClient,
    ref_lines: &[String],
    asr_words: &[AsrWord],
) -> Result<MappingResult, anyhow::Error> {
    if ref_lines.is_empty() || asr_words.is_empty() {
        anyhow::bail!("empty input to claude line-mapping");
    }
    let prompt = build_mapping_prompt(ref_lines, asr_words);
    let raw = ai_client.chat("", &prompt).await?;
    let parsed: ClaudeMappingResponse = parse_first_json_object(&raw)?;
    let mapping = sparse_to_dense(&parsed.assignments, ref_lines.len(), asr_words.len())?;
    let added = validate_added_ref_lines(&parsed.added_ref_lines, ref_lines.len());
    Ok(MappingResult { mapping, added })
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
    let n_ref = ref_lines.len();
    let n_asr = asr_words.len();
    format!(
        r#"You receive a worship-song reference text (clean lines from a YouTube description) and a WhisperX audio transcription word stream (with mishearings and possible filler).

TASK: list which ASR words map to which reference lines, AND list any reference lines that should be ADDED to fill audio sections the description omitted.

OUTPUT SCHEMA: {{
  "assignments": [{{"a": <asr_word_idx>, "l": <ref_line_idx>}}, ...],
  "added_ref_lines": [{{"after_line": <ref_line_idx>, "text": "<string>"}}, ...]
}}

ASSIGNMENTS — each entry says "ASR word `a` belongs to reference line `l`". Words you omit default to skip (filler, mishearings, instrumental, ad-libs, chorus-repeat words for a separate pass).

ASSIGNMENT VALIDATION RULES (your output is rejected if any rule is broken):
- `a` values STRICTLY INCREASING — each entry's `a` is greater than the previous entry's `a`. No duplicates.
- `l` values MONOTONIC NON-DECREASING — each entry's `l` is >= the previous entry's `l`. Once you advance past line K, no later assignment maps to a line < K.
- `a` in [0, {n_asr}). `l` in [0, {n_ref}).
- A reference line MAY receive zero assignments (singer dropped it) — that's fine.
- For chorus REPEATS in audio: assign words to the FIRST occurrence in the reference and OMIT later repeats. A separate pass handles repeats.
- Use ASR start_ms timing to pick natural phrase boundaries when ambiguous.

ADDED REFERENCE LINES — fill missing-section gaps the description omitted:
- Add a line ONLY when the audio contains a run of unmatched ASR words ≥ 5 SECONDS long that you cannot map to any existing reference line.
- Each added line's text MUST be grammatically clean (capitalised, punctuated, mishearings corrected against song context).
- Each added line MUST be ≤ 64 characters; longer phrasing splits into multiple added lines, all sharing the same after_line index in the order they were sung.
- `after_line` is the index in REFERENCE LINES (the original numbered list below) AFTER WHICH the added line semantically belongs. Use the index of the closest preceding description line whose mapped audio ends BEFORE the unmatched run.
- DO NOT add lines for unmatched runs < 5 seconds — those are typically filler ("Say", "Oh") or chorus-repeats that a separate pass will recover.
- DO NOT modify, reorder, or drop existing reference lines — only the assignments field assigns audio to them.
- DO NOT add lines that DUPLICATE existing reference text exactly — those are chorus-repeats; leave them unmatched and the chorus-repeat pass will re-emit the existing line.
- If the description is complete (no ≥ 5 s unmatched runs), return `"added_ref_lines": []`.

REFERENCE LINES (numbered):
{ref_repr}

ASR WORD STREAM (numbered, with timing):
{asr_repr}

First char of response = `{{`. No prose, no fences, no markdown."#
    )
}

/// Convert sparse Claude assignments to a dense `Vec<Option<usize>>` of length
/// `n_asr`. Validates strict-increasing `a`, monotonic `l`, and range bounds in
/// the same pass. ASR word indices not listed in `assignments` default to None
/// (skip). This shape is robust to Claude failing to count to `n_asr` exactly:
/// the model just lists what it wants to map and we fill in the rest.
fn sparse_to_dense(
    assignments: &[Assignment],
    n_ref: usize,
    n_asr: usize,
) -> Result<Vec<Option<usize>>, anyhow::Error> {
    let mut dense = vec![None; n_asr];
    let mut last_a: Option<usize> = None;
    let mut last_l: Option<usize> = None;
    for (i, a) in assignments.iter().enumerate() {
        if a.a >= n_asr {
            anyhow::bail!(
                "assignment[{}] asr idx {} out of range [0..{})",
                i,
                a.a,
                n_asr
            );
        }
        if a.l >= n_ref {
            anyhow::bail!(
                "assignment[{}] line idx {} out of range [0..{})",
                i,
                a.l,
                n_ref
            );
        }
        if let Some(prev) = last_a {
            if a.a <= prev {
                anyhow::bail!(
                    "assignment[{}] asr idx not strictly increasing: {} after {}",
                    i,
                    a.a,
                    prev
                );
            }
        }
        if let Some(prev) = last_l {
            if a.l < prev {
                anyhow::bail!(
                    "assignment[{}] line idx not monotonic: {} after {}",
                    i,
                    a.l,
                    prev
                );
            }
        }
        dense[a.a] = Some(a.l);
        last_a = Some(a.a);
        last_l = Some(a.l);
    }
    Ok(dense)
}

/// Drop AddedRefLine entries whose `after_line` is out of range. Logs warn.
/// Stable-sort by `after_line` so Phase 1.5's prev_added_end monotonic
/// advance picks each line's correct gap window. Same-after_line entries
/// retain input order so process()'s K-th counter slot calc stays
/// consistent with expand_ref_lines's iter-and-insert.
pub(crate) fn validate_added_ref_lines(
    added: &[AddedRefLine],
    n_orig_ref: usize,
) -> Vec<AddedRefLine> {
    let mut out: Vec<AddedRefLine> = added
        .iter()
        .filter(|a| {
            if a.after_line >= n_orig_ref {
                tracing::warn!(
                    after_line = a.after_line,
                    n_orig_ref,
                    text = %a.text,
                    "text_reference_merge: dropping added_ref_line — after_line out of range"
                );
                false
            } else {
                true
            }
        })
        .cloned()
        .collect();
    out.sort_by_key(|a| a.after_line);
    out
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

    fn assn(a: usize, l: usize) -> Assignment {
        Assignment { a, l }
    }

    #[test]
    fn sparse_to_dense_fills_skips_with_none() {
        // 6 ASR words; assignments only cover 0,1,3,5. Words 2,4 default skip.
        let asgs = vec![assn(0, 0), assn(1, 0), assn(3, 1), assn(5, 1)];
        let dense = sparse_to_dense(&asgs, 2, 6).unwrap();
        assert_eq!(dense, vec![Some(0), Some(0), None, Some(1), None, Some(1)]);
    }

    #[test]
    fn sparse_to_dense_empty_assignments_yields_all_none() {
        let dense = sparse_to_dense(&[], 3, 4).unwrap();
        assert_eq!(dense, vec![None, None, None, None]);
    }

    #[test]
    fn sparse_to_dense_robust_to_short_list() {
        // Claude returned only 2 entries for a 100-ASR-word stream — common
        // length-counting failure mode that the old dense-array schema
        // tripped on. Now: just fill the rest with None.
        let asgs = vec![assn(0, 0), assn(1, 0)];
        let dense = sparse_to_dense(&asgs, 2, 100).unwrap();
        assert_eq!(dense.len(), 100);
        assert_eq!(dense[0], Some(0));
        assert_eq!(dense[1], Some(0));
        assert!(dense[2..].iter().all(|x| x.is_none()));
    }

    #[test]
    fn sparse_to_dense_rejects_asr_idx_out_of_range() {
        let asgs = vec![assn(0, 0), assn(99, 1)];
        let err = sparse_to_dense(&asgs, 2, 5).unwrap_err();
        assert!(format!("{err}").contains("out of range"));
    }

    #[test]
    fn sparse_to_dense_rejects_line_idx_out_of_range() {
        let asgs = vec![assn(0, 0), assn(1, 9)];
        let err = sparse_to_dense(&asgs, 2, 5).unwrap_err();
        assert!(format!("{err}").contains("out of range"));
    }

    #[test]
    fn sparse_to_dense_rejects_non_strict_increasing_a() {
        let asgs = vec![assn(0, 0), assn(0, 1)]; // duplicate a
        let err = sparse_to_dense(&asgs, 2, 5).unwrap_err();
        assert!(format!("{err}").contains("strictly increasing"));
    }

    #[test]
    fn sparse_to_dense_rejects_decreasing_a() {
        let asgs = vec![assn(2, 0), assn(1, 1)];
        let err = sparse_to_dense(&asgs, 2, 5).unwrap_err();
        assert!(format!("{err}").contains("strictly increasing"));
    }

    #[test]
    fn sparse_to_dense_rejects_non_monotonic_l() {
        let asgs = vec![assn(0, 1), assn(1, 0)];
        let err = sparse_to_dense(&asgs, 2, 5).unwrap_err();
        assert!(format!("{err}").contains("monotonic"));
    }

    #[test]
    fn sparse_to_dense_allows_equal_consecutive_l() {
        // Common case: multiple ASR words map to same ref line.
        let asgs = vec![assn(0, 0), assn(1, 0), assn(2, 0)];
        sparse_to_dense(&asgs, 1, 3).unwrap();
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

    #[test]
    fn parse_first_json_object_assignments_clean() {
        let raw = r#"{"assignments":[{"a":0,"l":0},{"a":1,"l":0},{"a":3,"l":1}]}"#;
        let p: ClaudeMappingResponse = parse_first_json_object(raw).unwrap();
        assert_eq!(p.assignments.len(), 3);
        assert_eq!((p.assignments[0].a, p.assignments[0].l), (0, 0));
        assert_eq!((p.assignments[2].a, p.assignments[2].l), (3, 1));
    }

    #[test]
    fn parse_first_json_object_strips_prose() {
        let raw = "Sure! Here:\n```json\n{\"assignments\":[{\"a\":0,\"l\":1}]}\n```";
        let p: ClaudeMappingResponse = parse_first_json_object(raw).unwrap();
        assert_eq!(p.assignments.len(), 1);
        assert_eq!((p.assignments[0].a, p.assignments[0].l), (0, 1));
    }

    #[test]
    fn parse_first_json_object_handles_nested_braces() {
        let raw = r#"{"assignments":[{"a":0,"l":0}],"meta":{"nested":true}}"#;
        let p: ClaudeMappingResponse = parse_first_json_object(raw).unwrap();
        assert_eq!(p.assignments.len(), 1);
    }

    #[test]
    fn parse_first_json_object_handles_braces_in_strings() {
        let raw = r#"{"note":"has } brace","assignments":[{"a":0,"l":0}]}"#;
        let p: ClaudeMappingResponse = parse_first_json_object(raw).unwrap();
        assert_eq!(p.assignments.len(), 1);
    }

    #[test]
    fn parse_first_json_object_malformed_returns_err() {
        let raw = "no json here";
        let r: Result<ClaudeMappingResponse, _> = parse_first_json_object(raw);
        assert!(r.is_err());
    }

    #[test]
    fn parse_mapping_response_extracts_added_ref_lines() {
        let raw = r#"{
            "assignments": [{"a": 0, "l": 0}, {"a": 1, "l": 0}],
            "added_ref_lines": [
                {"after_line": 0, "text": "There's no place I'd rather be"},
                {"after_line": 0, "text": "No one like the king"}
            ]
        }"#;
        let p: ClaudeMappingResponse = parse_first_json_object(raw).unwrap();
        assert_eq!(p.assignments.len(), 2);
        assert_eq!(p.added_ref_lines.len(), 2);
        assert_eq!(p.added_ref_lines[0].after_line, 0);
        assert_eq!(p.added_ref_lines[0].text, "There's no place I'd rather be");
        assert_eq!(p.added_ref_lines[1].text, "No one like the king");
    }

    #[test]
    fn parse_mapping_response_handles_missing_added_field() {
        let raw = r#"{"assignments":[{"a":0,"l":0}]}"#;
        let p: ClaudeMappingResponse = parse_first_json_object(raw).unwrap();
        assert_eq!(p.assignments.len(), 1);
        assert!(p.added_ref_lines.is_empty());
    }

    #[test]
    fn validate_added_drops_out_of_range_after_line() {
        let added = vec![
            AddedRefLine {
                after_line: 0,
                text: "in range".into(),
            },
            AddedRefLine {
                after_line: 99,
                text: "out of range".into(),
            },
        ];
        let validated = validate_added_ref_lines(&added, 3);
        assert_eq!(validated.len(), 1);
        assert_eq!(validated[0].text, "in range");
    }

    #[test]
    fn validate_added_ref_lines_sorts_by_after_line() {
        // Out-of-order input → output must be sorted ascending by after_line.
        // Same-after_line entries keep their input order (stable sort) so
        // the K-th-counter slot calc in process() stays correct.
        let added = vec![
            AddedRefLine {
                after_line: 3,
                text: "C".into(),
            },
            AddedRefLine {
                after_line: 1,
                text: "A".into(),
            },
            AddedRefLine {
                after_line: 1,
                text: "B".into(),
            },
            AddedRefLine {
                after_line: 5,
                text: "D".into(),
            },
        ];
        let validated = validate_added_ref_lines(&added, 10);
        assert_eq!(validated.len(), 4);
        assert_eq!(validated[0].text, "A"); // after_line=1, first input
        assert_eq!(validated[1].text, "B"); // after_line=1, second input (stable)
        assert_eq!(validated[2].text, "C"); // after_line=3
        assert_eq!(validated[3].text, "D"); // after_line=5
    }
}
