# Genius+WhisperX Quality Fix — Design

**Goal:** Genius (HTML-scraped) and lrclib-plain (no-timing) text candidates pass through the same Claude cleanup step that the description provider already uses, before reaching `text_reference_merge`. Output quality matches the description+whisperx flow.

**Date:** 2026-05-11
**Status:** Brainstormed, awaiting plan.

---

## Context

Three text providers feed `text_reference_merge::process` as `CandidateText { has_timing: false }`:

| Source | How it's gathered | Cleaned? |
|---|---|---|
| `description` | YouTube description → Claude extraction prompt → JSON `{"lines":[...]}` | **Yes** (Claude) |
| `genius` | HTTP GET genius.com → HTML scrape → strip `[Chorus]` headers + banners | **No** (raw) |
| `lrclib` (plain) | `lrclib.net/api/get` → `plainLyrics` field → parse_plain | **No** (raw) |

Description gets a clean ordered list of sung lines. Genius and lrclib-plain do not. On 2026-05-11 wall-verify of id=233 Saints (planetboom), the genius+whisperx output was unusable: 9 consecutive literal `"It's the power of Jesus"` lines (genius scraped the chorus verbatim), unmatched intro chants (`"Has He changed your life?"`, `"Come on and give Him the glory"`, `"Go 'head testify"`) at lines 0–2 with `match=0`, and Phase 1 nw_dp could only assign one occurrence of repeated text to one ASR position.

Root cause: text_reference_merge expects clean, de-duplicated reference lines. Phase 2 chorus-repeat expander takes ONE chorus line and projects it to N ASR occurrences. When the input already has N literal repeats, the expander cannot recover. Description provider's Claude pass produces the clean form; genius and lrclib-plain skip that pass.

Also discovered during scoping: `gather.rs` marks lrclib as `has_timing: true` unconditionally, but `lrclib::parse_plain` returns lines with `start_ms = 0` / `end_ms = 0`. Plain-text lrclib reaches text_reference_merge under a `has_timing=true` flag, breaking the timed/text dispatch. Fix included.

## Decisions (from brainstorming dialogue)

1. **Direction:** Clean genius via Claude, mirror the description path.
2. **Scope:** Genius AND lrclib-no-timing. Description unchanged.
3. **Method:** Full re-extraction with the existing description prompt (`build_description_extraction_prompt`). Treat genius/lrclib raw text as a "description blob".
4. **Failure mode:** Claude unavailable / refusal / parse error / `{"lines": null}` → fail the whole song; worker re-picks when Claude recovers. NEVER ship raw genius or raw lrclib-plain.
5. **Placement:** Reuse `description_provider`; expose a shared `clean_lyrics_via_claude` function.
6. **No `LYRICS_PIPELINE_VERSION` bump.** Targeted manual reprocess.

## Architecture

### Change 1 — `description_provider::clean_lyrics_via_claude`

Extract the Claude-call-and-parse path out of `fetch_description_lyrics` so it is reusable by any caller that already has a raw lyrics blob (genius, lrclib).

```rust
/// Claude-clean a raw lyrics blob. Reuses the description-extraction prompt.
/// Reads / writes the cleaned-lines cache at `cache_path`.
///
/// Returns:
/// - `Ok(Some(lines))` — Claude produced clean lines.
/// - `Ok(None)`        — Claude returned `{"lines": null}` (refusal or "no lyrics found").
/// - `Err(_)`          — Claude error, JSON parse error, IO error.
///
/// Caller policy decides whether `Ok(None)` is fatal:
/// - description: `Ok(None)` is normal (description had no lyrics → fall through to other candidates).
/// - genius / lrclib-plain: `Ok(None)` is fatal (input was lyrics; Claude refusing means we have no clean candidate).
pub async fn clean_lyrics_via_claude(
    ai: &AiClient,
    title: &str,
    artist: &str,
    raw_blob: &str,
    cache_path: &Path,
) -> Result<Option<Vec<String>>>
```

Internally:
- Read `cache_path` first: if cached, return cached `Option<Vec<String>>`.
- Otherwise build prompt via existing `build_description_extraction_prompt(title, artist, raw_blob)`.
- Call `ai.chat(system, user)`.
- Parse via existing `parse_claude_response`.
- Write result to `cache_path`.
- Return the parsed value.

`fetch_description_lyrics` is refactored to call `clean_lyrics_via_claude(ai, title, artist, &description, cache_dir/{id}_description_lyrics.json)`. Behavior unchanged for the description path.

### Change 2 — `gather.rs` genius branch

Before:
```rust
if let Some(t) = &genius_track {
    candidate_texts.push(CandidateText {
        source: "genius".into(),
        lines: t.lines.iter().map(|l| l.en.clone()).collect(),
        has_timing: false,
        line_timings: None,
    });
}
```

