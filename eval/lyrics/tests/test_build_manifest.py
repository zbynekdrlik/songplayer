"""Tests for build_manifest.py (no live HTTP)."""

import json
from pathlib import Path

import pytest

from eval.lyrics import build_manifest


def test_filter_by_gold_source_keeps_only_allowed() -> None:
    songs = [
        {"youtube_id": "aaaaaaaaaaa", "lyrics_source": "lrclib_synced", "lines": ["x"]},
        {"youtube_id": "bbbbbbbbbbb", "lyrics_source": "description", "lines": ["y"]},
        {"youtube_id": "ccccccccccc", "lyrics_source": "spotify_proxy", "lines": ["z"]},
        {"youtube_id": "ddddddddddd", "lyrics_source": "asr_gap", "lines": []},
        {
            "youtube_id": "eeeeeeeeeee",
            "lyrics_source": "yt_subs_manual",
            "lines": ["q"],
        },
    ]
    out = build_manifest.filter_by_gold_source(songs)
    ids = [s["youtube_id"] for s in out]
    assert ids == ["aaaaaaaaaaa", "ccccccccccc", "eeeeeeeeeee"]


def test_to_manifest_entry_shapes_one_fixture() -> None:
    song = {
        "youtube_id": "BW_vUblj_RA",
        "lyrics_source": "lrclib_synced",
        "lines": [
            {"text": "Line one", "start_ms": 1000, "end_ms": 2500},
            {"text": "Line two", "start_ms": 2600, "end_ms": 4000},
        ],
    }
    entry = build_manifest.to_manifest_entry(song, category="dense_vocal", notes="Test")
    assert entry["video_id"] == "BW_vUblj_RA"
    assert entry["category"] == "dense_vocal"
    assert entry["gold_source"] == "lrclib_synced"
    assert entry["gold_lines"] == [
        {"text": "Line one", "start_ms": 1000, "end_ms": 2500},
        {"text": "Line two", "start_ms": 2600, "end_ms": 4000},
    ]
    assert entry["notes"] == "Test"


def test_to_manifest_entry_omits_notes_when_empty() -> None:
    song = {
        "youtube_id": "BW_vUblj_RA",
        "lyrics_source": "lrclib_synced",
        "lines": [{"text": "x", "start_ms": 0, "end_ms": 1000}],
    }
    entry = build_manifest.to_manifest_entry(song, category="dense_vocal", notes="")
    assert "notes" not in entry


def test_write_manifest_validates_schema(tmp_path: Path) -> None:
    out = tmp_path / "manifest.json"
    fixtures = [
        {
            "video_id": "BW_vUblj_RA",
            "category": "dense_vocal",
            "gold_source": "lrclib_synced",
            "gold_lines": [{"text": "x", "start_ms": 0, "end_ms": 1000}],
        }
    ]
    build_manifest.write_manifest(fixtures, out)
    data = json.loads(out.read_text(encoding="utf-8"))
    assert data["version"] == 1
    assert len(data["fixtures"]) == 1
    assert data["fixtures"][0]["video_id"] == "BW_vUblj_RA"


def test_write_manifest_passes_real_schema_validation(tmp_path: Path) -> None:
    """Round-trip: write_manifest output validates against the live schema."""
    import jsonschema

    schema_path = (
        Path(__file__).resolve().parents[1] / "schemas" / "manifest.schema.json"
    )
    schema = json.loads(schema_path.read_text(encoding="utf-8"))

    out = tmp_path / "manifest.json"
    fixtures = [
        {
            "video_id": "BW_vUblj_RA",
            "category": "dense_vocal",
            "gold_source": "lrclib_synced",
            "gold_lines": [{"text": "hello world", "start_ms": 100, "end_ms": 800}],
        }
    ]
    build_manifest.write_manifest(fixtures, out)
    data = json.loads(out.read_text(encoding="utf-8"))
    jsonschema.validate(data, schema)


def test_fetch_songs_uses_correct_endpoint(monkeypatch: pytest.MonkeyPatch) -> None:
    """fetch_songs hits GET /api/v1/songs and returns the list."""
    captured: dict = {}

    class FakeResp:
        def __init__(self, payload: dict) -> None:
            self._payload = payload

        def json(self) -> dict:
            return self._payload

        def raise_for_status(self) -> None:
            return None

    def fake_get(url: str, timeout: int) -> FakeResp:  # noqa: ARG001
        captured["url"] = url
        return FakeResp({"songs": [{"youtube_id": "aaaaaaaaaaa"}]})

    monkeypatch.setattr(build_manifest.requests, "get", fake_get)
    out = build_manifest.fetch_songs("http://example.invalid")
    assert captured["url"] == "http://example.invalid/api/v1/songs"
    assert out == [{"youtube_id": "aaaaaaaaaaa"}]
