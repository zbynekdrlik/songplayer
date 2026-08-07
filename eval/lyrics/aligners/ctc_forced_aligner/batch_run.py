#!/usr/bin/env python3
"""batch_run.py — orchestrates the full 22-fixture x2-config ctc-forced-
aligner shootout run on win-resolume (GPU box). Windows-side only (imports
`run.py` from the same directory to reuse ONE loaded model across all
fixtures in a config, instead of `run.py`'s own self-contained CLI reloading
the model on every invocation).

The poisoned fixture (Xvm4_fWkXe8 — qwen35-omni hallucinated a 395-line
degenerate repetition loop as "reference text" for a ~4.6 min song) is
processed LAST, in a SEPARATE subprocess (a fresh `run.py` CLI invocation)
with a generous hard wall-clock timeout, so a hang/crash/OOM on that one
fixture can never block or kill the other 21 in-process runs.

Usage (run ON win-resolume, inside the ctc_aligner_venv):
    python batch_run.py --text-refs-dir C:\\...\\text-refs ^
                         --wav-dir C:\\...\\eval-cache ^
                         --out-dir C:\\...\\eval-run\\out\\aligners
"""

from __future__ import annotations

import argparse
import json
import logging
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run as run_mod  # noqa: E402  (local import, see sys.path insert above)

logger = logging.getLogger("lyrics_eval.ctc_forced_aligner.batch_run")

POISONED_VIDEO_ID = "Xvm4_fWkXe8"
POISONED_TIMEOUT_S = 900  # 15 min — generous, per task brief ("10+ minutes")
CLI_STAR_FREQUENCIES = ["none", "segment"]  # matches run.py's --star-frequency choices


def set_below_normal_priority() -> None:
    try:
        import os

        import psutil

        psutil.Process(os.getpid()).nice(psutil.BELOW_NORMAL_PRIORITY_CLASS)
        logger.info("process priority set to BELOW_NORMAL")
    except Exception:
        logger.exception("could not set BELOW_NORMAL priority — continuing anyway")


def run_in_process(
    *,
    model,
    tokenizer,
    wav_path: Path,
    ref_lines: list[dict],
    cli_star_frequency: str,
    out_path: Path,
) -> dict:
    star_frequency = run_mod.cli_star_frequency_to_aligner(cli_star_frequency)
    t0 = time.time()
    try:
        lines_out, duration_ms, runtime_sec = run_mod.align_one_song(
            model=model,
            tokenizer=tokenizer,
            wav_path=wav_path,
            ref_lines=ref_lines,
            star_frequency=star_frequency,
        )
        error = None
    except Exception as exc:  # noqa: BLE001
        logger.exception(
            "in-process alignment failed: wav=%s star_frequency=%s",
            wav_path,
            star_frequency,
        )
        lines_out = run_mod.all_untimed_lines(ref_lines)
        runtime_sec = None
        error = f"{type(exc).__name__}: {exc}"
        try:
            duration_ms = run_mod.wav_duration_ms_fallback(wav_path)
        except Exception:
            duration_ms = 0
    payload = run_mod.build_payload(
        star_frequency=star_frequency,
        wav_path=wav_path,
        lines_out=lines_out,
        duration_ms=duration_ms,
        runtime_sec=runtime_sec,
        model_load_sec=None,  # amortized — model loaded once for the whole batch
        error=error,
    )
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(payload, ensure_ascii=False), encoding="utf-8")
    wall_sec = time.time() - t0
    return {
        "video_id": wav_path.stem,
        "star_frequency": cli_star_frequency,
        "error": error,
        "wall_sec": wall_sec,
    }


