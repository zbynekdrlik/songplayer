---
paths:
  - "eval/dubbing/**"
---

# Sermon dubbing eval harness (#175, epic #174) — engine gotchas + run recipe

The `eval/dubbing/` harness compares TTS engines for the Slovak sermon-dub lane.
`fit.py` is PURE (CI runs only `tests/test_fit.py`); engines/mix/runner run at
eval time on dev1 + dev2. Every hard-won fact below was verified live 2026-09-18.

## Soniox TTS v2 (`tts-rt-v2`) voice cloning — REST surface (verified)

Two DIFFERENT base URLs, both `Authorization: Bearer <SONIOX_API_KEY>`:

- **Voices** live at `https://api.soniox.com/v1` (NOT the tts host):
  `POST /voices` multipart (form field `name`, file field `file`) →
  `{id, name, models:[{model, status}]}`; if the `tts-rt-v2` model entry is
  `not_computed`, `POST /voices/{id}/recompute {"model":"tts-rt-v2"}`; poll
  `GET /voices/{id}` until that model entry `status=="ready"` (states:
  `not_computed|processing|ready|failed`); `DELETE /voices/{id}` to clean up
  (org cap = 20 voices).
- **Synthesis** at `https://tts-rt.soniox.com/tts`:
  `POST {model, language:"sk", voice:<id>, audio_format:"wav", sample_rate:24000,
  text}` → raw WAV bytes. Cloning ~5 s, synth ~0.77× realtime.
- Key ONLY from `SONIOX_API_KEY` env (currently in `/home/newlevel/devel/voiceagent/.env`),
  never on a command line / in a log / committed. Read it inside a Python runner,
  not via inline `$()` (the worktree guard blocks `KEY=$(...) python ...`).

## Chatterbox Multilingual (Resemble AI, MIT) on dev2 — three traps

