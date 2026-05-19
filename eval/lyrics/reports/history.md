# Lyrics Eval Run History

Append-only one-line summary of every `/lyrics-eval` run. Newest at the bottom.

| Date | Backend | Revision | Mean Score | Wall Pass | Note |
|------|---------|----------|------------|-----------|------|
| 2026-05-19 | whisperx-large-v3 | r1 | 4.4 | 1/5 | initial baseline; 5-of-24 fixtures, see `2026-05-19-whisperx-large-v3.md` |
| 2026-05-19 | gemini-3-1-flash-lite | r1 | 5.0 | 0/5 | first OpenRouter candidate; +17%/+34% coverage on whisperx-weak categories but +1.2s late timing on multi-language; whisperx still wins clean_pop. `2026-05-19-gemini-3-1-flash-lite.md` |
| 2026-05-19 | mimo-v2-omni | r1 | 5.2 | 0/5 | Xiaomi audio-LLM via OpenRouter. Dominates reverb_heavy (94% cov, score 7), instrumental_breaks (80%, score 6), clean_pop (93% cov but +3s late). Systematic late-timing offset across all fixtures — fixable with per-category offset calibration or prompt tune. `2026-05-19-mimo-v2-omni.md` |
| 2026-05-19 | mimo-v2-5 | r1 | INCOMPLETE | n/a | reasoning model; emitted full transcripts on 3 of 5 pilot fixtures, other 2 (longest songs) burned 8192 tokens on reasoning_tokens=8191 with empty content. Wrapper kept; use V2-Omni instead for transcription. |
| 2026-05-19 | nemotron-3-nano-omni | r1 | FAILED | n/a | `nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free` did not ingest audio (audio_tokens=0 every call). Wrapper kept for when upstream audio routing is fixed. |
