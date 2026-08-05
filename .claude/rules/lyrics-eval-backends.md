---
paths:
  - "eval/lyrics/**"
---

# Lyrics-eval backend integration — verified API shapes and traps

Every shape below was proven against the LIVE API on 2026-08-05, and in several
cases the vendor docs were WRONG. Trust this file over the docs; re-verify only
if a call starts failing.

## Driving win-resolume from a backend session

- **`mcp__win-resolume__FileWrite` SILENTLY TRUNCATES content over ~20,000
  characters.** No error — it writes less than you gave it and reports the
  smaller count. Two dispatched agents lost significant time to this. Write
  large files in `append: true` chunks and verify the remote size/line count
  afterwards.
- **PowerShell mangles inline python.** `python -c "..."` breaks on `$`, quotes
  and backticks (`select key, length(value)` → *"The term 'value' is not
  recognized"*). ALWAYS `FileWrite` a `.py` file, then run it via `Shell`.
- Venv with `requests`: `C:\ProgramData\SongPlayer\cache\tools\lyrics_venv\Scripts\python.exe`.
  Fixture audio: `C:\ProgramData\SongPlayer\eval-cache\<video_id>_vocal16k.wav`.
- API keys live in SQLite `C:\ProgramData\SongPlayer\songplayer.db`, table
  `settings`. `eval-run\run_backend.py` holds a `BACKEND_SPECS` registry that
  injects them into the child's env — add new backends there. Never put a key on
  a command line or into a repo file. Note `gemini_api_key` is a COMMA-SEPARATED
  LIST (take the first entry).
- To store a NEW key without it ever touching the transcript: `airuleset.py
  secret request <NAME>` for a one-shot encrypted URL, then `secret exec <NAME>
  -- python3 store.py` where the script PATCHes
  `http://10.77.9.201:8920/api/v1/settings` (a dict of `{name: value}`, returns
  204), then `secret forget <NAME>`. `airuleset.py upload` is a FILE uploader —
  wrong tool for a credential.

## Vendor API shapes (verified live — docs were wrong where noted)

**DashScope / Qwen-Omni**
- Endpoint: the classic `https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions`.
  **Docs claiming an `sk-ws-` workspace key needs a per-workspace
  `https://{WorkspaceId}.<region>.maas.aliyuncs.com` host are empirically FALSE** —
  the classic host resolves the workspace from the Bearer token. No
  `X-DashScope-WorkSpace` header needed. `dashscope-us` rejects an international key.
- Audio: `{"type":"input_audio","input_audio":{"data":"data:;base64,<B64>","format":"wav"}}`
  (empty MIME before `;base64,`). Set `modalities:["text"]` to avoid paying for
  synthesized speech. `stream:true` is documented as required.
- **The zh docs' "base64 must be <10 MB" is FALSE** — 19.2 MB base64 (14.4 MB raw)
  worked. The real failure is a TLS drop (`SSLEOFError`, not a clean 4xx) around
  57 MB base64; transcode anything over ~14 MB raw to mono MP3 first.
- `qwen3.8-max` has NO audio input (`input_modalities: ["text","image","video"]`,
  and video is frame-sampling only — the audio track is never decoded). Audio
  lives in the Omni line: `qwen3.5-omni-plus` / `-flash`.

**AssemblyAI Universal-3.5 Pro + same-call translation**
- `speech_models: ["universal-3-5-pro"]` — **dashes, not dots**; a wrong id
  returns HTTP 400 listing the valid set.
- Translation nests under `speech_understanding.request.translation`, NOT a
  top-level `translation` key.
- `match_original_utterance` requires `speaker_labels: true`.
- Translated text comes back per-utterance at
  `utterance["translated_texts"]["sk"]` — not `transcript["translation"]["sk"]`.
- Known limitation: translated text carries only utterance-level timing; per-word
  timings exist for the source language only.

**Soniox stt-async-v5**
- `POST /v1/files` (multipart) → `POST /v1/transcriptions` with
  `{"model":"stt-async-v5","file_id":...,"translation":{"type":"one_way","target_language":"sk"}}`
  → poll `GET /v1/transcriptions/{id}` (error detail is `error_message`) →
  `GET /v1/transcriptions/{id}/transcript`.
- Auth is `Authorization: Bearer <key>` (unlike AssemblyAI's raw token).
- **Translated tokens carry NO timestamps** and are produced at segment
  granularity; distinguish them by `translation_status`
  (`original`/`none` vs `translation`). Tokens can be sub-word — join on leading
  spaces before grouping.

## Scoring — known defects in `score_one_call.py`

`greedy_match` has **no monotonicity constraint** and picks the CLOSEST-START
eligible candidate, so on repetitive worship material it mis-pairs lines and
systematically pulls repeated-phrase deltas toward zero. 35.6% of one backend's
pairs violated ordering and carried 78% of the total delta mass, including a
physically impossible 603 s pair. **Only `median` and `% ≤400 ms` are quotable;
mean/p90 are not.** Always report a conservative second view alongside it: pair
only to gold lines whose normalized text is UNIQUE in the song, similarity ≥0.75
— see `eval/lyrics/reports/2026-08-05-combine-experiment.md` and reuse
`combine_lines_times.py` / `run_combine_experiment.py` so numbers stay comparable
across sessions.

Gold itself (lrclib/spotify/yt_subs line-sync) has its own noise floor, and the
models often transcribe what is REALLY sung more accurately than the gold text —
which weakens text-similarity matching while the timing is fine. Say so when
quoting any figure.
