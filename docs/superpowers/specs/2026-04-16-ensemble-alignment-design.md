# Ensemble Alignment Pipeline — Design Spec

**Status:** Design approved 2026-04-16
**Parent issue:** #29 (reframed from "skip Qwen3" to "ensemble alignment")
**Related:** #32 (Sub-project B: individual providers)
**Scope:** Sub-project A — merge layer, provider interface, AI infrastructure, quality gate. Individual provider implementations are tracked in #32.

## Goal

Replace the single-source lyrics alignment pipeline (YT manual subs + Qwen3) with a multi-source ensemble that runs all available providers independently, then merges their word-timing estimates via an LLM-powered merge layer (Claude Opus) to produce the best possible result. Quality ratchets up over time as new providers are added — each new model is just another signal, not a rewrite.

## Architecture Overview

```
GATHER (parallel, free)          ALIGN (sequential by cost)       MERGE (LLM)
┌──────────────────┐            ┌──────────────────┐            ┌──────────────┐
│ YT manual subs   │──┐         │ Auto-sub transfer│──┐         │              │
│ YT auto-subs     │  │  pick   │ WhisperX         │  │  all    │ Claude Opus  │
│ LRCLIB           │──┼─best──→ │ Qwen3            │──┼─results→│ via          │
│ Description      │  │  text   │ (future models)  │  │         │ CLIProxyAPI  │
│ CCLI / Genius    │──┘         └──────────────────┘  │         │              │
└──────────────────┘                                   │         │  + audit log │
                                                       └────────→│              │
                                                                 └──────┬───────┘
                                                                        │
                                                                        ▼
                                                                 LyricsTrack JSON
                                                                 (merged, confident)
```

## Data Model

### Provider output (common interface)

```rust
struct WordTiming {
    text: String,
    start_ms: u64,
    end_ms: u64,
    confidence: f32,  // 0.0–1.0, provider's self-reported
}

struct LineTiming {
    text: String,
    start_ms: u64,
    end_ms: u64,
    words: Vec<WordTiming>,
}

struct ProviderResult {
    provider_name: String,
    lines: Vec<LineTiming>,
    metadata: serde_json::Value,  // provider-specific, preserved for audit
}
```

### Merge output

Extends the existing `LyricsTrack` / `LyricsWord` types with confidence metadata:

```rust
struct MergedWordTiming {
    text: String,
    start_ms: u64,
    end_ms: u64,
    confidence: f32,
    source_count: u8,
    spread_ms: u32,
}
```

The merged output is written as the standard `LyricsTrack` JSON (backward-compatible with the existing karaoke display). The confidence/source_count/spread fields are stored in the audit log, not in the lyrics JSON itself, to avoid breaking existing consumers.

### Audit log

Stored per-song as `<video_id>_alignment_audit.json` in the cache directory. NOT committed to the repo.

```rust
struct AuditLog {
    video_id: String,
    timestamp: String,
    reference_text_source: String,
    providers_run: Vec<String>,
    providers_skipped: Vec<(String, String)>,  // (name, reason)
    per_word_details: Vec<WordMergeDetail>,
    quality_metrics: QualityMetrics,
}

struct WordMergeDetail {
    word_index: usize,
    reference_text: String,
    provider_estimates: Vec<(String, u64, f32)>,  // (provider, start_ms, confidence)
    outliers_rejected: Vec<(String, u64)>,
    merged_start_ms: u64,
    merged_confidence: f32,
    spread_ms: u32,
}
```

## Provider Interface

```rust
#[async_trait]
trait AlignmentProvider: Send + Sync {
    /// Unique name for logging/audit ("qwen3", "whisperx", "autosub")
    fn name(&self) -> &str;

    /// Static base confidence weight (0.0–1.0)
    fn base_confidence(&self) -> f32;

    /// Can this provider produce results for this song?
    /// Cheap pre-check — e.g. auto-sub checks density threshold,
    /// Qwen3 checks if vocal isolation prerequisites are met.
    async fn can_provide(&self, ctx: &SongContext) -> bool;

    /// Run alignment independently. Returns word-timed lines.
    async fn align(&self, ctx: &SongContext) -> Result<ProviderResult>;
}
```

