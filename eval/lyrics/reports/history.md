# Lyrics Eval Run History

Append-only one-line summary of every `/lyrics-eval` run. Newest at the bottom.

| Date | Backend | Revision | Mean Score | Wall Pass | Note |
|------|---------|----------|------------|-----------|------|
| 2026-05-19 | whisperx-large-v3 | r1 | 4.4 | 1/5 | initial baseline; 5-of-24 fixtures, `2026-05-19-whisperx-large-v3.md`. NOTE: scored before gold re-anchor. |
| 2026-05-19 | gemini-3-1-flash-lite | r1 | 5.0 | 0/5 | already-known-tech, kept for reference. `2026-05-19-gemini-3-1-flash-lite.md`. NOTE: scored before gold re-anchor. |
| 2026-05-19 | nemotron-3-nano-omni | r1 | DROPPED | n/a | audio_tokens=0 every call. Wrapper deleted. |
| 2026-05-19 | mimo-v2-5 | r1 | DROPPED | n/a | reasoning model; 2 of 5 longest fixtures emitted empty content. Wrapper deleted. |
| 2026-05-19 | mimo-v2-omni | r1 | DROPPED | 0/5 | systematic +400-4030 ms late-timing; one prompt-tune shot with 3 variants all failed wall gate. Wrapper deleted. |
| 2026-05-19 | (manifest re-anchor) | - | - | - | 3 of 5 LRClib gold fixtures had systematic late-bias detected by whisperx + Gemini consensus: `s8o2YuTBYk4` +473ms, `tCivrrU4SSM` +1137ms, `YbGFYaA0SbY` +275ms. See `feedback_verify_gold_reference.md`. |
| 2026-05-19 | assemblyai-universal-3-pro | r1 | 6.6 | 4/5 | **First backend to pass the wall-gate timing**. Median offsets +5/+319/-29/+118/-119 ms across fixtures (4 of 5 inside ±400). Coverage 37-70% — line-splitter merges adjacent gold lines; tightenable. ZERO hallucination clusters. Multi_language category 8/10 (whisperx 3, gemini 4). See `2026-05-19-assemblyai-universal-3-pro.md`. |
