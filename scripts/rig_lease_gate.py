#!/usr/bin/env python3
"""Rig-lease gate for SongPlayer's win-resolume deploy (#221 L4a).

The CI job "Deploy to win-resolume" STOPS SongPlayer before it installs the
new build, and SongPlayer drives the shared rig (SP-program, the facade
Companion talks to, cg OBS through the mirror). Other repos (camera-box's
full-path E2E, the A/V soak) hold a cross-repo "rig lease" on dev1 while they
drive the same rig. A deploy must never land inside another repo's lease;
cancelling a run that already started its deploy is no answer either (it left
SongPlayer stopped, 29.9.2026 03:15Z, `.claude/rules/ci-workflows.md`). So
this gate runs as the deploy job's first step after checkout, before anything
stops SongPlayer:

- It reads the lease (`GET /rig-lease.json`, camera-box
  `scripts/rig-lease-server.py`: `held`, `stale`, `holder.repo`,
  `expected_release_at`), trying each `--url` in order.
- Held, not stale, by another repo -> it waits, polling every
  `--poll-seconds` (30) and logging the holder and its expected release, at
  most `--max-wait-seconds` (3600). Still held then -> exit 3: the job fails
  loudly, it never deploys over the lease.
- Free, stale (an abandoned holder, the lease's own self-heal rule) or held
  by this repo -> exit 0.
- No URL answers with a lease (dev1 down, the service down, something else
  on the port) -> a WARN, exit 0: an outage of the lease service never
  blocks a deploy. Right after another repo's live lease was read, one or
  two unreachable reads are a blip and the lease still counts as held; the
  `OUTAGE_READS`-th (3rd) in a row is the outage (review round 1).

Stdlib only: it runs on win-resolume's `C:\\Program Files\\Python312`.
Usage:
  python rig_lease_gate.py --own-repo OWNER/NAME --url URL [--url URL ...]
      [--poll-seconds 30] [--max-wait-seconds 3600]
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.request
from collections.abc import Callable, Sequence
from typing import Any

DEFAULT_POLL_SECONDS = 30.0
DEFAULT_MAX_WAIT_SECONDS = 3600.0
# One GET may take this long; a cold mDNS lookup of `dev1` on the box took
# ~3 s (29.9.2026).
FETCH_TIMEOUT_SECONDS = 10.0

# `decide` verdicts.
FREE = "free"
STALE = "stale"
OWN = "own"
WAIT = "wait"

# Unreachable reads in a row, right after another repo's live lease, that
# count as an outage (then the deploy goes on with a WARN). Fewer are a blip
# (a cold mDNS lookup of dev1, a restart of the lease server): the lease is
# still treated as held. An unreachable FIRST read is an outage at once.
OUTAGE_READS = 3

# Exit codes.
EXIT_PROCEED = 0
EXIT_STILL_HELD = 3

Lease = dict[str, Any]
Log = Callable[[str], None]


def is_lease(doc: object) -> bool:
    """A lease document: a JSON object whose `held` is a boolean and whose
    `holder` (when present) is an object or null."""
    return (
        isinstance(doc, dict)
        and isinstance(doc.get("held"), bool)
        and isinstance(doc.get("holder"), (dict, type(None)))
    )


def decide(lease: Lease, own_repo: str) -> str:
    """What the deploy does about this lease.

    FREE: nobody holds it. STALE: its holder stopped beating (abandoned,
    reclaimable). OWN: this repo holds it. WAIT: another repo holds it live
    (a held lease with no readable holder counts as another repo's).
    """
    if not lease["held"]:
        return FREE
    if lease.get("stale") is True:
        return STALE
    holder = lease.get("holder") or {}
    if holder.get("repo") == own_repo:
        return OWN
    return WAIT


def describe_holder(lease: Lease) -> str:
    """The holder and its expected release, for the log."""
    holder = lease.get("holder") or {}
    repo = holder.get("repo") or "an unknown holder"
    job = holder.get("job") or "?"
    run = holder.get("run_url") or holder.get("run_id") or "?"
    eta = lease.get("expected_release_at") or "unknown"
    ttl = lease.get("ttl_s")
    ttl_text = f"{ttl} s" if isinstance(ttl, int) else "unknown"
    return f"{repo} (job {job}, run {run}), expected release {eta} (in {ttl_text})"


def fetch_lease(
    urls: Sequence[str], log: Log, timeout: float = FETCH_TIMEOUT_SECONDS
) -> Lease | None:
    """The first lease any of `urls` answers with; None when none does."""
    for url in urls:
        try:
            with urllib.request.urlopen(url, timeout=timeout) as resp:
                doc = json.load(resp)
        except (OSError, ValueError) as e:
            # URLError / HTTPError / a timeout are OSErrors; bad JSON is a
            # ValueError. Logged, then the next URL is tried.
            log(f"rig lease: {url} did not answer with JSON ({e})")
            continue
        if not is_lease(doc):
            log(
                f"rig lease: {url} answered something that is not a lease: {str(doc)[:200]}"
            )
            continue
        return doc
    return None


def gate(
    fetch: Callable[[], Lease | None],
    own_repo: str,
    poll_seconds: float,
    max_wait_seconds: float,
    clock: Callable[[], float],
    sleep: Callable[[float], None],
    log: Log,
) -> int:
    """Wait while another repo holds the lease (the module doc); the exit code.

    A lease seen held stays held through up to `OUTAGE_READS - 1`
    unreachable reads in a row; the next one is an outage (WARN, deploy).
    """
    deadline = clock() + max_wait_seconds
    held: Lease | None = None  # the last live lease of another repo read
    unreachable = 0  # unreachable reads in a row
    while True:
        lease = fetch()
        if lease is None:
            unreachable += 1
            if held is None or unreachable >= OUTAGE_READS:
                log(
                    "::warning::rig lease: the lease service is not reachable - deploying"
                    " without a lease check (an outage never blocks a deploy)"
                )
                return EXIT_PROCEED
            log(
                f"rig lease: not reachable ({unreachable} of {OUTAGE_READS} reads in a"
                " row) right after it was held - still treating it as held"
            )
        else:
            unreachable = 0
            verdict = decide(lease, own_repo)
            if verdict == FREE:
                log("rig lease: free - deploying")
                return EXIT_PROCEED
            if verdict == STALE:
                log(
                    "rig lease: stale (its holder stopped beating) - deploying:"
                    f" {describe_holder(lease)}"
                )
                return EXIT_PROCEED
            if verdict == OWN:
                log(f"rig lease: held by this repo ({own_repo}) - deploying")
                return EXIT_PROCEED
            held = lease
        remaining = deadline - clock()
        if remaining <= 0:
            log(
                f"::error::rig lease: still held after {max_wait_seconds / 60:.0f} min by"
                f" {describe_holder(held)} - not deploying over another repo's lease"
            )
            return EXIT_STILL_HELD
        pause = min(poll_seconds, remaining)
        log(
            f"rig lease: held by {describe_holder(held)} - waiting; next check in"
            f" {pause:.0f} s, giving up in {remaining / 60:.1f} min"
        )
        sleep(pause)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--own-repo", required=True, help="this repo, OWNER/NAME")
    parser.add_argument(
        "--url",
        action="append",
        required=True,
        help="a /rig-lease.json URL; repeat it, the first that answers wins",
    )
    parser.add_argument("--poll-seconds", type=float, default=DEFAULT_POLL_SECONDS)
    parser.add_argument(
        "--max-wait-seconds", type=float, default=DEFAULT_MAX_WAIT_SECONDS
    )
    args = parser.parse_args(argv)
    if args.poll_seconds <= 0 or args.max_wait_seconds < 0:
        parser.error("--poll-seconds must be > 0 and --max-wait-seconds >= 0")

    def log(message: str) -> None:
        print(message, flush=True)

    return gate(
        fetch=lambda: fetch_lease(args.url, log),
        own_repo=args.own_repo,
        poll_seconds=args.poll_seconds,
        max_wait_seconds=args.max_wait_seconds,
        clock=time.monotonic,
        sleep=time.sleep,
        log=log,
    )


if __name__ == "__main__":
    # The runner's console may not be UTF-8; a holder field must never crash
    # the gate while it prints.
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="backslashreplace")
    sys.exit(main())
