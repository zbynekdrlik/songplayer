# YouTube Description Lyrics Provider — Design Spec

**Date:** 2026-04-18
**Status:** Approved for implementation
**Blocks:** PR #38 merge to `main` (merge this first, confirm quality recovery on production catalog, then merge #38).

## Goal

Add a fourth candidate-text source to the lyrics ensemble pipeline: YouTube video descriptions. Many worship-music videos paste full lyrics into the description; today none of those sources reach the text-merge step. The new provider fetches the raw description via `yt-dlp`, pipes it to Claude with a narrow extraction prompt, and emits a `CandidateText { source: "description", has_timing: false }` into the existing `gather_sources` flow. Zero orchestrator changes — the existing Claude text-merge already reconciles N candidate texts into one reference.

## Why this PR exists

PR #38 (ensemble lyrics pipeline) regressed catalog quality from **0.631 → 0.524 (−17%)** measured by `scripts/measure_lyrics_quality.py`. Root cause: `gather_sources` fetches only `yt_subs` (rare), `lrclib` (patchy worship coverage), and `autosub` (timing only, ASR mistakes). 79% of songs fall to single-provider `ensemble:qwen3` with unreliable reference text from Qwen3's own transcription.

Descriptions are the biggest untapped source on the worship catalog. Adding them should recover and exceed the pre-#38 baseline.

## Non-goals

- Heuristic/regex lyrics extraction. The user was explicit: "one simple claude call, no complicated code". Extraction is a single Claude call. No skip-word lists, no social-link regexes, no artist-boilerplate templates.
- Section-marker preservation. Claude strips `Verse 1:` / `Chorus:` / etc. and keeps only the lyric lines.
- Language detection. Claude returns whatever the description contains, in reading order. `text_merge.rs` downstream handles reconciliation across languages.
- A separate HTTP endpoint or dashboard surface. Description lyrics are plumbed into `candidate_texts` and consumed by the existing text_merge / orchestrator path. Nothing user-visible except the resulting improved quality.

## Architecture

### New module: `crates/sp-server/src/lyrics/description_provider.rs`

Single public async function:

```rust
pub async fn fetch_description_lyrics(
    ai: &AiClient,
    ytdlp: &Path,
    youtube_id: &str,
    cache_dir: &Path,
    title: &str,
    artist: &str,
) -> Result<Option<Vec<String>>>
```

Returns:
- `Ok(Some(lines))` — description had extractable lyrics; `lines` is the per-line string vector for a `CandidateText`.
- `Ok(None)` — either no description, no lyrics in the description, or Claude refused / errored. Pipeline proceeds without this candidate.
- `Err(e)` — reserved for catastrophic failures the caller should log and treat as `None` (kept `Result<Option<_>>` for callsite symmetry with `fetch_autosub`).

### Three-step data flow, each step disk-cached

Step 1 — fetch raw description:
- Cache path: `{cache_dir}/{youtube_id}_description.txt`
- If file exists → read and skip yt-dlp.
- Else → spawn `yt-dlp --skip-download --print "%(description)s" https://www.youtube.com/watch?v={youtube_id}`. Write stdout to cache. On yt-dlp failure (HTTP 429, unavailable, network) warn + return `Ok(None)`; do NOT cache a failure so the next reprocess retries.
- On Windows, include `CREATE_NO_WINDOW` flag (same pattern as `fetch_autosub`).

Step 2 — extract lyrics via Claude:
- Cache path: `{cache_dir}/{youtube_id}_description_lyrics.json`
- Shape: `{"lines": ["...", "..."]}` when lyrics present, `{"lines": null}` when none.
- If file exists → parse and return (null → `Ok(None)`, array → `Ok(Some(lines))`). No Claude call.
- Else → call `ai.chat_with_timeout(system, user, 180)` with the prompt below.
- On malformed Claude response (non-JSON, missing `lines` key, wrong type) warn + return `Ok(None)`; do NOT cache bad output.
- On successful parse → write cache, return.

Step 3 — return — no post-processing needed; Claude already returns cleaned lines.

### Claude prompt (narrow + deterministic)

