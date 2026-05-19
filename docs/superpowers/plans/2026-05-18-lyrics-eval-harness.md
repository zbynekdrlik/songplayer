# Lyrics Eval Harness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a Claude-orchestrated `/lyrics-eval` slash command + supporting Python infrastructure that scores any lyrics-alignment backend against a pinned fixture set, with Claude as judge and reports committed to repo. PR1 closes issue #110.

**Architecture:** Repo-scoped skill at `.claude/skills/lyrics-eval/SKILL.md` + Python tools under `eval/lyrics/` (manifest, audio prep, per-backend caller, judge prompt, reports). Zero new Rust. Backends are throwaway-iteration Python files Claude can edit live; the production Rust `AlignmentBackend` impl is untouched. Reports + champion tracking committed to repo for full history.

**Tech Stack:** Python 3.11+ (already on win-resolume via `crates/sp-server/src/lyrics/bootstrap.rs`), `audio-separator[gpu]` (already installed), `replicate` Python SDK, `requests`, `pytest`, `ruff`. GitHub Actions Ubuntu runner adds Python steps; win-resolume self-hosted runner provides GPU for first eval run.

**Design spec:** `docs/superpowers/specs/2026-05-18-lyrics-eval-harness-design.md` (commits `96cdf2a` + `b0682b6`).

**Related GH issues:** Closes #110. Sequenced ahead of #111 + #112. Will file a follow-up issue during Task 11 for `/lyrics-research`.

---

## File Structure

Files this plan creates or modifies:

**Skill:**
- Create: `.claude/skills/lyrics-eval/SKILL.md` — slash-command instructions Claude follows

**Python eval tools:**
- Create: `eval/lyrics/README.md` — directory orientation
- Create: `eval/lyrics/manifest.json` — fixture list, built once from production DB, committed
- Create: `eval/lyrics/build_manifest.py` — one-shot DB-to-manifest helper (HTTP API)
- Create: `eval/lyrics/audio_prep.py` — yt-dlp + Mel-Roformer + anvuew dereverb on win-resolume
- Create: `eval/lyrics/backends/whisperx_replicate.py` — baseline backend caller
- Create: `eval/lyrics/judge_prompt.md` — versioned judge prompt template
- Create: `eval/lyrics/schemas/manifest.schema.json` — JSONSchema for manifest
- Create: `eval/lyrics/schemas/report.schema.json` — JSONSchema for per-run report
- Create: `eval/lyrics/schemas/judgment.schema.json` — JSONSchema for per-fixture judgment

**Reports (initial):**
- Create: `eval/lyrics/reports/CHAMPION.md` — initial champion (whisperx-large-v3 rev 1)
- Create: `eval/lyrics/reports/history.md` — empty append-only log header
- Create: `eval/lyrics/reports/2026-05-18-whisperx.json` + `.md` — first eval run (produced by Task 10, committed in Task 11)

**Tests:**
- Create: `eval/lyrics/tests/__init__.py`
- Create: `eval/lyrics/tests/conftest.py` — shared fixtures
- Create: `eval/lyrics/tests/test_manifest_schema.py`
- Create: `eval/lyrics/tests/test_report_schema.py`
- Create: `eval/lyrics/tests/test_build_manifest.py`
- Create: `eval/lyrics/tests/test_whisperx_backend.py`

**CI:**
- Modify: `.github/workflows/ci.yml` — add `eval-checks` job + add it to gate's needs list

**Gitignore:**
- Modify: `.gitignore` — add `eval/lyrics/.eval-cache/` (local-machine cache scratch)

