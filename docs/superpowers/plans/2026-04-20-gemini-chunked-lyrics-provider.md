# Gemini Chunked Lyrics Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace qwen3 forced alignment with Gemini 3 Pro transcription (via CLIProxy Google OAuth, free tier) over Demucs-dereverbed vocals, chunked 60 s / 10 s overlap for precision. Line-level timings only.

**Architecture:** Two phases. Phase 0 is a throwaway Python prototype in `scripts/experiments/` that we iterate on live data. Only after it's validated on 5 varied songs does Phase 1 port the approach to Rust as a new `AlignmentProvider` behind a feature flag, bumping pipeline version 10→11.

**Tech Stack:** Python 3.12 (phase 0), Rust 2024 + tokio + reqwest + async-trait + wiremock (phase 1), Gemini 3 Pro via CLIProxy OpenAI-compatible endpoint on `http://127.0.0.1:18787`, existing Demucs preprocessing.

**Spec:** `docs/superpowers/specs/2026-04-20-gemini-chunked-lyrics-provider-design.md`

---

## Phase 0 — Python prototype (throwaway)

Phase 0 is **experimental**. No unit tests, no mutation tests, no CI. Iterate until output on 5 real songs matches the spec success criteria. The script is committed so the approach is reproducible, but it is not a product surface.

### Task 0.1: Script skeleton with argparse + path resolution

**Files:**
- Create: `scripts/experiments/gemini_lyrics.py`

- [ ] **Step 1: Create the script file with argparse, path resolution, and a stubbed `main` that prints the plan**

```python
#!/usr/bin/env python3
"""Phase 0 prototype: generate _lyrics.json for a song via Gemini chunked transcription.

This script is experimental and throwaway. It reuses the existing Demucs-dereverbed
vocal WAV (or generates one via lyrics_worker.py preprocess-vocals) and calls Gemini 3
Pro via CLIProxyAPI's OpenAI-compatible endpoint to transcribe 60s chunks with 10s
overlap, then merges and writes a LyricsTrack-shaped JSON into the cache dir.
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

CHUNK_DURATION_S = 60
CHUNK_OVERLAP_S = 10
CHUNK_STRIDE_S = CHUNK_DURATION_S - CHUNK_OVERLAP_S  # 50

DEFAULT_CACHE = Path(r"C:\ProgramData\SongPlayer\cache")
DEFAULT_TOOLS = DEFAULT_CACHE / "tools"
DEFAULT_PROXY = "http://127.0.0.1:18787"
DEFAULT_MODEL = "gemini-3-pro-preview"


def resolve_paths(cache_dir: Path, youtube_id: str) -> dict:
    """Locate the song's normalized audio + vocal WAV + description lyrics."""
    audio_glob = list(cache_dir.glob(f"*_{youtube_id}_normalized_audio.flac"))
    if not audio_glob:
        raise FileNotFoundError(f"no normalized audio for {youtube_id} in {cache_dir}")
    audio = audio_glob[0]
    vocal = cache_dir / f"{youtube_id}_vocals_dereverbed.wav"
    description = cache_dir / f"{youtube_id}_description_lyrics.json"
    return {"audio": audio, "vocal": vocal, "description": description}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--video-id", required=True, help="YouTube video id")
    ap.add_argument("--cache-dir", default=str(DEFAULT_CACHE))
    ap.add_argument("--proxy-url", default=DEFAULT_PROXY)
    ap.add_argument("--model", default=DEFAULT_MODEL)
    ap.add_argument("--dry-run", action="store_true", help="skip API calls, print plan")
    args = ap.parse_args()

    cache = Path(args.cache_dir)
    paths = resolve_paths(cache, args.video_id)
    print(f"[gemini_lyrics] video_id={args.video_id}")
    print(f"  audio:       {paths['audio']}")
    print(f"  vocal:       {paths['vocal']}  (exists={paths['vocal'].exists()})")
    print(f"  description: {paths['description']}  (exists={paths['description'].exists()})")
    print(f"  proxy:       {args.proxy_url}")
    print(f"  model:       {args.model}")
    if args.dry_run:
        print("[dry-run] exiting before any API calls")
        return


if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Dry-run against song 230 to confirm paths resolve**

Run on win-resolume:
```powershell
& 'C:\ProgramData\SongPlayer\cache\tools\lyrics_venv\Scripts\python.exe' `
  'C:\devel\songplayer\scripts\experiments\gemini_lyrics.py' `
  --video-id Avi4sMPQqzI --dry-run
```
Expected: all three paths print, `vocal exists=True` (we already made it today), `description exists=True`, exit clean.

- [ ] **Step 3: Commit**

```bash
git add scripts/experiments/gemini_lyrics.py
git commit -m "experiment: gemini_lyrics.py skeleton with path resolution"
```

---

### Task 0.2: Audio chunking via ffmpeg subprocess

**Files:**
- Modify: `scripts/experiments/gemini_lyrics.py`

- [ ] **Step 1: Add `probe_duration_ms` and `chunk_audio` functions**

Append to the script (after `resolve_paths`):

```python
def probe_duration_ms(audio: Path, ffprobe: Path) -> int:
    out = subprocess.check_output(
        [str(ffprobe), "-v", "error", "-show_entries", "format=duration",
         "-of", "default=nokey=1:noprint_wrappers=1", str(audio)],
        text=True,
    ).strip()
    return int(float(out) * 1000)


def chunk_audio(vocal: Path, duration_ms: int, out_dir: Path, ffmpeg: Path) -> list[dict]:
    """Split vocal WAV into 60s chunks with 10s overlap. Returns list of chunk specs."""
    out_dir.mkdir(parents=True, exist_ok=True)
    chunks = []
    idx = 0
    start_ms = 0
    while start_ms < duration_ms:
        end_ms = min(start_ms + CHUNK_DURATION_S * 1000, duration_ms)
        name = f"chunk{idx:02d}_{start_ms}_{end_ms}.wav"
        path = out_dir / name
        subprocess.check_call([
            str(ffmpeg), "-y", "-loglevel", "error",
            "-ss", str(start_ms / 1000.0),
            "-t", str((end_ms - start_ms) / 1000.0),
            "-i", str(vocal),
            "-c:a", "pcm_s16le",
            str(path),
        ])
        chunks.append({"idx": idx, "start_ms": start_ms, "end_ms": end_ms, "path": path})
        idx += 1
        if end_ms >= duration_ms:
            break
        start_ms += CHUNK_STRIDE_S * 1000
    return chunks
```

Update `main()` to call it after path resolution:

```python
    ffmpeg = DEFAULT_TOOLS / "ffmpeg.exe"
    ffprobe = DEFAULT_TOOLS / "ffprobe.exe"
    duration_ms = probe_duration_ms(paths["audio"], ffprobe)
    print(f"  duration:    {duration_ms} ms ({duration_ms/1000:.1f} s)")

    chunk_dir = cache / "gemini_chunks" / args.video_id
    chunks = chunk_audio(paths["vocal"], duration_ms, chunk_dir, ffmpeg)
    print(f"[chunk] produced {len(chunks)} chunks:")
    for c in chunks:
        print(f"  {c['idx']:>2}: {c['start_ms']/1000:>6.1f}s..{c['end_ms']/1000:>6.1f}s  {c['path'].name}")

    if args.dry_run:
        print("[dry-run] exiting before any API calls")
        return
```

- [ ] **Step 2: Dry-run on song 230**

Expected: 13 chunks, indices 00–12, first chunk 0.0s..60.0s, last chunk 600.0s..659.98s (approx).

- [ ] **Step 3: Commit**

```bash
git add scripts/experiments/gemini_lyrics.py
git commit -m "experiment: chunk audio into 60s/10s-overlap segments via ffmpeg"
```

---

### Task 0.3: CLIProxy Gemini call for a single chunk (eyeball test)

**Files:**
- Modify: `scripts/experiments/gemini_lyrics.py`

CLIProxy exposes Gemini via both `/v1/chat/completions` (OpenAI-compat) and `/v1beta/models/{model}:generateContent` (Gemini native). Use the Gemini native endpoint so we can pass inline audio the same way Gemini 3 Pro expects it.

- [ ] **Step 1: Add `load_description_reference`, `build_prompt`, `call_gemini` functions**

Append to the script:

```python
import base64
import urllib.request

def load_description_reference(description_path: Path) -> str:
    """Load clean description lyrics as a newline-joined reference block."""
    if not description_path.exists():
        return "(no description lyrics available for this song)"
    data = json.loads(description_path.read_text(encoding="utf-8"))
    lines = data.get("lines") or []
    return "\n".join(lines) if lines else "(description lyrics file empty)"


