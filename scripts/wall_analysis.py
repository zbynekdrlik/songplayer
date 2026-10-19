"""What a capture of a wall measurement clip shows (#272 lane 1): the pure
analysis behind ``wall_frames.py``.

``analyse`` turns a run of readings (``Reading``: one per captured picture,
``wall_frames.read_frame``) into the report: boundaries and the refreshes
each was on screen for (another count than the wall's refresh rate gives
is judder; with capture times a stall that held a picture shows here too,
at the capture's edges as well; a wall that stopped presenting is
``frozen``), the distinct pictures shown (a re-show counted once), lost
frames, lost boundaries, jumps back, cadence errors,
loops, and the blend weights against the pacer's rule for the clip's fps on
SongPlayer's 30 fps grid (``.claude/rules/genlock.md``: 25 on 30 = plain,
213, 170, 128, 85, 42), at the grid phase fitted per play. What the capture
did not see (unreadable pictures, spans it was blind) can excuse at most
one boundary per refresh a boundary takes, as ``unverified``; anything
beyond that is lost. ``clean`` needs positive evidence: a capture with
unverified steps, under ``MIN_COVERAGE`` decoded pictures, no refresh rate,
an inner boundary it left open, or fewer than ``MIN_BOUNDARIES`` boundaries
judged for judder is ``incomplete``, never ``clean``. The clip LOOPS (the
test playlist's mode): the song's last boundary shows the last frame plain
(the pacer's clamp), which ``step_count`` models, so losses and cadence at
a loop seam are judged like any; the seam is spared only the new play's
open (its two boundaries may be held up to ``SEAM_HOLD_S`` longer) and the
clamped frame's weight, both reported under ``loop_seams``; a jump to the
start from mid-clip is a jump back.

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
TIMED_SHARE = 0.1
# The wall's period is known to this share: the given --refresh-hz may be
# its nominal rate (60 for a 60/1.001 wall, 0.1 %); dxcam's measured
# present rate must agree to it (check_present_rate).
RATE_TOLERANCE = 0.0015
# How far a dxcam stamp may be off. A picture's poll is stamped before its
# grab and its acquire window closed after it (``Timing.late``): the frame
# was on screen inside that window, never assumed at the stamp (review round
# 7: a grab preempted between the stamp and the acquire read judder).
POLL_TIME_ERROR = 0.0005
# The least a recording's instant may be off: showinfo prints 6 decimals.
PTS_TIME_ERROR = 1e-6
# Below this share of decoded pictures a capture is incomplete.
MIN_COVERAGE = 0.95
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
    """Consecutive decoded readings that show the same picture: its first
    and last sample, its first and last reading (within ``SAME_PICTURE`` of
    each other: a weight near 256 and the next frame plain), and how many
    pictures read it."""

    first_sample: int
    last_sample: int
    reading: Reading
    pictures: int = 1
    last_reading: Reading | None = None

    def end(self) -> Reading:
        """Its last reading."""
        return self.last_reading or self.reading


def mixed_exposures(
    readings: list[Reading], n_frames: int, fps: int, grid_fps: int
) -> list[Reading]:
    """A camera's exposure lasts: one taken across a boundary change mixes
    the two and reads a position between them (review round 7: a 120 fps
    camera read hundreds of cadence errors on a clean wall). A lone decoded
    picture strictly between its decoded neighbours, off the grid from the
    one before (the capture's first: to the one after; its last: from the
    one before), is such a mix: ``mixed``, unreadable — the boundaries
    beside it stay open (unresolved), never a cadence error. A real
    off-grid boundary a camera sees once is then incomplete, never clean."""

    def off(x: Reading, y: Reading) -> bool:
        return step_count(x, y, n_frames, fps, grid_fps)[2]

    out = list(readings)
    last = len(readings) - 1
    for k, r in enumerate(readings):
        a = readings[k - 1] if k > 0 and readings[k - 1].ok else None
        b = readings[k + 1] if k < last and readings[k + 1].ok else None
        if not r.ok or (a is None and b is None):
            continue
        v = r.position()
        after = a is None or a.position() + SAME_PICTURE < v
        before = b is None or v < b.position() - SAME_PICTURE
        if after and before and (off(a, r) if a else off(r, b)):
            out[k] = Reading("mixed")
    return out


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
            runs[-1].pictures += 1
            runs[-1].last_sample = n
            runs[-1].last_reading = reading
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


def hidden_refreshes(blind: float, period: float, error: float = 0.0) -> int:
    """The wall refreshes a capture blind for ``blind`` s before a picture
    can have missed whole: dxcam, the presents in that span but the newest,
    which the poll that ends it gets; a recording, the refreshes no picture
    fell in — ``ceil(blind / period) - 1``, the span taken as long as its
    instants' ``error`` and the period as short as ``RATE_TOLERANCE`` allow
    (an excuse errs to unverified, never to lost)."""
    longest = (blind + 2 * error) / (period * (1 - RATE_TOLERANCE))
    return max(0, math.ceil(longest - EPS) - 1)


def pts_jitter(times: list[float]) -> float:
    """How far a recording's regular intervals stray from their mean: the
    pts' own quantization (a 1/10000 s timebase moves a 30 fps frame
    between 33.3 and 33.4 ms), so about how far one instant may be off.
    Intervals over 1.5 x the mean (dropped frames) are not regular."""
    gaps = [b - a for a, b in zip(times, times[1:])]
    if not gaps:
        return 0.0
    mean = statistics.fmean(gaps)
    return max((abs(g - mean) for g in gaps if g < 1.5 * mean), default=0.0)


def capture_blind(times: list[float], blind: list[float] | None) -> list[float]:
    """Each picture's blind span: ``blind`` as given, else (a recording)
    the time since the picture before it; the first picture's is unknown
    (``inf``)."""
    if blind is not None:
        return blind
    return [math.inf] + [b - a for a, b in zip(times, times[1:])]


@dataclass(frozen=True)
class Timing:
    """When the capture looked (see ``analyse``): ``times`` ``None`` = one
    sample per refresh; ``blind`` each picture's blind span; ``error`` how
    far one capture instant may be off; ``locked`` each picture one wall
    refresh (its instants on the refreshes)."""

    times: list[float] | None
    blind: list[float] | None
    period: float | None
    locked: bool
    span: tuple[float, float] | None
    error: float = 0.0
    late: list[float] | None = None

    def by(self, k: int) -> float:
        """An instant at or after picture ``k``'s content was on screen:
        its time (the frame was on screen at or after it), plus its acquire
        window (``late``: dxcam stamps a poll before its grab)."""
        assert self.times is not None
        return self.times[k] + (self.late[k] if self.late else 0.0)

    def lag(self, k: int) -> float:
        """How long before picture ``k`` its content can have appeared: its
        blind span, less the refresh it is when ``locked``."""
        assert self.blind is not None and self.period is not None
        return max(0.0, self.blind[k] - (self.period if self.locked else 0.0))

    def fewest(self, span: float) -> float:
        """The fewest periods ``span`` s between two instants can be."""
        assert self.period is not None
        return (span - 2 * self.error) / (self.period * (1 + RATE_TOLERANCE))

    def most(self, span: float) -> float:
        """The most periods ``span`` s between two instants can be."""
        assert self.period is not None
        return (span + 2 * self.error) / (self.period * (1 - RATE_TOLERANCE))

    def hidden(self, k: int) -> int:
        """The refreshes the capture can have missed whole before picture
        ``k`` (locked: the frames missing from the gap)."""
        assert self.blind is not None and self.period is not None
        if self.locked:
            return max(0, round(self.blind[k] / self.period) - 1)
        late = self.late[k] if self.late else 0.0
        return hidden_refreshes(self.blind[k] + late, self.period, self.error)


def unseen_refreshes(prev: Boundary, run: Boundary, timing: Timing) -> int:
    """How many refreshes between two boundaries' pictures the capture did
    not see: each unreadable picture, and the refreshes it was blind to
    before each picture after ``prev``'s last up to ``run``'s first."""
    unread = run.first_sample - prev.last_sample - 1
    if timing.blind is None or not timing.period:
        return unread
    pictures = range(prev.last_sample + 1, run.first_sample + 1)
    return unread + sum(timing.hidden(k) for k in pictures)


def is_last_frame(reading: Reading, n_frames: int) -> bool:
    """Whether a reading shows the clip's last frame plain: the song's last
    boundary of a play, its true position anywhere in that frame (there is
    no next frame to blend: the pacer clamps)."""
    return reading.index == n_frames - 1 and reading.next_index is None


def step_count(
    before: Reading, after: Reading, n_frames: int, fps: int, grid_fps: int
) -> tuple[float, int, bool]:
    """A step's clip advance (frames), how many grid steps it is, and
    whether that is off the grid. Into the song's last boundary the advance
    may be any whole number of steps that lands in the last frame (the
    clamp); across the loop, it is the song's boundaries left after
    ``before`` (none from its last) plus the new play's from frame 0."""
    step = fps / grid_fps
    tolerance = step_tolerance(fps)
    u, v = before.position(), after.position()
    if v < u:  # across the loop (steps_between decides it is one)
        left = 0
        if not is_last_frame(before, n_frames):
            left = max(0, math.ceil((n_frames - u - tolerance) / step - EPS) - 1)
        played = round(v / step)
        off = abs(v - played * step) > tolerance
        return v + n_frames - u, left + played + 1, off
    if is_last_frame(after, n_frames):
        # The last frame is a whole frame wide, a step at most one: some
        # whole number of steps always lands in it; the fewest is taken.
        k = max(1, math.ceil((n_frames - 1 - u - tolerance) / step - EPS))
        return v - u, k, False
    count = round((v - u) / step)
    return v - u, count, count == 0 or abs(v - u - count * step) > tolerance


