"""#229: PP gets main releases only, on its own runner label (deploy-pp.yml).

A deploy-pp job that ran on a dev push, or on a runner SNV's jobs can land
on, would put a dev build on the PP site's live wall. A deploy that stopped
SongPlayer and then failed would leave that wall dark.
"""

import re
from pathlib import Path

import check_workflow_ps_ascii as ps_ascii

_REPO = Path(__file__).resolve().parents[2]
_WORKFLOWS = _REPO / ".github" / "workflows"
_PP_RUNNER = "runs-on: [self-hosted, windows, resolume-pp]"
# The resolve job's `if:` and the concurrency group's real-deploy branch
# must be the same condition (see the group's comment in deploy-pp.yml).
_REAL_DEPLOY = re.compile(
    r"github\.event_name == 'workflow_dispatch'\s*\|\|\s*"
    r"\(github\.event\.workflow_run\.conclusion == 'success'\s*&&\s*"
    r"github\.event\.workflow_run\.event == 'push'\s*&&\s*"
    r"github\.event\.workflow_run\.head_branch == 'main'\s*&&\s*"
    r"github\.event\.workflow_run\.head_sha == github\.sha\)"
)


def _deploy_pp() -> str:
    return (_WORKFLOWS / "deploy-pp.yml").read_text(encoding="utf-8")


def _step(text: str, name_prefix: str) -> str:
    """The text of the step whose `- name:` starts with `name_prefix`."""
    lines = text.split("\n")
    head = f"- name: {name_prefix}"
    starts = [i for i, line in enumerate(lines) if line.strip().startswith(head)]
    assert len(starts) == 1, f"one step named {name_prefix!r}, found {len(starts)}"
    start = starts[0]
    indent = len(lines[start]) - len(lines[start].lstrip(" "))
    end = start + 1
    while end < len(lines):
        line = lines[end]
        stripped = line.strip()
        ind = len(line) - len(line.lstrip(" "))
        if stripped and not stripped.startswith("#") and ind <= indent:
            break
        end += 1
    return "\n".join(lines[start:end])


def _conditions(text: str) -> list[str]:
    """Every `if:` condition, a folded one (`if: >-`) joined into one line."""
    lines = text.split("\n")
    out: list[str] = []
    for i, line in enumerate(lines):
        if not line.strip().startswith("if:"):
            continue
        indent = len(line) - len(line.lstrip(" "))
        parts = [line.strip()]
        for nxt in lines[i + 1 :]:
            if nxt.strip() and len(nxt) - len(nxt.lstrip(" ")) <= indent:
                break
            parts.append(nxt.strip())
        out.append(" ".join(p for p in parts if p))
    return out


def _step_names(text: str) -> list[str]:
    return [
        line.strip()[len("- name: ") :]
        for line in text.split("\n")
        if line.strip().startswith("- name: ")
    ]


def test_pp_deploys_a_green_main_push_or_an_explicit_dispatch_only():
    text = _deploy_pp()
    assert "workflow_run:" in text
    assert "workflows: [CI]" in text
    assert "branches: [main]" in text
    assert "workflow_dispatch:" in text
    assert "github.event.workflow_run.conclusion == 'success'" in text
    assert "github.event.workflow_run.event == 'push'" in text
    assert "github.event.workflow_run.head_branch == 'main'" in text
    assert "push:" not in text, "never on a push of its own"
    assert "pull_request" not in text


def test_a_dispatched_run_goes_through_the_same_pick_as_a_release():
    resolve = _step(_deploy_pp(), "Resolve the CI run")
    assert "scripts/pp_deploy_pick.py" in resolve
    assert '--repo "$GITHUB_REPOSITORY"' in resolve
    assert "actions/runs/$RUN_ID_INPUT" in resolve
    assert "toJSON(github.event.workflow_run)" in resolve


