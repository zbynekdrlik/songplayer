"""Tests for build_manifest.py (no live HTTP)."""

import json
from pathlib import Path
from typing import Any

import pytest

from eval.lyrics import build_manifest


def test_filter_by_gold_source_keeps_only_allowed_and_has_lyrics() -> None:
    songs = [
        {"youtube_id": "aaaaaaaaaaa", "source": "lrclib", "has_lyrics": True},
        {"youtube_id": "bbbbbbbbbbb", "source": "description", "has_lyrics": True},
        {"youtube_id": "ccccccccccc", "source": "spotify", "has_lyrics": True},
        {"youtube_id": "ddddddddddd", "source": "asr_gap", "has_lyrics": False},
        {"youtube_id": "eeeeeeeeeee", "source": "yt_subs", "has_lyrics": True},
        # has_lyrics false even though source is allowed → excluded
        {"youtube_id": "fffffffffff", "source": "lrclib", "has_lyrics": False},
        # compound source containing whisperx → excluded (we want pure gold)
        {
            "youtube_id": "ggggggggggg",
            "source": "lrclib+timed-merge+whisperx-large-v3@rev1",
            "has_lyrics": True,
        },
    ]
    out = build_manifest.filter_by_gold_source(songs)
    ids = [s["youtube_id"] for s in out]
    assert ids == ["aaaaaaaaaaa", "ccccccccccc", "eeeeeeeeeee"]


def test_to_manifest_entry_shapes_one_fixture() -> None:
    list_item = {
        "youtube_id": "BW_vUblj_RA",
        "source": "lrclib",
        "video_id": 42,
    }
    lyrics_json: dict[str, Any] = {
        "version": 19,
        "source": "lrclib",
        "lines": [
            {"start_ms": 1000, "end_ms": 2500, "en": "Line one", "sk": "Riadok jedna"},
            {"start_ms": 2600, "end_ms": 4000, "en": "Line two", "words": None},
        ],
    }
    entry = build_manifest.to_manifest_entry(
        list_item, lyrics_json=lyrics_json, category="dense_vocal", notes="Test"
    )
    assert entry["video_id"] == "BW_vUblj_RA"
    assert entry["category"] == "dense_vocal"
    assert entry["gold_source"] == "lrclib"
    assert entry["gold_lines"] == [
        {"text": "Line one", "start_ms": 1000, "end_ms": 2500},
        {"text": "Line two", "start_ms": 2600, "end_ms": 4000},
    ]
    assert entry["notes"] == "Test"


def test_to_manifest_entry_skips_empty_en_lines() -> None:
    list_item = {"youtube_id": "BW_vUblj_RA", "source": "lrclib", "video_id": 1}
    lyrics_json: dict[str, Any] = {
        "lines": [
            {"start_ms": 0, "end_ms": 500, "en": ""},
            {"start_ms": 600, "end_ms": 1200, "en": "Real line"},
            {"start_ms": 1300, "end_ms": 1800, "en": "   "},
        ],
    }
    entry = build_manifest.to_manifest_entry(
        list_item, lyrics_json=lyrics_json, category="dense_vocal"
    )
    assert len(entry["gold_lines"]) == 1
    assert entry["gold_lines"][0]["text"] == "Real line"


def test_to_manifest_entry_omits_notes_when_empty() -> None:
    list_item = {"youtube_id": "BW_vUblj_RA", "source": "lrclib", "video_id": 1}
    lyrics_json: dict[str, Any] = {
        "lines": [{"start_ms": 0, "end_ms": 1000, "en": "x"}],
    }
    entry = build_manifest.to_manifest_entry(
        list_item, lyrics_json=lyrics_json, category="dense_vocal", notes=""
    )
    assert "notes" not in entry


def test_write_manifest_validates_schema(tmp_path: Path) -> None:
    out = tmp_path / "manifest.json"
    fixtures = [
        {
            "video_id": "BW_vUblj_RA",
            "category": "dense_vocal",
            "gold_source": "lrclib",
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
            "gold_source": "lrclib",
            "gold_lines": [{"text": "hello world", "start_ms": 100, "end_ms": 800}],
        }
    ]
    build_manifest.write_manifest(fixtures, out)
    data = json.loads(out.read_text(encoding="utf-8"))
    jsonschema.validate(data, schema)


def test_fetch_song_list_uses_correct_endpoint(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captured: dict[str, Any] = {}

    class FakeResp:
        def __init__(self, payload: Any) -> None:
            self._payload = payload

        def json(self) -> Any:
            return self._payload

        def raise_for_status(self) -> None:
            return None

    def fake_get(url: str, **kwargs: Any) -> FakeResp:  # noqa: ARG001
        captured["url"] = url
        return FakeResp(
            [
                {
                    "youtube_id": "aaaaaaaaaaa",
                    "video_id": 1,
                    "source": "lrclib",
                    "has_lyrics": True,
                }
            ]
        )

    monkeypatch.setattr(build_manifest.requests, "get", fake_get)
    out = build_manifest.fetch_song_list("http://example.invalid")
    assert captured["url"] == "http://example.invalid/api/v1/lyrics/songs"
    assert isinstance(out, list)
    assert out[0]["youtube_id"] == "aaaaaaaaaaa"


def test_fetch_song_list_rejects_non_list_response(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    class FakeResp:
        def json(self) -> Any:
            return {"songs": []}  # wrong shape — should be a bare list

        def raise_for_status(self) -> None:
            return None

    def fake_get(url: str, **kwargs: Any) -> FakeResp:  # noqa: ARG001
        return FakeResp()

    monkeypatch.setattr(build_manifest.requests, "get", fake_get)
    with pytest.raises(RuntimeError, match="unexpected response shape"):
        build_manifest.fetch_song_list("http://example.invalid")


def test_fetch_song_detail_uses_correct_endpoint(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captured: dict[str, Any] = {}

    class FakeResp:
        def json(self) -> Any:
            return {"list_item": {}, "lyrics_json": {"lines": []}}

        def raise_for_status(self) -> None:
            return None

    def fake_get(url: str, **kwargs: Any) -> FakeResp:  # noqa: ARG001
        captured["url"] = url
        return FakeResp()

    monkeypatch.setattr(build_manifest.requests, "get", fake_get)
    out = build_manifest.fetch_song_detail("http://example.invalid", 42)
    assert captured["url"] == "http://example.invalid/api/v1/lyrics/songs/42"
    assert "lyrics_json" in out
