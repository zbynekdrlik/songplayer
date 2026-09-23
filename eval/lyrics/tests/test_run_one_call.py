"""Tests for run_one_call.py — the #144 loop over the CURRENT manifest that
writes one raw file per fixture (`<label>_<video_id>.json`, the shape
`score_one_call.load_produced` reads), never aborting the whole run on one
fixture's failure."""

from __future__ import annotations

import json
from pathlib import Path

from eval.lyrics import run_one_call, score_one_call


def _manifest(tmp_path: Path, ids: list[str]) -> Path:
    path = tmp_path / "manifest.json"
    fixtures = [
        {
            "video_id": vid,
            "category": "clean_pop",
            "gold_source": "lrclib",
            "gold_lines": [{"text": "a", "start_ms": 0, "end_ms": 1}],
        }
        for vid in ids
    ]
    path.write_text(json.dumps({"version": 1, "fixtures": fixtures}), encoding="utf-8")
    return path


def _row(label: str, vid: str, error: str | None = None) -> dict:
    return {
        "backend_id": label,
        "video_id": vid,
        "lines": []
        if error
        else [{"text": "a", "start_ms": 5, "end_ms": 9, "text_sk": "b"}],
        "error": error,
        "metadata": {},
    }


def test_run_all_writes_one_file_per_fixture_in_manifest_order(tmp_path: Path) -> None:
    manifest = _manifest(tmp_path, ["v1", "v2", "v3"])
    raw = tmp_path / "raw"
    seen: list[str] = []

    def run_one(video_id: str) -> dict:
        seen.append(video_id)
        return _row("gemini38-flash-whole", video_id)

    summary = run_one_call.run_all(
        manifest_path=manifest, mode="whole", raw_dir=raw, run_one=run_one, force=False
    )
    assert seen == ["v1", "v2", "v3"]
    for vid in seen:
        data = json.loads((raw / f"gemini38-flash-whole_{vid}.json").read_text("utf-8"))
        assert data["video_id"] == vid
    # the scorer reads exactly these files
    assert score_one_call.load_produced(raw, "gemini38-flash-whole", "v2") is not None
    assert summary == {"written": 3, "skipped": 0, "errors": 0}


def test_run_all_exception_becomes_error_row_and_loop_continues(tmp_path: Path) -> None:
    manifest = _manifest(tmp_path, ["v1", "v2"])
    raw = tmp_path / "raw"

    def run_one(video_id: str) -> dict:
        if video_id == "v1":
            raise RuntimeError("ffmpeg exploded")
        return _row("gemini38-flash-win60", video_id)

    summary = run_one_call.run_all(
        manifest_path=manifest, mode="win60", raw_dir=raw, run_one=run_one, force=False
    )
    bad = json.loads((raw / "gemini38-flash-win60_v1.json").read_text("utf-8"))
    assert "ffmpeg exploded" in bad["error"]
    assert bad["lines"] == []
    assert (raw / "gemini38-flash-win60_v2.json").exists()
    assert summary == {"written": 2, "skipped": 0, "errors": 1}


def test_run_all_skips_existing_ok_output_unless_forced(tmp_path: Path) -> None:
    manifest = _manifest(tmp_path, ["v1", "v2"])
    raw = tmp_path / "raw"
    raw.mkdir()
    (raw / "gemini38-flash-whole_v1.json").write_text(
        json.dumps(_row("gemini38-flash-whole", "v1")), encoding="utf-8"
    )
    # an earlier ERROR row is retried (a 429 mid-run must not freeze a gap)
    (raw / "gemini38-flash-whole_v2.json").write_text(
        json.dumps(_row("gemini38-flash-whole", "v2", error="429")), encoding="utf-8"
    )
    seen: list[str] = []

    def run_one(video_id: str) -> dict:
        seen.append(video_id)
        return _row("gemini38-flash-whole", video_id)

    summary = run_one_call.run_all(
        manifest_path=manifest, mode="whole", raw_dir=raw, run_one=run_one, force=False
    )
    assert seen == ["v2"]
    assert summary == {"written": 1, "skipped": 1, "errors": 0}

    seen.clear()
    run_one_call.run_all(
        manifest_path=manifest, mode="whole", raw_dir=raw, run_one=run_one, force=True
    )
    assert seen == ["v1", "v2"]


def test_run_all_only_filter(tmp_path: Path) -> None:
    manifest = _manifest(tmp_path, ["v1", "v2", "v3"])
    seen: list[str] = []

    def run_one(video_id: str) -> dict:
        seen.append(video_id)
        return _row("gemini38-flash-whole", video_id)

    run_one_call.run_all(
        manifest_path=manifest,
        mode="whole",
        raw_dir=tmp_path / "raw",
        run_one=run_one,
        force=False,
        only=["v3"],
    )
    assert seen == ["v3"]
