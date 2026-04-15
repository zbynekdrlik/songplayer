# YouTube Manual Subtitles + Chunked Qwen3 Alignment (Phase 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the broken whole-song Qwen3 alignment path with a chunked alignment path driven by author-verified YouTube manual subtitles, so song #148 "Get This Party Started" ships real word-level karaoke after deploy.

**Architecture:** Rust-heavy orchestration. Python shrinks to three narrow subprocess entry points (`cmd_preprocess_vocals`, `cmd_align_chunks`, `cmd_preload`). Three new pure-Rust modules — `chunking`, `assembly`, `quality` — carry all data shaping and quality metrics. DB migration V9 resets all lyrics rows. E2E hard-asserts six gates on #148 post-deploy.

**Tech Stack:** Rust (sqlx, tokio, anyhow, serde, tracing), Python (qwen-asr, audio-separator[gpu], librosa, soundfile, torch 2.6.0+cu124), Playwright for E2E, cargo-mutants.

**Spec:** `docs/superpowers/specs/2026-04-15-yt-subs-chunked-alignment-design.md`

---

## File Structure

**Created:**
- `crates/sp-server/src/lyrics/chunking.rs` — `plan_chunks(track) → Vec<ChunkRequest>`, pure function
- `crates/sp-server/src/lyrics/assembly.rs` — `assemble(track, results) → LyricsTrack`, pure function
- `crates/sp-server/src/lyrics/quality.rs` — `duplicate_start_pct`, `gap_stddev_ms`

**Modified:**
- `VERSION` — bump to `0.16.0-dev.1`
- `crates/sp-server/src/lyrics/youtube_subs.rs` — drop `--write-auto-subs`
- `crates/sp-server/src/lyrics/aligner.rs` — rewrite to two thin wrappers; delete old post-proc
- `crates/sp-server/src/lyrics/worker.rs` — rewrite `process_song` + `acquire_lyrics`; delete `retry_missing_alignment`
- `crates/sp-server/src/lyrics/mod.rs` — register new modules
- `crates/sp-server/src/lyrics/bootstrap.rs` — pin matched torch triplet; add anvuew dereverb preload
- `crates/sp-server/src/db/mod.rs` — add `MIGRATION_V9`
- `crates/sp-server/src/db/models.rs` — delete `set_video_lyrics_source`, `get_next_video_missing_alignment`
- `scripts/lyrics_worker.py` — delete `cmd_align`, `cmd_transcribe`, `cmd_download_models`, `_group_words_into_lines`; add `cmd_preprocess_vocals`, `cmd_align_chunks`; rework `cmd_preload`
- `e2e/post-deploy-flac.spec.ts` — add `#148` hard-asserted test
- `.github/workflows/ci.yml` — add legacy-code deletion audit step

---

## Task 0: Bump VERSION to 0.16.0-dev.1

**Files:**
- Modify: `VERSION`

- [ ] **Step 1: Read current VERSION**

Run: `cat VERSION`
Expected: `0.15.0-dev.1`

- [ ] **Step 2: Bump to 0.16.0-dev.1**

Write `VERSION`:
```
0.16.0-dev.1
```

- [ ] **Step 3: Sync propagation**

Run: `./scripts/sync-version.sh`
Expected: exits 0; all `Cargo.toml` files and `src-tauri/tauri.conf.json` reflect `0.16.0-dev.1`.

- [ ] **Step 4: Commit**

```bash
git add VERSION Cargo.toml sp-ui/Cargo.toml src-tauri/Cargo.toml src-tauri/tauri.conf.json
git commit -m "chore: bump VERSION to 0.16.0-dev.1"
```

---

## Task 1: Drop `--write-auto-subs` from yt-dlp invocation

**Files:**
- Modify: `crates/sp-server/src/lyrics/youtube_subs.rs:52-64`

- [ ] **Step 1: Add a failing unit test asserting `--write-auto-subs` is NOT in the argv**

Append to the `mod tests` block in `crates/sp-server/src/lyrics/youtube_subs.rs` (just above the final `}`):

```rust
    /// The yt-dlp invocation must NOT pass `--write-auto-subs`. Auto-generated
    /// captions are unusable (overlapping words, [music] markers, duplicated
    /// timestamps). Only author-uploaded manual subs are acceptable.
    #[test]
    fn fetch_subtitles_source_does_not_contain_auto_subs_flag() {
        let src = include_str!("youtube_subs.rs");
        assert!(
            !src.contains("--write-auto-subs"),
            "youtube_subs.rs must not pass --write-auto-subs to yt-dlp"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p sp-server youtube_subs::tests::fetch_subtitles_source_does_not_contain_auto_subs_flag`
Expected: FAIL — the source still contains the string.

- [ ] **Step 3: Remove the `--write-auto-subs` argument**

In `crates/sp-server/src/lyrics/youtube_subs.rs`, change the `cmd.args([...])` call starting at line 53 from:

```rust
    cmd.args([
        "--write-subs",
        "--write-auto-subs",
        "--sub-format",
        "json3",
        "--sub-lang",
        "en",
        "--skip-download",
        "-o",
        &output_template,
        &url,
    ]);
```

to:

```rust
    cmd.args([
        "--write-subs",
        "--sub-format",
        "json3",
        "--sub-lang",
        "en",
        "--skip-download",
        "-o",
        &output_template,
        &url,
    ]);
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p sp-server youtube_subs::tests::fetch_subtitles_source_does_not_contain_auto_subs_flag`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/lyrics/youtube_subs.rs
git commit -m "feat(lyrics): drop --write-auto-subs; manual subs only"
```

---

## Task 2: New `chunking.rs` module — pure function `plan_chunks`

**Files:**
- Create: `crates/sp-server/src/lyrics/chunking.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs`

- [ ] **Step 1: Declare the module**

In `crates/sp-server/src/lyrics/mod.rs`, after `pub mod bootstrap;` (line 2), insert:

```rust
pub mod chunking;
```

- [ ] **Step 2: Write the failing tests**

Create `crates/sp-server/src/lyrics/chunking.rs`:

```rust
//! Pure function that plans chunked alignment requests from a `LyricsTrack`.
//!
//! Each line in the input track becomes one `ChunkRequest`. The chunk's
//! audio window is the line's `[start_ms, end_ms]` padded by ±500 ms
//! (clamped to `>= 0`). Word counts are computed from `line.en` by
//! whitespace split so the assembly phase can redistribute aligned words
//! back to their source line.

use sp_core::lyrics::LyricsTrack;

/// Audio-window pre/post padding applied around each line, in milliseconds.
/// 500 ms was validated empirically on #148 Planetshakers "Get This Party
/// Started" — smaller windows trunc'd leading phonemes, larger windows let
/// neighbour-line bleed into the alignment.
pub const CHUNK_PAD_MS: u64 = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRequest {
    /// Index into the original `LyricsTrack.lines` — assembly uses this
    /// to place aligned words back on their source line.
    pub line_index: usize,
    /// Audio slice start, in ms. Never negative (clamped at 0).
    pub start_ms: u64,
    /// Audio slice end, in ms.
    pub end_ms: u64,
    /// Lyrics text to align against the slice (one line).
    pub text: String,
    /// Expected word count. The aligner may return fewer or more; the
    /// assembly phase handles both cases.
    pub word_count: usize,
}

/// Build a `ChunkRequest` per non-empty line of `track`.
///
/// Empty lines (`.en` trimmed is empty) are skipped. The start/end of
/// each chunk is padded by `CHUNK_PAD_MS` on both sides, clamped to zero
/// on the low end so the first line doesn't produce a negative slice.
pub fn plan_chunks(track: &LyricsTrack) -> Vec<ChunkRequest> {
    let mut out = Vec::with_capacity(track.lines.len());
    for (idx, line) in track.lines.iter().enumerate() {
        let trimmed = line.en.trim();
        if trimmed.is_empty() {
            continue;
        }
        let word_count = trimmed.split_whitespace().count();
        if word_count == 0 {
            continue;
        }
        let start_ms = line.start_ms.saturating_sub(CHUNK_PAD_MS);
        let end_ms = line.end_ms.saturating_add(CHUNK_PAD_MS);
        out.push(ChunkRequest {
            line_index: idx,
            start_ms,
            end_ms,
            text: trimmed.to_string(),
            word_count,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_core::lyrics::{LyricsLine, LyricsTrack};

    fn line(start_ms: u64, end_ms: u64, en: &str) -> LyricsLine {
        LyricsLine {
            start_ms,
            end_ms,
            en: en.to_string(),
            sk: None,
            words: None,
        }
    }

    fn track(lines: Vec<LyricsLine>) -> LyricsTrack {
        LyricsTrack {
            version: 1,
            source: "yt_subs".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines,
        }
    }

    #[test]
    fn plan_chunks_builds_one_request_per_non_empty_line() {
        let t = track(vec![
            line(1000, 3000, "hey there friend"),
            line(4000, 6000, "goodbye"),
        ]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 2);

        assert_eq!(chunks[0].line_index, 0);
        assert_eq!(chunks[0].start_ms, 500); // 1000 - 500 pad
        assert_eq!(chunks[0].end_ms, 3500); // 3000 + 500 pad
        assert_eq!(chunks[0].text, "hey there friend");
        assert_eq!(chunks[0].word_count, 3);

        assert_eq!(chunks[1].line_index, 1);
        assert_eq!(chunks[1].start_ms, 3500);
        assert_eq!(chunks[1].end_ms, 6500);
        assert_eq!(chunks[1].word_count, 1);
    }

    #[test]
    fn plan_chunks_clamps_first_line_start_to_zero() {
        let t = track(vec![line(200, 1000, "hello")]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].start_ms, 0,
            "200ms - 500ms pad must clamp to 0 not wrap around"
        );
        assert_eq!(chunks[0].end_ms, 1500);
    }

    #[test]
    fn plan_chunks_skips_empty_and_whitespace_only_lines() {
        let t = track(vec![
            line(0, 1000, ""),
            line(1000, 2000, "   "),
            line(2000, 3000, "real"),
            line(3000, 4000, "\t\n"),
        ]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].line_index, 2);
        assert_eq!(chunks[0].text, "real");
    }

    #[test]
    fn plan_chunks_preserves_line_indices_across_skips() {
        // Line index must still point at the original slot in track.lines
        // — assembly relies on this to slot words back.
        let t = track(vec![
            line(0, 1000, ""),
            line(1000, 2000, "one two"),
            line(2000, 3000, "   "),
            line(3000, 4000, "three"),
        ]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].line_index, 1);
        assert_eq!(chunks[1].line_index, 3);
    }

    #[test]
    fn plan_chunks_splits_text_on_any_whitespace_for_word_count() {
        let t = track(vec![line(0, 1000, "hey  there\tfriend\nhello")]);
        let chunks = plan_chunks(&t);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].word_count, 4);
    }

    #[test]
    fn plan_chunks_empty_track_returns_empty_vec() {
        let t = track(vec![]);
        assert_eq!(plan_chunks(&t).len(), 0);
    }
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p sp-server chunking`
Expected: 6 tests pass (module compiles, all tests green).

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/chunking.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): add chunking module for chunked alignment planning"
```

