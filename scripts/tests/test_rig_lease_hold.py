"""#314: the rig-lease HOLD of SongPlayer's resolume-snv jobs (scripts/rig_lease_hold.py).

The acquire's wait / outage / refusal rules on a fake clock, the heartbeat
and the release, and every subcommand end to end over a real local HTTP
server that answers like camera-box's write routes (rig_lease_write.py).
"""

from __future__ import annotations

import json
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest
from rig_lease_hold import (
    EXIT_CONFIG,
    EXIT_LOST,
    EXIT_OK,
    EXIT_STILL_HELD,
    acquire,
    heartbeat,
    main,
    read_state,
    release,
)

TOKEN = "t0ken"
LEASE = "0123456789abcdef0123456789abcdef"
BODY = {
    "repo": "zbynekdrlik/songplayer",
    "run_id": "43227731000",
    "run_url": "https://github.com/zbynekdrlik/songplayer/actions/runs/43227731000",
    "job": "Deploy to win-resolume",
    "expected_secs": 3600,
    "resources": "resolume-snv",
}
HELD = (
    409,
    {
        "acquired": False,
        "holder": {
            "repo": "zbynekdrlik/camera-box",
            "job": "full-path",
            "run_url": "https://github.com/zbynekdrlik/camera-box/actions/runs/1",
            "expected_release_at": "2026-11-12T08:10:40Z",
        },
    },
)
TAKEN = (200, {"acquired": True, "lease_id": LEASE})


class Clock:
    def __init__(self):
        self.now = 0.0
        self.sleeps: list[float] = []

    def __call__(self) -> float:
        return self.now

    def sleep(self, s: float) -> None:
        self.sleeps.append(s)
        self.now += s


def scripted(*replies):
    """A poster answering `replies` in order (None = no answer)."""
    calls: list[tuple[str, dict]] = []
    it = iter(replies)

    def post(path, body):
        calls.append((path, body))
        r = next(it)
        return None if r is None else ("http://dev1:8890", r)

    post.calls = calls
    return post


def run_acquire(post, max_wait=3600.0):
    clock, logs = Clock(), []
    code, state = acquire(post, BODY, 30.0, max_wait, clock, clock.sleep, logs.append)
    return code, state, clock, logs


# ---- acquire -------------------------------------------------------------------


def test_a_free_lease_is_taken_at_once():
    post = scripted(TAKEN)
    code, state, clock, _ = run_acquire(post)
    assert (code, state) == (EXIT_OK, {"lease_id": LEASE, "url": "http://dev1:8890"})
    assert post.calls == [("/rig-lease/acquire", BODY)]
    assert clock.sleeps == []


def test_another_holder_is_waited_out_every_30_s_then_taken():
    post = scripted(HELD, HELD, TAKEN)
    code, state, clock, logs = run_acquire(post)
    assert code == EXIT_OK and state["lease_id"] == LEASE
    assert clock.sleeps == [30.0, 30.0]
    assert any("zbynekdrlik/camera-box (job full-path" in line for line in logs)


def test_a_holder_past_the_bound_fails_the_job():
    post = scripted(*([HELD] * 5))
    code, state, clock, logs = run_acquire(post, max_wait=90.0)
    assert (code, state) == (EXIT_STILL_HELD, None)
    assert clock.sleeps == [30.0, 30.0, 30.0]
    assert logs[-1].startswith("::error::")


@pytest.mark.parametrize("reply", [None, (503, {"error": "lease_io"})])
def test_a_service_that_does_not_answer_runs_the_job_unheld(reply):
    code, state, clock, logs = run_acquire(scripted(reply))
    assert (code, state) == (EXIT_OK, None)
    assert clock.sleeps == []
    assert logs[-1].startswith("::warning::")


def test_a_blip_after_a_holder_keeps_waiting_and_the_third_is_an_outage():
    post = scripted(HELD, None, None, None)
    code, state, clock, logs = run_acquire(post)
    assert (code, state) == (EXIT_OK, None)
    assert clock.sleeps == [30.0, 30.0, 30.0]
    assert logs[-1].startswith("::warning::")


