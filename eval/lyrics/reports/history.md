# Lyrics Eval Run History

Append-only one-line summary of every `/lyrics-eval` run. Newest at the bottom.

| Date | Backend | Revision | Mean Score | Wall Pass | Note |
|------|---------|----------|------------|-----------|------|
| 2026-05-19 | whisperx-large-v3 | r1 | 4.4 | 1/5 | initial baseline; pre-shift gold. `2026-05-19-whisperx-large-v3.md` |
| 2026-05-19 | gemini-3-1-flash-lite | r1 | 5.0 | 0/5 | already-known-tech, kept for reference. `2026-05-19-gemini-3-1-flash-lite.md` |
| 2026-05-19 | nemotron-3-nano-omni | r1 | DROPPED | n/a | audio_tokens=0 every call. Wrapper deleted. |
| 2026-05-19 | mimo-v2-5 | r1 | DROPPED | n/a | reasoning model; 2 fixtures emitted empty content. Wrapper deleted. |
| 2026-05-19 | mimo-v2-omni | r1 | DROPPED | 0/5 | systematic +400-4030ms late-timing; prompt-tune shot failed. Wrapper deleted. |
| 2026-05-19 | (manifest re-anchor) | - | - | - | 3 of 5 LRClib gold timings shifted via multi-backend consensus. `feedback_verify_gold_reference.md`. |
| 2026-05-19 | assemblyai-universal-3-pro | r1 | 6.6 | 4/5 | first wall-gate-passing backend; coverage 37-70% (line-splitter merged adjacent gold lines). `2026-05-19-assemblyai-universal-3-pro.md` |
| 2026-05-19 | assemblyai-universal-3-pro | r2 | **7.6** | **5/5** | LINE_GAP_MS 800→400; coverage jumped +11..+34pp every category, timing held inside gate. Multi_language 9/10 (whisperx 3, Gemini 4). Zero hallucinations. New front-runner for production champion. `2026-05-19-assemblyai-universal-3-pro-r2.md` |