def test_every_pp_job_runs_on_the_pp_runner_and_no_other_workflow_does():
    text = _deploy_pp()
    assert text.count(_PP_RUNNER) == 2
    assert "[self-hosted, windows, resolume]" not in text
    for workflow in _WORKFLOWS.glob("*.yml"):
        if workflow.name == "deploy-pp.yml":
            continue
        other = workflow.read_text(encoding="utf-8")
        assert "resolume-pp" not in other, workflow.name
        # The converse: a self-hosted job elsewhere names SNV's label, so it
        # can never be picked up by the PP runner. Every runs-on is a one-line
        # literal there (a block list or an expression would escape this read).
        for line in other.split("\n"):
            if not line.strip().startswith("runs-on:"):
                continue
            value = line.split("runs-on:", 1)[1].strip()
            assert value and "${{" not in value, f"{workflow.name}: {line}"
            if "self-hosted" in value:
                assert re.search(r"\bresolume\]", value), f"{workflow.name}: {line}"


def test_a_rerun_of_an_older_main_run_never_reaches_pp():
    # GitHub fires `workflow_run` `completed` for every attempt of a run: a
    # re-run of an OLDER main CI run (the SNV restart recipe re-runs a Deploy
    # job) ends green too. Only a run whose commit is main's tip at that
    # moment deploys (`github.sha` of a workflow_run event = the default
    # branch's last commit); an older build reaches PP only by a dispatch.
    text = _deploy_pp()
    group = next(line for line in text.split("\n") if line.strip().startswith("group:"))
    assert "github.event.workflow_run.head_sha == github.sha" in group
    resolve_if = next(c for c in _conditions(text) if "workflow_dispatch" in c)
    assert "github.event.workflow_run.head_sha == github.sha" in resolve_if


def test_pp_deploys_in_its_own_group_and_never_cancels_one_in_flight():
    text = _deploy_pp()
    assert "cancel-in-progress: false" in text
    group = next(line for line in text.split("\n") if line.strip().startswith("group:"))
    assert "'deploy-pp'" in group
    assert "format('deploy-pp-skipped-{0}', github.run_id)" in group


def test_only_a_real_deploy_takes_the_pp_group():
    # A CI run on main that failed or was cancelled still starts this
    # workflow (it then skips). In the shared group it would replace a
    # release deploy that is pending there, and that release would never
    # reach PP. So it gets a group of its own, by the resolve job's own test.
    text = _deploy_pp()
    group = next(line for line in text.split("\n") if line.strip().startswith("group:"))
    assert _REAL_DEPLOY.search(group), group
    resolve_if = text.split("  resolve:", 1)[1].split("outputs:", 1)[0]
    assert _REAL_DEPLOY.search(" ".join(resolve_if.split())), resolve_if


def test_pp_powershell_is_ascii():
    assert ps_ascii.violations(_deploy_pp()) == []


def test_the_build_is_downloaded_and_checked_before_songplayer_stops():
    # An expired or missing artifact must fail the deploy while PP still
    # runs its old build.
    names = _step_names(_deploy_pp())
    stop = names.index("Stop SongPlayer")
    for before in (
        "Download Tauri installer",
        "Download WASM frontend",
        "Check the build",
    ):
        assert (
            names.index(next(n for n in names if n.startswith(before))) < stop
        ), before
    check = _step(_deploy_pp(), "Check the build")
    assert "$installers.Count -ne 1" in check
    # The phase-0 task too: without it the deploy could stop SongPlayer and
    # never start it again.
    assert 'Get-ScheduledTask -TaskName "SongPlayer"' in check
    # The box itself: a runner that carries resolume-pp by mistake (labels
    # can be edited later) never stops and installs anything.
    assert '$env:COMPUTERNAME -ine "RESOLUME-PP"' in check


def test_a_rerun_of_an_old_deploy_pp_run_never_installs_an_older_build():
    # GitHub re-runs a workflow with its ORIGINAL event and github.sha, so a
    # re-run of an old deploy-pp run (the button on one that failed after
    # its 24 h queue) passes the resolve job's main-tip test again. The
    # check step reads main's LIVE tip before anything stops.
    check = _step(_deploy_pp(), "Check the build")
    assert "repos/$env:GITHUB_REPOSITORY/commits/main" in check
    assert '$env:EVENT_NAME -eq "workflow_run"' in check
    assert "$tip -ne $env:HEAD_SHA" in check


