"""#243 follow-up: every workflow step that kills SongPlayer quiesces its Spout senders first.

A `taskkill /F` (or a `Stop-ScheduledTask`, which ends the task's process the
same way) that lands while one of SongPlayer's threads holds a Spout lock
leaves that lock ABANDONED. Spout 2.007.017 never gives back a lock it was
handed that way, so the receiver it goes to (Resolume Arena) keeps Spout's
sender list for good, and every new SongPlayer sender is refused: PP,
12.10.2026, after deploy-pp 38239301748, MAX and SP-program were down until a
manual rescue. So every step that kills SongPlayer first runs
`scripts/spout-quiesce.ps1`: SongPlayer drops both Spout senders and makes no
Spout call any more (`POST /api/v1/program/spout/quiesce`, then `max.state`
reads `quiesced`), and only then is it killed, as before.
"""

import re
from pathlib import Path

import check_workflow_ps_ascii as ps_ascii

_REPO = Path(__file__).resolve().parents[2]
_WORKFLOWS = _REPO / ".github" / "workflows"
_SCRIPT = _REPO / "scripts" / "spout-quiesce.ps1"
_QUIESCE = "scripts\\spout-quiesce.ps1"
_KILL = re.compile(r'taskkill\s+/F\s+/IM\s+"SongPlayer\.exe"', re.IGNORECASE)
_STOPS = (
    _KILL,
    re.compile(r'Stop-ScheduledTask\s+-TaskName\s+"?SongPlayer"?', re.IGNORECASE),
)
_STARTS = (
    re.compile(r'schtasks(?:\.exe)?\s+/run\s+/tn\s+"?SongPlayer"?', re.IGNORECASE),
    re.compile(r'Start-ScheduledTask\s+-TaskName\s+"?SongPlayer"?', re.IGNORECASE),
)


def _code(step: list[tuple[int, str]]) -> list[tuple[int, str]]:
    """The code lines of a PowerShell step's `run:` block (comments dropped)."""
    return [
        (no, line.strip())
        for no, line in ps_ascii._run_block(step)
        if line.strip() and not line.strip().startswith("#")
    ]


def _killing_steps(text: str) -> list[list[tuple[int, str]]]:
    """The code of every PowerShell step that kills SongPlayer.exe."""
    steps = []
    for _, step in ps_ascii._steps(text.split("\n")):
        if not ps_ascii._is_ps_step(step):
            continue
        code = _code(step)
        if any(_KILL.search(line) for _, line in code):
            steps.append(code)
    return steps


def _unquiesced(code: list[tuple[int, str]]) -> list[tuple[int, str]]:
    """Every stop of SongPlayer no quiesce covers: a quiesce covers the stops
    after it until SongPlayer is started again (a later kill of the new
    process needs its own)."""
    bad = []
    covered = False
    for no, line in code:
        if _QUIESCE in line:
            covered = True
        elif any(start.search(line) for start in _STARTS):
            covered = False
        elif any(stop.search(line) for stop in _STOPS) and not covered:
            bad.append((no, line))
    return bad


def _stops(steps: list[list[tuple[int, str]]]) -> int:
    return sum(
        1
        for code in steps
        for _, line in code
        if any(stop.search(line) for stop in _STOPS)
    )


def test_the_rule_reads_a_step_as_the_deploys_run_it():
    step = [
        (1, '& "$env:GITHUB_WORKSPACE\\scripts\\spout-quiesce.ps1"'),
        (2, 'Stop-ScheduledTask -TaskName "SongPlayer"'),
        (3, 'taskkill /F /IM "SongPlayer.exe" 2>&1 | Out-Null'),
        (4, "schtasks.exe /run /tn SongPlayer"),
        (5, 'taskkill /F /IM "SongPlayer.exe" 2>&1 | Out-Null'),
    ]
    assert _unquiesced(step) == [(5, step[4][1])], "a relaunched process needs its own"
    assert _unquiesced(step[1:3]) == [(2, step[1][1]), (3, step[2][1])]


def test_every_kill_of_songplayer_on_snv_is_quiesced_first():
    steps = _killing_steps((_WORKFLOWS / "ci.yml").read_text(encoding="utf-8"))
    # The deploy, the E2E restart and the restart's fallback kill.
    assert len(steps) == 2, "Deploy SongPlayer + Restart SongPlayer"
    assert _stops(steps) >= 4
    for code in steps:
        assert _unquiesced(code) == []


def test_every_kill_of_songplayer_at_pp_is_quiesced_first():
    steps = _killing_steps((_WORKFLOWS / "deploy-pp.yml").read_text(encoding="utf-8"))
    assert len(steps) == 1, "Stop SongPlayer"
    assert _stops(steps) == 2, "the task instance, then the process"
    assert _unquiesced(steps[0]) == []


def test_no_other_workflow_kills_songplayer():
    for path in sorted(_WORKFLOWS.glob("*.yml")):
        if path.name in ("ci.yml", "deploy-pp.yml"):
            continue
        assert not _KILL.search(path.read_text(encoding="utf-8")), path.name


def test_the_quiesce_script_asks_then_polls_bounded_and_never_fails_the_step():
    text = _SCRIPT.read_text(encoding="utf-8")
    assert all(ord(ch) < 128 for ch in text), "Windows PowerShell 5.1 reads cp1252"
    assert "-Method Post" in text
    assert "/api/v1/program/spout/quiesce" in text
    assert "/api/v1/program" in text
    assert '"quiesced"' in text
    assert "$TimeoutSec = 5" in text, "the deploy waits at most 5 s"
    # Best effort: SongPlayer is killed whatever happened here, so the
    # script never throws out of the step and never sets an exit code.
    assert not re.search(r"^\s*exit\b", text, re.MULTILINE | re.IGNORECASE)
    assert not re.search(r"^\s*throw\b", text, re.MULTILINE | re.IGNORECASE)
    assert text.count("-ErrorAction Stop") >= 2, "each request's failure is caught"