def steps_between(
    runs: list[Boundary],
    n_frames: int,
    fps: int,
    grid_fps: int,
    timing: Timing,
    per_boundary: int = 1,
) -> list[Step]:
    """Each step's clip advance and its grid boundaries (``step_count``),
    its ``kind`` ``jump_back`` (the clip went back), ``cadence`` (off the
    grid) or ``None``. Boundaries a step skips that fit in the refreshes
    the capture did not see (``unseen``, ``per_boundary`` refreshes each: a
    boundary shown for fewer is judder) are ``excused`` (the step is
    unverified); only what does not fit is ``lost``. The clip's loop is ``wrapped``: back to its start from
    no further from its end than ``WRAP_FRAMES`` and the boundaries the
    capture could have missed reach."""
    step = fps / grid_fps
    tolerance = step_tolerance(fps)
    out = []
    for prev, run in zip(runs[:-1], runs[1:]):
        unseen = unseen_refreshes(prev, run, timing)
        # From the boundary's last reading: the song's last boundary can
        # read the frame before plain at a weight near 256, then the clamp.
        before, after = prev.end().position(), run.reading.position()
        reach = WRAP_FRAMES + (unseen // per_boundary) * step
        wrapped = after < before and n_frames - before + after <= reach
        if after < before and not wrapped:  # back: a jump, or off the grid
            delta, count, off = after - before, 0, True
        else:
            delta, count, off = step_count(
                prev.end(), run.reading, n_frames, fps, grid_fps
            )
        kind = None
        excused = lost = 0
        if not wrapped and delta < -tolerance:
            kind = "jump_back"
        elif off:
            kind = "cadence"
        elif count > 1:  # boundaries skipped: excused, or lost
            excused = min(count - 1, unseen // per_boundary)
            lost = count - 1 - excused
        out.append(
            Step(run.first_sample, delta, count, wrapped, unseen, kind, excused, lost)
        )
    return out


def seam_steps(steps: list[Step]) -> set[int]:
    """The steps at a loop seam: the wrap and the step on each side. Their
    losses and cadence are judged like any step's (``step_count`` knows the
    song-end clamp); what the seam is spared is its timing (the song's last
    boundary and the new play's first may be held up to
    ``SEAM_HOLD_S``) and its weights (the clamped last frame is plain)."""
    seam: set[int] = set()
    for n, s in enumerate(steps):
        if s.wrapped:
            seam.update(k for k in (n - 1, n, n + 1) if 0 <= k < len(steps))
    return seam


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
    late: list[float] | None = None,
) -> dict:
    """The report of a capture of the ``fps`` clip (see the module doc).

    ``times`` are the pictures' capture times (s); ``blind`` each one's
    blind span before it when the pictures are the wall's presents (dxcam:
    since the poll before), else ``None``: a recording, each picture an
    instant sample (``capture_blind``); ``span`` the capture's own start
    and end (default: its first and last picture); ``late`` each dxcam
    picture's acquire window after its time; ``locked`` whether each
    recorded picture is one wall refresh. Without times, each sample is one
    refresh. The refresh check needs the wall's ``refresh_hz``. ``clean``
    needs ``min_boundaries`` boundaries judged for judder."""
    n_frames = clip.frame_count(fps, clip.CLIP_SECONDS) if frames is None else frames
    period = 1 / refresh_hz if refresh_hz else None
    expected = expected_refreshes(refresh_hz, grid_fps)
    presents = blind is not None
    error = 0.0
    if times is not None:
        error = POLL_TIME_ERROR if presents else max(pts_jitter(times), PTS_TIME_ERROR)
        blind = capture_blind(times, blind)
        if span is None and times:
            span = (times[0], times[-1])
    timing = Timing(times, blind, period, locked, span, error, late)
    if times is not None and not presents and not locked:
        readings = mixed_exposures(readings, n_frames, fps, grid_fps)
    runs = boundaries(readings)
    steps = steps_between(
        runs, n_frames, fps, grid_fps, timing, min(expected, default=1)
    )
    seam = seam_steps(steps)
    jumps = [s.sample for s in steps if s.kind == "jump_back"]
    losses = frame_losses(runs, steps, n_frames, fps, grid_fps)
    weights = weight_report(runs, steps, seam, fps, grid_fps, n_frames)
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
        "distinct_boundaries": distinct_boundaries(runs, steps, n_frames),
        "pictures_per_boundary": {
            str(k): v for k, v in sorted(Counter(b.pictures for b in runs).items())
        },
        "refreshes": refresh,
        # Only presents tell a stopped wall: a recording keeps its frames.
        "frozen": frozen(times if presents else None, span, grid_fps),
        "frames_seen": len(frames_seen(runs, steps, n_frames)),
        "lost_frames": len(losses["lost"]),
        "lost_frame_list": losses["lost"][:LIST_MAX],
        "lost_boundaries": sum(s.lost for s in steps),
        "jumps_back": len(jumps),
        "jump_back_samples": jumps[:LIST_MAX],
        "cadence_errors": sum(s.kind == "cadence" for s in steps),
        "unverified": {
            "steps": sum(s.excused > 0 for s in steps),
            "boundaries": sum(s.excused for s in steps),
            "frames": losses["unverified"],
        },
        "loops": sum(s.wrapped for s in steps),
        "loop_seams": {
            "steps": len(seam),
            "weight_mismatches": weights.pop("seam_mismatches"),
        },
        "weights": weights,
    }
    if refresh is not None:
        report["loop_seams"]["hold_s"] = refresh.pop("seam_hold_s")
    if times is not None:
        report["capture"] = capture_report(times, timing)
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
    pictures decoded, no refresh check, an inner boundary the capture
    cannot resolve (``unresolved``), or fewer boundaries judged for it than
    the report's ``min_boundaries`` (an inner boundary is judged or
    unresolved, so only the capture's edges and the seams stay out); else
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
    if refresh and (refresh["unresolved"] or refresh["inconsistent"]):
        return "incomplete"
    if judged < report["min_boundaries"]:
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


