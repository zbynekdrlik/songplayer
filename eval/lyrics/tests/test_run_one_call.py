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
    assert summary == {
        "written": 3,
        "skipped": 0,
        "errors": 0,
        "missing_input": 0,
        "exhausted": 0,
    }


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
    assert summary == {
        "written": 2,
        "skipped": 0,
        "errors": 1,
        "missing_input": 0,
        "exhausted": 0,
    }


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
    assert summary == {
        "written": 1,
        "skipped": 1,
        "errors": 0,
        "missing_input": 0,
        "exhausted": 0,
    }

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


# ── review round 1 (#144): window-error retries, missing inputs, real wiring ─


def test_run_all_retries_a_row_with_window_errors(tmp_path: Path) -> None:
    """A win60 row that kept its good windows but lost one (429/5xx) is NOT
    done: a re-run must retry it, or ~55 s of lines vanish silently."""
    manifest = _manifest(tmp_path, ["v1"])
    raw = tmp_path / "raw"
    raw.mkdir()
    partial = _row("gemini38-flash-win60", "v1")
    partial["metadata"] = {"n_window_errors": 1}
    (raw / "gemini38-flash-win60_v1.json").write_text(
        json.dumps(partial), encoding="utf-8"
    )
    seen: list[str] = []

    def run_one(video_id: str) -> dict:
        seen.append(video_id)
        return _row("gemini38-flash-win60", video_id)

    run_one_call.run_all(
        manifest_path=manifest, mode="win60", raw_dir=raw, run_one=run_one, force=False
    )
    assert seen == ["v1"]


def test_run_all_counts_missing_input_separately(tmp_path: Path) -> None:
    manifest = _manifest(tmp_path, ["v1", "v2"])

    def run_one(video_id: str) -> dict:
        row = _row("gemini38-flash-whole", video_id, error="FileNotFoundError: x")
        if video_id == "v1":
            row["metadata"] = {"error_kind": "missing_input"}
        return row

    summary = run_one_call.run_all(
        manifest_path=manifest,
        mode="whole",
        raw_dir=tmp_path / "raw",
        run_one=run_one,
        force=False,
    )
    assert summary == {
        "written": 2,
        "skipped": 0,
        "errors": 2,
        "missing_input": 1,
        "exhausted": 0,
    }


def test_run_all_records_attempts_and_gives_up_after_the_cap(tmp_path: Path) -> None:
    """A deterministic failure (e.g. a RECITATION block on one clip) must not
    keep the re-run loop alive forever: attempts are recorded and a fixture
    that failed MAX_ATTEMPTS times is left as is (still visible in scoring)."""
    manifest = _manifest(tmp_path, ["v1"])
    raw = tmp_path / "raw"
    calls: list[str] = []

    def run_one(video_id: str) -> dict:
        calls.append(video_id)
        return _row("gemini38-flash-whole", video_id, error="RECITATION")

    for _ in range(run_one_call.MAX_ATTEMPTS + 2):
        summary = run_one_call.run_all(
            manifest_path=manifest,
            mode="whole",
            raw_dir=raw,
            run_one=run_one,
            force=False,
        )
    assert len(calls) == run_one_call.MAX_ATTEMPTS
    data = json.loads((raw / "gemini38-flash-whole_v1.json").read_text("utf-8"))
    assert data["metadata"]["attempt"] == run_one_call.MAX_ATTEMPTS
    assert summary == {
        "written": 0,
        "skipped": 0,
        "errors": 0,
        "missing_input": 0,
        "exhausted": 1,
    }
    assert run_one_call.exit_code(summary) == 0

    # --force re-runs it anyway
    run_one_call.run_all(
        manifest_path=manifest, mode="whole", raw_dir=raw, run_one=run_one, force=True
    )
    assert len(calls) == run_one_call.MAX_ATTEMPTS + 1


def test_exit_code_ignores_missing_inputs() -> None:
    assert run_one_call.exit_code({"errors": 2, "missing_input": 2}) == 0
    assert run_one_call.exit_code({"errors": 3, "missing_input": 2}) == 1
    assert run_one_call.exit_code({"errors": 0, "missing_input": 0}) == 0


def test_run_all_through_the_real_run_fixture(tmp_path: Path) -> None:
    """End-to-end wiring: run_all -> gemini38_flash.run_fixture with only the
    external seams faked (model call + ffmpeg), incl. a missing WAV."""
    import wave

    from eval.lyrics.backends import gemini38_flash as g38

    cache = tmp_path / "cache"
    cache.mkdir()
    with wave.open(str(cache / "v1_vocal16k.wav"), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(16_000)
        w.writeframes(b"\x00\x00" * 16_000 * 5)
    manifest = _manifest(tmp_path, ["v1", "v2"])  # v2 has no WAV
    raw = tmp_path / "raw"

    def caller(path: Path) -> g38.CallResult:
        return g38.CallResult(
            text=json.dumps(
                {"lines": [{"text": "a", "start_ms": 5, "end_ms": 9, "text_sk": "b"}]}
            ),
            finish_reason="STOP",
            usage=None,
        )

    def run_one(video_id: str) -> dict:
        return g38.run_fixture(
            video_id=video_id,
            mode="whole",
            audio=g38.default_audio_path(cache, video_id),
            caller=caller,
            slicer=None,
            work_dir=tmp_path / "clips",
            model="m",
        )

    summary = run_one_call.run_all(
        manifest_path=manifest, mode="whole", raw_dir=raw, run_one=run_one, force=False
    )
    assert summary == {
        "written": 2,
        "skipped": 0,
        "errors": 1,
        "missing_input": 1,
        "exhausted": 0,
    }
    ok = json.loads((raw / "gemini38-flash-whole_v1.json").read_text("utf-8"))
    assert ok["error"] is None and ok["lines"][0]["start_ms"] == 5
    bad = json.loads((raw / "gemini38-flash-whole_v2.json").read_text("utf-8"))
    assert bad["metadata"]["error_kind"] == "missing_input"