### SongContext (shared input)

```rust
struct SongContext {
    video_id: String,
    audio_path: PathBuf,
    clean_vocal_path: Option<PathBuf>,
    candidate_texts: Vec<CandidateText>,
    autosub_json3: Option<PathBuf>,
    duration_ms: u64,
}

struct CandidateText {
    source: String,           // "ccli", "manual_subs", "description", "lrclib", "autosub"
    lines: Vec<String>,       // verse/line structure preserved
    has_timing: bool,         // line-level timing available?
    line_timings: Option<Vec<(u64, u64)>>,
}
```

### Adding a new provider

One file implementing the `AlignmentProvider` trait + register it in the provider list. No merge layer changes. No SongContext changes unless the provider needs a new input field (additive, non-breaking).

### Text sources

Text sources are NOT providers. They are gathered in a pre-step and placed into `SongContext.candidate_texts`. The text with the best verse structure is selected as the reference text:

Priority: CCLI > manual YT subs > video description > LRCLIB > auto-sub text

The reference text defines the authoritative line/sentence structure. Alignment providers align against this text. The merge layer preserves this structure.

## AI Infrastructure

### CLIProxyAPI (ported from presenter)

SongPlayer embeds CLIProxyAPI as a managed child process, identical to the presenter pattern:

- **Binary:** `CLIProxyAPI` from `github.com/router-for-me/CLIProxyAPI`
- **Port:** configurable, default 18787
- **Auth:** Claude OAuth via `-claude-login` flow
- **Interface:** OpenAI-compatible `/v1/chat/completions`
- **Model:** Claude Opus (best available, Max plan — no cost concern)
- **Config:** `cli-proxy-api-config.yaml` in the app data directory

Lifecycle managed via HTTP endpoints (same as presenter):
- `POST /api/v1/ai/proxy/start`
- `POST /api/v1/ai/proxy/stop`
- `POST /api/v1/ai/proxy/login`
- `POST /api/v1/ai/proxy/complete-login`
- `GET /api/v1/ai/status`

### Claude Opus as the single AI gateway

All AI tasks in SongPlayer route through CLIProxyAPI → Claude Opus:

| Task | Currently | After |
|------|-----------|-------|
| Merge word alignments | n/a (new) | Claude Opus |
| Word matching across providers | n/a (new) | Claude Opus |
| SK translation | Gemini | Claude Opus |
| Metadata extraction (song/artist) | Gemini | Claude Opus |

Gemini becomes an optional fallback if CLIProxyAPI is down. The existing `gemini.rs` client stays but is no longer the primary path.

### Merge prompt

One LLM call per song (not per word). Prompt structure:

```
You are a lyrics alignment merger. Given N provider results for the same
song, each containing word-level timestamps, produce a single merged result
with the best possible timing for each word.

Reference text (authoritative lyrics with verse/line structure):
{reference_text_with_line_breaks}

Provider results:
- {provider_name} (base confidence {conf}): [{word, start_ms, end_ms}, ...]
- ...

For each word in the reference text:
1. Match it to the corresponding word in each provider's output. Handle
   contractions ("you're" = "youre"), ASR errors ("grace" vs "Grace's"),
   abbreviations ("G.O.D" = "GOD"), and dropped words intelligently.
2. If multiple providers matched: use weighted average of their timings,
   weighted by base confidence. If any provider is >2s from the others,
   ignore it as an outlier.
3. If only one provider matched: use its timing with reduced confidence.
4. If no provider matched: mark as zero-timed placeholder (confidence 0).
5. If the singer pauses >2s between adjacent words within a line, mark a
   display_split point.

Return JSON:
{
  "lines": [
    {
      "text": "full line text",
      "start_ms": N,
      "end_ms": N,
      "display_split": false,
      "words": [
        {"text": "word", "start_ms": N, "end_ms": N, "confidence": 0.95,
         "sources_agreed": 3, "spread_ms": 50},
        ...
      ]
    }
  ],
  "quality": {
    "avg_confidence": 0.87,
    "words_with_zero_timing": 2,
    "display_splits_added": 1
  }
}
```