PROMPT_TEMPLATE = """You are a precise sung-lyrics transcription assistant. Your only output format is timed lines in this exact schema, one per line, nothing else:
(MM:SS.x --> MM:SS.x) text

Transcribe the sung vocals in the attached audio.

Rules:

1. Timestamps are LOCAL to this audio chunk, starting at 00:00. Do NOT offset.

2. COVERAGE — Output a timed line for EVERY sung phrase. Do NOT skip or collapse repeated choruses or refrains. If a phrase is sung 5 times, output 5 separate lines. Do not summarize.

3. SHORT LINES — Break long phrases into short, separately timed lines.
   - Break at every comma, semicolon, or breath pause.
   - Example: "To know Your heart, oh it's the goal of my life, it's the aim of my life" MUST be 3 separate lines:
     (07:23.0 --> 07:25.5) To know Your heart
     (07:26.0 --> 07:30.0) Oh it's the goal of my life
     (07:31.0 --> 07:34.0) It's the aim of my life
   - Aim for <= 8 words per output line where the phrasing allows.

4. PRECISION — Line start_time = the exact moment the first syllable BEGINS being sung (not the breath before, not a preceding beat). Line end_time = the last syllable finishes, before the next silence.

5. SILENCE — If the chunk has no vocals (instrumental only, or pre-roll silence), output exactly: # no vocals

6. OUTPUT FORMAT — Output ONLY timed lines. No intro text, no commentary, no markdown fences, no summary at the end.

7. DO NOT HALLUCINATE — Only transcribe what you actually hear. If you hear a word not matching the reference lyrics below, still write what you hear. If the reference has a line that doesn't appear in this audio chunk, do NOT include it.

Reference lyrics for this song (extracted from YouTube description — may be out of order, missing chorus repeats, or contain extra phrases not in this chunk):
{reference}

This chunk covers audio from {chunk_start_s:.1f}s to {chunk_end_s:.1f}s of the full song ({full_duration_s:.1f}s total). The chunk may start or end mid-phrase.
"""


def build_prompt(reference: str, chunk_start_ms: int, chunk_end_ms: int, full_duration_ms: int) -> str:
    return PROMPT_TEMPLATE.format(
        reference=reference,
        chunk_start_s=chunk_start_ms / 1000.0,
        chunk_end_s=chunk_end_ms / 1000.0,
        full_duration_s=full_duration_ms / 1000.0,
    )


def call_gemini(proxy_url: str, model: str, prompt: str, audio_path: Path, timeout_s: int = 60) -> str:
    """Call Gemini via CLIProxy /v1beta/models/{model}:generateContent. Returns raw text body."""
    audio_b64 = base64.b64encode(audio_path.read_bytes()).decode("ascii")
    body = {
        "contents": [{
            "parts": [
                {"text": prompt},
                {"inline_data": {"mime_type": "audio/wav", "data": audio_b64}},
            ]
        }],
        "generationConfig": {"temperature": 0.0},
    }
    url = f"{proxy_url}/v1beta/models/{model}:generateContent"
    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=timeout_s) as resp:
        raw = resp.read().decode("utf-8")
    doc = json.loads(raw)
    # Gemini returns: {"candidates":[{"content":{"parts":[{"text":"..."}]}}]}
    try:
        return doc["candidates"][0]["content"]["parts"][0]["text"]
    except (KeyError, IndexError) as e:
        raise RuntimeError(f"unexpected Gemini response shape: {raw[:500]}") from e
```

Add a `--one-chunk-test` flag to `main()` that calls Gemini on a single chunk and prints the raw response:

```python
    ap.add_argument("--one-chunk-test", type=int, default=None,
                    help="call Gemini on only this chunk index and print raw output")
    # ...after chunks is built, before args.dry_run check:
    if args.one_chunk_test is not None:
        reference = load_description_reference(paths["description"])
        c = chunks[args.one_chunk_test]
        prompt = build_prompt(reference, c["start_ms"], c["end_ms"], duration_ms)
        print(f"[one-chunk-test] chunk {c['idx']} prompt length={len(prompt)} chars")
        out = call_gemini(args.proxy_url, args.model, prompt, c["path"])
        print(f"\n=== RAW GEMINI RESPONSE (chunk {c['idx']}) ===")
        print(out)
        print("=== END ===")
        return
```

- [ ] **Step 2: Run one-chunk test**

Pre-req: CLIProxy must be logged in to Google (user runs `CLIProxyAPI.exe -login` once). Verify with:
```powershell
curl.exe -s http://127.0.0.1:18787/v1beta/models | Select-String -Pattern gemini
```
Expected: lists at least one `gemini-*` model.

Then:
```powershell
& 'C:\ProgramData\SongPlayer\cache\tools\lyrics_venv\Scripts\python.exe' `
  'C:\devel\songplayer\scripts\experiments\gemini_lyrics.py' `
  --video-id Avi4sMPQqzI --one-chunk-test 0
```
Expected: prints a block of `(MM:SS.x --> MM:SS.x) text` lines covering 0–60 s of the song.

- [ ] **Step 3: Commit**

```bash
git add scripts/experiments/gemini_lyrics.py
git commit -m "experiment: call Gemini via CLIProxy for single chunk"
```

---

### Task 0.4: Parse Gemini response into line tuples

**Files:**
- Modify: `scripts/experiments/gemini_lyrics.py`

- [ ] **Step 1: Add `parse_timed_lines` function**

```python
import re

TIMED_LINE_RE = re.compile(
    r"\((\d{1,2}):(\d{1,2}(?:\.\d+)?)\s*-->\s*(\d{1,2}):(\d{1,2}(?:\.\d+)?)\)\s*(.+)"
)


def parse_timed_lines(raw: str) -> list[dict]:
    """Parse (MM:SS.x --> MM:SS.x) text format. Returns list of {start_ms,end_ms,text}."""
    out = []
    for line in raw.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        m = TIMED_LINE_RE.match(line)
        if not m:
            continue
        s_min, s_sec, e_min, e_sec, text = m.groups()
        start_ms = int((int(s_min) * 60 + float(s_sec)) * 1000)
        end_ms = int((int(e_min) * 60 + float(e_sec)) * 1000)
        out.append({"start_ms": start_ms, "end_ms": end_ms, "text": text.strip()})
    return out
```

- [ ] **Step 2: Smoke-test the parser by piping the one-chunk-test output through it**

Extend the `--one-chunk-test` branch:

```python
        parsed = parse_timed_lines(out)
        print(f"\n=== PARSED ({len(parsed)} lines) ===")
        for p in parsed:
            print(f"  {p['start_ms']/1000:>6.2f}..{p['end_ms']/1000:>6.2f}s  '{p['text']}'")
```

Re-run the one-chunk test and confirm the parser yields non-zero lines from the raw output.

- [ ] **Step 3: Commit**

```bash
git add scripts/experiments/gemini_lyrics.py
git commit -m "experiment: parse Gemini timed-line response format"
```

---

### Task 0.5: Full song processing — all chunks + overlap merge + write `_lyrics.json`

**Files:**
- Modify: `scripts/experiments/gemini_lyrics.py`

- [ ] **Step 1: Add `process_all_chunks`, `merge_overlap`, and `write_lyrics_json`**

```python
def normalize_text(s: str) -> str:
    """Normalize line text for dedup: lowercase, strip punct, collapse whitespace."""
    s = s.lower()
    s = re.sub(r"[^\w\s']", " ", s)
    s = re.sub(r"\s+", " ", s).strip()
    return s


def process_all_chunks(chunks: list[dict], reference: str, full_duration_ms: int,
                       proxy_url: str, model: str) -> list[list[dict]]:
    """Call Gemini per chunk, return list-of-lists of entries (local timings still)."""
    results = []
    for c in chunks:
        prompt = build_prompt(reference, c["start_ms"], c["end_ms"], full_duration_ms)
        print(f"[chunk {c['idx']:>2}] calling Gemini...")
        try:
            raw = call_gemini(proxy_url, model, prompt, c["path"])
        except Exception as e:
            print(f"[chunk {c['idx']:>2}] FAILED: {e}")
            results.append([])
            continue
        parsed = parse_timed_lines(raw)
        print(f"[chunk {c['idx']:>2}] {len(parsed)} lines parsed")
        results.append(parsed)
    return results


