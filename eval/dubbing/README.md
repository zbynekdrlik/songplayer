# Sermon Dubbing — D0 Listening-Test Harness (#175)

A reusable evaluation harness for the sermon-dubbing epic (#174). It produces a
side-by-side Slovak dub of the SAME ~2-minute segment of the owner's sample
sermon with two candidate engines, delivered as share URLs so the owner can pick
by ear. **No engine is baked into the dubbing feature before this verdict.**

The winner's `engines/<x>.py` becomes the reference for the D4 Rust `DubEngine`
impl, so the harness mirrors the `eval/lyrics/` layout and keeps the engine
surface minimal.

## Layout

| Path | Purpose |
|------|---------|
| `engines/base.py` | `DubEngine` protocol (`clone_voice`, `synthesize`) + shared per-line synthesis driver + CLI wrapper |
| `engines/soniox.py` | Soniox TTS v2 (`tts-rt-v2`) voice cloning — cloud, HTTPS from dev1, no GPU |
| `engines/chatterbox.py` | Chatterbox Multilingual (Resemble AI, MIT) — dev2 GPU, zero-shot cloning |
| `fit.py` | **Pure** timing placement: fit / tempo (≤ +15 %) / overflow (to next start − 150 ms) / hard cut (50 ms fade) |
| `mix.py` | Three mix variants over the ambient stem: dub only; dub + original voice −12 dB; original only |
| `run_listening_test.py` | Assemble one engine's dub: measure durations → `fit` plan → ffmpeg `atempo` + cut → build dub track → write the three variants + a stats JSON |
| `tests/test_fit.py` | Pure unit tests for every `fit.py` branch (the only part CI runs) |

## CI

The `eval-checks` job (`.github/workflows/ci.yml`) lints all of `eval/` with
`ruff==0.5.7` and runs `pytest eval/dubbing/tests`. Only `fit.py` is exercised in
CI (pure, no audio deps); the engines / mix / runner run at eval time on dev1 +
dev2. Verify locally with the pinned toolchain (per
`.claude/rules/lyrics-eval-backends.md`):

```bash
python3 -m venv /tmp/evalvenv
/tmp/evalvenv/bin/pip install -q ruff==0.5.7 pytest==8.3.2
/tmp/evalvenv/bin/ruff check eval/ && /tmp/evalvenv/bin/ruff format --check eval/
/tmp/evalvenv/bin/python -m pytest eval/dubbing/tests -q
```

## Secrets

- **Soniox** — `SONIOX_API_KEY` from the environment only (never on a command
  line, in a log, in a committed file, or in the report). The key currently lives
  in `/home/newlevel/devel/voiceagent/.env`; read it into an env var in the same
  shell command and print only its length.
- **Gemini** (EN transcript) — `GEMINI_API_KEY` env; the box's settings endpoint
  `GET http://10.77.9.201:8920/api/v1/settings` returns `gemini_api_key`
  (comma-separated; use entry that works).

## Reproducing the run (hand-run inputs — never committed)

Work under `~/.claude/work-products/songplayer/dubbing-test/` (NOT the repo).
Sample: `https://www.youtube.com/watch?v=Dhp-qrZDK1g`.

1. **Download + cut + loudnorm** the 2-minute segment (02:00–04:00 or the first
   clear 2-minute speech span):

   ```bash
   yt-dlp -f bestaudio -x --audio-format wav -o sermon.wav \
     "https://www.youtube.com/watch?v=Dhp-qrZDK1g"
   ffmpeg -y -i sermon.wav -ss 00:02:00 -t 00:02:00 seg_raw.wav
   ffmpeg -y -i seg_raw.wav -af loudnorm=I=-14:TP=-1.5:LRA=11 -ar 48000 -ac 2 seg.wav
   ```

2. **Separate voice / ambient** with the repo's Kim stem worker (on dev2 GPU,
   `.claude/rules/karaoke-stems.md`):

   ```bash
   python scripts/stem_worker.py separate --audio seg.wav \
     --vocals-out seg_voice.flac --instrumental-out seg_ambient.flac \
     --models-dir <models> --work-dir <scratch>
   ```

   `seg_voice` is the voice stem (clone source + the −12 dB layer);
   `seg_ambient` is the ambient bed under every mix. Pick a clean 15–20 s span of
   `seg_voice` as the cloning sample (`ffmpeg -ss <t> -t 18 ... clone.wav`).

3. **EN transcript with sentence timestamps** via the Gemini 3.5 Transcribe route:

   ```bash
   GEMINI_API_KEY=... python eval/lyrics/backends/gemini_3_5_transcribe.py \
     --wav seg.wav --out transcript.json
   ```

   Group the words into sentences (start_ms/end_ms per sentence).

4. **SK translation** through CLIProxyAPI (`http://10.77.9.201:18787/v1`,
   model `claude-fable-5-1`), reusing the production translator prompt
   (`crates/sp-server/src/lyrics/translator.rs::build_prompt`): "Translate each of
   the following numbered lines into Slovak. Keep the exact same numbering and
   output exactly N numbered lines. …". Parse the numbered reply back per
   `parse_translation_response`. Save `seg_spec.json`:

   ```json
   { "segment_end_ms": 120000,
     "lines": [ {"index": 0, "start_ms": 0, "end_ms": 3200, "en": "...", "sk": "..."}, ... ] }
   ```

5. **Synthesize** each engine's lines (a `lines.json` of `{index, text}` with the
   SK text), then assemble + mix:

   ```bash
   # Soniox (dev1):
   export SONIOX_API_KEY=$(...)          # length only, never echoed
   python -m eval.dubbing.engines.soniox --sample clone.wav \
     --lines-json lines.json --out-dir soniox_lines
   python eval/dubbing/run_listening_test.py --engine-name soniox \
     --manifest soniox_lines/manifest.json --seg-spec seg_spec.json \
     --voice-stem seg_voice.flac --ambient-stem seg_ambient.flac --out-dir mixes

   # Chatterbox (dev2 venv ~/devel/dubbing-eval/, model on the RTX 5050):
   python -m eval.dubbing.engines.chatterbox --sample clone.wav \
     --lines-json lines.json --out-dir chatterbox_lines
   # then the same run_listening_test.py on dev1 with the copied-back manifest
   ```

6. **Deliver**: `python3 ~/devel/airuleset/airuleset.py share <mix>.mp3` for the
   six variants (2 engines × 3), verify each URL is 200, and post the comparison
   table on #175 (latency per minute, cost, timing-fit counts, naturalness /
   sk-vs-cs notes).
