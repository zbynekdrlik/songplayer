"""Tests for combine_lines_times.py — the LLM-lines + ASR-times combiner used
by the 2026-08-05 offline "does the timing survive combination" experiment."""

from __future__ import annotations

from eval.lyrics import combine_lines_times as clt


def _timed_line(text: str, words: list[tuple[str, int, int]]) -> dict:
    """Build a time-source line: text + real per-word timestamps."""
    return {
        "text": text,
        "start_ms": words[0][1] if words else None,
        "end_ms": words[-1][2] if words else None,
        "text_sk": None,
        "words": [
            {"text": w, "start_ms": s, "end_ms": e, "confidence": 0.95}
            for w, s, e in words
        ],
    }


def _llm_line(text: str, text_sk: str | None = None) -> dict:
    """Build a line-source (audio-LLM) line: only text/text_sk matter — its
    own timestamps are deliberately wrong/estimated and never read."""
    return {
        "text": text,
        "start_ms": 999999,  # deliberately bogus — combiner must ignore it
        "end_ms": 999999,
        "text_sk": text_sk,
        "words": None,
    }


# --- normalize_word / tokenize_line_text -----------------------------------


def test_normalize_word_strips_punctuation_and_case() -> None:
    assert clt.normalize_word("'Cos") == "cos"
    assert clt.normalize_word("Jesus,") == "jesus"
    assert clt.normalize_word("there's") == "theres"
    assert clt.normalize_word("...") == ""


def test_tokenize_line_text_whitespace_split() -> None:
    assert clt.tokenize_line_text("Nothing satisfies like Jesus") == [
        "Nothing",
        "satisfies",
        "like",
        "Jesus",
    ]
    assert clt.tokenize_line_text("  extra   spaces  ") == ["extra", "spaces"]


# --- flatten_line_source / flatten_time_source ------------------------------


def test_flatten_line_source_tags_each_word_with_its_line() -> None:
    lines = [_llm_line("hello world"), _llm_line("second line here")]
    words = clt.flatten_line_source(lines)
    assert [w.text for w in words] == ["hello", "world", "second", "line", "here"]
    assert [w.line_idx for w in words] == [0, 0, 1, 1, 1]


def test_flatten_time_source_reads_real_word_timestamps() -> None:
    lines = [_timed_line("hello world", [("hello", 100, 400), ("world", 400, 900)])]
    words = clt.flatten_time_source(lines)
    assert len(words) == 2
    assert words[0] == clt.TimedWord(
        text="hello", start_ms=100, end_ms=400, source_line_idx=0
    )
    assert words[1].start_ms == 400 and words[1].end_ms == 900


def test_flatten_time_source_skips_line_with_no_words() -> None:
    lines = [
        {"text": "no timing here", "start_ms": 0, "end_ms": 1000, "words": []},
        _timed_line("has timing", [("has", 1000, 1200), ("timing", 1200, 1600)]),
    ]
    words = clt.flatten_time_source(lines)
    assert [w.text for w in words] == ["has", "timing"]


# --- align_word_streams: exact match -----------------------------------


def test_align_exact_match_all_words() -> None:
    line_words = clt.flatten_line_source([_llm_line("hello world today")])
    time_words = clt.flatten_time_source(
        [
            _timed_line(
                "hello world today",
                [("hello", 0, 300), ("world", 300, 700), ("today", 700, 1100)],
            )
        ]
    )
    alignments = clt.align_word_streams(line_words, time_words)
    assert len(alignments) == 3
    assert [a.kind for a in alignments] == ["exact", "exact", "exact"]
    # monotonic: line_word_idx and time_word_idx both strictly increasing
    assert [a.line_word_idx for a in alignments] == [0, 1, 2]
    assert [a.time_word_idx for a in alignments] == [0, 1, 2]


# --- align_word_streams: small word substitutions -----------------------------------


def test_align_recovers_minor_spelling_drift_via_fuzzy_match() -> None:
    # "cause" (LLM) vs "caus" (ASR mis-transcription) is a same-length
    # replace block of length 1 -> should fuzzy-recover (ratio 0.89).
    line_words = clt.flatten_line_source([_llm_line("cause freedom")])
    time_words = clt.flatten_time_source(
        [_timed_line("caus freedom", [("caus", 0, 300), ("freedom", 300, 900)])]
    )
    alignments = clt.align_word_streams(line_words, time_words)
    assert len(alignments) == 2
    assert alignments[0].kind == "fuzzy"
    assert alignments[0].ratio >= clt.FUZZY_MATCH_RATIO_THRESHOLD
    assert alignments[1].kind == "exact"


