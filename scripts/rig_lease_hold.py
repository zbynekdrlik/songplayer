#!/usr/bin/env python3
"""Rig-lease HOLD for SongPlayer's resolume-snv jobs (#314).

`rig_lease_gate.py` only WAITS while another repo holds the dev1 rig lease; it
never takes it, so a camera-box run could start in the middle of our deploy or
E2E. This script makes a SongPlayer job a real holder through camera-box's
write routes (camera-box issue 1432, `scripts/rig_lease_write.py`):

  POST /rig-lease/acquire   {repo, run_id, run_url, job, expected_secs, resources}
       200 {acquired: true, lease_id, ...}   409 {acquired: false, holder, ...}
  POST /rig-lease/heartbeat {lease_id}   200 / 404 not_holder / 409 over_hold
  POST /rig-lease/release   {lease_id}   200 {released: bool}, idempotent
  401 without the bearer token, 400 on a bad field, 503 lease_io.

Three subcommands, one per workflow step:

- `acquire` (right after the gate's wait, before anything stops SongPlayer):
  takes `resources` (resolume-snv) for `--expected-secs`, waiting while
  another holder answers 409 (every `--poll-seconds`, at most
  `--max-wait-seconds`, then exit 3 like the gate). Taken -> the lease id and
  the URL that took it go into `--state-file`. No token in `RIG_LEASE_TOKEN`,
  a server without the write routes (404) or a lease service that does not
  answer (the gate's own outage rule: `OUTAGE_READS` reads in a row once a
  holder was seen, else at once) -> a WARN and exit 0 with no state file: the
  job runs as before, waited for but not holding. A refused token (401) or a
  refused field (400) is our own misconfiguration -> exit 2, loudly.
- `heartbeat` (started detached by the acquire step, `--log-file`): beats every
  `--interval` (30 s; a silent remote holder is reclaimable after 180 s)
  while the state file exists, at most `--max-seconds`. The server refusing
  the beat (404 / 409) while the state file still exists = the lease is lost:
  an `::error::` line and exit 4. An unreachable beat is a WARN, the next one
  is tried.
- `release` (an `always()` last step of the job): releases the state file's
  lease and deletes the file. No file -> nothing to release. A release that
  does not answer is a WARN (the lease goes stale 180 s after the last beat).

Stdlib only: it runs on resolume-snv's `C:\\Program Files\\Python312`.
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import Any

from rig_lease_gate import OUTAGE_READS, is_http_url, one_line

ACQUIRE = "/rig-lease/acquire"
HEARTBEAT = "/rig-lease/heartbeat"
RELEASE = "/rig-lease/release"

TOKEN_ENV = "RIG_LEASE_TOKEN"
DEFAULT_POLL_SECONDS = 30.0
DEFAULT_MAX_WAIT_SECONDS = 3600.0
DEFAULT_INTERVAL_SECONDS = 30.0
# camera-box's bounds of `expected_secs` (rig_lease_write.MIN/MAX_EXPECTED_S).
MIN_EXPECTED_SECS = 60
MAX_EXPECTED_SECS = 5400
POST_TIMEOUT_SECONDS = 10.0
MAX_BODY_BYTES = 65_536
LEASE_ID = re.compile(r"[0-9a-f]{32}")

EXIT_OK = 0
EXIT_CONFIG = 2
EXIT_STILL_HELD = 3
EXIT_LOST = 4

Reply = tuple[int, dict[str, Any]]
Post = Callable[[str, dict[str, Any]], "tuple[str, Reply] | None"]
Log = Callable[[str], None]


def post_json(
    url: str,
    body: dict[str, Any],
    token: str,
    timeout: float = POST_TIMEOUT_SECONDS,
) -> Reply | None:
    """POST `body` to `url` with the bearer token; (status, JSON object) or
    None when nothing answered with a JSON object."""
    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode("utf-8"),
        method="POST",
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {token}",
        },
    )
    try:
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                status, raw = resp.status, resp.read(MAX_BODY_BYTES + 1)
        except urllib.error.HTTPError as e:
            status, raw = e.code, e.read(MAX_BODY_BYTES + 1)
        if len(raw) > MAX_BODY_BYTES:
            return None
        doc = json.loads(raw)
    except (OSError, ValueError, RecursionError, http.client.HTTPException):
        return None
    if not isinstance(doc, dict):
        return None
    return status, doc


def make_post(bases: Sequence[str], token: str, log: Log) -> Post:
    """A poster over `bases`: the first base that answers wins; its reply and
    the base are returned."""

    def post(path: str, body: dict[str, Any]) -> tuple[str, Reply] | None:
        for base in bases:
            reply = post_json(base.rstrip("/") + path, body, token)
            if reply is not None:
                return base, reply
            log(f"rig lease hold: {base}{path} did not answer with JSON")
        return None

    return post


def describe(payload: dict[str, Any]) -> str:
    holder = payload.get("holder") or {}
    if not isinstance(holder, dict):
        holder = {}
    repo = holder.get("repo") or "an unknown holder"
    job = holder.get("job") or "?"
    run = holder.get("run_url") or holder.get("run_id") or "?"
    eta = holder.get("expected_release_at") or "unknown"
    return one_line(f"{repo} (job {job}, run {run}), expected release {eta}")


def acquire(
    post: Post,
    body: dict[str, Any],
    poll_seconds: float,
    max_wait_seconds: float,
    clock: Callable[[], float],
    sleep: Callable[[float], None],
    log: Log,
) -> tuple[int, dict[str, str] | None]:
    """Take the lease (the module doc); (exit code, state to keep or None)."""
    deadline = clock() + max_wait_seconds
    held: dict[str, Any] | None = None  # the last 409 payload
    unreachable = 0
    while True:
        answer = post(ACQUIRE, body)
        status = answer[1][0] if answer else None
        if answer is None or status == 503:
            unreachable += 1
            if held is None or unreachable >= OUTAGE_READS:
                log(
                    "::warning::rig lease hold: the lease service does not answer -"
                    " this job runs without holding the lease"
                )
                return EXIT_OK, None
            log(
                f"rig lease hold: no answer ({unreachable} of {OUTAGE_READS} in a row)"
                " right after it was held - still treating it as held"
            )
        else:
            unreachable = 0
            base, (status, payload) = answer
            lease_id = payload.get("lease_id")
            if status == 200 and payload.get("acquired") is True:
                if not isinstance(lease_id, str) or not LEASE_ID.fullmatch(lease_id):
                    log(
                        "::error::rig lease hold: the acquire answered no valid"
                        " lease_id - not holding"
                    )
                    return EXIT_CONFIG, None
                log(
                    f"rig lease hold: taken at {base} for {body['resources']}"
                    f" (expected {body['expected_secs']} s)"
                )
                return EXIT_OK, {"lease_id": lease_id, "url": base}
            if status == 404:
                log(
                    f"::warning::rig lease hold: {base} has no write routes (404) -"
                    " this job runs without holding the lease"
                )
                return EXIT_OK, None
            if status != 409:
                error = one_line(payload.get("error", ""))
                field = one_line(payload.get("field", ""))
                log(
                    f"::error::rig lease hold: the acquire was refused ({status}"
                    f" {error} {field}) - our token or request is wrong"
                )
                return EXIT_CONFIG, None
            held = payload
        remaining = deadline - clock()
        if remaining <= 0:
            log(
                f"::error::rig lease hold: still held after {max_wait_seconds / 60:.0f}"
                f" min by {describe(held or {})} - not touching resolume-snv"
            )
            return EXIT_STILL_HELD, None
        pause = min(poll_seconds, remaining)
        log(
            f"rig lease hold: held by {describe(held or {})} - waiting; next try in"
            f" {pause:.0f} s, giving up in {remaining / 60:.1f} min"
        )
        sleep(pause)


def heartbeat(
    post: Post,
    lease_id: str,
    still_held: Callable[[], bool],
    interval: float,
    max_seconds: float,
    clock: Callable[[], float],
    sleep: Callable[[float], None],
    log: Log,
) -> int:
    """Beat while `still_held()` (the state file exists), at most
    `max_seconds`; the exit code."""
    end = clock() + max_seconds
    while still_held() and clock() < end:
        answer = post(HEARTBEAT, {"lease_id": lease_id})
        if answer is None or answer[1][0] == 503:
            log("::warning::rig lease hold: a heartbeat got no answer - trying again")
        elif answer[1][0] != 200:
            if not still_held():
                return EXIT_OK  # released between the check and the beat
            status, payload = answer[1]
            log(
                f"::error::rig lease hold: the lease is lost ({status}"
                f" {one_line(payload.get('error', ''))}) - another run may take"
                " resolume-snv now"
            )
            return EXIT_LOST
        sleep(interval)
    return EXIT_OK


def release(post: Post, lease_id: str, log: Log) -> int:
    answer = post(RELEASE, {"lease_id": lease_id})
    if answer is None or answer[1][0] != 200:
        status = answer[1][0] if answer else "no answer"
        log(
            f"::warning::rig lease hold: the release failed ({status}) - the lease"
            " goes stale 180 s after its last heartbeat"
        )
        return EXIT_OK
    if answer[1][1].get("released") is True:
        log("rig lease hold: released")
    else:
        log(
            "::warning::rig lease hold: nothing to release - the lease was no longer"
            " ours (lost or reclaimed during the job)"
        )
    return EXIT_OK


def read_state(path: Path) -> dict[str, str] | None:
    try:
        state = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return None
    if (
        not isinstance(state, dict)
        or not isinstance(state.get("lease_id"), str)
        or not LEASE_ID.fullmatch(state["lease_id"])
        or not isinstance(state.get("url"), str)
        or not is_http_url(state["url"])
    ):
        raise ValueError(f"{path} is not a rig lease state file")
    return state


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    sub = parser.add_subparsers(dest="command", required=True)
    acq = sub.add_parser("acquire")
    acq.add_argument("--url", action="append", required=True, help="a lease base URL")
    acq.add_argument("--repo", required=True)
    acq.add_argument("--run-id", required=True)
    acq.add_argument("--run-url", required=True)
    acq.add_argument("--job", required=True)
    acq.add_argument("--resources", required=True)
    acq.add_argument("--expected-secs", type=int, required=True)
    acq.add_argument("--poll-seconds", type=float, default=DEFAULT_POLL_SECONDS)
    acq.add_argument("--max-wait-seconds", type=float, default=DEFAULT_MAX_WAIT_SECONDS)
    for p in (acq, sub.add_parser("heartbeat"), sub.add_parser("release")):
        p.add_argument("--state-file", required=True, type=Path)
    hb = sub.choices["heartbeat"]
    hb.add_argument("--interval", type=float, default=DEFAULT_INTERVAL_SECONDS)
    hb.add_argument("--max-seconds", type=float, default=float(MAX_EXPECTED_SECS))
    # The acquire step starts the heartbeat detached, with no redirection (a
    # redirected child inherits the step's output pipe and the runner waits for
    # it), so the heartbeat writes its own log; the release step prints it.
    hb.add_argument("--log-file", type=Path)
    args = parser.parse_args(argv)

    log_file = getattr(args, "log_file", None)

    def log(message: str) -> None:
        if log_file is None:
            print(message, flush=True)
            return
        stamp = time.strftime("%H:%M:%SZ", time.gmtime())
        with log_file.open("a", encoding="utf-8", errors="backslashreplace") as f:
            f.write(f"{stamp} {message}\n")

    token = os.environ.get(TOKEN_ENV, "").strip()

    if args.command == "acquire":
        if not MIN_EXPECTED_SECS <= args.expected_secs <= MAX_EXPECTED_SECS:
            parser.error(
                f"--expected-secs must be {MIN_EXPECTED_SECS}..{MAX_EXPECTED_SECS}"
            )
        if args.poll_seconds <= 0 or args.max_wait_seconds < 0:
            parser.error("--poll-seconds must be > 0 and --max-wait-seconds >= 0")
        for url in args.url:
            if not is_http_url(url):
                parser.error(f"--url {url!r} is not an http(s) URL with a host")
        args.state_file.unlink(missing_ok=True)
        if not token:
            log(
                f"::warning::rig lease hold: no {TOKEN_ENV} - this job runs without"
                " holding the lease (the gate's wait still ran)"
            )
            return EXIT_OK
        body = {
            "repo": args.repo,
            "run_id": args.run_id,
            "run_url": args.run_url,
            "job": args.job,
            "expected_secs": args.expected_secs,
            "resources": args.resources,
        }
        code, state = acquire(
            make_post(args.url, token, log),
            body,
            args.poll_seconds,
            args.max_wait_seconds,
            time.monotonic,
            time.sleep,
            log,
        )
        if state is not None:
            args.state_file.write_text(json.dumps(state), encoding="utf-8")
        return code

    state = read_state(args.state_file)
    if state is None:
        if args.command == "release":
            log("rig lease hold: this job holds no lease - nothing to release")
        return EXIT_OK
    post = make_post([state["url"]], token, log)
    if args.command == "heartbeat":
        if args.interval <= 0 or args.max_seconds <= 0:
            parser.error("--interval and --max-seconds must be > 0")
        return heartbeat(
            post,
            state["lease_id"],
            args.state_file.exists,
            args.interval,
            args.max_seconds,
            time.monotonic,
            time.sleep,
            log,
        )
    code = release(post, state["lease_id"], log)
    args.state_file.unlink(missing_ok=True)
    return code


if __name__ == "__main__":
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="backslashreplace")
    sys.exit(main())