---

## Task 3: New `assembly.rs` module — pure function `assemble`

**Files:**
- Create: `crates/sp-server/src/lyrics/assembly.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs`

- [ ] **Step 1: Declare the module**

In `crates/sp-server/src/lyrics/mod.rs`, after `pub mod chunking;`, insert:

```rust
pub mod assembly;
```

- [ ] **Step 2: Write the failing tests and implementation**

Create `crates/sp-server/src/lyrics/assembly.rs`:

```rust
//! Pure function that assembles per-chunk aligned word streams back into
//! a `LyricsTrack` with `.words` populated on each line.
//!
//! Input:
//!   - `original`: the line-level `LyricsTrack` the chunks were planned from
//!   - `results`: one `ChunkResult` per `ChunkRequest` produced by Python
//!
//! Output:
//!   - A new `LyricsTrack` where each line whose chunk returned words now
//!     has `.words` populated; lines without a chunk (empty lines that
//!     chunking skipped) keep `.words = None`.
//!
//! Under-aligned chunks (aligner returned fewer words than expected) leave
//! the remaining words as a synthesised placeholder: text from
//! `LyricsLine.en` split by whitespace, with `start_ms == end_ms == 0` so
//! the renderer can detect and skip them. Over-aligned chunks drop the
//! surplus words.

use sp_core::lyrics::{LyricsLine, LyricsTrack, LyricsWord};

#[derive(Debug, Clone)]
pub struct AlignedWord {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ChunkResult {
    pub line_index: usize,
    pub words: Vec<AlignedWord>,
}

/// Merge per-chunk alignment output back into a full `LyricsTrack`.
///
/// - Lines referenced by a `ChunkResult` get their `.words` populated.
/// - Aligned words beyond the expected count (from `LyricsLine.en` split)
///   are dropped.
/// - Missing aligned words (fewer than expected) are padded with
///   `LyricsWord { start_ms: 0, end_ms: 0, text: "<expected>" }` so the
///   renderer can detect and skip placeholder entries.
pub fn assemble(mut original: LyricsTrack, results: Vec<ChunkResult>) -> LyricsTrack {
    for result in results {
        if result.line_index >= original.lines.len() {
            continue;
        }
        let expected_words: Vec<String> = original.lines[result.line_index]
            .en
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();
        if expected_words.is_empty() {
            continue;
        }

        let mut out = Vec::with_capacity(expected_words.len());
        for (i, expected) in expected_words.iter().enumerate() {
            if let Some(got) = result.words.get(i) {
                out.push(LyricsWord {
                    text: got.text.clone(),
                    start_ms: got.start_ms,
                    end_ms: got.end_ms,
                });
            } else {
                // Aligner under-delivered — synthesize a placeholder with
                // zero timing so renderer skips it.
                out.push(LyricsWord {
                    text: expected.clone(),
                    start_ms: 0,
                    end_ms: 0,
                });
            }
        }
        original.lines[result.line_index].words = Some(out);
    }
    original
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(start_ms: u64, end_ms: u64, en: &str) -> LyricsLine {
        LyricsLine {
            start_ms,
            end_ms,
            en: en.to_string(),
            sk: None,
            words: None,
        }
    }

    fn track(lines: Vec<LyricsLine>) -> LyricsTrack {
        LyricsTrack {
            version: 1,
            source: "yt_subs".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines,
        }
    }

    fn aw(start_ms: u64, end_ms: u64, text: &str) -> AlignedWord {
        AlignedWord {
            text: text.to_string(),
            start_ms,
            end_ms,
        }
    }

    #[test]
    fn assemble_exact_word_count_places_every_word() {
        let orig = track(vec![line(1000, 3000, "hey there friend")]);
        let results = vec![ChunkResult {
            line_index: 0,
            words: vec![
                aw(1000, 1200, "hey"),
                aw(1200, 1400, "there"),
                aw(1400, 1800, "friend"),
            ],
        }];
        let out = assemble(orig, results);
        let words = out.lines[0].words.as_ref().expect("words populated");
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].text, "hey");
        assert_eq!(words[0].start_ms, 1000);
        assert_eq!(words[2].text, "friend");
        assert_eq!(words[2].start_ms, 1400);
    }

    #[test]
    fn assemble_under_aligned_pads_with_zero_timing_placeholders() {
        let orig = track(vec![line(0, 2000, "one two three four")]);
        let results = vec![ChunkResult {
            line_index: 0,
            words: vec![aw(100, 200, "one"), aw(200, 300, "two")],
        }];
        let out = assemble(orig, results);
        let words = out.lines[0].words.as_ref().expect("words populated");
        assert_eq!(words.len(), 4);
        assert_eq!(words[0].start_ms, 100);
        assert_eq!(words[1].start_ms, 200);
        assert_eq!(words[2].start_ms, 0, "missing words get 0 start");
        assert_eq!(words[2].end_ms, 0);
        assert_eq!(words[2].text, "three");
        assert_eq!(words[3].text, "four");
    }

    #[test]
    fn assemble_over_aligned_drops_surplus() {
        let orig = track(vec![line(0, 2000, "one two")]);
        let results = vec![ChunkResult {
            line_index: 0,
            words: vec![
                aw(100, 200, "one"),
                aw(200, 300, "two"),
                aw(300, 400, "extra"),
                aw(400, 500, "words"),
            ],
        }];
        let out = assemble(orig, results);
        let words = out.lines[0].words.as_ref().expect("words populated");
        assert_eq!(words.len(), 2);
        assert_eq!(words[1].text, "two");
    }

    #[test]
    fn assemble_leaves_lines_without_results_untouched() {
        let orig = track(vec![
            line(0, 1000, "first line"),
            line(1000, 2000, "untouched line"),
        ]);
        let results = vec![ChunkResult {
            line_index: 0,
            words: vec![aw(0, 500, "first"), aw(500, 1000, "line")],
        }];
        let out = assemble(orig, results);
        assert!(out.lines[0].words.is_some());
        assert!(out.lines[1].words.is_none());
    }

    #[test]
    fn assemble_ignores_out_of_bounds_line_index() {
        let orig = track(vec![line(0, 1000, "only line")]);
        let results = vec![ChunkResult {
            line_index: 99,
            words: vec![aw(0, 500, "garbage")],
        }];
        let out = assemble(orig, results);
        assert!(out.lines[0].words.is_none());
    }

    #[test]
    fn assemble_empty_line_en_skipped() {
        let orig = track(vec![line(0, 1000, "")]);
        let results = vec![ChunkResult {
            line_index: 0,
            words: vec![aw(0, 500, "x")],
        }];
        let out = assemble(orig, results);
        assert!(out.lines[0].words.is_none());
    }
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p sp-server assembly`
Expected: 6 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/assembly.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): add assembly module for chunked alignment output merge"
```

---

## Task 4: New `quality.rs` module — `duplicate_start_pct` and `gap_stddev_ms`

**Files:**
- Create: `crates/sp-server/src/lyrics/quality.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs`

- [ ] **Step 1: Declare the module**

In `crates/sp-server/src/lyrics/mod.rs`, after `pub mod assembly;`, insert:

```rust
pub mod quality;
```

- [ ] **Step 2: Write the failing tests and implementation**

Create `crates/sp-server/src/lyrics/quality.rs`:

```rust
//! Pure functions that compute quality metrics on aligned lyric lines.
//!
//! Used by `worker.rs` for `warn!` logs when a line comes back degenerate
//! (e.g. 100% duplicate word starts) and by the E2E post-deploy test to
//! hard-assert #148 alignment quality.

use sp_core::lyrics::LyricsLine;

/// Percentage of words whose `start_ms` equals their in-line predecessor's
/// `start_ms`. Range: 0.0–100.0. 0.0 = every word has a unique start.
/// Returns 0.0 for lines with < 2 words.
pub fn duplicate_start_pct(line: &LyricsLine) -> f64 {
    let Some(words) = line.words.as_ref() else {
        return 0.0;
    };
    if words.len() < 2 {
        return 0.0;
    }
    let mut duplicates = 0usize;
    for pair in words.windows(2) {
        if pair[1].start_ms == pair[0].start_ms {
            duplicates += 1;
        }
    }
    let denom = (words.len() - 1) as f64;
    100.0 * (duplicates as f64) / denom
}

