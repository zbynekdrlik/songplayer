#!/usr/bin/env python3
"""#308: the post-deploy E2E puts SP-program's playlist back as it found it.

The E2E job's title check and every post-deploy spec restore SP-program's
start scene through a cut, and a cut to a playlist re-kicks it: the
playlist starts its next song whether it was playing or paused before. A
box that carried silence before the job (camera-box's rig, 11.11.2026)
then plays a song after it.

    program_transport.py save <file>     # the job's first box step
    program_transport.py restore <file>  # its last step

`save` records SP-program's source and, for a playlist, its transport
(`/api/v1/ndi/health` row). `restore` pauses that playlist again when it
was not playing at the save, is still the program's source and plays now,
then reads the pause back. Anything else is left as it is (an operator's
cut or play meanwhile wins). Stdlib only: it runs on the box's Python.
"""

from __future__ import annotations

import json
import sys
import time
import urllib.request
from pathlib import Path
from typing import Callable, Optional

BASE = "http://127.0.0.1:8920"
PAUSE_READBACK_S = 10.0
TIMEOUT_S = 10.0

Get = Callable[[str], object]
Post = Callable[[str], None]


def http_get(path: str) -> object:
    with urllib.request.urlopen(BASE + path, timeout=TIMEOUT_S) as resp:
        return json.loads(resp.read().decode("utf-8"))


def http_post(path: str) -> None:
    req = urllib.request.Request(BASE + path, data=b"", method="POST")
    with urllib.request.urlopen(req, timeout=TIMEOUT_S) as resp:
        resp.read()


def transport_of(health: object, playlist_id: int) -> Optional[str]:
    """The transport of `playlist_id`'s health row, `None` when it has none."""
    if not isinstance(health, list):
        raise ValueError("/api/v1/ndi/health is not a list")
    for row in health:
        if isinstance(row, dict) and row.get("playlist_id") == playlist_id:
            t = row.get("transport")
            return t if isinstance(t, str) else None
    return None


def read_state(get: Get) -> dict:
    """SP-program's source and, for a playlist (id >= 0), its transport."""
    program = get("/api/v1/program")
    if not isinstance(program, dict):
        raise ValueError("/api/v1/program is not an object")
    source = program.get("source")
    if not isinstance(source, int) or isinstance(source, bool):
        return {"source": None, "transport": None}
    if source < 0:
        return {"source": source, "transport": None}
    return {
        "source": source,
        "transport": transport_of(get("/api/v1/ndi/health"), source),
    }


def to_pause(start: dict, now: dict) -> Optional[int]:
    """The playlist to pause again: the start's, when it did not play then,
    is still on program and plays now; else `None`."""
    source = start.get("source")
    if not isinstance(source, int) or source < 0:
        return None
    still_on = now.get("source") == source
    replays = start.get("transport") == "Playing" and now.get("transport") == "Playing"
    return source if still_on and replays else None


def save(path: Path, get: Get) -> dict:
    state = read_state(get)
    path.write_text(json.dumps(state), encoding="utf-8")
    return state


def restore(
    path: Path,
    get: Get,
    post: Post,
    sleep: Callable[[float], None] = time.sleep,
    readback_s: float = PAUSE_READBACK_S,
) -> str:
    """What was done, as one line; raises when a pause does not read back."""
    start = json.loads(path.read_text(encoding="utf-8"))
    now = read_state(get)
    pid = to_pause(start, now)
    if pid is None:
        return f"left as it is (start {start}, now {now})"
    post(f"/api/v1/playback/{pid}/pause")
    for _ in range(int(readback_s * 2)):
        if transport_of(get("/api/v1/ndi/health"), pid) != "Playing":
            return f"paused playlist {pid} again (start {start}, now {now})"
        sleep(0.5)
    raise RuntimeError(f"playlist {pid} still plays {readback_s} s after its pause")


def main(argv: list[str], get: Get = http_get, post: Post = http_post) -> int:
    if len(argv) != 2 or argv[0] not in ("save", "restore"):
        print("usage: program_transport.py save|restore <file>", file=sys.stderr)
        return 2
    path = Path(argv[1])
    if argv[0] == "save":
        print(f"[#308] SP-program at the start: {save(path, get)}")
    else:
        print(f"[#308] SP-program's playlist {restore(path, get, post)}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