def merge_overlap(chunks: list[dict], per_chunk_lines: list[list[dict]]) -> list[dict]:
    """Merge overlapping chunk outputs into a single ordered line list (global ms).

    Algorithm per Appendix B of the spec:
    1. Shift each chunk's local timestamps to global by adding chunk.start_ms.
    2. Walk chunk pairs (N, N+1). In their overlap region (last 10s of N, first 10s of N+1):
       for each pair of entries with normalized text match AND start_ms within 1500ms,
       keep the one whose start is further from the chunk boundary.
    """
    # Shift each chunk's lines to global time
    globals_per_chunk = []
    for c, lines in zip(chunks, per_chunk_lines):
        shifted = [{"start_ms": l["start_ms"] + c["start_ms"],
                    "end_ms": l["end_ms"] + c["start_ms"],
                    "text": l["text"]} for l in lines]
        globals_per_chunk.append(shifted)

    # Walk adjacent chunk pairs to mark dupes in overlap regions
    merged = [list(x) for x in globals_per_chunk]  # mutable copy
    for i in range(len(chunks) - 1):
        overlap_start_ms = chunks[i + 1]["start_ms"]
        overlap_end_ms = chunks[i]["end_ms"]
        if overlap_end_ms <= overlap_start_ms:
            continue
        a_tail = [l for l in merged[i] if l["end_ms"] > overlap_start_ms]
        b_head = [l for l in merged[i + 1] if l["start_ms"] < overlap_end_ms]
        drop_from_a = set()
        drop_from_b = set()
        for ia, la in enumerate(a_tail):
            for ib, lb in enumerate(b_head):
                if ib in drop_from_b or ia in drop_from_a:
                    continue
                if normalize_text(la["text"]) != normalize_text(lb["text"]):
                    continue
                if abs(la["start_ms"] - lb["start_ms"]) > 1500:
                    continue
                # duplicate: keep the one further from the boundary
                a_dist = abs(la["start_ms"] - overlap_end_ms)
                b_dist = abs(lb["start_ms"] - overlap_start_ms)
                if a_dist >= b_dist:
                    drop_from_b.add(ib)
                else:
                    drop_from_a.add(ia)
        merged[i] = [l for j, l in enumerate(merged[i]) if l not in [a_tail[ja] for ja in drop_from_a]]
        merged[i + 1] = [l for j, l in enumerate(merged[i + 1]) if l not in [b_head[jb] for jb in drop_from_b]]

    # Flatten, sort by start_ms
    flat = [l for chunk_lines in merged for l in chunk_lines]
    flat.sort(key=lambda l: l["start_ms"])
    return flat


def write_lyrics_json(out_path: Path, lines: list[dict], full_duration_ms: int) -> None:
    """Write LyricsTrack schema to disk."""
    entries = []
    for i, l in enumerate(lines):
        # Gap-fill: extend end_ms to just before next line's start, so no blank flicker
        next_start = lines[i + 1]["start_ms"] if i + 1 < len(lines) else min(l["end_ms"] + 2000, full_duration_ms)
        end_ms = min(l["end_ms"], next_start - 50) if next_start > l["start_ms"] + 50 else l["end_ms"]
        # Word-level timings: distribute evenly across the line
        words = l["text"].split()
        if words:
            dur = max(200, (end_ms - l["start_ms"]) // len(words))
            t = l["start_ms"]
            word_objs = []
            for k, w in enumerate(words):
                w_end = t + dur if k < len(words) - 1 else end_ms
                word_objs.append({"text": w, "start_ms": t, "end_ms": w_end})
                t = w_end
        else:
            word_objs = []
        entries.append({
            "start_ms": l["start_ms"],
            "end_ms": end_ms,
            "en": l["text"],
            "sk": None,
            "words": word_objs,
        })
    out = {
        "version": 2,
        "source": "gemini-chunked-prototype",
        "language_source": "en",
        "language_translation": "",
        "lines": entries,
    }
    out_path.write_text(json.dumps(out, indent=2), encoding="utf-8")
```

Rewrite `main()` to wire everything together (the `--dry-run` and `--one-chunk-test` branches stay):

```python
    reference = load_description_reference(paths["description"])
    print(f"[reference] {len(reference.splitlines())} description lines loaded")

    per_chunk = process_all_chunks(chunks, reference, duration_ms, args.proxy_url, args.model)

    # Cache raw per-chunk outputs for later re-merging without re-calling Gemini
    chunks_cache_path = cache / f"{args.video_id}_gemini_chunks.json"
    chunks_cache_path.write_text(
        json.dumps({"chunks": [{"start_ms": c["start_ms"], "end_ms": c["end_ms"], "lines": l}
                               for c, l in zip(chunks, per_chunk)]}, indent=2),
        encoding="utf-8",
    )
    print(f"[cache] wrote raw chunks to {chunks_cache_path}")

    merged = merge_overlap(chunks, per_chunk)
    print(f"[merge] {sum(len(x) for x in per_chunk)} raw lines -> {len(merged)} merged")

    out_path = cache / f"{args.video_id}_lyrics.json"
    write_lyrics_json(out_path, merged, duration_ms)
    print(f"[write] {out_path} ({len(merged)} entries, span "
          f"{merged[0]['start_ms']/1000:.1f}s..{merged[-1]['end_ms']/1000:.1f}s)")
```

- [ ] **Step 2: Run full pipeline on song 230**

```powershell
& 'C:\ProgramData\SongPlayer\cache\tools\lyrics_venv\Scripts\python.exe' `
  'C:\devel\songplayer\scripts\experiments\gemini_lyrics.py' `
  --video-id Avi4sMPQqzI
```

Expected console output: 13 chunks each reporting 5–15 parsed lines, merge reducing total by ~30 (the overlaps), output file `Avi4sMPQqzI_lyrics.json` with ~100–150 entries spanning ~17 s to ~640 s of the 660 s song.

Trigger a fresh reload and watch on Resolume:
```powershell
Invoke-WebRequest -Uri 'http://localhost:8920/api/v1/playback/184/skip' -Method Post
Start-Sleep -Seconds 4
Invoke-WebRequest -Uri 'http://localhost:8920/api/v1/playlists/184/play-video' -Method Post -ContentType 'application/json' -Body '{"video_id":230}'
```

- [ ] **Step 3: Commit**

```bash
git add scripts/experiments/gemini_lyrics.py
git commit -m "experiment: full song processing with overlap merge"
```

---

### Task 0.6: Validate on 5 varied songs and iterate prompt

This task is **validation + prompt iteration**, not code changes. No subagent — this needs the user's ear.

- [ ] **Step 1: Pick 5 representative video_ids from the catalog**

Criteria (one song per bucket):
1. Worship ballad (slow, clear vocals, long sustained notes) — e.g. song 230 "Known By You"
2. Fast tempo (many words per second)
3. Song with a > 20 s instrumental bridge
4. Non-English or mixed-language song
5. Short track (< 4 min)

Run `gemini_lyrics.py` for each. Save each song's full per-chunk cache so we can re-merge without re-calling.

- [ ] **Step 2: For each of the 5, listen in Resolume and score against spec success criteria**

Record in a scratch notes file (not committed):
- Song id, title
- First-line offset vs true vocal onset (ms, signed)
- Drift at 50% of song (ms, signed)
- Drift at 90% of song (ms, signed)
- Any phantom lines during instrumental sections (yes/no + count)
- Any missed chorus repeats (yes/no + count)

- [ ] **Step 3: Iterate on `PROMPT_TEMPLATE` if failures cluster on a specific symptom**

Keep each iteration in git history (commit after each prompt change + re-run). Common knobs:
- Add explicit rule if Gemini drops repeated refrains ("Rule: at least one separately-timed line per sung phrase — identical text sung twice = two lines")
- Add explicit silence-gap threshold if lines smear across breaths

- [ ] **Step 4: Commit the final validated prompt + a notes file**

```bash
git add scripts/experiments/gemini_lyrics.py docs/superpowers/specs/2026-04-20-gemini-phase-0-validation-notes.md
git commit -m "experiment: phase 0 validation — prompt tuned on 5 songs"
```

**Exit gate for Phase 0:** all 5 songs pass the spec success criteria (first-word + every line start within 500 ms, no phantoms, no missed repeats, full vocal span). Only then proceed to Phase 1.

---

## Phase 1 — Rust port

Phase 1 is a real code change with TDD, tests, CI, PR. Each task commits on green with `cargo fmt --all --check`.

### Task 1.1: `gemini_prompt.rs` — prompt builder (pure fn, TDD)

**Files:**
- Create: `crates/sp-server/src/lyrics/gemini_prompt.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub mod gemini_prompt;`)

- [ ] **Step 1: Write failing test**

Create `crates/sp-server/src/lyrics/gemini_prompt.rs`:

```rust
//! Gemini chunk prompt builder (pure function, no I/O).

/// Build the Gemini chunked-transcription prompt.
pub fn build_prompt(
    reference_lyrics: &str,
    chunk_start_ms: u64,
    chunk_end_ms: u64,
    full_duration_ms: u64,
) -> String {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_chunk_time_window_in_seconds() {
        let p = build_prompt("Hello world", 17_160, 77_160, 659_980);
        assert!(
            p.contains("from 17.2s to 77.2s"),
            "expected chunk window in seconds, got:\n{p}"
        );
        assert!(
            p.contains("659.9s total") || p.contains("659.98s total") || p.contains("660.0s total"),
            "expected full duration in seconds, got:\n{p}"
        );
    }

    #[test]
    fn prompt_includes_reference_lyrics_verbatim() {
        let p = build_prompt("I could search all this world\nI still find", 0, 60_000, 180_000);
        assert!(p.contains("I could search all this world"));
        assert!(p.contains("I still find"));
    }

    #[test]
    fn prompt_contains_required_rules() {
        let p = build_prompt("ref", 0, 60_000, 180_000);
        assert!(p.contains("Timestamps are LOCAL"));
        assert!(p.contains("COVERAGE"));
        assert!(p.contains("SHORT LINES"));
        assert!(p.contains("PRECISION"));
        assert!(p.contains("# no vocals"));
        assert!(p.contains("DO NOT HALLUCINATE"));
    }
}
```

Register the module in `crates/sp-server/src/lyrics/mod.rs`. Locate the section of `pub mod ...;` declarations (after `pub mod qwen3_provider;` near the top) and add:

```rust
pub mod gemini_prompt;
```

- [ ] **Step 2: Run — expect `unimplemented!()` panic in all three tests**

```bash
cargo test -p sp-server --lib lyrics::gemini_prompt -- --nocapture
```
Expected: 3 tests, 3 failures, all `panicked at ... not yet implemented`.

- [ ] **Step 3: Implement `build_prompt`**

Replace the stub body with the Appendix-A template (same wording as Phase 0's `PROMPT_TEMPLATE`):

```rust
pub fn build_prompt(
    reference_lyrics: &str,
    chunk_start_ms: u64,
    chunk_end_ms: u64,
    full_duration_ms: u64,
) -> String {
    format!(
        "You are a precise sung-lyrics transcription assistant. Your only output format is timed lines in this exact schema, one per line, nothing else:\n\
         (MM:SS.x --> MM:SS.x) text\n\n\
         Transcribe the sung vocals in the attached audio.\n\n\
         Rules:\n\n\
         1. Timestamps are LOCAL to this audio chunk, starting at 00:00. Do NOT offset.\n\n\
         2. COVERAGE — Output a timed line for EVERY sung phrase. Do NOT skip or collapse repeated choruses or refrains. If a phrase is sung 5 times, output 5 separate lines. Do not summarize.\n\n\
         3. SHORT LINES — Break long phrases into short, separately timed lines.\n\
            - Break at every comma, semicolon, or breath pause.\n\
            - Example: \"To know Your heart, oh it's the goal of my life, it's the aim of my life\" MUST be 3 separate lines:\n\
              (07:23.0 --> 07:25.5) To know Your heart\n\
              (07:26.0 --> 07:30.0) Oh it's the goal of my life\n\
              (07:31.0 --> 07:34.0) It's the aim of my life\n\
            - Aim for <= 8 words per output line where the phrasing allows.\n\n\
         4. PRECISION — Line start_time = the exact moment the first syllable BEGINS being sung (not the breath before, not a preceding beat). Line end_time = the last syllable finishes, before the next silence.\n\n\
         5. SILENCE — If the chunk has no vocals (instrumental only, or pre-roll silence), output exactly: # no vocals\n\n\
         6. OUTPUT FORMAT — Output ONLY timed lines. No intro text, no commentary, no markdown fences, no summary at the end.\n\n\
         7. DO NOT HALLUCINATE — Only transcribe what you actually hear. If you hear a word not matching the reference lyrics below, still write what you hear. If the reference has a line that doesn't appear in this audio chunk, do NOT include it.\n\n\
         Reference lyrics for this song (extracted from YouTube description — may be out of order, missing chorus repeats, or contain extra phrases not in this chunk):\n\
         {reference_lyrics}\n\n\
         This chunk covers audio from {start:.1}s to {end:.1}s of the full song ({total:.1}s total). The chunk may start or end mid-phrase.\n",
        start = chunk_start_ms as f64 / 1000.0,
        end = chunk_end_ms as f64 / 1000.0,
        total = full_duration_ms as f64 / 1000.0,
    )
}
```

- [ ] **Step 4: Re-run tests — expect pass**

```bash
cargo test -p sp-server --lib lyrics::gemini_prompt -- --nocapture
```
Expected: `3 passed; 0 failed`.

- [ ] **Step 5: Format + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/gemini_prompt.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): gemini prompt builder with rules from design spec"
```

---

### Task 1.2: `gemini_parse.rs` — response parser (pure fn, TDD)

**Files:**
- Create: `crates/sp-server/src/lyrics/gemini_parse.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub mod gemini_parse;`)

