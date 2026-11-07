# Dabing Dubbing — eval harness (#175, #184)

The eval side of the dabing-dubbing epic (#174). Production dubs through Gemini
Live Translate (`docs/ai-providers.md`, `.claude/rules/dabing.md`); this harness
holds the tools that measure that model before a production change:

## Layout

| Path | Purpose |
|------|---------|
| `engines/gemini_live_translate.py` | Gemini Live Translate audio-to-audio reference engine (16 kHz EN in, 24 kHz SK out) |
| `live_translate_continuous_probe.py` | ONE logical Live session over a whole talk (#184 round H step 1) — the eval tool for a new `dub_model` |
| `voice_band_measure.py` | Each catalogue voice's f0 band, the seed for `scripts/dub_worker.py::VOICE_F0_BAND` (#184 round E2) |
| `voices.py` | The vetted Gemini prebuilt voice catalogue (mirrored by the sp-ui settings) |
| `wavutil.py` | Stdlib-only PCM → WAV wrapper |
| `tests/` | Pure unit tests (CI runs them; no network, no audio libs beyond numpy) |

The per-sentence TTS engines (Soniox, Chatterbox, XTTS-sk, Gemini TTS) and their
listening-test runners were rejected in listening rounds 1–2 and are deleted
(#286); their verdicts are in `.claude/rules/dubbing-eval.md`.

## CI

The `eval-checks` job (`.github/workflows/ci.yml`) lints all of `eval/` with
`ruff==0.5.7` and runs `pytest eval/dubbing/tests`. The probe, the engine and
the voice-band measure run at eval time; their pure parts (pacing on a virtual
clock, the key read, the WAV wrapper) are what CI exercises. Verify locally with
the pinned toolchain (per `.claude/rules/lyrics-eval-backends.md`):

```bash
python3 -m venv /tmp/evalvenv
/tmp/evalvenv/bin/pip install -q ruff==0.5.7 pytest==8.3.2 numpy==2.1.1
/tmp/evalvenv/bin/ruff check eval/ && /tmp/evalvenv/bin/ruff format --check eval/
/tmp/evalvenv/bin/python -m pytest eval/dubbing/tests -q
```

## Secrets

- **Gemini** — `GEMINI_API_KEY` env only. Since #229 the box's settings
  endpoint `GET http://10.77.9.201:8920/api/v1/settings` shows
  `gemini_api_key` as `********`, so never read it there: on dev1 put the key
  in `GEMINI_API_KEY` through the secret channel (`python3
  ~/devel/airuleset/airuleset.py secret exec GEMINI_API_KEY -- <cmd>`); on the
  box read it read-only from SongPlayer's database inside Python
  (`.claude/rules/dubbing-eval.md`, "Reading the Gemini key"). The stored value
  is comma-separated; use an entry that works.

## The sample input (hand-run — never committed)

Work under `~/.claude/work-products/songplayer/dubbing-test/` (NOT the repo).
Sample: `https://www.youtube.com/watch?v=Dhp-qrZDK1g`. `seg.wav` is the
2-minute slice the voice-band measure and the voice-pinning probes read:

```bash
yt-dlp -f bestaudio -x --audio-format wav -o dabing.wav \
  "https://www.youtube.com/watch?v=Dhp-qrZDK1g"
ffmpeg -y -i dabing.wav -ss 00:02:00 -t 00:02:00 seg_raw.wav
ffmpeg -y -i seg_raw.wav -af loudnorm=I=-14:TP=-1.5:LRA=11 -ar 48000 -ac 2 seg.wav
```

The continuous probe reads a whole cached talk instead (its run recipe is in
`.claude/rules/dubbing-eval.md`).
