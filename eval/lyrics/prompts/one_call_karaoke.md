# One-Call Karaoke Prompt — Revision 2

System prompt for the one-call audio-LLM lyrics backend
(`backends/gemini38_flash.py`, #144). The backend loads it at runtime via
`load_prompt()`; only the text between the PROMPT-START / PROMPT-END marker
lines below (each marker alone on its own line) is sent as
`system_instruction` — this header is never sent.

The backend also enforces the output shape with the SDK's structured output
(`response_mime_type=application/json` + `response_json_schema`,
`{lines:[{text,start_ms,end_ms,text_sk}]}`), so the prompt describes WHAT to
produce, not JSON syntax. The same prompt serves both arms: `whole` (the whole
vocal track) and `win60` (60 s clips) — "the audio" is always the clip the
model received, and the backend adds each clip's offset afterwards.

<!-- PROMPT-START -->
You receive the isolated lead-vocal audio of a worship song (or a clip of one).
For a church LED wall, produce the sung lyrics line by line.

For every sung line:
- `text`: exactly what is sung, in the song's own language (English or
  Spanish). Do not translate, correct or paraphrase it. Write only what you can
  hear — never invent lyrics. Repeats and ad-libs you clearly hear are lines
  too. One line per sung phrase, the way a published lyrics sheet breaks it.
- `start_ms` / `end_ms`: when the first sung syllable of the line begins and
  the last one ends, in milliseconds from the start of THIS audio. Measure them
  from the audio itself. Leave instrumental passages out.
- `text_sk`: a natural, singable, versed Slovak worship rendering of the line —
  how a Slovak worship team would sing it, faithful to the meaning, familiar
  worship vocabulary, not a literal word-for-word gloss.

Return the lines in the order they are sung.
<!-- PROMPT-END -->

## Revision history

- Revision 1 (2026-08-05 north-star spike, `gemini36_flash.py` /
  `gemini31_pro.py`, both since deleted): 32-char line cap, optional per-word
  `words` array, explicit STRICT-JSON instructions. **Never actually sent:**
  the header named both markers inline, the unanchored loader regex matched
  those mentions first, and the spike's `system_instruction` was the 5
  characters "` / `" (found 2026-09-23, #144) — the 7.3 % gold <= 400 ms of
  that spike measured the model with NO instructions beyond the schema.
- Revision 2 (2026-09-23, #144): one model, one call, models used as designed —
  the structured-output schema carries the JSON shape; per-word timing dropped
  (line-level only, v18 rule); the line break follows the sung phrase (the gold
  is lyrics-sheet line-synced); the song's own language is kept in `text`;
  `text_sk` asks for versed, singable Slovak; timestamps are relative to the
  audio the model received, so the same prompt serves the `win60` clips.
