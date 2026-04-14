# Qwen3-ForcedAligner Word-Level Lyrics Alignment Design

**Issue:** [#25 — Re-enable Qwen3-ForcedAligner word-level lyrics alignment](https://github.com/zbynekdrlik/songplayer/issues/25)

**Related:** [#16 — Karaoke text display (parent feature)](https://github.com/zbynekdrlik/songplayer/issues/16); PR #24 (initial karaoke implementation, line-level only).

## Goal

Produce real word-level timestamps for lyrics by running the official Qwen3-ForcedAligner-0.6B model, so the karaoke dashboard can highlight word-by-word instead of line-by-line. Scope of this cycle: the 27 LRCLIB-covered songs currently in the catalog. Coverage improvements are a separate issue.

## Why the previous attempt was blocked

PR #24 shipped Qwen3 subprocess wrappers and a Python helper, but the pipeline was gated behind `if false {}` because loading failed with:

```
KeyError: 'qwen3_asr'
ValueError: The checkpoint you are trying to load has model type `qwen3_asr`
but Transformers does not recognize this architecture.
```

Diagnosis on win-resolume confirmed:

1. **Architecture error is real and affects both models.** The ForcedAligner checkpoint declares `model_type: qwen3_asr` with architecture `Qwen3ASRForConditionalGeneration` — same as Qwen3-ASR. Transformers 5.5.3 has no entry for `qwen3_asr` in its `CONFIG_MAPPING`.
2. **`trust_remote_code=True` cannot fix it.** The HF repo ships no `modeling_*.py` — there is no custom code for transformers to trust.
3. **Prior model download was incomplete.** Only tokenizer/config files (~4.5MB) were cached; no `.safetensors` weights. `snapshot_download` skipped the big files for reasons we did not diagnose.
4. **The fix is the `qwen-asr` PyPI package.** The Qwen team ships a dedicated Python package (`pip install -U qwen-asr`) that provides `Qwen3ForcedAligner` with its own loading code. It pins `transformers==4.57.6`. This is the official path — `qwen-asr` is what the HF model card tells users to install.

## Architecture

No Rust-side structural changes. The `aligner.rs` subprocess wrapper and `worker.rs` pipeline from PR #24 already have the shape we need. Work is:

- New Python venv bootstrap (isolated from system Python)
- Rewrite of the alignment command inside `scripts/lyrics_worker.py` to use `qwen_asr.Qwen3ForcedAligner`
- Flip the `if false {}` gate to `if true {}`
- Deploy verification on one song before full rollout

### Python environment isolation

`qwen-asr` forces `transformers==4.57.6` and pulls in ~15 heavy deps (gradio 6, pandas 3, accelerate, dyNET38, soynlp, etc.). We will not let it touch system Python.

- New location: `{tools_dir}/lyrics_venv/`
- Bootstrap: on startup, `LyricsWorker` runs `python -m venv lyrics_venv` if missing, then `lyrics_venv\Scripts\python.exe -m pip install -U qwen-asr`. Idempotent.
- `LyricsWorker::python_path` points to `{tools_dir}/lyrics_venv/Scripts/python.exe` (Windows) — the system `python` path becomes irrelevant to alignment.
- On Linux (CI / local dev), alignment path is dead (no venv, no aligner). `cfg(target_os = "windows")` gates the venv bootstrap. Cross-platform unit tests cover the parsing/conversion code, not the subprocess.

### Model weights

The HF model snapshot on win-resolume is incomplete (no `.safetensors`). On first call, `Qwen3ForcedAligner.from_pretrained("Qwen/Qwen3-ForcedAligner-0.6B")` with the standard HF cache will download the missing weights (~1.2 GB for bfloat16) automatically. We set `HF_HOME` / `TRANSFORMERS_CACHE` to `{tools_dir}/hf_models/` so the download lands in the existing cache directory and is visible to future runs.

### Data flow

```
process_song(video_row):
    track = acquire_lyrics(row)                 # LRCLIB: line-level timestamps
    if windows and venv_ready and audio <= 5 min:
        aligned = aligner::align_lyrics(...)    # subprocess → Python qwen_asr
        if aligned succeeded:
            track.lines = merge_word_timings(track.lines, aligned)
            source = "lrclib+qwen3"
        else:
            source = "lrclib"                   # alignment failure is non-fatal
    else:
        source = "lrclib"                       # too long or no venv
    gemini_translate(track)
    persist_json(track)
    mark_video_lyrics(source)
```

### Long-song handling

Qwen3-ForcedAligner has a hard 5-minute architectural limit (`max_source_positions: 1500` in the audio encoder — ~5 min of mel frames, no sliding-window support in the model or the `qwen-asr` package). For this cycle: songs > 5 min skip alignment and keep LRCLIB's line-level timestamps. Log a one-line info message so we can count affected songs. Chunking is deferred to a follow-up issue; we will decide based on actual numbers after deploy.

### Source label

DB `lyrics_source` uses `"lrclib+qwen3"` when alignment ran and `"lrclib"` when it didn't (for any reason: long song, aligner failure, venv missing). The explicit `qwen3` suffix is so we can tell aligned songs from line-only songs in the DB when we add other aligners or sources later.

### Python script rewrite

`scripts/lyrics_worker.py` currently has a complex CTC / torchaudio `forced_align` fallback stack from PR #24. Delete it. Replace the `align` command body with:

```python
def cmd_align(args):
    import torch
    from qwen_asr import Qwen3ForcedAligner

    with open(args.text, "r", encoding="utf-8") as f:
        lyrics_lines = [l.strip() for l in f.read().splitlines() if l.strip()]

    model = Qwen3ForcedAligner.from_pretrained(
        "Qwen/Qwen3-ForcedAligner-0.6B",
        dtype=torch.bfloat16,
        device_map="cuda:0",
    )

    # qwen-asr expects the text as a single string joined by spaces/newlines;
    # the aligner returns tokenized word alignments.
    full_text = "\n".join(lyrics_lines)

    results = model.align(
        audio=args.audio,
        text=full_text,
        language="English",
    )

    # results[0] → list of WordTimestamp(text, start_time, end_time) in seconds
    word_timestamps = results[0]

    # Group aligned words back into source lines
    lines_out = _group_words_into_lines(word_timestamps, lyrics_lines)

    with open(args.output, "w", encoding="utf-8") as f:
        json.dump({"lines": lines_out}, f, ensure_ascii=False)
```

`_group_words_into_lines` is the only non-trivial helper — it walks the aligned word stream and the source line list in parallel, assigning each word to its line by matching text and counting words per expected line. This matches the helper already in the current script; we keep that logic but drop the torchaudio / CTC / evenly-distributed-fallback code.

Non-goals for the Python rewrite:
- No CTC fallback. If `qwen_asr` fails to load or align, the subprocess exits non-zero; Rust logs it and the song keeps line-level timing.
- No "evenly distributed" fake alignment. Previous cycle had this as a last resort; it produced fake numbers that looked real. Remove it — honest failure is better.
- No ASR transcription path in this cycle. `transcribe` subcommand stays for future use but is not invoked.

### Error handling (Rust)

- Venv bootstrap fails → log warn, set `self.python_path = None` so alignment path no-ops. Lyrics still work line-level.
- `aligner::align_lyrics` returns Err → log warn, keep `source = "lrclib"`, persist without words. Not fatal; next song continues.
- 120s subprocess timeout (already in PR #24 code) → kill child, skip song.
- Audio file missing → skip alignment, keep line-level.

### State kept in DB

No schema changes. The existing `lyrics_source` text column carries the label; `has_lyrics` stays the same. JSON file on disk (`{youtube_id}_lyrics.json`) already has the optional `words` field per line from PR #24. We start populating it now.

## Testing

### Rust unit tests (run on Linux CI)

- Extend `aligner.rs` tests with a case where `words` in the JSON is empty — `convert_align_output` must return a `LyricsLine` with `words: None` and zero line timestamps (already covered by one existing test; add one more for line-level-only fallback path).
- New helper: `merge_word_timings(lrclib_lines, aligned_lines)` — matches aligned words to LRCLIB lines by order, preserves LRCLIB's line `start_ms`/`end_ms` if they exist, sets `line.words = Some(word_vec)`. Unit-test with four shapes: same line count; fewer aligned lines than LRCLIB; more aligned lines; empty aligned. All pure-function, no subprocess, no mocks.

### Windows deploy verification (post-deploy, manual)

After the CI deploy lands on win-resolume:

1. Trigger alignment on one known song (`Touch of Heaven`, ~4 min, has LRCLIB). Expected: DB row `lyrics_source = "lrclib+qwen3"`, JSON file has `words: [...]` with plausible `start_ms`/`end_ms` per line.
2. Play the song via the dashboard; confirm word-level highlight advances in sync with the audio within ~100 ms.
3. Pick one long song (>5 min) — expected: `lyrics_source = "lrclib"` (unchanged), JSON has no `words`, dashboard falls back to line highlight.

### Playwright E2E (committed, runs every CI cycle)

Extend `e2e/post-deploy-flac.spec.ts` with one new test: during playback of an aligned song, assert the karaoke panel shows a `.word.active` element (or equivalent class) at least once during the first 30 seconds of playback, AND that the active word changes at least 3 times in that window. This fails if word-level highlighting regresses to line-level.

## Rollout

1. Land spec + plan.
2. Implement on `dev`: Rust changes + Python rewrite + venv bootstrap. Keep `if false` gate during code review; flip it to `if true` in the final commit.
3. CI green. Deploy to win-resolume.
4. Manual verification on `Touch of Heaven`.
5. If OK: let the worker re-process the 27 songs one at a time (5s throttle between songs, built in).
6. If fail: revert the `if false` flip only, investigate, retry.

## Out of scope (explicitly)

- Expanding lyrics coverage beyond 27/230 (Genius, Gemini LLM sources). Separate issue.
- Audio chunking for songs > 5 min. Separate issue, opened only if needed.
- Retry of Qwen3-ASR (transcription) for songs where LRCLIB has no lyrics. Same architecture blocker — will need the same `qwen-asr` package rewrite. Separate issue.
- WhisperX / torchaudio MMS_FA alternatives. Rejected in favor of sticking with Qwen3 (user decision).

## Risks

- **First-run model download** might fail or be slow (~1.2 GB). If it fails silently, alignment will throw on every song. Mitigation: after bootstrap succeeds, run a one-shot `python -c "from qwen_asr import Qwen3ForcedAligner; Qwen3ForcedAligner.from_pretrained(...)"` to force the download on setup, not on first real song. Log success.
- **VRAM on RTX 3070 Ti (8 GB).** bfloat16 Qwen3-ForcedAligner-0.6B is ~1.2 GB. Plenty of headroom. Monitor in deploy verification.
- **`qwen-asr` API surface is small and not fully documented.** If `model.align(...)` has different argument shapes than our script expects, the subprocess will error on first run — we catch that in Windows verification, not in CI (CI doesn't run Python alignment).
