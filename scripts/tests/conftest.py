"""Put ``scripts/`` on ``sys.path`` for every test in this directory.

A script imports its sibling modules by plain name, the way it runs on the box
(``python scripts/av_sync_check.py`` has ``scripts/`` as ``sys.path[0]``). For
example ``av_sync_check`` imports ``av_sync_drift``. A test that loads a
script by file path needs the same path, or that import fails.
"""

from __future__ import annotations

import sys
from pathlib import Path

_SCRIPTS_DIR = str(Path(__file__).resolve().parents[1])
if _SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, _SCRIPTS_DIR)