/// Sample standard deviation of inter-word gap durations (ms).
///
/// A line whose aligner produced perfectly even spacing (band-aid /
/// synthesized timings) collapses to stddev ≈ 0. Real singing produces
/// irregular phonetic gaps with stddev ≥ 50 ms on typical worship vocals.
/// Returns 0.0 for lines with < 3 words (need at least 2 gaps).
pub fn gap_stddev_ms(line: &LyricsLine) -> f64 {
    let Some(words) = line.words.as_ref() else {
        return 0.0;
    };
    if words.len() < 3 {
        return 0.0;
    }
    let mut gaps: Vec<f64> = Vec::with_capacity(words.len() - 1);
    for pair in words.windows(2) {
        gaps.push((pair[1].start_ms as f64) - (pair[0].start_ms as f64));
    }
    let mean = gaps.iter().sum::<f64>() / (gaps.len() as f64);
    let variance: f64 = gaps.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / (gaps.len() as f64);
    variance.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_core::lyrics::{LyricsLine, LyricsWord};

    fn line_with_words(words: &[(u64, u64, &str)]) -> LyricsLine {
        LyricsLine {
            start_ms: words.first().map(|w| w.0).unwrap_or(0),
            end_ms: words.last().map(|w| w.1).unwrap_or(0),
            en: String::new(),
            sk: None,
            words: Some(
                words
                    .iter()
                    .map(|(s, e, t)| LyricsWord {
                        start_ms: *s,
                        end_ms: *e,
                        text: (*t).into(),
                    })
                    .collect(),
            ),
        }
    }

    fn line_no_words() -> LyricsLine {
        LyricsLine {
            start_ms: 0,
            end_ms: 1000,
            en: "nope".into(),
            sk: None,
            words: None,
        }
    }

    // ---------- duplicate_start_pct ----------

    #[test]
    fn duplicate_start_pct_zero_for_progressive_words() {
        let l = line_with_words(&[(0, 100, "a"), (200, 300, "b"), (400, 500, "c")]);
        assert!((duplicate_start_pct(&l) - 0.0).abs() < 0.001);
    }

    #[test]
    fn duplicate_start_pct_fully_collapsed_is_100_pct() {
        let l = line_with_words(&[(100, 200, "a"), (100, 200, "b"), (100, 200, "c")]);
        assert!((duplicate_start_pct(&l) - 100.0).abs() < 0.001);
    }

    #[test]
    fn duplicate_start_pct_half_collapsed_is_50_pct() {
        // 4 words, 3 pairs. pair (1,2) shares start_ms; (0,1) and (2,3) do not.
        // => 1/3 ≈ 33.33 %, not 50 %.
        let l = line_with_words(&[
            (0, 100, "a"),
            (200, 300, "b"),
            (200, 300, "c"),
            (500, 600, "d"),
        ]);
        let pct = duplicate_start_pct(&l);
        assert!(
            (pct - (100.0 / 3.0)).abs() < 0.01,
            "expected ~33.33 %, got {pct}"
        );
    }

    #[test]
    fn duplicate_start_pct_no_words_returns_zero() {
        assert_eq!(duplicate_start_pct(&line_no_words()), 0.0);
    }

    #[test]
    fn duplicate_start_pct_single_word_returns_zero() {
        let l = line_with_words(&[(0, 100, "one")]);
        assert_eq!(duplicate_start_pct(&l), 0.0);
    }

    // ---------- gap_stddev_ms ----------

    #[test]
    fn gap_stddev_ms_zero_for_perfectly_even_gaps() {
        let l = line_with_words(&[(0, 50, "a"), (100, 150, "b"), (200, 250, "c"), (300, 350, "d")]);
        // All gaps == 100 ms → stddev 0.
        assert!(gap_stddev_ms(&l).abs() < 0.001);
    }

    #[test]
    fn gap_stddev_ms_positive_for_irregular_gaps() {
        // gaps: 100, 300, 200. mean 200. variance = (10000 + 10000 + 0) / 3.
        // stddev ~= 81.65 ms
        let l = line_with_words(&[
            (0, 50, "a"),
            (100, 150, "b"),
            (400, 450, "c"),
            (600, 650, "d"),
        ]);
        let s = gap_stddev_ms(&l);
        assert!((s - 81.65).abs() < 1.0, "expected ~81.65 ms, got {s}");
    }

    #[test]
    fn gap_stddev_ms_fewer_than_three_words_returns_zero() {
        let l = line_with_words(&[(0, 100, "a"), (200, 300, "b")]);
        assert_eq!(gap_stddev_ms(&l), 0.0);
    }

    #[test]
    fn gap_stddev_ms_no_words_returns_zero() {
        assert_eq!(gap_stddev_ms(&line_no_words()), 0.0);
    }
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p sp-server quality`
Expected: 9 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/quality.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): add quality metrics module (duplicate_start_pct, gap_stddev_ms)"
```

---

## Task 5: DB migration V9 — reset all lyrics rows

**Files:**
- Modify: `crates/sp-server/src/db/mod.rs:11-20, 126-128`

- [ ] **Step 1: Write failing test for V9 behaviour**

Append to `crates/sp-server/src/db/mod.rs` inside `mod tests { … }` (line ~200+):

```rust
    #[tokio::test]
    async fn migration_v9_resets_has_lyrics_and_lyrics_source_for_all_rows() {
        // Seed a DB at V8 with various lyrics_source values, then re-run
        // migrations to V9 and confirm all rows are back at (0, NULL).
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES ('p', 'u', 'n')",
        )
        .execute(&pool)
        .await
        .unwrap();

        for (yt, src, has) in [
            ("a1", Some("lrclib"), 1),
            ("a2", Some("yt_subs+qwen3"), 1),
            ("a3", Some("lrclib+qwen3"), 1), // retired value
            ("a4", None::<&str>, 0),
        ] {
            sqlx::query(
                "INSERT INTO videos (playlist_id, youtube_id, title, has_lyrics, lyrics_source) \
                 VALUES (1, ?, 't', ?, ?)",
            )
            .bind(yt)
            .bind(has)
            .bind(src)
            .execute(&pool)
            .await
            .unwrap();
        }

        // Rewind schema_version to force V9 to re-run.
        sqlx::query("DELETE FROM schema_version WHERE version = 9")
            .execute(&pool)
            .await
            .unwrap();
        run_migrations(&pool).await.unwrap();

        let rows = sqlx::query("SELECT has_lyrics, lyrics_source FROM videos ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 4);
        for row in rows {
            let hl: i64 = row.get("has_lyrics");
            let src: Option<String> = row.get("lyrics_source");
            assert_eq!(hl, 0, "has_lyrics must be 0 after V9");
            assert_eq!(src, None, "lyrics_source must be NULL after V9");
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p sp-server db::tests::migration_v9_resets_has_lyrics_and_lyrics_source_for_all_rows`
Expected: FAIL — V9 does not exist yet (may error on schema_version delete returning 0 rows, or assertion fails because rows still have the old `lyrics_source` values).

- [ ] **Step 3: Add MIGRATION_V9**

In `crates/sp-server/src/db/mod.rs`, change the `MIGRATIONS` slice (line 11) to add `(9, MIGRATION_V9)`:

```rust
const MIGRATIONS: &[(i32, &str)] = &[
    (1, MIGRATION_V1),
    (2, MIGRATION_V2),
    (3, MIGRATION_V3),
    (4, MIGRATION_V4),
    (5, MIGRATION_V5),
    (6, MIGRATION_V6),
    (7, MIGRATION_V7),
    (8, MIGRATION_V8),
    (9, MIGRATION_V9),
];
```

After `const MIGRATION_V8: &str = "…";` (line 126-128), add:

```rust
// V9 = reset all lyrics rows to re-process them through the new
// YT-subs-first pipeline. Retires 'lrclib+qwen3' (whole-song alignment)
// in favour of 'yt_subs+qwen3' (chunked) or plain 'lrclib' (line-level
// fallback). Idempotent: a row already at (0, NULL) is a no-op.
const MIGRATION_V9: &str = "
UPDATE videos SET has_lyrics = 0, lyrics_source = NULL;
";
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p sp-server db::tests::migration_v9_resets_has_lyrics_and_lyrics_source_for_all_rows`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/db/mod.rs
git commit -m "feat(db): add migration V9 resetting all lyrics rows for YT-subs pipeline"
```

---

## Task 6: Python rewrite — `cmd_align_chunks`, delete legacy commands

**Files:**
- Modify: `scripts/lyrics_worker.py` (full rewrite — ~180 LOC target)

- [ ] **Step 1: Overwrite `scripts/lyrics_worker.py` with the new contents**

Write `scripts/lyrics_worker.py`:

```python
#!/usr/bin/env python3
"""
lyrics_worker.py — narrow Python entry points for the lyrics pipeline.

Commands:
  preprocess-vocals  Mel-Roformer + anvuew dereverb + 16 kHz mono float32 WAV
  align-chunks       Chunked Qwen3-ForcedAligner alignment (loads model once,
                     loops over all chunks from a JSON request file)
  preload            Warm Mel-Roformer + anvuew + Qwen3-ForcedAligner at boot
  isolate-vocals     Diagnostic: Mel-Roformer only, 16 kHz mono float32 WAV
"""

import argparse
import gc
import json
import os
import shutil
import sys
import tempfile


MEL_ROFORMER_MODEL = "model_bs_roformer_ep_317_sdr_12.9755.ckpt"
DEREVERB_MODEL = "dereverb_mel_band_roformer_anvuew_sdr_19.1729.ckpt"


def _pick_vocal_stem(out_files, fallback_dir):
    """Return the absolute path of the Vocals stem among `out_files`."""
    def _abs(p):
        return p if os.path.isabs(p) else os.path.join(fallback_dir, p)

    vocal = [p for p in out_files if "Vocals" in p or "vocals" in p]
    if vocal:
        return _abs(vocal[0])
    non_inst = [
        p for p in out_files if "Instrumental" not in p and "instrumental" not in p
    ]
    if len(non_inst) == 1:
        return _abs(non_inst[0])
    raise RuntimeError(
        f"audio-separator did not produce an identifiable Vocals stem (got: {out_files})"
    )


def _pick_dereverbed_stem(out_files, fallback_dir):
    """Return the absolute path of the anvuew *(noreverb)* stem.

    Match on the parenthesized token *(noreverb)* in the filename — the
    substring 'dry' false-matched real filenames on earlier runs. If no
    explicit noreverb tag is present, fall back to the single file that
    does not contain '(reverb)'.
    """
    def _abs(p):
        return p if os.path.isabs(p) else os.path.join(fallback_dir, p)

    noreverb = [p for p in out_files if "(noreverb)" in p.lower()]
    if noreverb:
        return _abs(noreverb[0])
    non_reverb = [p for p in out_files if "(reverb)" not in p.lower()]
    if len(non_reverb) == 1:
        return _abs(non_reverb[0])
    raise RuntimeError(
        f"anvuew dereverb did not produce an identifiable (noreverb) stem (got: {out_files})"
    )


def _free_vram(sep):
    """Drop separator state so the next model can load without OOM."""
    import torch
    if hasattr(sep, "model_instance"):
        sep.model_instance = None
    del sep
    gc.collect()
    if torch.cuda.is_available():
        torch.cuda.empty_cache()


def cmd_preprocess_vocals(args):
    """Mel-Roformer isolate → anvuew dereverb → 16 kHz mono float32 WAV.

    Writes a FLOAT WAV to --output. Exits 0 on success.
    """
    import numpy as np
    import librosa
    import soundfile as sf
    from audio_separator.separator import Separator

    stem_dir = tempfile.mkdtemp(prefix="sp_stems_")
    try:
        # Step 1: Mel-Roformer vocal isolation.
        sep = Separator(
            model_file_dir=args.models_dir,
            output_format="WAV",
            output_dir=stem_dir,
        )
        sep.load_model(MEL_ROFORMER_MODEL)
        out_files = sep.separate(args.audio)
        vocal_path = _pick_vocal_stem(out_files, stem_dir)
        _free_vram(sep)

        # Step 2: anvuew mel-band roformer dereverb on the isolated vocal.
        sep2 = Separator(
            model_file_dir=args.models_dir,
            output_format="WAV",
            output_dir=stem_dir,
        )
        sep2.load_model(DEREVERB_MODEL)
        out_files2 = sep2.separate(vocal_path)
        dry_path = _pick_dereverbed_stem(out_files2, stem_dir)
        _free_vram(sep2)

        # Step 3: resample to exactly 16 kHz mono float32, peak-clamp.
        audio, _ = librosa.load(dry_path, sr=16000, mono=True)
        peak = float(np.max(np.abs(audio))) if audio.size else 0.0
        if peak > 1.0:
            audio = audio / peak
        sf.write(args.output, audio, 16000, subtype="FLOAT")
    finally:
        shutil.rmtree(stem_dir, ignore_errors=True)

    print(json.dumps({"output": args.output}))


def cmd_align_chunks(args):
    """Chunked Qwen3-ForcedAligner: loads the model ONCE, loops over all chunks.

    --chunks is a path to JSON with shape:
      {"chunks": [{"chunk_idx": 0, "start_ms": 500, "end_ms": 3500,
                   "text": "hey there friend", "word_count": 3}, ...]}

    Writes JSON to --output with shape:
      {"chunks": [{"chunk_idx": 0, "words": [
          {"text": "hey", "start_ms": 1000, "end_ms": 1200}, ...
      ]}, ...]}

    Word timestamps are absolute (start_ms of chunk + aligner offset).
    """
    import numpy as np
    import soundfile as sf
    import torch
    from qwen_asr import Qwen3ForcedAligner

    with open(args.chunks, "r", encoding="utf-8") as f:
        request = json.load(f)
    chunks_in = request["chunks"]

    audio, sr = sf.read(args.audio, dtype="float32")
    if sr != 16000:
        raise RuntimeError(f"expected 16 kHz audio, got {sr}")
    if audio.ndim != 1:
        audio = np.mean(audio, axis=1).astype("float32")

    device_map = "cuda:0" if torch.cuda.is_available() else "cpu"
    model = Qwen3ForcedAligner.from_pretrained(
        "Qwen/Qwen3-ForcedAligner-0.6B",
        dtype=torch.bfloat16,
        device_map=device_map,
    )

    results = []
    total_samples = audio.shape[0]
    for c in chunks_in:
        start_s = int(round(c["start_ms"] * 16000 / 1000))
        end_s = int(round(c["end_ms"] * 16000 / 1000))
        start_s = max(0, start_s)
        end_s = min(total_samples, end_s)
        if end_s <= start_s:
            results.append({"chunk_idx": c["chunk_idx"], "words": []})
            continue
        slice_ = audio[start_s:end_s]
        # Qwen3 writes audio from an ndarray on disk; feed via a tmpfile.
        fd, wav_path = tempfile.mkstemp(suffix="_chunk.wav")
        os.close(fd)
        try:
            sf.write(wav_path, slice_, 16000, subtype="FLOAT")
            aligned = model.align(
                audio=wav_path,
                text=c["text"],
                language="English",
            )
            word_stream = aligned[0]
            offset_ms = c["start_ms"]
            words_out = [
                {
                    "text": w.text,
                    "start_ms": int(round(w.start_time * 1000)) + offset_ms,
                    "end_ms": int(round(w.end_time * 1000)) + offset_ms,
                }
                for w in word_stream
            ]
        finally:
            try:
                os.remove(wav_path)
            except OSError:
                pass
        results.append({"chunk_idx": c["chunk_idx"], "words": words_out})

    with open(args.output, "w", encoding="utf-8") as f:
        json.dump({"chunks": results}, f, ensure_ascii=False)


def cmd_preload(args):
    """Warm Mel-Roformer + anvuew dereverb + Qwen3-ForcedAligner at bootstrap.

    Surfaces model-download failures before any real song is processed.
    """
    import torch
    from audio_separator.separator import Separator
    from qwen_asr import Qwen3ForcedAligner

    mel = Separator(model_file_dir=args.models_dir, output_format="WAV")
    mel.load_model(MEL_ROFORMER_MODEL)
    _free_vram(mel)

    dereverb = Separator(model_file_dir=args.models_dir, output_format="WAV")
    dereverb.load_model(DEREVERB_MODEL)
    _free_vram(dereverb)

    device_map = "cuda:0" if torch.cuda.is_available() else "cpu"
    model = Qwen3ForcedAligner.from_pretrained(
        "Qwen/Qwen3-ForcedAligner-0.6B",
        dtype=torch.bfloat16,
        device_map=device_map,
    )
    _ = next(model.parameters())
    print(
        json.dumps(
            {
                "loaded": True,
                "device": device_map,
                "mel_roformer": MEL_ROFORMER_MODEL,
                "dereverb": DEREVERB_MODEL,
            }
        )
    )


def cmd_isolate_vocals(args):
    """Diagnostic: Mel-Roformer only, 16 kHz mono float32 WAV path printed."""
    import numpy as np
    import librosa
    import soundfile as sf
    from audio_separator.separator import Separator

    stem_dir = tempfile.mkdtemp(prefix="sp_diag_")
    try:
        sep = Separator(
            model_file_dir=args.models_dir,
            output_format="WAV",
            output_dir=stem_dir,
        )
        sep.load_model(MEL_ROFORMER_MODEL)
        out_files = sep.separate(args.audio)
        vocal_path = _pick_vocal_stem(out_files, stem_dir)
        _free_vram(sep)

        audio, _ = librosa.load(vocal_path, sr=16000, mono=True)
        peak = float(np.max(np.abs(audio))) if audio.size else 0.0
        if peak > 1.0:
            audio = audio / peak

        fd, resampled = tempfile.mkstemp(suffix="_vocals16k.wav")
        os.close(fd)
        sf.write(resampled, audio, 16000, subtype="FLOAT")
    finally:
        shutil.rmtree(stem_dir, ignore_errors=True)
    print(json.dumps({"vocal_path": resampled}))


def main():
    parser = argparse.ArgumentParser(description="SongPlayer lyrics Python helper")
    subparsers = parser.add_subparsers(dest="command", required=True)

    p_pre = subparsers.add_parser("preprocess-vocals")
    p_pre.add_argument("--audio", required=True)
    p_pre.add_argument("--output", required=True)
    p_pre.add_argument("--models-dir", required=True)

    p_ac = subparsers.add_parser("align-chunks")
    p_ac.add_argument("--audio", required=True)
    p_ac.add_argument("--chunks", required=True)
    p_ac.add_argument("--output", required=True)

    p_pl = subparsers.add_parser("preload")
    p_pl.add_argument("--models-dir", required=True)

    p_iv = subparsers.add_parser("isolate-vocals")
    p_iv.add_argument("--audio", required=True)
    p_iv.add_argument("--models-dir", required=True)

    args = parser.parse_args()
    dispatch = {
        "preprocess-vocals": cmd_preprocess_vocals,
        "align-chunks": cmd_align_chunks,
        "preload": cmd_preload,
        "isolate-vocals": cmd_isolate_vocals,
    }
    try:
        dispatch[args.command](args)
    except Exception as e:
        print(json.dumps({"error": str(e)}), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Verify file size is sane**

Run: `wc -l scripts/lyrics_worker.py`
Expected: between 150 and 220 lines.

- [ ] **Step 3: Verify Python syntax**

Run: `python3 -c "import ast; ast.parse(open('scripts/lyrics_worker.py').read())"`
Expected: exits 0 (no syntax errors).

- [ ] **Step 4: Commit**

```bash
git add scripts/lyrics_worker.py
git commit -m "feat(lyrics): rewrite Python helper for chunked alignment + de-reverb"
```

---

## Task 7: Bootstrap — pin matched torch triplet + preload anvuew

**Files:**
- Modify: `crates/sp-server/src/lyrics/bootstrap.rs`

- [ ] **Step 1: Add failing test for matched torch triplet pin**

Append to `mod tests { … }` in `crates/sp-server/src/lyrics/bootstrap.rs`:

```rust
    /// bootstrap must pin torch + torchvision + torchaudio to versions that
    /// form a compatible ABI triplet. Observed on win-resolume: installing
    /// `torch` alone with --force-reinstall leaves torchvision at 0.26 and
    /// torchaudio at 2.11, which binds against a torch 2.11 ABI that
    /// doesn't exist on the cu124 index — qwen_asr import fails with
    /// "operator torchvision::nms does not exist".
    #[test]
    fn bootstrap_pins_matched_torch_triplet() {
        let src = include_str!("bootstrap.rs");
        assert!(
            src.contains("torch==2.6.0+cu124"),
            "bootstrap.rs must pin torch==2.6.0+cu124"
        );
        assert!(
            src.contains("torchvision==0.21.0+cu124"),
            "bootstrap.rs must pin torchvision==0.21.0+cu124"
        );
        assert!(
            src.contains("torchaudio==2.6.0+cu124"),
            "bootstrap.rs must pin torchaudio==2.6.0+cu124"
        );
    }

    /// The anvuew dereverb model (SDR 19.17, 2026 SOTA) must be preloaded
    /// at bootstrap so the first song doesn't pay the ~500 MB download
    /// inside the alignment subprocess timeout.
    #[test]
    fn bootstrap_preloads_anvuew_dereverb() {
        let py_src = include_str!("../../../../scripts/lyrics_worker.py");
        assert!(
            py_src.contains("dereverb_mel_band_roformer_anvuew_sdr_19.1729.ckpt"),
            "lyrics_worker.py must reference the anvuew dereverb checkpoint"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sp-server bootstrap::tests::bootstrap_pins_matched_torch_triplet bootstrap::tests::bootstrap_preloads_anvuew_dereverb`
Expected: FAIL (torch triplet not pinned yet). The anvuew test should already PASS because Task 6 wrote the Python file — if it fails, go complete Task 6 first.

- [ ] **Step 3: Change the torch install step to pin the triplet**

In `crates/sp-server/src/lyrics/bootstrap.rs`, replace the `torch_pip.args([…])` block (around lines 215-224) from:

```rust
        torch_pip.args([
            "-m",
            "pip",
            "install",
            "--upgrade",
            "--force-reinstall",
            "torch",
            "--index-url",
            "https://download.pytorch.org/whl/cu124",
        ]);
```

to:

```rust
        // Pin the triplet: installing `torch` alone with --force-reinstall
        // on win-resolume produced torchvision 0.26 + torchaudio 2.11, which
        // bind against a torch 2.11 ABI that doesn't exist on the cu124
        // index. Matched versions keep qwen_asr importable.
        torch_pip.args([
            "-m",
            "pip",
            "install",
            "--upgrade",
            "--force-reinstall",
            "torch==2.6.0+cu124",
            "torchvision==0.21.0+cu124",
            "torchaudio==2.6.0+cu124",
            "--index-url",
            "https://download.pytorch.org/whl/cu124",
        ]);
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sp-server bootstrap::tests`
Expected: all bootstrap tests pass, including the two new ones.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/lyrics/bootstrap.rs
git commit -m "fix(lyrics): pin matched torch 2.6.0+cu124 triplet"
```

---

## Task 8: Rewrite `aligner.rs` — thin wrappers only, delete legacy symbols

**Files:**
- Modify: `crates/sp-server/src/lyrics/aligner.rs` (full rewrite, target ~150 LOC)

- [ ] **Step 1: Overwrite `aligner.rs` with two thin subprocess wrappers**

Write `crates/sp-server/src/lyrics/aligner.rs`:

```rust
//! Rust subprocess wrappers for `lyrics_worker.py`.
//!
//! Two entry points:
//!   - `preprocess_vocals(flac) → clean_wav`: Mel-Roformer + anvuew + 16 kHz
//!   - `align_chunks(wav, chunks) → ChunkResults`: chunked Qwen3 alignment
//!
//! No post-processing, no band-aid, no duplicate-timing fixups. The
//! assembly and quality modules in this crate own all data shaping.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::process::Command;
use tracing::debug;

use crate::lyrics::assembly::{AlignedWord, ChunkResult};
use crate::lyrics::chunking::ChunkRequest;

// ---------------------------------------------------------------------------
// On-disk JSON shapes shared with Python
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ChunkInRequest<'a> {
    chunk_idx: usize,
    start_ms: u64,
    end_ms: u64,
    text: &'a str,
    word_count: usize,
}

#[derive(Debug, Serialize)]
struct ChunkRequestFile<'a> {
    chunks: Vec<ChunkInRequest<'a>>,
}