After:
```rust
if let Some(t) = &genius_track {
    let Some(ai) = ai_client else {
        anyhow::bail!("genius candidate present but no AI client; cannot clean lyrics");
    };
    let raw_blob: String = t.lines.iter().map(|l| l.en.as_str()).collect::<Vec<_>>().join("\n");
    let cache_path = cache_dir.join(format!("{youtube_id}_genius_cleaned.json"));
    match clean_lyrics_via_claude(ai, &row.song, &row.artist, &raw_blob, &cache_path).await {
        Ok(Some(cleaned)) if !cleaned.is_empty() => {
            candidate_texts.push(CandidateText {
                source: "genius".into(),
                lines: cleaned,
                has_timing: false,
                line_timings: None,
            });
        }
        Ok(_) => anyhow::bail!("genius cleanup returned no lyrics for {youtube_id}"),
        Err(e) => anyhow::bail!("genius cleanup failed for {youtube_id}: {e}"),
    }
}
```

### Change 3 — `gather.rs` lrclib branch (bug fix + cleanup)

Before (current — bug: `has_timing: true` for plain mode):
```rust
if let Some(t) = &lrclib_track {
    candidate_texts.push(CandidateText {
        source: "lrclib".into(),
        lines: t.lines.iter().map(|l| l.en.clone()).collect(),
        has_timing: true,
        line_timings: Some(t.lines.iter().map(|l| (l.start_ms, l.end_ms)).collect()),
    });
}
```

After:
```rust
if let Some(t) = &lrclib_track {
    let real_timing = t.lines.iter().any(|l| l.end_ms > 0);
    if real_timing {
        // Synced LRC — keep timing as-is.
        candidate_texts.push(CandidateText {
            source: "lrclib".into(),
            lines: t.lines.iter().map(|l| l.en.clone()).collect(),
            has_timing: true,
            line_timings: Some(t.lines.iter().map(|l| (l.start_ms, l.end_ms)).collect()),
        });
    } else {
        // Plain lrclib — same cleanup path as genius.
        let Some(ai) = ai_client else {
            anyhow::bail!("lrclib-plain candidate present but no AI client; cannot clean lyrics");
        };
        let raw_blob: String = t.lines.iter().map(|l| l.en.as_str()).collect::<Vec<_>>().join("\n");
        let cache_path = cache_dir.join(format!("{youtube_id}_lrclib_cleaned.json"));
        match clean_lyrics_via_claude(ai, &row.song, &row.artist, &raw_blob, &cache_path).await {
            Ok(Some(cleaned)) if !cleaned.is_empty() => {
                candidate_texts.push(CandidateText {
                    source: "lrclib".into(),
                    lines: cleaned,
                    has_timing: false,
                    line_timings: None,
                });
            }
            Ok(_) => anyhow::bail!("lrclib-plain cleanup returned no lyrics for {youtube_id}"),
            Err(e) => anyhow::bail!("lrclib-plain cleanup failed for {youtube_id}: {e}"),
        }
    }
}
```

## Data flow

```
genius HTML scrape
  → strip_html_preserving_breaks
  → is_section_label / is_genius_banner filtering   (in genius.rs)
  → LyricsTrack { lines, source: "genius" }
  → gather.rs: join lines → blob
  → clean_lyrics_via_claude(blob)                   (NEW Claude pass)
  → cleaned lines
  → CandidateText { source: "genius", has_timing: false }
  → best_authoritative_candidate
  → text_reference_merge::process

lrclib.net plain_lyrics
  → parse_plain → LyricsTrack { lines, all timing=0 }
  → gather.rs: detect plain via end_ms==0 check
  → join lines → blob
  → clean_lyrics_via_claude(blob)                   (NEW Claude pass)
  → cleaned lines
  → CandidateText { source: "lrclib", has_timing: false }
  → best_authoritative_candidate
  → text_reference_merge::process

description (existing — unchanged behavior)
  → fetch_description_lyrics (now internally calls clean_lyrics_via_claude)
  → cleaned lines
  → CandidateText { source: "description", has_timing: false }
```

## Cache files

| Path | Purpose | Existing? |
|---|---|---|
| `{youtube_id}_description_lyrics.json` | description Claude-cleaned output | yes (unchanged) |
| `{youtube_id}_genius_cleaned.json` | genius Claude-cleaned output | **new** |
| `{youtube_id}_lrclib_cleaned.json` | lrclib-plain Claude-cleaned output | **new** |

JSON format identical to existing description cache: `{"lines": [...]}` or `{"lines": null}`.

Cache layout integrated with `self_heal_cache` in `sp_server::startup` requires no change — orphan-half detection only inspects `*_video.mp4` / `*_audio.flac` pairs; auxiliary JSON sidecars are left alone.

## Failure modes (explicit)