No Rust files modified. No `Cargo.toml` modified. No `VERSION` bump (dev is already `0.43.0-dev.1` ahead of main's `0.42.0`).

---

### Task 1: Scaffold `eval/lyrics/` directory + CI integration

**Files:**
- Create: `eval/lyrics/.gitkeep` (placeholder), `eval/lyrics/tests/__init__.py`
- Modify: `.gitignore`
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Create the directory skeleton**

```bash
mkdir -p eval/lyrics/{backends,schemas,reports,tests}
touch eval/lyrics/tests/__init__.py
```

- [ ] **Step 2: Add cache exclusion to `.gitignore`**

Append to `/home/newlevel/devel/songplayer/.gitignore`:

```
# Lyrics eval local cache (vocal stems, downloaded audio)
eval/lyrics/.eval-cache/
```

- [ ] **Step 3: Add `eval-checks` job to `.github/workflows/ci.yml`**

Insert after the existing `security` job (around line 110) and before `build-wasm`:

```yaml
  eval-checks:
    name: Eval Checks (ruff + pytest)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with:
          python-version: "3.11"
      - name: Install ruff + pytest + jsonschema
        run: pip install ruff==0.5.7 pytest==8.3.2 jsonschema==4.23.0 requests==2.32.3
      - name: Lint with ruff
        run: ruff check eval/
      - name: Format check with ruff
        run: ruff format --check eval/
      - name: Run pytest
        run: pytest eval/lyrics/tests -v
```

- [ ] **Step 4: Add `eval-checks` to the gate job**

In `.github/workflows/ci.yml`, find the `gate` / `all-jobs-pass` job (around line 613). Add `eval-checks` to its `needs:` list and to the result-collection loop (around line 632).

The loop becomes:

```yaml
          for job in lint test test-wasm security eval-checks build-windows build-wasm build-tauri frontend-e2e test-integrity file-size coverage; do
```

- [ ] **Step 5: Verify the workflow file parses**

```bash
python -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))"
```

Expected: no output (parse OK). On error, fix the YAML and re-run.

- [ ] **Step 6: Commit**

```bash
git add .gitignore .github/workflows/ci.yml eval/
git commit -m "$(cat <<'EOF'
chore(eval): scaffold eval/lyrics/ directory + CI eval-checks job

First task of #110 plan. Adds directory skeleton, gitignore for
local cache, and a new CI job running ruff + pytest on eval/.
Gate job gains eval-checks as a required check. No Python files yet
so ruff has nothing to lint and pytest has nothing to run — both
should exit zero on this commit.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Manifest JSON schema + validator test (RED-GREEN)

**Files:**
- Create: `eval/lyrics/schemas/manifest.schema.json`
- Create: `eval/lyrics/manifest.json`
- Create: `eval/lyrics/tests/test_manifest_schema.py`

- [ ] **Step 1: Write the failing test**

Create `eval/lyrics/tests/test_manifest_schema.py`:

```python
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
```

- [ ] **Step 2: Run pytest, expect FAIL**

```bash
cd /home/newlevel/devel/songplayer
pip install jsonschema pytest  # if not already present
pytest eval/lyrics/tests/test_manifest_schema.py -v
```

Expected: FAILs because `schemas/manifest.schema.json` and `manifest.json` do not exist.

- [ ] **Step 3: Create the schema file**

Create `eval/lyrics/schemas/manifest.schema.json`:

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Lyrics Eval Manifest",
  "type": "object",
  "additionalProperties": false,
  "required": ["version", "fixtures"],
  "properties": {
    "version": {"const": 1},
    "fixtures": {
      "type": "array",
      "minItems": 1,
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["video_id", "category", "gold_source", "gold_lines"],
        "properties": {
          "video_id": {"type": "string", "pattern": "^[A-Za-z0-9_-]{11}$"},
          "category": {
            "type": "string",
            "enum": [
              "dense_vocal",
              "reverb_heavy",
              "instrumental_breaks",
              "multi_language",
              "clean_pop",
              "chant_repetition"
            ]
          },
          "gold_source": {
            "type": "string",
            "enum": ["lrclib_synced", "spotify_proxy", "yt_subs_manual"]
          },
          "gold_lines": {
            "type": "array",
            "minItems": 1,
            "items": {
              "type": "object",
              "additionalProperties": false,
              "required": ["text", "start_ms", "end_ms"],
              "properties": {
                "text": {"type": "string", "minLength": 1},
                "start_ms": {"type": "integer", "minimum": 0},
                "end_ms": {"type": "integer", "minimum": 0}
              }
            }
          },
          "notes": {"type": "string"}
        }
      }
    }
  }
}
```

- [ ] **Step 4: Create a placeholder `manifest.json`**

Create `eval/lyrics/manifest.json` — this will be replaced by `build_manifest.py` output in Task 4, but for now ship one valid example fixture so tests pass:

```json
{
  "version": 1,
  "fixtures": [
    {
      "video_id": "BW_vUblj_RA",
      "category": "dense_vocal",
      "gold_source": "lrclib_synced",
      "gold_lines": [
        {"text": "I'm not a sinner, I'm a saint", "start_ms": 25800, "end_ms": 27500}
      ],
      "notes": "Placeholder; replace via build_manifest.py during Task 4"
    }
  ]
}
```

- [ ] **Step 5: Run pytest, expect PASS**

```bash
pytest eval/lyrics/tests/test_manifest_schema.py -v
```

Expected: 4 tests PASS.

- [ ] **Step 6: Lint check**

```bash
ruff check eval/
ruff format --check eval/
```

Expected: no errors. If `ruff format --check` fails, run `ruff format eval/` then re-check.

- [ ] **Step 7: Commit**

```bash
git add eval/lyrics/schemas/manifest.schema.json eval/lyrics/manifest.json eval/lyrics/tests/test_manifest_schema.py
git commit -m "$(cat <<'EOF'
feat(eval): manifest JSONSchema + validator tests for #110

Defines the fixture-manifest contract: 6 categories, 3 allowed
gold sources, video_id format check, required gold_lines with
text+start_ms+end_ms. Ships a 1-fixture placeholder manifest that
will be overwritten by build_manifest.py in Task 4.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Per-fixture judgment + per-run report JSONSchemas (RED-GREEN)

**Files:**
- Create: `eval/lyrics/schemas/judgment.schema.json`
- Create: `eval/lyrics/schemas/report.schema.json`
- Create: `eval/lyrics/tests/test_report_schema.py`

- [ ] **Step 1: Write the failing test**

Create `eval/lyrics/tests/test_report_schema.py`:

```python
"""Validate per-fixture judgment + per-run report schemas."""

import json
from pathlib import Path

import jsonschema
import pytest

ROOT = Path(__file__).resolve().parents[1]
JUDGMENT_SCHEMA = ROOT / "schemas" / "judgment.schema.json"
REPORT_SCHEMA = ROOT / "schemas" / "report.schema.json"


@pytest.fixture(scope="module")
def judgment_schema() -> dict:
    return json.loads(JUDGMENT_SCHEMA.read_text(encoding="utf-8"))


@pytest.fixture(scope="module")
def report_schema() -> dict:
    return json.loads(REPORT_SCHEMA.read_text(encoding="utf-8"))


def test_valid_judgment_validates(judgment_schema: dict) -> None:
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


def test_judgment_score_must_be_0_to_10(judgment_schema: dict) -> None:
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


def test_valid_report_validates(report_schema: dict, judgment_schema: dict) -> None:
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
    jsonschema.validate(sample, report_schema)
```

- [ ] **Step 2: Run pytest, expect FAIL**

```bash
pytest eval/lyrics/tests/test_report_schema.py -v
```

Expected: FAILs because schema files do not exist.

- [ ] **Step 3: Create `eval/lyrics/schemas/judgment.schema.json`**

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Lyrics Eval Per-Fixture Judgment",
  "type": "object",
  "additionalProperties": false,
  "required": [
    "video_id",
    "score",
    "verdict",
    "wer_estimate",
    "line_timing_assessment",
    "hallucination_count",
    "hallucination_details",
    "coverage_pct",
    "wall_acceptable",
    "reasoning",
    "judged_at",
    "judge_model",
    "judge_prompt_revision"
  ],
  "properties": {
    "video_id": {"type": "string", "pattern": "^[A-Za-z0-9_-]{11}$"},
    "score": {"type": "integer", "minimum": 0, "maximum": 10},
    "verdict": {"type": "string", "enum": ["match", "partial", "miss", "hallucination"]},
    "wer_estimate": {"type": "number", "minimum": 0.0, "maximum": 1.0},
    "line_timing_assessment": {"type": "string"},
    "hallucination_count": {"type": "integer", "minimum": 0},
    "hallucination_details": {"type": "string"},
    "coverage_pct": {"type": "number", "minimum": 0.0, "maximum": 1.0},
    "wall_acceptable": {"type": "boolean"},
    "reasoning": {"type": "string", "minLength": 1},
    "judged_at": {"type": "string", "pattern": "^\\d{4}-\\d{2}-\\d{2}T\\d{2}:\\d{2}:\\d{2}Z$"},
    "judge_model": {"type": "string", "minLength": 1},
    "judge_prompt_revision": {"type": "integer", "minimum": 1}
  }
}
```

- [ ] **Step 4: Create `eval/lyrics/schemas/report.schema.json`**

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Lyrics Eval Per-Run Report",
  "type": "object",
  "additionalProperties": false,
  "required": [
    "run_id",
    "backend_id",
    "backend_revision",
    "judge_model",
    "judge_prompt_revision",
    "fixtures_run",
    "fixtures_passed_wall",
    "aggregate",
    "per_fixture"
  ],
  "properties": {
    "run_id": {"type": "string", "pattern": "^\\d{4}-\\d{2}-\\d{2}T\\d{2}:\\d{2}:\\d{2}Z$"},
    "backend_id": {"type": "string", "minLength": 1},
    "backend_revision": {"type": "integer", "minimum": 1},
    "judge_model": {"type": "string", "minLength": 1},
    "judge_prompt_revision": {"type": "integer", "minimum": 1},
    "fixtures_run": {"type": "integer", "minimum": 1},
    "fixtures_passed_wall": {"type": "integer", "minimum": 0},
    "aggregate": {
      "type": "object",
      "additionalProperties": false,
      "required": ["mean_score", "median_score", "scores_by_category"],
      "properties": {
        "mean_score": {"type": "number", "minimum": 0.0, "maximum": 10.0},
        "median_score": {"type": "number", "minimum": 0.0, "maximum": 10.0},
        "scores_by_category": {
          "type": "object",
          "additionalProperties": {"type": "number", "minimum": 0.0, "maximum": 10.0}
        }
      }
    },
    "per_fixture": {
      "type": "array",
      "minItems": 1,
      "items": {"$ref": "judgment.schema.json"}
    }
  }
}
```

- [ ] **Step 5: Run pytest, expect PASS**

```bash
pytest eval/lyrics/tests/test_report_schema.py -v
```

Expected: 3 tests PASS.

- [ ] **Step 6: Lint check**

```bash
ruff check eval/
ruff format --check eval/
```

- [ ] **Step 7: Commit**

```bash
git add eval/lyrics/schemas/judgment.schema.json eval/lyrics/schemas/report.schema.json eval/lyrics/tests/test_report_schema.py
git commit -m "$(cat <<'EOF'
feat(eval): per-fixture judgment + per-run report JSONSchemas for #110

Locks down the contract Claude must produce when judging fixtures
(score 0-10, verdict enum, wall_acceptable boolean, full reasoning
string) and the aggregate report shape (mean/median/per-category
breakdown).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: `build_manifest.py` (RED-GREEN)

