#!/usr/bin/env python3
"""
measure_lyrics_quality.py -- extract per-song quality metrics from a lyrics cache.

Walks the given cache dir, reads every *_lyrics.json + *_alignment_audit.json pair,
and emits a JSON file: {"songs":[{video_id, source, pipeline_version, avg_confidence,
duplicate_start_pct, provider_count}], "aggregate":{...}}.

Usage:
    python measure_lyrics_quality.py --cache-dir <path> --out baseline_before.json
"""
import argparse
import json
import re
import sys
from pathlib import Path
from typing import Optional


def iter_song_pairs(cache_dir: Path):
    lyrics_re = re.compile(r"^(.+)_lyrics\.json$")
    for f in sorted(cache_dir.iterdir()):
        m = lyrics_re.match(f.name)
        if not m:
            continue
        video_id = m.group(1)
        audit = cache_dir / f"{video_id}_alignment_audit.json"
        yield video_id, f, audit if audit.exists() else None


def extract(lyrics_path: Path, audit_path: Optional[Path]) -> Optional[dict]:
    try:
        lyrics = json.loads(lyrics_path.read_text(encoding="utf-8-sig"))
    except Exception:
        return None
    source = lyrics.get("source", "unknown")
    pipeline_version = lyrics.get("pipeline_version") or lyrics.get("version") or 0

    avg_confidence = None
    duplicate_start_pct = None
    provider_count = 0
    if audit_path:
        try:
            audit = json.loads(audit_path.read_text(encoding="utf-8-sig"))
            qm = audit.get("quality_metrics", {})
            avg_confidence = qm.get("avg_confidence")
            duplicate_start_pct = qm.get("duplicate_start_pct")
            provider_count = len(audit.get("providers_run", []))
        except Exception:
            pass
    return {
        "video_id": lyrics_path.stem.replace("_lyrics", ""),
        "source": source,
        "pipeline_version": pipeline_version,
        "avg_confidence": avg_confidence,
        "duplicate_start_pct": duplicate_start_pct,
        "provider_count": provider_count,
    }


def aggregate(songs: list) -> dict:
    def mean_of(key):
        vs = [s[key] for s in songs if s.get(key) is not None]
        return sum(vs) / len(vs) if vs else None

    multi_provider = [s for s in songs if s.get("provider_count", 0) >= 2]
    return {
        "song_count": len(songs),
        "avg_confidence_mean": mean_of("avg_confidence"),
        "duplicate_start_pct_mean": mean_of("duplicate_start_pct"),
        "multi_provider_count": len(multi_provider),
        "multi_provider_pct": (100.0 * len(multi_provider) / len(songs)) if songs else 0.0,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache-dir", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    if not args.cache_dir.is_dir():
        print(f"cache dir not found: {args.cache_dir}", file=sys.stderr)
        return 2
    songs = []
    for video_id, lyrics_path, audit_path in iter_song_pairs(args.cache_dir):
        entry = extract(lyrics_path, audit_path)
        if entry:
            songs.append(entry)
    out = {"songs": songs, "aggregate": aggregate(songs)}
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(out, indent=2), encoding="utf-8")
    print(f"wrote {args.out} ({len(songs)} songs)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