- [ ] **Step 1: Write failing tests**

Create `crates/sp-server/src/lyrics/gemini_parse.rs`:

```rust
//! Parser for Gemini's `(MM:SS.x --> MM:SS.x) text` timed-line output.

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedLine {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// Parse a Gemini response body into a list of timed lines.
/// Skips blank lines and comment lines (lines starting with `#`).
pub fn parse_timed_lines(raw: &str) -> Vec<ParsedLine> {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_lines() {
        let out = parse_timed_lines(
            "(00:17.2 --> 00:20.0) I could search all this world,\n\
             (00:20.3 --> 00:26.5) I still find there is no one like You\n",
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], ParsedLine { start_ms: 17_200, end_ms: 20_000, text: "I could search all this world,".into() });
        assert_eq!(out[1].start_ms, 20_300);
        assert_eq!(out[1].end_ms, 26_500);
    }

    #[test]
    fn skips_no_vocals_sentinel() {
        let out = parse_timed_lines("# no vocals\n(00:05.0 --> 00:07.0) hi\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "hi");
    }

    #[test]
    fn skips_blank_lines() {
        let out = parse_timed_lines("\n\n(00:01.0 --> 00:02.0) x\n\n");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn ignores_lines_without_timing_format() {
        let out = parse_timed_lines(
            "Here are the lyrics:\n\
             (00:01.0 --> 00:02.0) valid line\n\
             ```\n\
             bare text without timing\n",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "valid line");
    }

    #[test]
    fn handles_minutes_only_no_decimal_seconds() {
        let out = parse_timed_lines("(01:23 --> 01:25) no decimals\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_ms, 83_000);
        assert_eq!(out[0].end_ms, 85_000);
    }

    #[test]
    fn trims_trailing_whitespace_from_text() {
        let out = parse_timed_lines("(00:01.0 --> 00:02.0) trailing spaces   \n");
        assert_eq!(out[0].text, "trailing spaces");
    }
}
```

Add to mod.rs:

```rust
pub mod gemini_parse;
```

- [ ] **Step 2: Run — expect 6 panics**

```bash
cargo test -p sp-server --lib lyrics::gemini_parse -- --nocapture
```

- [ ] **Step 3: Implement `parse_timed_lines`**

Replace the stub:

```rust
use regex::Regex;
use std::sync::OnceLock;

fn timed_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\((\d{1,2}):(\d{1,2}(?:\.\d+)?)\s*-->\s*(\d{1,2}):(\d{1,2}(?:\.\d+)?)\)\s*(.+)$")
            .expect("static regex compiles")
    })
}

pub fn parse_timed_lines(raw: &str) -> Vec<ParsedLine> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(caps) = timed_line_re().captures(line) else {
            continue;
        };
        let s_min: u64 = caps[1].parse().unwrap_or(0);
        let s_sec: f64 = caps[2].parse().unwrap_or(0.0);
        let e_min: u64 = caps[3].parse().unwrap_or(0);
        let e_sec: f64 = caps[4].parse().unwrap_or(0.0);
        let start_ms = (s_min * 60_000) + (s_sec * 1000.0) as u64;
        let end_ms = (e_min * 60_000) + (e_sec * 1000.0) as u64;
        let text = caps[5].trim().to_string();
        out.push(ParsedLine { start_ms, end_ms, text });
    }
    out
}
```

Verify `regex` is already in workspace deps (`autosub_provider.rs` uses it). If not, add to `crates/sp-server/Cargo.toml` under `[dependencies]`:
```toml
regex = { workspace = true }
```
(It already is — confirmed by existing usage.)

- [ ] **Step 4: Tests pass**

```bash
cargo test -p sp-server --lib lyrics::gemini_parse -- --nocapture
```
Expected: `6 passed; 0 failed`.

- [ ] **Step 5: Format + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/gemini_parse.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): parser for Gemini timed-line response format"
```

---

### Task 1.3: `gemini_chunks.rs` — chunk planning + overlap merge (pure fn, TDD)

**Files:**
- Create: `crates/sp-server/src/lyrics/gemini_chunks.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub mod gemini_chunks;`)

- [ ] **Step 1: Write failing tests**

Create `crates/sp-server/src/lyrics/gemini_chunks.rs`:

```rust
//! Chunk planning (how to slice the song into 60s/10s-overlap chunks) and
//! overlap-merge logic for stitching per-chunk timed-line outputs into a
//! single global timeline.

use crate::lyrics::gemini_parse::ParsedLine;

pub const CHUNK_DURATION_MS: u64 = 60_000;
pub const CHUNK_OVERLAP_MS: u64 = 10_000;
pub const CHUNK_STRIDE_MS: u64 = CHUNK_DURATION_MS - CHUNK_OVERLAP_MS; // 50_000

/// A planned chunk: range of the full song this chunk covers.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkPlan {
    pub idx: usize,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Plan the chunks for a song of the given total duration.
/// Always at least one chunk. Last chunk is clipped to duration_ms.
pub fn plan_chunks(duration_ms: u64) -> Vec<ChunkPlan> {
    unimplemented!()
}

/// A line with globally-offset timing (chunk local_ms + chunk_start_ms).
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalLine {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// Merge per-chunk parsed lines into a single ordered GlobalLine list.
/// Deduplicates entries that appear in overlap regions of adjacent chunks.
///
/// Dedup rule: two lines from adjacent chunks N and N+1 are duplicates if
/// - their normalized text matches, AND
/// - their global start times are within 1500ms of each other.
/// When duplicates found: keep the one whose start is further from the
/// chunk boundary between N and N+1.
pub fn merge_overlap(plans: &[ChunkPlan], per_chunk: &[Vec<ParsedLine>]) -> Vec<GlobalLine> {
    unimplemented!()
}

/// Normalize text for dedup: lowercase, strip non-word chars except apostrophes,
/// collapse whitespace.
pub fn normalize_text(s: &str) -> String {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_chunks_single_chunk_for_short_song() {
        let p = plan_chunks(45_000);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0], ChunkPlan { idx: 0, start_ms: 0, end_ms: 45_000 });
    }

    #[test]
    fn plan_chunks_exact_60s_is_single_chunk() {
        let p = plan_chunks(60_000);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].end_ms, 60_000);
    }

    #[test]
    fn plan_chunks_11min_song_yields_13_chunks() {
        let p = plan_chunks(659_980);
        assert_eq!(p.len(), 13);
        assert_eq!(p[0].start_ms, 0);
        assert_eq!(p[0].end_ms, 60_000);
        assert_eq!(p[1].start_ms, 50_000);
        assert_eq!(p[12].end_ms, 659_980);
        // Stride is 50s
        for i in 1..p.len() {
            assert_eq!(p[i].start_ms - p[i - 1].start_ms, 50_000);
        }
    }

    #[test]
    fn normalize_text_lowercase_strip_punct() {
        assert_eq!(normalize_text("I Want to Know You,"), "i want to know you");
        assert_eq!(normalize_text("I'm gonna love You"), "i'm gonna love you");
        assert_eq!(normalize_text("  Hello   World  "), "hello world");
    }

    #[test]
    fn merge_overlap_deduplicates_matching_lines_across_boundary() {
        let plans = vec![
            ChunkPlan { idx: 0, start_ms: 0, end_ms: 60_000 },
            ChunkPlan { idx: 1, start_ms: 50_000, end_ms: 110_000 },
        ];
        // Chunk 0 ends with "overlap line" at local 55s. Chunk 1 begins with same at local 5s (both = global 55s).
        let per_chunk = vec![
            vec![
                ParsedLine { start_ms: 1_000, end_ms: 3_000, text: "first line".into() },
                ParsedLine { start_ms: 55_000, end_ms: 58_000, text: "overlap line".into() },
            ],
            vec![
                ParsedLine { start_ms: 5_000, end_ms: 8_000, text: "overlap line".into() },
                ParsedLine { start_ms: 20_000, end_ms: 22_000, text: "chunk1 tail".into() },
            ],
        ];
        let merged = merge_overlap(&plans, &per_chunk);
        assert_eq!(merged.len(), 3, "expected 3 lines after dedup, got: {:?}", merged);
        assert_eq!(merged[0].text, "first line");
        assert_eq!(merged[0].start_ms, 1_000);
        assert_eq!(merged[1].text, "overlap line");
        assert_eq!(merged[2].text, "chunk1 tail");
        assert_eq!(merged[2].start_ms, 70_000); // 50_000 + 20_000
    }

    #[test]
    fn merge_overlap_keeps_both_when_text_differs_in_overlap() {
        let plans = vec![
            ChunkPlan { idx: 0, start_ms: 0, end_ms: 60_000 },
            ChunkPlan { idx: 1, start_ms: 50_000, end_ms: 110_000 },
        ];
        let per_chunk = vec![
            vec![ParsedLine { start_ms: 55_000, end_ms: 58_000, text: "from chunk 0".into() }],
            vec![ParsedLine { start_ms: 6_000, end_ms: 8_000, text: "from chunk 1".into() }],
        ];
        let merged = merge_overlap(&plans, &per_chunk);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_overlap_keeps_both_when_text_same_but_start_gap_large() {
        let plans = vec![
            ChunkPlan { idx: 0, start_ms: 0, end_ms: 60_000 },
            ChunkPlan { idx: 1, start_ms: 50_000, end_ms: 110_000 },
        ];
        // Same text but starts 4s apart in global time → NOT duplicates
        let per_chunk = vec![
            vec![ParsedLine { start_ms: 51_000, end_ms: 53_000, text: "oh jesus".into() }],
            vec![ParsedLine { start_ms: 5_000, end_ms: 7_000, text: "oh jesus".into() }], // global 55s
        ];
        let merged = merge_overlap(&plans, &per_chunk);
        assert_eq!(merged.len(), 2, "4s apart > 1500ms threshold — keep both");
    }
}
```

Add to mod.rs:
```rust
pub mod gemini_chunks;
```

- [ ] **Step 2: Run — expect panics**

```bash
cargo test -p sp-server --lib lyrics::gemini_chunks -- --nocapture
```

- [ ] **Step 3: Implement**

```rust
pub fn plan_chunks(duration_ms: u64) -> Vec<ChunkPlan> {
    let mut out = Vec::new();
    if duration_ms == 0 {
        return out;
    }
    let mut start = 0u64;
    let mut idx = 0usize;
    loop {
        let end = (start + CHUNK_DURATION_MS).min(duration_ms);
        out.push(ChunkPlan { idx, start_ms: start, end_ms: end });
        if end >= duration_ms {
            break;
        }
        idx += 1;
        start += CHUNK_STRIDE_MS;
    }
    out
}

pub fn normalize_text(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut prev_space = true;
    for c in lower.chars() {
        if c.is_alphanumeric() || c == '\'' {
            out.push(c);
            prev_space = false;
        } else if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        }
        // else: drop punctuation
    }
    out.trim().to_string()
}

pub fn merge_overlap(plans: &[ChunkPlan], per_chunk: &[Vec<ParsedLine>]) -> Vec<GlobalLine> {
    assert_eq!(plans.len(), per_chunk.len(), "plans and per_chunk must align");

    // Shift each chunk's lines to global
    let mut globals: Vec<Vec<GlobalLine>> = plans
        .iter()
        .zip(per_chunk.iter())
        .map(|(plan, lines)| {
            lines
                .iter()
                .map(|l| GlobalLine {
                    start_ms: l.start_ms + plan.start_ms,
                    end_ms: l.end_ms + plan.start_ms,
                    text: l.text.clone(),
                })
                .collect()
        })
        .collect();

    // Walk adjacent pairs, dedup in overlap region
    const AGREEMENT_MS: i64 = 1_500;
    for i in 0..plans.len().saturating_sub(1) {
        let overlap_start = plans[i + 1].start_ms;
        let overlap_end = plans[i].end_ms;
        if overlap_end <= overlap_start {
            continue;
        }
        // Indices (into globals[i] and globals[i+1]) of lines in the overlap region
        let a_indices: Vec<usize> = globals[i]
            .iter()
            .enumerate()
            .filter(|(_, l)| l.end_ms > overlap_start)
            .map(|(k, _)| k)
            .collect();
        let b_indices: Vec<usize> = globals[i + 1]
            .iter()
            .enumerate()
            .filter(|(_, l)| l.start_ms < overlap_end)
            .map(|(k, _)| k)
            .collect();

        let mut drop_a: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut drop_b: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for &ia in &a_indices {
            for &ib in &b_indices {
                if drop_a.contains(&ia) || drop_b.contains(&ib) {
                    continue;
                }
                let la = &globals[i][ia];
                let lb = &globals[i + 1][ib];
                if normalize_text(&la.text) != normalize_text(&lb.text) {
                    continue;
                }
                if (la.start_ms as i64 - lb.start_ms as i64).abs() > AGREEMENT_MS {
                    continue;
                }
                // Keep the one further from the boundary (which is overlap_start == overlap_end boundary idea,
                // but the actual chunk boundary is at globals[i].end_ms == overlap_end for A, and
                // globals[i+1].start_ms == overlap_start for B).
                let a_dist = (la.start_ms as i64 - overlap_end as i64).abs();
                let b_dist = (lb.start_ms as i64 - overlap_start as i64).abs();
                if a_dist >= b_dist {
                    drop_b.insert(ib);
                } else {
                    drop_a.insert(ia);
                }
            }
        }
        globals[i] = globals[i].iter().enumerate()
            .filter(|(k, _)| !drop_a.contains(k))
            .map(|(_, l)| l.clone())
            .collect();
        globals[i + 1] = globals[i + 1].iter().enumerate()
            .filter(|(k, _)| !drop_b.contains(k))
            .map(|(_, l)| l.clone())
            .collect();
    }

    let mut flat: Vec<GlobalLine> = globals.into_iter().flatten().collect();
    flat.sort_by_key(|l| l.start_ms);
    flat
}
```

- [ ] **Step 4: Tests pass**

```bash
cargo test -p sp-server --lib lyrics::gemini_chunks -- --nocapture
```
Expected: all 7 tests pass.

