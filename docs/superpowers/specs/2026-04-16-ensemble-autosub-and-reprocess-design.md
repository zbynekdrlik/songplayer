# Ensemble Alignment: AutoSub Provider + Pipeline Version Tracking + Dashboard

**Issues:** #34 (pipeline version tracking + auto-reprocess), #35 (YouTube auto-sub transfer provider)

**Parent:** #29 (ensemble alignment umbrella)

**Prior PR:** #33 shipped the ensemble infrastructure (`AlignmentProvider` trait, Claude Opus merge layer, Orchestrator). This spec is the first PR to *use* it meaningfully: a second alignment provider + the version tracking + reprocess queue that makes future pipeline improvements actually propagate through the catalog.

---

## Context

The ensemble alignment infrastructure from PR #33 has exactly one real provider (`Qwen3Provider`), so every song takes the single-provider pass-through branch — the LLM merge layer never activates. Until we ship a second provider, the ensemble architecture is theoretical.

The six active playlists (~231 songs) have inconsistent lyrics quality: songs processed pre-#33 have `source=yt_subs+qwen3`, post-#33 have `source=ensemble:qwen3` (same data, different audit log). When we ship new providers or tune prompts, pre-improvement songs stay stuck on their old output. The catalog never converges.

The dashboard currently shows no information about lyrics processing: no queue visibility, no per-song quality data, no manual re-run control. Operators can't see what improved or what's stuck.

## Goals & non-goals

**Goals:**
1. Ship a second alignment provider (`AutoSubProvider`) so the ensemble merge path actually runs for a meaningful subset of the catalog.
2. Dissolve the `if yt_subs: ... elif lrclib: ...` decision tree — every song goes through the same ensemble pipeline with whatever sources it has.
3. Use Claude as the merger at **every** point where multiple source options exist: text-source reconciliation (new) and word-timing merge (existing).
4. Track a single monotonic `LYRICS_PIPELINE_VERSION` so older outputs auto-reprocess when the pipeline improves.
5. Prioritize reprocessing: null-lyrics first, then worst-quality-first.
6. Give the dashboard real visibility into lyrics processing and a way to manually trigger reprocess for a song/playlist.
7. Publish a post-deploy comparison report (avg_confidence, duplicate_start_pct, provider_count) so we can see whether the pipeline actually improved quality on real songs.

