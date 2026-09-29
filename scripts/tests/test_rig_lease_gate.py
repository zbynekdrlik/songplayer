"""#221 L4a: the rig-lease gate of the win-resolume deploy (scripts/rig_lease_gate.py).

The decision, the bounded wait (on a fake clock), and the fetch over a real
local HTTP server (first answering URL wins, a dead URL or a non-lease answer
is skipped, none answering = unreachable).
"""

from __future__ import annotations

import json
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
import rig_lease_gate as gate_mod
from rig_lease_gate import (
    EXIT_PROCEED,
    EXIT_STILL_HELD,
    FREE,
    OWN,
    STALE,
    WAIT,
    decide,
    fetch_lease,
    gate,
    is_lease,
    main,
)

OWN_REPO = "zbynekdrlik/songplayer"


def lease(held=True, stale=False, repo="zbynekdrlik/camera-box"):
    if not held:
        return {"schema": 1, "held": False, "holder": None, "stale": None}
    return {
        "schema": 1,
        "held": True,
        "stale": stale,
        "holder": {
            "repo": repo,
            "run_id": "36518120678",
            "run_url": "https://github.com/zbynekdrlik/camera-box/actions/runs/36518120678",
            "job": "full-path",
            "acquired_at": "2026-09-29T03:39:56Z",
            "expected_release_at": "2026-09-29T04:24:56Z",
        },
        "expected_release_at": "2026-09-29T04:24:56Z",
        "ttl_s": 2132,
    }


# ---- decide ------------------------------------------------------------------


def test_decide():
    assert decide(lease(held=False), OWN_REPO) == FREE
    assert decide(lease(stale=True), OWN_REPO) == STALE
    assert decide(lease(repo=OWN_REPO), OWN_REPO) == OWN
    assert decide(lease(), OWN_REPO) == WAIT
    # A held lease with no readable holder is another repo's (fail closed).
    no_holder = {"held": True, "stale": False, "holder": None}
    assert decide(no_holder, OWN_REPO) == WAIT
    # camera-box's own fail-closed document (`rig_lease_state.py`: the lock
    # directory exists, its holder.json could not be read) is held too.
    fail_closed = {"held": True, "stale": None, "holder": None}
    assert decide(fail_closed, OWN_REPO) == WAIT
    # Only `stale: true` releases a held lease; a missing one does not.
    assert decide({"held": True, "holder": {"repo": "x/y"}}, OWN_REPO) == WAIT


def test_only_a_json_object_with_a_boolean_held_is_a_lease():
    assert is_lease({"held": False})
    assert is_lease(lease())
    assert not is_lease({"held": "true"})
    assert not is_lease({"schema": 1})
    assert not is_lease([{"held": True}])
    assert not is_lease(None)
    # `holder` is an object or null: anything else is not a lease document
    # (never an AttributeError that fails the deploy).
    assert is_lease({"held": True, "holder": None})
    assert not is_lease({"held": True, "holder": "zbynekdrlik/camera-box"})
    assert not is_lease({"held": True, "holder": ["x"]})


# ---- the wait (fake clock) ---------------------------------------------------


class Rig:
    """Scripted lease answers (the last one repeats) on a fake clock."""

    def __init__(self, answers):
        self.answers = list(answers)
        self.fetches = 0
        self.now = 1000.0
        self.sleeps: list[float] = []
        self.lines: list[str] = []

    def fetch(self):
        self.fetches += 1
        if len(self.answers) > 1:
            return self.answers.pop(0)
        return self.answers[0]

    def clock(self):
        return self.now

    def sleep(self, seconds):
        assert seconds > 0, "a pause is always positive"
        self.sleeps.append(seconds)
        self.now += seconds

    def run(self, poll=30.0, max_wait=3600.0):
        return gate(
            self.fetch,
            OWN_REPO,
            poll,
            max_wait,
            self.clock,
            self.sleep,
            self.lines.append,
        )


def test_a_free_lease_deploys_at_once():
    rig = Rig([lease(held=False)])
    assert rig.run() == EXIT_PROCEED
    assert (rig.fetches, rig.sleeps) == (1, [])


@pytest.mark.parametrize("answer", [lease(stale=True), lease(repo=OWN_REPO)])
def test_a_stale_or_own_lease_deploys_at_once(answer):
    rig = Rig([answer])
    assert rig.run() == EXIT_PROCEED
    assert (rig.fetches, rig.sleeps) == (1, [])


def test_an_unreachable_lease_service_warns_and_deploys():
    rig = Rig([None])
    assert rig.run() == EXIT_PROCEED
    assert rig.sleeps == []
    assert rig.lines[-1].startswith("::warning::")