**Files:**
- Create: `eval/lyrics/build_manifest.py`
- Create: `eval/lyrics/tests/test_build_manifest.py`

- [ ] **Step 1: Write the failing test**

Create `eval/lyrics/tests/test_build_manifest.py`:

```python
"""Tests for build_manifest.py helper (no live HTTP)."""

import json
from pathlib import Path

import pytest

from eval.lyrics import build_manifest  # type: ignore[import-not-found]


def test_filter_by_gold_source_keeps_only_allowed() -> None:
    songs = [
        {"youtube_id": "aaaaaaaaaaa", "lyrics_source": "lrclib_synced", "lines": ["x"]},
        {"youtube_id": "bbbbbbbbbbb", "lyrics_source": "description", "lines": ["y"]},
        {"youtube_id": "ccccccccccc", "lyrics_source": "spotify_proxy", "lines": ["z"]},
        {"youtube_id": "ddddddddddd", "lyrics_source": "asr_gap", "lines": []},
    ]
    out = build_manifest.filter_by_gold_source(songs)
    ids = [s["youtube_id"] for s in out]
    assert ids == ["aaaaaaaaaaa", "ccccccccccc"]


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
```

Then add a conftest.py so the import resolves:

```python
# eval/lyrics/tests/conftest.py
import sys
from pathlib import Path

# Make `from eval.lyrics import build_manifest` resolve when pytest runs from repo root.
sys.path.insert(0, str(Path(__file__).resolve().parents[3]))
```

- [ ] **Step 2: Run pytest, expect FAIL**

```bash
pytest eval/lyrics/tests/test_build_manifest.py -v
```

Expected: FAILs with ImportError (no `build_manifest` module yet).

- [ ] **Step 3: Implement `build_manifest.py`**

Create `eval/lyrics/build_manifest.py`:

```python
#!/usr/bin/env python3
"""build_manifest.py — query production SongPlayer HTTP API and write manifest.json.

One-shot helper. Lists songs whose `lyrics_source` is one of {lrclib_synced,
spotify_proxy, yt_subs_manual}, prompts the user to bucket them into the 6
manifest categories, and writes `eval/lyrics/manifest.json`.

Usage:
    python eval/lyrics/build_manifest.py \\
        --songplayer-url http://10.77.9.201:8920 \\
        --out eval/lyrics/manifest.json
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

import requests

ALLOWED_GOLD_SOURCES = {"lrclib_synced", "spotify_proxy", "yt_subs_manual"}
CATEGORIES = [
    "dense_vocal",
    "reverb_heavy",
    "instrumental_breaks",
    "multi_language",
    "clean_pop",
    "chant_repetition",
]
TARGET_PER_CATEGORY = 5


def fetch_songs(base_url: str) -> list[dict[str, Any]]:
    """Hit /api/v1/songs and return the song list."""
    r = requests.get(f"{base_url}/api/v1/songs", timeout=30)
    r.raise_for_status()
    return r.json().get("songs", [])


def filter_by_gold_source(songs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [s for s in songs if s.get("lyrics_source") in ALLOWED_GOLD_SOURCES]


def to_manifest_entry(
    song: dict[str, Any], *, category: str, notes: str = ""
) -> dict[str, Any]:
    entry: dict[str, Any] = {
        "video_id": song["youtube_id"],
        "category": category,
        "gold_source": song["lyrics_source"],
        "gold_lines": [
            {
                "text": line["text"],
                "start_ms": int(line["start_ms"]),
                "end_ms": int(line["end_ms"]),
            }
            for line in song["lines"]
        ],
    }
    if notes:
        entry["notes"] = notes
    return entry


def write_manifest(fixtures: list[dict[str, Any]], out_path: Path) -> None:
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(
        json.dumps({"version": 1, "fixtures": fixtures}, indent=2, ensure_ascii=False)
        + "\n",
        encoding="utf-8",
    )


def interactive_bucket(eligible: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Prompt the user to assign each picked song to a category.

    Returns the list of finalized manifest entries.
    """
    print(f"\nEligible songs: {len(eligible)}", file=sys.stderr)
    fixtures: list[dict[str, Any]] = []
    for category in CATEGORIES:
        print(f"\nCategory: {category} (target {TARGET_PER_CATEGORY})", file=sys.stderr)
        for i, s in enumerate(eligible):
            print(
                f"  [{i:3}] {s['youtube_id']}  ({s.get('lyrics_source')})  "
                f"{s.get('title', '<no title>')}",
                file=sys.stderr,
            )
        idx_line = input(
            f"  Pick {TARGET_PER_CATEGORY} indices for {category} (space-separated): "
        )
        for idx_str in idx_line.split():
            song = eligible[int(idx_str)]
            notes = input(f"  Notes for {song['youtube_id']} (or blank): ")
            fixtures.append(to_manifest_entry(song, category=category, notes=notes))
    return fixtures


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--songplayer-url",
        default="http://10.77.9.201:8920",
        help="Base URL of the production SongPlayer HTTP API.",
    )
    p.add_argument(
        "--out",
        type=Path,
        default=Path("eval/lyrics/manifest.json"),
        help="Where to write the manifest JSON.",
    )
    args = p.parse_args(argv)

    songs = fetch_songs(args.songplayer_url)
    eligible = filter_by_gold_source(songs)
    if not eligible:
        print(
            "No songs with allowed gold_source found. Cannot build manifest.",
            file=sys.stderr,
        )
        return 1

    fixtures = interactive_bucket(eligible)
    write_manifest(fixtures, args.out)
    print(f"Wrote {len(fixtures)} fixtures to {args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 4: Run pytest, expect PASS**

```bash
pytest eval/lyrics/tests/test_build_manifest.py -v
```

Expected: 3 tests PASS.

- [ ] **Step 5: Lint check**

```bash
ruff check eval/
ruff format --check eval/
```

If format check fails, run `ruff format eval/` and re-check.

- [ ] **Step 6: Commit**

```bash
git add eval/lyrics/build_manifest.py eval/lyrics/tests/conftest.py eval/lyrics/tests/test_build_manifest.py
git commit -m "$(cat <<'EOF'
feat(eval): build_manifest.py — fetch eligible songs + interactive bucketing for #110

Queries the production SongPlayer HTTP API on win-resolume, filters
to lyrics_source in {lrclib_synced, spotify_proxy, yt_subs_manual},
then walks the user through assigning songs to the 6 manifest
categories. Output is a schema-valid eval/lyrics/manifest.json.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: `audio_prep.py` (implementation only — no unit test)

**Why no test:** the script depends on GPU + audio-separator + yt-dlp on win-resolume. Cannot be tested on Ubuntu CI without a multi-gigabyte model download. Verified end-to-end during Task 10's first eval run.

**Files:**
- Create: `eval/lyrics/audio_prep.py`

- [ ] **Step 1: Implement `audio_prep.py`**

Create `eval/lyrics/audio_prep.py`:

```python
#!/usr/bin/env python3
"""audio_prep.py — yt-dlp + Mel-Roformer + anvuew dereverb for one video_id.

Designed to run on win-resolume where audio-separator + checkpoints are
already installed via the lyrics-bootstrap venv. Caches the dereverbed
16 kHz mono WAV at the eval-cache path; emits that path on stdout.

Usage:
    python eval/lyrics/audio_prep.py --video-id <id> [--force]
    python eval/lyrics/audio_prep.py --video-id <id> --cache-dir C:/custom

The same `dereverb_mel_band_roformer_anvuew_sdr_19.1729.ckpt` checkpoint
the production aligner uses (crates/sp-server/src/lyrics/aligner.rs::
preprocess_vocals) is invoked here directly from Python.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

DEFAULT_CACHE_DIR = Path(r"C:\ProgramData\SongPlayer\eval-cache")
MEL_ROFORMER_MODEL = "model_bs_roformer_ep_317_sdr_12.9755.ckpt"
DEREVERB_MODEL = "dereverb_mel_band_roformer_anvuew_sdr_19.1729.ckpt"


def expected_cache_path(cache_dir: Path, video_id: str) -> Path:
    return cache_dir / f"{video_id}_vocal16k.wav"


def download_audio_with_ytdlp(video_id: str, work_dir: Path) -> Path:
    out_template = str(work_dir / f"{video_id}.%(ext)s")
    cmd = [
        "yt-dlp",
        "-q",
        "-f",
        "bestaudio",
        "-x",
        "--audio-format",
        "wav",
        "-o",
        out_template,
        f"https://www.youtube.com/watch?v={video_id}",
    ]
    subprocess.run(cmd, check=True)
    out = work_dir / f"{video_id}.wav"
    if not out.exists():
        raise RuntimeError(f"yt-dlp produced no output at {out}")
    return out


def separate_vocals_and_dereverb(audio_path: Path, work_dir: Path) -> Path:
    """Run audio-separator twice: vocals isolation, then dereverb.

    Mirrors crates/sp-server/src/lyrics/aligner.rs::preprocess_vocals.
    Returns the final dereverbed 16 kHz mono float32 WAV.
    """
    from audio_separator.separator import Separator  # type: ignore[import-not-found]

    stage_a_dir = work_dir / "stage_a"
    stage_a_dir.mkdir(parents=True, exist_ok=True)
    sep = Separator(output_dir=str(stage_a_dir), output_format="WAV")
    sep.load_model(model_filename=MEL_ROFORMER_MODEL)
    out_files = sep.separate(str(audio_path))
    vocal_stage_a = _pick_vocal_stem(out_files, stage_a_dir)

    stage_b_dir = work_dir / "stage_b"
    stage_b_dir.mkdir(parents=True, exist_ok=True)
    sep2 = Separator(output_dir=str(stage_b_dir), output_format="WAV")
    sep2.load_model(model_filename=DEREVERB_MODEL)
    out_files_2 = sep2.separate(vocal_stage_a)
    dereverbed = _pick_dereverbed_stem(out_files_2, stage_b_dir)

    # Downsample + mono via ffmpeg
    target = work_dir / "vocal16k.wav"
    subprocess.run(
        [
            "ffmpeg",
            "-y",
            "-loglevel",
            "error",
            "-i",
            str(dereverbed),
            "-ac",
            "1",
            "-ar",
            "16000",
            "-acodec",
            "pcm_s16le",
            str(target),
        ],
        check=True,
    )
    return target


def _pick_vocal_stem(out_files: list[str], fallback_dir: Path) -> str:
    abs_files = [
        p if os.path.isabs(p) else str(fallback_dir / p) for p in out_files
    ]
    vocal = [p for p in abs_files if "Vocals" in p or "vocals" in p]
    if vocal:
        return vocal[0]
    non_inst = [
        p for p in abs_files if "Instrumental" not in p and "instrumental" not in p
    ]
    if len(non_inst) == 1:
        return non_inst[0]
    raise RuntimeError(f"could not find vocal stem in: {abs_files}")


def _pick_dereverbed_stem(out_files: list[str], fallback_dir: Path) -> str:
    abs_files = [
        p if os.path.isabs(p) else str(fallback_dir / p) for p in out_files
    ]
    noreverb = [p for p in abs_files if "(noreverb)" in p.lower()]
    if noreverb:
        return noreverb[0]
    not_reverb = [p for p in abs_files if "(reverb)" not in p.lower()]
    if len(not_reverb) == 1:
        return not_reverb[0]
    raise RuntimeError(f"could not find dereverbed stem in: {abs_files}")


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--video-id", required=True)
    p.add_argument("--cache-dir", type=Path, default=DEFAULT_CACHE_DIR)
    p.add_argument("--force", action="store_true")
    args = p.parse_args(argv)

    cache_dir = args.cache_dir
    cache_dir.mkdir(parents=True, exist_ok=True)
    target = expected_cache_path(cache_dir, args.video_id)

    if target.exists() and not args.force:
        print(str(target))
        return 0

    with tempfile.TemporaryDirectory(prefix="lyrics_eval_") as tmp_str:
        work = Path(tmp_str)
        audio = download_audio_with_ytdlp(args.video_id, work)
        vocal = separate_vocals_and_dereverb(audio, work)
        shutil.copy2(vocal, target)

    print(str(target))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 2: Lint check**

```bash
ruff check eval/lyrics/audio_prep.py
ruff format --check eval/lyrics/audio_prep.py
```

- [ ] **Step 3: Commit**

```bash
git add eval/lyrics/audio_prep.py
git commit -m "$(cat <<'EOF'
feat(eval): audio_prep.py — yt-dlp + Mel-Roformer + anvuew dereverb for #110

Materializes the cached 16 kHz mono vocal WAV used by every eval
backend caller. Uses the same anvuew checkpoint as the production
aligner. End-to-end verification deferred to Task 10's first eval
run on win-resolume; pure unit tests would require >5 GB of model
downloads on CI.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: `backends/whisperx_replicate.py` (RED-GREEN)

**Files:**
- Create: `eval/lyrics/backends/__init__.py` (empty)
- Create: `eval/lyrics/backends/whisperx_replicate.py`
- Create: `eval/lyrics/tests/test_whisperx_backend.py`

- [ ] **Step 1: Write the failing test**

Create `eval/lyrics/tests/test_whisperx_backend.py`:

```python
"""Unit tests for whisperx_replicate backend output parsing."""

import json

from eval.lyrics.backends import whisperx_replicate as wx  # type: ignore


def test_parse_replicate_output_to_lines() -> None:
    raw = {
        "segments": [
            {
                "start": 1.0,
                "end": 2.5,
                "text": " Hello world ",
                "words": [
                    {"word": "Hello", "start": 1.0, "end": 1.5, "score": 0.99},
                    {"word": "world", "start": 1.6, "end": 2.5, "score": 0.97},
                ],
            },
            {
                "start": 3.0,
                "end": 4.2,
                "text": "Second line",
                "words": [],
            },
            {
                "start": 5.0,
                "end": 5.5,
                "text": "   ",
                "words": [],
            },
        ]
    }
    lines = wx.parse_output(raw)
    assert len(lines) == 2
    assert lines[0]["text"] == "Hello world"
    assert lines[0]["start_ms"] == 1000
    assert lines[0]["end_ms"] == 2500
    assert lines[0]["words"] == [
        {"text": "Hello", "start_ms": 1000, "end_ms": 1500, "confidence": 0.99},
        {"text": "world", "start_ms": 1600, "end_ms": 2500, "confidence": 0.97},
    ]
    assert lines[1]["text"] == "Second line"
    assert lines[1]["words"] is None  # no word-level info from this segment


def test_build_predict_input_shape() -> None:
    body = wx.build_predict_input("https://example.com/audio.wav", "en")
    assert body == {
        "audio_file": "https://example.com/audio.wav",
        "language": "en",
        "align_output": True,
        "diarization": False,
        "batch_size": 32,
    }


def test_emit_result_writes_schema_valid_json(tmp_path) -> None:
    out = tmp_path / "result.json"
    wx.emit_result(
        out_path=out,
        wav_path="C:\\test.wav",
        duration_ms=10000,
        lines=[
            {
                "text": "hi",
                "start_ms": 0,
                "end_ms": 500,
                "words": None,
            }
        ],
        raw_confidence=0.9,
        metadata={"model": "whisperx", "elapsed_s": 12.3},
    )
    data = json.loads(out.read_text(encoding="utf-8"))
    assert data["backend_id"] == "whisperx-large-v3"
    assert data["backend_revision"] == 1
    assert data["wav_path"] == "C:\\test.wav"
    assert data["duration_ms"] == 10000
    assert len(data["lines"]) == 1
    assert data["raw_confidence"] == 0.9
    assert data["metadata"]["model"] == "whisperx"
```