System prompt:
```
You are a lyrics extractor. Given a YouTube video description, return the song's lyrics
as a JSON object with exactly one key, "lines", whose value is either:
  - an array of strings (one per lyric line, in reading order, in the song's original language), OR
  - null, when the description contains NO lyrics.

Rules:
1. Strip section markers ("Verse 1:", "Chorus:", "Bridge:", etc.), keep the line text.
2. Preserve non-English lyrics as-is. Do NOT translate.
3. Ignore: artist bio, social links, streaming/buy links, copyright notices, producer/
   writer credits, album promo, tour dates, comment/like/subscribe prompts.
4. If multiple languages appear (e.g., English + Spanish side-by-side or verse/translation
   blocks), include ALL lines in reading order — downstream reconciliation handles dedupe.
5. Do not fabricate lyrics. If you are not confident the text is the song's lyrics,
   return {"lines": null}.
6. Output ONLY the JSON object. No preamble, no markdown fences, no commentary.
```

User prompt:
```
Video title: {title}
Artist: {artist}

Description:
---
{description}
---
```

Temperature 0.1, max output 2048 tokens. Reuse `AiClient::chat_with_timeout` already added in PR #38.

### Integration point: `gather_sources` in `worker.rs`

Add a 4th concurrent source after autosub. Pseudocode:

```rust
// 4. YouTube description lyrics (LLM-extracted)
let description_lines = match description_provider::fetch_description_lyrics(
    &self.ai_client,
    &self.ytdlp_path,
    &youtube_id,
    &self.cache_dir,
    &row.song,
    &row.artist,
).await {
    Ok(Some(lines)) => Some(lines),
    Ok(None) => None,
    Err(e) => {
        warn!("gather: description fetch error for {youtube_id}: {e}");
        None
    }
};

if let Some(lines) = description_lines {
    candidate_texts.push(CandidateText {
        source: "description".into(),
        lines,
        has_timing: false,
        line_timings: None,
    });
}
```

The existing `candidate_texts.is_empty()` guard at the bottom of `gather_sources` stays — if all four sources return None, we still bail per existing behavior.

### Pipeline version bump

Bump `LYRICS_PIPELINE_VERSION` from **3 → 4** in `crates/sp-server/src/lyrics/mod.rs`. This triggers catalog auto-reprocess via the existing 3-bucket stale queue. Rationale: adding a new candidate source changes merge outputs materially.

