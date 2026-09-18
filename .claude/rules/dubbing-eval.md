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