- [ ] **Step 2: Run pytest, expect FAIL**

```bash
pytest eval/lyrics/tests/test_whisperx_backend.py -v
```

Expected: FAIL with ImportError.

- [ ] **Step 3: Implement the backend caller**

Create `eval/lyrics/backends/__init__.py` (empty file) and `eval/lyrics/backends/whisperx_replicate.py`:

```python
#!/usr/bin/env python3
"""whisperx_replicate.py — baseline backend caller for /lyrics-eval.

Calls victor-upmeet/whisperx on Replicate, mirroring the pinned
version hash from `crates/sp-server/src/lyrics/whisperx_replicate.rs::
WHISPERX_VERSION`. Reads REPLICATE_API_TOKEN from the environment.

Usage:
    python eval/lyrics/backends/whisperx_replicate.py \\
        --wav /abs/path/vocal16k.wav \\
        --out /tmp/result.json \\
        [--language en]

Output JSON matches the backend-call contract documented in
`docs/superpowers/specs/2026-05-18-lyrics-eval-harness-design.md` §7.
"""

from __future__ import annotations

import argparse
import json
import os
import time
from pathlib import Path
from typing import Any

import requests

# Mirror crates/sp-server/src/lyrics/whisperx_replicate.rs::WHISPERX_VERSION.
WHISPERX_VERSION = "84d2ad2d6194fe98a17d2b60bef1c7f910c46b2f6fd38996ca457afd9c8abfcb"
BACKEND_ID = "whisperx-large-v3"
BACKEND_REVISION = 1
REPLICATE_BASE = "https://api.replicate.com/v1"


def build_predict_input(audio_url: str, language: str) -> dict[str, Any]:
    return {
        "audio_file": audio_url,
        "language": language,
        "align_output": True,
        "diarization": False,
        "batch_size": 32,
    }


def upload_file(wav_path: Path, token: str) -> str:
    """Upload local WAV to Replicate's file API and return the public URL."""
    with wav_path.open("rb") as fh:
        r = requests.post(
            f"{REPLICATE_BASE}/files",
            headers={"Authorization": f"Token {token}"},
            files={"content": (wav_path.name, fh, "audio/wav")},
            timeout=300,
        )
    r.raise_for_status()
    payload = r.json()
    return payload["urls"]["get"]


def run_prediction(audio_url: str, language: str, token: str) -> dict[str, Any]:
    body = {
        "version": WHISPERX_VERSION,
        "input": build_predict_input(audio_url, language),
    }
    r = requests.post(
        f"{REPLICATE_BASE}/predictions",
        headers={
            "Authorization": f"Token {token}",
            "Content-Type": "application/json",
        },
        json=body,
        timeout=60,
    )
    r.raise_for_status()
    pred = r.json()
    poll_url = pred["urls"]["get"]
    while True:
        time.sleep(2.0)
        rr = requests.get(
            poll_url,
            headers={"Authorization": f"Token {token}"},
            timeout=60,
        )
        rr.raise_for_status()
        cur = rr.json()
        status = cur.get("status")
        if status == "succeeded":
            return cur["output"]
        if status in {"failed", "canceled"}:
            raise RuntimeError(
                f"replicate prediction {status}: {cur.get('error')!r}"
            )


def parse_output(output: dict[str, Any]) -> list[dict[str, Any]]:
    segments = output.get("segments")
    if not isinstance(segments, list):
        raise ValueError("output missing segments[]")
    lines: list[dict[str, Any]] = []
    for seg in segments:
        text = (seg.get("text") or "").strip()
        if not text:
            continue
        start_ms = int(round(float(seg["start"]) * 1000))
        end_ms = int(round(float(seg["end"]) * 1000))
        raw_words = seg.get("words") or []
        word_objs = []
        for w in raw_words:
            ws = w.get("start")
            we = w.get("end")
            if ws is None or we is None:
                continue
            word_objs.append(
                {
                    "text": w.get("word", "").strip(),
                    "start_ms": int(round(float(ws) * 1000)),
                    "end_ms": int(round(float(we) * 1000)),
                    "confidence": float(w.get("score") or 0.0),
                }
            )
        lines.append(
            {
                "text": text,
                "start_ms": start_ms,
                "end_ms": end_ms,
                "words": word_objs if word_objs else None,
            }
        )
    return lines


def emit_result(
    *,
    out_path: Path,
    wav_path: str,
    duration_ms: int,
    lines: list[dict[str, Any]],
    raw_confidence: float,
    metadata: dict[str, Any],
) -> None:
    payload = {
        "backend_id": BACKEND_ID,
        "backend_revision": BACKEND_REVISION,
        "wav_path": wav_path,
        "duration_ms": duration_ms,
        "lines": lines,
        "raw_confidence": raw_confidence,
        "metadata": metadata,
    }
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def estimate_duration_ms(lines: list[dict[str, Any]]) -> int:
    if not lines:
        return 0
    return max(int(line["end_ms"]) for line in lines)


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--wav", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--language", default="en")
    args = p.parse_args(argv)

    token = os.environ.get("REPLICATE_API_TOKEN")
    if not token:
        print("REPLICATE_API_TOKEN not set", flush=True)
        return 2

    t0 = time.time()
    audio_url = upload_file(args.wav, token)
    output = run_prediction(audio_url, args.language, token)
    lines = parse_output(output)
    elapsed = time.time() - t0

    emit_result(
        out_path=args.out,
        wav_path=str(args.wav),
        duration_ms=estimate_duration_ms(lines),
        lines=lines,
        raw_confidence=float(output.get("confidence", 0.0)),
        metadata={
            "model": "victor-upmeet/whisperx",
            "model_version": WHISPERX_VERSION,
            "elapsed_s": round(elapsed, 1),
            "segment_count": len(lines),
        },
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 4: Run pytest, expect PASS**

```bash
pytest eval/lyrics/tests/test_whisperx_backend.py -v
```

Expected: 3 tests PASS.

- [ ] **Step 5: Lint check**

```bash
ruff check eval/
ruff format --check eval/
```

- [ ] **Step 6: Commit**

```bash
git add eval/lyrics/backends/__init__.py eval/lyrics/backends/whisperx_replicate.py eval/lyrics/tests/test_whisperx_backend.py
git commit -m "$(cat <<'EOF'
feat(eval): backends/whisperx_replicate.py baseline caller for #110

Mirrors crates/sp-server/src/lyrics/whisperx_replicate.rs (pinned
version hash, predict-input shape, segment->line parsing). Output
JSON matches the backend contract Claude consumes during eval.
Unit tests cover parser shape + emit-result formatting; live
Replicate call verified during Task 10's first eval run.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: `judge_prompt.md` (versioned judge prompt template)

**Files:**
- Create: `eval/lyrics/judge_prompt.md`

- [ ] **Step 1: Write the judge prompt v1**

Create `eval/lyrics/judge_prompt.md`:

```markdown
# Lyrics Eval Judge Prompt — Revision 1

You are judging the quality of a lyrics-alignment backend's output against a
known-good gold reference. Your output is a single JSON object matching
`eval/lyrics/schemas/judgment.schema.json`. Do not write prose around the JSON.

## Inputs you receive

1. **Gold lines** — the reference lyrics with `text`, `start_ms`, `end_ms` per
   line. These come from a line-synced source the user already trusts
   (lrclib_synced, spotify_proxy, or yt_subs_manual).
2. **Candidate output** — the backend's `lines` array, each with `text`,
   `start_ms`, `end_ms`, and optional per-word timings (which you ignore for
   this judgment per `feedback_line_timing_only.md`).
3. **Song metadata** — `video_id`, `duration_ms`, `category`, optional notes.

## What to judge

- **WER estimate (`wer_estimate`)**: rough word-error-rate of candidate text vs
  gold text, line by line. Float in [0.0, 1.0]. You estimate; this is not a
  mechanical metric.
- **Line-timing assessment (`line_timing_assessment`)**: one sentence on the
  median absolute offset between candidate and gold `start_ms`. Call out
  systematic drift (always early/late) or outlier lines.
- **Hallucinations (`hallucination_count`, `hallucination_details`)**: lines
  the candidate produced that are not in the gold (repetition loops,
  fabricated bridges, vocal-isolation artifacts). Count them; describe in
  one or two sentences.
- **Coverage (`coverage_pct`)**: fraction of gold lines that have a matching
  candidate line within ±500 ms of the gold `start_ms`. Float in [0.0, 1.0].
- **Wall-acceptable (`wall_acceptable`)**: boolean. Would you ship this on the
  live LED wall during a service? A single hallucination cluster, > 5 missing
  lines, or median timing offset > 400 ms = not wall-acceptable.
- **Score (`score`)**: integer 0–10 summary. 10 = perfect match. 0 = unusable.
  7 = wall-acceptable with minor flaws. 4 = useful for debugging but not wall.
- **Verdict (`verdict`)**: one of `match`, `partial`, `miss`, `hallucination`.
  `match` = wall-acceptable. `partial` = mostly right but missing/drifted.
  `miss` = the candidate doesn't track the gold. `hallucination` = candidate
  contains fabricated content that would visibly fail on the wall.
- **Reasoning (`reasoning`)**: 2-4 sentences explaining the score + verdict.

## Required output fields

Emit exactly one JSON object with these keys:

- `video_id` — copy from input
- `score` — integer 0–10
- `verdict` — enum
- `wer_estimate` — float
- `line_timing_assessment` — string
- `hallucination_count` — integer
- `hallucination_details` — string (use "" when none)
- `coverage_pct` — float
- `wall_acceptable` — boolean
- `reasoning` — string
- `judged_at` — current UTC timestamp in `YYYY-MM-DDTHH:MM:SSZ`
- `judge_model` — your model id (e.g. `claude-opus-4-7`)
- `judge_prompt_revision` — integer; this revision is `1`

## Iron rules

- **Line-level focus only.** Ignore per-word timings (`feedback_line_timing_only.md`).
- **Wall is the bar.** Mechanical metrics are diagnostic; `wall_acceptable` is
  the decision.
- **Don't speculate.** If the candidate has no lines covering minutes 1:00-2:00
  and the gold does, that's missing coverage, not a "maybe instrumental
  break" excuse.
- **Cluster repetitions = hallucination.** If candidate has the same text
  string repeated 4+ times in a row with sub-second gaps, count it as one
  hallucination cluster, not 4+ separate matches.

## Updating this prompt

When this prompt changes in a way that materially affects judgments, bump the
revision number in this file's header AND in the `judge_prompt_revision`
default in `SKILL.md`. Old reports remain valid under their original revision.
```

- [ ] **Step 2: Commit**

```bash
git add eval/lyrics/judge_prompt.md
git commit -m "$(cat <<'EOF'
feat(eval): judge_prompt.md v1 for #110

Versioned judge prompt Claude follows when scoring fixtures.
Per-revision content lets us diff judge behavior over time and
re-run old fixtures under a new prompt revision.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: Initial `reports/` (CHAMPION.md + empty history.md)

**Files:**
- Create: `eval/lyrics/reports/CHAMPION.md`
- Create: `eval/lyrics/reports/history.md`

- [ ] **Step 1: Create `CHAMPION.md`**

Create `eval/lyrics/reports/CHAMPION.md`:

```markdown
# Current Lyrics Eval Champion

**Backend:** `whisperx-large-v3`
**Revision:** 1
**Promoted:** 2026-05-18 (initial baseline; first measured eval run pending)
**Score:** TBD (set by first `/lyrics-eval` run committed under this PR)
**Detail report:** `reports/2026-05-18-whisperx.json` (filled by Task 10)

## Promotion log

| Date | Backend | Revision | Decision | Reason |
|------|---------|----------|----------|--------|
| 2026-05-18 | whisperx-large-v3 | 1 | initial baseline | only production backend at the time the harness was built |
```

- [ ] **Step 2: Create `history.md`**

Create `eval/lyrics/reports/history.md`:

```markdown
# Lyrics Eval Run History

Append-only one-line summary of every `/lyrics-eval` run. Newest at the bottom.

| Date | Backend | Revision | Mean Score | Wall Pass | Note |
|------|---------|----------|------------|-----------|------|
```

- [ ] **Step 3: Commit**

```bash
git add eval/lyrics/reports/CHAMPION.md eval/lyrics/reports/history.md
git commit -m "$(cat <<'EOF'
feat(eval): initial CHAMPION.md + empty history.md for #110

Reserves the spot for the production champion (whisperx-large-v3
@ rev 1) and the append-only run log. Task 10 fills in the first
measured score.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 9: `.claude/skills/lyrics-eval/SKILL.md`

**Files:**
- Create: `.claude/skills/lyrics-eval/SKILL.md`

- [ ] **Step 1: Write the skill**

Create `.claude/skills/lyrics-eval/SKILL.md`:

```markdown
---
name: lyrics-eval
description: Score a lyrics-alignment backend against the pinned fixture set in eval/lyrics/manifest.json. Claude orchestrates: picks fixtures, runs audio prep + backend on win-resolume, judges output, writes a report committed to the repo. Use when evaluating a new backend or re-running an existing one after a prompt tune.
user-invocable: true
disable-model-invocation: true
---

# Lyrics Eval

**Single entry point for measuring lyrics-alignment backend quality.** The user
invokes `/lyrics-eval`; Claude drives every step. The user observes and can
intervene at any per-fixture step.

This skill follows the design in
`docs/superpowers/specs/2026-05-18-lyrics-eval-harness-design.md` and the
memories listed in MEMORY.md under "Lyrics eval is Claude-orchestrated" and
"Eval = Python, not Rust". Re-read those before any structural change to this
skill.

## Phase 0 — Preconditions

1. **Event status is user-authoritative.** Ask the user whether a live event
   is currently running on win-resolume. Never infer from OBS scene state. If
   an event is running → STOP and report. Per
   `feedback_event_status_user_authoritative.md` +
   `feedback_winresolume_is_shared_event_machine.md`.
2. **win-resolume reachable.** `mcp__win-resolume__Ping` returns success. If
   not → STOP and report.
3. **SongPlayer HTTP API reachable.** `curl -m 5 http://10.77.9.201:8920/api/v1/status` returns 200.
4. **REPLICATE_API_TOKEN set** in the win-resolume environment (or the local
   shell, if the backend caller is being invoked from dev). If missing → STOP
   and ask the user.

## Phase 1 — Configure the run

Ask the user (one AskUserQuestion):

- **Backend** to test (default `whisperx-large-v3`; the production champion).
- **Fixtures** subset: `all`, a category name, or a comma-separated list of
  `video_id`s.

Load `eval/lyrics/manifest.json`. Validate against
`eval/lyrics/schemas/manifest.schema.json`. If invalid → STOP and report.