#[derive(Debug, Deserialize)]
struct ChunkOutWord {
    text: String,
    start_ms: u64,
    end_ms: u64,
}

#[derive(Debug, Deserialize)]
struct ChunkOut {
    chunk_idx: usize,
    words: Vec<ChunkOutWord>,
}

#[derive(Debug, Deserialize)]
struct ChunkResultFile {
    chunks: Vec<ChunkOut>,
}

// ---------------------------------------------------------------------------
// preprocess_vocals
// ---------------------------------------------------------------------------

/// Run Mel-Roformer vocal isolation + anvuew de-reverb + 16 kHz mono float32
/// resample on `audio_in`. Writes the clean WAV to `wav_out` and returns
/// the same path on success.
#[cfg_attr(test, mutants::skip)]
pub async fn preprocess_vocals(
    python_path: &Path,
    script_path: &Path,
    models_dir: &Path,
    audio_in: &Path,
    wav_out: &Path,
) -> Result<PathBuf> {
    let mut cmd = Command::new(python_path);
    cmd.args([
        script_path.as_os_str(),
        "preprocess-vocals".as_ref(),
        "--audio".as_ref(),
        audio_in.as_os_str(),
        "--output".as_ref(),
        wav_out.as_os_str(),
        "--models-dir".as_ref(),
        models_dir.as_os_str(),
    ]);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    debug!(
        "running preprocess-vocals: {} --audio {} --output {}",
        python_path.display(),
        audio_in.display(),
        wav_out.display()
    );

    let mut child = cmd.spawn().context("failed to spawn preprocess-vocals")?;
    let status = match tokio::time::timeout(std::time::Duration::from_secs(600), child.wait()).await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => anyhow::bail!("preprocess-vocals wait failed: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!("preprocess-vocals timed out after 600 s");
        }
    };
    if !status.success() {
        anyhow::bail!("preprocess-vocals exited with status {status}");
    }
    Ok(wav_out.to_path_buf())
}