def test_a_blip_then_the_lease_is_taken():
    code, state, _, _ = run_acquire(scripted(HELD, None, TAKEN))
    assert code == EXIT_OK and state["lease_id"] == LEASE


def test_a_server_without_write_routes_runs_the_job_unheld():
    code, state, _, logs = run_acquire(scripted((404, {"error": "not_found"})))
    assert (code, state) == (EXIT_OK, None)
    assert logs[-1].startswith("::warning::")


@pytest.mark.parametrize(
    "reply",
    [
        (401, {"error": "unauthorized"}),
        (400, {"error": "bad_field", "field": "resources"}),
        (200, {"acquired": True, "lease_id": "not-hex"}),
        (200, {"acquired": False}),
    ],
)
def test_a_refused_or_broken_acquire_fails_loudly(reply):
    code, state, _, logs = run_acquire(scripted(reply))
    assert (code, state) == (EXIT_CONFIG, None)
    assert logs[-1].startswith("::error::")


# ---- heartbeat / release ---------------------------------------------------------


def run_heartbeat(post, held_checks, max_seconds=5400.0):
    clock, logs = Clock(), []
    checks = iter(held_checks)
    code = heartbeat(
        post,
        LEASE,
        lambda: next(checks),
        30.0,
        max_seconds,
        clock,
        clock.sleep,
        logs.append,
    )
    return code, clock, logs


def test_the_heartbeat_beats_every_30_s_until_the_state_file_goes():
    ok = (200, {"refreshed": True})
    post = scripted(ok, ok, ok)
    code, clock, _ = run_heartbeat(post, [True, True, True, False])
    assert code == EXIT_OK
    assert post.calls == [("/rig-lease/heartbeat", {"lease_id": LEASE})] * 3
    assert clock.sleeps == [30.0] * 3


def test_an_unanswered_beat_is_tried_again():
    ok = (200, {"refreshed": True})
    post = scripted(None, (503, {}), ok)
    code, _, logs = run_heartbeat(post, [True, True, True, False])
    assert code == EXIT_OK and len(post.calls) == 3
    assert sum(line.startswith("::warning::") for line in logs) == 2


@pytest.mark.parametrize(
    "reply", [(404, {"error": "not_holder"}), (409, {"error": "over_hold"})]
)
def test_a_refused_beat_while_held_is_a_lost_lease(reply):
    code, _, logs = run_heartbeat(scripted(reply), [True, True])
    assert code == EXIT_LOST
    assert logs[-1].startswith("::error::rig lease hold: the lease is lost")


def test_a_refused_beat_right_after_the_release_is_no_loss():
    code, _, logs = run_heartbeat(
        scripted((404, {"error": "not_holder"})), [True, False]
    )
    assert code == EXIT_OK and logs == []


def test_the_heartbeat_stops_at_its_bound():
    ok = (200, {"refreshed": True})
    post = scripted(ok, ok, ok)
    code, _, _ = run_heartbeat(post, [True] * 10, max_seconds=90.0)
    assert code == EXIT_OK and len(post.calls) == 3


@pytest.mark.parametrize(
    ("reply", "line"),
    [
        ((200, {"released": True}), "rig lease hold: released"),
        ((200, {"released": False}), "::warning::rig lease hold: nothing to release"),
        (None, "::warning::rig lease hold: the release failed"),
        ((401, {"error": "unauthorized"}), "::warning::rig lease hold: the release"),
    ],
)
def test_the_release_never_fails_the_job(reply, line):
    logs: list[str] = []
    post = scripted(reply)
    assert release(post, LEASE, logs.append) == EXIT_OK
    assert post.calls == [("/rig-lease/release", {"lease_id": LEASE})]
    assert logs[-1].startswith(line)


# ---- end to end over HTTP ---------------------------------------------------------


