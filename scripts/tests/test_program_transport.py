"""#308: scripts/program_transport.py puts SP-program's playlist back as the
post-deploy E2E found it (a paused playlist the restore cut re-kicked is
paused again)."""

import json
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import program_transport as pt  # noqa: E402


class Box:
    """A fake SongPlayer: SP-program's source and each playlist's transport;
    a pause stops the playlist after `pause_lag` health reads."""

    def __init__(self, source, transports, pause_lag=0, pause_works=True):
        self.source = source
        self.transports = dict(transports)
        self.pause_lag = pause_lag
        self.pause_works = pause_works
        self.posts = []
        self.pending = None

    def get(self, path):
        if path == "/api/v1/program":
            return {"source": self.source, "degraded_reason": None}
        if path == "/api/v1/ndi/health":
            if self.pending is not None:
                pid, left = self.pending
                if left == 0:
                    self.transports[pid] = "Paused"
                    self.pending = None
                else:
                    self.pending = (pid, left - 1)
            return [
                {"playlist_id": p, "transport": t} for p, t in self.transports.items()
            ]
        raise AssertionError(path)

    def post(self, path):
        self.posts.append(path)
        pid = int(path.split("/")[4])
        if self.pause_works:
            self.pending = (pid, self.pause_lag)


def run(tmp_path, start_box, end_box, **kw):
    f = tmp_path / "start.json"
    pt.save(f, start_box.get)
    return pt.restore(f, end_box.get, end_box.post, sleep=lambda _s: None, **kw)


def test_a_paused_start_playlist_the_suite_restarted_is_paused_again(tmp_path):
    start = Box(7, {5: "Idle", 7: "Paused"})
    end = Box(7, {5: "Idle", 7: "Playing"})
    line = run(tmp_path, start, end)
    assert end.posts == ["/api/v1/playback/7/pause"]
    assert end.transports[7] == "Paused"
    assert line.startswith("paused playlist 7 again")


def test_an_idle_start_playlist_is_paused_again_too(tmp_path):
    end = Box(7, {7: "Playing"})
    run(tmp_path, Box(7, {7: "Idle"}), end)
    assert end.posts == ["/api/v1/playback/7/pause"]


def test_a_playlist_that_was_playing_is_left_playing(tmp_path):
    end = Box(7, {7: "Playing"})
    assert run(tmp_path, Box(7, {7: "Playing"}), end).startswith("left as it is")
    assert end.posts == []


def test_another_source_on_program_now_is_left_alone(tmp_path):
    end = Box(5, {5: "Playing", 7: "Playing"})
    run(tmp_path, Box(7, {5: "Idle", 7: "Paused"}), end)
    assert end.posts == []


def test_a_start_playlist_still_paused_is_left_alone(tmp_path):
    end = Box(7, {7: "Paused"})
    run(tmp_path, Box(7, {7: "Paused"}), end)
    assert end.posts == []


@pytest.mark.parametrize("source", [-1, -2, None])
def test_no_playlist_on_program_at_the_start_pauses_nothing(tmp_path, source):
    end = Box(source, {7: "Playing"})
    run(tmp_path, Box(source, {7: "Paused"}), end)
    assert end.posts == []


def test_the_saved_state_names_the_source_and_its_transport(tmp_path):
    f = tmp_path / "s.json"
    pt.save(f, Box(7, {5: "Playing", 7: "Paused"}).get)
    assert json.loads(f.read_text()) == {"source": 7, "transport": "Paused"}
    pt.save(f, Box(-1, {7: "Playing"}).get)
    assert json.loads(f.read_text()) == {"source": -1, "transport": None}
    pt.save(f, Box(9, {7: "Playing"}).get)
    assert json.loads(f.read_text()) == {"source": 9, "transport": None}


def test_a_pause_read_back_after_a_lag_passes(tmp_path):
    end = Box(7, {7: "Playing"}, pause_lag=3)
    assert run(tmp_path, Box(7, {7: "Paused"}), end).startswith("paused playlist 7")


def test_a_pause_that_never_takes_fails_loudly(tmp_path):
    end = Box(7, {7: "Playing"}, pause_works=False)
    with pytest.raises(RuntimeError, match="still plays"):
        run(tmp_path, Box(7, {7: "Paused"}), end, readback_s=1.0)


def test_to_pause_table():
    assert (
        pt.to_pause(
            {"source": 0, "transport": "Paused"}, {"source": 0, "transport": "Playing"}
        )
        == 0
    )
    assert (
        pt.to_pause(
            {"source": 3, "transport": None}, {"source": 3, "transport": "Playing"}
        )
        == 3
    )
    assert (
        pt.to_pause(
            {"source": 3, "transport": "Paused"}, {"source": 3, "transport": None}
        )
        is None
    )


def test_main_usage_and_both_commands(tmp_path, capsys):
    assert pt.main([]) == 2
    assert pt.main(["load", "x"]) == 2
    f = str(tmp_path / "s.json")
    box = Box(7, {7: "Paused"})
    assert pt.main(["save", f], get=box.get, post=box.post) == 0
    box.transports[7] = "Playing"
    assert pt.main(["restore", f], get=box.get, post=box.post) == 0
    out = capsys.readouterr().out
    assert "[#308] SP-program at the start" in out
    assert "paused playlist 7 again" in out


def test_a_health_answer_that_is_no_list_is_refused():
    with pytest.raises(ValueError):
        pt.transport_of({"rows": []}, 7)


def _steps(job: str) -> list[str]:
    """The step names of `job` in ci.yml, in order (a line scan: the
    workflow is parsed by GitHub, not by this test)."""
    text = (
        Path(__file__).resolve().parents[2] / ".github/workflows/ci.yml"
    ).read_text()
    lines = text.splitlines()
    start = lines.index(f"  {job}:")
    names = []
    for line in lines[start + 1 :]:
        if (
            line.startswith("  ")
            and not line.startswith("   ")
            and line.strip().endswith(":")
        ):
            break
        if line.strip().startswith("- name:"):
            names.append(line.strip()[len("- name:") :].strip())
    return names


def test_the_record_is_taken_in_the_deploy_job_before_its_stop():
    """The deploy's own restart plays SP-program's playlist (dev run
    43111838795: recorded `Playing` in the E2E job after it), so the record
    must be read before the deploy stops SongPlayer."""
    deploy = _steps("deploy-resolume")
    record = "Record SP-program's playlist transport (#308)"
    assert record in deploy
    assert deploy.index(record) < deploy.index("Deploy SongPlayer")
    assert record not in _steps("e2e-resolume")


def test_the_restore_reads_the_deploy_jobs_record():
    text = (
        Path(__file__).resolve().parents[2] / ".github/workflows/ci.yml"
    ).read_text()
    assert "program_start: ${{ steps.program_start.outputs.program_start }}" in text
    assert "needs.deploy-resolume.outputs.program_start" in text
