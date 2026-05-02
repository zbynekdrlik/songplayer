# Spotify Tier-1 Wiring + Vocal-Isolation Preservation — Design

**Date:** 2026-05-01
**Branch:** dev (0.29.0-dev.1, post-PR #66 merge)
**Issues bundled:** #67 (Wire SpotifyLyricsFetcher into Tier-1 gather) + #41 (Preserve vocal-isolation intermediate output)
**Deferred:** #60 (NDI runtime re-init recovery) — re-scoped as "investigate the actual NDI bug, fix SongPlayer." Not part of this run.

## Goal

Two small follow-ups after PR #66's lyrics-pipeline redesign:

1. **#67** — Make the existing `SpotifyLyricsFetcher` reachable from production. Currently the fetcher is unit-tested only; `gather_sources_impl` never calls it and there is no API/UI to set `videos.spotify_track_id`.
2. **#41** — Stop deleting `{youtube_id}_vocals16k.wav` after every alignment run. Reuse logic already exists in `aligner::preprocess_vocals` (cache hit at lines 87-96); the deletes at `worker.rs:484` and `worker.rs:495` defeat it.

Both changes are independent and low-risk. Bundled into one PR for one CI cycle.

## Why now

- Spotify provides authoritative LINE_SYNCED lyrics for songs Gemini/yt_subs/LRCLIB/Genius miss (chant-heavy, dense vocal, niche worship songs). Without wiring, that data path is dead in production.
- Demucs / Mel-Roformer preprocess takes minutes per song. Every reprocess (e.g. when the operator re-runs a single song to clean up its lyrics) re-runs Demucs from scratch. On a catalog of 200+ songs, this is hours of wasted CPU during reprocess sessions.

## #67 — Spotify Tier-1 wiring

### Server (Rust)

**API extension** (`crates/sp-server/src/api/routes.rs`):

Extend the `PatchVideoRequest` struct (currently accepts `suppress_resolume_en` + `lyrics_override_text`) to also accept `spotify_url: Option<String>`. The handler:

- If `spotify_url` is `Some("")` or whitespace-only → set `spotify_track_id = NULL`.
- If `spotify_url` is `Some(url)` → extract the track ID by splitting on `/track/` and taking the segment up to the first `?`, `/`, or end-of-string. Validate against `^[A-Za-z0-9]{22}$` (Spotify track ID shape). Reject malformed with HTTP 400.
- Persist via UPDATE `videos SET spotify_track_id = ?`.
- Field is independent of the existing two; the same handler can patch any subset.

**Gather extension** (`crates/sp-server/src/lyrics/gather.rs::gather_sources_impl`):

After the LRCLIB block (line 62-77) and Genius block (80-94) but before `candidate_texts` assembly, add a Spotify fetch:

```rust
let spotify_track = if let Some(track_id) = row.spotify_track_id.as_deref() {
    let fetcher = SpotifyLyricsFetcher::new();
    match fetcher.fetch(client, track_id).await {
        Ok(Some(track)) => {
            info!(%youtube_id, line_count = track.lines.len(), "gather: Spotify hit");
            Some(track)
        }
        Ok(None) => None,
        Err(e) => {
            warn!("gather: Spotify error for {youtube_id}: {e}");
            None
        }
    }
} else {
    None
};
```

Push `CandidateText` into `candidate_texts` after the `override` block and before `yt_subs`:

```rust
if let Some(t) = &spotify_track {
    candidate_texts.push(CandidateText {
        source: "tier1:spotify".into(),
        lines: t.lines.iter().map(|l| l.en.clone()).collect(),
        has_timing: true,
        line_timings: Some(t.lines.iter().map(|l| (l.start_ms, l.end_ms)).collect()),
    });
}
```

Insertion order is cosmetic; orchestrator selects by `claude_merge::source_priority`, which has Spotify already at priority 4 (between override at 5 and LRCLIB at 3).

**Source label:** `"tier1:spotify"`. Already prepared in `source_priority` (no edit there).

**Error handling:** any failure (transport, 404, `error:true` field, malformed JSON, empty `lines`) logs a `warn!` and skips the Spotify candidate. Gather continues with whatever yt_subs/LRCLIB/Genius/description produced. Never returns Err — Spotify is best-effort.

**Re-fetch policy:** on every reprocess. The proxy is fast (~200 ms cache hit), no rate-limit concerns at the catalog scale we have. No on-disk cache.

### Frontend (Leptos)

**Component:** add `spotify_url` input to the existing video card that holds the `lyrics_override_text` paste box (likely `sp-ui/src/components/lyrics_*.rs` or wherever the override input lives — exact location resolved during implementation).

**UX:**

- Label: "Spotify URL"
- Input: `<input type="text" placeholder="https://open.spotify.com/track/...">`
- On blur (or Save button shared with the override text), issue `PATCH /api/v1/videos/{id}` with `{ spotify_url: "..." }`.
- Show the resolved track ID below the input as a small, dim line: "→ track id: 3n3Ppam7vgaVa1iaRUc9Lp" (returned in the PATCH response, or extracted client-side as a preview before save).
- Empty input clears the track ID.

No new WebSocket events. The video card already re-fetches on `LyricsPipelineUpdate`.

### #67 Tests

- **Unit (api/routes.rs):** track-ID extractor: `https://open.spotify.com/track/X`, `…/track/X?si=...`, `…/track/X/`, intl variant `…/intl-cz/track/X?si=...`, malformed → 400, empty → NULL.
- **Unit (gather.rs):** `gather_sources_impl` with `spotify_track_id = Some(...)` and a wiremock returning a valid LINE_SYNCED response → `candidate_texts` includes a `tier1:spotify` entry with `has_timing = true` and the right number of `line_timings` tuples.
- **Unit (gather.rs):** `gather_sources_impl` with `spotify_track_id = None` → no Spotify candidate.
- **Unit (gather.rs):** wiremock returns 404 / `{"error":true,...}` / network error → no Spotify candidate, gather still returns `Ok(...)` with the other sources, warning logged (assert via `tracing-test` capture).
- **E2E (mock-api + Playwright):** paste Spotify URL on a video card → resolved track ID renders → save → PATCH is issued with the correct payload.

## #41 — Preserve vocal-isolation output

### Core fix

`crates/sp-server/src/lyrics/worker.rs`:

- Line 483-484 (orchestrator-error path):
  ```rust
  let wav_path = self.cache_dir.join(format!("{youtube_id}_vocals16k.wav"));
  let _ = tokio::fs::remove_file(&wav_path).await;
  ```
  → **Delete both lines.** Keep the file on disk; next reprocess of the same song will reuse it via `aligner::preprocess_vocals` cache hit (lines 87-96 of aligner.rs).

- Line 494-495 (success path):
  ```rust
  let wav_path = self.cache_dir.join(format!("{youtube_id}_vocals16k.wav"));
  let _ = tokio::fs::remove_file(&wav_path).await;
  ```
  → **Delete both lines.** Same reason.

That is the entire fix on the persistence side. No changes to `aligner.rs`. The cache-hit path (`if meta.is_file() && meta.len() > 1_000_000`) is already correct.

### Self-heal extension

`crates/sp-server/src/downloader/cache.rs::cleanup_removed_videos` (or wherever the orphan scan lives — exact site resolved during implementation):

- The orphan scan currently walks the cache for `_video.mp4` and `_audio.flac` files whose `youtube_id` no longer matches any active DB row, and deletes them.
- Extend the scan to also walk `_vocals16k.wav` files. Same orphan rule: if no DB row has the matching `youtube_id`, delete.

This means when a song is removed from a playlist and gets cleaned out of cache, its preprocess vocals go too — no orphans accumulate.

### Scope cut from issue body

Issue #41 mentions a full-quality `{id}_vocals.wav` alongside `{id}_vocals16k.wav`. Post-PR #66, the alignment backend is WhisperX on Replicate (cloud), and it consumes only the 16 kHz mono vocals (passed to the orchestrator as `clean_vocal`). Nothing in the codebase reads or writes a non-16k `_vocals.wav`. We do not produce that file today, and we do not need to start. Just persist the one file we already produce.

### #41 Tests

- **Unit (lyrics/aligner.rs):** existing cache-hit test still passes (no regression).
- **Unit (worker.rs):** no test added for the deletion removal directly — the absence of `remove_file` on the post-success and post-error paths is verified by the cache-hit path being reachable on consecutive runs (covered by the existing aligner cache-hit test).
- **Unit (downloader/cache.rs):** new test — scan a temp cache dir containing `abc_vocals16k.wav`, `def_vocals16k.wav`, with only `abc` present in a fake DB; assert `def_vocals16k.wav` is deleted, `abc_vocals16k.wav` is kept.

### No admin cleanup endpoint

YAGNI. The orphan-scan self-heal handles cleanup automatically. If an operator ever wants to nuke all preprocess vocals across the catalog (e.g. to force-rerun preprocess after a Mel-Roformer model upgrade), they can do it from the filesystem with a one-liner. We do not need an API surface for that today.

## Combined PR scope

- **Files touched (estimate):**
  - `crates/sp-server/src/api/routes.rs` (extend PATCH handler)
  - `crates/sp-server/src/lyrics/gather.rs` (Spotify fetch + candidate)
  - `crates/sp-server/src/lyrics/worker.rs` (delete two `remove_file` calls)
  - `crates/sp-server/src/downloader/cache.rs` (extend orphan scan to `_vocals16k.wav`)
  - `sp-ui/src/components/<lyrics-input>.rs` (Spotify URL field)
  - Tests across all of the above.
- **DB migration:** none (V17 already added `videos.spotify_track_id`).
- **`LYRICS_PIPELINE_VERSION` bump:** none. Per `feedback_pipeline_version_approval.md`, requires explicit user approval. Operator triggers reprocess song-by-song; new Spotify candidate becomes available on next reprocess of any song with a Spotify URL set.
- **CI cost:** one cycle (~17 min critical path on dev push). No new CI jobs.

## Non-goals

- Auto-resolve Spotify track ID from artist + song (cover/remix mismatch risk; manual-first is settled in closed #52).
- Producing a full-quality `_vocals.wav` alongside the 16k version (no consumer post-#66).
- An admin cleanup endpoint for vocals files (YAGNI; self-heal covers).
- Anything related to NDI runtime re-init or "graceful restart" approaches (#60 is deferred and re-scoped as bug investigation per `feedback_no_ndi_bandaids.md`).

## Risks

- **Spotify proxy availability** — `spotify-lyrics-api-khaki.vercel.app` is a third-party free service. If it goes down, we lose the Spotify candidate for affected songs but the gather still falls through to other sources. No production impact beyond "some songs miss out on the Spotify path until the proxy returns." No SLA.
- **Disk usage growth** — preserving `_vocals16k.wav` adds roughly the audio file size per cached song (16 kHz mono float32 ≈ 64 KB/sec → ~250 MB for a full catalog of 200 songs at 4 minutes each). Acceptable on the production target.
- **Stale cache after Mel-Roformer model upgrade** — if the preprocess model is upgraded, on-disk vocals will be from the old model. Solution: filesystem nuke; no code change needed.