def distinct_boundaries(runs: list[Boundary], steps: list[Step], n_frames: int) -> int:
    """The distinct pictures the boundaries showed, numbered on across the
    clip's loops: a run within ``SAME_PICTURE`` of a picture shown before
    (a jump back re-shows one, and so can the steps after it) is that
    picture again, never a new one. The pictures the program's boundaries
    should have shown, minus this, are the ones the wall did not show or
    the capture did not see (``unverified``) (#279: a back adds nothing, and
    a capture that is dead or freezes counts every picture it did not
    show, where ``lost_boundaries`` sees only skips between two readings
    and counts the step out of a back as one). Away from a loop seam: the
    new play's open hold (``loop_seams.hold_s``) repeats pictures, and a
    back across the seam reads as one more loop. A run is one picture
    within ``SAME_PICTURE`` of its FIRST reading, as ``boundaries``
    groups it."""
    positions = []
    loops = 0
    for n, run in enumerate(runs):
        if n and steps[n - 1].wrapped:
            loops += 1
        positions.append(run.reading.position() + loops * n_frames)
    positions.sort()
    return sum(
        k == 0 or v - positions[k - 1] > SAME_PICTURE for k, v in enumerate(positions)
    )


def frame_losses(
    runs: list[Boundary],
    steps: list[Step],
    n_frames: int,
    fps: int,
    grid_fps: int,
) -> dict:
    """The clip frames no boundary showed, step by step: ``lost`` (the
    frames) and ``unverified`` (how many the step's excused boundaries
    could have shown unseen: up to the last one's position, rounded up
    when it blends; across the loop, all of them when nothing was lost)."""
    seen = frames_seen(runs, steps, n_frames)
    step = Fraction(fps, grid_fps)
    lost: list[int] = []
    unverified = 0
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
        if s.wrapped:
            unseen = missing if s.excused and not s.lost else []
        else:
            start = prev.index + before * n_frames + Fraction(prev.weight_q8, 256)
            reach = math.ceil(start + s.excused * step)
            unseen = [f for f in missing if s.excused and f <= reach]
        unverified += len(unseen)
        lost.extend(f % n_frames for f in missing if f not in unseen)
    return {"lost": lost, "unverified": unverified}