def test_align_leaves_genuinely_different_word_unaligned() -> None:
    # "'Cos" (LLM) vs "because" (ASR) is a real word-choice difference
    # (ratio 0.4, below threshold) — must NOT be force-matched, but the
    # surrounding identical words on either side still align.
    line_words = clt.flatten_line_source([_llm_line("'Cos in His presence")])
    time_words = clt.flatten_time_source(
        [
            _timed_line(
                "because in His presence",
                [
                    ("because", 0, 400),
                    ("in", 400, 600),
                    ("His", 600, 800),
                    ("presence", 800, 1400),
                ],
            )
        ]
    )
    alignments = clt.align_word_streams(line_words, time_words)
    aligned_line_words = {a.line_word_idx for a in alignments}
    # "'Cos" (line word 0) has no counterpart -> unaligned
    assert 0 not in aligned_line_words
    # "in", "His", "presence" (line words 1,2,3) all align
    assert aligned_line_words == {1, 2, 3}


# --- diagnose_replace_block_mismatches -----------------------------------


def test_diagnose_replace_block_mismatches_reports_both_recovered_and_rejected() -> (
    None
):
    line_words = clt.flatten_line_source([_llm_line("'Cos cause freedom")])
    time_words = clt.flatten_time_source(
        [
            _timed_line(
                "because caus freedom",
                [
                    ("because", 0, 400),
                    ("caus", 400, 700),
                    ("freedom", 700, 1200),
                ],
            )
        ]
    )
    mismatches = clt.diagnose_replace_block_mismatches(line_words, time_words)
    # "freedom" is an exact match (equal opcode), not in a replace block.
    texts = {(m.line_text, m.time_text) for m in mismatches}
    assert ("'Cos", "because") in texts
    assert ("cause", "caus") in texts
    by_pair = {(m.line_text, m.time_text): m for m in mismatches}
    assert by_pair[("'Cos", "because")].recovered is False
    assert by_pair[("cause", "caus")].recovered is True


# --- align_word_streams: repeated chorus (monotonicity) -----------------------------------


def test_align_repeated_chorus_stays_monotonic_no_backward_matches() -> None:
    """Three lines with IDENTICAL text ('we will burn') must each align to
    their OWN chronological occurrence in the time source, never all
    collapsing onto the first occurrence."""
    lines = [_llm_line("we will burn") for _ in range(3)]
    line_words = clt.flatten_line_source(lines)

    time_lines = [
        _timed_line(
            "we will burn",
            [("we", 1000, 1200), ("will", 1200, 1400), ("burn", 1400, 1800)],
        ),
        _timed_line(
            "we will burn",
            [("we", 5000, 5200), ("will", 5200, 5400), ("burn", 5400, 5800)],
        ),
        _timed_line(
            "we will burn",
            [("we", 9000, 9200), ("will", 9200, 9400), ("burn", 9400, 9800)],
        ),
    ]
    time_words = clt.flatten_time_source(time_lines)

    alignments = clt.align_word_streams(line_words, time_words)
    assert len(alignments) == 9  # all 9 words align, none skipped
    # line_word_idx 0,1,2 (first "we will burn") must map to time_word_idx
    # 0,1,2 (the FIRST occurrence at 1000ms), not a later repeat.
    by_line_idx = {a.line_word_idx: a.time_word_idx for a in alignments}
    assert by_line_idx[0] == 0 and by_line_idx[1] == 1 and by_line_idx[2] == 2
    assert by_line_idx[3] == 3 and by_line_idx[4] == 4 and by_line_idx[5] == 5
    assert by_line_idx[6] == 6 and by_line_idx[7] == 7 and by_line_idx[8] == 8
    # strictly increasing overall -> monotonic
    time_indices = [a.time_word_idx for a in alignments]
    assert time_indices == sorted(time_indices)


def test_combine_lines_times_repeated_chorus_gives_each_line_its_own_time() -> None:
    lines = [_llm_line("we will burn") for _ in range(3)]
    time_lines = [
        _timed_line(
            "we will burn",
            [("we", 1000, 1200), ("will", 1200, 1400), ("burn", 1400, 1800)],
        ),
        _timed_line(
            "we will burn",
            [("we", 5000, 5200), ("will", 5200, 5400), ("burn", 5400, 5800)],
        ),
        _timed_line(
            "we will burn",
            [("we", 9000, 9200), ("will", 9200, 9400), ("burn", 9400, 9800)],
        ),
    ]
    result = clt.combine_lines_times(lines, time_lines)
    out_lines = result["lines"]
    assert [line["start_ms"] for line in out_lines] == [1000, 5000, 9000]
    assert [line["end_ms"] for line in out_lines] == [1800, 5800, 9800]
    assert all(line["timed"] for line in out_lines)
    assert result["stats"]["word_align_rate"] == 1.0


# --- combine_lines_times: LLM line with no matching audio words -----------------------------------


def test_combine_lines_times_unmatched_line_is_untimed_not_guessed() -> None:
    lines = [
        _llm_line("hello world"),
        _llm_line("completely unrelated gibberish text"),
        _llm_line("hello again"),
    ]
    time_lines = [
        _timed_line("hello world", [("hello", 0, 300), ("world", 300, 700)]),
        _timed_line("hello again", [("hello", 2000, 2300), ("again", 2300, 2700)]),
    ]
    result = clt.combine_lines_times(lines, time_lines)
    out = result["lines"]
    assert out[0]["timed"] is True
    assert out[0]["start_ms"] == 0
    assert out[0]["end_ms"] == 700

    # "completely unrelated gibberish text" has zero words in the time
    # source's vocabulary -> UNTIMED, never guessed/interpolated.
    assert out[1]["timed"] is False
    assert out[1]["start_ms"] is None
    assert out[1]["end_ms"] is None
    assert out[1]["n_words_aligned"] == 0

    assert out[2]["timed"] is True
    assert out[2]["start_ms"] == 2000
    assert out[2]["end_ms"] == 2700

    assert result["stats"]["n_lines_untimed"] == 1
    assert result["stats"]["n_lines_timed"] == 2


