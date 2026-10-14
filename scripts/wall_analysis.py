"""What a capture of a wall measurement clip shows (#272 lane 1): the pure
analysis behind ``wall_frames.py``.

``analyse`` turns a run of readings (``Reading``: one per captured picture,
``wall_frames.read_frame``) into the report: boundaries and the refreshes
each was on screen for (another count than the wall's refresh rate gives
is judder; with capture times a stall that held a picture shows here too,
at the capture's edges as well; a wall that stopped presenting is
``frozen``), lost frames, lost boundaries, jumps back, cadence errors,
loops, and the blend weights against the pacer's rule for the clip's fps on
SongPlayer's 30 fps grid (``.claude/rules/genlock.md``: 25 on 30 = plain,
213, 170, 128, 85, 42), at the grid phase fitted per play. What the capture
did not see (unreadable pictures, spans it was blind) can excuse at most
one boundary per refresh a boundary takes, as ``unverified``; anything
beyond that is lost. ``clean`` needs positive evidence: a capture with
unverified steps, under ``MIN_COVERAGE`` decoded pictures, no refresh rate,
or fewer than ``MIN_BOUNDARIES`` (and ``MIN_JUDGED`` of its) boundaries
judged for judder is ``incomplete``, never ``clean``. The clip LOOPS (the
test playlist's mode): the song's end and the new play are not the measured
cadence, so the steps at a loop seam are reported under ``loop_seams`` and
kept out of the verdict while they are a seam's own irregularity (forward,
at most two grid steps): a bigger one is judged like any step; a jump to
the start from mid-clip is a jump back.

The capture's timing is read as BRACKETS, never as rounded guesses. Each
picture has a time and the span right before it the capture could not see
(``blind``: dxcam, since the poll before it — an empty poll proves nothing
was presented; a recording, since the picture before it). A boundary was
surely on screen at each of its pictures (``held_range``: the refreshes
two such instants span); it began after the last look that still showed
the boundary before it and at or before its first picture, and ended after
its own last look and at or before the next boundary's first picture. A
boundary is judged only when that bracket proves its refresh count — in the
expected counts or outside them — and a wall refresh the capture was blind
to (``hidden_refreshes``) can hide a boundary. A recording whose every
picture is one wall refresh (a capture of the wall's own signal at its
rate) is ``locked``: its pictures sit on the refreshes. dxcam's present
rate is checked against the given rate (``check_present_rate``). The
wall's refresh rate must be GIVEN: a rate read from the capture is the
capture's, not the wall's. Everything here is pure and unit-tested on the
pacer's readings.
"""

from __future__ import annotations

import math
import statistics
from collections import Counter
from dataclasses import dataclass
from fractions import Fraction

import wall_clip as clip

GRID_FPS = 30
# The pacer's dead band (`BLEND_DEAD_BAND_100NS`, 2 ms): no blend that close
# after the shown frame, and only for a gap strictly inside (1, 2) slots by it.
DEAD_BAND_S = Fraction(2, 1000)
# A cell whose level is within LEVEL_EDGE of black or white is a plain bit.
LEVEL_EDGE = 0.04
# analyse(): a step or a weight off by more than this (Q8) is an error.
Q8_TOLERANCE = 8
# Two readings show the same picture within the Q8 tolerance plus the level
# edge (one picture near a weight of 0 or 256 can read grey or plain).
SAME_PICTURE = (Q8_TOLERANCE + 256 * LEVEL_EDGE) / 256
# A loop: the clip's last WRAP_FRAMES frames to its first WRAP_FRAMES.
WRAP_FRAMES = 3
# Float slack, in refresh periods, for times that sit on whole periods
# (the capture's clock, the pts' printed decimals): never real jitter.
EPS = 1e-6
# At a loop seam a boundary may be held this long (the new play's open)
# before the hold is a stall; the report gives the longest seam hold.
SEAM_HOLD_S = 1.0
# dxcam: a picture whose poll came within this share of a refresh after the
# poll before it is timed to its present (check_present_rate).
TIMED_SHARE = 0.5
# Below this share of decoded pictures, or of boundaries judged for
# judder, a capture is incomplete.
MIN_COVERAGE = 0.95
MIN_JUDGED = 0.9
# A clean capture judges at least this many boundaries for judder (10 s on
# the 30 fps grid): a short one cannot show a fault that comes every few
# seconds. ``--min-boundaries`` sets it.
MIN_BOUNDARIES = 300
LIST_MAX = 20
# check_present_rate() needs this many timed intervals.
MIN_TIMED = 10


