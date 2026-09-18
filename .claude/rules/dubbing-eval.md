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

## Round 3 (remaining open-weight SK candidates, verified live 2026-09-18)

Round 3 renders the SK-capable open-weight models the round-2 discovery found but
did not render, so the owner's verdict rests on a COMPLETE open-weight table. The
dev2 synth driver is `ow_synth3.py` (mirrors `ow_synth.py`: measures load_s +
total_synth_s + max VRAM, writes `line_XXX.wav` + `manifest.json`); dev1 mixing is
`eval/dubbing/run_round3.py` (re-points the manifest via `localize_manifest`, then
reuses `run_round2.process` — DRY). Committed engine modules mirror each recipe.

### Reference clips — TRANSCRIBE, never assume
- `clone.wav` is NOT the span of seg_spec lines 2..8 — it is a SEPARATE ~18 s
  preacher span (its content: "A detestation… when you pray for the sick go to
  another level when you hate sickness in somebody's body…"). `native_sk_ref.wav`
  ≈ the SK of lines 2..8. F5/OmniVoice need the reference clip's TRANSCRIPTION
  (`ref_text`); Chatterbox/XTTS do not. Transcribe with faster-whisper `small` on
  CPU (dev1) — `en` for clone.wav, `sk` for native_sk_ref.wav; pass to `ow_synth3.py`
  via `REF_TEXT_FILE` (never a shell arg — Slovak diacritics + apostrophes).

### Rendered engines
- **k2-fsa/OmniVoice** (`engines/omnivoice.py`): `pip install omnivoice` + torch
  2.8.0+cu128. `OmniVoice.from_pretrained(id, device_map="cuda:0", dtype=fp16)`;
  `generate(text=, ref_audio=, ref_text=)` → list of np.ndarray @ 24 kHz. Qwen3-0.6B
  diffusion-LM, 600+ langs incl. `sk`, fits 8 GB. **LICENCE: model weights CC-BY-NC**
  (code Apache-2.0) — open-weight data point, NOT a commercial prod winner.
- **pekiskol/chatterbox-tts-slovak** (`engines/chatterbox_sk.py`): **MIT — code AND
  weights (commercial-OK)**, the only such open SK TTS. Reuses `cbvenv` (round-2
  chatterbox). Drop-in T3 swap: load base `ChatterboxMultilingualTTS`, hf_hub_download
  `t3_sk_v2.2.safetensors` (~2 GB), reconcile vocab rows (`plan_vocab_reconcile` —
  the SK fine-tune's `text_emb`/`text_head` vocab differs; trim or mean-pad),
  `t3.load_state_dict(strict)`, register `sk` in SUPPORTED_LANGUAGES (base 23-lang
  map omits it), `generate(text, language_id="sk", audio_prompt_path=ref)`. Refutes
  round 1's "Chatterbox has no Slovak". ~3.9 GB VRAM, ~4.5 s/sentence.
- **petercheben/F5_TTS_Slovak** (`engines/f5_sk.py`): **GPL**. `pip install f5-tts` +
  torch cu128. `F5TTS(ckpt_file=model_30000.safetensors, vocab_file=model_30000.txt)`;
  `infer(ref_file, ref_text, gen_text, remove_silence=True)`. EN/ZH base → the SK
  fine-tune wants a NATIVE-SK reference (SK ref → SK gen); cross-lingual EN ref is
  weaker. Card: "Numbers are not recognized, please use words instead."

### Reason rows (verified blockers, not rendered)
- **fishaudio/s2-pro** (`engines/fish_s2.py`): `sk` supported, but **Fish Audio
  Research License = non-commercial** (commercial needs a separate licence) +
  `fish_qwen3_omni` 2-shard model served via SGLang — not a plain 8 GB in-process load.
- **bosonai/higgs-*** (`engines/higgs.py`): `sk` supported, but **Boson Research &
  Non-Commercial License** + `higgs-tts-3-4b` weights index total_size = 8.49 GB
  (7.91 GiB) leave only ~55 MiB below the RTX 5050's 8151 MiB — no room for the CUDA
  context + audio tokenizer + activations. v2-3B / v3 need the full `boson_multimodal`
  stack + a separate audio tokenizer.

### Gotchas
- Two concurrent torch-cu128 `pip install` on dev2 saturate bandwidth (~15–20 min);
  `pip -q` hides progress — check `du -sh <venv>` (base venv ≈ 13 MB, torch ≈ 6–7.6 GB).
- Long dev2 renders: a plain `ssh … python …` that the client times out MAY keep
  running remotely (file-redirected stdout survives), but prefer `nohup … &` + poll
  the log for `DONE`/`Traceback` so a killed ssh never orphans a half-render.
- OmniVoice first run fetches 13 files (incl. the safetensors) unauthenticated —
  slow; set `HF_HUB_DISABLE_XET=1`.
