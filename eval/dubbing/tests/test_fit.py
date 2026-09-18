"""Pure unit tests for eval/dubbing/fit.py — one for every placement branch,
the branch boundaries, and the edge cases (last line, zero-length slot, room
clamped to zero), plus the segment-level disposition counter."""

from __future__ import annotations

from eval.dubbing import fit


def _plan(start, end, boundary, nat, **kw):
    return fit.plan_line(
        index=0, start_ms=start, end_ms=end, boundary_ms=boundary, nat_dur_ms=nat, **kw
    )


def test_fit_branch_clip_inside_slot():
    p = _plan(0, 2000, 3000, 1500)
    assert p.disposition == "fit"
    assert p.at_ms == 0
    assert p.tempo == 1.0
    assert p.cut is False
    assert p.overflow is False
    assert p.placed_dur_ms == 1500
    assert p.cut_ms == 0
    assert p.overflow_ms == 0
    assert p.fade_ms == 0


def test_fit_branch_exactly_slot_length():
    # nat == slot is still a fit (not a tempo change).
    p = _plan(0, 1000, 5000, 1000)
    assert p.disposition == "fit"
    assert p.tempo == 1.0
    assert p.placed_dur_ms == 1000


def test_tempo_branch_within_15_percent():
    p = _plan(0, 1000, 5000, 1100)
    assert p.disposition == "tempo"
    assert p.tempo == 1.1
    assert p.cut is False
    assert p.overflow is False
    # A tempo line is compressed to exactly its slot.
    assert p.placed_dur_ms == 1000


def test_tempo_branch_at_the_115_boundary():
    # Ratio exactly 1.15 must still be the tempo branch (<=), not overflow.
    p = _plan(0, 1000, 5000, 1150)
    assert p.disposition == "tempo"
    assert p.tempo == 1.15
    assert p.placed_dur_ms == 1000


def test_overflow_branch_compressed_fits_before_next_guard():
    p = _plan(0, 1000, 5000, 2000)
    assert p.disposition == "overflow"
    assert p.tempo == 1.15
    assert p.cut is False
    assert p.overflow is True
    # compressed = round(2000 / 1.15) = 1739
    assert p.placed_dur_ms == 1739
    assert p.overflow_ms == 1739 - 1000
    assert p.cut_ms == 0
    assert p.fade_ms == 0


def test_cut_branch_compressed_exceeds_room():
    # Next sentence starts at 1200; room = 1200 - 150 - 0 = 1050 < compressed 1739.
    p = _plan(0, 1000, 1200, 2000)
    assert p.disposition == "cut"
    assert p.tempo == 1.15
    assert p.cut is True
    assert p.placed_dur_ms == 1050  # == room
    assert p.cut_ms == 1739 - 1050
    assert p.fade_ms == fit.FADE_MS
    # room (1050) > slot (1000), so it does overflow its own slot before the cut.
    assert p.overflow is True


def test_room_clamped_to_zero_forces_a_zero_length_cut():
    # boundary earlier than start + guard -> room clamps to 0.
    p = _plan(1000, 2000, 1000, 3000)
    assert p.room_ms == 0
    assert p.disposition == "cut"
    assert p.placed_dur_ms == 0
    assert p.cut is True


def test_zero_length_slot_skips_tempo_branch():
    # slot_dur == 0 must not divide-by-zero; it falls through to overflow/cut.
    p = _plan(0, 0, 5000, 1000)
    assert p.disposition == "overflow"
    assert p.tempo == 1.15
    assert p.slot_dur_ms == 0
    assert p.placed_dur_ms == round(1000 / 1.15)


def test_negative_nat_is_clamped_and_fits():
    p = _plan(0, 1000, 5000, -50)
    assert p.disposition == "fit"
    assert p.nat_dur_ms == 0
    assert p.placed_dur_ms == 0


def test_plan_segment_uses_next_start_then_segment_end_and_counts():
    lines = [
        {"index": 0, "start_ms": 0, "end_ms": 1000, "nat_dur_ms": 800},  # fit
        {"index": 1, "start_ms": 1000, "end_ms": 2000, "nat_dur_ms": 1100},  # tempo
        {
            "index": 2,
            "start_ms": 2000,
            "end_ms": 3000,
            "nat_dur_ms": 5000,
        },  # last -> cut
    ]
    placements, counts = fit.plan_segment(lines, segment_end_ms=3200)

    assert [p.disposition for p in placements] == ["fit", "tempo", "cut"]
    # Line 1's boundary is line 2's start (2000), not the segment end.
    assert placements[1].room_ms == 2000 - fit.GUARD_MS - 1000
    # The last line's boundary is the segment end (3200).
    assert placements[2].room_ms == 3200 - fit.GUARD_MS - 2000
    assert counts["fit"] == 1
    assert counts["tempo"] == 1
    assert counts["cut"] == 1


def test_plan_segment_preserves_explicit_indices():
    lines = [
        {"index": 7, "start_ms": 0, "end_ms": 1000, "nat_dur_ms": 500},
        {"index": 9, "start_ms": 1000, "end_ms": 2000, "nat_dur_ms": 500},
    ]
    placements, _ = fit.plan_segment(lines, segment_end_ms=2000)
    assert [p.index for p in placements] == [7, 9]
