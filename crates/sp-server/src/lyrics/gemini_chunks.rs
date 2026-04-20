//! Chunk planning (how to slice the song into 60s/10s-overlap chunks) and
//! overlap-merge logic for stitching per-chunk timed-line outputs into a
//! single global timeline.

use crate::lyrics::gemini_parse::ParsedLine;

pub const CHUNK_DURATION_MS: u64 = 60_000;
pub const CHUNK_OVERLAP_MS: u64 = 10_000;
pub const CHUNK_STRIDE_MS: u64 = CHUNK_DURATION_MS - CHUNK_OVERLAP_MS; // 50_000

/// A planned chunk: range of the full song this chunk covers.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkPlan {
    pub idx: usize,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Plan the chunks for a song of the given total duration.
/// Always at least one chunk (unless duration_ms == 0, which returns empty).
/// Last chunk is clipped to duration_ms.
pub fn plan_chunks(duration_ms: u64) -> Vec<ChunkPlan> {
    let mut out = Vec::new();
    if duration_ms == 0 {
        return out;
    }
    let mut start = 0u64;
    let mut idx = 0usize;
    loop {
        let end = (start + CHUNK_DURATION_MS).min(duration_ms);
        out.push(ChunkPlan {
            idx,
            start_ms: start,
            end_ms: end,
        });
        if end >= duration_ms {
            break;
        }
        idx += 1;
        start += CHUNK_STRIDE_MS;
    }
    out
}

/// A line with globally-offset timing (chunk local_ms + chunk_start_ms).
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalLine {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// Normalize text for dedup: lowercase, keep alphanumerics and apostrophes,
/// drop other punctuation, collapse whitespace.
pub fn normalize_text(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut prev_space = true;
    for c in lower.chars() {
        if c.is_alphanumeric() || c == '\'' {
            out.push(c);
            prev_space = false;
        } else if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        }
        // else: drop punctuation
    }
    out.trim().to_string()
}

