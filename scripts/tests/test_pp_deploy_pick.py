"""#229: which CI run's build the PP site gets (scripts/pp_deploy_pick.py).

PP's wall takes a build only from a completed, green `push` run of `CI` in
this repository: a release (deploy-pp.yml's `workflow_run`) or a run the
main session dispatches on purpose (`ci_run_id`). Anything else is refused
before PP is touched.
"""

from __future__ import annotations

import json
import re

import pytest
from pp_deploy_pick import Refused, main, pick

REPO = "zbynekdrlik/songplayer"
SHA = "0123456789abcdef" * 2 + "01234567"


def run(**over):
    base = {
        "id": 37600000001,
        "name": "CI",
        "event": "push",
        "status": "completed",
        "conclusion": "success",
        "head_branch": "main",
        "head_sha": SHA,
        "head_repository": {"full_name": REPO},
    }
    base.update(over)
    return base


def test_a_green_push_run_of_ci_gives_its_id_and_commit():
    assert pick(run(), REPO) == (37600000001, SHA)


def test_a_dev_run_is_taken_on_purpose_too():
    assert pick(run(head_branch="dev"), REPO) == (37600000001, SHA)


@pytest.mark.parametrize(
    ("over", "reason"),
    [
        ({"name": "Release"}, "'Release', not 'CI'"),
        ({"event": "pull_request"}, "event 'pull_request', not a push"),
        ({"status": "in_progress"}, "status 'in_progress', not completed"),
        ({"conclusion": "failure"}, "conclusion 'failure', not success"),
        ({"conclusion": None}, "conclusion None, not success"),
        (
            {"head_repository": {"full_name": "someone/songplayer"}},
            "'someone/songplayer'",
        ),
        ({"head_repository": None}, "None"),
    ],
)
def test_anything_but_a_green_push_run_of_ci_here_is_refused(over, reason):
    with pytest.raises(Refused, match=re.escape(reason)):
        pick(run(**over), REPO)


@pytest.mark.parametrize("bad_id", [0, -1, True, "37600000001", None, 1.5])
def test_a_run_without_a_positive_integer_id_is_refused(bad_id):
    with pytest.raises(Refused, match="no valid run id"):
        pick(run(id=bad_id), REPO)


def test_the_smallest_run_id_is_taken():
    assert pick(run(id=1), REPO) == (1, SHA)


@pytest.mark.parametrize(
    "bad_sha", ["", SHA[:39], SHA + "0", SHA.upper(), "g" * 40, None]
)
def test_a_run_without_a_full_commit_sha_is_refused(bad_sha):
    with pytest.raises(Refused, match="no valid head_sha"):
        pick(run(head_sha=bad_sha), REPO)


def test_a_non_object_is_refused():
    with pytest.raises(Refused, match="not a JSON object"):
        pick([run()], REPO)


def test_main_prints_the_outputs_and_a_log_line(tmp_path, capsys):
    path = tmp_path / "run.json"
    path.write_text(json.dumps(run()), encoding="utf-8")
    assert main(["--repo", REPO, str(path)]) == 0
    out, err = capsys.readouterr()
    assert out == f"run_id=37600000001\nhead_sha={SHA}\n"
    assert "PP gets CI run 37600000001" in err
    assert "branch 'main'" in err


def test_main_refuses_with_one_line_and_no_output(tmp_path, capsys):
    path = tmp_path / "run.json"
    path.write_text(json.dumps(run(conclusion="failure")), encoding="utf-8")
    assert main(["--repo", REPO, str(path)]) == 1
    out, err = capsys.readouterr()
    assert out == ""
    assert err.startswith("FAIL: ")
    assert err.count("\n") == 1


def test_main_names_only_the_position_of_bad_json(tmp_path, capsys):
    path = tmp_path / "run.json"
    path.write_text('{"name": "CI",\n "id": }', encoding="utf-8")
    assert main(["--repo", REPO, str(path)]) == 1
    out, err = capsys.readouterr()
    assert out == ""
    assert "line 2 column 8" in err


def test_a_branch_name_cannot_start_a_workflow_command(tmp_path, capsys):
    # The log line goes to the job log: a branch name is printed quoted, so
    # a line break in it can never begin a `::` workflow command.
    path = tmp_path / "run.json"
    path.write_text(json.dumps(run(head_branch="x\n::error::boom")), encoding="utf-8")
    assert main(["--repo", REPO, str(path)]) == 0
    _, err = capsys.readouterr()
    assert "\n::error::" not in err
