# YouTube Manual Subtitles + Chunked Qwen3 Alignment (Phase 1)

**Status:** design approved 2026-04-15, awaiting spec review
**Supersedes:** `2026-04-14-vocal-isolation-revised-design.md` (its approach — whole-song Qwen3 alignment — proved unviable on live recordings; 91.5% degenerate timings on Planetshakers #148 even with best vocal isolation + de-reverb)

## Why this revision

Whole-song Qwen3-ForcedAligner-0.6B hit a hard capacity ceiling. Empirical findings on win-resolume:

1. **Mel-Roformer isolation is clean** — Qwen3-ASR-1.7B transcribed the isolated vocal stem fluently, confirming separation quality.
2. **Aligner is the bottleneck** — even when fed ASR's own transcription back into Qwen3-ForcedAligner, 57-71% of word-start timestamps collapsed to duplicates. The 0.6 B parameter aligner cannot localize phoneme boundaries on 3-4 minute music recordings.
3. **LRCLIB is community-sourced, not authoritative** — for `#148` Planetshakers' "Get This Party Started", LRCLIB's text matched the official published lyrics, but the song was a live recording where the band extended sections and changed pronouns; the aligner had no way to reconcile LRCLIB text against audio that contained additional material.
4. **YouTube manual subtitles are author-verified** — `yt-dlp --write-subs --sub-langs en` (no `--write-auto-subs`) retrieves captions uploaded by the video owner. On #148 these captions carry both correct lyric text AND accurate line-level timings as verified by spot-check against the published song.
5. **Chunked alignment with YT-sub timings works** — feeding Qwen3-ForcedAligner short (5-15 s) audio chunks matched to 1-3 lines of text at a time produced **4.7 % duplicate-start pairs on #148**, a 19× improvement over the whole-song path. All 27 SRT blocks aligned cleanly; per-line word timings are usable for karaoke highlighting.

The catalog survey (112 of 230 cached videos sampled) showed **12.5 % of songs have English manual YT subs**, concentrated in the Planetshakers and Elevation Worship channels. Public word-level lyric databases (`syncedlyrics --enhanced`) are not a viable alternative — Musixmatch returns 401 on free endpoints, LRCLIB's enhanced tier has no worship coverage.

This revision scopes word-level karaoke to the YT-sub subset. Non-YT-sub songs get LRCLIB line-level display (current behavior, unchanged).

## Target outcome

- `#148` Planetshakers "Get This Party Started" displays real per-word karaoke highlighting after deploy.
- 13 other songs with English manual YT subs get the same treatment.
- Remaining ~87 % of catalog gets line-level lyrics from LRCLIB (no regression vs current state).
- CI fails if `#148` re-deploys with the old broken pipeline — a hard anti-regression gate.

## Per-song decision tree

```
process_song(video):
  1. acquire_lyrics:
      ├── YT manual subs found? → (track, source="yt_subs"), goto 2
      ├── LRCLIB found?         → (track, source="lrclib"),  goto 3
      └── neither               → bail, no lyrics

  2. [yt_subs path] chunked Qwen3 alignment
      preprocess audio once:  Mel-Roformer → anvuew 19.17 de-reverb → 16 kHz mono float32
      plan_chunks:            from YT-sub events, build list of ChunkRequests
                              (each = {start_ms, end_ms with ±500ms pad, lines[], per_line_word_counts[]})
      align_chunks:           Python subprocess, Qwen3 loaded ONCE, loops over all chunks
      assemble:               split word stream back into per-line arrays
      quality check:          warn if duplicate-start > 50% on any line (diagnostic, non-blocking)
      lyrics_source = "yt_subs+qwen3"

  3. [lrclib path] line-level only (no alignment)
      lyrics_source = "lrclib"

  4. Gemini SK translation (unchanged)
  5. persist JSON + DB row
```

## Component split — Rust heavy, Python minimal

Everything not requiring PyTorch / audio-separator inference moves to Rust.

### Python — inference only (`scripts/lyrics_worker.py`)

```
cmd_preprocess_vocals --audio FLAC --output WAV:
    Mel-Roformer vocal isolation (load, separate, unload)
    anvuew 19.17 de-reverb      (load, separate, unload)
    librosa resample to 16 kHz mono float32
    soundfile write FLOAT WAV

cmd_align_chunks --audio WAV --chunks chunks.json --output result.json:
    load Qwen3-ForcedAligner-0.6B ONCE
    for each chunk: align audio[start..end] with chunk.text
    write {chunks: [{chunk_idx, words: [{text,start_ms,end_ms}, ...]}, ...]}

cmd_preload:
    warm Mel-Roformer + anvuew dereverb + Qwen3-ForcedAligner
    (surface model-download failures at bootstrap, not first song)

cmd_isolate_vocals (unchanged, diagnostic)
```

Python is ~180 LOC total. All model loading/unloading/VRAM management in this file; no orchestration, no data-shape decisions, no quality checks.

### Rust — orchestration + data (`crates/sp-server/src/lyrics/`)

| File | Responsibility | LOC estimate |
|---|---|---|
| `youtube_subs.rs` | yt-dlp invocation (drop `--write-auto-subs`, keep `--write-subs --sub-format json3 --sub-lang en`). Existing json3 parser emits `LyricsTrack`. | −20 / +0 |
| `aligner.rs` | Two thin subprocess wrappers: `preprocess_vocals(flac) → clean_wav_path`, `align_chunks(wav, chunks) → ChunkResults`. No post-processing. No band-aid. | −500 / +80 (deletes `align_lyrics`, `merge_word_timings`, `ensure_progressive_words`, `count_duplicate_start_ms` + 19 unit tests) |
| `chunking.rs` **new** | Pure Rust: `plan_chunks(track: LyricsTrack) → Vec<ChunkRequest>`. Adds ±500 ms audio-window padding, derives per-line word counts. Unit-testable with zero audio. | +120 |
| `assembly.rs` **new** | Pure Rust: `assemble(original_track, chunk_results) → LyricsTrack`. Splits word stream into per-line arrays, handles under/over-aligned counts. Unit-testable. | +100 |
| `quality.rs` **new** | Pure Rust: `duplicate_start_pct(line) → f64`, `gap_stddev_ms(line) → f64`. Used for server-side warn! and E2E assertion mirror. | +60 |
| `worker.rs::process_song` | Orchestrates: fetch subs → `plan_chunks` → `preprocess_vocals` → `align_chunks` → `assemble` → Gemini translate → persist. | net −50 |
| `worker.rs::acquire_lyrics` | Priority: YT subs first, LRCLIB fallback, else bail. | +10 |
| `worker.rs::retry_missing_alignment` | **Deleted** — migration V9 resets everything once; no retroactive loop needed. | −120 |
| `db/models.rs` | Delete `set_video_lyrics_source`, `get_next_video_missing_alignment` (only used by retry loop). | −60 |
| `db/mod.rs` | Add `MIGRATION_V9` = reset all rows. | +10 |

All new Rust files stay well under the 1000-line CI cap. `worker.rs` shrinks.

### Bootstrap (`bootstrap.rs`)

Install order confirmed working on win-resolume: `qwen-asr` → `audio-separator[gpu]` → CUDA torch LAST. The new change: pin matched torch set in one pip call.

```rust
// Pin matched torch triplet — installing `torch` alone with --force-reinstall
// leaves torchvision/torchaudio at incompatible versions (ABI mismatch proven
// on win-resolume: torchvision 0.26 paired with torch 2.6 → "operator
// torchvision::nms does not exist" at qwen_asr import time).
let args = [
    "-m", "pip", "install", "--upgrade", "--force-reinstall",
    "torch==2.6.0+cu124",
    "torchvision==0.21.0+cu124",
    "torchaudio==2.6.0+cu124",
    "--index-url", "https://download.pytorch.org/whl/cu124",
];
```

The existing `is_ready` probe constant (`IS_READY_PROBE` with `import qwen_asr, torch, audio_separator, sys; sys.exit(0 if torch.cuda.is_available() else 1)`) stays as-is.

## Data model + migration

**`lyrics_source` values in use after this PR:**

| Value | Meaning |
|---|---|
| `yt_subs+qwen3` | Word-level karaoke (happy path — YT manual subs + chunked Qwen3) |
| `lrclib` | Line-level only (fallback for songs without YT subs) |
| `NULL` | Not yet processed |

Retired: `lrclib+qwen3`, `yt_subs` (the intermediate state never persists — either the song gets word-level alignment or it fell back to LRCLIB during acquire_lyrics).

**Migration V9:**

```sql
-- V9: reset all lyrics rows to re-process through the new YT-subs-first
-- pipeline. Old lyrics_source values ('lrclib+qwen3') are retired.
UPDATE videos SET has_lyrics = 0, lyrics_source = NULL;
```

Idempotent. Runs exactly once per database via `schema_version` gate. V7+V8 still fire cleanly on fresh installs; V9 resets again on top of them — harmless no-op when the columns were already `(0, NULL)`.

## Testing

### Unit tests (Rust, pure functions, zero audio)

- `chunking::plan_chunks` — fixture `LyricsTrack` with 3 lines → 3 `ChunkRequest`s with correct start/end padding and per-line word counts
- `chunking` edge cases — first/last line padding clamped to song bounds; empty lines skipped
- `assembly::assemble` — chunk with 10-word stream + 2 input lines of [4, 6] words → 2 output lines with correct word slices
- `assembly` under-aligned — aligner returned 8 words for 10 expected → remaining 2 words get `NULL` placeholder (don't crash)
- `assembly` over-aligned — aligner returned 12 for 10 → surplus 2 words dropped
- `quality::duplicate_start_pct` and `gap_stddev_ms` on degenerate + real fixtures
- `youtube_subs::fetch_subtitles` returns `None` when yt-dlp writes no `.json3` file (simulates video without manual subs)
- `acquire_lyrics` priority: mock YT returns `Some` → LRCLIB not called; mock YT returns `None` → LRCLIB called; mock both `None` → `bail!`
- `is_ready` probe constant still references all three required imports
- `bootstrap` pins matched torch triplet (static audit against the `args` constant)
- DB migration V9 test: seed rows with various sources, apply V9, assert all → `(0, NULL)`

### E2E test (post-deploy Playwright against win-resolume)

New test in `e2e/post-deploy-flac.spec.ts`:

**"song #148 Planetshakers 'Get This Party Started' has real word-level alignment"**

Polls up to 25 min (handles bootstrap model-download + retry cycles). Hard assertions, all must pass:

1. `track.source === "yt_subs+qwen3"` — proves new pipeline ran, not LRCLIB fallback
2. `track.lines.length >= 25` — song has 27 SRT events (observed on win-resolume)
3. Every line has `words` array populated
4. Total word count `>= 200` — song has 214
5. weighted `duplicate_start_pct < 10%` — live value measured 6.32 %
6. `>= 10 lines` have inter-word gap `stddev >= 50 ms` — rejects synthetic even-spread masking

Failing any one assertion makes the CI job fail and the PR unmergeable.

The existing generic E2E check (`at least one lyrics JSON has word-level timestamps`) stays as a weaker floor covering non-#148 songs.

### Cross-CI deletion audit (test-integrity workflow)

A new static check greps the repo for references to retired symbols. Any hit fails CI with `"legacy code leaked back in"`:

- Function names: `align_lyrics`, `merge_word_timings`, `ensure_progressive_words`, `count_duplicate_start_ms`, `retry_missing_alignment`, `set_video_lyrics_source`, `get_next_video_missing_alignment`
- String literals: `"lrclib+qwen3"`
- CLI args: `--write-auto-subs`
- Python function: `def cmd_align(` (the old whole-song entry point; `cmd_align_chunks` is OK)
- Python function: `def cmd_transcribe(`, `def cmd_download_models(`, `def _group_words_into_lines(`

## Risks and rollback

| Risk | Mitigation |
|---|---|
| yt-dlp or YouTube changes the sub-download API | `fetch_subtitles` returns `None` on any error; song falls through to LRCLIB line-level. |
| Mel-Roformer / anvuew model download fails on first deploy | `cmd_preload` surfaces at bootstrap; `is_ready` blocks worker start until all three models loaded. |
| Subprocess timeout | 300 s ceiling retained; one song at a time. |
| Qwen3 produces degenerate output on some SRT blocks in non-#148 songs | Logged via `warn!` with song id; worker continues. Per-song quality reports are a future PR, not a blocker. |
| Torch vs torchvision ABI mismatch | Matched triplet pin enforced by bootstrap (`torch==2.6.0+cu124 torchvision==0.21.0+cu124 torchaudio==2.6.0+cu124`). |

**Rollback path:** revert the PR merge commit on `main`. V9/V10/V11 migrations already ran but only blanked `has_lyrics`/`lyrics_source` — reverting code re-runs the old worker against blanked rows, which repopulates safely through old code paths. No destructive data loss.

## Completeness gate (merge blockers)

Before the PR is mergeable on `dev`:

- `cargo test --workspace` all green
- `cargo fmt --all --check` clean
- `cargo clippy -- -D warnings` clean
- Deletion audit grep (above) returns zero hits
- `trunk build --release` produces clean WASM
- CI job `Deploy to win-resolume` succeeds
- CI job `E2E Tests (win-resolume)` succeeds **including new #148 test**
- Post-deploy manual inspection at http://10.77.9.201:8920/ shows real karaoke highlighting on #148

Version: `VERSION` → `0.16.0-dev.1` at PR start (per airuleset).

## Out of scope (future PRs)

- Per-song quality report endpoint and dashboard badge
- De-reverb model swap for non-YT-sub songs (they stay line-level in this PR)
- Expanding YT-subs coverage via alternate video uploads (lyrics-video versions with better captions)
- Public-DB word-level lyrics integration (all providers probed either returned 401 or produced no word-level hits for this catalog)
- Swap aligner to WhisperX / MMS-FA (Qwen3-0.6B is sufficient for chunked mode on YT-sub songs per empirical measurement)