- [ ] **Step 5: Format + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/gemini_chunks.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): gemini chunk planning and overlap merge logic"
```

---

### Task 1.4: `gemini_client.rs` — HTTP call to CLIProxy Gemini endpoint (with wiremock test)

**Files:**
- Create: `crates/sp-server/src/lyrics/gemini_client.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub mod gemini_client;`)
- Modify: `crates/sp-server/Cargo.toml` (confirm `wiremock` is in `[dev-dependencies]`; add if missing)

- [ ] **Step 1: Confirm `wiremock` is available**

```bash
grep -n '^wiremock' crates/sp-server/Cargo.toml
```
If no output, add under `[dev-dependencies]`:
```toml
wiremock = "0.6"
```

- [ ] **Step 2: Write failing test**

Create `crates/sp-server/src/lyrics/gemini_client.rs`:

```rust
//! HTTP client for calling Gemini via CLIProxyAPI's Gemini-native endpoint.

use anyhow::{Context, Result};
use serde_json::json;
use std::path::Path;
use std::time::Duration;

pub struct GeminiClient {
    pub base_url: String,
    pub model: String,
    pub timeout_s: u64,
}

impl GeminiClient {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            timeout_s: 90,
        }
    }

    /// Send prompt + audio to Gemini, return the text body from the first candidate.
    pub async fn transcribe_chunk(&self, prompt: &str, audio_wav: &Path) -> Result<String> {
        let bytes = tokio::fs::read(audio_wav)
            .await
            .with_context(|| format!("read chunk audio {audio_wav:?}"))?;
        let audio_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

        let body = json!({
            "contents": [{
                "parts": [
                    {"text": prompt},
                    {"inline_data": {"mime_type": "audio/wav", "data": audio_b64}}
                ]
            }],
            "generationConfig": {"temperature": 0.0}
        });

        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url.trim_end_matches('/'),
            self.model
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.timeout_s))
            .build()
            .context("reqwest client")?;
        let resp = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("POST to CLIProxy Gemini")?;
        let status = resp.status();
        let text = resp.text().await.context("read response body")?;
        if !status.is_success() {
            anyhow::bail!("gemini call failed: HTTP {status}: {text}");
        }
        let doc: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("parse JSON: {text}"))?;
        let out = doc
            .pointer("/candidates/0/content/parts/0/text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("no text in candidates[0]: {text}"))?;
        Ok(out.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn transcribe_chunk_extracts_text_from_first_candidate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1beta/models/.+:generateContent$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{
                    "content": {
                        "parts": [
                            {"text": "(00:01.0 --> 00:02.0) hello"}
                        ]
                    }
                }]
            })))
            .mount(&server)
            .await;

        // Write a trivial WAV file to disk
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &[0u8; 16]).unwrap();

        let client = GeminiClient::new(server.uri(), "gemini-3-pro-preview");
        let out = client.transcribe_chunk("prompt", tmp.path()).await.unwrap();
        assert_eq!(out, "(00:01.0 --> 00:02.0) hello");
    }

    #[tokio::test]
    async fn transcribe_chunk_errors_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &[0u8; 16]).unwrap();
        let client = GeminiClient::new(server.uri(), "gemini-3-pro-preview");
        let err = client.transcribe_chunk("p", tmp.path()).await.unwrap_err();
        assert!(format!("{err}").contains("HTTP 500"), "err = {err}");
    }

    #[tokio::test]
    async fn transcribe_chunk_errors_when_no_candidates() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"candidates": []})))
            .mount(&server)
            .await;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &[0u8; 16]).unwrap();
        let client = GeminiClient::new(server.uri(), "gemini-3-pro-preview");
        let err = client.transcribe_chunk("p", tmp.path()).await.unwrap_err();
        assert!(format!("{err}").contains("no text in candidates"), "err = {err}");
    }
}
```

Add to mod.rs:
```rust
pub mod gemini_client;
```

Confirm `base64` and `tempfile` are already workspace dev-dependencies (both are used elsewhere in the repo).

- [ ] **Step 3: Run tests — expect compilation error first, then test failures after fixing imports**

```bash
cargo test -p sp-server --lib lyrics::gemini_client -- --nocapture
```

- [ ] **Step 4: Fix imports / add `base64::Engine` import where needed**

If the compile error is on `general_purpose::STANDARD.encode`, add:
```rust
use base64::Engine as _;
```

Re-run:
```bash
cargo test -p sp-server --lib lyrics::gemini_client -- --nocapture
```
Expected: 3 tests, all pass.

- [ ] **Step 5: Format + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/gemini_client.rs crates/sp-server/src/lyrics/mod.rs crates/sp-server/Cargo.toml
git commit -m "feat(lyrics): gemini HTTP client via CLIProxy /v1beta"
```

---

### Task 1.5: `gemini_provider.rs` — `AlignmentProvider` impl tying it together

**Files:**
- Create: `crates/sp-server/src/lyrics/gemini_provider.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub mod gemini_provider;`)

This provider splits the vocal WAV via ffmpeg into per-chunk WAVs in a tmp dir, calls Gemini per chunk, parses, merges, and emits a `ProviderResult` with `LineTiming` entries (word vectors empty). It also writes a raw-chunk cache `{youtube_id}_gemini_chunks.json` next to other cache files.

- [ ] **Step 1: Write failing integration test using wiremock + a pre-made tiny WAV**

Create `crates/sp-server/src/lyrics/gemini_provider.rs`:

```rust
//! Gemini-based AlignmentProvider. Chunks the Demucs-dereverbed vocal WAV,
//! calls Gemini per chunk via CLIProxyAPI, merges per Appendix B of the
//! design spec. Produces line-level timings only (word vectors empty).

use crate::lyrics::gemini_chunks::{GlobalLine, plan_chunks};
use crate::lyrics::gemini_client::GeminiClient;
use crate::lyrics::gemini_parse::parse_timed_lines;
use crate::lyrics::gemini_prompt::build_prompt;
use crate::lyrics::provider::{
    AlignmentProvider, LineTiming, ProviderResult, SongContext, WordTiming,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tracing::{debug, warn};

pub struct GeminiProvider {
    pub client: GeminiClient,
    pub ffmpeg_path: PathBuf,
    pub ffprobe_path: PathBuf,
    pub cache_dir: PathBuf,
}

#[async_trait]
impl AlignmentProvider for GeminiProvider {
    fn name(&self) -> &str {
        "gemini"
    }

    fn base_confidence(&self) -> f32 {
        // Treated as the sole line-timing source; confidence value is unused while
        // the merge layer is still pick-highest (qwen3 is disabled).
        0.9
    }

    async fn can_provide(&self, ctx: &SongContext) -> bool {
        ctx.clean_vocal_path.as_ref().is_some_and(|p| p.exists())
    }

    #[cfg_attr(test, mutants::skip)]
    async fn align(&self, ctx: &SongContext) -> Result<ProviderResult> {
        let vocal = ctx
            .clean_vocal_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("gemini: clean_vocal_path missing"))?;
        let reference = gather_reference_text(&ctx.candidate_texts);
        let plans = plan_chunks(ctx.duration_ms);

        // Per-song chunk tmp dir — cleaned on drop
        let tmp = tempfile::tempdir().context("create chunk tmp dir")?;
        let mut per_chunk = Vec::with_capacity(plans.len());
        let mut raw_cache_entries = Vec::with_capacity(plans.len());

        for plan in &plans {
            let chunk_wav = tmp.path().join(format!("chunk_{:02}.wav", plan.idx));
            if let Err(e) =
                slice_chunk(&self.ffmpeg_path, vocal, plan.start_ms, plan.end_ms, &chunk_wav).await
            {
                warn!("gemini: chunk {} slice failed: {e}", plan.idx);
                per_chunk.push(Vec::new());
                raw_cache_entries.push(RawChunk { start_ms: plan.start_ms, end_ms: plan.end_ms, raw: String::new() });
                continue;
            }
            let prompt = build_prompt(&reference, plan.start_ms, plan.end_ms, ctx.duration_ms);
            debug!(chunk = plan.idx, "gemini: calling Gemini for chunk");
            match self.client.transcribe_chunk(&prompt, &chunk_wav).await {
                Ok(raw) => {
                    let parsed = parse_timed_lines(&raw);
                    debug!(
                        chunk = plan.idx,
                        parsed = parsed.len(),
                        "gemini: chunk parsed"
                    );
                    per_chunk.push(parsed);
                    raw_cache_entries.push(RawChunk {
                        start_ms: plan.start_ms,
                        end_ms: plan.end_ms,
                        raw,
                    });
                }
                Err(e) => {
                    warn!("gemini: chunk {} call failed: {e}", plan.idx);
                    per_chunk.push(Vec::new());
                    raw_cache_entries.push(RawChunk {
                        start_ms: plan.start_ms,
                        end_ms: plan.end_ms,
                        raw: String::new(),
                    });
                }
            }
        }

        // Write raw cache (best-effort; do not fail align on cache write error)
        if let Err(e) = write_raw_cache(&self.cache_dir, &ctx.video_id, &raw_cache_entries).await {
            warn!("gemini: raw cache write failed: {e}");
        }

        // Merge
        let merged =
            crate::lyrics::gemini_chunks::merge_overlap(&plans, &per_chunk);
        if merged.is_empty() {
            anyhow::bail!("gemini: no lines produced from any chunk");
        }

        // Convert to ProviderResult (word timings empty for MVP)
        let lines = merged
            .into_iter()
            .map(|g| LineTiming {
                text: g.text,
                start_ms: g.start_ms,
                end_ms: g.end_ms,
                words: Vec::<WordTiming>::new(),
            })
            .collect();
        Ok(ProviderResult {
            provider_name: self.name().into(),
            lines,
            metadata: serde_json::json!({
                "base_confidence": self.base_confidence(),
                "chunks": plans.len(),
            }),
        })
    }
}

fn gather_reference_text(candidates: &[crate::lyrics::provider::CandidateText]) -> String {
    // Prefer the description source; fall back to whichever candidate has the most lines.
    let pick = candidates
        .iter()
        .find(|c| c.source == "description")
        .or_else(|| candidates.iter().max_by_key(|c| c.lines.len()));
    match pick {
        Some(c) if !c.lines.is_empty() => c.lines.join("\n"),
        _ => "(no reference lyrics available for this song)".to_string(),
    }
}

async fn slice_chunk(
    ffmpeg: &Path,
    input: &Path,
    start_ms: u64,
    end_ms: u64,
    out: &Path,
) -> Result<()> {
    let dur_s = (end_ms - start_ms) as f64 / 1000.0;
    let ss_s = start_ms as f64 / 1000.0;
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args([
        "-y",
        "-loglevel",
        "error",
        "-ss",
        &format!("{ss_s}"),
        "-t",
        &format!("{dur_s}"),
        "-i",
    ])
    .arg(input)
    .args(["-c:a", "pcm_s16le"])
    .arg(out)
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let output = cmd.output().await.context("run ffmpeg for chunk slice")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ffmpeg failed: {err}");
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct RawChunk {
    start_ms: u64,
    end_ms: u64,
    raw: String,
}

async fn write_raw_cache(cache_dir: &Path, video_id: &str, chunks: &[RawChunk]) -> Result<()> {
    let path = cache_dir.join(format!("{video_id}_gemini_chunks.json"));
    let body = serde_json::json!({"chunks": chunks});
    tokio::fs::write(&path, serde_json::to_string_pretty(&body)?).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::provider::CandidateText;
    use serde_json::json;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn gather_reference_text_prefers_description() {
        let cands = vec![
            CandidateText { source: "autosub".into(), lines: vec!["a".into()], has_timing: false, line_timings: None },
            CandidateText { source: "description".into(), lines: vec!["b".into(), "c".into()], has_timing: false, line_timings: None },
        ];
        assert_eq!(gather_reference_text(&cands), "b\nc");
    }

    #[test]
    fn gather_reference_text_falls_back_to_longest() {
        let cands = vec![
            CandidateText { source: "autosub".into(), lines: vec!["a".into(), "b".into()], has_timing: false, line_timings: None },
            CandidateText { source: "lrclib".into(), lines: vec!["c".into()], has_timing: false, line_timings: None },
        ];
        assert_eq!(gather_reference_text(&cands), "a\nb");
    }

    #[test]
    fn gather_reference_text_empty_is_placeholder() {
        assert!(gather_reference_text(&[]).contains("no reference"));
    }

    // Integration smoke test — needs a working ffmpeg binary. Gated behind
    // a non-default env var so CI lint-only runs skip it. Windows-only
    // because the deploy runner is Windows and has ffmpeg in the cache dir.
    #[tokio::test]
    #[cfg(target_os = "windows")]
    async fn align_happy_path_with_mocked_gemini() {
        use std::path::PathBuf;
        let ffmpeg = PathBuf::from(r"C:\ProgramData\SongPlayer\cache\tools\ffmpeg.exe");
        let ffprobe = PathBuf::from(r"C:\ProgramData\SongPlayer\cache\tools\ffprobe.exe");
        if !ffmpeg.exists() {
            eprintln!("skipping — ffmpeg not found");
            return;
        }

        // Make a short WAV (2s silence) via ffmpeg
        let tmp = tempfile::tempdir().unwrap();
        let wav = tmp.path().join("vocal.wav");
        let s = tokio::process::Command::new(&ffmpeg)
            .args(["-y", "-f", "lavfi", "-i", "anullsrc=r=16000:cl=mono", "-t", "2"])
            .arg(&wav)
            .output().await.unwrap();
        assert!(s.status.success(), "ffmpeg test wav generation failed");

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1beta/models/.+:generateContent$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{"content": {"parts": [{"text": "(00:00.5 --> 00:01.5) hello there"}]}}]
            })))
            .mount(&server)
            .await;

        let cache = tempfile::tempdir().unwrap();
        let provider = GeminiProvider {
            client: GeminiClient::new(server.uri(), "gemini-3-pro-preview"),
            ffmpeg_path: ffmpeg,
            ffprobe_path: ffprobe,
            cache_dir: cache.path().to_path_buf(),
        };

        let ctx = SongContext {
            video_id: "test123".into(),
            audio_path: wav.clone(),
            clean_vocal_path: Some(wav.clone()),
            candidate_texts: vec![CandidateText {
                source: "description".into(),
                lines: vec!["hello there".into()],
                has_timing: false,
                line_timings: None,
            }],
            autosub_json3: None,
            duration_ms: 2_000,
        };
        let out = provider.align(&ctx).await.unwrap();
        assert_eq!(out.provider_name, "gemini");
        assert_eq!(out.lines.len(), 1);
        assert_eq!(out.lines[0].text, "hello there");
        assert_eq!(out.lines[0].start_ms, 500);
        assert_eq!(out.lines[0].end_ms, 1_500);
        // Raw cache file written
        let cache_file = cache.path().join("test123_gemini_chunks.json");
        assert!(cache_file.exists());
    }
}
```

Add to mod.rs:
```rust
pub mod gemini_provider;
```

- [ ] **Step 2: Run non-Windows tests (the three pure fns)**

```bash
cargo test -p sp-server --lib lyrics::gemini_provider::tests::gather -- --nocapture
```
Expected: 3 passed.

- [ ] **Step 3: Format + commit**

The Windows-only integration test runs only on the deploy runner's `cargo test` but is an intentional skip on Linux CI. Unit tests for the pure helpers are sufficient for the mutation gate; the I/O body is already `mutants::skip`.

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/gemini_provider.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): GeminiProvider implements AlignmentProvider with chunk-merge pipeline"
```

---

### Task 1.6: Register GeminiProvider in worker + feature-flag constants

**Files:**
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add feature flag constants)
- Modify: `crates/sp-server/src/lyrics/worker.rs` (register GeminiProvider, skip Qwen3 when disabled)

- [ ] **Step 1: Write failing test against worker's provider list**

This one is tricky because `worker.rs::process_song` is `mutants::skip` and has no existing unit test shape. Instead test the flag constants directly.

Add to `crates/sp-server/src/lyrics/mod.rs` after the existing `LYRICS_PIPELINE_VERSION` const:

```rust
/// Feature flag: enable the Gemini-based AlignmentProvider. When true, the
/// worker registers `GeminiProvider` in the provider list.
pub const LYRICS_GEMINI_ENABLED: bool = true;

/// Feature flag: enable the Qwen3 forced-alignment provider. When false, the
/// worker skips registering it even if Python venv is available. Kept as a
/// flag (not a code removal) so word-level work can revive qwen3 without a
/// history rewrite.
pub const LYRICS_QWEN3_ENABLED: bool = false;
```

Append to the existing `#[cfg(test)] mod tests { ... }` in `mod.rs`:

```rust
    #[test]
    fn gemini_enabled_and_qwen3_disabled_by_default() {
        assert!(super::LYRICS_GEMINI_ENABLED);
        assert!(!super::LYRICS_QWEN3_ENABLED);
    }
```

- [ ] **Step 2: Run — expect a test failure IF defaults are wrong, else pass and proceed to the functional wiring**

```bash
cargo test -p sp-server --lib lyrics::tests::gemini_enabled_and_qwen3 -- --nocapture
```
Expected: pass (the constants exist per Step 1).

- [ ] **Step 3: Wire `GeminiProvider` in `worker.rs::process_song`**

Find the block in `worker.rs` around lines 574–585 that builds the provider list. Replace:

```rust
        // Build provider list. AutoSubProvider always registered; Qwen3Provider only
        // when Python venv + clean vocal are available.
        let mut providers: Vec<Box<dyn crate::lyrics::provider::AlignmentProvider>> = Vec::new();
        providers.push(Box::new(AutoSubProvider));
        if let Some(python) = python_for_qwen3 {
            providers.push(Box::new(Qwen3Provider {
                python_path: python,
                script_path: self.script_path.clone(),
                models_dir: self.models_dir.clone(),
            }));
        }
```

With:

```rust
        // Build provider list.
        // - AutoSubProvider: always registered (cheap text candidate, also provides
        //   autosub timing anchors for legacy callers).
        // - GeminiProvider: registered when LYRICS_GEMINI_ENABLED and the CLIProxy
        //   Gemini URL is resolvable.
        // - Qwen3Provider: registered only when LYRICS_QWEN3_ENABLED is true AND
        //   Python venv + clean vocal are available. Parked off for now; will be
        //   revived when word-level work resumes.
        use crate::lyrics::{
            LYRICS_GEMINI_ENABLED, LYRICS_QWEN3_ENABLED,
            gemini_client::GeminiClient, gemini_provider::GeminiProvider,
        };
        let mut providers: Vec<Box<dyn crate::lyrics::provider::AlignmentProvider>> = Vec::new();
        providers.push(Box::new(AutoSubProvider));
        if LYRICS_GEMINI_ENABLED {
            let proxy_url = std::env::var("CLIPROXY_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:18787".to_string());
            let model = std::env::var("GEMINI_LYRICS_MODEL")
                .unwrap_or_else(|_| "gemini-3-pro-preview".to_string());
            providers.push(Box::new(GeminiProvider {
                client: GeminiClient::new(proxy_url, model),
                ffmpeg_path: self.ffmpeg_path.clone(),
                ffprobe_path: self.ffprobe_path.clone(),
                cache_dir: self.cache_dir.clone(),
            }));
        }
        if LYRICS_QWEN3_ENABLED {
            if let Some(python) = python_for_qwen3 {
                providers.push(Box::new(Qwen3Provider {
                    python_path: python,
                    script_path: self.script_path.clone(),
                    models_dir: self.models_dir.clone(),
                }));
            }
        }
```

If `self.ffmpeg_path` / `self.ffprobe_path` don't exist on the worker struct, locate where it has `self.script_path` and add two new fields plumbed from the same startup code that populates `script_path`. Follow the pattern already used for `script_path` in `worker.rs`.

- [ ] **Step 4: `cargo check` to catch missing field plumbing**

```bash
cargo check -p sp-server
```
Fix any compile errors by adding the two new fields and wiring them from the caller (find where the worker is constructed; likely in `lib.rs`'s `start` function).

- [ ] **Step 5: Run full sp-server tests**

```bash
cargo test -p sp-server --lib lyrics -- --nocapture
```
Expected: all existing lyrics tests still pass.

- [ ] **Step 6: Format + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/mod.rs crates/sp-server/src/lyrics/worker.rs crates/sp-server/src/lib.rs
git commit -m "feat(lyrics): wire GeminiProvider with feature-flag gate; park Qwen3"
```

---

### Task 1.7: Bump LYRICS_PIPELINE_VERSION 10 → 11

**Files:**
- Modify: `crates/sp-server/src/lyrics/mod.rs`

- [ ] **Step 1: Update the existing test for version**

Find in `mod.rs`:
```rust
    fn lyrics_pipeline_version_is_v10() {
        assert_eq!(
            LYRICS_PIPELINE_VERSION, 10,
            ...
```
Change to:
```rust
    fn lyrics_pipeline_version_is_v11() {
        assert_eq!(
            LYRICS_PIPELINE_VERSION, 11,
            "v11 = Gemini chunked lyrics provider replaces qwen3 for line timing"
        );
    }
```

- [ ] **Step 2: Run — expect failure**

```bash
cargo test -p sp-server --lib lyrics::tests::lyrics_pipeline_version_is_v11 -- --nocapture
```
Expected: fails with "left: 10, right: 11".

- [ ] **Step 3: Bump the constant**

```rust
pub const LYRICS_PIPELINE_VERSION: u32 = 11;
```

- [ ] **Step 4: Re-run — expect pass**

```bash
cargo test -p sp-server --lib lyrics::tests::lyrics_pipeline_version_is_v11 -- --nocapture
```

- [ ] **Step 5: Format + commit**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): bump LYRICS_PIPELINE_VERSION 10 -> 11 (gemini provider)"
```

---

### Task 1.8: Update `CLAUDE.md` pipeline version history

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Append the v11 entry**

Find in `CLAUDE.md` the section "## Pipeline versioning (lyrics)" → "History:". After the `- v10 ...` bullet add:

```markdown
- v11 (#TBD): Gemini 3 Pro chunked transcription replaces qwen3 forced alignment
  for line-level timing. Demucs-dereverbed vocal WAV is sliced into 60 s chunks
  with 10 s overlap, each chunk transcribed independently via CLIProxyAPI's
  Google-OAuth free tier, overlapping regions deduplicated by normalized-text
  match + 1.5 s start-time agreement. Word-level timings deferred; qwen3 parked
  behind `LYRICS_QWEN3_ENABLED=false`. Addresses the song-230 collapse from v10
  where untimed reference text caused qwen3 to cram an 11-min song into 10 s.
```

Also update the "Bump the constant when:" bullet list with:

```markdown
- Switching alignment-provider registration (e.g. enabling/disabling Gemini or Qwen3 via the feature flags in `mod.rs`)
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs(CLAUDE): add lyrics pipeline v11 entry for gemini provider"
```

---

### Task 1.9: Local sanity + push + CI monitor

**Files:** none

- [ ] **Step 1: Full local fmt check**

```bash
cargo fmt --all --check
```
Expected: clean.

- [ ] **Step 2: Push to dev**

```bash
git fetch origin
git push origin dev
```

- [ ] **Step 3: Monitor CI**

Single command:
```bash
RUN=$(gh run list --branch dev --limit 1 --json databaseId --jq '.[0].databaseId')
sleep 300 && gh run view "$RUN" --json status,conclusion,jobs
```

If any job fails: `gh run view "$RUN" --log-failed`; fix in one commit; re-push; re-monitor.

Expected final state: all CI jobs green.

- [ ] **Step 4: Verify deploy job ran and SongPlayer on win-resolume now has v11**

```bash
# Over MCP or SSH to win-resolume
Get-Item "C:\Program Files\SongPlayer\SongPlayer.exe" | Select @{n='Version';e={$_.VersionInfo.FileVersion}}, LastWriteTime
```

Then via HTTP:
```bash
curl -s http://10.77.9.201:8920/api/v1/status | jq .version
```
Expected: version string reflects the newly-pushed dev build.

---

### Task 1.10: 24-hour post-deploy acceptance

**Files:** none (notes go in `docs/superpowers/specs/2026-04-20-gemini-phase-0-validation-notes.md` for traceability)

- [ ] **Step 1: Wait for the auto-reprocess pass**

On pipeline-version bump, the worker re-queues every song whose DB `lyrics_pipeline_version < 11` as stale and reprocesses them worst-quality-first. For a 200+ song catalog at ~1 min per song, this takes ~3-4 hours.

Monitor progress (reading the dashboard's lyrics queue on `/lyrics`):
```bash
curl -s "http://10.77.9.201:8920/api/v1/lyrics/queue" | jq .
```
Expected: `bucket2_count` (stale) drains from ~231 to ~0 over the window; `bucket1_count` stays ≈ 0 (bucket1 = newly-downloaded, no change).

- [ ] **Step 2: Play 5 songs manually via `/live` page and evaluate**

The same 5 songs used in Phase 0 Task 0.6. For each: switch OBS to `sp-live`, click-to-play, observe Resolume for:
- First-line onset matches vocal within 500 ms
- No phantom lines during instrumentals
- Every chorus repeat displays
- No drift at end of song

- [ ] **Step 3: Record results**

Append to `docs/superpowers/specs/2026-04-20-gemini-phase-0-validation-notes.md`:
```markdown
## Phase 1 post-deploy acceptance (24h)

| song | first-line Δ | drift@50% | drift@90% | phantoms | missed repeats | pass? |
| ...  | ...          | ...       | ...       | ...      | ...            | ...   |
```

- [ ] **Step 4: Open PR and report results**

```bash
gh pr create --base main --head dev --title "feat(lyrics): gemini chunked provider (v11)" --body "$(cat docs/superpowers/specs/2026-04-20-gemini-chunked-lyrics-provider-design.md | head -40; echo; echo '## Acceptance'; tail -40 docs/superpowers/specs/2026-04-20-gemini-phase-0-validation-notes.md)"
```

Report PR URL. **Do not merge.** Wait for explicit user "merge it" instruction per airuleset pr-merge-policy.

---

## Verification

After all Phase 1 tasks complete:

1. `cargo fmt --all --check` clean
2. `cargo test -p sp-server --lib lyrics` all pass
3. CI green on dev branch
4. Deploy job succeeded to win-resolume
5. `LYRICS_PIPELINE_VERSION` shows 11 on the deployed instance
6. Catalog reprocess drained (stale → 0)
7. 5-song acceptance table filled, all pass the success-criteria checks
8. PR opened from dev → main, green

---

## Notes on execution

- **Phase 0 and Phase 1 are sequential.** Do not start Phase 1 tasks until Phase 0's Task 0.6 exits with "all 5 songs pass the success criteria".
- **Subagent-driven development** is the recommended execution mode. Each Phase 1 task is sized for one subagent round-trip (write test → verify fail → implement → verify pass → fmt → commit).
- **Phase 0 tasks do not use subagents** — they're interactive (user must evaluate Resolume playback between iterations).
- Each commit is a single-step change per airuleset commit conventions. No squashes.
- Never merge without explicit user instruction.
