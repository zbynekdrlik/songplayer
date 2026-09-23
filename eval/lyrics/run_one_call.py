#!/usr/bin/env python3
"""run_one_call.py — the #144 manifest loop for the one-call Gemini 3.8 Flash
arms (`whole` / `win60`).

Loops the CURRENT `eval/lyrics/manifest.json` fixtures in manifest order, runs
`backends/gemini38_flash.run_fixture` on each fixture's isolated-vocal WAV
(`<cache-dir>\\<video_id>_vocal16k.wav`), and writes one raw file per fixture
to `--raw-dir` as `<label>_<video_id>.json` with `<label>` =
`gemini38-flash-whole` / `gemini38-flash-win60` — exactly the path
`score_one_call.load_produced` reads. The main session then scores the raw dir
with `score_aligner.py` / `score_one_call.py` (pure algorithmic scorers).

Every fixture ALWAYS gets a file: a failure (the model call, ffmpeg, a missing
WAV) is written as an error row (`error` set, `lines: []`) so the fixture keeps
its gold lines in the scorer's honest denominator. A re-run skips fixtures that
already have an OK file and retries only error rows / missing files (a 429 or a
transient 5xx mid-run never freezes a gap); `--force` re-runs everything.

The poisoned fixture (`Xvm4_fWkXe8`) is run like any other — the scorers
exclude it from the pooled aggregate and report it on its own.

Key: `GEMINI_API_KEY` in the environment (one key or the settings CSV list);
never on argv, never logged. Box recipe: `.claude/rules/lyrics-eval-backends.md`.
"""

from __future__ import annotations

import json
import logging
import os
from pathlib import Path
from typing import Any, Callable

from eval.lyrics import score_one_call
from eval.lyrics.backends import gemini38_flash

logger = logging.getLogger("lyrics_eval.run_one_call")

RunOne = Callable[[str], dict[str, Any]]


def _existing_ok(path: Path) -> bool:
    """An existing raw file counts as done only if it parses and carries no
    error — error rows and corrupt files are retried."""
    if not path.exists():
        return False
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as e:
        logger.warning("existing %s unreadable (%s) — re-running", path.name, e)
        return False
    return isinstance(data, dict) and data.get("error") is None


def run_all(
    *,
    manifest_path: Path,
    mode: str,
    raw_dir: Path,
    run_one: RunOne,
    force: bool,
    only: list[str] | None = None,
) -> dict[str, int]:
    label = gemini38_flash.backend_label(mode)
    fixtures = score_one_call.load_manifest(manifest_path)
    if only:
        unknown = sorted(set(only) - set(fixtures))
        if unknown:
            raise ValueError(f"--only ids not in the manifest: {unknown}")
    video_ids = [vid for vid in fixtures if not only or vid in only]
    raw_dir.mkdir(parents=True, exist_ok=True)
    summary = {"written": 0, "skipped": 0, "errors": 0}

    for n, vid in enumerate(video_ids, start=1):
        out = raw_dir / f"{label}_{vid}.json"
        if not force and _existing_ok(out):
            logger.info(
                "[%d/%d] %s: OK output exists — skipped", n, len(video_ids), vid
            )
            summary["skipped"] += 1
            continue
        try:
            row = run_one(vid)
        except Exception as e:  # noqa: BLE001 — becomes this fixture's error row
            row = gemini38_flash.error_row(label, vid, f"{type(e).__name__}: {e}")
        out.write_text(
            json.dumps(row, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
        )
        summary["written"] += 1
        if row.get("error"):
            summary["errors"] += 1
        md = row.get("metadata") or {}
        logger.info(
            "[%d/%d] %s %s: lines=%d past_audio_end=%s window_errors=%s error=%s",
            n,
            len(video_ids),
            label,
            vid,
            len(row.get("lines") or []),
            md.get("n_lines_past_audio_end"),
            md.get("n_window_errors"),
            row.get("error"),
        )
    return summary


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(
        level=os.environ.get("LYRICS_EVAL_LOG_LEVEL", "INFO"),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    root = Path(__file__).resolve().parents[2]
    p = gemini38_flash.build_arg_parser(__doc__)
    p.add_argument(
        "--manifest", type=Path, default=root / "eval" / "lyrics" / "manifest.json"
    )
    p.add_argument("--raw-dir", type=Path, required=True)
    p.add_argument("--work-dir", type=Path, default=None)
    p.add_argument("--force", action="store_true")
    p.add_argument("--only", nargs="+", default=None, help="video ids subset")
    args = p.parse_args(argv)

    keys = gemini38_flash.key_pool(os.environ.get("GEMINI_API_KEY"))
    if not keys:
        logger.error("GEMINI_API_KEY not set — aborting")
        return 2
    caller = gemini38_flash.GeminiCaller(
        model=args.model, keys=keys, thinking_level=args.thinking_level
    )
    slicer = gemini38_flash.make_ffmpeg_slicer(
        args.ffmpeg or gemini38_flash.default_ffmpeg()
    )
    work_dir = args.work_dir or args.raw_dir / "_clips"

    def run_one(video_id: str) -> dict[str, Any]:
        return gemini38_flash.run_fixture(
            video_id=video_id,
            mode=args.mode,
            audio=gemini38_flash.default_audio_path(args.cache_dir, video_id),
            caller=caller,
            slicer=slicer,
            work_dir=work_dir,
            model=args.model,
        )

    summary = run_all(
        manifest_path=args.manifest,
        mode=args.mode,
        raw_dir=args.raw_dir,
        run_one=run_one,
        force=args.force,
        only=args.only,
    )
    label = gemini38_flash.backend_label(args.mode)
    print(
        f"{label}: written={summary['written']} skipped={summary['skipped']} "
        f"errors={summary['errors']} -> {args.raw_dir}"
    )
    # Non-zero while any fixture is still an error row, so a re-run is visibly
    # needed (it retries exactly those).
    return 1 if summary["errors"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
