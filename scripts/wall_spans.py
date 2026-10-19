"""How long a capture of the wall saw no new picture (#279 P3a's A/B,
``.claude/rules/video-decode.md``): ``wall_analysis.capture_report`` adds
``still_report`` to every capture's ``capture`` block.

``still_ms_max`` is the longest span with no new picture, the capture's own
edges included (dxcam: its first and last poll). ``held_ms_max`` is the
longest the output SURELY held: an empty dxcam poll proves nothing new was
presented (``wall_frames.poll_blind``), so a span is held but for the blind
span of the picture that ended it, and the capture's end whole. A hold over
1 s with the polls on reads in both; a capture that starved reads a long
still span that holds little.
"""

from __future__ import annotations

import math
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from wall_analysis import Timing


def still_spans(times: list[float], timing: Timing) -> list[tuple[float, float]]:
    """Each span with no new picture (s), the capture's own edges included
    (``span``: dxcam's first and last poll), with how much of it the output
    surely held: an empty poll proves nothing new was presented, and the
    picture that ended the span can have come anywhere in its blind span,
    so the span less that blind span; the capture's end, whole (its last
    poll found nothing new). A starved capture's span holds little."""
    if not times and timing.span is None:
        return []
    start, end = timing.span or (times[0], times[-1])
    blind = timing.blind or [math.inf] * len(times)
    edges = [start, *times]
    spans = [
        (b - a, max(0.0, b - a - blind[k]))
        for k, (a, b) in enumerate(zip(edges[:-1], edges[1:]))
    ]
    spans.append((end - edges[-1], end - edges[-1]))
    return spans


def still_report(times: list[float], timing: Timing) -> dict:
    """``still_ms_max``: the longest span with no new picture; ``held_ms_max``:
    the longest the output surely held (``still_spans``, ms; #279's A/B)."""
    spans = still_spans(times, timing)
    return {
        "still_ms_max": round(max((s for s, _ in spans), default=0.0) * 1000, 3),
        "held_ms_max": round(max((h for _, h in spans), default=0.0) * 1000, 3),
    }