// ---------------------------------------------------------------------------
// align_chunks
// ---------------------------------------------------------------------------

/// Write `requests` to a temp file, invoke `lyrics_worker.py align-chunks`
/// on the clean WAV, parse the result JSON, and return `ChunkResult`s.
///
/// `chunks_path` and `output_path` are caller-owned scratch files that
/// this function writes and then removes on success.
#[cfg_attr(test, mutants::skip)]
pub async fn align_chunks(
    python_path: &Path,
    script_path: &Path,
    audio_wav: &Path,
    requests: &[ChunkRequest],
    chunks_path: &Path,
    output_path: &Path,
) -> Result<Vec<ChunkResult>> {
    let req_file = ChunkRequestFile {
        chunks: requests
            .iter()
            .enumerate()
            .map(|(idx, r)| ChunkInRequest {
                chunk_idx: idx,
                start_ms: r.start_ms,
                end_ms: r.end_ms,
                text: &r.text,
                word_count: r.word_count,
            })
            .collect(),
    };
    let json = serde_json::to_vec(&req_file)?;
    fs::write(chunks_path, &json)
        .await
        .context("failed to write chunks request file")?;

    let mut cmd = Command::new(python_path);
    cmd.args([
        script_path.as_os_str(),
        "align-chunks".as_ref(),
        "--audio".as_ref(),
        audio_wav.as_os_str(),
        "--chunks".as_ref(),
        chunks_path.as_os_str(),
        "--output".as_ref(),
        output_path.as_os_str(),
    ]);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    debug!(
        "running align-chunks with {} requests on {}",
        requests.len(),
        audio_wav.display()
    );

    let mut child = cmd.spawn().context("failed to spawn align-chunks")?;
    let status = match tokio::time::timeout(std::time::Duration::from_secs(900), child.wait()).await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => anyhow::bail!("align-chunks wait failed: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!("align-chunks timed out after 900 s");
        }
    };
    if !status.success() {
        anyhow::bail!("align-chunks exited with status {status}");
    }

    let content = fs::read_to_string(output_path)
        .await
        .context("failed to read align-chunks output")?;
    let parsed: ChunkResultFile =
        serde_json::from_str(&content).context("failed to parse align-chunks output JSON")?;

    let results = parsed
        .chunks
        .into_iter()
        .map(|c| {
            let line_index = requests
                .get(c.chunk_idx)
                .map(|r| r.line_index)
                .unwrap_or(usize::MAX);
            ChunkResult {
                line_index,
                words: c
                    .words
                    .into_iter()
                    .map(|w| AlignedWord {
                        text: w.text,
                        start_ms: w.start_ms,
                        end_ms: w.end_ms,
                    })
                    .collect(),
            }
        })
        .filter(|r| r.line_index != usize::MAX)
        .collect();

    let _ = fs::remove_file(chunks_path).await;
    let _ = fs::remove_file(output_path).await;

    Ok(results)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// Audit: retired symbols must no longer be referenced from this file.
    /// Keeps the compiler from being the only line of defence against a
    /// dangling `pub use aligner::align_lyrics` re-export leaking back in.
    #[test]
    fn aligner_source_has_no_retired_symbols() {
        let src = include_str!("aligner.rs");
        for banned in [
            "align_lyrics",
            "merge_word_timings",
            "ensure_progressive_words",
            "count_duplicate_start_ms",
        ] {
            assert!(
                !src.contains(banned),
                "aligner.rs must not contain retired symbol `{banned}`"
            );
        }
    }
}
```

- [ ] **Step 2: Run tests and confirm workspace compiles**

Run: `cargo test -p sp-server aligner`
Expected: 1 test passes (the retired-symbols audit). Compilation succeeds.

Run: `cargo clippy -p sp-server --lib -- -D warnings`
Expected: clean (no warnings, no errors). If `worker.rs` still references `aligner::align_lyrics`, that's expected — we fix it in Task 9.

NOTE: If `cargo test` fails because `worker.rs` still references old aligner symbols, do NOT revert this task — continue straight to Task 9, which rewrites `worker.rs` in a paired change. The combined commit of Task 8 + Task 9 should be pushed together to keep the tree compiling.

- [ ] **Step 3: Commit**

```bash
git add crates/sp-server/src/lyrics/aligner.rs
git commit -m "refactor(lyrics): rewrite aligner.rs as thin subprocess wrappers"
```

---

## Task 9: Rewrite `worker.rs` process_song + acquire_lyrics; delete `retry_missing_alignment`

**Files:**
- Modify: `crates/sp-server/src/lyrics/worker.rs` (full rewrite of the processing path)

- [ ] **Step 1: Overwrite `worker.rs`**

Write `crates/sp-server/src/lyrics/worker.rs`:

```rust
//! Lyrics worker orchestrator.
//!
//! Per-song decision tree:
//!   1. acquire_lyrics: YT manual subs first, then LRCLIB fallback, else bail.
//!   2. If source == "yt_subs": run chunked Qwen3 alignment.
//!   3. Gemini SK translation.
//!   4. Persist JSON + DB row.

use anyhow::Result;
use reqwest::Client;
use sp_core::lyrics::LyricsTrack;
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::{
    db::models::{
        get_next_video_missing_translation, get_next_video_without_lyrics, mark_video_lyrics,
    },
    lyrics::{aligner, assembly, chunking, lrclib, quality, translator, youtube_subs},
};

const DUPLICATE_START_WARN_PCT: f64 = 50.0;

#[allow(dead_code)]
pub struct LyricsWorker {
    pool: SqlitePool,
    client: Client,
    cache_dir: PathBuf,
    ytdlp_path: PathBuf,
    python_path: Option<PathBuf>,
    tools_dir: PathBuf,
    script_path: PathBuf,
    models_dir: PathBuf,
    gemini_api_key: String,
    gemini_model: String,
    venv_python: tokio::sync::RwLock<Option<PathBuf>>,
    retry_backoff: tokio::sync::Mutex<RetryBackoff>,
}

#[derive(Default)]
struct RetryBackoff {
    silent_until: Option<Instant>,
    consecutive_failures: u32,
}