@dataclass(frozen=True)
class Reading:
    """One captured picture: what its strip rows say."""

    status: str
    index: int | None = None
    next_index: int | None = None
    weight_q8: int = 0
    strips: int = 0

    @property
    def ok(self) -> bool:
        return self.status == "ok"

    def position(self) -> float:
        """The clip position it shows, in frames (``index + weight``)."""
        assert self.index is not None
        return self.index + self.weight_q8 / 256


def blend_q8(position: Fraction, fps: int, grid_fps: int = GRID_FPS) -> int:
    """The weight (Q8, 0 = plain) the pacer shows at clip ``position``
    (frames): ``into * 256 / gap`` floored, none inside the dead band, none
    for a gap not strictly inside (one slot, two slots) by it."""
    gap = Fraction(1, fps)
    slot = Fraction(1, grid_fps)
    if not slot + DEAD_BAND_S < gap < 2 * slot - DEAD_BAND_S:
        return 0
    frac = position - math.floor(position)
    if frac * gap < DEAD_BAND_S:
        return 0
    return math.floor(frac * 256)


def readable_positions(
    position: Fraction, fps: int, grid_fps: int = GRID_FPS
) -> list[Fraction]:
    """Where the reader may read the pacer's boundary at clip ``position``:
    at its weight, and — for a weight within the level edge plus the Q8
    tolerance of 0 or 256 (the program's 8-bit blend rounds it either way of
    the edge) — as the plain frame at that end."""
    whole = math.floor(position)
    q8 = blend_q8(position, fps, grid_fps)
    band = 256 * LEVEL_EDGE + Q8_TOLERANCE
    out = [whole + Fraction(q8, 256)]
    if q8 and q8 >= 256 - band:
        out.append(Fraction(whole + 1))
    if q8 and q8 <= band:
        out.append(Fraction(whole))
    return out


def cycle_length(fps: int, grid_fps: int = GRID_FPS) -> int:
    """Grid boundaries per repeat of the clip/grid pattern (25/30: 6)."""
    return Fraction(fps, grid_fps).denominator


def expected_weights(
    fps: int, grid_fps: int = GRID_FPS, phase: Fraction = Fraction(0)
) -> list[int]:
    """One cycle of the weights the pacer shows, from a boundary at clip
    position ``phase`` (25 on 30 from 0: 0, 213, 170, 128, 85, 42)."""
    step = Fraction(fps, grid_fps)
    return [
        blend_q8(phase + k * step, fps, grid_fps)
        for k in range(cycle_length(fps, grid_fps))
    ]


@dataclass
class Boundary:
    """Consecutive decoded readings that show the same picture."""

    first_sample: int
    last_sample: int
    reading: Reading
    refreshes: int = 1

    def whole(self) -> bool:
        """Every sample from its first to its last was read (none undecoded)."""
        return self.last_sample - self.first_sample + 1 == self.refreshes


def boundaries(readings: list[Reading]) -> list[Boundary]:
    """Group the decoded readings into the boundaries they show: a reading
    within ``SAME_PICTURE`` of the one before shows the same picture. An
    undecoded reading between two such readings does not split them."""
    runs: list[Boundary] = []
    for n, reading in enumerate(readings):
        if not reading.ok:
            continue
        if runs and abs(runs[-1].reading.position() - reading.position()) <= (
            SAME_PICTURE
        ):
            runs[-1].refreshes += 1
            runs[-1].last_sample = n
        else:
            runs.append(Boundary(n, n, reading))
    return runs


@dataclass(frozen=True)
class Step:
    """From one boundary to the next: ``unseen`` refreshes the capture did
    not see between them (an unreadable picture, or one it was blind to)."""

    sample: int
    delta: float
    count: int
    wrapped: bool
    unseen: int
    kind: str | None
    excused: int = 0
    lost: int = 0