class FakeLeaseServer:
    """camera-box's three write routes over one in-memory lease."""

    def __init__(self):
        self.holder: dict | None = None
        self.requests: list[tuple[str, str | None, dict]] = []
        server_self = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                body = json.loads(raw)
                auth = self.headers.get("Authorization")
                server_self.requests.append((self.path, auth, body))
                status, payload = server_self.answer(self.path, auth, body)
                data = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def answer(self, path, auth, body):
        if auth != f"Bearer {TOKEN}":
            return 401, {"error": "unauthorized"}
        if path == "/rig-lease/acquire":
            if self.holder is not None:
                return 409, {"acquired": False, "holder": self.holder}
            self.holder = {"repo": body["repo"], "lease_id": LEASE}
            return 200, {"acquired": True, "lease_id": LEASE}
        mine = self.holder is not None and body.get("lease_id") == LEASE
        if path == "/rig-lease/heartbeat":
            return (
                (200, {"refreshed": True}) if mine else (404, {"error": "not_holder"})
            )
        if path == "/rig-lease/release":
            self.holder = None if mine else self.holder
            return 200, {"released": mine}
        return 404, {"error": "not_found"}


@pytest.fixture
def lease_server():
    server = FakeLeaseServer()
    yield server
    server.httpd.shutdown()


def acquire_args(url, state_file, *extra):
    return [
        "acquire",
        "--url",
        url,
        "--repo",
        BODY["repo"],
        "--run-id",
        BODY["run_id"],
        "--run-url",
        BODY["run_url"],
        "--job",
        BODY["job"],
        "--resources",
        "resolume-snv",
        "--expected-secs",
        "3600",
        "--state-file",
        str(state_file),
        *extra,
    ]


def test_acquire_heartbeat_release_end_to_end(lease_server, tmp_path, monkeypatch):
    monkeypatch.setenv("RIG_LEASE_TOKEN", TOKEN)
    state_file = tmp_path / "lease.json"
    dead = "http://127.0.0.1:9"  # nothing listens: the next URL is tried
    assert main(acquire_args(dead, state_file, "--url", lease_server.url)) == EXIT_OK
    assert read_state(state_file) == {"lease_id": LEASE, "url": lease_server.url}
    path, auth, body = lease_server.requests[0]
    assert (path, auth) == ("/rig-lease/acquire", f"Bearer {TOKEN}")
    assert body == BODY

    hb = ["heartbeat", "--state-file", str(state_file), "--max-seconds", "0.05"]
    assert main([*hb, "--interval", "0.01"]) == EXIT_OK
    assert ("/rig-lease/heartbeat", f"Bearer {TOKEN}", {"lease_id": LEASE}) in (
        lease_server.requests
    )

    assert main(["release", "--state-file", str(state_file)]) == EXIT_OK
    assert lease_server.holder is None
    assert not state_file.exists()


def test_a_lease_taken_from_us_ends_the_heartbeat_as_lost(
    lease_server, tmp_path, monkeypatch
):
    monkeypatch.setenv("RIG_LEASE_TOKEN", TOKEN)
    state_file = tmp_path / "lease.json"
    assert main(acquire_args(lease_server.url, state_file)) == EXIT_OK
    lease_server.holder = None  # reclaimed as stale by someone else
    log_file = tmp_path / "heartbeat.log"
    hb = ["heartbeat", "--state-file", str(state_file), "--interval", "0.01"]
    assert main([*hb, "--log-file", str(log_file)]) == EXIT_LOST
    # detached, it has no console: the loss is in its own log for the release
    assert "::error::rig lease hold: the lease is lost" in log_file.read_text()


def test_a_wrong_token_fails_the_acquire(lease_server, tmp_path, monkeypatch):
    monkeypatch.setenv("RIG_LEASE_TOKEN", "wrong")
    state_file = tmp_path / "lease.json"
    assert main(acquire_args(lease_server.url, state_file)) == EXIT_CONFIG
    assert not state_file.exists()


