#!/usr/bin/env python3
"""fit.py — pure timing placement of synthesized dub lines onto the source timeline.

Given the source (English) sentence spans of the dabing recording segment and the NATURAL
duration of each synthesized Slovak line, decide where and how each line is laid
onto the dub track. This module is PURE (no audio, no I/O, no external deps) so
every branch is unit-tested on any box — the runner (`run_listening_test.py`)
applies the plan to real audio with ffmpeg + soundfile.

Placement policy (epic #174 dabing-dubbing spec, §6), evaluated per line:

  1. **fit**       — the natural clip already fits its slot
                     (`nat_dur <= slot_dur`). Placed at the line's own start
                     (`at_ms == start_ms`), tempo 1.0. Leaves natural padding.
  2. **tempo**     — it overruns the slot but a speed-up of at most `MAX_TEMPO`
                     (+15 %) brings it back to the slot
                     (`nat_dur / slot_dur <= MAX_TEMPO`). tempo = that ratio;
                     the placed clip is exactly `slot_dur` long.
  3. **overflow**  — even at `MAX_TEMPO` it is longer than its slot, but the
                     compressed clip still fits BEFORE the next sentence starts
                     (`compressed <= room`, where `room` reaches to the next
                     sentence start minus a `GUARD_MS` (150 ms) gap, or the
                     segment end for the last line). tempo = `MAX_TEMPO`; the
                     clip overflows its own slot into the gap, never onto the
                     next line.
  4. **cut**       — even compressed it would run past the next sentence's guard.
                     tempo = `MAX_TEMPO`, the clip is hard-cut to `room` with a
                     `FADE_MS` (50 ms) fade-out.

Every line is anchored at its OWN source start (`at_ms == start_ms`), so lines
never overlap — overflow/cut only govern how far a clip may run into the silence
AFTER its slot, bounded by the next sentence's guard. `plan_segment` returns the
per-line `Placement`s plus a `Counter` of dispositions.
"""

from __future__ import annotations

from collections import Counter
from dataclasses import dataclass, asdict

GUARD_MS = 150
"""Minimum silence kept before the next sentence a clip may overflow into."""

MAX_TEMPO = 1.15
"""Maximum speed-up (+15 %); above this, speech starts to sound rushed."""

FADE_MS = 50
"""Fade-out length applied when a clip has to be hard-cut."""


@dataclass
class Placement:
    """The decision for one dub line. All durations are whole milliseconds."""

    index: int
    disposition: str  # "fit" | "tempo" | "overflow" | "cut"
    at_ms: int  # timeline offset the clip is anchored at (== source start_ms)
    tempo: float  # atempo factor to apply (1.0 = unchanged)
    cut: bool  # was the clip hard-cut to fit?
    overflow: bool  # does the clip run past its own source slot?
    nat_dur_ms: int  # natural (synthesized) clip duration
    slot_dur_ms: int  # source sentence span
    room_ms: int  # room from start to the next guard boundary
    placed_dur_ms: int  # duration actually laid on the track
    cut_ms: int  # how much was trimmed (cut branch only, else 0)
    overflow_ms: int  # how far past the slot the clip runs (else 0)
    fade_ms: int  # fade-out applied (cut branch only, else 0)

    def as_dict(self) -> dict:
        return asdict(self)


def plan_line(
    *,
    index: int,
    start_ms: int,
    end_ms: int,
    boundary_ms: int,
    nat_dur_ms: int,
    guard_ms: int = GUARD_MS,
    max_tempo: float = MAX_TEMPO,
    fade_ms: int = FADE_MS,
) -> Placement:
    """Decide the placement of one line.

    `boundary_ms` is the start of the NEXT sentence (or the segment end for the
    last line). The usable room is `boundary_ms - guard_ms - start_ms`, clamped
    to >= 0.
    """
    slot_dur = max(0, end_ms - start_ms)
    room = max(0, boundary_ms - guard_ms - start_ms)
    nat = max(0, nat_dur_ms)

    # 1. fit — already inside its slot.
    if nat <= slot_dur:
        return Placement(
            index=index,
            disposition="fit",
            at_ms=start_ms,
            tempo=1.0,
            cut=False,
            overflow=False,
            nat_dur_ms=nat,
            slot_dur_ms=slot_dur,
            room_ms=room,
            placed_dur_ms=nat,
            cut_ms=0,
            overflow_ms=0,
            fade_ms=0,
        )

    # 2. tempo — a <= +15 % speed-up fits it back into the slot.
    if slot_dur > 0 and nat / slot_dur <= max_tempo:
        tempo = round(nat / slot_dur, 4)
        return Placement(
            index=index,
            disposition="tempo",
            at_ms=start_ms,
            tempo=tempo,
            cut=False,
            overflow=False,
            nat_dur_ms=nat,
            slot_dur_ms=slot_dur,
            room_ms=room,
            placed_dur_ms=slot_dur,
            cut_ms=0,
            overflow_ms=0,
            fade_ms=0,
        )

    # From here the clip needs the maximum speed-up.
    compressed = round(nat / max_tempo)

    # 3. overflow — compressed clip fits before the next guard boundary.
    if compressed <= room:
        return Placement(
            index=index,
            disposition="overflow",
            at_ms=start_ms,
            tempo=round(max_tempo, 4),
            cut=False,
            overflow=True,
            nat_dur_ms=nat,
            slot_dur_ms=slot_dur,
            room_ms=room,
            placed_dur_ms=compressed,
            cut_ms=0,
            overflow_ms=max(0, compressed - slot_dur),
            fade_ms=0,
        )

    # 4. cut — hard-cut to the room with a fade-out.
    return Placement(
        index=index,
        disposition="cut",
        at_ms=start_ms,
        tempo=round(max_tempo, 4),
        cut=True,
        overflow=room > slot_dur,
        nat_dur_ms=nat,
        slot_dur_ms=slot_dur,
        room_ms=room,
        placed_dur_ms=room,
        cut_ms=max(0, compressed - room),
        overflow_ms=max(0, room - slot_dur),
        fade_ms=fade_ms,
    )


def plan_segment(
    lines: list[dict],
    segment_end_ms: int,
    *,
    guard_ms: int = GUARD_MS,
    max_tempo: float = MAX_TEMPO,
    fade_ms: int = FADE_MS,
) -> tuple[list[Placement], Counter]:
    """Plan a whole segment.

    `lines` is an ordered list of dicts carrying at least `start_ms`, `end_ms`
    and `nat_dur_ms` (the synthesized clip's natural duration); an `index` is
    used if present, else the list position. The boundary for line *i* is the
    NEXT line's `start_ms`, or `segment_end_ms` for the last line.
    """
    placements: list[Placement] = []
    counts: Counter = Counter()
    n = len(lines)
    for i, line in enumerate(lines):
        boundary = lines[i + 1]["start_ms"] if i + 1 < n else segment_end_ms
        p = plan_line(
            index=int(line.get("index", i)),
            start_ms=int(line["start_ms"]),
            end_ms=int(line["end_ms"]),
            boundary_ms=int(boundary),
            nat_dur_ms=int(line["nat_dur_ms"]),
            guard_ms=guard_ms,
            max_tempo=max_tempo,
            fade_ms=fade_ms,
        )
        placements.append(p)
        counts[p.disposition] += 1
    return placements, counts