def test_the_stop_and_the_install_are_bounded_on_their_own():
    # A hung stop or installer would otherwise run until the job's timeout,
    # with SongPlayer stopped; a step timeout is a plain step failure, after
    # which the always() Start step brings SongPlayer back.
    for name, minutes in (("Stop SongPlayer", 5), ("Install SongPlayer", 10)):
        assert f"timeout-minutes: {minutes}" in _step(_deploy_pp(), name), name


def test_the_install_checks_the_installers_exit_code():
    install = _step(_deploy_pp(), "Install SongPlayer")
    assert "-Wait -PassThru" in install
    assert "$p.ExitCode -ne 0" in install


def test_songplayer_is_started_again_whatever_happened_before():
    # A failed install or a cancel ("produkcia bezi") after the stop must
    # never leave PP's wall without SongPlayer. `always()` is allowed on this
    # one step only; every job uses the default `success()`. The step proves
    # SongPlayer answers again: "Health checks" is skipped on those paths.
    text = _deploy_pp()
    start = _step(text, "Start SongPlayer")
    assert "if: always()" in start
    assert "http://localhost:8920/api/v1/status" in start
    assert "did not come back" in start
    conditions = _conditions(text)
    assert [c for c in conditions if "always()" in c] == ["if: always()"]
    assert not [c for c in conditions if "cancelled()" in c]


def test_a_folded_condition_is_read_whole():
    folded = "    if: >-\n      a == 'b'\n      || always()\n    steps:\n"
    assert _conditions(folded) == ["if: >- a == 'b' || always()"]


def test_the_pp_deploy_never_touches_the_db_task_acl_or_firewall():
    text = _deploy_pp()
    for forbidden in (
        "Register-ScheduledTask",
        "Unregister-ScheduledTask",
        "Set-ScheduledTask",
        "New-ScheduledTask",
        "icacls",
        "Set-Acl",
        "NetFirewallRule",
        "netsh",
        "advfirewall",
        "songplayer.db",
        "/api/v1/settings",
    ):
        assert forbidden not in text, forbidden
    # schtasks only RUNS the phase-0 task (no /create, /change, /delete).
    verbs = re.findall(r"\bschtasks(?:\.exe)?\s+/(\w+)", text, re.IGNORECASE)
    assert verbs, "the deploy starts the task with schtasks /run"
    assert {v.lower() for v in verbs} == {"run"}, verbs


def test_the_pp_label_is_declared_and_the_runner_setup_takes_it():
    actionlint = (_REPO / ".github" / "actionlint.yaml").read_text(encoding="utf-8")
    assert "- resolume-pp" in actionlint
    setup = (_REPO / "scripts" / "setup-runner.ps1").read_text(encoding="utf-8")
    assert "$env:RUNNER_LABELS" in setup
    # Forgetting RUNNER_LABELS at PP must not register a `resolume` runner.
    assert '$env:COMPUTERNAME -ieq "RESOLUME-PP"' in setup
    assert '$labelList -contains "resolume"' in setup


def test_the_dist_artifact_outlives_a_powered_off_pp():
    # A deploy-pp job queued on an offline PP runner fails after 24 h; the
    # dispatch that redoes it needs the CI run's artifacts.
    ci = (_WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    upload = ci.split("      - name: Upload dist artifact", 1)[1].split("\n\n", 1)[0]
    assert "name: dist" in upload
    assert "retention-days: 5" in upload


def test_snv_never_runs_the_pp_subset_and_pp_runs_it_with_max():
    e2e = _REPO / "e2e"
    snv = (e2e / "post-deploy.config.ts").read_text(encoding="utf-8")
    assert '"**/post-deploy-pp*.spec.ts"' in snv
    pp = (e2e / "post-deploy-pp.config.ts").read_text(encoding="utf-8")
    assert (
        'testMatch: ["**/post-deploy-pp.spec.ts", "**/post-deploy-max.spec.ts"]' in pp
    )