def test_combine_lines_times_preserves_llm_text_and_text_sk() -> None:
    lines = [_llm_line("hello world", text_sk="ahoj svet")]
    time_lines = [_timed_line("hello world", [("hello", 0, 300), ("world", 300, 700)])]
    result = clt.combine_lines_times(lines, time_lines)
    out = result["lines"][0]
    assert out["text"] == "hello world"
    assert out["text_sk"] == "ahoj svet"
    # the LLM line's own (bogus) 999999ms timestamps must NEVER survive
    assert out["start_ms"] == 0
    assert out["end_ms"] == 700


# --- monotonicity enforcement -----------------------------------


def test_enforce_monotonic_clamps_out_of_order_start() -> None:
    lines = [
        {"text": "a", "start_ms": 1000, "end_ms": 2000, "timed": True},
        {"text": "b", "start_ms": 1500, "end_ms": 1800, "timed": True},  # overlaps a
        {"text": "c", "start_ms": 3000, "end_ms": 4000, "timed": True},
    ]
    out, violations = clt._enforce_monotonic(lines)
    assert violations == 1
    assert out[0]["start_ms"] == 1000 and out[0]["end_ms"] == 2000
    # clamped up to the floor (2000), and end_ms clamped to be >= start_ms
    assert out[1]["start_ms"] == 2000
    assert out[1]["end_ms"] == 2000
    assert out[2]["start_ms"] == 3000 and out[2]["end_ms"] == 4000


def test_enforce_monotonic_untimed_lines_do_not_move_the_floor() -> None:
    lines = [
        {"text": "a", "start_ms": 1000, "end_ms": 2000, "timed": True},
        {"text": "untimed", "start_ms": None, "end_ms": None, "timed": False},
        {"text": "b", "start_ms": 2500, "end_ms": 3000, "timed": True},
    ]
    out, violations = clt._enforce_monotonic(lines)
    assert violations == 0
    assert out[1]["start_ms"] is None and out[1]["timed"] is False
    assert out[2]["start_ms"] == 2500


def test_combine_lines_times_end_to_end_clamps_monotonic_violation() -> None:
    """A degenerate case where two different LLM lines both align to
    overlapping/out-of-order words (e.g. a text duplication in the LLM
    output) must come out of combine_lines_times() already clamped."""
    lines = [_llm_line("second phrase"), _llm_line("first phrase")]
    # time source: "first phrase" spoken AFTER "second phrase" in reality —
    # deliberately construct out-of-order alignment relative to LLM's line
    # order so the enforcement pass has real work to do.
    time_lines = [
        _timed_line(
            "second phrase first phrase",
            [
                ("second", 5000, 5300),
                ("phrase", 5300, 5600),
                ("first", 1000, 1300),
                ("phrase", 1300, 1600),
            ],
        )
    ]
    result = clt.combine_lines_times(lines, time_lines)
    out = result["lines"]
    # both lines timed (their words did align), but the SECOND line's
    # start (1000/1300) is before the first line's end (5600) -> clamp.
    assert out[0]["timed"] and out[1]["timed"]
    assert out[1]["start_ms"] >= out[0]["end_ms"]
    assert result["stats"]["n_monotonic_violations_clamped"] >= 1


# --- load_backend_output / combine_backend_outputs -----------------------------------


def test_load_backend_output_raises_on_missing_file(tmp_path) -> None:
    import pytest

    with pytest.raises(FileNotFoundError):
        clt.load_backend_output(tmp_path, "no-such-backend", "abc123")


def test_combine_backend_outputs_round_trip(tmp_path) -> None:
    import json

    line_payload = {
        "backend_id": "fake-llm",
        "wav_path": "/tmp/x.wav",
        "duration_ms": 5000,
        "lines": [_llm_line("hello world")],
    }
    time_payload = {
        "backend_id": "fake-asr",
        "duration_ms": 5000,
        "lines": [_timed_line("hello world", [("hello", 0, 300), ("world", 300, 700)])],
    }
    (tmp_path / "fake-llm_vid1.json").write_text(json.dumps(line_payload))
    (tmp_path / "fake-asr_vid1.json").write_text(json.dumps(time_payload))

    result = clt.combine_backend_outputs(tmp_path, "fake-llm", "fake-asr", "vid1")
    assert result["backend_id"] == "combo:fake-llm+fake-asr"
    assert result["line_backend"] == "fake-llm"
    assert result["time_backend"] == "fake-asr"
    assert result["lines"][0]["start_ms"] == 0
    assert result["lines"][0]["end_ms"] == 700
