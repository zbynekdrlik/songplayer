# Vocal Isolation + Forced Alignment (Revised Design)

**Supersedes:** `2026-04-14-qwen3-forced-aligner-design.md` (vocal-isolation step missing)

**Issue:** Continuation of [#25](https://github.com/zbynekdrlik/songplayer/issues/25) and informs [#27](https://github.com/zbynekdrlik/songplayer/issues/27)

## Why this revision

The original design fed raw mixed-music audio (vocals + drums + bass + keys) directly to Qwen3-ForcedAligner. That's broken by design — speech models can't reliably localize phoneme boundaries when masked by background instruments. Result: 175/214 words on a real worship song collapsed to duplicate timestamps (degenerate alignment). My post-processor synthesized fake even-spacing — visible to the user as "word colors changing at wrong times".

Standard practice for forced alignment on sung music (WhisperX, every serious lyrics pipeline) is to **isolate the vocal stem first**. Quantitative impact from MUSDB-ALT benchmarks: WER drops from 23.59% (raw mix) to 14.19% (clean vocals) — a 40% relative improvement. Timestamp accuracy gains are even larger because the model can now hear phoneme boundaries.

## Revised pipeline

```
song_audio.flac (typically 48 kHz stereo)
   ↓
[Mel-Roformer vocal isolation]   ← NEW: preprocessing step
   ↓
vocal_stem (vocals only, native rate)
   ↓
[Resample to 16 kHz mono float32]   ← NEW: explicit, not auto
   ↓
vocal_16k_mono.wav
   ↓
[Qwen3-ForcedAligner-0.6B]       ← unchanged from prior design
   ↓
word-level timestamps (real this time)
   ↓
[merge_word_timings + ensure_progressive_words]   ← safety net, rarely fires now
   ↓
{song}_lyrics.json
```

**Sample-rate decision:** Qwen3's docstring says "All audios will be converted into mono 16k float32 arrays in [-1, 1]." The library auto-resamples, but we resample explicitly for three reasons: (1) we control the mono-conversion strategy (avoid silent failures on hard-panned vocals), (2) smaller intermediate file → faster subprocess I/O, (3) eliminates dependency on `normalize_audios()` internal behavior across qwen-asr versions.

## Vocal isolation: Mel-Roformer

**Why Mel-Roformer specifically (2026 SOTA):**

| Model | Vocal SDR (Multisong) | Notes |
|---|---|---|
| **Mel-Roformer** | **~11.89 dB** | Current SOTA, ByteDance/lucidrains, Mel-band variant of BS-Roformer |
| BS-Roformer | ~11.31 dB | Direct predecessor, still excellent |
| HTDemucs (Demucs v4) | ~9.00 dB | Meta's, what WhisperX bundles, 2-3 dB worse |
| MDX-Net mdx_extra | ~8.50 dB | Older, what most ASR pipelines used until 2024 |

**Tooling:** [`python-audio-separator`](https://github.com/nomadkaraoke/python-audio-separator) — single pip package with Mel-Roformer presets built in. Active maintenance, MIT license, used in production karaoke services.

Specific model preset: `model_bs_roformer_ep_317_sdr_12.9755.ckpt` (currently top vocal SDR on MVSEP leaderboard) or `mel_band_roformer_vocals_fv4_gabox.ckpt`.

## VRAM management on RTX 3070 Ti (8 GB)

Mel-Roformer (~6-8 GB) and Qwen3-ForcedAligner-0.6B (~1.2 GB) **cannot both be loaded at the same time** on 8 GB. Pipeline runs sequentially per song:

1. Load Mel-Roformer
2. Isolate vocals → write temp `.wav`
3. **Unload Mel-Roformer** (`del model; torch.cuda.empty_cache()`)
4. Load Qwen3-ForcedAligner
5. Align text to temp vocal `.wav`
6. **Unload Qwen3** (between songs)
7. Delete temp `.wav`

Per-song cost: ~30s isolation + ~30s alignment + ~30s combined load/unload overhead ≈ 90s per song. For 27 songs: ~40 min total wall-clock. Acceptable for one-shot retroactive processing, then per-new-song going forward.

**Alternative:** Two-pass batch — isolate ALL 27 songs first (load Mel-Roformer once), then align ALL 27 (load Qwen3 once). Saves ~25 min of model-load overhead. Recommended for the one-shot retroactive run.

## File-system layout

Vocal stems are temporary by default (deleted after alignment). Optionally cache them under `cache/vocals/{youtube_id}.wav` if the user wants to keep them for re-alignment without re-isolating (saves ~30s per song on retry). Decision: **do not cache** — disk is precious, vocals are only useful for alignment.

## Implementation plan

### Phase 1: Add Mel-Roformer to the venv

**Files:**
- Modify `crates/sp-server/src/lyrics/bootstrap.rs`: add a third pip install step for `audio-separator[gpu]`
- Modify `scripts/lyrics_worker.py`: add `cmd_isolate_vocals` subcommand for direct testing

`bootstrap.rs` adds:
```rust
// 2c. Install audio-separator (Mel-Roformer + others) for vocal isolation.
//     ~3 GB of weights download on first run via the model preset cache.
let mut sep_pip = Command::new(&venv_python);
sep_pip.args(["-m", "pip", "install", "-U", "audio-separator[gpu]"]);
// ... same spawn/timeout/kill pattern as existing pip steps
```

`is_ready` extended:
```python
import qwen_asr, torch, audio_separator
sys.exit(0 if torch.cuda.is_available() else 1)
```

**Tests:** is_ready returns false if audio-separator missing (1 new test, mocked via PATH).

### Phase 2: Vocal isolation in Python

**Files:** `scripts/lyrics_worker.py`

New helper:
```python
def _isolate_vocals(audio_path: str, models_dir: str) -> str:
    """Run Mel-Roformer to extract vocal stem, then resample to 16 kHz mono.
    Returns path to a 16k mono float32 WAV ready for Qwen3."""
    from audio_separator.separator import Separator
    import soundfile as sf
    import librosa
    import tempfile, os

    sep = Separator(model_file_dir=models_dir, output_format="WAV")
    sep.load_model("model_bs_roformer_ep_317_sdr_12.9755.ckpt")
    out = sep.separate(audio_path)
    vocal_path = [p for p in out if "Vocals" in p][0]

    # Resample to exactly 16 kHz mono float32 — Qwen3's expected input.
    # We do this explicitly to avoid relying on qwen_asr's internal
    # normalize_audios() behavior, and to avoid losing energy on
    # hard-panned vocals from a naive L+R average.
    audio, _ = librosa.load(vocal_path, sr=16000, mono=True)
    fd, resampled_path = tempfile.mkstemp(suffix=".wav")
    os.close(fd)
    sf.write(resampled_path, audio, 16000, subtype="FLOAT")
    os.remove(vocal_path)  # free disk; only need the resampled version
    return resampled_path
```

Modify `cmd_align`:
```python
def cmd_align(args):
    # ... read lyrics text ...
    vocal_path = _isolate_vocals(args.audio, args.models_dir)
    try:
        # ... existing Qwen3ForcedAligner code with vocal_path instead of args.audio ...
    finally:
        os.remove(vocal_path)  # cleanup temp
```

New CLI subcommand `isolate-vocals` for diagnostic use:
```python
def cmd_isolate_vocals(args):
    out = _isolate_vocals(args.audio, args.models_dir)
    print(json.dumps({"vocal_path": out}))
```

**Tests:** Python script syntax check (Linux); manual test via `cmd_isolate_vocals` on win-resolume after deploy.

### Phase 3: Adjust Rust subprocess timeout

**Files:** `crates/sp-server/src/lyrics/aligner.rs`

The existing 120-second timeout for `align_lyrics` is now insufficient — isolation + alignment together can take 90s+ on first run. Bump to 300 seconds (5 min) per song.

```rust
let timeout = std::time::Duration::from_secs(300);  // was 120
```

### Phase 4: Retroactive re-alignment

The existing `retry_missing_alignment` worker function already handles re-running alignment on songs labeled `lrclib+qwen3`. After deploy:

1. Reset all 14 already-aligned songs to `lyrics_source = 'lrclib'` so retry_missing_alignment picks them up again
2. Worker re-aligns each one with the new vocal-isolation pipeline
3. Updated JSON files have real per-word timestamps (no synthesis needed)

This is a one-shot DB UPDATE in startup self-heal, gated by a "new pipeline version" marker so it only runs once:

```rust
// Gate: only reset on first boot of the new vocal-isolation pipeline
const PIPELINE_VERSION: &str = "qwen3-with-mel-roformer-v1";
// Compare against settings table value; reset has_lyrics+source for all
// rows where lyrics_source LIKE 'lrclib+qwen3%' if marker mismatches.
```

### Phase 5: Strengthen E2E test (again)

The current E2E asserts strictly-increasing word `start_ms`. Even my synthetic post-processor passes that. Need a stricter quality check that catches degenerate input even after our band-aid:

**New assertion:** Of the words in a line, **inter-word gaps must vary by ≥30 ms standard deviation** (real singing has irregular timing; even distribution has zero variance). This catches both:
- Truly degenerate aligner output (all same time → after band-aid, perfect even spacing → variance = 0)
- Real aligner output (varied timing → variance > 0)

```typescript
const gaps = w.slice(1).map((ww, i) => ww.start_ms - w[i].start_ms);
const mean = gaps.reduce((a, b) => a + b, 0) / gaps.length;
const variance = gaps.map(g => (g - mean) ** 2).reduce((a, b) => a + b, 0) / gaps.length;
const stddev = Math.sqrt(variance);
if (stddev < 30) return false;  // looks like even distribution, not real alignment
```

### Phase 6: Update issue #27

If Mel-Roformer + Qwen3 produces clean per-word timestamps (verified on win-resolume), **close issue #27** with a comment that vocal isolation solved the problem and WhisperX swap is unnecessary. Otherwise keep #27 open as fallback plan.

## Test plan

**Unit (Linux CI):**
- Existing 25 aligner tests still pass (no algorithmic changes)
- New: `is_ready` validates `audio-separator` import
- New: bootstrap installs `audio-separator` package (mocked via test fixture)

**Manual deploy verification on win-resolume:**
1. After deploy, watch log for "vocal isolation" messages
2. Wait for retroactive re-alignment of one song (~2 min for first model load)
3. Inspect `{youtube_id}_lyrics.json` — ≥80% of lines should have word `start_ms` values that DON'T look like even-distribution (gaps vary, not all multiples of `line_duration / word_count`)
4. Play "Get This Party Started" — word colors should track actual singing within ~200 ms

**E2E (Playwright post-deploy):**
- Existing test extended with stddev variance check (Phase 5)
- Polls 18 min for at least one song with non-uniform word spacing

## Risks

- **First model load is slow** — Mel-Roformer weights ~3 GB download on first bootstrap. Mitigate via preload step (similar to existing Qwen3 preload).
- **VRAM exhaustion** — Sequential loading is required; concurrent model loads will OOM. Bootstrap preload tests this.
- **`audio-separator` package may have dep conflicts** with `qwen-asr` — both pin transformers/torch versions. If conflict, may need separate venvs or version pinning. Bootstrap will fail loudly if so.
- **Mel-Roformer separation can clip vocals** in heavily-mixed sections. Monitor in deploy verification — if alignment is still degenerate on some lines, we know separation isn't enough.

## Out of scope

- Caching vocal stems on disk (only use them for alignment, then delete)
- Other stems (drums, bass) — we only need vocals
- Real-time isolation during playback (offline batch only)
- Larger Qwen3 ASR models (still using 0.6B aligner per #27 fallback plan)
