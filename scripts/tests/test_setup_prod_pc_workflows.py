"""#255 lane 2: every deploy path applies the production PC profile between
SongPlayer's stop and its start.

`scripts/setup-prod-pc.ps1` applies what SongPlayer's environment check
(lane 1) and the post-deploy gate read: Windows Update, MMCSS, the SoundGrid
adapter, power, Defender and the firewall. SongPlayer itself never changes a
system setting; the script does, while SongPlayer is stopped (an adapter
restart costs nothing then). So in every workflow, each time SongPlayer is
started again after a stop, the script ran in between. And a failed or hung
profile step must never leave the box without SongPlayer: the start still
runs, the job still fails.

The stop / start vocabulary is the Spout quiesce guard's
(`test_spout_quiesce_workflows.py`): one reading of what a step does to
SongPlayer. A script call is the path with a backslash (how a step calls
it); a message naming `scripts/setup-prod-pc.ps1` is no call.
"""

import re
from pathlib import Path

import check_workflow_ps_ascii as ps_ascii
import test_spout_quiesce_workflows as songplayer

_REPO = Path(__file__).resolve().parents[2]
_WORKFLOWS = _REPO / ".github" / "workflows"
_SCRIPT = _REPO / "scripts" / "setup-prod-pc.ps1"
_PESTER = _REPO / "scripts" / "tests" / "setup-prod-pc.Tests.ps1"
# A run of the script: its path as a step calls it, or the start of the
# process the E2E restart step built from it.
_SETUP = re.compile(r"scripts\\setup-prod-pc\.ps1|::Start\(\$setupRun\)")
# `$Name = "literal"`: a step's plain string variable (`$TaskName`).
_ASSIGN = re.compile(r'^\$(\w+)\s*=\s*"([^"$]*)"\s*$')
_CHILD = (
    "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "
    '"$env:GITHUB_WORKSPACE\\scripts\\setup-prod-pc.ps1"'
)


def _workflow(name: str) -> str:
    return (_WORKFLOWS / name).read_text(encoding="utf-8")


def _resolved(code: list[tuple[int, str]]) -> list[tuple[int, str]]:
    """The step's code with its plain string variables put in (so
    `schtasks /run /tn $TaskName` reads as the start of SongPlayer)."""
    values: dict[str, str] = {}
    out = []
    for no, line in code:
        for name, value in values.items():
            line = re.sub(
                r"\$" + name + r"\b", lambda _, v=value: v, line, flags=re.IGNORECASE
            )
        assigned = _ASSIGN.match(line)
        if assigned:
            values[assigned.group(1).lower()] = assigned.group(2)
        out.append((no, line))
    return out


def _unprofiled_starts(text: str) -> list[tuple[int, str]]:
    """Every start of SongPlayer, after a stop, with no profile run since
    that stop. The walk goes through every PowerShell step of the workflow
    in order (a stop and its start are often in different steps of one
    job); SongPlayer runs when the walk begins. A stop of what is already
    stopped (the PP start step ends a dead task instance first) changes
    nothing."""
    running = True
    profiled = False
    bad = []
    for _, step in ps_ascii._steps(text.split("\n")):
        if not ps_ascii._is_ps_step(step):
            continue
        for no, line in _resolved(songplayer._code(step)):
            if _SETUP.search(line):
                if not running:
                    profiled = True
            elif songplayer._is_start(line):
                if not running and not profiled:
                    bad.append((no, line))
                running = True
            elif songplayer._is_stop(line) and running:
                running = False
                profiled = False
    return bad


def _step(text: str, name_prefix: str) -> str:
    """The text of the one step whose `- name:` starts with `name_prefix`."""
    lines = text.split("\n")
    starts = [
        i
        for i, line in enumerate(lines)
        if line.strip().startswith(f"- name: {name_prefix}")
    ]
    assert len(starts) == 1, f"one step named {name_prefix!r}, found {len(starts)}"
    start = starts[0]
    indent = len(lines[start]) - len(lines[start].lstrip(" "))
    end = start + 1
    while end < len(lines):
        stripped = lines[end].strip()
        if (
            stripped
            and not stripped.startswith("#")
            and len(lines[end]) - len(lines[end].lstrip(" ")) <= indent
        ):
            break
        end += 1
    return "\n".join(lines[start:end])