def step_tolerance(fps: int) -> float:
    """A step's slack (frames): the Q8 tolerance, plus the larger of the
    pacer's dead band (a boundary inside it shows the plain frame up to
    ``DEAD_BAND_S`` x fps frames before its position) and the reader's level
    edge (a weight within it of 256 reads as the next frame). Both never
    fall in one step."""
    dead_band = float(DEAD_BAND_S) * fps
    return Q8_TOLERANCE / 256 + max(dead_band, LEVEL_EDGE)


def expected_refreshes(refresh_hz: float | None, grid_fps: int) -> list[int]:
    """How many refreshes a boundary is on screen at ``refresh_hz`` (60 Hz
    on a 30 fps grid: 2; a rate the grid does not divide: the two around
    it); none without a rate. A rate below the grid cannot show every
    boundary: refused (``ValueError``)."""
    if not refresh_hz:
        return []
    per = refresh_hz / grid_fps
    if abs(per - round(per)) <= 0.01 * per:
        return [round(per)]
    if per < 1:
        raise ValueError(
            f"a refresh rate of {refresh_hz:g} Hz is below the {grid_fps} fps"
            " grid: it cannot show every boundary (give the wall's --refresh-hz)"
        )
    return [math.floor(per), math.ceil(per)]


def hidden_refreshes(blind: float, period: float) -> int:
    """The wall refreshes a capture blind for ``blind`` s before a picture
    can have missed whole (``ceil(blind / period) - 1``): dxcam, the
    presents in that span but the newest, which the poll that ends it gets;
    a recording, the refreshes no picture fell in."""
    return max(0, math.ceil(blind / period - EPS) - 1)


def pts_jitter(times: list[float]) -> float:
    """How far a recording's regular intervals stray from their mean: the
    pts' own quantization (a 1/10000 s timebase moves a 30 fps frame
    between 33.3 and 33.4 ms), never a refresh. Intervals over 1.5 x the
    mean (dropped frames) are not regular."""
    if len(times) < 3:
        return 0.0
    mean = (times[-1] - times[0]) / (len(times) - 1)
    gaps = [b - a for a, b in zip(times, times[1:])]
    regular = [abs(g - mean) for g in gaps if g < 1.5 * mean]
    return max(regular, default=0.0)


def capture_blind(times: list[float], blind: list[float] | None) -> list[float]:
    """Each picture's blind span: ``blind`` as given, else (a recording)
    the time since the picture before it; the first picture's is unknown
    (``inf``)."""
    if blind is not None:
        return blind
    return [math.inf] + [b - a for a, b in zip(times, times[1:])]


def unseen_refreshes(
    prev: Boundary,
    run: Boundary,
    blind: list[float] | None,
    period: float | None,
    jitter: float = 0.0,
) -> int:
    """How many refreshes between two boundaries' pictures the capture did
    not see: each unreadable picture, and the refreshes it was blind to
    before each picture after ``prev``'s last up to ``run``'s first
    (``hidden_refreshes``, less the times' own ``jitter``)."""
    unread = run.first_sample - prev.last_sample - 1
    if blind is None or not period:
        return unread
    pictures = range(prev.last_sample + 1, run.first_sample + 1)
    return unread + sum(hidden_refreshes(blind[k] - jitter, period) for k in pictures)


