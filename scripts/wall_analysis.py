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

The capture's own timing is read here too, purely: a recording's pts gaps
and a recording slower than the wall (``video_blind_spans``), dxcam's late
polls (``late_polls``) and its present rate (``check_present_rate``).
Everything here is pure and unit-tested on the pacer's readings.
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
# A video pts interval over this many times the median: the recording was
# blind there (it dropped frames).
CAPTURE_GAP = 1.5
# A capture interval may run this share of a period over a whole number of
# periods without a refresh hidden in it (pts and poll jitter).
PERIOD_JITTER = 0.05
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
    not read between them; ``blind`` when the next boundary's first picture
    is ``late_timed``."""

    sample: int
    delta: float
    count: int
    wrapped: bool
    unseen: int
    kind: str | None
    excused: int = 0
    lost: int = 0
    blind: bool = False


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


def late_timed(
    times: list[float] | None, k: int, blind_spans: list[tuple[float, float]] | None
) -> bool:
    """Whether picture ``k`` ends a blind span: its time is a late poll's
    (dxcam: it may come over half a refresh after its present), or it
    follows a recording's gap."""
    if times is None or not blind_spans:
        return False
    return any(lo < times[k] <= hi for lo, hi in blind_spans)


def hidden_presents(length: float, period: float) -> int:
    """The presents a blind span ``length`` long can hide: all it can hold
    but the newest, which ends it (``ceil(length / period - PERIOD_JITTER)
    - 1``), and one once it is over a period (two presents a little over a
    period apart)."""
    span = length / period
    return max(math.ceil(span - PERIOD_JITTER) - 1, 1 if span > 1 else 0)


def unseen_refreshes(
    prev: Boundary,
    run: Boundary,
    times: list[float] | None,
    period: float | None,
    blind_spans: list[tuple[float, float]] | None,
) -> int:
    """How many refreshes between two boundaries' pictures the capture did
    not read: each unreadable picture, or, when more, the presents the blind
    spans between them can hide (``hidden_presents``; a span that only
    touches either picture, or lies elsewhere, hides none there)."""
    unseen = run.first_sample - prev.last_sample - 1
    if times is None or not period or not blind_spans:
        return unseen
    t0, t1 = times[prev.last_sample], times[run.first_sample]
    overlaps = [min(hi, t1) - max(lo, t0) for lo, hi in blind_spans]
    hidden = sum(hidden_presents(length, period) for length in overlaps if length > 0)
    return max(unseen, hidden)


