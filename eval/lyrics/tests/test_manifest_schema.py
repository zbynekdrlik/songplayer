"""Validate the fixture manifest against its JSONSchema."""

import json
from pathlib import Path
from typing import Any

import jsonschema
import pytest

ROOT = Path(__file__).resolve().parents[1]
SCHEMA_PATH = ROOT / "schemas" / "manifest.schema.json"
MANIFEST_PATH = ROOT / "manifest.json"


@pytest.fixture(scope="module")
def schema() -> dict[str, Any]:
    return json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))


def test_manifest_file_exists() -> None:
    assert MANIFEST_PATH.exists(), f"missing manifest at {MANIFEST_PATH}"


def test_manifest_validates_against_schema(schema: dict[str, Any]) -> None:
    manifest = json.loads(MANIFEST_PATH.read_text(encoding="utf-8"))
    jsonschema.validate(manifest, schema)


def test_manifest_gold_lines_have_monotonic_timing() -> None:
    manifest = json.loads(MANIFEST_PATH.read_text(encoding="utf-8"))
    for f_idx, fixture in enumerate(manifest["fixtures"]):
        for l_idx, line in enumerate(fixture["gold_lines"]):
            assert line["end_ms"] > line["start_ms"], (
                f"fixture[{f_idx}] '{fixture['video_id']}' line[{l_idx}] "
                f"has end_ms ({line['end_ms']}) <= start_ms ({line['start_ms']})"
            )


def test_invalid_manifest_rejected_for_bad_category(schema: dict[str, Any]) -> None:
    bad = {
        "version": 1,
        "fixtures": [
            {
                "video_id": "BW_vUblj_RA",
                "category": "not_in_enum",
                "gold_source": "lrclib_synced",
                "gold_lines": [{"text": "hello", "start_ms": 0, "end_ms": 1000}],
            }
        ],
    }
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(bad, schema)