impl LyricsWorker {
    pub fn new(
        pool: SqlitePool,
        cache_dir: PathBuf,
        ytdlp_path: PathBuf,
        python_path: Option<PathBuf>,
        tools_dir: PathBuf,
        gemini_api_key: String,
        gemini_model: String,
    ) -> Self {
        let script_path = tools_dir.join("lyrics_worker.py");
        let models_dir = tools_dir.join("hf_models");
        Self {
            pool,
            client: Client::new(),
            cache_dir,
            ytdlp_path,
            python_path,
            tools_dir,
            script_path,
            models_dir,
            gemini_api_key,
            gemini_model,
            venv_python: tokio::sync::RwLock::new(None),
            retry_backoff: tokio::sync::Mutex::new(RetryBackoff::default()),
        }
    }

    #[cfg_attr(test, mutants::skip)]
    async fn ensure_script(&self) -> Result<()> {
        if let Some(parent) = self.script_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(
            &self.script_path,
            include_str!("../../../../scripts/lyrics_worker.py"),
        )
        .await?;
        tracing::info!("lyrics_worker: wrote {}", self.script_path.display());
        Ok(())
    }

    #[cfg_attr(test, mutants::skip)]
    pub async fn run(self, mut shutdown_rx: broadcast::Receiver<()>) {
        tracing::info!("lyrics_worker: started");

        if let Err(e) = self.ensure_script().await {
            error!("lyrics_worker: failed to write lyrics_worker.py: {e}");
        }

        if let Some(sys_py) = self.python_path.as_ref() {
            match crate::lyrics::bootstrap::ensure_ready(
                &self.tools_dir,
                &self.script_path,
                &self.models_dir,
                sys_py,
            )
            .await
            {
                Ok(Some(venv)) => {
                    tracing::info!("lyrics_worker: aligner ready at {}", venv.display());
                    *self.venv_python.write().await = Some(venv);
                }
                Ok(None) => tracing::info!("lyrics_worker: alignment disabled (non-Windows)"),
                Err(e) => warn!("lyrics_worker: bootstrap failed, alignment disabled: {e}"),
            }
        } else {
            warn!("lyrics_worker: no system Python, alignment disabled");
        }

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                _ = self.process_next() => {}
            }
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
        tracing::info!("lyrics_worker: stopped");
    }

    #[cfg_attr(test, mutants::skip)]
    async fn process_next(&self) {
        let row = match get_next_video_without_lyrics(&self.pool).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                self.retry_missing_translations().await;
                debug!("lyrics_worker: no pending videos");
                return;
            }
            Err(e) => {
                error!("lyrics_worker: DB query failed: {e}");
                return;
            }
        };

        let video_id = row.id;
        let youtube_id = row.youtube_id.clone();
        tracing::info!(
            "lyrics_worker: processing video {} ({} - {})",
            youtube_id,
            row.artist,
            row.song
        );

        match self.process_song(row).await {
            Ok(()) => {}
            Err(e) => {
                debug!("lyrics_worker: no lyrics for {youtube_id}: {e}");
                if let Err(db_err) =
                    mark_video_lyrics(&self.pool, video_id, false, Some("no_source")).await
                {
                    error!("lyrics_worker: failed to mark video {youtube_id} as failed: {db_err}");
                }
            }
        }
    }

    #[cfg_attr(test, mutants::skip)]
    async fn process_song(&self, row: crate::db::models::VideoLyricsRow) -> Result<()> {
        let video_id = row.id;
        let youtube_id = row.youtube_id.clone();

        // Step 1: Acquire lyrics. YT subs first, LRCLIB fallback.
        let (mut track, acquired_source) = self.acquire_lyrics(&row).await?;

        // Step 2: If the source is YT manual subs and a venv is ready, run
        // chunked alignment to populate word-level timestamps.
        let final_source = if acquired_source == "yt_subs" {
            let venv_python = self.venv_python.read().await.clone();
            let audio_path = row.audio_file_path.as_ref().map(PathBuf::from);
            match (venv_python.as_ref(), audio_path.as_ref()) {
                (Some(python), Some(audio)) if audio.exists() => {
                    match self.run_chunked_alignment(python, audio, &youtube_id, track).await {
                        Ok(t) => {
                            track = t;
                            "yt_subs+qwen3".to_string()
                        }
                        Err(e) => {
                            warn!("lyrics_worker: chunked alignment failed for {youtube_id}: {e}");
                            "yt_subs".to_string()
                        }
                    }
                }
                _ => {
                    debug!(
                        "lyrics_worker: alignment skipped for {youtube_id} (no venv or audio)"
                    );
                    "yt_subs".to_string()
                }
            }
        } else {
            acquired_source
        };
        track.source = final_source.clone();

        // Step 3: Gemini translation (if configured).
        if !self.gemini_api_key.is_empty() {
            if let Err(e) =
                translator::translate_lyrics(&self.gemini_api_key, &self.gemini_model, &mut track)
                    .await
            {
                warn!(
                    "lyrics_worker: translation failed for {youtube_id}, persisting EN only: {e}"
                );
            }
        }

        // Step 4: Persist.
        let json_path = self.cache_dir.join(format!("{youtube_id}_lyrics.json"));
        let json_bytes = serde_json::to_vec(&track)?;
        tokio::fs::write(&json_path, &json_bytes).await?;
        mark_video_lyrics(&self.pool, video_id, true, Some(&final_source)).await?;

        tracing::info!(
            "lyrics_worker: persisted lyrics for {youtube_id} (source={final_source})"
        );
        Ok(())
    }

    /// Plan chunks → preprocess vocals → align → assemble. On any hard
    /// error, returns `Err` and the caller keeps the line-level track.
    #[cfg_attr(test, mutants::skip)]
    async fn run_chunked_alignment(
        &self,
        python: &std::path::Path,
        audio: &std::path::Path,
        youtube_id: &str,
        track: LyricsTrack,
    ) -> Result<LyricsTrack> {
        let requests = chunking::plan_chunks(&track);
        if requests.is_empty() {
            return Ok(track);
        }

        let wav_path = self.cache_dir.join(format!("{youtube_id}_vocals16k.wav"));
        aligner::preprocess_vocals(
            python,
            &self.script_path,
            &self.models_dir,
            audio,
            &wav_path,
        )
        .await?;

        let chunks_path = self.cache_dir.join(format!("{youtube_id}_chunks.json"));
        let out_path = self.cache_dir.join(format!("{youtube_id}_align_out.json"));
        let results = aligner::align_chunks(
            python,
            &self.script_path,
            &wav_path,
            &requests,
            &chunks_path,
            &out_path,
        )
        .await?;

        // Best-effort cleanup of the scratch WAV.
        let _ = tokio::fs::remove_file(&wav_path).await;

        let assembled = assembly::assemble(track, results);
        self.warn_on_degenerate_lines(&assembled, youtube_id);
        Ok(assembled)
    }

    fn warn_on_degenerate_lines(&self, track: &LyricsTrack, youtube_id: &str) {
        for (idx, line) in track.lines.iter().enumerate() {
            let pct = quality::duplicate_start_pct(line);
            if pct > DUPLICATE_START_WARN_PCT {
                warn!(
                    "lyrics_worker: degenerate alignment on {youtube_id} line {idx} ({pct:.1}% duplicate starts)"
                );
            }
        }
    }

    /// YT manual subs first, LRCLIB second, else bail.
    #[cfg_attr(test, mutants::skip)]
    async fn acquire_lyrics(
        &self,
        row: &crate::db::models::VideoLyricsRow,
    ) -> Result<(LyricsTrack, String)> {
        let youtube_id = &row.youtube_id;

        // 1. YouTube manual subs (skip on non-Windows / if ytdlp missing).
        let tmp = std::env::temp_dir().join("sp_yt_subs");
        let _ = tokio::fs::create_dir_all(&tmp).await;
        match youtube_subs::fetch_subtitles(&self.ytdlp_path, youtube_id, &tmp).await {
            Ok(Some(track)) => {
                info!("lyrics_worker: YT manual subs hit for {youtube_id}");
                return Ok((track, "yt_subs".to_string()));
            }
            Ok(None) => debug!("lyrics_worker: no YT manual subs for {youtube_id}"),
            Err(e) => warn!("lyrics_worker: YT sub fetch error for {youtube_id}: {e}"),
        }

        // 2. LRCLIB.
        if !row.song.is_empty() && !row.artist.is_empty() {
            let duration_s = row.duration_ms.map(|ms| (ms / 1000) as u32).unwrap_or(0);
            match lrclib::fetch_lyrics(&self.client, &row.artist, &row.song, duration_s).await {
                Ok(Some(track)) => {
                    info!("lyrics_worker: LRCLIB hit for {youtube_id}");
                    return Ok((track, "lrclib".to_string()));
                }
                Ok(None) => debug!("lyrics_worker: LRCLIB miss for {youtube_id}"),
                Err(e) => warn!("lyrics_worker: LRCLIB error for {youtube_id}: {e}"),
            }
        }

        anyhow::bail!("no lyrics source for {youtube_id}")
    }

    #[cfg_attr(test, mutants::skip)]
    async fn retry_missing_translations(&self) {
        if self.gemini_api_key.is_empty() {
            return;
        }
        {
            let backoff = self.retry_backoff.lock().await;
            if let Some(until) = backoff.silent_until
                && Instant::now() < until
            {
                return;
            }
        }
        let result = get_next_video_missing_translation(&self.pool, &self.cache_dir).await;
        let (_video_id, youtube_id) = match result {
            Ok(Some(pair)) => pair,
            _ => return,
        };
        let lyrics_path = self.cache_dir.join(format!("{youtube_id}_lyrics.json"));
        let content = match tokio::fs::read_to_string(&lyrics_path).await {
            Ok(c) => c,
            Err(e) => {
                debug!("lyrics retry: read failed for {youtube_id}: {e}");
                return;
            }
        };
        let mut track: LyricsTrack = match serde_json::from_str(&content) {
            Ok(t) => t,
            Err(e) => {
                debug!("lyrics retry: parse failed for {youtube_id}: {e}");
                return;
            }
        };
        info!("lyrics_worker: retrying translation for {youtube_id}");
        match translator::translate_lyrics(&self.gemini_api_key, &self.gemini_model, &mut track)
            .await
        {
            Ok(()) => {
                let json = serde_json::to_vec(&track).unwrap_or_default();
                let _ = tokio::fs::write(&lyrics_path, &json).await;
                info!("lyrics_worker: translation retry succeeded for {youtube_id}");
                let mut backoff = self.retry_backoff.lock().await;
                backoff.consecutive_failures = 0;
                backoff.silent_until = None;
            }
            Err(e) => {
                debug!("lyrics_worker: translation retry failed for {youtube_id}: {e}");
                let mut backoff = self.retry_backoff.lock().await;
                backoff.consecutive_failures = backoff.consecutive_failures.saturating_add(1);
                let attempt_index = backoff.consecutive_failures.saturating_sub(1).min(4);
                let secs = 60u64.saturating_mul(1u64 << attempt_index).min(600);
                backoff.silent_until = Some(Instant::now() + Duration::from_secs(secs));
                warn!(
                    "lyrics_worker: translation backoff for {secs}s after {} consecutive failures",
                    backoff.consecutive_failures
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn worker_has_no_retired_symbols() {
        let src = include_str!("worker.rs");
        for banned in [
            "retry_missing_alignment",
            "count_duplicate_start_ms",
            "merge_word_timings",
            "ensure_progressive_words",
            "set_video_lyrics_source",
            "get_next_video_missing_alignment",
        ] {
            assert!(
                !src.contains(banned),
                "worker.rs must not contain retired symbol `{banned}`"
            );
        }
        // The retired lyrics_source value must not appear as a literal.
        // Match on the quoted form to avoid false-positives from the
        // retired-symbol audit lines above.
        assert!(
            !src.contains("\"lrclib+qwen3\""),
            "worker.rs must not write the retired 'lrclib+qwen3' source literal"
        );
    }

    /// `acquire_lyrics` must call YouTube manual subs BEFORE LRCLIB. This
    /// is the single most important ordering decision in the pipeline —
    /// if LRCLIB wins for a song that has YT manual subs, the #148 E2E
    /// gate fails because `source == "lrclib"` instead of `yt_subs+qwen3`.
    #[test]
    fn acquire_lyrics_calls_youtube_subs_before_lrclib() {
        let src = include_str!("worker.rs");
        // Find the `async fn acquire_lyrics` body.
        let body_start = src
            .find("async fn acquire_lyrics")
            .expect("acquire_lyrics must exist");
        let body = &src[body_start..];
        let yt_pos = body
            .find("youtube_subs::fetch_subtitles")
            .expect("acquire_lyrics must call youtube_subs::fetch_subtitles");
        let lrclib_pos = body
            .find("lrclib::fetch_lyrics")
            .expect("acquire_lyrics must call lrclib::fetch_lyrics");
        assert!(
            yt_pos < lrclib_pos,
            "YouTube subs fetch must happen before LRCLIB fetch in acquire_lyrics"
        );
    }
}
```

- [ ] **Step 2: Run all tests**

Run: `cargo test -p sp-server`
Expected: compiles and all tests pass. If `db::models` still has `set_video_lyrics_source` / `get_next_video_missing_alignment` definitions, that's OK — they're about to be deleted in Task 10.

- [ ] **Step 3: Verify clippy clean**

Run: `cargo clippy -p sp-server --lib -- -D warnings`
Expected: 0 warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/worker.rs
git commit -m "refactor(lyrics): rewrite worker around YT-subs-first + chunked alignment"
```

---

## Task 10: Delete retired DB query methods

**Files:**
- Modify: `crates/sp-server/src/db/models.rs` (delete `set_video_lyrics_source` at ~line 422-436 and `get_next_video_missing_alignment` at ~line 438-460)

- [ ] **Step 1: Delete the two functions**

In `crates/sp-server/src/db/models.rs`, remove lines 422-460 (both function definitions including their doc comments). Keep `reset_video_lyrics` (lines 412-420) untouched.

The removed block is:

```rust
/// Update only the `lyrics_source` label on a video (used after re-alignment
/// adds word-level timestamps to a previously line-only LRCLIB track).
#[cfg_attr(test, mutants::skip)]
pub async fn set_video_lyrics_source(
    pool: &SqlitePool,
    video_id: i64,
    lyrics_source: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE videos SET lyrics_source = ? WHERE id = ?")
        .bind(lyrics_source)
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Return the next video that has lyrics from LRCLIB (line-level only, never
/// aligned with Qwen3) AND has an audio file ≤5 minutes (Qwen3 architectural
/// limit). Used by the worker to retroactively add word-level timestamps to
/// songs processed before the aligner was wired in.
#[cfg_attr(test, mutants::skip)]
pub async fn get_next_video_missing_alignment(
    pool: &SqlitePool,
) -> Result<Option<VideoLyricsRow>, sqlx::Error> {
    sqlx::query_as::<_, VideoLyricsRow>(
        "SELECT v.id, v.youtube_id, COALESCE(v.song, '') as song, \
         COALESCE(v.artist, '') as artist, v.duration_ms, v.audio_file_path, \
         p.youtube_url \
         FROM videos v \
         JOIN playlists p ON p.id = v.playlist_id \
         WHERE v.has_lyrics = 1 AND v.lyrics_source = 'lrclib' \
         AND v.audio_file_path IS NOT NULL \
         AND v.duration_ms IS NOT NULL AND v.duration_ms <= 300000 \
         AND p.is_active = 1 \
         ORDER BY v.id LIMIT 1",
    )
    .fetch_optional(pool)
    .await
}
```

Delete it entirely. Also remove any existing test cases in `mod tests` (search the file) that reference these two functions — if none exist, no further cleanup needed.

- [ ] **Step 2: Remove the obsolete `rs-import` line from `worker.rs`**

If you kept the paired imports in Task 9 they are already correct; otherwise open `crates/sp-server/src/lyrics/worker.rs` line ~17 and confirm the `use crate::db::models::{...}` block only imports `get_next_video_missing_translation`, `get_next_video_without_lyrics`, and `mark_video_lyrics`. There should be no reference to `set_video_lyrics_source` or `get_next_video_missing_alignment`.

- [ ] **Step 3: Verify compile and tests**

Run: `cargo test -p sp-server`
Expected: compiles, all tests pass (retired-symbols audit in `worker.rs` and `aligner.rs` both green).

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/db/models.rs
git commit -m "refactor(db): delete retired set_video_lyrics_source + get_next_video_missing_alignment"
```

---

## Task 11: CI — legacy-code deletion audit

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add a deletion-audit step to the `test-integrity` job**

In `.github/workflows/ci.yml`, inside the `test-integrity` job (starting at line 211), add a new step AFTER the existing "Ban skip patterns in E2E tests" step and BEFORE "Verify deploy job uses always() condition":

```yaml
      - name: Deletion audit — no legacy lyrics pipeline symbols
        run: |
          set -eu
          echo "Scanning repo for retired symbols from the old Qwen3 whole-song path..."
          BANNED_NAMES=(
            "align_lyrics"
            "merge_word_timings"
            "ensure_progressive_words"
            "count_duplicate_start_ms"
            "retry_missing_alignment"
            "set_video_lyrics_source"
            "get_next_video_missing_alignment"
          )
          BANNED_LITERALS=(
            '"lrclib+qwen3"'
          )
          BANNED_CLI=(
            "--write-auto-subs"
          )
          BANNED_PY=(
            "def cmd_align("
            "def cmd_transcribe("
            "def cmd_download_models("
            "def _group_words_into_lines("
          )
          FAIL=0
          SEARCH_PATHS=(crates src-tauri sp-ui scripts e2e .github/workflows)
          for name in "${BANNED_NAMES[@]}"; do
            HITS=$(grep -rn "$name" "${SEARCH_PATHS[@]}" \
              --exclude-dir=node_modules --exclude-dir=target \
              --exclude-dir=dist --exclude=ci.yml 2>/dev/null || true)
            if [ -n "$HITS" ]; then
              echo "ERROR: retired symbol '$name' still referenced:"; echo "$HITS"; FAIL=1
            fi
          done
          for lit in "${BANNED_LITERALS[@]}"; do
            HITS=$(grep -rn "$lit" "${SEARCH_PATHS[@]}" \
              --exclude-dir=node_modules --exclude-dir=target \
              --exclude-dir=dist --exclude=ci.yml 2>/dev/null || true)
            if [ -n "$HITS" ]; then
              echo "ERROR: retired literal $lit still present:"; echo "$HITS"; FAIL=1
            fi
          done
          for cli in "${BANNED_CLI[@]}"; do
            HITS=$(grep -rn -- "$cli" "${SEARCH_PATHS[@]}" \
              --exclude-dir=node_modules --exclude-dir=target \
              --exclude-dir=dist --exclude=ci.yml 2>/dev/null || true)
            if [ -n "$HITS" ]; then
              echo "ERROR: retired CLI arg '$cli' still present:"; echo "$HITS"; FAIL=1
            fi
          done
          for py in "${BANNED_PY[@]}"; do
            HITS=$(grep -rn "$py" scripts 2>/dev/null || true)
            if [ -n "$HITS" ]; then
              echo "ERROR: retired Python function '$py' still defined:"; echo "$HITS"; FAIL=1
            fi
          done
          if [ "$FAIL" -ne 0 ]; then
            echo "FAIL: legacy code leaked back in."
            exit 1
          fi
          echo "OK: no retired symbols found."
```

- [ ] **Step 2: Verify the audit runs clean locally**

Run (from repo root):
```bash
set -eu
BANNED_NAMES=(align_lyrics merge_word_timings ensure_progressive_words count_duplicate_start_ms retry_missing_alignment set_video_lyrics_source get_next_video_missing_alignment)
for name in "${BANNED_NAMES[@]}"; do
  HITS=$(grep -rn "$name" crates src-tauri sp-ui scripts e2e --exclude-dir=node_modules --exclude-dir=target --exclude-dir=dist 2>/dev/null || true)
  if [ -n "$HITS" ]; then echo "HIT: $name"; echo "$HITS"; fi
done
```

Expected: no output (all retired symbols are already deleted). If any hit remains, go back to Tasks 8-10 and clean it up before committing this CI change.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add legacy lyrics pipeline deletion audit"
```

---

## Task 12: E2E test — #148 Planetshakers "Get This Party Started"

**Files:**
- Modify: `e2e/post-deploy-flac.spec.ts` (append one new test inside the existing `test.describe` block)

- [ ] **Step 1: Add the new test case**

In `e2e/post-deploy-flac.spec.ts`, just above the existing final `});` of the `test.describe` block (the one at the very bottom of the file), insert:

```typescript
  test("song #148 Planetshakers 'Get This Party Started' has real word-level alignment", async ({
    request,
  }) => {
    // 25-minute budget covers cold-start bootstrap (1.2 GB Qwen3 + 500 MB
    // Mel-Roformer + 500 MB anvuew downloads) PLUS the first song running
    // through the new pipeline. Tight enough to catch the pipeline
    // quietly falling back to LRCLIB line-level, generous enough for a
    // genuine first boot.
    test.setTimeout(28 * 60 * 1000);

    // Match by song + artist rather than YouTube ID: the same track may be
    // uploaded multiple times; what matters is that AT LEAST ONE copy hits
    // the YT-subs chunked path. Lowercased substring match is robust to
    // "(Live)" / "(feat. ...)" suffixes.
    const SONG_NEEDLE = "party started";
    const ARTIST_NEEDLE = "planetshaker";
    const MIN_LINES = 30;               // song has ~39 lines
    const MIN_TOTAL_WORDS = 200;         // song has ~214 words
    const MAX_DUPLICATE_PCT = 10;        // empirical ceiling (Resolume: 4.7%)
    const MIN_STDDEV_LINES = 10;         // ≥10 lines must show irregular gaps
    const MIN_STDDEV_MS = 50;            // ms threshold per line

    interface Word { text: string; start_ms: number; end_ms: number }
    interface Line { start_ms?: number; end_ms?: number; en: string; words?: Word[] }
    interface Track { source?: string; lines: Line[] }

    function duplicateStartPct(line: Line): number {
      const w = line.words ?? [];
      if (w.length < 2) return 0;
      let dup = 0;
      for (let i = 1; i < w.length; i++) {
        if (w[i].start_ms === w[i - 1].start_ms) dup += 1;
      }
      return (100 * dup) / (w.length - 1);
    }

    function gapStddevMs(line: Line): number {
      const w = line.words ?? [];
      if (w.length < 3) return 0;
      const gaps: number[] = [];
      for (let i = 1; i < w.length; i++) gaps.push(w[i].start_ms - w[i - 1].start_ms);
      const mean = gaps.reduce((a, b) => a + b, 0) / gaps.length;
      const variance =
        gaps.map((g) => (g - mean) ** 2).reduce((a, b) => a + b, 0) / gaps.length;
      return Math.sqrt(variance);
    }

    async function findTrack(): Promise<Track | null> {
      const plResp = await request.get("/api/v1/playlists");
      if (!plResp.ok()) return null;
      const playlists: PlaylistEntry[] = await plResp.json();
      for (const pl of playlists) {
        const vidResp = await request.get(`/api/v1/playlists/${pl.id}/videos`);
        if (!vidResp.ok()) continue;
        const videos: VideoEntry[] = await vidResp.json();
        for (const v of videos) {
          const song = (v.song ?? "").toLowerCase();
          const artist = (v.artist ?? "").toLowerCase();
          if (!song.includes(SONG_NEEDLE)) continue;
          if (!artist.includes(ARTIST_NEEDLE)) continue;
          const lyricsResp = await request.get(`/api/v1/videos/${v.id}/lyrics`);
          if (!lyricsResp.ok()) return null;
          return (await lyricsResp.json()) as Track;
        }
      }
      return null;
    }

    const track = await expect
      .poll(
        async () => {
          const t = await findTrack();
          const summary = t
            ? `source=${t.source ?? "?"} lines=${t.lines?.length ?? 0}`
            : "not-yet";
          console.log(`[#148 poll] ${summary} @ ${new Date().toISOString()}`);
          return t;
        },
        {
          message:
            `#148 "Get This Party Started" never produced lyrics in 25 min. ` +
            `Either the song didn't sync into any playlist, or the pipeline ` +
            `aborted before persisting. Check the server log on win-resolume.`,
          timeout: 25 * 60 * 1000,
          intervals: [30_000],
        },
      )
      .not.toBeNull();

    // Gate 1: source must be the YT-subs chunked-alignment happy path.
    expect(
      track!.source,
      `Expected source "yt_subs+qwen3" (proves new pipeline ran). Got "${track!.source}". ` +
        `Fallback to LRCLIB means #148 did NOT get word-level karaoke.`,
    ).toBe("yt_subs+qwen3");

    // Gate 2: line count plausible for this song.
    expect(track!.lines.length).toBeGreaterThanOrEqual(MIN_LINES);

    // Gate 3: every line must have a populated `words` array.
    for (const [i, line] of track!.lines.entries()) {
      expect(
        Array.isArray(line.words) && line.words.length > 0,
        `Line ${i} ("${line.en}") has no words — assembly/align failed for this chunk.`,
      ).toBe(true);
    }

    // Gate 4: total word count ≥ threshold.
    const totalWords = track!.lines.reduce((sum, l) => sum + (l.words?.length ?? 0), 0);
    expect(
      totalWords,
      `Total word count ${totalWords} below threshold ${MIN_TOTAL_WORDS}`,
    ).toBeGreaterThanOrEqual(MIN_TOTAL_WORDS);

    // Gate 5: duplicate-start percentage across the whole track < threshold.
    const perLinePcts = track!.lines.map(duplicateStartPct);
    const allDuplicate =
      perLinePcts.reduce((s, p, i) => s + p * (track!.lines[i].words?.length ?? 0), 0) /
      Math.max(1, totalWords);
    expect(
      allDuplicate,
      `Weighted duplicate_start_pct ${allDuplicate.toFixed(2)}% exceeds ${MAX_DUPLICATE_PCT}% — ` +
        `alignment is degenerate (multiple words share start_ms on many lines).`,
    ).toBeLessThan(MAX_DUPLICATE_PCT);

    // Gate 6: ≥ MIN_STDDEV_LINES lines show real inter-word timing variation.
    const stddevLines = track!.lines.filter((l) => gapStddevMs(l) >= MIN_STDDEV_MS).length;
    expect(
      stddevLines,
      `Only ${stddevLines} lines have gap_stddev ≥ ${MIN_STDDEV_MS}ms ` +
        `(need ≥ ${MIN_STDDEV_LINES}). Even spacing is a signature of a synthetic ` +
        `post-processor, not a real aligner.`,
    ).toBeGreaterThanOrEqual(MIN_STDDEV_LINES);

    console.log(
      `#148 alignment OK: lines=${track!.lines.length} ` +
        `words=${totalWords} dup%=${allDuplicate.toFixed(2)} ` +
        `stddev_lines=${stddevLines}`,
    );
  });
