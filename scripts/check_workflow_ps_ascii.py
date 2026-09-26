"""Fail when a PowerShell `run:` block in a GitHub workflow holds non-ASCII code.

Windows PowerShell 5.1 (`shell: powershell` on the win-resolume runner) reads the
generated step script as ANSI (cp1252), not UTF-8. An em dash (U+2014, UTF-8
E2 80 94) then decodes to three cp1252 characters, and 0x94 is a curly double
quote that PowerShell treats as a string delimiter: the string ends early and the
step fails with "A positional parameter cannot be found" (26.9.2026, E2E restart
step). Comments are harmless; code lines must be ASCII.

Usage: python3 scripts/check_workflow_ps_ascii.py [workflow.yml ...]
(default: every .github/workflows/*.yml). Exit 1 and print each offending line.
"""

from __future__ import annotations

import sys
from pathlib import Path

PS_SHELLS = ("powershell", "pwsh")


def _indent(line: str) -> int:
    return len(line) - len(line.lstrip(" "))


def _steps(lines: list[str]) -> list[tuple[int, list[tuple[int, str]]]]:
    """Split a workflow into steps: each `- ` list item under a `steps:` key."""
    steps: list[tuple[int, list[tuple[int, str]]]] = []
    step_indent: int | None = None
    current: list[tuple[int, str]] | None = None
    for no, line in enumerate(lines, 1):
        stripped = line.strip()
        if stripped == "steps:":
            step_indent = None
            current = None
            continue
        if stripped.startswith("- ") or stripped == "-":
            ind = _indent(line)
            if step_indent is None or ind == step_indent:
                step_indent = ind
                current = [(no, line)]
                steps.append((no, current))
                continue
        ends_steps = (
            current is not None
            and stripped
            and step_indent is not None
            and _indent(line) <= step_indent
            and not stripped.startswith("#")
        )
        if ends_steps:
            current = None
            step_indent = None
            continue
        if current is not None:
            current.append((no, line))
    return steps


def _is_ps_step(step: list[tuple[int, str]]) -> bool:
    for _, line in step:
        key = line.strip().lstrip("- ").strip()
        if key.startswith("shell:"):
            value = key.split(":", 1)[1].strip().strip("'\"").split()[0:1]
            return bool(value) and value[0].lower() in PS_SHELLS
    return False


def _run_block(step: list[tuple[int, str]]) -> list[tuple[int, str]]:
    block: list[tuple[int, str]] = []
    run_indent: int | None = None
    for no, line in step:
        key = line.strip().lstrip("- ").strip()
        if run_indent is None:
            if key.startswith("run:"):
                run_indent = _indent(line) + (2 if line.strip().startswith("- ") else 0)
                inline = key[4:].strip()
                if inline and inline not in ("|", ">", "|-", ">-"):
                    block.append((no, inline))
            continue
        if line.strip() and _indent(line) <= run_indent:
            break
        block.append((no, line))
    return block


def violations(text: str) -> list[tuple[int, str]]:
    """(line number, line) of every non-ASCII code line in a PowerShell run block."""
    lines = text.split("\n")
    found: list[tuple[int, str]] = []
    for _, step in _steps(lines):
        if not _is_ps_step(step):
            continue
        for no, line in _run_block(step):
            code = line.strip()
            if not code or code.startswith("#"):
                continue
            if any(ord(ch) > 127 for ch in code):
                found.append((no, code))
    return found


def main(argv: list[str]) -> int:
    paths = [Path(a) for a in argv] or sorted(Path(".github/workflows").glob("*.yml"))
    bad = 0
    for path in paths:
        for no, code in violations(path.read_text(encoding="utf-8")):
            print(f"{path}:{no}: non-ASCII in a PowerShell run block: {code[:120]}")
            bad += 1
    if bad:
        print(
            f"{bad} line(s): Windows PowerShell 5.1 reads step scripts as cp1252 - use ASCII"
        )
        return 1
    print("ok: every PowerShell run block is ASCII")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
