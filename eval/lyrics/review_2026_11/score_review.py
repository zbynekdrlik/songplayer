#!/usr/bin/env python3
"""score_review.py — score the #349 November-2026 review (dev side, repo root).

Every backend goes through the unchanged ``score_aligner.py`` so its numbers
pool with 2026-08-05 / 2026-09-12. Two views of the same raw files:

  - ``common``: only the 17 fixtures the production pair has raw output for
    (``vpwDdb8r9Bk`` / ``fHYLw-2tTx4`` had no audio on win-resolume in
    August/September, so mtl and g35t errored there). Their files are left
    out here, so they count as errored for EVERY backend, gold kept in the
    1414-line denominator — the comparable table.
  - ``all``: every file the dev1 run produced (dev1 could download those two
    songs today), for the ASR backends' own reading.

The production baselines are re-scored from their committed raw files on the
same manifest. Output: one JSON with ``{view: {backend: report}}``.
"""

from __future__ import annotations

import argparse
import json
import sys
import tempfile
from pathlib import Path

from eval.lyrics import score_aligner

REPORTS = Path(__file__).resolve().parents[1] / "reports"
RAW = REPORTS / "2026-11-21-raw"
NO_BASELINE = ("vpwDdb8r9Bk", "fHYLw-2tTx4")
BASELINES = {
    "lyrics-alignment-mtl": REPORTS / "2026-08-05-aligner-raw",
    "gemini-3-5-transcribe": REPORTS / "2026-09-12-raw",
}


def score(raw_dir: Path, backends: list[str]) -> dict:
    with tempfile.TemporaryDirectory() as tmp:
        out = Path(tmp) / "s.json"
        args = ["--raw-dir", str(raw_dir), "--backends", *backends, "--out", str(out)]
        rc = score_aligner.main(args)
        if rc not in (0, None):
            raise RuntimeError(f"score_aligner exit {rc} for {backends}")
        return json.loads(out.read_text())


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--backends", nargs="+", required=True)
    ap.add_argument("--out", type=Path, required=True)
    args = ap.parse_args(argv)

    result: dict = {"baselines": {}, "common": {}, "all": {}}
    for bid, raw in BASELINES.items():
        result["baselines"].update(score(raw, [bid]))
    result["all"] = score(RAW, args.backends)
    with tempfile.TemporaryDirectory() as tmp:
        common = Path(tmp)
        for f in RAW.glob("*.json"):
            if not f.stem.endswith(NO_BASELINE):
                (common / f.name).symlink_to(f)
        result["common"] = score(common, args.backends)
    args.out.write_text(json.dumps(result, indent=1, ensure_ascii=False))
    print(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
