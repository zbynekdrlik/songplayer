#!/usr/bin/env python3
"""Which CI run's build the PP site gets (#229, deploy-pp.yml's `resolve` job).

PP (the `resolume-pp` runner) drives a church site's live wall. It takes
main releases only (a `workflow_run` of CI on main), or a build the main
session dispatches on purpose (`workflow_dispatch -f ci_run_id=<run>`).
Either way the build must come from a completed, green `push` run of the
`CI` workflow of this repository. Refused, before PP is touched: a run whose
Gate went red (a failed test, a surviving mutant), a `pull_request` run (it
builds nothing; a fork's head branch may even be named `main`), a run still
in progress, and another repository's run.

Input: one workflow run as GitHub returns it (`GET
/repos/{owner}/{repo}/actions/runs/{id}`, or a `workflow_run` event's
`workflow_run` object), in a JSON file. Output: the lines `run_id=<id>` and
`head_sha=<sha>` on stdout (the job appends them to `$GITHUB_OUTPUT`) and
one log line on stderr; a refusal is one `FAIL:` line on stderr, nothing on
stdout, exit 1.

Usage: python3 scripts/pp_deploy_pick.py --repo OWNER/NAME RUN_JSON
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections.abc import Sequence
from pathlib import Path
from typing import Any

WORKFLOW = "CI"
_SHA = re.compile(r"[0-9a-f]{40}")


class Refused(Exception):
    """Why a run's build may not go to PP."""


def pick(run: Any, repo: str) -> tuple[int, str]:
    """`(run id, head sha)` of a run whose build PP may take; else `Refused`."""
    if not isinstance(run, dict):
        raise Refused("the run is not a JSON object")
    name = run.get("name")
    if name != WORKFLOW:
        raise Refused(f"the run is the workflow {name!r}, not {WORKFLOW!r}")
    event = run.get("event")
    if event != "push":
        raise Refused(f"the run's event {event!r}, not a push: it built nothing for PP")
    status = run.get("status")
    if status != "completed":
        raise Refused(f"the run's status {status!r}, not completed")
    conclusion = run.get("conclusion")
    if conclusion != "success":
        raise Refused(f"the run's conclusion {conclusion!r}, not success")
    head_repo = run.get("head_repository")
    head_name = head_repo.get("full_name") if isinstance(head_repo, dict) else None
    if head_name != repo:
        raise Refused(f"the run's commit comes from {head_name!r}, not {repo!r}")
    run_id = run.get("id")
    if not isinstance(run_id, int) or isinstance(run_id, bool) or run_id < 1:
        raise Refused(f"no valid run id ({run_id!r})")
    head_sha = run.get("head_sha")
    if not isinstance(head_sha, str) or not _SHA.fullmatch(head_sha):
        raise Refused(f"no valid head_sha ({head_sha!r})")
    return run_id, head_sha


def main(argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--repo", required=True, help="OWNER/NAME of this repository")
    parser.add_argument("run_json", type=Path, help="the workflow run, as JSON")
    args = parser.parse_args(argv)
    try:
        try:
            run = json.loads(args.run_json.read_text(encoding="utf-8"))
        except json.JSONDecodeError as e:
            # The position only: the text could be anything.
            raise Refused(
                f"the run is not JSON (line {e.lineno} column {e.colno})"
            ) from None
        run_id, head_sha = pick(run, args.repo)
    except Refused as e:
        print(f"FAIL: {e}", file=sys.stderr)
        return 1
    branch = run.get("head_branch")
    # !r keeps a branch name on one line: a line break in it could start a
    # `::` workflow command in the job log.
    print(
        f"PP gets CI run {run_id} (commit {head_sha}, branch {branch!r})",
        file=sys.stderr,
    )
    print(f"run_id={run_id}")
    print(f"head_sha={head_sha}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