/// Merge per-chunk `ParsedLine` lists into a single ordered `GlobalLine` list.
///
/// Dedup rule: in the overlap region between chunks N and N+1, two lines are
/// considered the same sung phrase when
///   - `normalize_text(a.text) == normalize_text(b.text)`, AND
///   - `|a.start_ms - b.start_ms| <= AGREEMENT_MS` (1500 ms)
/// When a pair is found, KEEP the one whose start is FURTHER from the chunk
/// boundary (less boundary effect — the other chunk had more context).
///
/// Panics if `plans.len() != per_chunk.len()`.
pub fn merge_overlap(plans: &[ChunkPlan], per_chunk: &[Vec<ParsedLine>]) -> Vec<GlobalLine> {
    assert_eq!(
        plans.len(),
        per_chunk.len(),
        "plans and per_chunk must align"
    );

    // Step 1: shift each chunk's lines to global time
    let mut globals: Vec<Vec<GlobalLine>> = plans
        .iter()
        .zip(per_chunk.iter())
        .map(|(plan, lines)| {
            lines
                .iter()
                .map(|l| GlobalLine {
                    start_ms: l.start_ms + plan.start_ms,
                    end_ms: l.end_ms + plan.start_ms,
                    text: l.text.clone(),
                })
                .collect()
        })
        .collect();

    // Step 2: walk adjacent pairs, dedup in overlap regions
    const AGREEMENT_MS: i64 = 1_500;
    for i in 0..plans.len().saturating_sub(1) {
        let overlap_start = plans[i + 1].start_ms;
        let overlap_end = plans[i].end_ms;
        if overlap_end <= overlap_start {
            continue;
        }
        let a_indices: Vec<usize> = globals[i]
            .iter()
            .enumerate()
            .filter(|(_, l)| l.end_ms > overlap_start)
            .map(|(k, _)| k)
            .collect();
        let b_indices: Vec<usize> = globals[i + 1]
            .iter()
            .enumerate()
            .filter(|(_, l)| l.start_ms < overlap_end)
            .map(|(k, _)| k)
            .collect();

        let mut drop_a: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut drop_b: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for &ia in &a_indices {
            for &ib in &b_indices {
                if drop_a.contains(&ia) || drop_b.contains(&ib) {
                    continue;
                }
                let la = &globals[i][ia];
                let lb = &globals[i + 1][ib];
                if normalize_text(&la.text) != normalize_text(&lb.text) {
                    continue;
                }
                if (la.start_ms as i64 - lb.start_ms as i64).abs() > AGREEMENT_MS {
                    continue;
                }
                // Keep the one further from its own chunk's boundary with the other.
                // A's boundary: overlap_end. B's boundary: overlap_start.
                let a_dist = (la.start_ms as i64 - overlap_end as i64).abs();
                let b_dist = (lb.start_ms as i64 - overlap_start as i64).abs();
                if a_dist >= b_dist {
                    drop_b.insert(ib);
                } else {
                    drop_a.insert(ia);
                }
            }
        }
        globals[i] = globals[i]
            .iter()
            .enumerate()
            .filter(|(k, _)| !drop_a.contains(k))
            .map(|(_, l)| l.clone())
            .collect();
        globals[i + 1] = globals[i + 1]
            .iter()
            .enumerate()
            .filter(|(k, _)| !drop_b.contains(k))
            .map(|(_, l)| l.clone())
            .collect();
    }

    // Step 3: flatten + sort by start_ms
    let mut flat: Vec<GlobalLine> = globals.into_iter().flatten().collect();
    flat.sort_by_key(|l| l.start_ms);
    flat
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_chunks_single_chunk_for_short_song() {
        let p = plan_chunks(45_000);
        assert_eq!(p.len(), 1);
        assert_eq!(
            p[0],
            ChunkPlan {
                idx: 0,
                start_ms: 0,
                end_ms: 45_000
            }
        );
    }

    #[test]
    fn plan_chunks_exact_60s_is_single_chunk() {
        let p = plan_chunks(60_000);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].end_ms, 60_000);
    }

    #[test]
    fn plan_chunks_11min_song_yields_13_chunks() {
        let p = plan_chunks(659_980);
        assert_eq!(p.len(), 13);
        assert_eq!(p[0].start_ms, 0);
        assert_eq!(p[0].end_ms, 60_000);
        assert_eq!(p[1].start_ms, 50_000);
        assert_eq!(p[12].end_ms, 659_980);
        // Stride is 50s
        for i in 1..p.len() {
            assert_eq!(p[i].start_ms - p[i - 1].start_ms, 50_000);
        }
    }

    #[test]
    fn plan_chunks_zero_duration_is_empty() {
        assert!(plan_chunks(0).is_empty());
    }

    #[test]
    fn normalize_text_lowercase_strip_punct() {
        assert_eq!(normalize_text("I Want to Know You,"), "i want to know you");
        assert_eq!(normalize_text("I'm gonna love You"), "i'm gonna love you");
        assert_eq!(normalize_text("  Hello   World  "), "hello world");
    }

    #[test]
    fn merge_overlap_deduplicates_matching_lines_across_boundary() {
        let plans = vec![
            ChunkPlan {
                idx: 0,
                start_ms: 0,
                end_ms: 60_000,
            },
            ChunkPlan {
                idx: 1,
                start_ms: 50_000,
                end_ms: 110_000,
            },
        ];
        // Chunk 0 ends with "overlap line" at local 55s (global 55s).
        // Chunk 1 begins with same at local 5s (global 55s). → duplicate.
        let per_chunk = vec![
            vec![
                ParsedLine {
                    start_ms: 1_000,
                    end_ms: 3_000,
                    text: "first line".into(),
                },
                ParsedLine {
                    start_ms: 55_000,
                    end_ms: 58_000,
                    text: "overlap line".into(),
                },
            ],
            vec![
                ParsedLine {
                    start_ms: 5_000,
                    end_ms: 8_000,
                    text: "overlap line".into(),
                },
                ParsedLine {
                    start_ms: 20_000,
                    end_ms: 22_000,
                    text: "chunk1 tail".into(),
                },
            ],
        ];
        let merged = merge_overlap(&plans, &per_chunk);
        assert_eq!(
            merged.len(),
            3,
            "expected 3 lines after dedup, got: {:?}",
            merged
        );
        assert_eq!(merged[0].text, "first line");
        assert_eq!(merged[0].start_ms, 1_000);
        assert_eq!(merged[1].text, "overlap line");
        // The chunk-1 instance's boundary distance is 5s (from overlap_start=50_000
        // its global=55_000 is 5s away). The chunk-0 instance at global=55_000 is
        // 5s from overlap_end=60_000. Tie → rule says keep B (chunk 1's).
        assert_eq!(merged[2].text, "chunk1 tail");
        assert_eq!(merged[2].start_ms, 70_000); // 50_000 + 20_000
    }

    #[test]
    fn merge_overlap_keeps_both_when_text_differs_in_overlap() {
        let plans = vec![
            ChunkPlan {
                idx: 0,
                start_ms: 0,
                end_ms: 60_000,
            },
            ChunkPlan {
                idx: 1,
                start_ms: 50_000,
                end_ms: 110_000,
            },
        ];
        let per_chunk = vec![
            vec![ParsedLine {
                start_ms: 55_000,
                end_ms: 58_000,
                text: "from chunk 0".into(),
            }],
            vec![ParsedLine {
                start_ms: 6_000,
                end_ms: 8_000,
                text: "from chunk 1".into(),
            }],
        ];
        let merged = merge_overlap(&plans, &per_chunk);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_overlap_keeps_both_when_text_same_but_start_gap_large() {
        let plans = vec![
            ChunkPlan {
                idx: 0,
                start_ms: 0,
                end_ms: 60_000,
            },
            ChunkPlan {
                idx: 1,
                start_ms: 50_000,
                end_ms: 110_000,
            },
        ];
        // Same text but global starts 4s apart → NOT duplicates (> 1500ms threshold)
        let per_chunk = vec![
            vec![ParsedLine {
                start_ms: 51_000,
                end_ms: 53_000,
                text: "oh jesus".into(),
            }],
            vec![ParsedLine {
                start_ms: 5_000,
                end_ms: 7_000,
                text: "oh jesus".into(),
            }], // global 55s
        ];
        let merged = merge_overlap(&plans, &per_chunk);
        assert_eq!(merged.len(), 2, "4s apart > 1500ms threshold — keep both");
    }

    #[test]
    fn merge_overlap_output_sorted_by_start_ms() {
        let plans = vec![
            ChunkPlan {
                idx: 0,
                start_ms: 0,
                end_ms: 60_000,
            },
            ChunkPlan {
                idx: 1,
                start_ms: 50_000,
                end_ms: 110_000,
            },
        ];
        let per_chunk = vec![
            vec![
                ParsedLine {
                    start_ms: 30_000,
                    end_ms: 35_000,
                    text: "chunk0 middle".into(),
                },
                ParsedLine {
                    start_ms: 5_000,
                    end_ms: 9_000,
                    text: "chunk0 early".into(),
                },
            ],
            vec![
                ParsedLine {
                    start_ms: 40_000,
                    end_ms: 42_000,
                    text: "chunk1 far".into(),
                }, // global 90s
            ],
        ];
        let merged = merge_overlap(&plans, &per_chunk);
        let starts: Vec<u64> = merged.iter().map(|l| l.start_ms).collect();
        let mut sorted = starts.clone();
        sorted.sort();
        assert_eq!(starts, sorted, "merged list must be sorted by start_ms");
    }
}
