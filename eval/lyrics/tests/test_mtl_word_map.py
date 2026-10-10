"""#144 F3: the mtl runner's word list must equal upstream's, word for word.

``run.py::build_line_word_map`` replicates the word list that
``LyricsAlignment-MTL/wrapper.py::preprocess_lyrics`` derives, and
``align_fixture`` refuses to align when the two differ ("word-filtering
mismatch"). On SNV that refusal kept rows 286/295 (``WL1ivzWbQGI``, ours 224
words, upstream 220) and 57 (``h-A1Tzkjsi4``) out of the ★ tier.

Upstream (read from the box's ``wrapper.py``, below verbatim in
``upstream_words``) keeps only ``a-z``, the apostrophe, the ASCII space and
``~`` of each lowercased line, then splits. A non-breaking or thin space, or a
tab, is DROPPED, so the words on either side of it merge; the replication
split on every Unicode whitespace instead (more words), and dropped ``~``.
"""

from __future__ import annotations

import importlib.util
from pathlib import Path
from typing import Any

RUN_PY = (
    Path(__file__).resolve().parents[1] / "aligners" / "lyrics_alignment_mtl" / "run.py"
)


def _load_run_py() -> Any:
    spec = importlib.util.spec_from_file_location("mtl_run_word_map", RUN_PY)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def upstream_words(lines: list[str]) -> list[str]:
    """``wrapper.preprocess_lyrics`` with no word file, on the temp file
    ``align_fixture`` writes (one line per text, ``\\n``-terminated)."""
    from string import ascii_lowercase

    d = {ascii_lowercase[i]: i for i in range(26)}
    d["'"] = 26
    d[" "] = 27
    d["~"] = 28
    raw = "".join((text or "") + "\n" for text in lines)
    raw_lines = raw.splitlines()
    raw_lines = [
        "".join([c for c in line.lower() if c in d]).strip() for line in raw_lines
    ]
    raw_lines = [" ".join(line.split()) for line in raw_lines if len(line) > 0]
    full_lyrics = " ".join(raw_lines)
    return full_lyrics.split()


CASES = [
    ["Holy is the Lord", "We lift Your name"],
    ["I believe in the Gospel", "Oh oh oh"],
    ["Here\tI am, Lord", "Send me"],
    ["Oh~ oh~", "~", "20 years"],
    ["", "   ", "Don't stop", "na-na-na"],
]


def test_the_runner_lists_exactly_upstreams_words() -> None:
    run = _load_run_py()
    for lines in CASES:
        words, origin, counts = run.build_line_word_map(lines)
        assert words == upstream_words(lines), lines
        assert len(origin) == len(words), lines
        assert sum(counts) == len(words), lines
        assert [o[0] for o in origin] == sorted(o[0] for o in origin), lines


def test_a_non_ascii_space_merges_the_words_around_it_as_upstream_does() -> None:
    run = _load_run_py()
    words, origin, counts = run.build_line_word_map(["I believe in it"])
    assert words == ["i", "believein", "it"]
    assert origin == [(0, "I"), (0, "believe in"), (0, "it")]
    assert counts == [3]


def test_a_line_break_inside_a_line_splits_its_words_as_upstream_does() -> None:
    """Upstream reads the lyric file with ``str.splitlines()``, so a line
    separator INSIDE a text (``\\u2028``, a vertical tab) ends a line there:
    the words around it stay two words."""
    run = _load_run_py()
    lines = ["foo bar baz", "one\x0btwo", "x\x85y"]
    words, origin, counts = run.build_line_word_map(lines)
    assert (
        words == upstream_words(lines) == ["foo", "bar", "baz", "one", "two", "x", "y"]
    )
    assert counts == [3, 2, 2]
    assert origin[0] == (0, "foo")
