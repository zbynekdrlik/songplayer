#!/usr/bin/env python3
"""gemini38_flash.py — the #144 one-call arm: Gemini 3.8 Flash, ONE call per
audio input, sung lyrics + line timing + versed Slovak in one structured reply.

The decisive #144 eval (design record: issue #144 comment 5801233778) measures
the newest one-call audio model against today's mtl chain on the gold set.
Two input shapes, same model, same prompt (`prompts/one_call_karaoke.md`):

- `--mode whole` — the whole isolated-vocal track in one call.
- `--mode win60` — the same call on 60 s ffmpeg `atrim` slices with 5 s
  overlap; each slice's timestamps get the slice offset added and the overlap
  is de-duplicated (`eval/lyrics/window_merge.py`, pure + unit-tested). The
  only mitigation for the 2026-08-05 whole-song drift (Gemini 3.6 Flash:
  7.3 % gold <= 400 ms, 13.6 % of lines past the audio end).

Restored from the deleted `gemini36_flash.py` (`git show 949fc79^:...`) and
moved onto the google-genai SDK's documented structured output — the owner's
directive is "models used as designed" (#144 comment 5793993578), so no
thinking cap and no temperature override are set unless `--thinking-level` is
passed explicitly.

Model id: `gemini-3.8-flash` — confirmed 2026-09-23 against the primary source
https://ai.google.dev/gemini-api/docs/models/gemini-3.8-flash (Stable; input
Text/Image/Video/Audio/PDF; structured outputs supported; thinking levels
low/medium/high, minimal NOT supported). Overridable with `--model`.

SDK facts (google-genai 2.24.0 — the version the box's lyrics venv pins via
`crates/sp-server/src/lyrics/bootstrap.rs::GENAI_PACKAGE`; read from the
wheel's `types.py` / `files.py` / `client.py`, not from memory):
`genai.Client(api_key=, http_options=types.HttpOptions(timeout=<ms>))`,
`types.GenerateContentConfig(system_instruction=, response_mime_type=,
response_json_schema=, thinking_config=types.ThinkingConfig(thinking_level=))`,
`types.Part.from_bytes(data=, mime_type=)` / `types.Part.from_uri(file_uri=,
mime_type=)`, `client.files.upload(file=, config={"mime_type": ...})` ->
poll `client.files.get(name=)` until `state == ACTIVE` -> `client.files.delete
(name=)`, `FinishReason.RECITATION`, `errors.ClientError.code`.
`google.genai` is imported LAZILY inside `GeminiCaller` — CI's eval-checks job
has no google-genai, and the tests drive everything through the injected
`caller` / `slicer` seams.

Never synthesized timing (project rule): a malformed / empty / blocked
response is an ERROR ROW (`error` set, `lines: []`), a line with impossible
timing (negative start, end before start) is dropped and counted, a line past
the end of the audio the call received is dropped and counted
(`metadata.n_lines_past_audio_end`) — nothing is ever repaired or estimated.

Key: `GEMINI_API_KEY` from the environment — a single key or the settings
table's comma-separated `gemini_api_key` list (keys are tried in order; an
invalid key or a 429 rotates to the next one). Never printed, never logged,
never on argv; every error text is redacted.

Usage (win-resolume, lyrics venv — see `.claude/rules/lyrics-eval-backends.md`
for the full box recipe incl. the key export):
    python -m eval.lyrics.backends.gemini38_flash --mode whole \\
        --video-id 5JW87KKDTcU --out <raw-dir>\\gemini38-flash-whole_5JW87KKDTcU.json
The manifest-wide loop is `eval/lyrics/run_one_call.py`.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import re
import subprocess
import sys
import time
import wave
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

from eval.lyrics import window_merge

logger = logging.getLogger("lyrics_eval.gemini38_flash")

BACKEND_ID = "gemini38-flash"
BACKEND_REVISION = 1
PROMPT_REVISION = 2
DEFAULT_MODEL = "gemini-3.8-flash"
MODES = ("whole", "win60")
THINKING_LEVELS = ("low", "medium", "high")

DEFAULT_CACHE_DIR = Path(r"C:\ProgramData\SongPlayer\eval-cache")
DEFAULT_TOOLS_DIR = r"C:\ProgramData\SongPlayer\cache\tools"

# Above this the WAV goes through the Files API instead of inline bytes. The
# inline request is capped at 20 MB and inline bytes travel base64-encoded
# (x 4/3), so 14 MB raw (~18.7 MB encoded) leaves room for the prompt. A 16 kHz
# mono s16 WAV is ~1.9 MB/min: songs over ~7.3 min (e.g. cej4vn4sWtE, 533 s)
# take the Files API path in the `whole` arm.
INLINE_LIMIT_BYTES = 14 * 1024 * 1024
AUDIO_MIME_TYPE = "audio/wav"
# HttpOptions.timeout is MILLISECONDS (types.py: "Timeout for the request in
# milliseconds"). A whole-song transcribe+translate can run for minutes.
REQUEST_TIMEOUT_MS = 900_000
FILE_ACTIVE_TIMEOUT_S = 120.0

USER_TURN = "Produce the sung lyrics of this audio as instructed."

PROMPT_PATH = Path(__file__).resolve().parents[1] / "prompts" / "one_call_karaoke.md"
# The markers must stand ALONE on their own line. The 2026-08-05 loader matched
# them anywhere, and the prompt file's header mentions both markers inline in
# backticks — so it extracted the 5 characters "` / `" between those mentions
# and the gemini36-flash spike ran with an effectively EMPTY system prompt.
PROMPT_MARKER_RE = re.compile(
    r"^<!-- PROMPT-START -->[ \t]*$(.*?)^<!-- PROMPT-END -->[ \t]*$",
    re.DOTALL | re.MULTILINE,
)
JSON_FENCE_RE = re.compile(r"```(?:json)?\s*(.*?)```", re.DOTALL)
JSON_OBJECT_RE = re.compile(r"\{.*\}", re.DOTALL)

# Structured output, JSON-Schema form (`response_json_schema`). Line-level
# only: no per-word array (v18 rule — never synthesized word timing).
RESPONSE_JSON_SCHEMA: dict[str, Any] = {
    "type": "object",
    "properties": {
        "lines": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "text": {"type": "string"},
                    "start_ms": {"type": "integer"},
                    "end_ms": {"type": "integer"},
                    "text_sk": {"type": "string"},
                },
                "required": ["text", "start_ms", "end_ms", "text_sk"],
                "propertyOrdering": ["text", "start_ms", "end_ms", "text_sk"],
            },
        }
    },
    "required": ["lines"],
}

REQUIRED_LINE_FIELDS = ("text", "start_ms", "end_ms")

# Windows: hide the ffmpeg console (CREATE_NO_WINDOW).
_CREATE_NO_WINDOW = 0x08000000


class ResponseError(RuntimeError):
    """The model's reply cannot be used (blocked, empty, malformed)."""


