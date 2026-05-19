#!/usr/bin/env python3
"""build_manifest.py — query production SongPlayer HTTP API and write manifest.json.

One-shot helper. Lists songs whose `source` is one of {lrclib, spotify,
yt_subs} (the three line-synced gold-truth sources we trust for eval —
see crates/sp-server/src/lyrics/worker.rs:680 for the production label
set), fetches each one's detail to pull `lyrics_json.lines`, prompts the
user to bucket them into the 6 manifest categories, and writes
`eval/lyrics/manifest.json`.

Usage:
    python eval/lyrics/build_manifest.py \\
        --songplayer-url http://10.77.9.201:8920 \\
        --out eval/lyrics/manifest.json
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

import requests

# Match the production `track.source` strings for line-synced gold pool.
# See crates/sp-server/src/lyrics/worker.rs:680 — these are checked with
# exact equality (compound labels like `lrclib+timed-merge+whisperx-large-v3@rev1`
# DO NOT qualify as gold; they include whisperx as an alignment step).
ALLOWED_GOLD_SOURCES = {"lrclib", "spotify", "yt_subs"}

CATEGORIES = [
    "dense_vocal",
    "reverb_heavy",
    "instrumental_breaks",
    "multi_language",
    "clean_pop",
    "chant_repetition",
]
TARGET_PER_CATEGORY = 5


def fetch_song_list(base_url: str) -> list[dict[str, Any]]:
    """GET <base_url>/api/v1/lyrics/songs and return the bare JSON array."""
    r = requests.get(f"{base_url}/api/v1/lyrics/songs", timeout=30)
    r.raise_for_status()
    data = r.json()
    if not isinstance(data, list):
        raise RuntimeError(
            f"unexpected response shape from /api/v1/lyrics/songs: {type(data).__name__}"
        )
    return data


def fetch_song_detail(base_url: str, video_id: int) -> dict[str, Any]:
    """GET <base_url>/api/v1/lyrics/songs/{video_id} and return SongDetail."""
    r = requests.get(f"{base_url}/api/v1/lyrics/songs/{video_id}", timeout=30)
    r.raise_for_status()
    return r.json()


def filter_by_gold_source(songs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Keep only songs whose `source` is in ALLOWED_GOLD_SOURCES AND has_lyrics is True."""
    return [
        s
        for s in songs
        if s.get("source") in ALLOWED_GOLD_SOURCES and s.get("has_lyrics") is True
    ]


def to_manifest_entry(
    list_item: dict[str, Any],
    *,
    lyrics_json: dict[str, Any],
    category: str,
    notes: str = "",
) -> dict[str, Any]:
    """Build one manifest fixture from the (list_item, lyrics_json) pair.

    list_item comes from /api/v1/lyrics/songs (must have youtube_id + source).
    lyrics_json comes from the song-detail endpoint's lyrics_json field.
    """
    raw_lines: list[dict[str, Any]] = lyrics_json.get("lines") or []
    gold_lines: list[dict[str, Any]] = []
    for ln in raw_lines:
        text = (ln.get("en") or "").strip()
        if not text:
            # Skip empty lines so the manifest passes minLength: 1 + the
            # monotonic-timing invariant test.
            continue
        gold_lines.append(
            {
                "text": text,
                "start_ms": int(ln["start_ms"]),
                "end_ms": int(ln["end_ms"]),
            }
        )

    entry: dict[str, Any] = {
        "video_id": list_item["youtube_id"],
        "category": category,
        "gold_source": list_item["source"],
        "gold_lines": gold_lines,
    }
    if notes:
        entry["notes"] = notes
    return entry


def write_manifest(fixtures: list[dict[str, Any]], out_path: Path) -> None:
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(
        json.dumps({"version": 1, "fixtures": fixtures}, indent=2, ensure_ascii=False)
        + "\n",
        encoding="utf-8",
    )