```

- [ ] **Step 2: Verify TypeScript type-checks**

Run: `cd e2e && npx tsc --noEmit && cd ..`
Expected: clean output (no type errors).

- [ ] **Step 3: Commit**

```bash
git add e2e/post-deploy-flac.spec.ts
git commit -m "test(e2e): hard-assert #148 Planetshakers word-level alignment"
```

---

## Task 13: Final compile + CI + deploy verification

**Files:** none modified — verification only.

- [ ] **Step 1: Final local format check**

Run: `cargo fmt --all --check`
Expected: clean (exit 0).
If this fails, run `cargo fmt --all` and include the fixup in the final commit.

- [ ] **Step 2: Push to dev**

```bash
git push origin dev
```

- [ ] **Step 3: Monitor CI run to terminal state**

```bash
gh run list --branch dev --limit 3
# Pick the latest run id
gh run view <run-id> --json status,conclusion
sleep 300 && gh run view <run-id> --json status,conclusion,jobs
```

Expected: run reaches `status=completed conclusion=success`. All jobs green — including `test-integrity` (deletion audit passes), `mutation-testing`, and `deploy-resolume`.

- [ ] **Step 4: Post-deploy verification on win-resolume**

Use MCP `mcp__win-resolume__*` tools (never SSH fallback):
1. Confirm SongPlayer service running:
   ```
   mcp__win-resolume__Shell: "Get-Process SongPlayer | Select-Object Id, StartTime"
   ```
   Expected: one process, fresh `StartTime`.
2. Confirm E2E `song #148` test executed in the CI frontend-e2e job:
   ```bash
   gh run view <run-id> --log | grep -E "#148 alignment OK|#148 poll"
   ```
   Expected: at least one `#148 alignment OK` line AND multiple `[#148 poll]` lines showing progression to a real track.