@dataclass(frozen=True)
class CallResult:
    """What one model call returned — the transport-neutral seam the tests
    fake. `text` is the concatenated text parts of the first candidate."""

    text: str | None
    finish_reason: str | None
    usage: dict[str, Any] | None
    block_reason: str | None = None


Caller = Callable[[Path], CallResult]
Slicer = Callable[[Path, Path, int, int], None]


# ── small helpers ────────────────────────────────────────────────────────────


def backend_label(mode: str) -> str:
    """The raw-file / scorer label of an arm: `gemini38-flash-<mode>`."""
    if mode not in MODES:
        raise ValueError(f"unknown mode {mode!r}; expected one of {MODES}")
    return f"{BACKEND_ID}-{mode}"


def default_audio_path(cache_dir: Path, video_id: str) -> Path:
    return cache_dir / f"{video_id}_vocal16k.wav"


def key_pool(raw: str | None) -> list[str]:
    """`GEMINI_API_KEY` may be one key or the settings table's CSV list."""
    if not raw:
        return []
    return [k.strip() for k in raw.split(",") if k.strip()]


def redact(text: str, keys: list[str]) -> str:
    for key in keys:
        if key:
            text = text.replace(key, "<redacted>")
    return text


def load_prompt(path: Path = PROMPT_PATH) -> str:
    raw = path.read_text(encoding="utf-8")
    m = PROMPT_MARKER_RE.search(raw)
    if not m:
        raise RuntimeError(
            f"{path} is missing <!-- PROMPT-START -->/<!-- PROMPT-END --> markers"
        )
    return m.group(1).strip()