### Translation prompt

SK translation also goes through Claude Opus (replacing Gemini):

```
Translate these English worship song lyrics to Slovak. Preserve the line
structure exactly — each English line maps to one Slovak line. Use natural
Slovak worship vocabulary. Return JSON array of translated lines.
```

## Sentence Structure

Text source defines the authoritative line/sentence structure. Timing providers align words within that structure. The merge layer preserves it with one exception:

**Display split rule:** If the merged timing shows a gap >2000ms between adjacent words within a single reference line, the line is split for karaoke display. The original line grouping is preserved as metadata so SK translation pairing still works (one EN line = one SK line, regardless of display splits).

## Single-Provider Handling

When only one provider produces results for a song (common today — most songs only have Qwen3):

- Results pass through the merge layer (still runs via LLM for text cleaning and structure validation)
- Per-word confidence is scaled down: `provider_base_confidence * 0.7`
- Song is flagged as "improvable" in the quality gate
- When a new provider ships and runs on this song, the merge re-runs with 2+ sources and confidence increases

## Orchestration Flow

```
1. GATHER (parallel, all free/instant)
   ├── Fetch YT auto-subs (json3)
   ├── Fetch YT manual subs (if exist)
   ├── Query LRCLIB
   ├── Extract description lyrics
   └── (future: CCLI, Genius, Musixmatch)

   → Select reference text (best verse structure)
   → Compute auto-sub density

2. ALIGN (sequential by processing time)
   ├── Auto-sub transfer (instant, if density > 1 word/sec)
   ├── Gemini audio transcription (30s, cloud, no vocal isolation)
   ├── WhisperX (30s, lightweight local GPU)
   ├── Qwen3 (5min, needs vocal isolation)
   └── (future providers)

   After each provider completes:
   → Run LLM merge on all results so far
   → Check quality metrics
   → If avg confidence > 0.9 and duplicate_start_pct < 5% → STOP early
   → Otherwise continue to next provider

3. MERGE (Claude Opus via CLIProxyAPI)
   → One prompt with reference text + all provider results
   → LLM returns merged word timings with confidence
   → Apply display-split rule
   → Write LyricsTrack JSON
   → Write audit log

4. TRANSLATE (Claude Opus via CLIProxyAPI)
   → SK translation of merged lyrics
   → Paired line-by-line with EN

5. QUALITY GATE
   → Compute duplicate_start_pct, gap_stddev_ms, avg_confidence
   → If below threshold → flag for re-processing
   → Re-processing triggers when new provider is added
```

The early-stop in step 2 is a **time optimization**, not cost (Claude Opus on Max plan has no per-call cost). A slow ballad with great auto-subs can finish in 2 seconds. A fast worship song with poor auto-subs falls through to Qwen3 for 5 minutes.

## Provider Weights (static, v1)

| Provider | Base confidence | Notes |
|----------|----------------|-------|
| Qwen3-ForcedAligner | 0.9 | Best on isolated vocals, local GPU |
| Gemini audio transcription | 0.8 | Cloud-based, handles full mix (no vocal isolation needed), native audio input since Gemini 1.5 |
| WhisperX | 0.7 | Good general-purpose, lightweight local GPU |
| Auto-sub (dense) | 0.6 | >1 word/sec density |
| Auto-sub (sparse) | 0.1 | <0.3 words/sec |
| Manual sub line anchors | 0.8 | Line-level only, no word timing |
| LRCLIB line anchors | 0.5 | Community-sourced, variable quality |