def held_range(n: int, runs: list[Boundary], timing: Timing) -> tuple[int, float]:
    """The refreshes boundary ``n`` can have been on screen: ``(lo, hi)``,
    ``hi`` ``inf`` when its start or end lies outside what the capture saw.

    ``lo``: on screen at its first and last picture (the capture's last
    boundary: to the capture's end, when no picture follows it; else to the
    last look before the next one), so in every refresh between. ``hi`` and
    a tighter ``lo``: it began after the last look that still showed the
    boundary before it and at or before its first picture, and ended after
    its own last look and at or before the next boundary's first picture
    (the last look before a boundary's first picture, with no unreadable
    picture between, still showed the boundary before it: a refresh hidden
    in a blind span starts later still). Every span is taken as short and as
    long as the instants' error and the rate's tolerance allow
    (``Timing.fewest`` / ``most``). Without times: one sample per refresh."""
    run = runs[n]
    last = len(runs) - 1
    clean_in = 0 < n and run.first_sample == runs[n - 1].last_sample + 1
    clean_out = n < last and runs[n + 1].first_sample == run.last_sample + 1
    if timing.times is None:
        lo = run.last_sample - run.first_sample + 1
        return lo, lo if clean_in and clean_out else math.inf
    t, p = timing.times, timing.period
    assert p is not None
    # A lower bound of when a picture's content was on screen is its time,
    # an upper bound ``by`` (the end of dxcam's acquire window).
    a, b = t[run.first_sample], t[run.last_sample]
    a_by = timing.by(run.first_sample)
    if n == last and timing.span is not None:
        after = run.last_sample + 1
        if after < len(t):  # unreadable pictures follow: to the look before
            b = max(b, t[after] - timing.blind[after])
        else:
            b = max(b, timing.span[1])
    # Locked: every time sits on a refresh and the content changes ON one,
    # so the bounds are whole periods, closed. Else instants: open.
    if timing.locked:
        lo = round((b - a) / p) + 1
    else:
        lo = math.floor(timing.fewest(b - a_by) + EPS) + 1
    if n < last and clean_out and not timing.locked:
        # It ended after the last look before the next one's first picture
        # (locked, that look is its own last picture: nothing to add).
        following = runs[n + 1].first_sample
        end_lo = max(b, t[following] - timing.lag(following))
        lo = max(lo, math.floor(timing.fewest(end_lo - a_by) + EPS) + 1)
    if n in (0, last):
        return lo, math.inf
    start_lo = t[runs[n - 1].last_sample]
    if clean_in:
        start_lo = max(start_lo, a - timing.lag(run.first_sample))
    next_by = timing.by(runs[n + 1].first_sample)
    if timing.locked:
        return lo, round((next_by - start_lo) / p)
    return lo, math.ceil(timing.most(next_by - start_lo) - EPS) - 1


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
    is judder whatever the rest). The song's last boundary and the new
    play's first (around a wrap) may be held up to ``SEAM_HOLD_S`` longer (``seam_hold_s``: the longest), never shorter. ``judged`` counts
    the boundaries judged; ``unresolved`` the inner ones left open — the
    capture too coarse for the wall, an unreadable picture or a blind span
    beside it: what it did not resolve could hide judder, so a clean
    verdict needs none (review rounds 5-6: a 60 fps camera on a 75 Hz wall
    left a 4-refresh boundary open; a stall that also stalled dxcam hid
    judder). ``None`` without a refresh rate."""
    if not timing.period:
        return None
    allowance = round(SEAM_HOLD_S / timing.period)
    uneven = []
    judged = unresolved = inconsistent = 0
    seam_hold = 0
    last = len(runs) - 1
    # The song's last boundary and the new play's first, around each wrap.
    held = {k for w, s in enumerate(steps) if s.wrapped for k in (w, w + 1)}
    for n, run in enumerate(runs):
        lo, hi = held_range(n, runs, timing)
        if lo > hi:  # no count fits: the capture's timing broke its model
            inconsistent += 1
            continue
        if n in held:
            seam_hold = max(seam_hold, lo)
            if lo > max(expected) + allowance or hi < min(expected):
                judged += 1
                uneven.append(run.first_sample)
            continue
        if lo > max(expected) or hi < min(expected):
            judged += 1
            uneven.append(run.first_sample)
        elif hi <= max(expected) and all(m in expected for m in range(lo, hi + 1)):
            judged += 1
        elif 0 < n < last:
            unresolved += 1
    return {
        "refresh_hz": round(1 / timing.period, 3),
        "expected": expected,
        "judged": judged,
        "uneven": len(uneven),
        "uneven_samples": uneven[:LIST_MAX],
        "unresolved": unresolved,
        "inconsistent": inconsistent,
        "seam_hold_s": round(seam_hold * timing.period, 3),
    }


def weight_report(
    runs: list[Boundary],
    steps: list[Step],
    seam: set[int],
    fps: int,
    grid_fps: int,
    n_frames: int,
) -> dict:
    """The observed weights against the pacer's rule, at the phase fitted
    per play (a loop or a jump back starts a new one, which may sit on
    another phase). A boundary next to a seam step, or showing the clip's
    last frame plain (the clamp: no weight), counts under
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
    near_seam |= {k for k, r in enumerate(runs) if is_last_frame(r.reading, n_frames)}
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


