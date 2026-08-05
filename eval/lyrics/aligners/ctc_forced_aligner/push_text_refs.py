#!/usr/bin/env python3
"""push_text_refs.py — generate the compact per-fixture reference-text files
that `run.py` / `batch_run.py` align against.

The forced aligner receives FIXED reference text — it does not transcribe.
The reference text is qwen35-omni's own already-produced `lines[].text` /
`text_sk` for each of the 22 fixtures (see
`eval/lyrics/reports/2026-08-05-raw/qwen35-omni_<video_id>.json`), stripped
down to just `{text, text_sk}` per line (dropping qwen35-omni's own
`start_ms`/`end_ms`/`words` — this backend ignores them entirely, since its
whole point is to re-time that same text with a real forced aligner instead
of trusting the audio-LLM's guessed timestamps).

This script only WRITES the compact files locally (this repo, under
`--out-dir`, default `/tmp` staging) — it does not push them to win-resolume
itself (no MCP access from a plain script). The actual push is a handful of
`mcp__win-resolume__FileWrite` calls (one per fixture; each compact file is
a few KB, well under any transport limit) to
`C:\\ProgramData\\SongPlayer\\eval-run\\aligners\\text-refs\\<video_id>.json`,
done once per shootout run — `run.py`/`batch_run.py` read from that pushed
location, never from this repo directly (they run ON win-resolume where the
repo isn't checked out).

Usage:
    python3 eval/lyrics/aligners/ctc_forced_aligner/push_text_refs.py \\
        --manifest eval/lyrics/manifest.json \\
        --raw-dir eval/lyrics/reports/2026-08-05-raw \\
        --out-dir /tmp/text-refs-staging
"""

from __future__ import annotations

import argparse
import json
import logging
from pathlib import Path

logger = logging.getLogger("lyrics_eval.ctc_forced_aligner.push_text_refs")


def build_compact_ref(video_id: str, category: str, raw_path: Path) -> dict | None:
    if not raw_path.exists():
        logger.warning(
            "no qwen35-omni raw output for video_id=%s (%s) — skipping",
            video_id,
            raw_path,
        )
        return None
    data = json.loads(raw_path.read_text(encoding="utf-8"))
    lines = data.get("lines") or []
    return {
        "video_id": video_id,
        "category": category,
        "lines": [
            {"text": line.get("text"), "text_sk": line.get("text_sk")} for line in lines
        ],
    }


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(
        level=logging.INFO, format="%(levelname)s %(name)s: %(message)s"
    )
    root = Path(__file__).resolve().parents[3]

    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--manifest", type=Path, default=root / "eval" / "lyrics" / "manifest.json"
    )
    p.add_argument(
        "--raw-dir",
        type=Path,
        default=root / "eval" / "lyrics" / "reports" / "2026-08-05-raw",
    )
    p.add_argument("--out-dir", type=Path, required=True)
    args = p.parse_args(argv)

    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    args.out_dir.mkdir(parents=True, exist_ok=True)

    written = 0
    for fixture in manifest["fixtures"]:
        video_id = fixture["video_id"]
        raw_path = args.raw_dir / f"qwen35-omni_{video_id}.json"
        compact = build_compact_ref(video_id, fixture["category"], raw_path)
        if compact is None:
            continue
        out_path = args.out_dir / f"{video_id}.json"
        out_path.write_text(json.dumps(compact, ensure_ascii=False), encoding="utf-8")
        written += 1
        logger.info("wrote %s (%d lines)", out_path, len(compact["lines"]))

    logger.info(
        "done: %d compact reference file(s) written to %s", written, args.out_dir
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
