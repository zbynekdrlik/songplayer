"""Validate per-fixture judgment + per-run report schemas."""

import json
from pathlib import Path
from typing import Any

import jsonschema
import pytest
from jsonschema import Draft202012Validator

ROOT = Path(__file__).resolve().parents[1]
JUDGMENT_SCHEMA = ROOT / "schemas" / "judgment.schema.json"
REPORT_SCHEMA = ROOT / "schemas" / "report.schema.json"


@pytest.fixture(scope="module")
def judgment_schema() -> dict[str, Any]:
    return json.loads(JUDGMENT_SCHEMA.read_text(encoding="utf-8"))


@pytest.fixture(scope="module")
def report_validator() -> Draft202012Validator:
    """Build a validator that resolves the local $ref to judgment.schema.json.

    Uses the modern `referencing` API (RefResolver was deprecated in
    jsonschema 4.18). The judgment schema is registered under the
    bare filename so the report schema's `{"$ref": "judgment.schema.json"}`
    resolves cleanly.
    """
    from referencing import Registry, Resource

    judgment_schema = json.loads(JUDGMENT_SCHEMA.read_text(encoding="utf-8"))
    report_schema = json.loads(REPORT_SCHEMA.read_text(encoding="utf-8"))
    registry = Registry().with_resource(
        uri="judgment.schema.json",
        resource=Resource.from_contents(judgment_schema),
    )
    return Draft202012Validator(report_schema, registry=registry)


def test_valid_judgment_validates(judgment_schema: dict[str, Any]) -> None:
    sample = {
        "video_id": "BW_vUblj_RA",
        "score": 7,
        "verdict": "partial",
        "wer_estimate": 0.18,
        "line_timing_assessment": "median offset ~120 ms",
        "hallucination_count": 1,
        "hallucination_details": "Lines 12-23 are duplicate 'What's up?' cluster",
        "coverage_pct": 0.85,
        "wall_acceptable": False,
        "reasoning": "Most lines match within tolerance; hallucination cluster fails wall.",
        "judged_at": "2026-05-18T14:23:00Z",
        "judge_model": "claude-opus-4-7",
        "judge_prompt_revision": 1,
    }
    jsonschema.validate(sample, judgment_schema)


def test_judgment_score_must_be_0_to_10(judgment_schema: dict[str, Any]) -> None:
    sample = {
        "video_id": "BW_vUblj_RA",
        "score": 11,  # out of range
        "verdict": "match",
        "wer_estimate": 0.0,
        "line_timing_assessment": "ok",
        "hallucination_count": 0,
        "hallucination_details": "",
        "coverage_pct": 1.0,
        "wall_acceptable": True,
        "reasoning": "ok",
        "judged_at": "2026-05-18T14:23:00Z",
        "judge_model": "claude-opus-4-7",
        "judge_prompt_revision": 1,
    }
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(sample, judgment_schema)


def test_valid_report_validates(report_validator: Draft202012Validator) -> None:
    sample = {
        "run_id": "2026-05-18T14:30:00Z",
        "backend_id": "whisperx-large-v3",
        "backend_revision": 1,
        "judge_model": "claude-opus-4-7",
        "judge_prompt_revision": 1,
        "fixtures_run": 1,
        "fixtures_passed_wall": 0,
        "aggregate": {
            "mean_score": 7.0,
            "median_score": 7,
            "scores_by_category": {"dense_vocal": 7.0},
        },
        "per_fixture": [
            {
                "video_id": "BW_vUblj_RA",
                "score": 7,
                "verdict": "partial",
                "wer_estimate": 0.18,
                "line_timing_assessment": "median offset ~120 ms",
                "hallucination_count": 1,
                "hallucination_details": "cluster",
                "coverage_pct": 0.85,
                "wall_acceptable": False,
                "reasoning": "ok",
                "judged_at": "2026-05-18T14:23:00Z",
                "judge_model": "claude-opus-4-7",
                "judge_prompt_revision": 1,
            }
        ],
    }
    report_validator.validate(sample)


def test_invalid_verdict_rejected(judgment_schema: dict[str, Any]) -> None:
    sample = {
        "video_id": "BW_vUblj_RA",
        "score": 5,
        "verdict": "not_a_real_verdict",  # outside enum
        "wer_estimate": 0.0,
        "line_timing_assessment": "",
        "hallucination_count": 0,
        "hallucination_details": "",
        "coverage_pct": 1.0,
        "wall_acceptable": True,
        "reasoning": "ok",
        "judged_at": "2026-05-18T14:23:00Z",
        "judge_model": "claude-opus-4-7",
        "judge_prompt_revision": 1,
    }
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(sample, judgment_schema)


