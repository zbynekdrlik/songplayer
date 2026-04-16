# Phase 2 Auto-Sub Drift Experiment

Validation experiment for issue #29. Decides whether YouTube auto-subtitles carry word-level timestamps accurate enough on sung vocals to skip the Qwen3-ForcedAligner timing stage.

## Methodology

- Auto-subs pulled with `yt-dlp --write-auto-subs --sub-format json3 --sub-langs en --skip-download`.
- Qwen3 reference word timings read from win-resolume's production lyrics cache (`<lyrics-dir>/<video_id>_lyrics.json`); no DB query — the alignment is JSON-on-disk, not a SQLite table.
- Matcher (Option A): sequential forward walk; for each Qwen3 word, search up to 10 auto-sub words ahead for an exact text match after lowercasing + punctuation stripping. No backtrack. Skipped words are reported separately and do NOT pollute the drift distribution.
- Decision rule: per-song RMS drift `< 300 ms` → green, `300–700 ms` → amber, `> 700 ms` → red. Worst per-song bucket sets the project recommendation (one red kills, one amber refines, all green greenlights).

## Per-song results

### Get This Party Started — Planetshakers (`VtHoABitbpw`)

- URL: https://www.youtube.com/watch?v=VtHoABitbpw
- Match rate: **7/214** Qwen3 words attempted (3.3%), 207 skipped (no auto-sub counterpart in window)
- Auto-sub stream: 36 words
- Drift: RMS **12926 ms**, mean 6877 ms, median 139 ms, min -1935 ms, max 27874 ms, p05 -1935 ms, p95 27874 ms
- Bucket: **red**

Histogram (drift in ms, `#` = one Qwen3 word):

```
[-2000, -1000) # (1)
[-1000, -500)   (0)
[-500, -300)    (0)
[-300, -100)    (0)
[-100, 0)       (0)
[0, 100)        (0)
[100, 300)     ### (3)
[300, 500)      (0)
[500, 1000)     (0)
[1000, 2000)    (0)
```

### The Name Above — planetboom (`EOzYwg2Tuw0`)

- URL: https://www.youtube.com/watch?v=EOzYwg2Tuw0
- Match rate: **4/249** Qwen3 words attempted (1.6%), 245 skipped (no auto-sub counterpart in window)
- Auto-sub stream: 64 words
- Drift: RMS **27024 ms**, mean -17931 ms, median -11440 ms, min -50105 ms, max 1260 ms, p05 -50105 ms, p95 1260 ms
- Bucket: **red**

Histogram (drift in ms, `#` = one Qwen3 word):

```
[-2000, -1000)  (0)
[-1000, -500)   (0)
[-500, -300)    (0)
[-300, -100)    (0)
[-100, 0)       (0)
[0, 100)        (0)
[100, 300)      (0)
[300, 500)      (0)
[500, 1000)     (0)
[1000, 2000)   # (1)
```

### There Is A King — Elevation Worship (`NuPP2Kxyo00`)

- URL: https://www.youtube.com/watch?v=NuPP2Kxyo00
- **No data: no auto-subs available**

## Conclusion

| Song | RMS drift | Bucket |
| --- | --- | --- |
| Get This Party Started — Planetshakers | 12926 ms | red |
| The Name Above — planetboom | 27024 ms | red |
| There Is A King — Elevation Worship | n/a | no data (no auto-subs available) |

## Recommendation

**KILL** — auto-sub timing is not accurate enough on sung worship vocals to skip Qwen3. Worst-case song: **Get This Party Started** at 12926 ms RMS. Close issue #29.

## Raw data references

Auto-sub json3 files are pulled into a per-run tmp dir created by `tempfile.mkdtemp(prefix="autosub_drift_")` and are NOT committed to the repo. Re-run the script (see header docstring) to regenerate them. The Qwen3 reference word timings are read directly from the production lyrics cache at `<lyrics-dir>/<video_id>_lyrics.json` on win-resolume; those files are also not committed.