def test_another_repos_lease_is_waited_out_every_30_s():
    rig = Rig([lease(), lease(), lease(), lease(held=False)])
    assert rig.run() == EXIT_PROCEED
    assert rig.fetches == 4
    assert rig.sleeps == [30.0, 30.0, 30.0]
    waits = [line for line in rig.lines if "waiting" in line]
    assert len(waits) == 3
    # The holder and its expected release are logged.
    assert "zbynekdrlik/camera-box" in waits[0]
    assert "2026-09-29T04:24:56Z" in waits[0]
    assert "full-path" in waits[0]


def test_a_lease_still_held_after_the_bound_fails_the_deploy():
    rig = Rig([lease()])
    assert rig.run(poll=30.0, max_wait=3600.0) == EXIT_STILL_HELD
    # 120 pauses of 30 s = the 60 min bound, then one last look.
    assert rig.sleeps == [30.0] * 120
    assert rig.fetches == 121
    assert rig.lines[-1].startswith("::error::")
    assert "zbynekdrlik/camera-box" in rig.lines[-1]


def test_the_last_pause_ends_exactly_at_the_bound():
    rig = Rig([lease()])
    assert rig.run(poll=30.0, max_wait=45.0) == EXIT_STILL_HELD
    assert rig.sleeps == [30.0, 15.0]
    assert rig.fetches == 3


def test_a_zero_bound_never_waits():
    rig = Rig([lease()])
    assert rig.run(max_wait=0.0) == EXIT_STILL_HELD
    assert (rig.fetches, rig.sleeps) == (1, [])


def test_a_blip_after_a_held_lease_keeps_waiting():
    # Review round 1: one or two unreachable reads right after another
    # repo's live lease are a blip (a cold mDNS lookup, a server restart),
    # not an outage: the gate keeps treating the lease as held.
    rig = Rig([lease(), None, None, lease(), lease(held=False)])
    assert rig.run() == EXIT_PROCEED
    assert rig.fetches == 5
    assert rig.sleeps == [30.0] * 4
    assert not any(line.startswith("::warning::") for line in rig.lines)
    assert rig.lines[-1] == "rig lease: free - deploying"


def test_the_third_unreachable_read_after_a_held_lease_is_an_outage():
    # The service really went away: the third read in a row deploys with a
    # WARN, so an outage never blocks a deploy (at most two more polls).
    rig = Rig([lease(), None])
    assert rig.run() == EXIT_PROCEED
    assert rig.fetches == 4
    assert rig.sleeps == [30.0] * 3
    assert rig.lines[-1].startswith("::warning::")
    assert sum(line.startswith("::warning::") for line in rig.lines) == 1


def test_the_bound_during_a_blip_fails_as_still_held():
    # The last lease read was held: at the bound the deploy fails as held.
    rig = Rig([lease(), None])
    assert rig.run(poll=30.0, max_wait=45.0) == EXIT_STILL_HELD
    assert rig.sleeps == [30.0, 15.0]
    assert rig.fetches == 3
    assert rig.lines[-1].startswith("::error::")
    assert "zbynekdrlik/camera-box" in rig.lines[-1]


# ---- the fetch (a real local HTTP server) -------------------------------------


class _Handler(BaseHTTPRequestHandler):
    body = b"{}"

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(self.body)))
        self.end_headers()
        self.wfile.write(self.body)

    def log_message(self, fmt, *args):
        pass  # the test output stays clean; nothing here is a finding


@pytest.fixture
def serve():
    servers = []

    def start(body: bytes) -> str:
        handler = type("H", (_Handler,), {"body": body})
        server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        servers.append(server)
        return f"http://127.0.0.1:{server.server_address[1]}/rig-lease.json"

    yield start
    for server in servers:
        server.shutdown()
        server.server_close()


def dead_url() -> str:
    """A URL on a port nothing listens on (connection refused at once)."""
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return f"http://127.0.0.1:{port}/rig-lease.json"


def test_the_first_url_that_answers_with_a_lease_wins(serve):
    free = serve(json.dumps(lease(held=False)).encode())
    held = serve(json.dumps(lease()).encode())
    lines: list[str] = []
    assert fetch_lease([dead_url(), free, held], lines.append) == lease(held=False)
    assert len(lines) == 1, "the dead URL is logged"


def test_a_non_lease_answer_is_skipped(serve):
    garbage = serve(b"<html>not a lease</html>")
    not_lease = serve(b'{"schema": 1}')
    held = serve(json.dumps(lease()).encode())
    lines: list[str] = []
    assert fetch_lease([garbage, not_lease, held], lines.append) == lease()
    assert len(lines) == 2


def test_no_url_answering_is_unreachable(serve):
    lines: list[str] = []
    assert fetch_lease([dead_url(), serve(b"[]")], lines.append) is None
    assert len(lines) == 2


@pytest.fixture
def raw_serve():
    """A TCP server that answers every connection with fixed raw bytes."""
    socks = []

    def start(payload: bytes) -> str:
        sock = socket.socket()
        sock.bind(("127.0.0.1", 0))
        sock.listen(5)
        socks.append(sock)

        def run():
            while True:
                try:
                    conn, _ = sock.accept()
                except OSError:
                    return  # the fixture closed the socket
                with conn:
                    conn.recv(4096)
                    conn.sendall(payload)

        threading.Thread(target=run, daemon=True).start()
        return f"http://127.0.0.1:{sock.getsockname()[1]}/rig-lease.json"

    yield start
    for sock in socks:
        sock.close()