def steps_between(
    runs: list[Boundary],
    n_frames: int,
    fps: int,
    grid_fps: int = GRID_FPS,
    times: list[float] | None = None,
    period: float | None = None,
    blind_spans: list[tuple[float, float]] | None = None,
    per_boundary: int = 1,
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
        unseen = unseen_refreshes(prev, run, times, period, blind_spans)
        blind = late_timed(times, run.first_sample, blind_spans)
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
            Step(
                run.first_sample,
                delta,
                count,
                wrapped,
                unseen,
                kind,
                excused,
                lost,
                blind,
            )
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


def capture_period(times: list[float] | None) -> float | None:
    """The median interval between captured pictures (s); ``None`` with
    fewer than two."""
    if times is None or len(times) < 2:
        return None
    return statistics.median(b - a for a, b in zip(times, times[1:]))


def video_blind_spans(
    times: list[float], refresh_hz: float | None
) -> list[tuple[float, float]]:
    """The spans a recording was blind: its pts gaps (``pts_gaps``), or,
    when it records slower than the wall refreshes (by ``CAPTURE_GAP``),
    every interval: a 30 fps camera on a 60 Hz wall sees every other
    refresh, so no judder and no 1-refresh boundary can be told."""
    period = capture_period(times)
    if refresh_hz and period and period > CAPTURE_GAP / refresh_hz:
        return list(zip(times, times[1:]))
    return pts_gaps(times)


def pts_gaps(times: list[float]) -> list[tuple[float, float]]:
    """The spans a recording was blind: a pts interval over ``CAPTURE_GAP``
    times the median (the recorder dropped frames there)."""
    period = capture_period(times)
    if period is None:
        return []
    limit = CAPTURE_GAP * period
    return [(a, b) for a, b in zip(times, times[1:]) if b - a > limit]


def analyse(
    readings: list[Reading],
    fps: int,
    grid_fps: int = GRID_FPS,
    frames: int | None = None,
    times: list[float] | None = None,
    refresh_hz: float | None = None,
    blind_spans: list[tuple[float, float]] | None = None,
    span: tuple[float, float] | None = None,
    min_boundaries: int = MIN_BOUNDARIES,
) -> dict:
    """The report of a capture of the ``fps`` clip (see the module doc).

    ``times`` are the pictures' capture times (s), ``blind_spans`` the
    spans the capture could not see, ``span`` the capture's own start and
    end (default: its first and last picture). The refresh rate is
    ``refresh_hz`` (``refresh_from`` "given"), else read from ``times``
    ("capture"), else there is no refresh check. Errors at a loop seam (``seam_steps``) are reported under
    ``loop_seams``; the verdict counts the steady cadence. ``clean`` needs
    ``min_boundaries`` boundaries judged for judder."""
    n_frames = clip.frame_count(fps, clip.CLIP_SECONDS) if frames is None else frames
    period = capture_period(times)
    rate_from = "given" if refresh_hz else "capture"
    if refresh_hz is None and period:
        refresh_hz = 1 / period
    period = 1 / refresh_hz if refresh_hz else None
    expected = expected_refreshes(refresh_hz, grid_fps)
    runs = boundaries(readings)
    steps = steps_between(
        runs,
        n_frames,
        fps,
        grid_fps,
        times,
        period,
        blind_spans,
        min(expected, default=1),
    )
    seam = seam_steps(steps, fps, grid_fps)
    steady = [s for n, s in enumerate(steps) if n not in seam]
    at_seam = [s for n, s in enumerate(steps) if n in seam]
    jumps = [s.sample for s in steady if s.kind == "jump_back"]
    losses = frame_losses(runs, steps, seam, n_frames, fps, grid_fps)
    weights = weight_report(runs, steps, seam, fps, grid_fps)
    if span is None and times:
        span = (times[0], times[-1])
    refresh = refresh_report(
        runs, steps, seam, refresh_hz, expected, times, span, blind_spans
    )
    if refresh is not None:
        # Read from the capture, the rate is the capture's own: a camera
        # slower than the wall then counts what it saw, not the wall.
        refresh["refresh_from"] = rate_from
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
    if times is not None:
        report["capture"] = capture_report(times, blind_spans or [])
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
    pictures decoded, no refresh check, or fewer boundaries judged for it
    than the report's ``min_boundaries`` and ``MIN_JUDGED`` of them; else
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


def refresh_report(
    runs: list[Boundary],
    steps: list[Step],
    seam: set[int],
    refresh_hz: float | None,
    expected: list[int],
    times: list[float] | None,
    span: tuple[float, float] | None,
    blind_spans: list[tuple[float, float]] | None = None,
) -> dict | None:
    """Every boundary on screen for the ``expected`` refreshes, else
    ``uneven`` (judder: held too long, a stall included, or too short). With
    capture times a boundary's count is the time from its first picture to
    the next boundary's (the last one: to the capture's end), so a wall that
    held a picture while the capture saw no new one counts in full. The
    capture's first and last boundary may have been on screen before or
    after it: judged for too long only. A boundary with an unreadable
    picture in it, or a step beside it with an unseen refresh or a late-timed
    picture (``Step.blind``), is judged only when the
    pictures it was read in already span more refreshes than a boundary
    may (too long whatever the rest showed); the song's last boundary and
    the new play's first two at a loop seam (the boundaries a seam step
    leads to) are not judged. ``judged`` counts the rest. ``None`` without
    a refresh rate."""
    if not refresh_hz:
        return None
    uneven = []
    judged = 0
    last = len(runs) - 1
    for n, run in enumerate(runs):
        if n and n - 1 in seam:
            continue
        beside = [steps[k] for k in (n - 1, n) if 0 <= k < len(steps)]
        if not run.whole() or any(s.unseen or s.blind for s in beside):
            if held_at_least(run, times, refresh_hz, blind_spans) > max(expected):
                judged += 1
                uneven.append(run.first_sample)
            continue
        shown = run.refreshes
        if times is not None:
            if n < last:
                stop = times[runs[n + 1].first_sample]
            else:
                stop = max(times[run.last_sample] + 1 / refresh_hz, span[1])
            shown = round((stop - times[run.first_sample]) * refresh_hz)
        judged += 1
        edge = n in (0, last)
        if shown > max(expected) if edge else shown not in expected:
            uneven.append(run.first_sample)
    return {
        "refresh_hz": round(refresh_hz, 3),
        "expected": expected,
        "judged": judged,
        "uneven": len(uneven),
        "uneven_samples": uneven[:LIST_MAX],
    }


def held_at_least(
    run: Boundary,
    times: list[float] | None,
    refresh_hz: float,
    blind_spans: list[tuple[float, float]] | None,
) -> int:
    """The refreshes a boundary was surely on screen. With capture times:
    from its first picture to its last one the capture timed (no blind span
    right before it: a dxcam picture's time is its poll's, up to half a
    refresh after its present), less that half refresh: on screen at two
    instants that far apart, so in at least ``floor(span) + 1`` refreshes.
    Without: one per sample."""
    if times is None:
        return run.last_sample - run.first_sample + 1
    k = run.last_sample
    while k > run.first_sample and late_timed(times, k, blind_spans):
        k -= 1
    held = (times[k] - times[run.first_sample]) * refresh_hz
    return max(1, math.floor(held - 0.5) + 1)


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


def capture_report(times: list[float], blind_spans: list[tuple[float, float]]) -> dict:
    """The capture's own cadence: the interval between pictures (ms), and
    the spans it was blind."""
    gaps = [(b - a) * 1000 for a, b in zip(times[:-1], times[1:])]
    blind_ms = sum((b - a) * 1000 for a, b in blind_spans)
    if not gaps:
        return {"interval_ms_p50": None, "interval_ms_max": None, "blind_spans": 0}
    return {
        "interval_ms_p50": round(statistics.median(gaps), 3),
        "interval_ms_max": round(max(gaps), 3),
        "blind_spans": len(blind_spans),
        "blind_ms": round(blind_ms, 3),
    }


def late_polls(polls: list[float], late: float | None) -> list[tuple[float, float]]:
    """The spans between two polls more than ``late`` apart: blind. dxcam's
    ``late`` is half a refresh: the picture after a later poll is timed by
    that poll, more than half a refresh after its present, so it cannot
    count refreshes; a gap over a whole refresh can also hold a present the
    next poll never gets (``unseen_refreshes``). A poll on time that found
    no new picture is the wall holding one. None without a period."""
    if not late:
        return []
    return [(a, b) for a, b in zip(polls, polls[1:]) if b - a > late]


def check_present_rate(
    times: list[float], refresh_hz: float, blind_spans: list[tuple[float, float]]
) -> None:
    """A dxcam capture's pictures are the wall's presents: the median
    interval between two pictures the capture timed (neither ``late_timed``) is
    a whole number of refreshes. Else ``--refresh-hz`` is not the wall's
    (``ValueError``). Fewer than ``MIN_TIMED`` such intervals: no check."""
    timed = [
        times[n + 1] - times[n]
        for n in range(len(times) - 1)
        if not late_timed(times, n, blind_spans)
        and not late_timed(times, n + 1, blind_spans)
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