Existing cached `_description.txt` and `_description_lyrics.json` files are reused across version bumps (they're keyed by `youtube_id`, not version). Only Claude's merge and orchestrator outputs get re-computed.

### CI quality gate

Currently `measure_lyrics_quality.py` runs as a CI step and uploads a comparison artifact, but doesn't fail the build on regression. Add a new CI step immediately after the existing 30-min snapshot step:

```yaml
- name: Fail merge on quality regression
  run: |
    python - <<'PY'
    import json, sys
    before = json.load(open(r"C:\ProgramData\SongPlayer\baseline_before.json"))["aggregate"]
    after  = json.load(open(r"C:\ProgramData\SongPlayer\measure_after.json"))["aggregate"]
    b, a = before["avg_confidence_mean"], after["avg_confidence_mean"]
    tol = 0.02
    print(f"avg_confidence_mean: {b:.3f} -> {a:.3f} (tolerance {tol})")
    if a < b - tol:
        print(f"REGRESSION: avg_confidence_mean dropped by more than {tol}", file=sys.stderr)
        sys.exit(1)
    PY
```

This closes the green-CI-theater hole that let PR #38 ship with a −17% regression.

**Caveat:** the 30-min snapshot reprocesses only ~6 songs out of 47, so measurement noise is real. The 0.02 tolerance is calibrated to avoid false positives. For this PR's own validation, the acceptance criterion is stricter and uses a **24-48 hour** post-deploy snapshot (below), not the 30-min one.

## Testing

### Unit tests — `description_provider.rs`

Mirror the pattern in `autosub_provider.rs` tests. Use `MockAiClient` already established in PR #38.

1. `fetch_description_lyrics_cached_skips_ytdlp_and_claude` — pre-seed both cache files, pass a mock `AiClient` that panics on call + a mock ytdlp path that doesn't exist; assert lyrics returned from cache.
2. `extracts_lyrics_from_description_with_lyrics_block` — mock `AiClient::chat_with_timeout` returns `{"lines": ["line one", "line two"]}`, assert `Ok(Some(vec!["line one", "line two"]))` and cache file written.
3. `handles_no_lyrics_response` — mock returns `{"lines": null}`, assert `Ok(None)` and null cached for future reprocess fast-path.
4. `handles_malformed_claude_response` — mock returns `not valid json`, assert `Ok(None)` and NO cache file created (so retry happens on next reprocess).
5. `handles_ytdlp_fetch_failure` — mock ytdlp spawn returns non-zero exit, assert `Ok(None)` and NO raw-description cache created (so retry happens).
6. `strips_bom_and_crlf` — raw description contains BOM + Windows line endings; assert passed cleanly to Claude (just string hygiene).

### Integration test — `worker.rs::gather_sources`

`gather_sources_pushes_description_candidate_when_available` — full `gather_sources` run with mock yt-dlp (yt_subs = None, lrclib = None, autosub = None) and mock `AiClient` returning `{"lines": [...]}`; assert `candidate_texts.len() == 1` and the sole entry has `source == "description"`, `has_timing == false`.

### Mutation testing

All test assertions pin behavior; no `mutants::skip` expected on this module except if the cache-file-exists check has multiple equivalent branches. Follow airuleset: if a skip is needed, annotate with one-line justification.

### Playwright E2E

Not required for this PR (no user-visible UI change). The `/lyrics` page already shows the `source` column; after deploy a reviewer will see rows with `source = "description"` or `ensemble:qwen3+...` labels that include description — but no UI code change is needed to render them.

### Measurable-improvement verification (acceptance gate)

**Before merge to main:**

1. Commit PR #42 on `dev`, monitor CI to green.
2. Deploy to win-resolume (self-hosted runner path, same as PR #38).
3. Take catalog snapshot immediately: `python scripts/measure_lyrics_quality.py > /tmp/pre.json` where `avg_confidence_mean` is expected ≈ 0.524.
4. Wait 24-48 hours for the stale bucket to fully drain (227 songs at ~4-5 min/song = ~16-19 hours with one worker; descriptions add a Claude call but are cached after first pass).
5. Take catalog snapshot: `python scripts/measure_lyrics_quality.py > /tmp/post.json`.
6. **Acceptance target: `avg_confidence_mean >= 0.65`** (exceeds pre-PR#38 baseline of 0.631, proving this PR is net positive).
7. If target not met, do NOT merge. Investigate (Claude prompt tweaks, quality-metric recalibration, or text_merge weight tuning) and retry.

## Cost model

Per song, once (first-time processing + first reprocess under v4):
- 1× `yt-dlp` description fetch (~1 sec, local process)
- 1× Claude call for description lyrics extraction (~2-5 sec, 180s timeout)
- Existing: 1× Claude call for text_merge reconciliation (unchanged from PR #38)

Subsequent reprocesses reuse both caches — zero Claude cost.

On 227-song catalog first pass: ~227 Claude calls ≈ tens of dollars of Anthropic API cost. Bounded + one-time.

## File structure

**New files:**
- `crates/sp-server/src/lyrics/description_provider.rs` (~200 LOC including tests)
- `docs/superpowers/plans/2026-04-18-youtube-description-lyrics-provider.md` (written by writing-plans skill next)

**Modified files:**
- `crates/sp-server/src/lyrics/mod.rs` — `pub mod description_provider;`, `LYRICS_PIPELINE_VERSION = 4`, add v4 entry to the version history doc comment.
- `crates/sp-server/src/lyrics/worker.rs::gather_sources` — add 4th source block.
- `.github/workflows/ci.yml` — add regression-fail-gate step after the existing 30-min snapshot.
- `CLAUDE.md` — add v4 entry to the pipeline versioning history.

## Follow-ups (not in this PR)

- **Issue #41 (vocal persistence)** — still the right next architecture improvement after this PR merges. Unblocks cheap reprocess.
- **Issue #36 (generic description/CCLI provider)** — this PR closes the "description" half; CCLI can be added later as a 5th source. Same plumbing, different fetch method.
- **Pinned-comment lyrics** — sometimes fan-posted tracks have lyrics in the pinned comment, not the description. Future 6th source; low priority until measurement shows descriptions aren't enough.
