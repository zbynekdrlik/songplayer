# Lyrics Eval Run History

Append-only one-line summary of every `/lyrics-eval` run. Newest at the bottom.

| Date | Backend | Revision | Mean Score | Wall Pass | Note |
|------|---------|----------|------------|-----------|------|
| 2026-05-19 | whisperx-large-v3 | r1 | 4.4 | 1/5 | initial baseline; 5-of-24 fixtures, `2026-05-19-whisperx-large-v3.md`. NOTE: scored before gold re-anchor; per-fixture timing-offset values pre-shift. |
| 2026-05-19 | gemini-3-1-flash-lite | r1 | 5.0 | 0/5 | already-known-tech, kept for reference. `2026-05-19-gemini-3-1-flash-lite.md`. NOTE: scored before gold re-anchor. |
| 2026-05-19 | nemotron-3-nano-omni | r1 | DROPPED | n/a | `nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free` did not ingest audio (audio_tokens=0 every call). Wrapper deleted. |
| 2026-05-19 | mimo-v2-5 | r1 | DROPPED | n/a | reasoning model; 2 of 5 fixtures burned 8192 tokens on reasoning_tokens=8191 with empty content. Wrapper deleted. |
| 2026-05-19 | mimo-v2-omni | r1 | DROPPED | 0/5 | systematic +400ms..+4030ms late-timing offset. One prompt-tune attempt with 3 variants (original / vocal-onset / calibration-anchor); best variant still landed at -970ms median absolute, all 3 outside ±400ms wall gate. Per `feedback_timing_is_hard_gate.md` candidates with intrinsic timing breakage are rejected, not promoted. Wrapper deleted. |
| 2026-05-19 | (manifest re-anchor) | - | - | - | 3 of 5 LRClib gold fixtures had systematic late-bias detected by whisperx + Gemini multi-backend consensus: `s8o2YuTBYk4` shifted +473ms, `tCivrrU4SSM` shifted +1137ms, `YbGFYaA0SbY` shifted +275ms. See `feedback_verify_gold_reference.md`. Future eval runs use the corrected gold. |
