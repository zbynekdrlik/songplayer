"""Issue 274: every deploy delivers the runner's auto-start from the repo.

The guarded `start-runner.vbs` (#223 follow-up, 9.10.2026) lived only in
`scripts/setup-runner.ps1`, which returns at once on a box whose runner is
already configured. PP (registered 7.10 with the old script) kept the old
file, its 5-minute task started a new `Runner.Listener` every time, four
were alive on 14.10.2026, and a duplicate cancelled the 0.82.0 PP E2E
(its job renewals were refused; it sent the cancellation itself).

So `scripts/runner-autostart.ps1` is the ONE source of `start-runner.bat`
and `start-runner.vbs`, and it ends a duplicate listener of the runner
directory. The installer and BOTH deploy jobs run it, the deploys before
anything stops SongPlayer (a failure there leaves SongPlayer running).
The decisions are Pester-tested (`runner-autostart.Tests.ps1`); this file
pins the wiring and the vbs guard.
"""

import re
from pathlib import Path

import check_workflow_ps_ascii as ps_ascii

_REPO = Path(__file__).resolve().parents[2]
_SCRIPT = _REPO / "scripts" / "runner-autostart.ps1"
_SETUP = _REPO / "scripts" / "setup-runner.ps1"
_CI = _REPO / ".github" / "workflows" / "ci.yml"
_PP = _REPO / ".github" / "workflows" / "deploy-pp.yml"
_PESTER = _REPO / "scripts" / "tests" / "runner-autostart.Tests.ps1"
# A run of the script as a step calls it: its path with a backslash.
_CALL = "\\scripts\\runner-autostart.ps1"


def _script() -> str:
    return _SCRIPT.read_text(encoding="utf-8")


def _function(text: str, name: str) -> str:
    """The body of a PowerShell function, up to the next top-level one."""
    start = text.index(f"function {name}")
    nxt = text.find("\nfunction ", start + 1)
    return text[start : nxt if nxt != -1 else len(text)]


def _job(workflow: Path, job: str) -> list[str]:
    """The lines of one job of a workflow (its header to the next job's)."""
    lines = workflow.read_text(encoding="utf-8").splitlines()
    start = next(i for i, line in enumerate(lines) if line == f"  {job}:")
    end = next(
        (
            i
            for i in range(start + 1, len(lines))
            if re.match(r"^  [A-Za-z0-9_-]+:\s*$", lines[i])
        ),
        len(lines),
    )
    return lines[start:end]


def _index(lines: list[str], needle: str) -> int:
    hits = [i for i, line in enumerate(lines) if needle in line]
    assert hits, f"no line holds {needle!r}"
    return hits[0]


def test_the_vbs_never_starts_a_second_runner():
    """#223 follow-up (9.10.2026): the auto-start task runs the vbs every
    5 minutes. It must quit when this runner directory's listener, or the
    start-runner.bat that runs it, is already running, before it runs the
    bat."""
    vbs = _function(_script(), "Get-RunnerStartVbs")
    run = vbs.index("WshShell.Run")
    guard = vbs[:run]
    assert "Win32_Process" in guard, "no process check before the start"
    assert "Runner.Listener.exe" in guard
    assert "\\bin\\Runner.Listener.exe" in guard, "only THIS runner's listener counts"
    assert "\\start-runner.bat" in guard, "the running wrapper counts too"
    assert "WScript.Quit" in guard


def test_the_installer_holds_no_copy_and_runs_the_script_on_every_path():
    """A configured box returned before the files were written: the fix of
    9.10 never reached PP. The installer now runs the one script, on a new
    install AND on a box that already has a runner, and keeps no copy."""
    s = _SETUP.read_text(encoding="utf-8")
    assert 'Set-Content "$RunnerDir\\start-runner.vbs"' not in s
    assert 'Set-Content "$RunnerDir\\start-runner.bat"' not in s
    assert "WshShell" not in s, "a second copy of the vbs"
    # The one helper that runs the script, defined before its first use.
    helper = _function(s, "Install-RunnerAutostart")
    assert "runner-autostart.ps1" in helper
    assert "-File" in helper and "$LASTEXITCODE" in helper and "throw" in helper
    call = "Install-RunnerAutostart -RunnerDir $RunnerDir"
    configured = s.index("Runner already configured")
    assert s.index("function Install-RunnerAutostart") < configured
    returned = s.index("return", configured)
    assert (
        call in s[configured:returned]
    ), "a box that already has a runner must get the files too"
    task = s.index("Register-ScheduledTask")
    assert (
        call in s[returned:task]
    ), "a new install writes the files before its task runs them"


def test_snvs_deploy_delivers_it_before_songplayer_stops():
    job = _job(_CI, "deploy-resolume")
    call = _index(job, _CALL)
    stop = _index(job, "- name: Deploy SongPlayer")
    assert call < stop, "the auto-start must be delivered before the stop"


def test_pps_deploy_delivers_it_after_the_box_check_before_the_stop():
    job = _job(_PP, "deploy-pp")
    check = _index(job, "- name: Check the build and PP's task")
    call = _index(job, _CALL)
    stop = _index(job, "- name: Stop SongPlayer")
    assert check < call < stop, "PP's box check first, then the delivery, then the stop"


def test_each_deploy_runs_it_as_a_child_and_fails_the_step_on_its_exit_code():
    """A throw inside the script must never end the calling step half-way,
    and a non-zero exit must fail the step (nothing stopped yet)."""
    for workflow, job in ((_CI, "deploy-resolume"), (_PP, "deploy-pp")):
        lines = _job(workflow, job)
        call = _index(lines, _CALL)
        assert "powershell.exe -NoProfile -NonInteractive" in lines[call]
        assert "-ExecutionPolicy Bypass -File" in lines[call]
        after = "\n".join(lines[call : call + 6])
        assert "$LASTEXITCODE" in after
        assert "exit 1" in after


def test_the_script_is_ascii_and_never_exits_inside_an_iex_run():
    text = _script()
    assert (
        text.isascii()
    ), "Windows PowerShell 5.1 reads a script without a BOM as cp1252"
    assert (
        "$ErrorActionPreference = 'Stop'" in text
        or '$ErrorActionPreference = "Stop"' in text
    )
    # The setup-prod-pc main guard: `exit` only when run as a file.
    assert "PSCommandPath" in text


def test_ci_runs_its_pester_tests_and_analyzes_it():
    ci = _CI.read_text(encoding="utf-8")
    assert _PESTER.exists()
    assert "scripts/tests/runner-autostart.Tests.ps1" in ci
    assert "scripts/runner-autostart.ps1" in ci.split("PSScriptAnalyzer", 1)[1]


def test_the_workflow_steps_stay_ascii():
    for workflow in (_CI, _PP):
        assert (
            ps_ascii.violations(workflow.read_text(encoding="utf-8")) == []
        ), workflow.name


def test_pps_deploy_skips_the_delivery_for_a_build_older_than_the_script():
    """A dispatch of an older build on purpose (a rollback: #251, PP back to
    0.82.0 before an event, run 39010164786) checks out a tree with no
    runner-autostart.ps1. The step must say so and go on, the auto-start
    left as the last deploy delivered it, never fail the rollback."""
    lines = _job(_PP, "deploy-pp")
    step = _index(lines, "- name: Deliver the runner's auto-start")
    call = _index(lines, _CALL)
    guard = "\n".join(lines[step:call])
    assert "Test-Path" in guard, "the step must look for the script first"
    assert "exit 0" in guard, "a build without the script must not fail the deploy"
    assert "::warning::" in guard, "the skip must be visible in the run"