def capture_report(times: list[float], timing: Timing) -> dict:
    """The capture's own cadence: the interval between pictures (ms), how
    far one instant may be off (ms), the pictures after a span it can have
    missed a whole refresh in (``blind_pictures``), and the longest blind
    span (ms)."""
    gaps = [(b - a) * 1000 for a, b in zip(times[:-1], times[1:])]
    if not gaps:
        return {"interval_ms_p50": None, "interval_ms_max": None, "blind_pictures": 0}
    assert timing.blind is not None
    spans = [x for x in timing.blind[1:] if math.isfinite(x)]
    blind = (
        sum(timing.hidden(k) > 0 for k in range(1, len(times))) if timing.period else 0
    )
    return {
        "interval_ms_p50": round(statistics.median(gaps), 3),
        "interval_ms_max": round(max(gaps), 3),
        "time_error_ms": round(timing.error * 1000, 4),
        "blind_pictures": blind,
        "blind_ms_max": round(max(spans, default=0.0) * 1000, 3),
    }


def check_locked(times: list[float], refresh_hz: float) -> None:
    """A ``--locked`` recording's pictures are the wall's refreshes: every
    interval between them is a whole number of periods (within a quarter),
    and their count over their sum matches the given rate to
    ``RATE_TOLERANCE``. Else it is no recording of the wall's own signal
    (``ValueError``): a 120 fps camera on a 60 Hz wall. A recorder clocked
    apart from the signal at the same nominal rate passes this and drifts
    against the wall: ``--locked`` needs the signal's own clock."""
    gaps = [b - a for a, b in zip(times, times[1:])]
    if not gaps:
        return
    counts = [g * refresh_hz for g in gaps]
    bad = [c for c in counts if round(c) < 1 or abs(c - round(c)) > 0.25]
    if bad:
        raise ValueError(
            f"--locked: {len(bad)} picture intervals are no whole {refresh_hz:g} Hz"
            f" refreshes (one is {bad[0]:.2f}): not a recording of the wall's own"
            " signal"
        )
    measured = sum(round(c) for c in counts) / sum(gaps)
    if abs(measured / refresh_hz - 1) > RATE_TOLERANCE:
        raise ValueError(
            f"--locked: the recording runs at {measured:.3f} Hz, not the wall's"
            f" {refresh_hz:g} Hz"
        )