Load the current `eval/lyrics/reports/CHAMPION.md` to know what to diff against
at the end.

## Phase 2 — Per-fixture loop

For each selected fixture, in manifest order:

1. **Audio prep.** On win-resolume:
   ```
   mcp__win-resolume__Shell:
       python C:\path\to\songplayer\eval\lyrics\audio_prep.py --video-id <id>
   ```
   Captures the printed WAV path. If exit code is non-zero, report the failure
   and ask the user whether to skip this fixture or stop the run.
2. **Backend call.** On win-resolume:
   ```
   mcp__win-resolume__Shell:
       python C:\path\to\songplayer\eval\lyrics\backends\<backend_id>.py \
           --wav <wav_path> --out C:\ProgramData\SongPlayer\eval-cache\<id>_result.json
   ```
   On failure: if Claude has reason to believe the backend prompt could be
   tuned (e.g. timeouts on dense chorus, repetition loops, malformed JSON),
   propose a concrete edit to `eval/lyrics/backends/<backend_id>.py`, apply
   it, and retry once. Per `feedback_eval_iteration_python_not_rust.md`, Claude
   has free hands here.
3. **Judge.** Read the result JSON + the fixture's `gold_lines`. Apply the
   prompt template in `eval/lyrics/judge_prompt.md`. Emit a JSON judgment
   matching `eval/lyrics/schemas/judgment.schema.json`. Validate the JSON
   shape before continuing.
4. **Print one-liner** to the user:
   `[7/30] BW_vUblj_RA  dense_vocal  score=7  verdict=partial  wall_acceptable=false`
   so they can interrupt if something looks wrong.

## Phase 3 — Aggregate + write report

After all selected fixtures judged:

- Compute `mean_score`, `median_score`, `scores_by_category`, `fixtures_passed_wall`.
- Build the per-run report JSON matching `eval/lyrics/schemas/report.schema.json`.
- Write `eval/lyrics/reports/<YYYY-MM-DD>-<backend_id>.json` AND a human
  Markdown twin `eval/lyrics/reports/<YYYY-MM-DD>-<backend_id>.md` with:
  - Header (run_id, backend, judge model, prompt revision)
  - Aggregate table
  - Per-category score table
  - Per-fixture single-row table (video_id | category | score | verdict |
    wall_acceptable | one-line reasoning)
- Append one row to `eval/lyrics/reports/history.md`.

## Phase 4 — Diff vs champion + recommend

If the tested backend is the current champion: report aggregate, ask the user
whether to update CHAMPION.md's `Score:` line (initial baseline case).

If the tested backend is NOT the current champion: load the champion's most
recent report from `history.md`. Diff mean score, median score,
wall-pass count, and per-category breakdown. Recommend one of:

- **Promote** — composite improvement across all critical categories, no
  regression on any.