| Failure | Caller behavior |
|---|---|
| Claude returns HTTP error / timeout | `gather()` bails with `anyhow::Error`. Worker logs, marks row, retries on next bucket pickup. |
| Claude returns body that fails `parse_claude_response` | Same as above. |
| Claude returns `{"lines": null}` for genius/lrclib-plain | `gather()` bails. Worker retries later. |
| Cache file exists but contains malformed JSON | `clean_lyrics_via_claude` returns `Err`; same as above. |
| AI client `None` (e.g. no API key configured) | `gather()` bails with explicit message. |
| Genius returned 0 lines (no scrape hit) | No genius branch taken — pre-existing behavior. |
| lrclib returned 404 | No lrclib branch taken — pre-existing behavior. |

The orchestrator's existing `text_reference_merge` → `whisperx raw line-split` fallback (orchestrator.rs:335) does NOT catch a gather-level bail. Gather failure propagates up to `worker::process_song`, which logs and clears `manual_priority` is unchanged; `fetch_bucket_manual` re-includes the row on next tick if `lyrics_pipeline_version < current_version` OR source NULL. For a Claude-limit transient failure, the row keeps its old `lyrics_source` and is not re-tried via the manual bucket until `manual_priority` is set again — that's acceptable: when the user retriggers reprocess, it picks up.

(Open follow-up — possibly worth a separate issue: should `manual_priority` survive a gather error so the row auto-retries when Claude recovers? Out of scope for this design.)

## Pipeline version

`LYRICS_PIPELINE_VERSION` stays at 20. No bump. Per `feedback_no_bump_until_proven.md` and `feedback_pipeline_version_approval.md`. Verify on individual songs via `POST /api/v1/lyrics/reprocess` with `video_ids`.

## Tests (7)

Unit tests in `description_provider.rs` (or sibling `_tests.rs`):

1. `clean_lyrics_via_claude_returns_parsed_lines` — mock AI returns `{"lines":["a","b"]}`; assert `Ok(Some(vec!["a","b"]))` and cache file written.
2. `clean_lyrics_via_claude_uses_cache_on_second_call` — first call writes cache; second call with mock that would panic-if-invoked returns cached value.
3. `clean_lyrics_via_claude_returns_err_on_claude_error` — mock AI returns `Err`; assert `Err` propagated, no cache write.
4. `clean_lyrics_via_claude_returns_none_when_claude_says_null` — mock returns `{"lines":null}`; assert `Ok(None)` and cache file contains `{"lines":null}`.

Integration tests in `gather.rs` sibling tests:

5. `gather_genius_runs_cleanup_and_pushes_cleaned_candidate` — fake genius HTML hit + mock AI returns clean JSON; assert pushed candidate has `source="genius"`, `has_timing=false`, lines from mock.
6. `gather_genius_fails_song_when_cleanup_returns_err` — fake genius hit + mock AI returns Err; assert `gather` returns `Err`.
7. `gather_lrclib_plain_uses_cleanup_synced_uses_direct_path` — two sub-cases:
   - lrclib_track with `end_ms > 0` on some line → pushed as `has_timing=true`, no Claude call.
   - lrclib_track with all `end_ms == 0` → cleanup called; pushed as `has_timing=false`.

## Files affected (~4)

| File | Change | LOC delta |
|---|---|---|
| `crates/sp-server/src/lyrics/description_provider.rs` | Add `pub async fn clean_lyrics_via_claude`; refactor `fetch_description_lyrics` to call it. | +40 |
| `crates/sp-server/src/lyrics/gather.rs` | Genius branch + lrclib branch rewrites. | +30 |
| `crates/sp-server/src/lyrics/description_provider_tests.rs` (new or existing sibling) | 4 new unit tests. | +90 |
| `crates/sp-server/src/lyrics/gather_tests.rs` (existing sibling) | 3 new integration tests. | +120 |

All four files stay under the 1000-line cap.

## Non-goals (explicitly excluded)

- No change to `text_reference_merge::process` itself. Phase 1 nw_dp / Phase 2 chorus-repeat / Phase 5 stay as-is — they work correctly on clean input.
- No change to source priority ranking (`claude_merge::priority_with_timing`). description=3 > lrclib=2 > genius=1 ordering is correct; this design fixes WHAT genius/lrclib emit, not how they rank.
- No `LYRICS_PIPELINE_VERSION` bump. Per feedback memory.
- No Gemini fallback for cleanup. Translation/cleanup is Claude-only per `feedback_claude_only_translation.md` and `feedback_chunk_success_required.md` (fail-and-retry beats degraded output).
- No new "is line actually sung" filter. The Claude prompt already drops non-lyric content; rely on it.

## Verification plan

1. Implement, merge, deploy via CI.
2. Manual-priority reprocess id=233 Saints. Inspect `BW_vUblj_RA_genius_cleaned.json` — should show ~15-20 deduped lines (down from 37 raw genius lines).
3. Inspect `BW_vUblj_RA_descmerge_audit.json` — Phase 1 emits should have far fewer `match=0` lines; chorus expansion in Phase 2 should produce coherent timing.
4. Wall-verify on sp-live: play Saints, check karaoke rendering line by line over full 3:39.
5. If wall is correct, sweep other genius-sourced songs in active playlists (manual reprocess in small batches per `feedback_song_by_song_iteration.md`).