def test_something_else_on_the_port_is_no_lease(serve, raw_serve):
    # Review round 2: a non-HTTP listener (`BadStatusLine`) or a truncated
    # body (`IncompleteRead`) are `http.client.HTTPException`s, not OSErrors;
    # they must be "no lease" (the next URL is tried; none -> WARN, deploy),
    # never an uncaught exception that fails the deploy.
    banner = raw_serve(b"SSH-2.0-OpenSSH_9.6\r\n")
    truncated = raw_serve(b'HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{"held": fa')
    held = serve(json.dumps(lease()).encode())
    lines: list[str] = []
    assert fetch_lease([banner, truncated, held], lines.append) == lease()
    assert len(lines) == 2
    assert fetch_lease([banner, truncated], lines.append) is None


def test_main_deploys_on_a_free_lease(serve, capsys):
    url = serve(json.dumps(lease(held=False)).encode())
    assert main(["--own-repo", OWN_REPO, "--url", url]) == EXIT_PROCEED
    assert "free" in capsys.readouterr().out


def test_main_fails_on_a_lease_held_past_the_bound(serve, capsys):
    url = serve(json.dumps(lease()).encode())
    code = main(["--own-repo", OWN_REPO, "--url", url, "--max-wait-seconds", "0"])
    assert code == EXIT_STILL_HELD
    assert "::error::" in capsys.readouterr().out


def test_main_rejects_a_non_positive_poll():
    with pytest.raises(SystemExit):
        main(["--own-repo", OWN_REPO, "--url", "http://x", "--poll-seconds", "0"])


def test_the_defaults_are_the_decided_30_s_and_60_min():
    assert gate_mod.DEFAULT_POLL_SECONDS == 30.0
    assert gate_mod.DEFAULT_MAX_WAIT_SECONDS == 3600.0


# ---- review round 3 ------------------------------------------------------------


def test_a_body_too_deep_or_too_big_is_no_lease(serve):
    # A JSON body nested past Python's recursion limit raises RecursionError
    # (not a ValueError); a body over the 64 KiB bound is never parsed (a
    # lease document is ~400 B). Both are "no lease" from that URL, never a
    # crash that fails the deploy.
    deep = serve(b"[" * 30_000 + b"]" * 30_000)
    padding = b" " * 70_000
    too_big = serve(b'{"held": false,' + padding + b'"x": 1}')
    held = serve(json.dumps(lease()).encode())
    lines: list[str] = []
    assert fetch_lease([deep, too_big, held], lines.append) == lease()
    assert len(lines) == 2
    assert "too large" in lines[1]


def test_main_refuses_a_url_that_is_not_http():
    # A broken --url in ci.yml would otherwise look like an outage forever
    # (every deploy warns and goes on, never checking the lease).
    for url in ["dev1:8890/rig-lease.json", "ftp://dev1/rig-lease.json", "http:///x"]:
        with pytest.raises(SystemExit):
            main(["--own-repo", OWN_REPO, "--url", url])


def test_a_holder_field_never_starts_a_new_log_line():
    # A newline in a lease string would start a new output line, which the
    # runner reads as a workflow command (`::error::`, `::add-mask::`).
    sneaky = lease(repo="x/y\n::error::injected")
    sneaky["holder"]["job"] = "full-path\r::add-mask::secret"
    text = gate_mod.describe_holder(sneaky)
    assert "\n" not in text and "\r" not in text
    assert "::error::" in text, "the text stays readable, only the break goes"


def test_the_body_bound_is_64_kib():
    assert gate_mod.MAX_BODY_BYTES == 65_536


# ---- review round 4 ------------------------------------------------------------


def test_nothing_the_lease_port_answers_breaks_a_log_line(serve, raw_serve):
    # A top-level JSON string is "not a lease" and was logged raw; a CRLF
    # banner reaches the log through the exception text. Neither may start
    # a new runner output line (a workflow command).
    string = serve(json.dumps("x\n::error::injected").encode())
    banner = raw_serve(b"SSH-2.0-OpenSSH_9.6\r\n::error::injected\r\n")
    lines: list[str] = []
    assert fetch_lease([string, banner], lines.append) is None
    assert len(lines) == 2
    for line in lines:
        assert "\n" not in line and "\r" not in line, repr(line)


def test_main_refuses_a_url_with_a_broken_port_or_host():
    for url in [
        "http://dev1:abc/rig-lease.json",
        "http://dev1:88900/rig-lease.json",
        "http://[::1/rig-lease.json",
    ]:
        with pytest.raises(SystemExit):
            main(["--own-repo", OWN_REPO, "--url", url])
