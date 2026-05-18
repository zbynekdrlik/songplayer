"""Validate the fixture manifest against its JSONSchema."""

import json
from pathlib import Path

import jsonschema
import pytest

ROOT = Path(__file__).resolve().parents[1]
SCHEMA_PATH = ROOT / "schemas" / "manifest.schema.json"
MANIFEST_PATH = ROOT / "manifest.json"


@pytest.fixture(scope="module")
def schema() -> dict:
    return json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))


def test_manifest_file_exists() -> None:
    assert MANIFEST_PATH.exists(), f"missing manifest at {MANIFEST_PATH}"


def test_manifest_validates_against_schema(schema: dict) -> None:
    manifest = json.loads(MANIFEST_PATH.read_text(encoding="utf-8"))
    jsonschema.validate(manifest, schema)


def test_manifest_has_version_field(schema: dict) -> None:
    manifest = json.loads(MANIFEST_PATH.read_text(encoding="utf-8"))
    assert manifest.get("version") == 1


def test_invalid_manifest_rejected(schema: dict) -> None:
    bad = {"version": 1, "fixtures": [{"video_id": "x", "category": "not_in_enum"}]}
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(bad, schema)