- **Reject** — net regression or critical-category regression.
- **Partial promote** — wins decisively on a specific category (e.g.
  `asr_gap`-adjacent songs) but loses on others. Suggest routing the new
  backend ONLY for that category in future production work (this becomes
  input to issue #112's routing decision).

Present the recommendation to the user. NEVER edit CHAMPION.md without
explicit user "promote" approval.

## Phase 5 — Commit

After the user approves the report:

```
git add eval/lyrics/reports/<YYYY-MM-DD>-<backend_id>.{json,md} \
        eval/lyrics/reports/history.md \
        eval/lyrics/reports/CHAMPION.md
git commit -m "eval(lyrics): run <backend_id> rev <N> — mean <X>"
```

Push only after running the pre-push checks:

```
ruff check eval/
ruff format --check eval/
pytest eval/lyrics/tests -v
```

If any fail → fix and re-commit. Never push a broken eval/.

## Iron rules

- **Win-resolume idle gate.** Live event running → STOP. Always.
- **Free hands on prompts, not on schemas.** Claude may edit
  `backends/<id>.py` mid-run to tune prompts. Claude may NOT change the
  output JSON shape, the manifest schema, the report schema, or the judgment
  schema during a run — those changes require a separate PR.
- **No mechanical fallback scoring.** If Claude cannot judge a fixture (e.g.
  candidate JSON is malformed), the fixture is marked `verdict=miss`,
  `wall_acceptable=false`, `score=0`, with the malformed-output snippet in
  `reasoning`. Do not invent a fake score.
- **No catalog reprocess from this skill.** The skill does NOT trigger
  reprocess of any production songs. It runs only against the manifest
  fixtures on win-resolume's eval-cache. Production songs are untouched.
- **No pipeline-version bump from this skill.** Per
  `feedback_no_bump_until_proven.md`.
- **No backend promotion without user approval.** Per
  `pr-merge-policy.md` + `approval-scope.md`.

## When the user corrects you

If the user corrects Claude mid-run (e.g. "that hallucination call is wrong,
those are valid backing vocals"), ADD the correction as a new bullet under
"Iron rules" in THIS file at the end of the session. The skill grows with
experience, same pattern as `lyrics-verify`.
```

- [ ] **Step 2: Commit**

```bash
mkdir -p .claude/skills/lyrics-eval
# (write step 1 created the file already)
git add .claude/skills/lyrics-eval/SKILL.md
git commit -m "$(cat <<'EOF'
feat(eval): /lyrics-eval skill for #110

Claude-orchestrated slash command. Preconditions (event gate, win-
resolume reachable, REPLICATE_API_TOKEN), per-fixture loop (audio
prep -> backend -> judge), aggregate + report write, diff vs
champion, commit. Iron rules pin the no-bump / no-promote /
no-catalog-reprocess constraints.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 10: First eval run (interactive, on win-resolume)

This task is executed BY THE USER invoking the skill, observed BY CLAUDE in
the same session. It produces three new files committed by Task 11.

- [ ] **Step 1: Replace placeholder manifest**

Run `build_manifest.py` interactively to produce the real fixture list:

```bash
python eval/lyrics/build_manifest.py \
    --songplayer-url http://10.77.9.201:8920 \
    --out eval/lyrics/manifest.json
```

Pick ~5 fixtures per category across all 6 categories. The script walks you
through it. Commit:

```bash
git add eval/lyrics/manifest.json
git commit -m "feat(eval): real fixture manifest built from production DB for #110"
```

- [ ] **Step 2: Invoke `/lyrics-eval` for the baseline**

In Claude Code:

```
/lyrics-eval
```

Answer the AskUserQuestion: backend = `whisperx-large-v3`, fixtures = `all`.
Confirm preconditions (event idle, win-resolume reachable, token set).

Claude runs Phase 2 per-fixture against all fixtures. Observe each one-liner.
If a fixture looks miscounted, intervene; otherwise let it finish.

- [ ] **Step 3: Confirm the report files were written**

Files expected after the run:

```
eval/lyrics/reports/2026-05-18-whisperx-large-v3.json
eval/lyrics/reports/2026-05-18-whisperx-large-v3.md
eval/lyrics/reports/history.md   (one new row appended)
eval/lyrics/reports/CHAMPION.md  (Score: line filled in)
```

- [ ] **Step 4: Pre-push checks**

```bash
ruff check eval/
ruff format --check eval/
pytest eval/lyrics/tests -v
python -c "
import json, jsonschema
schema = json.load(open('eval/lyrics/schemas/report.schema.json'))
report = json.load(open('eval/lyrics/reports/2026-05-18-whisperx-large-v3.json'))
jsonschema.validate(report, schema)
print('report schema OK')
"
```

Expected: ruff + format + pytest pass; report validates against its schema.

- [ ] **Step 5: Commit**

```bash
git add eval/lyrics/reports/2026-05-18-whisperx-large-v3.json \
        eval/lyrics/reports/2026-05-18-whisperx-large-v3.md \
        eval/lyrics/reports/history.md \
        eval/lyrics/reports/CHAMPION.md
git commit -m "$(cat <<'EOF'
eval(lyrics): first baseline run of whisperx-large-v3 for #110

Closes the loop on #110: harness runs end-to-end. Mean score and
wall-pass count recorded in history.md; CHAMPION.md updated with
the baseline score.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 11: README + follow-up issue + open PR

**Files:**
- Create: `eval/lyrics/README.md`

- [ ] **Step 1: Write `README.md`**

Create `eval/lyrics/README.md`:

```markdown
# Lyrics Eval Harness

Claude-orchestrated scoring rig for lyrics-alignment backends. Invoke via
`/lyrics-eval` slash command.

See:

- `docs/superpowers/specs/2026-05-18-lyrics-eval-harness-design.md` — design
- `docs/superpowers/plans/2026-05-18-lyrics-eval-harness.md` — implementation plan
- `.claude/skills/lyrics-eval/SKILL.md` — slash-command instructions

## Directory contents

| Path | Purpose |
|------|---------|
| `manifest.json` | Pinned fixture set (~30 songs with line-synced gold) |
| `build_manifest.py` | One-shot helper to rebuild the manifest from the production DB |
| `audio_prep.py` | yt-dlp + Mel-Roformer + anvuew dereverb on win-resolume |
| `backends/` | One Python file per backend (whisperx_replicate.py baseline) |
| `judge_prompt.md` | Versioned judge prompt Claude follows |
| `schemas/` | JSONSchemas for manifest, report, judgment |
| `reports/` | Committed per-run reports + CHAMPION.md + history.md |
| `tests/` | pytest tests for schemas, build_manifest, backend parsers |

## Adding a new backend

NOT done in this PR. See issue #111 (backend zoo). When adding a new backend:

1. Drop a new file at `backends/<backend_id>.py` following the same I/O
   contract as `whisperx_replicate.py`.
2. Optionally add a unit test under `tests/test_<backend_id>_backend.py`.
3. Run `/lyrics-eval` against the new backend on the existing manifest.
4. Diff vs champion. If it wins, propose promotion in a separate PR.

## Local cache

`audio_prep.py` writes vocal stems to
`C:\ProgramData\SongPlayer\eval-cache\<video_id>_vocal16k.wav` on
win-resolume. This path is rebuildable; gitignored at
`eval/lyrics/.eval-cache/` for any local-dev mirror.
```

- [ ] **Step 2: File `/lyrics-research` follow-up issue**

```bash
gh issue create \
  --title "Lyrics SOTA: /lyrics-research slash command (monthly model discovery)" \
  --body "Follow-up to #110. The eval harness landed in #110's PR; this issue covers the discovery half.

## Goal

Add a \`/lyrics-research\` slash command that runs Claude through a structured monthly research pass: web-search recent ASR / audio-LLM releases (HuggingFace trending, Replicate explore, vendor blogs), filter by capability (audio input + timestamps + English minimum), and produce a ranked candidate list with adapter-shape suggestions.

## Out of scope

- Implementing any new backend (that is #111)
- Wiring candidates into the eval harness automatically (Claude does that interactively under \`/lyrics-eval\`)

## Why separate from #110

#110's PR kept tight focus on the scoring rig. Adding a 200-LoC research command in the same PR would inflate review scope. Once the harness is proven by one real run, this command becomes a small additive PR.

## Related

- Depends on #110 (eval harness) — needs to exist before research has a place to send candidates.
- Feeds #111 (backend zoo) — promising candidates from research get implemented under #111.
"
```

Capture the returned issue URL — paste it into the PR body in step 4.

- [ ] **Step 3: Pre-push checks**

```bash
ruff check eval/
ruff format --check eval/
pytest eval/lyrics/tests -v
git status --short
```

Expected: clean; nothing uncommitted.

- [ ] **Step 4: Push + open PR**

```bash
git push origin dev
gh pr create --title "Lyrics SOTA: eval harness + baseline run (closes #110)" \
  --body "$(cat <<'EOF'
## Summary

- Adds `/lyrics-eval` Claude-orchestrated slash command (repo-scoped skill at `.claude/skills/lyrics-eval/`).
- Adds Python eval infrastructure under `eval/lyrics/`: manifest, build_manifest, audio_prep, whisperx_replicate baseline, judge prompt, schemas, pytest suite.
- Adds CI `eval-checks` job running ruff + pytest on every push.
- First baseline run of `whisperx-large-v3` committed under `eval/lyrics/reports/`.
- No Rust changes. No `LYRICS_PIPELINE_VERSION` bump.

Spec: `docs/superpowers/specs/2026-05-18-lyrics-eval-harness-design.md`
Plan: `docs/superpowers/plans/2026-05-18-lyrics-eval-harness.md`

Closes #110.

Sequenced ahead of #111 (backend zoo) and #112 (no-text-source ASR path).
Follow-up filed for `/lyrics-research` (link in commit history).

## Test plan

- [x] `ruff check eval/` passes
- [x] `ruff format --check eval/` passes
- [x] `pytest eval/lyrics/tests` passes
- [x] First `/lyrics-eval` run committed under `eval/lyrics/reports/2026-05-18-whisperx-large-v3.{json,md}`
- [x] CHAMPION.md reflects baseline score
- [x] history.md has one row

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Monitor CI**

```bash
gh run list --branch dev --limit 1
# capture the run id, then:
sleep 300 && gh run view <run-id> --json status,conclusion,jobs
```

Per `ci-monitoring.md`: do not poll faster than 60-300s. If `eval-checks` or
any other gate fails → investigate with `gh run view <id> --log-failed`, fix
the root cause in a new commit, push, monitor again. Never use
`gh run watch` (rate-limit risk).

Report the green PR URL to the user. Wait for explicit "merge it" before
merging per `pr-merge-policy.md`.

---

## Self-review (run by plan author)

**Spec coverage:**
- §1 Goal → Tasks 1–11 collectively
- §2 Non-goals → Task 11 PR body explicitly disclaims them
- §3 Architecture / repo layout → Task 1 scaffolds it
- §4 Skill flow → Task 9 SKILL.md implements every phase
- §5 Manifest schema + selection → Task 2 schema, Task 4 builder, Task 10 first real run
- §6 Audio prep → Task 5
- §7 Backend caller contract → Task 6
- §8 Scoring (Claude as judge) → Task 7 judge_prompt.md + Task 9 SKILL.md Phase 2
- §9 Reports → Tasks 7, 8, 9 (SKILL writes them), 10 commits first
- §10 PR1 deliverables → all items checked across Tasks 1–11
- §11 Testing → Tasks 1 (CI hook), 2-3-4-6 (schema + unit tests)
- §12 Risks → recorded in spec; SKILL.md Iron Rules cover the in-skill mitigations
- §13 Open items → all three resolved during plan tasks (fixture pick = Task 10 Step 1; judge prompt v1 = Task 7; CI step = Task 1 Step 3)

**Placeholder scan:** none — every step has a runnable command or full code block.

**Type / name consistency:**
- `backend_id` everywhere is the string id (e.g. `whisperx-large-v3`); `backend_revision` is the integer.
- `judge_prompt_revision` integer field appears in judgment schema, report schema, and judge_prompt.md header — all matched at `1`.
- `gold_source` enum identical in manifest schema + build_manifest filter set + spec §5.2.
- `wall_acceptable` boolean field name identical across schemas, judge_prompt.md, and SKILL.md text.

Plan is internally consistent. Ready for execution.