def steps_between(
    runs: list[Boundary],
    n_frames: int,
    fps: int,
    grid_fps: int = GRID_FPS,
    blind: list[float] | None = None,
    period: float | None = None,
    per_boundary: int = 1,
    jitter: float = 0.0,
) -> list[Step]:
    """Each step's clip advance (frames) and its grid boundaries, judged:
    ``jump_back`` (the clip went back), ``cadence`` (not a whole number of
    grid steps), ``lost`` (boundaries never shown) or ``None``. Boundaries
    a step skips that fit in the refreshes the capture did not see
    (``unseen``, ``per_boundary`` refreshes each: a boundary shown for
    fewer is judder) are ``excused``: such a step is ``unverified``, and
    only what does not fit is ``lost``. The clip's loop (its last frames to
    its first) is ``wrapped``, never a jump back."""
    step = fps / grid_fps
    tolerance = step_tolerance(fps)
    out = []
    for prev, run in zip(runs[:-1], runs[1:]):
        before, after = prev.reading.position(), run.reading.position()
        delta = after - before
        wrapped = before >= n_frames - WRAP_FRAMES and after < WRAP_FRAMES
        if wrapped:
            delta += n_frames
        count = round(delta / step)
        unseen = unseen_refreshes(prev, run, blind, period, jitter)
        kind = None
        excused = lost = 0
        if delta < -tolerance:
            kind = "jump_back"
        elif count == 0 or abs(delta - count * step) > tolerance:
            kind = "cadence"
        elif count > 1:
            excused = min(count - 1, unseen // per_boundary)
            lost = count - 1 - excused
            kind = "lost" if lost else "unverified"
        out.append(
            Step(run.first_sample, delta, count, wrapped, unseen, kind, excused, lost)
        )
    return out


def seam_steps(steps: list[Step], fps: int, grid_fps: int = GRID_FPS) -> set[int]:
    """The steps at a loop seam: the wrap itself and the step on each side
    (the song's end and the new play are not the measured cadence), each
    only while it is a seam's own irregularity: forward, at most two grid
    steps. A bigger one is judged like any step, so a seam never hides a
    loss or a jump."""
    step = fps / grid_fps
    tolerance = step_tolerance(fps)
    seam: set[int] = set()
    for n, s in enumerate(steps):
        if s.wrapped:
            seam.update(k for k in (n - 1, n, n + 1) if 0 <= k < len(steps))
    return {k for k in seam if -tolerance <= steps[k].delta <= 2 * step + tolerance}


def analyse(
    readings: list[Reading],
    fps: int,
    grid_fps: int = GRID_FPS,
    frames: int | None = None,
    times: list[float] | None = None,
    refresh_hz: float | None = None,
    blind: list[float] | None = None,
    span: tuple[float, float] | None = None,
    min_boundaries: int = MIN_BOUNDARIES,
    locked: bool = False,
) -> dict:
    """The report of a capture of the ``fps`` clip (see the module doc).

    ``times`` are the pictures' capture times (s), ``blind`` each one's
    blind span before it (``capture_blind``), ``span`` the capture's own
    start and end (default: its first and last picture), ``locked`` whether
    each picture is one wall refresh. Without times, each sample is one
    refresh. The refresh check needs the wall's ``refresh_hz``. Errors at a
    loop seam (``seam_steps``) are reported under ``loop_seams``; the
    verdict counts the steady cadence. ``clean`` needs ``min_boundaries``
    boundaries judged for judder."""
    n_frames = clip.frame_count(fps, clip.CLIP_SECONDS) if frames is None else frames
    period = 1 / refresh_hz if refresh_hz else None
    expected = expected_refreshes(refresh_hz, grid_fps)
    jitter = 0.0
    if times is not None:
        if blind is None:  # a recording: its pts' quantization is no blindness
            jitter = pts_jitter(times)
        blind = capture_blind(times, blind)
        if span is None and times:
            span = (times[0], times[-1])
    runs = boundaries(readings)
    steps = steps_between(
        runs,
        n_frames,
        fps,
        grid_fps,
        blind,
        period,
        min(expected, default=1),
        jitter,
    )
    seam = seam_steps(steps, fps, grid_fps)
    steady = [s for n, s in enumerate(steps) if n not in seam]
    at_seam = [s for n, s in enumerate(steps) if n in seam]
    jumps = [s.sample for s in steady if s.kind == "jump_back"]
    losses = frame_losses(runs, steps, seam, n_frames, fps, grid_fps)
    weights = weight_report(runs, steps, seam, fps, grid_fps)
    timing = Timing(times, blind, period, locked, span, jitter)
    refresh = refresh_report(runs, steps, seam, expected, timing)
    decoded = sum(r.ok for r in readings)
    report: dict = {
        "fps": fps,
        "grid_fps": grid_fps,
        "frames_in_clip": n_frames,
        "samples": len(readings),
        "decoded": decoded,
        "undecoded": dict(Counter(r.status for r in readings if not r.ok)),
        "boundaries": len(runs),
        "refreshes_per_boundary": {
            str(k): v for k, v in sorted(Counter(b.refreshes for b in runs).items())
        },
        "refreshes": refresh,
        "frozen": frozen(times, span, grid_fps),
        "frames_seen": len(frames_seen(runs, steps, n_frames)),
        "lost_frames": len(losses["lost"]),
        "lost_frame_list": losses["lost"][:LIST_MAX],
        "lost_boundaries": sum(s.lost for s in steady if s.kind == "lost"),
        "jumps_back": len(jumps),
        "jump_back_samples": jumps[:LIST_MAX],
        "cadence_errors": sum(s.kind == "cadence" for s in steady),
        "unverified": {
            "steps": sum(s.excused > 0 for s in steady),
            "boundaries": sum(s.excused for s in steady),
            "frames": losses["unverified"],
        },
        "loops": sum(s.wrapped for s in steps),
        "loop_seams": {
            "steps": len(at_seam),
            "errors": sum(s.kind is not None for s in at_seam)
            + losses["seam"]
            + weights.pop("seam_mismatches"),
        },
        "weights": weights,
    }
    if refresh is not None:
        report["loop_seams"]["hold_s"] = refresh.pop("seam_hold_s")
    if times is not None and blind is not None:
        report["capture"] = capture_report(times, blind, period, jitter)
    report["min_boundaries"] = min_boundaries
    report["verdict"] = verdict(report)
    return report


def frozen(
    times: list[float] | None, span: tuple[float, float] | None, grid_fps: int
) -> bool:
    """Whether the capture got fewer new pictures than half the grid's
    boundaries over its span: the wall stopped presenting (one picture in
    20 s has no refresh rate to judge). A span of two slots or less never
    is: one picture is half of it."""
    if not times or span is None:
        return False
    slots = (span[1] - span[0]) * grid_fps
    return len(times) < slots / 2


def verdict(report: dict) -> str:
    """``defects`` for any loss, jump back, cadence error, weight mismatch,
    judder or a frozen wall (or nothing decoded); else ``incomplete`` when
    the evidence is short: unverified steps, under ``MIN_COVERAGE`` of the
    pictures decoded, no refresh check, a boundary the capture saw whole yet
    cannot resolve (``unresolved``), or fewer boundaries judged for it than
    the report's ``min_boundaries`` and ``MIN_JUDGED`` of them; else
    ``clean``."""
    refresh = report["refreshes"]
    defects = (
        report["lost_frames"]
        + report["lost_boundaries"]
        + report["jumps_back"]
        + report["cadence_errors"]
        + report["weights"]["mismatches"]
        + (refresh["uneven"] if refresh else 0)
        + report["frozen"]
    )
    if defects or not report["boundaries"]:
        return "defects"
    coverage = report["decoded"] / report["samples"]
    judged = refresh["judged"] if refresh else 0
    if report["unverified"]["steps"] or coverage < MIN_COVERAGE:
        return "incomplete"
    if refresh and refresh["unresolved"]:
        return "incomplete"
    if judged < max(report["min_boundaries"], MIN_JUDGED * report["boundaries"]):
        return "incomplete"
    return "clean"


def frames_seen(runs: list[Boundary], steps: list[Step], n_frames: int) -> set[int]:
    """Every clip frame a boundary showed (alone or blended), numbered on
    across the clip's loops."""
    seen: set[int] = set()
    loops = 0
    for n, run in enumerate(runs):
        if n and steps[n - 1].wrapped:
            loops += 1
        r = run.reading
        assert r.index is not None
        seen.add(r.index + loops * n_frames)
        if r.next_index is not None:
            seen.add(r.next_index + loops * n_frames)
    return seen


def frame_losses(
    runs: list[Boundary],
    steps: list[Step],
    seam: set[int],
    n_frames: int,
    fps: int,
    grid_fps: int,
) -> dict:
    """The clip frames no boundary showed, step by step: ``lost`` (the
    frames), ``unverified`` (how many the step's excused boundaries could
    have shown unseen: up to the last one's position, rounded up when it
    blends) and ``seam`` (how many, at a loop seam)."""
    seen = frames_seen(runs, steps, n_frames)
    step = Fraction(fps, grid_fps)
    lost: list[int] = []
    unverified = at_seam = 0
    loops = 0
    for n, s in enumerate(steps):
        before = loops
        loops += s.wrapped
        prev, run = runs[n].reading, runs[n + 1].reading
        assert prev.index is not None and run.index is not None
        # A blended prev's next frame is in ``seen``.
        last = prev.index + before * n_frames
        first = run.index + loops * n_frames
        missing = [f for f in range(last + 1, first) if f not in seen]
        if n in seam:
            at_seam += len(missing)
            continue
        start = prev.index + before * n_frames + Fraction(prev.weight_q8, 256)
        reach = math.ceil(start + s.excused * step)
        unseen = [f for f in missing if s.excused and f <= reach]
        unverified += len(unseen)
        lost.extend(f % n_frames for f in missing if f not in unseen)
    return {"lost": lost, "unverified": unverified, "seam": at_seam}


@dataclass(frozen=True)
class Timing:
    """When the capture looked (see ``analyse``); ``times`` is ``None`` for
    one sample per refresh."""

    times: list[float] | None
    blind: list[float] | None
    period: float | None
    locked: bool
    span: tuple[float, float] | None
    jitter: float = 0.0

    def coarse(self) -> bool:
        """Whether the capture is slower than the wall: its median blind
        span is over a refresh, so it misses refreshes all along."""
        spans = [x - self.jitter for x in self.blind or [] if math.isfinite(x)]
        return bool(spans) and statistics.median(spans) > self.period * (1 + EPS)

    def lag(self, k: int) -> float:
        """How long before picture ``k`` its content can have appeared: its
        blind span, less the refresh it is when ``locked``."""
        assert self.blind is not None and self.period is not None
        return max(0.0, self.blind[k] - (self.period if self.locked else 0.0))


def held_range(
    n: int, runs: list[Boundary], steps: list[Step], timing: Timing
) -> tuple[int, float]:
    """The refreshes boundary ``n`` can have been on screen: ``(lo, hi)``,
    ``hi`` ``inf`` when its start or end lies outside what the capture saw.

    ``lo``: on screen at its first and last picture (the capture's last
    boundary: to the capture's end), so in every refresh between. ``hi``
    and a tighter ``lo``: it began after the last look that still showed
    the boundary before it and at or before its first picture, and ended
    after its own last look and at or before the next boundary's first
    picture (the last look before a boundary's first picture, with no
    unreadable picture between, still showed the boundary before it: a
    refresh hidden in a blind span starts later still). Without times:
    one sample per refresh."""
    run = runs[n]
    last = len(runs) - 1
    clean_in = 0 < n and run.first_sample == runs[n - 1].last_sample + 1
    clean_out = n < last and runs[n + 1].first_sample == run.last_sample + 1
    if timing.times is None:
        lo = run.last_sample - run.first_sample + 1
        return lo, lo if clean_in and clean_out else math.inf
    t, p = timing.times, timing.period
    assert p is not None
    a, b = t[run.first_sample], t[run.last_sample]
    if n == last and timing.span is not None:
        b = max(b, timing.span[1])
    # Locked: every time sits on a refresh and the content changes ON one,
    # so the bounds are whole periods, closed. Else instants: open.
    if timing.locked:
        lo = round((b - a) / p) + 1
    else:
        lo = math.floor((b - a) / p + EPS) + 1
    if n < last:
        t_next = t[runs[n + 1].first_sample]
    if n < last and clean_out and not timing.locked:
        # It ended after the last look before the next one's first picture
        # (locked, that look is its own last picture: nothing to add).
        end_lo = max(b, t_next - timing.lag(runs[n + 1].first_sample))
        lo = max(lo, math.floor((end_lo - a) / p + EPS) + 1)
    if n in (0, last):
        return lo, math.inf
    start_lo = t[runs[n - 1].last_sample]
    if clean_in:
        start_lo = max(start_lo, a - timing.lag(run.first_sample))
    high = (t_next - start_lo) / p
    return lo, round(high) if timing.locked else math.ceil(high - EPS) - 1


def refresh_report(
    runs: list[Boundary],
    steps: list[Step],
    seam: set[int],
    expected: list[int],
    timing: Timing,
) -> dict | None:
    """Every boundary on screen for the ``expected`` refreshes, else
    ``uneven`` (judder: held too long, a stall included, or too short).
    A boundary is judged when ``held_range`` proves it: all its possible
    counts expected, or none of them (a capture edge, an unreadable picture
    or a blind refresh only leaves the count open; a count already too high
    is judder whatever the rest). The boundaries a loop seam step leads to
    (the song's last, the new play's first two) are judged only for a hold
    over ``SEAM_HOLD_S`` (``seam_hold_s``: the longest). ``judged`` counts
    the boundaries judged; ``unresolved`` the inner ones it still cannot
    tell from judder with nothing unseen on either side (the capture too
    coarse for the wall), or any, when the capture is slower than the wall
    (``Timing.coarse``) — a clean verdict needs none (review
    round 5: a 60 fps camera on a 75 Hz wall could prove a 2 / 3 refresh
    boundary but leave a 4 open). ``None`` without a refresh rate."""
    if not timing.period:
        return None
    allowance = round(SEAM_HOLD_S / timing.period)
    uneven = []
    judged = unresolved = 0
    seam_hold = 0
    last = len(runs) - 1
    coarse = timing.coarse()
    for n, run in enumerate(runs):
        lo, hi = held_range(n, runs, steps, timing)
        if n and n - 1 in seam:
            seam_hold = max(seam_hold, lo)
            if lo > max(expected) + allowance:
                judged += 1
                uneven.append(run.first_sample)
            continue
        if lo > max(expected) or hi < min(expected):
            judged += 1
            uneven.append(run.first_sample)
        elif hi <= max(expected) and all(m in expected for m in range(lo, hi + 1)):
            judged += 1
        elif 0 < n < last:
            unresolved += coarse or not (steps[n - 1].unseen or steps[n].unseen)
    return {
        "refresh_hz": round(1 / timing.period, 3),
        "expected": expected,
        "judged": judged,
        "uneven": len(uneven),
        "uneven_samples": uneven[:LIST_MAX],
        "unresolved": unresolved,
        "seam_hold_s": round(seam_hold * timing.period, 3),
    }


def weight_report(
    runs: list[Boundary],
    steps: list[Step],
    seam: set[int],
    fps: int,
    grid_fps: int,
) -> dict:
    """The observed weights against the pacer's rule, at the phase fitted
    per play (a loop or a jump back starts a new one, which may sit on
    another phase). A boundary next to a seam step counts under
    ``seam_mismatches``."""
    cycle = cycle_length(fps, grid_fps)
    answer: dict = {
        "expected_cycle_from_0": expected_weights(fps, grid_fps),
        "observed_first_cycle": [b.reading.weight_q8 for b in runs[:cycle]],
        "phase_frames": [],
        "expected_cycle_fitted": [],
        "mismatches": 0,
        "seam_mismatches": 0,
        "max_residual_q8": 0.0,
    }
    near_seam = {k for n in seam for k in (n, n + 1)}
    for segment in play_segments(runs, steps):
        counts = [0]
        for n in segment[1:]:
            counts.append(counts[-1] + steps[n - 1].count)
        positions = [runs[n].reading.position() for n in segment]
        pairs = list(zip(positions, counts))
        fit = [p for n, p in zip(segment, pairs) if n not in near_seam] or pairs
        phase = fit_phase([u for u, _ in fit], [k for _, k in fit], fps, grid_fps)
        answer["phase_frames"].append(round(float(phase), 4))
        if not answer["expected_cycle_fitted"]:
            answer["expected_cycle_fitted"] = expected_weights(fps, grid_fps, phase)
        for n, u, k in zip(segment, positions, counts):
            residual = weight_residual_q8(u, phase, k, fps, grid_fps)
            if n in near_seam:
                answer["seam_mismatches"] += residual > Q8_TOLERANCE
                continue
            answer["mismatches"] += residual > Q8_TOLERANCE
            answer["max_residual_q8"] = max(
                answer["max_residual_q8"], round(residual, 2)
            )
    return answer


def play_segments(runs: list[Boundary], steps: list[Step]) -> list[list[int]]:
    """The boundaries of each play through the clip, split where a new one
    starts: at a loop, and at a jump back."""
    segments: list[list[int]] = [[]] if runs else []
    for n in range(len(runs)):
        if n and (steps[n - 1].wrapped or steps[n - 1].kind == "jump_back"):
            segments.append([])
        segments[-1].append(n)
    return segments


def fit_phase(
    positions: list[float], counts: list[int], fps: int, grid_fps: int
) -> Fraction:
    """The clip position (modulo one frame) of the grid the boundaries sit
    on: each position less its grid steps, centred on their circular mean,
    then their median (a few wrong boundaries do not move it), plus the half
    Q8 step the pacer's floored weight loses on average."""
    step = fps / grid_fps
    offsets = [(u - k * step) % 1 for u, k in zip(positions, counts)]
    angles = [2 * math.pi * x for x in offsets]
    mean = math.atan2(
        sum(math.sin(a) for a in angles), sum(math.cos(a) for a in angles)
    ) / (2 * math.pi)
    spread = [(x - mean + 0.5) % 1 - 0.5 for x in offsets]
    fitted = (mean + statistics.median(spread) + 0.5 / 256) % 1
    return Fraction(fitted).limit_denominator(1 << 16)


def weight_residual_q8(
    position: float, phase: Fraction, count: int, fps: int, grid_fps: int
) -> float:
    """How far (Q8, modulo whole frames: a frame off is the step check's) a
    boundary's position is from the nearest place the reader may read what
    the pacer shows ``count`` grid steps into a grid at ``phase``."""
    expected = phase + count * Fraction(fps, grid_fps)
    best = 1.0
    for shown in readable_positions(expected, fps, grid_fps):
        off = (position - float(shown)) % 1
        best = min(best, off, 1 - off)
    return 256 * best


def capture_report(
    times: list[float], blind: list[float], period: float | None, jitter: float
) -> dict:
    """The capture's own cadence: the interval between pictures (ms), the
    pictures after a span it was blind to a whole refresh in
    (``blind_pictures``), and the longest blind span (ms)."""
    gaps = [(b - a) * 1000 for a, b in zip(times[:-1], times[1:])]
    if not gaps:
        return {"interval_ms_p50": None, "interval_ms_max": None, "blind_pictures": 0}
    spans = [x for x in blind[1:] if math.isfinite(x)]
    hidden = [hidden_refreshes(x - jitter, period) for x in spans] if period else []
    return {
        "interval_ms_p50": round(statistics.median(gaps), 3),
        "interval_ms_max": round(max(gaps), 3),
        "pts_jitter_ms": round(jitter * 1000, 3),
        "blind_pictures": sum(h > 0 for h in hidden),
        "blind_ms_max": round(max(spans, default=0.0) * 1000, 3),
    }


def check_present_rate(
    times: list[float], refresh_hz: float, blind: list[float]
) -> None:
    """A dxcam capture's pictures are the wall's presents: the median
    interval between two pictures it timed (each within ``TIMED_SHARE`` of a
    refresh of its poll before) is a whole number of refreshes. Else
    ``--refresh-hz`` is not the wall's (``ValueError``). Fewer than
    ``MIN_TIMED`` such intervals: no check."""
    near = TIMED_SHARE / refresh_hz
    timed = [
        times[n + 1] - times[n]
        for n in range(len(times) - 1)
        if blind[n] <= near and blind[n + 1] <= near
    ]
    if len(timed) < MIN_TIMED:
        return
    period = statistics.median(timed)
    per = period * refresh_hz
    if round(per) < 1 or abs(per - round(per)) > 0.1:
        raise ValueError(
            f"the wall presents every {period * 1000:.3f} ms, no whole number of"
            f" {refresh_hz:g} Hz refreshes: give the wall's own --refresh-hz"
        )
