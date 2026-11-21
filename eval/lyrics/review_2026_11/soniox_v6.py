#!/usr/bin/env python3
"""soniox_v6.py — Soniox ``stt-async-v6`` on the fixture set (#349).

Soniox released ``stt-async-v6`` on 3.11.2026 ("fully compatible with the
existing v5 models and Soniox API. To upgrade, simply replace the model
name", docs.soniox.com/stt/models, read 21.11.2026). The REST flow is the
one the deleted ``backends/soniox_v5.py`` used (git b2aba761):

  1. POST   /v1/files                         multipart ``file``
  2. POST   /v1/transcriptions                {model, file_id, ...}
  3. GET    /v1/transcriptions/{id}           poll until completed | error
  4. GET    /v1/transcriptions/{id}/transcript -> {tokens: [...]}
  5. DELETE /v1/transcriptions/{id}, DELETE /v1/files/{id} (best effort)

Auth ``Authorization: Bearer $SONIOX_API_KEY`` (read from the environment,
never printed, never on argv).

The request is v5's minus the translation (this measures the transcript +
word times only): ``language_hints`` en/es, language identification on, no
diarization, no ``context``. Tokens are sub-word pieces; a token with a
leading space (or a pure-whitespace token) starts a new word, as in v5.
Audio-event tokens are dropped. Words are grouped into lines at a 400 ms
silence gap with the SAME ``group_lines`` as ``ctc_lyrics.py`` / the g35t
adapter, so the no-text tier is comparable.

Input = the dev1 ``<vid>_vocal16k.wav`` (``prep_fixtures.py``), the input
production's g35t gets. Output: ``<out-dir>/soniox-v6_<vid>.json`` in the
aligner-raw shape ``score_aligner.py`` reads; a failure writes an error row.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path
from typing import Any

import requests

from eval.lyrics.review_2026_11.ctc_lyrics import MANIFEST, POISONED, group_lines

BACKEND_ID = "soniox-v6"
MODEL = "stt-async-v6"
API = "https://api.soniox.com"
POLL_S = 2.0
POLL_TIMEOUT_S = 1800


def _h(key: str) -> dict[str, str]:
    return {"Authorization": f"Bearer {key}"}


def _ok(r: requests.Response, what: str) -> dict:
    if r.status_code not in (200, 201):
        raise RuntimeError(f"soniox {what}: HTTP {r.status_code} {r.text[:300]}")
    return r.json() or {}


def tokens_to_words(tokens: list[dict[str, Any]]) -> list[dict]:
    """Sub-word tokens -> timed words (leading space / whitespace token = a
    word boundary). A token without times is dropped, never guessed."""
    words: list[dict] = []
    cur: dict | None = None
    for tok in tokens:
        if tok.get("is_audio_event") or tok.get("translation_status") == "translation":
            cur = None  # an event ends the word it interrupts
            continue
        raw = tok.get("text") or ""
        ts, te = tok.get("start_ms"), tok.get("end_ms")
        content = raw.strip()
        if not content:
            cur = None
            continue
        if ts is None or te is None:
            continue
        if cur is None or raw[0].isspace():
            cur = {"text": content, "start_ms": int(ts), "end_ms": int(te)}
            words.append(cur)
        else:
            cur["text"] += content
            cur["end_ms"] = int(te)
    words.sort(key=lambda w: w["start_ms"])
    return words


def transcribe(wav: Path, key: str) -> tuple[list[dict], dict]:
    with wav.open("rb") as fh:
        f = _ok(
            requests.post(
                f"{API}/v1/files",
                headers=_h(key),
                files={"file": (wav.name, fh, "audio/wav")},
                timeout=300,
            ),
            "upload",
        )
    file_id = f["id"]
    tid = None
    try:
        body = {
            "model": MODEL,
            "file_id": file_id,
            "language_hints": ["en", "es"],
            "enable_language_identification": True,
            "enable_speaker_diarization": False,
        }
        tid = _ok(
            requests.post(
                f"{API}/v1/transcriptions", headers=_h(key), json=body, timeout=60
            ),
            "create",
        )["id"]
        deadline = time.monotonic() + POLL_TIMEOUT_S
        while True:
            if time.monotonic() > deadline:
                raise RuntimeError(f"soniox poll timed out after {POLL_TIMEOUT_S}s")
            time.sleep(POLL_S)
            st = _ok(
                requests.get(
                    f"{API}/v1/transcriptions/{tid}", headers=_h(key), timeout=60
                ),
                "poll",
            )
            if st.get("status") == "completed":
                break
            if st.get("status") == "error":
                raise RuntimeError(f"soniox error: {st.get('error_message')!r}")
        tr = _ok(
            requests.get(
                f"{API}/v1/transcriptions/{tid}/transcript", headers=_h(key), timeout=60
            ),
            "transcript",
        )
        tokens = tr.get("tokens")
        if not isinstance(tokens, list):
            raise RuntimeError("soniox transcript has no tokens[]")
        langs = sorted({t.get("language") for t in tokens if t.get("language")})
        return tokens_to_words(tokens), {
            "audio_duration_ms": st.get("audio_duration_ms"),
            "n_tokens": len(tokens),
            "languages": langs,
        }
    finally:
        for path in ([f"/v1/transcriptions/{tid}"] if tid else []) + [
            f"/v1/files/{file_id}"
        ]:
            try:
                requests.delete(f"{API}{path}", headers=_h(key), timeout=30)
            except requests.RequestException as exc:
                print(f"cleanup {path} failed: {exc!r}", file=sys.stderr)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--wav-dir", type=Path, required=True)
    ap.add_argument("--out-dir", type=Path, required=True)
    ap.add_argument("--only", nargs="*", default=[])
    args = ap.parse_args(argv)
    key = os.environ.get("SONIOX_API_KEY")
    if not key:
        print("SONIOX_API_KEY is not set", file=sys.stderr)
        return 2
    args.out_dir.mkdir(parents=True, exist_ok=True)
    failed = []
    for fx in json.loads(MANIFEST.read_text())["fixtures"]:
        vid = fx["video_id"]
        if vid == POISONED or (args.only and vid not in args.only):
            continue
        wav = args.wav_dir / f"{vid}_vocal16k.wav"
        out_path = args.out_dir / f"{BACKEND_ID}_{vid}.json"
        t0 = time.monotonic()
        try:
            if not wav.exists():
                raise FileNotFoundError(f"no input {wav.name}")
            words, meta = transcribe(wav, key)
        except Exception as exc:  # an error row, never a guessed timing
            failed.append(vid)
            out_path.write_text(
                json.dumps({"backend_id": BACKEND_ID, "error": repr(exc), "lines": []})
            )
            print(f"{vid}: FAILED {exc!r}", flush=True)
            continue
        runtime = time.monotonic() - t0
        lines = group_lines(words)
        out = {
            "backend_id": BACKEND_ID,
            "wav_path": str(wav),
            "duration_ms": meta["audio_duration_ms"],
            "lines": lines,
            "metadata": {"model": MODEL, "runtime_sec": round(runtime, 3), **meta},
        }
        out_path.write_text(json.dumps(out, ensure_ascii=False))
        print(
            f"{vid}: {len(words)} words, {len(lines)} lines, {runtime:.1f}s", flush=True
        )
    print(f"done; failed={failed}", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