def check_present_rate(
    times: list[float],
    refresh_hz: float,
    blind: list[float],
    late: list[float] | None = None,
) -> None:
    """A dxcam capture's pictures are the wall's presents: the intervals
    between successive pictures it timed (each known within ``TIMED_SHARE``
    of a refresh: its poll before plus its acquire window ``late``) are
    whole numbers of refreshes at the given rate: their refreshes over
    their sum, the rate measured over the whole capture, matches it to
    ``RATE_TOLERANCE``. Else ``--refresh-hz`` is not the wall's
    (``ValueError``). Fewer than ``MIN_TIMED`` such intervals: the rate
    cannot be checked, and an unchecked rate would read as judder or losses
    (review round 7), so that is a ``ValueError`` too."""
    near = TIMED_SHARE / refresh_hz
    windows = late or [0.0] * len(times)
    timed_at = [t for t, x, w in zip(times, blind, windows) if x + w <= near]
    timed = [b - a for a, b in zip(timed_at, timed_at[1:])]
    if len(timed) < MIN_TIMED:
        raise ValueError(
            f"only {len(timed)} intervals timed within {near * 1000:.2f} ms: the"
            f" {refresh_hz:g} Hz refresh rate cannot be checked (a faster capture,"
            " or a smaller --region)"
        )
    whole = [max(1, round(d * refresh_hz)) for d in timed]
    measured = sum(whole) / sum(timed)
    if abs(measured / refresh_hz - 1) > RATE_TOLERANCE:
        raise ValueError(
            f"the wall presents at {measured:.3f} Hz, not in whole {refresh_hz:g} Hz"
            " refreshes: give the wall's own --refresh-hz"
        )