def wav_duration_ms(path: Path) -> int:
    with wave.open(str(path), "rb") as w:
        return int(w.getnframes() * 1000 // w.getframerate())


def default_ffmpeg() -> str:
    tools = Path(os.environ.get("SONGPLAYER_TOOLS_DIR", DEFAULT_TOOLS_DIR))
    exe = tools / "ffmpeg.exe"
    if sys.platform == "win32" and exe.exists():
        return str(exe)
    return "ffmpeg"


def ffmpeg_slice_cmd(
    ffmpeg: str, src: Path, dst: Path, start_ms: int, end_ms: int
) -> list[str]:
    """One window of the vocal WAV, sample-exact, timestamps reset to 0."""
    atrim = (
        f"atrim=start={start_ms / 1000:.3f}:end={end_ms / 1000:.3f},"
        "asetpts=PTS-STARTPTS"
    )
    return [
        ffmpeg,
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-i",
        str(src),
        "-af",
        atrim,
        "-c:a",
        "pcm_s16le",
        str(dst),
    ]


def make_ffmpeg_slicer(ffmpeg: str) -> Slicer:
    def slicer(src: Path, dst: Path, start_ms: int, end_ms: int) -> None:
        cmd = ffmpeg_slice_cmd(ffmpeg, src, dst, start_ms, end_ms)
        kwargs: dict[str, Any] = {}
        if sys.platform == "win32":
            kwargs["creationflags"] = _CREATE_NO_WINDOW
        logger.debug("ffmpeg slice %s [%d, %d) -> %s", src, start_ms, end_ms, dst)
        # explicit UTF-8 + replacement: the Windows locale codec would raise on
        # non-ASCII ffmpeg stderr (see .claude/rules/yt-dlp-spawn-env.md)
        proc = subprocess.run(
            cmd, capture_output=True, encoding="utf-8", errors="replace", **kwargs
        )
        if proc.returncode != 0 or not dst.exists():
            raise RuntimeError(
                f"ffmpeg slice [{start_ms}, {end_ms}) failed "
                f"(exit {proc.returncode}): {proc.stderr.strip()[-400:]}"
            )

    return slicer


# ── response parsing (pure) ──────────────────────────────────────────────────


def parse_response_text(text: str) -> dict[str, Any]:
    """Parse the reply into a JSON object. Structured output normally returns
    bare JSON; a fence / preamble is tolerated as a second line of defence.
    Anything else raises — never a guessed shape."""
    trimmed = (text or "").strip()
    attempts = [trimmed]
    fence = JSON_FENCE_RE.search(trimmed)
    if fence:
        attempts.append(fence.group(1).strip())
    obj = JSON_OBJECT_RE.search(trimmed)
    if obj:
        attempts.append(obj.group(0))
    for candidate in attempts:
        try:
            parsed = json.loads(candidate)
        except json.JSONDecodeError as e:
            logger.debug("JSON parse attempt failed: %s", e)
            continue
        if isinstance(parsed, dict):
            return parsed
    raise ResponseError(f"model output is not a JSON object: {trimmed[:300]!r}")


def _as_int(value: Any, field: str, raw: dict[str, Any]) -> int:
    if isinstance(value, bool):
        raise ResponseError(f"line field {field} is not an integer: {raw!r}")
    if isinstance(value, int):
        return value
    if isinstance(value, float) and value.is_integer():
        return int(value)
    if isinstance(value, str):
        try:
            return int(value.strip())
        except ValueError:
            raise ResponseError(
                f"line field {field} is not an integer: {raw!r}"
            ) from None
    raise ResponseError(f"line field {field} is not an integer: {raw!r}")


def payload_to_lines(
    payload: dict[str, Any],
) -> tuple[list[dict[str, Any]], dict[str, int]]:
    """`{lines:[...]}` -> `[{text, start_ms, end_ms, text_sk}]` + parse stats.

    A missing `lines` array, a line missing `text`/`start_ms`/`end_ms`, or a
    non-integer time is a malformed response (ResponseError -> error row). A
    blank line is skipped, a physically impossible time (negative start, end
    before start) is dropped — both COUNTED, never repaired. A missing or
    blank `text_sk` keeps the line (timing is still measurable) with
    `text_sk: None`, counted as a translation gap."""
    raw_lines = payload.get("lines")
    if not isinstance(raw_lines, list):
        raise ResponseError(
            f"JSON payload has no 'lines' array: {json.dumps(payload)[:300]}"
        )
    stats = {"n_raw": len(raw_lines), "n_blank_text": 0, "n_invalid_timing": 0}
    lines: list[dict[str, Any]] = []
    n_missing_sk = 0
    for raw in raw_lines:
        if not isinstance(raw, dict):
            raise ResponseError(f"line is not an object: {raw!r}")
        missing = [f for f in REQUIRED_LINE_FIELDS if f not in raw]
        if missing:
            raise ResponseError(f"line missing {', '.join(missing)}: {raw!r}")
        start_ms = _as_int(raw["start_ms"], "start_ms", raw)
        end_ms = _as_int(raw["end_ms"], "end_ms", raw)
        text = str(raw["text"] or "").strip()
        if not text:
            stats["n_blank_text"] += 1
            continue
        if start_ms < 0 or end_ms < start_ms:
            stats["n_invalid_timing"] += 1
            logger.warning("dropped line with impossible timing: %r", raw)
            continue
        text_sk = str(raw.get("text_sk") or "").strip() or None
        if text_sk is None:
            n_missing_sk += 1
        lines.append(
            {"text": text, "start_ms": start_ms, "end_ms": end_ms, "text_sk": text_sk}
        )
    stats["n_missing_sk"] = n_missing_sk
    if n_missing_sk:
        logger.warning(
            "%d/%d lines without text_sk (translation gap)", n_missing_sk, len(lines)
        )
    return lines, stats


def interpret_call(call: CallResult) -> tuple[list[dict[str, Any]], dict[str, int]]:
    """A model reply -> lines, or ResponseError for a blocked / truncated /
    empty / malformed one. RECITATION (the copyright block) is named
    explicitly — it must be visible, never an empty result."""
    if call.block_reason:
        raise ResponseError(f"prompt blocked: block_reason={call.block_reason}")
    if call.finish_reason == "RECITATION":
        raise ResponseError("model output blocked: finish_reason=RECITATION")
    if call.finish_reason not in (None, "STOP"):
        raise ResponseError(
            f"model stopped abnormally: finish_reason={call.finish_reason}"
        )
    if not call.text or not call.text.strip():
        raise ResponseError(
            f"model returned no text (finish_reason={call.finish_reason})"
        )
    return payload_to_lines(parse_response_text(call.text))


# ── the real model call (lazy google-genai) ──────────────────────────────────


class GeminiCaller:
    """One `generate_content` call per audio file, structured output, keys
    tried in order (an invalid key or a 429 rotates; any other API error is
    raised, redacted). The caller is the ONLY thing the unit tests fake."""

    def __init__(
        self,
        *,
        model: str,
        keys: list[str],
        thinking_level: str | None = None,
        timeout_ms: int = REQUEST_TIMEOUT_MS,
    ) -> None:
        if not keys:
            raise ValueError("no Gemini API key")
        if thinking_level is not None and thinking_level not in THINKING_LEVELS:
            raise ValueError(f"thinking_level must be one of {THINKING_LEVELS}")
        self.model = model
        self.keys = keys
        self.thinking_level = thinking_level
        self.timeout_ms = timeout_ms
        self.prompt = load_prompt()
        self._key_idx = 0

    def __call__(self, wav_path: Path) -> CallResult:
        from google.genai import errors

        last = ""
        for attempt in range(len(self.keys)):
            idx = (self._key_idx + attempt) % len(self.keys)
            try:
                result = self._call_with_key(self.keys[idx], wav_path)
            except errors.ClientError as e:
                msg = redact(str(e), self.keys)
                rotatable = e.code == 429 or (
                    e.code in (400, 401, 403) and "api key" in msg.lower()
                )
                if not rotatable:
                    raise RuntimeError(f"gemini client error: {msg}") from None
                logger.warning("gemini key #%d unusable (%s) — rotating", idx + 1, msg)
                last = msg
                continue
            except Exception as e:  # noqa: BLE001 — re-raised, redacted
                raise RuntimeError(
                    f"gemini call failed: {type(e).__name__}: "
                    f"{redact(str(e), self.keys)}"
                ) from None
            self._key_idx = idx
            return result
        raise RuntimeError(f"all {len(self.keys)} Gemini keys failed; last: {last}")

    def _call_with_key(self, key: str, wav_path: Path) -> CallResult:
        from google import genai
        from google.genai import types

        client = genai.Client(
            api_key=key, http_options=types.HttpOptions(timeout=self.timeout_ms)
        )
        size = wav_path.stat().st_size
        uploaded_name: str | None = None
        try:
            if size <= INLINE_LIMIT_BYTES:
                part = types.Part.from_bytes(
                    data=wav_path.read_bytes(), mime_type=AUDIO_MIME_TYPE
                )
                transport = "inline"
            else:
                f = client.files.upload(
                    file=str(wav_path), config={"mime_type": AUDIO_MIME_TYPE}
                )
                uploaded_name = f.name
                f = self._wait_active(client, f)
                part = types.Part.from_uri(
                    file_uri=f.uri, mime_type=f.mime_type or AUDIO_MIME_TYPE
                )
                transport = "files_api"
            thinking = (
                types.ThinkingConfig(thinking_level=self.thinking_level.upper())
                if self.thinking_level
                else None
            )
            config = types.GenerateContentConfig(
                system_instruction=self.prompt,
                response_mime_type="application/json",
                response_json_schema=RESPONSE_JSON_SCHEMA,
                thinking_config=thinking,
            )
            logger.info(
                "gemini generate_content: model=%s audio=%s bytes=%d transport=%s",
                self.model,
                wav_path.name,
                size,
                transport,
            )
            t0 = time.time()
            resp = client.models.generate_content(
                model=self.model, contents=[part, USER_TURN], config=config
            )
            logger.info("gemini reply in %.1fs", time.time() - t0)
        finally:
            if uploaded_name:
                try:
                    client.files.delete(name=uploaded_name)
                except Exception as e:  # noqa: BLE001 — cleanup only, logged
                    logger.warning(
                        "could not delete uploaded file %s: %s",
                        uploaded_name,
                        redact(str(e), self.keys),
                    )
        return _to_call_result(resp)

    @staticmethod
    def _wait_active(client: Any, f: Any) -> Any:
        deadline = time.time() + FILE_ACTIVE_TIMEOUT_S
        while True:
            state = _enum_str(getattr(f, "state", None))
            if state == "ACTIVE":
                return f
            if state == "FAILED":
                raise RuntimeError(f"uploaded file processing FAILED: {f.error!r}")
            if time.time() > deadline:
                raise RuntimeError(
                    f"uploaded file not ACTIVE after {FILE_ACTIVE_TIMEOUT_S}s "
                    f"(state={state})"
                )
            time.sleep(2.0)
            f = client.files.get(name=f.name)


def _enum_str(value: Any) -> str | None:
    if value is None:
        return None
    return str(getattr(value, "value", value))


def _to_call_result(resp: Any) -> CallResult:
    candidates = resp.candidates or []
    finish = _enum_str(candidates[0].finish_reason) if candidates else None
    feedback = getattr(resp, "prompt_feedback", None)
    block = _enum_str(getattr(feedback, "block_reason", None)) if feedback else None
    usage = None
    if resp.usage_metadata is not None:
        usage = resp.usage_metadata.model_dump(mode="json", exclude_none=True)
    text = resp.text if candidates else None
    return CallResult(text=text, finish_reason=finish, usage=usage, block_reason=block)


# ── one fixture (pure orchestration over the caller/slicer seams) ────────────


def error_row(
    label: str,
    video_id: str,
    message: str,
    *,
    metadata: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """The output shape for a fixture that produced no usable lines."""
    return {
        "backend_id": label,
        "backend_revision": BACKEND_REVISION,
        "video_id": video_id,
        "wav_path": None,
        "duration_ms": None,
        "lines": [],
        "raw_confidence": None,
        "error": message,
        "metadata": metadata or {},
    }


def _run_whole(audio: Path, caller: Caller, md: dict[str, Any]) -> list[dict]:
    md["n_calls"] = 1
    call = caller(audio)
    md["finish_reason"] = call.finish_reason
    md["usage"] = call.usage
    lines, stats = interpret_call(call)
    md["parse"] = stats
    if not lines:
        raise ResponseError("empty response: the model returned no lines")
    clip = window_merge.clip_past_end(
        sorted(lines, key=lambda ln: ln["start_ms"]), md["audio_duration_ms"]
    )
    md["n_lines_past_audio_end"] = clip.n_past_end
    md["n_end_overrun"] = clip.n_end_overrun
    return clip.lines


def _run_win60(
    video_id: str,
    audio: Path,
    caller: Caller,
    slicer: Slicer,
    work_dir: Path,
    md: dict[str, Any],
) -> list[dict]:
    duration = md["audio_duration_ms"]
    windows = window_merge.split_windows(duration)
    work_dir.mkdir(parents=True, exist_ok=True)
    per_window: list[tuple[int, list[dict[str, Any]]]] = []
    records: list[dict[str, Any]] = []
    past_window_end = overrun = 0
    md.update(n_windows=len(windows), n_calls=0, windows=records)
    for i, (start, end) in enumerate(windows):
        rec: dict[str, Any] = {"index": i, "start_ms": start, "end_ms": end}
        records.append(rec)
        clip_path = work_dir / f"{video_id}_w{i:02d}_{start}_{end}.wav"
        try:
            slicer(audio, clip_path, start, end)
            md["n_calls"] += 1
            call = caller(clip_path)
            rec.update(finish_reason=call.finish_reason, usage=call.usage)
            lines, stats = interpret_call(call)
            clip = window_merge.clip_past_end(lines, end - start)
            past_window_end += clip.n_past_end
            overrun += clip.n_end_overrun
            per_window.append((start, clip.lines))
            rec.update(
                error=None,
                n_lines=len(clip.lines),
                n_past_window_end=clip.n_past_end,
                parse=stats,
            )
        except Exception as e:  # noqa: BLE001 — recorded per window, never hidden
            rec["error"] = f"{type(e).__name__}: {e}"
            logger.error("%s window %d [%d, %d): %s", video_id, i, start, end, e)
        finally:
            clip_path.unlink(missing_ok=True)

    merged = window_merge.merge_window_lines(
        per_window, audio_end_ms=duration, overlap_ms=window_merge.DEFAULT_OVERLAP_MS
    )
    errors = [r["error"] for r in records if r.get("error")]
    md.update(
        n_window_errors=len(errors),
        n_duplicates_dropped=merged.n_duplicates_dropped,
        duplicates_dropped=[
            {"text": d["text"], "start_ms": d["start_ms"], "kind": d["kind"]}
            for d in merged.duplicates
        ],
        n_lines_past_audio_end=past_window_end + merged.n_past_end,
        n_end_overrun=overrun + merged.n_end_overrun,
    )
    if errors and len(errors) == len(windows):
        raise ResponseError(f"all {len(windows)} windows failed; first: {errors[0]}")
    if not merged.lines and not md["n_lines_past_audio_end"]:
        # nothing at all came back. Lines that all fell past the audio end
        # are NOT this case: they stay a scored row so the count is pooled.
        raise ResponseError("the model returned no lines in any window")
    return merged.lines


def run_fixture(
    *,
    video_id: str,
    mode: str,
    audio: Path,
    caller: Caller,
    slicer: Slicer | None,
    work_dir: Path,
    model: str,
) -> dict[str, Any]:
    """Run one fixture in one mode; ALWAYS returns a row (an error row on any
    failure) so a manifest loop never loses the fixture's gold denominator."""
    label = backend_label(mode)
    md: dict[str, Any] = {
        "model": model,
        "mode": mode,
        "prompt_revision": PROMPT_REVISION,
        "n_calls": 0,
    }
    t0 = time.time()
    try:
        if not audio.exists():
            # a permanent input gap (no cached vocal WAV), not a model failure:
            # the runner counts it apart so exit 1 means "re-run helps"
            md["error_kind"] = "missing_input"
            raise FileNotFoundError(f"audio not found: {audio}")
        md["audio_duration_ms"] = wav_duration_ms(audio)
        if mode == "whole":
            lines = _run_whole(audio, caller, md)
        else:
            if slicer is None:
                raise ValueError("win60 mode needs a slicer")
            lines = _run_win60(video_id, audio, caller, slicer, work_dir, md)
        error = None
    except Exception as e:  # noqa: BLE001 — becomes the error row
        lines, error = [], f"{type(e).__name__}: {e}"
        logger.error("%s %s: %s", label, video_id, error)
    md["elapsed_s"] = round(time.time() - t0, 1)
    md["line_count"] = len(lines)
    row = error_row(label, video_id, error or "", metadata=md)
    row.update(
        wav_path=str(audio),
        duration_ms=md.get("audio_duration_ms"),
        lines=lines,
        error=error,
    )
    return row


# ── CLI ──────────────────────────────────────────────────────────────────────


def build_arg_parser(description: str | None = None) -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description=description)
    p.add_argument("--model", default=DEFAULT_MODEL)
    p.add_argument("--mode", choices=MODES, required=True)
    p.add_argument("--cache-dir", type=Path, default=DEFAULT_CACHE_DIR)
    p.add_argument("--thinking-level", choices=THINKING_LEVELS, default=None)
    p.add_argument("--ffmpeg", default=None, help="default: tools dir ffmpeg.exe")
    return p


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(
        level=os.environ.get("LYRICS_EVAL_LOG_LEVEL", "INFO"),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    p = build_arg_parser(__doc__)
    p.add_argument("--video-id", required=True)
    p.add_argument("--audio", type=Path, default=None)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--work-dir", type=Path, default=None)
    args = p.parse_args(argv)

    keys = key_pool(os.environ.get("GEMINI_API_KEY"))
    if not keys:
        logger.error("GEMINI_API_KEY not set — aborting")
        return 2
    audio = args.audio or default_audio_path(args.cache_dir, args.video_id)
    row = run_fixture(
        video_id=args.video_id,
        mode=args.mode,
        audio=audio,
        caller=GeminiCaller(
            model=args.model, keys=keys, thinking_level=args.thinking_level
        ),
        slicer=make_ffmpeg_slicer(args.ffmpeg or default_ffmpeg()),
        work_dir=args.work_dir or args.out.parent / "_clips",
        model=args.model,
    )
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(
        json.dumps(row, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    logger.info(
        "%s %s: lines=%d past_audio_end=%s error=%s -> %s",
        row["backend_id"],
        args.video_id,
        len(row["lines"]),
        row["metadata"].get("n_lines_past_audio_end"),
        row["error"],
        args.out,
    )
    return 0 if row["error"] is None else 1


if __name__ == "__main__":
    raise SystemExit(main())
