#!/usr/bin/env python3
"""build_manifest.py — query production SongPlayer HTTP API and write manifest.json.

One-shot helper. Lists songs whose `lyrics_source` is one of {lrclib_synced,
spotify_proxy, yt_subs_manual}, prompts the user to bucket them into the 6
manifest categories, and writes `eval/lyrics/manifest.json`.

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

ALLOWED_GOLD_SOURCES = {"lrclib_synced", "spotify_proxy", "yt_subs_manual"}
CATEGORIES = [
    "dense_vocal",
    "reverb_heavy",
    "instrumental_breaks",
    "multi_language",
    "clean_pop",
    "chant_repetition",
]
TARGET_PER_CATEGORY = 5


def fetch_songs(base_url: str) -> list[dict[str, Any]]:
    """GET <base_url>/api/v1/songs and return the song list."""
    r = requests.get(f"{base_url}/api/v1/songs", timeout=30)
    r.raise_for_status()
    return r.json().get("songs", [])


def filter_by_gold_source(songs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [s for s in songs if s.get("lyrics_source") in ALLOWED_GOLD_SOURCES]


def to_manifest_entry(
    song: dict[str, Any], *, category: str, notes: str = ""
) -> dict[str, Any]:
    entry: dict[str, Any] = {
        "video_id": song["youtube_id"],
        "category": category,
        "gold_source": song["lyrics_source"],
        "gold_lines": [
            {
                "text": line["text"],
                "start_ms": int(line["start_ms"]),
                "end_ms": int(line["end_ms"]),
            }
            for line in song["lines"]
        ],
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


def interactive_bucket(eligible: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Prompt the user to assign each picked song to a category."""
    print(f"\nEligible songs: {len(eligible)}", file=sys.stderr)
    fixtures: list[dict[str, Any]] = []
    for category in CATEGORIES:
        print(
            f"\nCategory: {category} (target {TARGET_PER_CATEGORY})",
            file=sys.stderr,
        )
        for i, s in enumerate(eligible):
            print(
                f"  [{i:3}] {s['youtube_id']}  ({s.get('lyrics_source')})  "
                f"{s.get('title', '<no title>')}",
                file=sys.stderr,
            )
        idx_line = input(
            f"  Pick {TARGET_PER_CATEGORY} indices for {category} (space-separated): "
        )
        for idx_str in idx_line.split():
            song = eligible[int(idx_str)]
            notes = input(f"  Notes for {song['youtube_id']} (or blank): ")
            fixtures.append(to_manifest_entry(song, category=category, notes=notes))
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

    songs = fetch_songs(args.songplayer_url)
    eligible = filter_by_gold_source(songs)
    if not eligible:
        print(
            "No songs with allowed gold_source found. Cannot build manifest.",
            file=sys.stderr,
        )
        return 1

    fixtures = interactive_bucket(eligible)
    write_manifest(fixtures, args.out)
    print(f"Wrote {len(fixtures)} fixtures to {args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