def test_no_token_runs_unheld_and_never_calls_the_server(
    lease_server, tmp_path, monkeypatch, capsys
):
    monkeypatch.delenv("RIG_LEASE_TOKEN", raising=False)
    state_file = tmp_path / "lease.json"
    state_file.write_text('{"lease_id": "%s", "url": "http://x"}' % LEASE)  # stale
    assert main(acquire_args(lease_server.url, state_file)) == EXIT_OK
    assert lease_server.requests == []
    assert not state_file.exists()
    assert "no RIG_LEASE_TOKEN" in capsys.readouterr().out
    assert main(["release", "--state-file", str(state_file)]) == EXIT_OK
    assert main(["heartbeat", "--state-file", str(state_file)]) == EXIT_OK


def test_a_held_lease_past_the_bound_fails_end_to_end(
    lease_server, tmp_path, monkeypatch
):
    monkeypatch.setenv("RIG_LEASE_TOKEN", TOKEN)
    lease_server.holder = {"repo": "zbynekdrlik/camera-box", "job": "full-path"}
    state_file = tmp_path / "lease.json"
    args = acquire_args(lease_server.url, state_file, "--max-wait-seconds", "0")
    assert main(args) == EXIT_STILL_HELD
    assert not state_file.exists()


@pytest.mark.parametrize("secs", ["59", "5401"])
def test_expected_secs_outside_camera_boxs_bounds_is_refused(tmp_path, secs):
    args = acquire_args("http://dev1:8890", tmp_path / "s.json")
    args[args.index("--expected-secs") + 1] = secs
    with pytest.raises(SystemExit):
        main(args)


def test_a_corrupt_state_file_is_refused(tmp_path):
    state_file = tmp_path / "lease.json"
    state_file.write_text('{"lease_id": "x", "url": "http://dev1:8890"}')
    with pytest.raises(ValueError):
        main(["release", "--state-file", str(state_file)])


# ---- the wiring in ci.yml ------------------------------------------------------------


def ci_jobs() -> dict[str, list[str]]:
    """Each ci.yml job's steps, as text blocks (the Eval job has no YAML lib)."""
    ci = (
        Path(__file__).resolve().parents[2] / ".github" / "workflows" / "ci.yml"
    ).read_text(encoding="utf-8")
    body = ci.split("\njobs:\n", 1)[1]
    jobs: dict[str, list[str]] = {}
    for chunk in re.split(r"\n(?=  [A-Za-z0-9_-]+:\n)", "\n" + body):
        head, _, rest = chunk.strip("\n").partition("\n")
        if not head.startswith("  ") or head.startswith("   "):
            continue
        jobs[head.strip().rstrip(":")] = re.split(r"\n(?=      - )", rest)[1:]
    return jobs


def test_every_job_that_takes_the_lease_releases_it_always_last():
    holders = {
        job: steps
        for job, steps in ci_jobs().items()
        if any("rig_lease_hold.py acquire" in s for s in steps)
    }
    assert set(holders) == {"deploy-resolume", "e2e-resolume"}, set(holders)
    for job, steps in holders.items():
        acquire_at = next(i for i, s in enumerate(steps) if "hold.py acquire" in s)
        gate_at = next(
            i for i, s in enumerate(steps) if "rig_lease_gate.py --own-repo" in s
        )
        # the gate's wait comes first, the hold right after it
        assert acquire_at == gate_at + 1, job
        assert "secrets.RIG_LEASE_TOKEN" in steps[acquire_at], job
        assert "Start-Process" in steps[acquire_at], job
        assert "-Redirect" not in steps[acquire_at], job  # would hang the step
        last = steps[-1]
        if "upload-artifact" in last:  # failure-only report after it is fine
            last = steps[-2]
        assert "rig_lease_hold.py release" in last, job
        assert "if: always()" in last, job
        assert "secrets.RIG_LEASE_TOKEN" in last, job