Run ONLY on dev2 (RTX 5050, Blackwell **sm_120**), never win-resolume. Use a
CLEAN venv (`python3 -m venv`, NOT `--system-site-packages` — that mixed system
torch 2.10 with chatterbox's pinned deps and broke transformers/`LlamaModel`).

1. **No Slovak.** The installed package's `chatterbox.mtl_tts.SUPPORTED_LANGUAGES`
   is **23 langs and `sk` is NOT one** (`generate(language_id="sk")` raises
   `ValueError`). Resemble's "v3 / 25 langs incl. Slovak" is a hosted model, not
   the open weights. For a by-ear voice test, render Slovak text through the
   closest Slavic language, `pl` (Polish) — label it a proxy.
2. **Blackwell needs cu128.** chatterbox-tts pins **torch 2.6.0+cu124**, which has
   NO CUDA kernels for sm_120 → `RuntimeError: CUDA error: no kernel image is
   available for execution on the device`. Fix:
   `pip install torch==2.8.0 torchaudio==2.8.0 --index-url https://download.pytorch.org/whl/cu128`.
3. **`perth` watermarker needs `pkg_resources`.** A clean 3.12 venv ships
   setuptools ≥ 84 which REMOVED `pkg_resources`, so `perth.PerthImplicitWatermarker`
   silently becomes `None` → `TypeError: 'NoneType' object is not callable` in
   `from_pretrained`. Fix: `pip install "setuptools<81"`.
4. **HF Xet stalls from dev2** (adaptive concurrency collapses to 1, resets
   partial blobs). Set `HF_HUB_DISABLE_XET=1` to force classic HTTP (~8 MB/s,
   steady). `hf_transfer` is deprecated (→ Xet) and does not help.

## Producing the sample inputs (hand-run, never committed — `~/.claude/work-products/songplayer/dubbing-test/`)

- **yt-dlp needs BOTH cookies AND a JS runtime.** The bot-check needs
  `--cookies C:\ProgramData\SongPlayer\cookies.txt` (on win-resolume; run yt-dlp
  ON the box so the cookie never leaves it), AND the newer YouTube **n-challenge**
  needs Deno (`yt-dlp` EJS solver) — without it every client fails
  `n challenge solving failed` / `The page needs to be reloaded`. Drop a
  `deno.exe` on PATH for the yt-dlp process. (This means SongPlayer's OWN box
  downloads are also n-challenge-broken until the box gets a JS runtime.)
- Pull large files OFF win-resolume via a temp `python -m http.server` +
  `curl` from dev1 (NOT `FileDownload` — it base64s into the transcript).
- Transcript: `eval/lyrics/backends/gemini_3_5_transcribe.py` (`GEMINI_API_KEY`
  from `GET http://10.77.9.201:8920/api/v1/settings`, csv list, first entry
  works). Translation: POST the `translator.rs::build_prompt` text to the box's
  CLIProxy `http://127.0.0.1:18787/v1/chat/completions` (localhost-only — run the
  curl ON the box), model `claude-fable-5-1`.
- Stems: `scripts/stem_worker.py separate` on dev2 (needs `audio-separator[gpu]`
  + `librosa` + `audioread`); Kim model ~913 MB downloads on first run.

## Round 2 (native-Slovak + intensity, verified live 2026-09-18)

Round 1 was rejected by ear ("slovenčina bez mäkčeňov"): a cross-lingual clone of
an ENGLISH speaker leaks the source accent. Round 2 = native voices + audio-to-audio,
same 7 sentences (`seg_spec` items 2..8), plus an intensity layer.

### Cloud reference engines
- **Gemini TTS** (`engines/gemini_tts.py`): `generateContent` AUDIO modality,
  `speechConfig.voiceConfig.prebuiltVoiceConfig.voiceName`; output `audio/L16`
  24 kHz PCM → `wavutil.pcm_l16_to_wav`. Model `gemini-3.1-flash-tts-preview`
  (owner dropped 2.5-pro as superseded). Native `sk`. **Tight free-tier quota**:
  a single key 429s after ~1 candidate; ROTATE the 5 `gemini_api_key` entries
  per-call (not per-engine — the engine reads the env once). 30 prebuilt voices.
  `finishReason=OTHER` with no audio happens intermittently → retry. Price
  $20/1M audio-out tokens.
- **Soniox stock** (`engines/soniox.py` `SonioxStockEngine` + `stock_voices_for_language`):
  the 403 is **Cloudflare `error code: 1010`** (banned browser signature on the
  default UA), NOT a rate limit — set `User-Agent: songplayer-dubbing-eval/1.0`
  (`COMMON_HEADERS`) on every request; no pacing needed. `GET /v1/tts-models`
  lists 200+ cross-lingual stock voices (Adrian, Emma, …) all speaking `sk`.
  Reference only — prod would need the owner's own key. ~$0.70/h.

### Open-weight engines (dev2 GPU / CPU — the owner's priority)
- **Felagund/XTTSv2-sk** (`engines/xtts_sk.py`, MIT/coqui): genuine SK fine-tune
  (config lists `sk`, vocab has `[sk]`) but coqui's `VoiceBpeTokenizer.preprocess_text`
  hard-raises for langs outside its 17-lang whitelist → route `sk` text-prep
  through `cs` (`_patch_coqui_env`); `encode` still emits `[sk]`. transformers ≥5
  removed `isin_mps_friendly` (tortoise import) → shim it (kwargs `elements,
  test_elements`). transformers 5.x + coqui-tts 0.27.5 co-exist with those two
  patches. ~2 GB VRAM, ~2.6 s/sentence, zero-shot clone. Card warns of artifacts.
- **vsisik/speecht5_tts_SK** (MIT): SpeechT5 SK fine-tune; use the
  `microsoft/speecht5_tts` processor (+`sentencepiece`) + `speecht5_hifigan`
  vocoder + a 512-d x-vector (cmu-arctic dataset is script-based/unsupported now
  → use a deterministic generic embedding; no cloning). 0.73 GB, very fast, 16 kHz.
- **Piper sk_SK-lili-medium** (rhasspy, MIT): onnx, `piper-tts` `PiperVoice.load`
  → `synthesize_wav`; native SK, CPU-only, ~0.2 s/sentence, 22 kHz. No cloning.
  The cheapest local option by far.
- **facebook/mms-tts-slk DOES NOT EXIST** — `mms-tts-pol` is 200 but `slk`/`ces`
  are 401/not-found; MMS-TTS has no Slovak model.
- HF is rate-limited for unauthenticated metadata calls (401) — set `HF_HUB_DISABLE_XET=1`
  and expect occasional 401 bursts; snapshot_download still works unauthenticated.
- More verified SK open-weights NOT yet rendered: `k2-fsa/OmniVoice`,
  `pekiskol/chatterbox-tts-slovak`, `petercheben/F5_TTS_Slovak`,
  `fishaudio/s2-pro`, `bosonai/higgs-*` (see `round2/hf_discovery.json`).

### Audio-to-audio (prosody-preserving) — the round-2 top candidate
- **Gemini Live Translate** (`engines/gemini_live_translate.py`,
  `gemini-3.5-live-translate-preview`): stream EN audio (16 kHz mono s16le, 100 ms
  chunks) into the Live API with `translation_config.target_language_code="sk"`,
  `response_modalities=["AUDIO"]`; output 24 kHz PCM. WORKS and keeps the
  speaker's own pacing/boundaries/continuity (per-sentence TTS cannot). The
  session streams silence after speech until closed → bound the drain
  (`drain_deadline_s`) and trim trailing silence (`trim_af`). ~1.2× real-time
  incl. drain. Price ~$0.037/min (in $0.0053 + out $0.0315). `gemini-3.8-live`
  with a translate *instruction* stops after ~1.4 s (turn-taking) — unusable for
  continuous dubbing; use the dedicated `-live-translate-` model.
- **Voice pinning WORKS on the translate model (#184 round C, verified 2026-09-21).**
  `gemini-3.5-live-translate-preview` ACCEPTS `speech_config=SpeechConfig(
  voice_config=VoiceConfig(prebuilt_voice_config=PrebuiltVoiceConfig(
  voice_name="Charon")))` ALONGSIDE `translation_config` — the same field
  `gemini_tts.py:85-86` uses for `generateContent`. Probe: one 20 s EN slice
  (`seg.wav [15,35)` → 16 kHz mono s16le PCM) streamed twice with the pin →
  both runs accepted, f0 median (librosa pyin voiced frames) 99.0 Hz vs 107.3 Hz
  = **1.4 semitones spread**, i.e. the SAME voice (male, consistent with Charon).
  So the prod dub child pins one voice per video via `speech_config`
  (`scripts/dub_worker.py`, `.claude/rules/dabing.md` round C); the "one Live
  session per video, no pin" fallback was NOT needed. Reuse the eval venv
  (`~/.claude/work-products/songplayer/dubbing-test/.venv-live`, has
  `google-genai==2.24.0`; add `librosa soundfile numpy` for the f0 read); the key
  is read INSIDE Python from `GET http://10.77.9.201:8920/api/v1/settings`
  `gemini_api_key` (csv, first entry), never on a command line / log / commit.
- **Session-length drift (#184 round E, verified 2026-09-21).** The pin HOLDS at a
  session START but the model DRIFTS inside a LONG session. Experiment on the same
  120 s EN slice (`seg.wav`, key read inside python from the box settings): ONE
  pinned 120 s session → 0 windows > 150 Hz, 0 band flips (5-s scan, median 93 Hz);
  the SAME slice as four 30 s pinned sessions → 3 windows (11 %), 3 flips — so
  shorter is NOT automatically better, ~2 min is the validated point and ~7 min is
  where the drift showed on video 344. Script:
  `scratchpad/voice_session_experiment.py` (reuses the eval venv
  `.venv-live` + `dub_voice_check.window_medians`). Prod fix: cap the Live session
  at `dub_session_max_s` (default 120 s) + a per-chunk voice-band guard in
  `dub_worker.py` (`.claude/rules/dabing.md` round E).
- **Voice-band measurement (#184 round E2, 2026-09-22).** `eval/dubbing/
  voice_band_measure.py` renders `seg.wav` through one pinned Live session per
  catalogue voice and prints the voiced 5-s f0 band. Two traps: (1) run it as a
  MODULE from the repo root (`python -m eval.dubbing.voice_band_measure`) — a
  direct `python eval/dubbing/voice_band_measure.py` dies on `from eval.dubbing.
  voices import` (only `eval/dubbing/` lands on `sys.path`). (2) Measure with
  `use_librosa=False` (autocorrelation) to match the RUNTIME seed median
  (`dub_worker._*_window_medians` force it); pyin bands would be a different unit
  and mis-seed. Autocorrelation octave-collapses all six voices to ~87–94 Hz, so
  `VOICE_F0_BAND` is a coarse seed gate, not a voice discriminator. Local verify:
  the eval venv `.venv-live` carries **ruff 0.5.7** (== the `ci.yml` eval-checks
  pin) + numpy/soundfile/pytest, so `.venv-live/bin/ruff` and `-m pytest scripts/
  tests` reproduce the CI eval-checks gate exactly without a push. `sync-version.
  sh` bumps the Cargo.toml versions but NOT `Cargo.lock` (already lags, e.g. 0.49
  vs 0.64) — harmless, the workspace build is not `--locked`.
- **SeamlessM4T v2 / Seamless Expressive**: NO Slovak SPEECH output (v2 = `slk`
  speech input + text output only, 35 speech-output langs exclude it; Expressive =
  en↔fr/de/it/zh/es). Reason rows, no GPU spent.

### Intensity + register layer
- `intensity.py` (pure mapping + librosa `measure_window`): per-sentence RMS dB,
  f0 spread (semitones), words/s from the ORIGINAL voice window → `intense/neutral/
  calm` label → Gemini per-sentence style instruction + Soniox `[emphatic]`/`[calm]`
  audio tag. XTTS/Piper/SpeechT5 have no style control (noted).
- Sermon-register SK translation: CLIProxy is localhost-only on win-resolume and
  ssh to that box is banned in this lane → use the Gemini text API
  (`gemini-2.5-flash` generateContent) for the register translation instead.
- `run_round2.py`: owner-approved mixes only — `dub only` + `dub + original −18 dB`
  (NO same-colour blend), loudnorm −16; windowed 7-sentence or full-34.