def _step_names(text: str) -> list[str]:
    return [
        line.strip()[len("- name: ") :]
        for line in text.split("\n")
        if line.strip().startswith("- name: ")
    ]


def _index(names: list[str], prefix: str) -> int:
    found = [i for i, name in enumerate(names) if name.startswith(prefix)]
    assert len(found) == 1, f"one step named {prefix!r}, found {len(found)}"
    return found[0]


def _ps_step(*lines: str) -> str:
    body = "\n".join(f"          {line}" for line in lines)
    return f"jobs:\n  j:\n    steps:\n      - name: s\n        shell: powershell\n        run: |\n{body}\n"


def test_the_walk_finds_a_start_with_no_profile_since_the_stop():
    gap = _ps_step(
        'taskkill /F /IM "SongPlayer.exe" 2>&1 | Out-Null',
        "schtasks.exe /run /tn SongPlayer",
    )
    assert [line for _, line in _unprofiled_starts(gap)] == [
        "schtasks.exe /run /tn SongPlayer"
    ]
    covered = _ps_step(
        'taskkill /F /IM "SongPlayer.exe" 2>&1 | Out-Null',
        _CHILD,
        '$TaskName = "SongPlayer"',
        "schtasks.exe /run /tn $TaskName",
    )
    assert _unprofiled_starts(covered) == []
    # A profile run BEFORE the stop covers nothing; a relaunched process
    # stopped again needs its own; a message naming the script is no call.
    early = _ps_step(
        _CHILD,
        'Stop-ScheduledTask -TaskName "SongPlayer"',
        'Write-Host "scripts/setup-prod-pc.ps1 ran"',
        "Start-ScheduledTask -TaskName SongPlayer",
    )
    assert len(_unprofiled_starts(early)) == 1
    again = _ps_step(
        'Stop-ScheduledTask -TaskName "SongPlayer"',
        _CHILD,
        "schtasks.exe /run /tn SongPlayer",
        'taskkill /F /IM "SongPlayer.exe" 2>&1 | Out-Null',
        "Start-ScheduledTask -TaskName SongPlayer",
    )
    assert [line for _, line in _unprofiled_starts(again)] == [
        "Start-ScheduledTask -TaskName SongPlayer"
    ]
    # Ending a task instance that is already stopped resets nothing.
    dead = _ps_step(
        'Stop-ScheduledTask -TaskName "SongPlayer"',
        _CHILD,
        'Stop-ScheduledTask -TaskName "SongPlayer"',
        'schtasks.exe /run /tn "SongPlayer"',
    )
    assert _unprofiled_starts(dead) == []


def test_every_start_after_a_stop_on_snv_runs_the_profile_first():
    text = _workflow("ci.yml")
    assert _unprofiled_starts(text) == []
    # The deploy (its step), the E2E restart and the restart's fallback.
    assert len(_SETUP.findall(text)) >= 4


def test_every_start_after_a_stop_at_pp_runs_the_profile_first():
    text = _workflow("deploy-pp.yml")
    assert _unprofiled_starts(text) == []
    assert len(_SETUP.findall(text)) == 1


def test_the_old_workflows_would_fail_the_walk():
    # The walk is no tautology: without the profile steps, the deploy's
    # launch, the restart's relaunch and PP's start are each a gap.
    for name, starts in (("ci.yml", 2), ("deploy-pp.yml", 1)):
        text = _workflow(name)
        stripped = "\n".join(
            line for line in text.split("\n") if not _SETUP.search(line)
        )
        assert len(_unprofiled_starts(stripped)) >= starts, name


def test_snv_starts_songplayer_after_a_failed_profile_step():
    ci = _workflow("ci.yml")
    deploy = ci.split("\n  deploy-resolume:", 1)[1].split("\n  e2e-resolume:", 1)[0]
    names = _step_names(deploy)
    stop = _index(names, "Deploy SongPlayer")
    profile = _index(names, "Apply the production PC profile")
    launch = _index(names, "Configure auto-start and launch")
    assert stop < profile < launch
    assert "id: deploy" in _step(deploy, "Deploy SongPlayer")
    step = _step(deploy, "Apply the production PC profile")
    assert _CHILD in step, "a child process: a throw in it never ends the job's steps"
    assert "timeout-minutes: 5" in step, "a hang fails the step, the launch still runs"
    assert "$LASTEXITCODE" in step and "exit 1" in step
    assert "if:" not in step
    launch_step = _step(deploy, "Configure auto-start and launch")
    assert "if: ${{ !cancelled() && steps.deploy.outcome == 'success' }}" in launch_step


