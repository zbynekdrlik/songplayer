"""Pure unit tests for eval/dubbing/run_round3.localize_manifest — re-pointing a
dev2-written manifest at the dev1 pulled-down line wavs. No audio/ffmpeg."""

from __future__ import annotations

import json

import pytest

from eval.dubbing import run_round3


def _write_manifest(tmp_path, indices):
    manifest = {
        "engine": "chatterbox_sk_clone",
        "lines": [
            {"index": i, "text": f"line {i}", "wav": f"/dev2/path/line_{i:03d}.wav"}
            for i in indices
        ],
    }
    mp = tmp_path / "manifest.json"
    mp.write_text(json.dumps(manifest), encoding="utf-8")
    return mp


def test_localize_repoints_every_line(tmp_path):
    indices = [2, 3, 4]
    mp = _write_manifest(tmp_path, indices)
    for i in indices:
        (tmp_path / f"line_{i:03d}.wav").write_bytes(b"RIFF")  # stand-in wav
    out = run_round3.localize_manifest(str(mp), str(tmp_path))
    for entry in out["lines"]:
        i = entry["index"]
        assert entry["wav"] == str(tmp_path / f"line_{i:03d}.wav")


def test_localize_missing_wav_raises(tmp_path):
    mp = _write_manifest(tmp_path, [2, 3])
    (tmp_path / "line_002.wav").write_bytes(b"RIFF")  # line 3 missing on purpose
    with pytest.raises(RuntimeError, match="missing line wav"):
        run_round3.localize_manifest(str(mp), str(tmp_path))
