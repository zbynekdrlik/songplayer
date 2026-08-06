# `elevenlabs-fa` partial re-run — 2026-08-06 (8 of 22 fixtures)

**These files are EVIDENCE, not a scoreable backend row. Never pool them
into a table beside the 21-fixture rows in
`reports/2026-08-05-aligner-scores.json`.** The authoritative
`elevenlabs-fa` row is scored from `../raw/` (all 22 fixtures, poisoned
excluded → 21) and nothing here changes it.

## Why this directory exists

Adversarial-review finding #5 established that the committed
`elevenlabs_fa.py` could not have produced the committed `../raw/*.json`:
those artifacts carry `metadata.runtime_sec` / `n_characters_returned`,
which the committed `emit_result` never wrote (no `import time`, never
read `payload["characters"]`). Commit `d42264e` restored the
instrumentation; the re-run below was the verification that the restored
script reproduces the committed artifacts.

The re-run **stopped at 8 of 22 fixtures** — the ElevenLabs account's
free-tier character quota (10,000 chars/month) was fully exhausted
mid-run (`GET /v1/user/subscription` → `character_count=10000`,
`character_limit=10000`, reset `2026-09-05T07:45:18Z`). The remaining 14
fixtures returned HTTP 401 `quota_exceeded` and wrote no output. Tracked
as **#125 (eval: elevenlabs-fa re-run blocked mid-way by exhausted
free-tier quota)**.

Because the quota does not reset until **2026-09-05**, these 8 files are
not reproducible before then — hence committed here rather than left in a
session-scoped scratchpad.

## What they prove

Compared against `../raw/` for the same 8 `video_id`s:

| Check | Result |
|---|---|
| Line `start_ms` / `end_ms` / `text` | **byte-identical, 0 differences across all 8 songs** |
| `n_tokens_sent`, `n_words_returned_raw/_content`, `n_characters_returned` | identical |
| `runtime_sec` | close but not identical (live-network variance); mean over the 8 is **5.74 s** in BOTH sets |
| per-line `words[]` | **present here, absent in `../raw/`** (see below) |

So the published accuracy figures (42.2 % ≤400 ms conditional, 625 ms
median, 72.6 % coverage, 0.0 % untimed) are confirmed **unaffected** by
finding #5 — only the metadata instrumentation was broken, not the
alignment. The `runtime_sec` instrumentation is confirmed reproducible,
though only 8 of the 21 scored fixtures' runtime values have been
re-measured; the other 13 still come from the pre-fix-era script.

## The `words[]` difference is deliberate, and only in `../raw/`

`../README.md` documents that `words[]` was **stripped remotely before
transfer** from every file in `../raw/` to keep the repo diff reasonable
(~87 % of the payload). These re-run files were pulled without that
stripping step, so they retain the full per-line `words[]` array.

That is why `scores.json` reports `fixtures_with_word_timings: 0` for
`elevenlabs-fa`: it is an artifact of the strip, **not** a finding that
this word-level aligner fails to return word timings. These 8 files are
the direct counter-evidence — every line in all 8 carries per-word
`{text, start_ms, end_ms, loss}`.

## Provenance

- Script: `../elevenlabs_fa.py` at commit `d42264e`, verified byte-identical
  on win-resolume by SHA256 before the run.
- Run host: win-resolume, `C:\ProgramData\SongPlayer\eval-run\aligners-11l\`.
- Fixtures completed: `5JW87KKDTcU`, `BpyP4HR8FBQ`, `hk4woCR12MM`,
  `KeZaADiRHVI`, `p74PDWAFk0A`, `s8o2YuTBYk4`, `tCivrrU4SSM`, `xPkg_vW4yE0`.
- Fixtures blocked by quota (14): `Xvm4_fWkXe8`, `cej4vn4sWtE`,
  `JRRbGCyr2Ac`, `bHCW5WMMF28`, `q5m09rqOoxE`, `edZVnKxKEUU`,
  `YbGFYaA0SbY`, `jUnyHptnsRo`, `wAV5fk1o7u4`, `JjgkhHlTROQ`,
  `h-A1Tzkjsi4`, `zVpDFHJtc_U`, `wjJ-izYndWs`, `hSMJa5tImRU`.

A future full re-run (after 2026-09-05, or on an upgraded plan) should
regenerate **all 22** in one pass for single-run provenance and replace
`../raw/` wholesale — at which point this directory can be deleted.