def test_the_e2e_restart_relaunches_songplayer_before_it_fails_on_the_profile():
    restart = _step(_workflow("ci.yml"), "Restart SongPlayer")
    assert (
        "$setupRun = New-Object System.Diagnostics.ProcessStartInfo('powershell.exe', "
        '"-NoProfile -NonInteractive -ExecutionPolicy Bypass -File '
        '`"$env:GITHUB_WORKSPACE\\scripts\\setup-prod-pc.ps1`"")'
    ) in restart
    assert "$setupRun.UseShellExecute = $false" in restart, "no window on the live box"
    # Bounded: this step has no timeout of its own, a hang must not keep
    # SongPlayer down.
    assert restart.count(".WaitForExit(300000)") == 2
    assert restart.count("$setup.Kill()") == 2
    relaunch = restart.index("schtasks.exe /run /tn SongPlayer")
    checked = restart.index("if ($prodPcExit -ne 0)")
    # The first `exit 0` after the relaunch (an earlier one is the #196
    # skip, before anything is stopped).
    ok = restart.index("exit 0", relaunch)
    assert restart.index("$prodPcExit = $setup.ExitCode") < relaunch < checked < ok


def test_pp_starts_songplayer_after_a_failed_profile_step():
    text = _workflow("deploy-pp.yml")
    names = _step_names(text)
    install = _index(names, "Install SongPlayer")
    profile = _index(names, "Apply the production PC profile")
    start = _index(names, "Start SongPlayer")
    assert install < profile < start
    step = _step(text, "Apply the production PC profile")
    assert _CHILD in step
    assert "timeout-minutes: 5" in step
    assert "$LASTEXITCODE" in step and "exit 1" in step
    assert "if:" not in step
    assert "if: always()" in _step(text, "Start SongPlayer")


def test_the_script_is_tested_by_pester_and_planned_on_windows():
    ci = _workflow("ci.yml")
    job = ci.split("\n  setup-prod-pc:", 1)[1].split("\n\n  ", 1)[0]
    assert "runs-on: ubuntu-latest" in job
    assert "scripts/tests/setup-prod-pc.Tests.ps1" in job
    assert "if ($result.PassedCount -lt 1) { throw 'Pester: no test ran' }" in job
    assert "$result.SkippedCount -gt 0 -or $result.NotRunCount -gt 0" in job
    assert "Invoke-ScriptAnalyzer -Path $path -Severity Error" in job
    assert "PSUseCompatibleSyntax" in job and "'5.1'" in job
    assert "continue-on-error" not in job
    gate = ci.split("\n  gate:", 1)[1].split("    steps:", 1)[0]
    assert "      - setup-prod-pc\n" in gate
    windows = ci.split("\n  build-windows:", 1)[1].split("\n  test-integrity:", 1)[0]
    plan = _step(windows, "Plan the production PC profile on the runner")
    assert "shell: powershell" in plan, "Windows PowerShell 5.1, the boxes' shell"
    assert "scripts\\setup-prod-pc.ps1 -Plan" in plan
    assert "Read-ProdPcAdapters" in plan and "Get-ProdPcUserAppData" in plan
    assert ps_ascii.violations(ci) == []
    assert ps_ascii.violations(_workflow("deploy-pp.yml")) == []


def test_the_script_and_its_tests_exist_and_the_script_is_ascii_and_strict():
    text = _SCRIPT.read_text(encoding="utf-8")
    assert all(ord(ch) < 128 for ch in text), "Windows PowerShell 5.1 reads cp1252"
    assert "$ErrorActionPreference = 'Stop'" in text
    assert "Set-StrictMode -Version Latest" in text
    assert (
        "irm https://raw.githubusercontent.com/zbynekdrlik/songplayer/main/scripts/setup-prod-pc.ps1 | iex"
        in text
    )
    assert "function Get-ProdPcPlan" in text
    pester = _PESTER.read_text(encoding="utf-8")
    assert "Get-ProdPcPlan" in pester
    for fixture in ("pp", "snv", "fresh"):
        assert (
            _REPO / "scripts" / "tests" / "fixtures" / "prod-pc" / f"{fixture}.json"
        ).is_file(), fixture