def run_poisoned_subprocess(
    *,
    wav_path: Path,
    text_json_path: Path,
    cli_star_frequency: str,
    out_path: Path,
    run_py_path: Path,
) -> dict:
    """Isolate the poisoned fixture in its OWN process with a hard timeout,
    so a hang/crash there can never take down the rest of the batch."""
    t0 = time.time()
    cmd = [
        sys.executable,
        str(run_py_path),
        "--wav",
        str(wav_path),
        "--text-json",
        str(text_json_path),
        "--out",
        str(out_path),
        "--star-frequency",
        cli_star_frequency,
    ]
    logger.info(
        "poisoned fixture: launching isolated subprocess (timeout=%ds): %s",
        POISONED_TIMEOUT_S,
        cmd,
    )

    def write_fallback(error_msg: str) -> None:
        """Shared by BOTH failure branches below (hard crash — e.g. an OS-level
        access violation from the C++ forced_align extension on a
        pathological target sequence — and a wall-clock timeout). Either way
        run.py's own process never got to (or never could) write valid
        output, so batch_run.py writes the same all-untimed fallback shape
        itself. Without this, a crashed/killed subprocess silently leaves
        NO output file at all for that (backend_id, video_id) pair —
        exactly the gap that let 2/44 files go missing on the first full
        run (2026-08-05)."""
        ref = json.loads(text_json_path.read_text(encoding="utf-8"))
        lines_out = run_mod.all_untimed_lines(ref.get("lines") or [])
        try:
            duration_ms = run_mod.wav_duration_ms_fallback(wav_path)
        except Exception:
            logger.exception(
                "could not read WAV duration for poisoned-fixture fallback either"
            )
            duration_ms = 0
        payload = run_mod.build_payload(
            star_frequency=run_mod.cli_star_frequency_to_aligner(cli_star_frequency),
            wav_path=wav_path,
            lines_out=lines_out,
            duration_ms=duration_ms,
            runtime_sec=None,
            model_load_sec=None,
            error=error_msg,
        )
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(json.dumps(payload, ensure_ascii=False), encoding="utf-8")

    try:
        proc = subprocess.run(
            cmd, capture_output=True, text=True, timeout=POISONED_TIMEOUT_S
        )
        wall_sec = time.time() - t0
        if proc.returncode != 0:
            logger.error(
                "poisoned fixture subprocess exited non-zero (%d) after %.1fs — writing all-untimed "
                "fallback; stderr tail:\n%s",
                proc.returncode,
                wall_sec,
                proc.stderr[-4000:],
            )
            write_fallback(
                f"subprocess exit code {proc.returncode} (0xC0000005=access violation in the C++ "
                f"forced_align extension is the observed crash mode on this fixture's degenerate "
                f"reference text) — killed/crashed before it could write its own output"
            )
        else:
            logger.info("poisoned fixture subprocess completed after %.1fs", wall_sec)
        return {
            "video_id": POISONED_VIDEO_ID,
            "star_frequency": cli_star_frequency,
            "error": None
            if proc.returncode == 0
            else f"subprocess exit code {proc.returncode}",
            "wall_sec": wall_sec,
            "timed_out": False,
        }
    except subprocess.TimeoutExpired:
        wall_sec = time.time() - t0
        logger.error(
            "poisoned fixture subprocess TIMED OUT after %.1fs — writing all-untimed fallback",
            wall_sec,
        )
        write_fallback(
            f"subprocess timed out after {POISONED_TIMEOUT_S}s (killed by batch_run.py)"
        )
        return {
            "video_id": POISONED_VIDEO_ID,
            "star_frequency": cli_star_frequency,
            "error": "timeout",
            "wall_sec": wall_sec,
            "timed_out": True,
        }


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(
        level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s"
    )
    set_below_normal_priority()

    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--text-refs-dir", type=Path, required=True)
    p.add_argument("--wav-dir", type=Path, required=True)
    p.add_argument("--out-dir", type=Path, required=True)
    p.add_argument("--device", default=None)
    args = p.parse_args(argv)

    import torch

    run_mod.ensure_ffmpeg_on_path()
    device = args.device or ("cuda" if torch.cuda.is_available() else "cpu")

    text_ref_files = sorted(args.text_refs_dir.glob("*.json"))
    video_ids = [f.stem for f in text_ref_files]
    normal_ids = [v for v in video_ids if v != POISONED_VIDEO_ID]
    logger.info(
        "batch_run starting: %d fixtures (%d normal + poisoned=%s) x %d star-frequency configs, device=%s",
        len(video_ids),
        len(normal_ids),
        POISONED_VIDEO_ID in video_ids,
        len(CLI_STAR_FREQUENCIES),
        device,
    )

    t_load0 = time.time()
    model, tokenizer = run_mod.load_model_with_retry(device)
    model_load_sec = time.time() - t_load0
    logger.info("model loaded once for the whole batch: %.1fs", model_load_sec)

    run_py_path = Path(__file__).resolve().parent / "run.py"
    results: list[dict] = []

    for cli_star_frequency in CLI_STAR_FREQUENCIES:
        backend_id = run_mod.backend_id_for(
            run_mod.cli_star_frequency_to_aligner(cli_star_frequency)
        )
        logger.info(
            "=== config: --star-frequency %s (backend_id=%s) ===",
            cli_star_frequency,
            backend_id,
        )

        for video_id in normal_ids:
            wav_path = args.wav_dir / f"{video_id}_vocal16k.wav"
            text_json_path = args.text_refs_dir / f"{video_id}.json"
            out_path = args.out_dir / f"{backend_id}_{video_id}.json"
            if not wav_path.exists():
                logger.error(
                    "missing wav for video_id=%s (%s) — skipping", video_id, wav_path
                )
                results.append(
                    {
                        "video_id": video_id,
                        "star_frequency": cli_star_frequency,
                        "error": "wav missing",
                        "wall_sec": 0.0,
                    }
                )
                continue
            ref = json.loads(text_json_path.read_text(encoding="utf-8"))
            r = run_in_process(
                model=model,
                tokenizer=tokenizer,
                wav_path=wav_path,
                ref_lines=ref.get("lines") or [],
                cli_star_frequency=cli_star_frequency,
                out_path=out_path,
            )
            logger.info(
                "  %-14s %-8s %.1fs error=%s",
                video_id,
                cli_star_frequency,
                r["wall_sec"],
                r["error"],
            )
            results.append(r)

        # Poisoned fixture LAST, isolated subprocess, own hard timeout.
        if POISONED_VIDEO_ID in video_ids:
            wav_path = args.wav_dir / f"{POISONED_VIDEO_ID}_vocal16k.wav"
            text_json_path = args.text_refs_dir / f"{POISONED_VIDEO_ID}.json"
            out_path = args.out_dir / f"{backend_id}_{POISONED_VIDEO_ID}.json"
            r = run_poisoned_subprocess(
                wav_path=wav_path,
                text_json_path=text_json_path,
                cli_star_frequency=cli_star_frequency,
                out_path=out_path,
                run_py_path=run_py_path,
            )
            logger.info(
                "  %-14s %-8s %.1fs error=%s timed_out=%s",
                POISONED_VIDEO_ID,
                cli_star_frequency,
                r["wall_sec"],
                r["error"],
                r.get("timed_out"),
            )
            results.append(r)

    n_ok = sum(1 for r in results if r["error"] is None)
    n_err = len(results) - n_ok
    total_wall = sum(r["wall_sec"] for r in results)
    logger.info(
        "batch_run done: %d/%d ok, %d error(s), model_load=%.1fs, total_align_wall=%.1fs",
        n_ok,
        len(results),
        n_err,
        model_load_sec,
        total_wall,
    )
    summary_path = args.out_dir / "_batch_run_summary.json"
    summary_path.write_text(
        json.dumps(
            {
                "device": device,
                "model_load_sec": round(model_load_sec, 1),
                "results": results,
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )
    logger.info("summary written to %s", summary_path)
    return 0 if n_err == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