def test_invalid_judged_at_rejected(judgment_schema: dict[str, Any]) -> None:
    sample = {
        "video_id": "BW_vUblj_RA",
        "score": 5,
        "verdict": "match",
        "wer_estimate": 0.0,
        "line_timing_assessment": "",
        "hallucination_count": 0,
        "hallucination_details": "",
        "coverage_pct": 1.0,
        "wall_acceptable": True,
        "reasoning": "ok",
        "judged_at": "2026-05-18 14:23:00",  # missing T + Z
        "judge_model": "claude-opus-4-7",
        "judge_prompt_revision": 1,
    }
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(sample, judgment_schema)


def test_report_rejects_zero_fixtures_run(
    report_validator: Draft202012Validator,
) -> None:
    """schema.fixtures_run has minimum: 1 — a zero-fixture report is rejected."""
    sample = {
        "run_id": "2026-05-18T14:30:00Z",
        "backend_id": "whisperx-large-v3",
        "backend_revision": 1,
        "judge_model": "claude-opus-4-7",
        "judge_prompt_revision": 1,
        "fixtures_run": 0,
        "fixtures_passed_wall": 0,
        "aggregate": {
            "mean_score": 0.0,
            "median_score": 0,
            "scores_by_category": {},
        },
        "per_fixture": [
            {
                "video_id": "BW_vUblj_RA",
                "score": 0,
                "verdict": "miss",
                "wer_estimate": 1.0,
                "line_timing_assessment": "",
                "hallucination_count": 0,
                "hallucination_details": "",
                "coverage_pct": 0.0,
                "wall_acceptable": False,
                "reasoning": "no output",
                "judged_at": "2026-05-18T14:23:00Z",
                "judge_model": "claude-opus-4-7",
                "judge_prompt_revision": 1,
            }
        ],
    }
    with pytest.raises(jsonschema.ValidationError):
        report_validator.validate(sample)


def test_report_rejects_bad_per_fixture_score_via_ref(
    report_validator: Draft202012Validator,
) -> None:
    """The per_fixture $ref to judgment.schema.json must actually enforce
    per-fixture constraints — mutating score to 99 inside per_fixture[0]
    must be rejected via the resolved reference."""
    sample = {
        "run_id": "2026-05-18T14:30:00Z",
        "backend_id": "whisperx-large-v3",
        "backend_revision": 1,
        "judge_model": "claude-opus-4-7",
        "judge_prompt_revision": 1,
        "fixtures_run": 1,
        "fixtures_passed_wall": 0,
        "aggregate": {
            "mean_score": 7.0,
            "median_score": 7,
            "scores_by_category": {"dense_vocal": 7.0},
        },
        "per_fixture": [
            {
                "video_id": "BW_vUblj_RA",
                "score": 99,  # out of judgment-schema range 0-10
                "verdict": "match",
                "wer_estimate": 0.0,
                "line_timing_assessment": "",
                "hallucination_count": 0,
                "hallucination_details": "",
                "coverage_pct": 1.0,
                "wall_acceptable": True,
                "reasoning": "ok",
                "judged_at": "2026-05-18T14:23:00Z",
                "judge_model": "claude-opus-4-7",
                "judge_prompt_revision": 1,
            }
        ],
    }
    with pytest.raises(jsonschema.ValidationError):
        report_validator.validate(sample)


def test_report_accepts_all_fixtures_passed(
    report_validator: Draft202012Validator,
) -> None:
    """fixtures_passed_wall == fixtures_run is valid (all-pass run)."""
    sample = {
        "run_id": "2026-05-18T14:30:00Z",
        "backend_id": "whisperx-large-v3",
        "backend_revision": 1,
        "judge_model": "claude-opus-4-7",
        "judge_prompt_revision": 1,
        "fixtures_run": 1,
        "fixtures_passed_wall": 1,
        "aggregate": {
            "mean_score": 10.0,
            "median_score": 10,
            "scores_by_category": {"clean_pop": 10.0},
        },
        "per_fixture": [
            {
                "video_id": "BW_vUblj_RA",
                "score": 10,
                "verdict": "match",
                "wer_estimate": 0.0,
                "line_timing_assessment": "perfect",
                "hallucination_count": 0,
                "hallucination_details": "",
                "coverage_pct": 1.0,
                "wall_acceptable": True,
                "reasoning": "every line matches gold within tolerance",
                "judged_at": "2026-05-18T14:23:00Z",
                "judge_model": "claude-opus-4-7",
                "judge_prompt_revision": 1,
            }
        ],
    }
    report_validator.validate(sample)
