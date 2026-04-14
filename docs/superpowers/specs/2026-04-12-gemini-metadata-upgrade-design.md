# Gemini Metadata Upgrade — Design Spec

## Problem

ALL 227 videos in the production DB have `gemini_failed=1`. The Gemini metadata extraction has never succeeded for any video in the Rust port. Root causes:

1. **Wrong model**: `gemini_model` setting is `gemini-2.0-flash` (old, restrictive). The Python code used `gemini-2.5-flash` successfully.
2. **Weak prompt**: The Rust prompt is 15 lines with 2 generic examples. The Python prompt is 74 lines with 7 worship-specific examples, explicit rules for medleys, album names, and bracket handling.
3. **Regex parser bugs**: The fallback parser produces 68% incorrect results — swaps song/artist, truncates at parentheses, picks album names as artist, fails on multi-pipe and `//`-delimited titles.

## Solution

### 1. Upgrade default Gemini model to `gemini-2.5-flash`

Change the default model from `gemini-2.0-flash` to `gemini-2.5-flash` in the server startup code where `GeminiProvider` is constructed. The `gemini_model` DB setting continues to override this default.

Free tier: 10 RPM, 250 RPD — sufficient for 227 videos in one batch.

### 2. Port the full Python prompt to Rust

Replace the 15-line prompt in `gemini.rs::build_request_body()` with the full Python prompt from `gemini_metadata.py`. Key additions:

- 8 numbered rules (worship music artist identification, no album names, medley handling, slash titles, etc.)
- 7 detailed examples covering pipe-separated, quote-wrapped, medley, slash-in-title patterns
- Explicit "CRITICAL: ONLY valid JSON" instruction
- System instruction matching Python's verbose version

### 3. Shorten artist names using initials

For multi-word artist names where the first/middle names are personal names (not band names), abbreviate to initials. This keeps the title overlay compact in Resolume.

**Rule:** If the artist is a person (not a band/group), abbreviate all names except the last to initials. Band/group names are never abbreviated.

Examples:
- `Martin W Smith` → `M. W. Smith`
- `Michael Bethany` → `M. Bethany`
- `Pat Barrett` → `P. Barrett`
- `Chris Tomlin` → `C. Tomlin`
- `Jenn Johnson` → `J. Johnson`
- `SEU Worship, Roosevelt Stewart, Grace Shuffitt` → `SEU Worship, R. Stewart, G. Shuffitt`

Not abbreviated (bands/groups):
- `Elevation Worship` → stays `Elevation Worship`
- `Planetshakers` → stays `Planetshakers`
- `Maverick City Music` → stays `Maverick City Music`
- `VOUS Worship` → stays `VOUS Worship`

**Implementation:** Add this as a Gemini prompt rule ("Abbreviate personal first/middle names to initials, keep last name full. Never abbreviate band or group names.") AND as a post-processing step in `gemini.rs::parse_response()` or a new `shorten_artist()` function applied after extraction. The prompt handles it for Gemini results; the post-processor handles regex fallback results.

The shortening function uses a heuristic: if the name has 2-3 space-separated words AND none of the words are common band indicators (Worship, Music, Church, Choir, Band, Team, etc.), treat it as a personal name and abbreviate all but the last word.

### 4. Fix regex parser fallback

Fix the systematic bugs in `parser.rs` that produce wrong results when Gemini is unavailable:

**Bug A — Artist truncated at `(`**: The PIPE_RE stops the artist capture at `(?:Official|Music|...)` but doesn't handle `(feat. ...)` or `(Live)` at the end of the artist segment. The captured artist includes a trailing `(`. Fix: strip trailing `(` from the artist capture, or apply `clean_song_title` to the artist field too.

**Bug B — Multi-pipe titles pick middle segment as artist**: Titles like `"Song | Album Name | Artist Official Music Video"` — the regex only sees the first `|` and captures `Album Name | Artist Official Music Video` as the artist. Fix: for titles with 3+ pipe segments, take the LAST segment as the artist candidate.

**Bug C — `//` and `||` delimiters not handled**: Titles like `"Song // Artist // Session"` or `"Song || Event"` use double delimiters the parser doesn't recognize. Fix: normalize `//` and `||` to `|` before parsing.

**Bug D — Em-dash `—` not handled**: Titles like `"Song — Artist — Event"` use em-dashes. Fix: normalize `—` and `–` to `-` before parsing.

### 5. Update the `gemini_model` setting in production DB

The migration or startup code should update the DB setting from `gemini-2.0-flash` to `gemini-2.5-flash` if it still has the old value.

### 6. Trigger reprocess of all failed videos

After deployment, the reprocess worker will automatically pick up all 227 `gemini_failed=1` videos. With the correct model and prompt, they should succeed.

## Files to modify

| File | Change |
|------|--------|
| `crates/sp-server/src/metadata/gemini.rs` | Port Python prompt, update system instruction |
| `crates/sp-server/src/metadata/parser.rs` | Fix bugs A-D in regex parser |
| `crates/sp-server/src/lib.rs` | Change default model to `gemini-2.5-flash` |
| `crates/sp-server/src/metadata/mod.rs` | No changes (provider chain is fine) |

## Testing

- Unit tests for each parser bug fix with real titles from the production DB
- Unit tests for the new prompt format (verify JSON structure)
- Wiremock test for Gemini response parsing (already exists, extend with worship examples)
- E2E post-deploy: verify at least one video loses `gemini_failed` flag after reprocess cycle

## Out of scope

- Multi-provider fallback (Groq/Llama) — Gemini's Google Search grounding is critical for worship music identification; other models lack this
- Paid tier — 250 RPD free tier is sufficient for current playlist sizes