def _read_indices(prompt: str, upper_bound: int) -> list[int]:
    """Read space-separated indices from stdin and validate against bound.

    Re-prompts on malformed or out-of-range input rather than crashing.
    Empty input is allowed and returns []. Duplicates within a single line
    are de-duplicated.
    """
    while True:
        raw = input(prompt)
        try:
            picks = [int(s) for s in raw.split()]
        except ValueError:
            print("  invalid: enter space-separated integers", file=sys.stderr)
            continue
        if any(i < 0 or i >= upper_bound for i in picks):
            print(
                f"  invalid: indices must be in [0, {upper_bound - 1}]",
                file=sys.stderr,
            )
            continue
        # de-dup preserving order
        seen: set[int] = set()
        unique = [i for i in picks if not (i in seen or seen.add(i))]
        return unique


def interactive_bucket(
    base_url: str, eligible: list[dict[str, Any]]
) -> list[dict[str, Any]]:
    """Prompt the user to bucket eligible songs across the 6 categories.

    Per pick: fetch song detail to obtain lyrics_json, then construct the
    manifest entry. Already-picked video_ids are filtered out of subsequent
    category prompts so the same song cannot be assigned to two categories.
    """
    print(f"\nEligible songs: {len(eligible)}", file=sys.stderr)
    fixtures: list[dict[str, Any]] = []
    used_youtube_ids: set[str] = set()

    for category in CATEGORIES:
        remaining = [s for s in eligible if s["youtube_id"] not in used_youtube_ids]
        if not remaining:
            print(
                f"\nCategory: {category} — no remaining eligible songs", file=sys.stderr
            )
            continue
        print(
            f"\nCategory: {category} (target {TARGET_PER_CATEGORY})",
            file=sys.stderr,
        )
        for i, s in enumerate(remaining):
            print(
                f"  [{i:3}] {s['youtube_id']}  ({s.get('source')})  "
                f"{s.get('title', '<no title>')}",
                file=sys.stderr,
            )
        idxs = _read_indices(
            f"  Pick up to {TARGET_PER_CATEGORY} indices for {category} (space-separated; blank to skip): ",
            upper_bound=len(remaining),
        )
        for i in idxs[:TARGET_PER_CATEGORY]:
            song = remaining[i]
            notes = input(f"  Notes for {song['youtube_id']} (or blank): ")
            detail = fetch_song_detail(base_url, song["video_id"])
            lyrics_json = detail.get("lyrics_json") or {}
            if not lyrics_json.get("lines"):
                print(
                    f"  WARN: {song['youtube_id']} has no lyrics_json.lines; skipping",
                    file=sys.stderr,
                )
                continue
            entry = to_manifest_entry(
                song,
                lyrics_json=lyrics_json,
                category=category,
                notes=notes,
            )
            if not entry["gold_lines"]:
                print(
                    f"  WARN: {song['youtube_id']} produced no non-empty gold_lines; skipping",
                    file=sys.stderr,
                )
                continue
            fixtures.append(entry)
            used_youtube_ids.add(song["youtube_id"])
    return fixtures


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--songplayer-url",
        default="http://10.77.9.201:8920",
        help="Base URL of the production SongPlayer HTTP API.",
    )
    p.add_argument(
        "--out",
        type=Path,
        default=Path("eval/lyrics/manifest.json"),
        help="Where to write the manifest JSON.",
    )
    args = p.parse_args(argv)

    songs = fetch_song_list(args.songplayer_url)
    eligible = filter_by_gold_source(songs)
    if not eligible:
        print(
            "No songs with allowed source (lrclib/spotify/yt_subs) found. Cannot build manifest.",
            file=sys.stderr,
        )
        return 1

    fixtures = interactive_bucket(args.songplayer_url, eligible)
    if not fixtures:
        print("No fixtures were picked; not writing manifest.", file=sys.stderr)
        return 1

    write_manifest(fixtures, args.out)
    print(f"Wrote {len(fixtures)} fixtures to {args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