3. Open dashboard in Playwright and navigate to the `ytfast` playlist card showing #148 playing — confirm karaoke highlights words live (visual verification).

- [ ] **Step 5: PR to main**

```bash
gh pr create --base main --head dev \
  --title "feat(lyrics): YT manual subs + chunked Qwen3 alignment (Phase 1)" \
  --body "$(cat <<'EOF'
## Summary

Replaces broken whole-song Qwen3 alignment with YT-manual-subs-first chunked alignment. Song #148 "Get This Party Started" now produces real word-level karaoke — verified by a new E2E gate with six hard assertions.

- Rust: three new pure modules (`chunking`, `assembly`, `quality`); `aligner.rs` shrinks to two thin subprocess wrappers; `worker.rs` rewrites the decision tree.
- Python: three narrow entry points (`preprocess-vocals`, `align-chunks`, `preload`); retired `cmd_align` / `cmd_transcribe` / `cmd_download_models` / `_group_words_into_lines`.
- Bootstrap: pins matched torch 2.6.0+cu124 / torchvision 0.21.0+cu124 / torchaudio 2.6.0+cu124 triplet; preloads anvuew dereverb (SDR 19.17 2026 SOTA).
- DB: migration V9 resets all rows; retired `lrclib+qwen3` lyrics_source value.
- CI: deletion audit hard-fails if any retired symbol leaks back.

## Test plan

- [x] Unit tests pass (`cargo test --workspace`)
- [x] Clippy clean
- [x] Deletion audit clean (`test-integrity`)
- [x] Mutation tests clean on diff
- [x] Build-windows green
- [x] Build-tauri green
- [x] Deploy to win-resolume succeeded
- [x] E2E gate on #148 green
- [x] Manual Playwright verification of live karaoke on dashboard

Spec: `docs/superpowers/specs/2026-04-15-yt-subs-chunked-alignment-design.md`
Plan: `docs/superpowers/plans/2026-04-15-yt-subs-chunked-alignment.md`
EOF
)"
```

Expected: PR URL returned. Do NOT merge — wait for explicit user approval.

---

## Verification

After all tasks are complete, the following must hold:

1. `cargo test --workspace` — all green
2. `cargo fmt --all --check` — clean
3. `cargo clippy --workspace -- -D warnings` — clean
4. CI job `test-integrity` — deletion audit reports zero retired symbols
5. CI job `deploy-resolume` — installer deployed successfully to win-resolume
6. CI job `frontend-e2e` — `#148 alignment OK` line in log, all six gates passed
7. Manual Playwright on `http://10.77.9.201:8920/` shows live karaoke highlighting on #148

If any item fails: STOP, investigate, fix on dev, rerun CI. No skip, no `continue-on-error`, no "future PR".