**Non-goals:**
1. Additional providers (WhisperX #27, description #36, Gemini audio #37). Each ships in its own PR.
2. Quality-gate rollback automation. If the metric goes negative, a human decides whether to revert `LYRICS_PIPELINE_VERSION`.
3. Cancellation of an in-flight song. Processing is 5 min max; worth the wait.
4. Full 21-hour catalog sweep before merging. Partial +30 min snapshot is sufficient signal for the PR gate.

---

## Architecture

### Per-song pipeline (source-agnostic)

Every song — regardless of which sources are available — flows through the same pipeline:

```
┌─ GATHER (parallel, cheap) ──────────────────────────────────┐
│  yt_subs json3       via yt-dlp (text + line timing)        │
│  autosub json3       via yt-dlp --write-auto-subs            │
│  lrclib              via HTTP  (text + line timing)          │
│  [future]            description, CCLI, Genius, Musixmatch   │
└──────────────────────────────────────────────────────────────┘
                         ↓
┌─ MERGE TEXT (Claude) ───────────────────────────────────────┐
│  0 candidates → bail                                         │
│  1 candidate  → pass-through                                 │
│  2+ candidates → Claude reconciles: spot transcription       │
│                  errors, fix capitalization, pick correct    │
│                  words where sources disagree                │
└──────────────────────────────────────────────────────────────┘
                         ↓
┌─ ALIGN (each provider self-gates via can_provide) ──────────┐
│  Qwen3Provider       (needs reference text + clean vocal)    │
│  AutoSubProvider     (needs autosub json3 + reference text)  │
│  [future]            WhisperX, Gemini audio, ...             │
└──────────────────────────────────────────────────────────────┘
                         ↓
┌─ MERGE TIMINGS (Claude) ────────────────────────────────────┐
│  0 results → bail                                            │
│  1 result  → pass-through                                    │
│  2+ results → Claude weighted median + outlier rejection     │
└──────────────────────────────────────────────────────────────┘
                         ↓
         translate (Claude → Gemini fallback) → persist
```

The current `worker.rs` decision tree (`if yt_subs: ... elif lrclib: ...`) is dissolved. `acquire_lyrics` is replaced by a `gather_sources` function that returns all available `CandidateText`s plus `autosub_json3` in a single `SongContext`. The orchestrator handles every case from there.

### Claude as the merger across sources (two merge points)

**Merge point A — Reference text (new):**
- Input: `Vec<CandidateText>` with `{ source, lines, line_timings }` per candidate.
- Claude prompt: system-neutral software-engineering framing (same pattern as the translator to avoid OAuth cloaking refusals). User message sketch:
  > "I'm building a karaoke subtitle app. I have N candidate lyric texts for the same song, each from a different source with its own transcription errors. Reconcile them into one canonical text. Rules: keep line structure (don't merge or split lines); prefer words that appear in 2+ candidates; fix obvious transcription errors (homophones, capitalization); return JSON `{ lines: [{ text: str, source: str }], ... }` where `source` is the source name most of the line came from. Candidates: [yt_subs] ..., [lrclib] ..., [autosub] ..."
- Output: one canonical text + `source_of_line: Vec<String>` audit data that flows into the audit log.
- Short-circuit: 1 candidate → pass through (no Claude call).

**Merge point B — Word timings (existing, unchanged):**
- Input: `Vec<ProviderResult>`.
- Claude prompt: timing reconciliation with outlier rejection (already in `merge.rs`).
- Output: merged `LyricsTrack` + per-word audit details.
- Short-circuit: 1 provider → pass through (no Claude call).

### The single-provider pass-through still applies

For worship-fast songs where autosub returns only a handful of sparse words, `AutoSubProvider::can_provide` returns false (density gate) and only Qwen3 runs. Merge layer is bypassed. No Claude call is made. Zero added cost on the edge case that the drift experiment identified as catastrophic.

---

## The AutoSub alignment provider

**New file:** `crates/sp-server/src/lyrics/autosub_provider.rs`

**Trait implementation:**

```rust
pub struct AutoSubProvider;

#[async_trait]
impl AlignmentProvider for AutoSubProvider {
    fn name(&self) -> &str { "autosub" }

    async fn can_provide(&self, ctx: &SongContext) -> bool {
        // Path present + file exists + at least 10 words parsed + density >= 0.3 wps
        match ctx.autosub_json3.as_ref() {
            Some(p) if p.exists() => {
                let words = parse_json3_path(p).await.ok().unwrap_or_default();
                let density = words.len() as f32 / (ctx.duration_ms as f32 / 1000.0);
                words.len() >= 10 && density >= 0.3
            }
            _ => false,
        }
    }

    async fn align(&self, ctx: &SongContext) -> Result<ProviderResult> { ... }
}

fn density_gate_confidence(wps: f32) -> f32 {
    if wps >= 1.0 { 0.6 }
    else if wps <= 0.3 { 0.1 } // defensive: can_provide already filters wps < 0.3
    else { 0.1 + (wps - 0.3) / 0.7 * 0.5 }
}
```

**Fetch step** (worker's gather phase): `yt-dlp --write-auto-subs --sub-format json3 --sub-langs en --skip-download -o <tmp>/<id>.%(ext)s <youtube_url>`. Produces `<id>.en.json3` or `<id>.en-orig.json3` (latter when both manual and auto-subs exist). Cleanup after successful processing.

**Matcher algorithm** (ported verbatim from `scripts/experiments/autosub_drift.py`):
- `normalize_word(s)`: lowercase, strip `[^\w]`, drop `[music]` / `>>` / `[applause]` / `[laughter]`, return "" for empties.
- `match_reference_to_autosub(reference_words, autosub_words, window=10)`: sequential forward walk. For each reference word, search up to 10 autosub words ahead for first exact match (after normalization). On match, record autosub start_ms and advance autosub pointer. On miss, skip — leaves autosub pointer untouched.
- Output: per reference word, either `(matched_start_ms, Some)` or `None`.

**Word end_ms assignment:** `end_ms[i] = start_ms[i+1]` (last word uses line `end_ms` from reference text). Autosub json3 gives only start times.

**Output structure:** `ProviderResult` with `lines: Vec<LineTiming>` matching reference-text line structure (not autosub native segmentation). Unmatched reference words are simply not emitted — merge layer fills from other providers.

**Confidence:** per-word confidence = `density_gate_confidence(density)`. Worship-fast songs at density 0.2 wps get 0.1 (and `can_provide` returns false anyway). Slow ballads at 1.0 wps get 0.6, matching Qwen3's base confidence — the merge layer weighs them roughly equal.

---

## Pipeline version tracking (#34)

### Constant and bump criteria

**Location:** `crates/sp-server/src/lyrics/mod.rs`

```rust
/// Monotonic version of the lyrics pipeline output format. Bump when
/// prompts, provider list, merge algorithm, or reference-text selection
/// changes. Every bump triggers auto-reprocess of existing songs.
pub const LYRICS_PIPELINE_VERSION: u32 = 2;
```

**This PR bumps v1 → v2.** Reason: added AutoSubProvider, introduced Claude text-merge step.

**When to bump:**
- Added or removed an `AlignmentProvider` from the worker registration.
- Changed a provider's algorithm (chunking, matcher, density gate thresholds).
- Changed either Claude merge prompt (text or timings).
- Changed reference-text-selection algorithm.

**When NOT to bump:**
- Bug fixes that produce identical output.
- Refactoring, renaming, logging changes.
- UI/dashboard-only changes.
- Performance optimizations with identical output.

**CI enforcement:** none. Rely on reviewer discipline + the bump-history entry in `CLAUDE.md`.

### DB migration V5

```sql
ALTER TABLE videos ADD COLUMN lyrics_pipeline_version INTEGER DEFAULT 0;
ALTER TABLE videos ADD COLUMN lyrics_quality_score REAL;
ALTER TABLE videos ADD COLUMN lyrics_manual_priority INTEGER DEFAULT 0;
```

All three columns default to 0 / NULL. No data rewrite needed. Pre-migration rows have `pipeline_version = 0 < current (2)`, so they route into the stale bucket automatically.

**When each column is written** — at the end of `process_song`, in the same transaction that sets `lyrics_processed = 1`:
- `lyrics_pipeline_version` ← `LYRICS_PIPELINE_VERSION` constant value
- `lyrics_quality_score` ← computed composite (formula below)
- `lyrics_manual_priority` ← `0` (clears manual-queue flag whether it was set or not)

This single transaction is the only write path. On failure, `lyrics_processed` stays 0 and `lyrics_manual_priority` stays at whatever it was — so a failed manual-reprocess stays in bucket 0 and retries on the next worker iteration.

### Reprocess queue (3 buckets)

Worker selects the next video in this order:

```sql
-- Bucket 0: Manual priority
SELECT ... WHERE lyrics_manual_priority = 1 AND is_active = 1
ORDER BY id ASC LIMIT 1;

-- Bucket 1: Null / failed lyrics (new playlist-sync songs + previously failed)
SELECT ... WHERE (lyrics_processed IS NULL OR lyrics_processed = 0)
  AND lyrics_manual_priority = 0 AND is_active = 1
ORDER BY id ASC LIMIT 1;

-- Bucket 2: Stale pipeline version, worst-first
SELECT ... WHERE lyrics_processed = 1
  AND lyrics_pipeline_version < ?current_version
  AND lyrics_manual_priority = 0 AND is_active = 1
ORDER BY lyrics_quality_score ASC NULLS FIRST, id ASC LIMIT 1;
```

**Why `NULLS FIRST`** in bucket 2: songs with no quality data (processed before quality scoring existed) treat as worst — always worth reprocessing.

**Quality score formula** (written at end of `process_song`):
```rust
let quality_score = audit.quality_metrics.avg_confidence
    - (audit.quality_metrics.duplicate_start_pct / 100.0) as f32;
// Higher = better. Range typically 0.0..1.0.
```

**Restart safety:** version + quality both live in the DB. Server crash mid-reprocess resumes from the same prioritized queue on next startup.

**Cost model:** 231 songs × ~5.5 min/song serial = ~21 hours. Runs invisibly in the background. New songs (bucket 1) never wait.

---

## Dashboard: visibility + manual triggers

### New HTTP API endpoints (under `/api/v1/lyrics/`)

| Method | Path | Body | Returns |
|---|---|---|---|
| `GET` | `/queue` | — | `{ bucket0_count, bucket1_count, bucket2_count, processing: ProcessingState?, pipeline_version }` |
| `GET` | `/songs?playlist_id=N` | — | `[{ video_id, title, artist, source, pipeline_version, quality_score, providers, is_stale, status }]` |
| `GET` | `/songs/:video_id` | — | Detailed view: reference text with provenance, providers run (with per-provider output preview), quality metrics, per-word audit (from audit log), final lyrics |
| `POST` | `/reprocess` | `{ video_ids: [...] }` or `{ playlist_id: N }` | `{ queued: N }` — sets `manual_priority = 1` |
| `POST` | `/reprocess-all-stale` | — | `{ queued: N }` — sets `manual_priority = 1` for all bucket-2 songs |
| `POST` | `/clear-manual-queue` | — | `{ cleared: N }` — sets `manual_priority = 0` for all rows |

### WebSocket events (extend existing `ServerMsg` enum)

- `LyricsQueueUpdate { bucket0_count, bucket1_count, bucket2_count, processing: Option<ProcessingState> }` — broadcast every 2s while worker is active, once on idle.
- `LyricsProcessingStage { video_id, stage: "gathering" | "text_merge" | "aligning" | "timing_merge" | "translating" | "persisting", provider?: string }` — broadcast as worker progresses through a song.
- `LyricsCompleted { video_id, source, quality_score, provider_count, duration_ms }` — broadcast on success.

### Leptos page: `/lyrics` dashboard

**Top card — Queue status:**
```
┌─ Lyrics Pipeline ─────────────────────────────┐
│ Currently processing:  "Song Title" — Artist  │
│ Stage: aligning (Qwen3 provider)              │
│                                                │
│ Queue:                                         │
│   Manual:   3                                  │
│   New:      12                                 │
│   Stale:    187   [Reprocess all stale →]     │
│                                                │
│ Pipeline version: 2  (bumped 2026-04-16)      │
└────────────────────────────────────────────────┘
```

**Per-playlist expandable list** (one row per song):
- Status badge: `●` green (fresh) / `●` yellow (stale) / `⚠` amber (low quality) / `✗` red (no lyrics).
- Source chip: `ensemble:qwen3+autosub`, `ensemble:qwen3`, `yt_subs`, etc.
- Quality score (numeric, 0.00..1.00).
- Action buttons: `Details` (opens modal), `Reprocess` (POST to `/reprocess`).

**Per-song detail modal:**
- Reference text (Claude-merged, with per-line provenance: `[yt_subs]`, `[lrclib]`, etc).
- Providers that ran (name, confidence, runtime).
- Per-word merge verdict (for low-confidence words): `qwen3=5230ms, autosub=5180ms → merged=5200ms`.
- Quality metrics breakdown.
- Final lyrics (EN + SK).
- Raw audit log JSON toggle.

**Per-playlist header button:** `Reprocess playlist` → POST `/reprocess` with `{ playlist_id }`.

**Clear-manual-queue button** on Queue card: sets all `manual_priority = 0` in case of operator regret.

### Scope of frontend changes

New page with 4 components:
- `LyricsQueueCard` — queue status, counts, pipeline version, action buttons.
- `LyricsPlaylistSection` — expandable per-playlist song table.
- `LyricsSongRow` — status badge, quality bar, action buttons.
- `LyricsSongDetailModal` — full breakdown.

Shared signals: `lyrics_queue`, `lyrics_songs`, `ws_connected` (existing).

---

## Testing

### Unit tests (fast, CI-gated)

| Area | Tests |
|---|---|
| `autosub::parse_json3` | 3 fixture files: word-level segs, sentence-level segs, mixed; empty events handled |
| `autosub::normalize_word` | strips punctuation, lowercases, drops `[music]` / `>>` / `[applause]` / `[laughter]`; empty for blank |
| `autosub::match_reference_to_autosub` | sequential forward walk; 10-word window boundary; unmatched ref words skipped; autosub pointer advances only on match |
| `autosub::density_gate_confidence` | 0.6 at ≥1.0 wps; 0.1 at ≤0.3 wps; linear between; boundary values |
| `autosub::AutoSubProvider::can_provide` | false when None; false when path doesn't exist; false when <10 words; false when density <0.3; true otherwise |
| `reprocess::select_next_video` | bucket 0 > bucket 1 > bucket 2; bucket 2 orders by `quality ASC NULLS FIRST`; respects `is_active = 1` |
| `db::migrate_v5` | V5 migration idempotent; V5 rollforward from V4 works; columns have correct defaults |
| `api::lyrics::reprocess` | POST with `video_ids` sets `manual_priority = 1`; POST with `playlist_id` sets it for every video in playlist |
| `api::lyrics::queue` | returns correct bucket counts; `processing` field matches worker state |
| `ws::LyricsQueueUpdate` | serde roundtrip |
| `text_merge::claude_merge_texts` | wiremock: 1 candidate pass-through; 2 candidates produce single reconciled text; JSON parse handles preamble before fences |

### Mutation testing

All new functions in `autosub_provider.rs`, `text_merge.rs`, `reprocess.rs`, and the new API handlers get `cargo-mutants`-clean coverage. Zero surviving mutants in diff, same standard as PR #33. I/O-only functions may use `#[cfg_attr(test, mutants::skip)]` with a one-line justification per PR #33 precedent.

### Playwright E2E tests (post-deploy, against real win-resolume)

**New file:** `e2e/lyrics-dashboard.spec.ts`

1. **Queue visibility** — navigate to `/lyrics`, assert Queue card renders with bucket counts.
2. **Reprocess trigger** — click "Reprocess" on a song row, assert API call fires with correct body, assert row status becomes "queued" within 2s.
3. **Live update** — wait for WS `LyricsProcessingStage`, assert "Currently processing" card updates with song title and stage name.
4. **Detail view** — click "Details" on a processed song, assert modal shows providers, quality metrics, merged lyrics.
5. **Reprocess all stale** — click button, assert bucket-2 count drops and bucket-0 count rises.
6. **Zero console errors/warnings** (per browser-console-zero-errors rule).

---

## Measurable-improvement verification

**Baseline snapshot** (runs on win-resolume via MCP **before** deploy):

`scripts/measure_lyrics_quality.py` walks `C:\ProgramData\SongPlayer\cache`, extracts per-song `{video_id, source, pipeline_version, avg_confidence, duplicate_start_pct, provider_count}` from JSON + audit logs. Writes `baseline_before.json` attached to the PR.

**Post-deploy partial snapshot** (runs +30 min after deploy):

Same script writes `after_30min.json`. At 30 min, worker has reprocessed ~5 songs through the v2 pipeline — enough for signal.

**Comparison report** (posted as PR comment by CI):

```
## Pipeline improvement: v1 → v2

Songs reprocessed in first 30 min: 5
  - Song 1 (Planetshakers #148): ensemble:qwen3 (autosub too sparse, density 0.2)
    avg_conf 0.68 → 0.68 (unchanged, worship-fast edge case — expected)
  - Song 2 (Elevation #73):     ensemble:qwen3+autosub (density 1.1)
    avg_conf 0.71 → 0.82 (+15%), duplicate_start_pct 12% → 4%
  ...

Aggregate across reprocessed subset:
  avg_confidence:        +8.3% mean improvement
  duplicate_start_pct:   -4.1% mean reduction
  provider_count:        1.0 → 1.4 avg (40% of songs now 2-provider)
```

**Hard gate:** if aggregate `avg_confidence` delta is **negative** across the reprocessed subset, the PR description must flag it and we discuss rollback (bump `LYRICS_PIPELINE_VERSION` back to 1 in a follow-up). Green CI does NOT imply pipeline improvement — this metric does.

**Follow-up snapshot:** 24h post-merge, same script runs once more and results are commented on the merged PR. Confirms long-tail convergence.

---

## Open decisions locked in this brainstorm

1. Every song goes through the same ensemble pipeline (no `yt_subs`-vs-`lrclib` fork). Decided 2026-04-16.
2. Claude is the merger at BOTH text-source selection AND word-timing merge. Decided 2026-04-16.
3. Reprocess priority: manual > null-lyrics > stale-worst-first. Decided 2026-04-16.
4. AutoSub density gate thresholds: 0.6 at ≥1.0 wps, 0.1 at ≤0.3 wps, linear between. Decided 2026-04-16.
5. `can_provide` for AutoSub requires density ≥ 0.3 wps AND ≥ 10 words (filters worship-fast edge case cheaply, no Claude call). Decided 2026-04-16.
6. Dashboard shows every processing detail and supports per-song / per-playlist manual reprocess. Decided 2026-04-16.
7. Pipeline version bumps are a human decision at PR time, documented in `CLAUDE.md`. No CI automation. Decided 2026-04-16.
8. Quality score = `avg_confidence - (duplicate_start_pct / 100.0)`. Simple composite; swap later if inadequate. Decided 2026-04-16.
9. Post-deploy +30 min partial snapshot is the improvement gate. Full sweep (21h) reported as PR comment follow-up. Decided 2026-04-16.

## Out of scope

1. Additional alignment providers (#27 WhisperX, #36 description text, #37 Gemini audio). Each ships in its own PR.
2. Automatic rollback on regression. Human reviews the improvement report and decides.
3. In-flight song cancellation.
4. Migration of the Claude text-merge prompt to a versioned prompt library. Prompt lives inline; bumping `LYRICS_PIPELINE_VERSION` covers prompt changes.
5. Dashboard pagination for very large catalogs. 231 songs render in one shot fine; revisit at 1000+.

## Related

- #29 — parent, ensemble alignment umbrella
- #34 — pipeline version tracking (this spec)
- #35 — AutoSub provider (this spec)
- #27 — WhisperX provider (next after this PR)
- #36 — description text provider (later)
- #37 — Gemini audio provider (later)
- `docs/experiments/2026-04-16-autosub-drift.md` — KILL on worship-fast songs; density gate in this design neutralizes the regression risk
- `scripts/experiments/autosub_drift.py` — reference implementation for the matcher (ported verbatim)
- PR #33 — shipped the ensemble infrastructure this PR fills in