**Gemini audio transcription** is a first-class provider, not a fallback. It receives the audio file (FLAC sidecar) and a prompt requesting word-level transcription with timestamps. Uses the latest Gemini model (`gemini-3.1-pro-preview` as of 2026-04, configurable via `SETTING_GEMINI_MODEL`). Key advantages: no local GPU needed, no vocal isolation pipeline (Gemini handles the full mix), different ASR model = uncorrelated errors with Qwen3 (improves ensemble diversity). Uses the existing Gemini API key already configured in SongPlayer. The codebase default must be updated from `gemini-2.5-flash` to the latest available model.

Weights are hardcoded in provider implementations. The audit log collects all data needed to move to adaptive weights in the future.

## Files (Sub-project A scope)

| Path | Purpose |
|------|---------|
| `crates/sp-server/src/ai/mod.rs` | AI client module: CLIProxyAPI lifecycle, OpenAI-compatible client |
| `crates/sp-server/src/ai/proxy.rs` | CLIProxyAPI process manager (port from presenter) |
| `crates/sp-server/src/ai/client.rs` | OpenAI-compatible HTTP client |
| `crates/sp-server/src/lyrics/provider.rs` | `AlignmentProvider` trait + `SongContext` + `ProviderResult` types |
| `crates/sp-server/src/lyrics/merge.rs` | LLM-powered merge layer: prompt construction, response parsing, audit log |
| `crates/sp-server/src/lyrics/orchestrator.rs` | Per-song pipeline: gather → align → merge → translate → quality gate |
| `crates/sp-server/src/lyrics/quality.rs` | Existing quality metrics (keep), add confidence-based metrics |
| `crates/sp-server/src/api/ai.rs` | HTTP endpoints for proxy management (start/stop/login/status) |
| `sp-ui/src/pages/ai_settings.rs` | Dashboard UI for proxy status + OAuth login flow |

Existing files refactored:
- `lyrics/worker.rs` — refactored to use orchestrator instead of direct Qwen3 calls
- `lyrics/aligner.rs` — becomes the Qwen3 provider implementation
- `metadata/gemini.rs` — replaced by AI client for metadata extraction (Gemini becomes fallback)

## Acceptance Criteria (Sub-project A)

1. CLIProxyAPI embedded and manageable via dashboard (start/stop/login/status)
2. `AlignmentProvider` trait defined with at least one provider (Qwen3, refactored)
3. LLM merge layer accepts 1–N provider results and produces merged LyricsTrack
4. Audit log written per song with full per-word merge details
5. Quality gate flags songs below threshold for re-processing
6. SK translation routed through Claude Opus (Gemini as fallback)
7. Metadata extraction routed through Claude Opus (Gemini as fallback)
8. Existing songs don't regress (single-provider pass-through works)
9. All existing unit tests pass, new tests for merge layer
10. E2E: process a test song, verify merged output has all required fields

## Future Expansion

The CLIProxyAPI proxy is model-agnostic (OpenAI-compatible). Any speech-to-text model available via the same interface (OpenAI Whisper API, or a local whisper served on `/v1/audio/transcriptions`) plugs in as another `AlignmentProvider` with zero architecture changes. The provider trait only cares about the output shape (`Vec<LineTiming>`), not how the timing was produced.

As AI models increasingly support native audio input (Gemini already does, Claude may follow), each becomes a potential alignment provider — send audio + "transcribe with word-level timestamps" → get timed words. The ensemble gets stronger with every new model that ships audio capabilities.

## Out of Scope (Sub-project B, issue #32)

- Individual provider implementations beyond Qwen3 refactor
- WhisperX integration
- Auto-sub transfer provider
- Description lyrics extraction
- CCLI / Genius / Musixmatch integrations
- Adaptive weight learning
