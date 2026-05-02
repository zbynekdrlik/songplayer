# Spotify Auto-Resolver + Priority Fix — Design

**Date:** 2026-05-02
**Branch:** dev (0.30.0-dev.1, post-PR #70 merge)
**Issues bundled:** #73 (auto-resolver replacing PR #70 manual UI) + #72 (`best_authoritative` priority weakness)
**Supersedes:** PR #70's manual-UI Spotify path

## Goal

Replace the manual 🎵 button + `PATCH spotify_url` UI from PR #70 with fully automatic Claude-based resolution. Make Spotify lyrics actually win over noisier candidates by fixing the `claude_merge::best_authoritative` ranking. Operator never types a Spotify URL; lyrics finding is automatic per `feedback_llm_over_heuristics.md`.

## Why

PR #70 shipped Spotify Tier-1 wiring but with the wrong UX: the operator had to find a Spotify URL on each song and paste it. That contradicts the user's stated intent ("lyrics finding should be automatic and 100% right not push user to do more work"). Plus, even with correctly resolved Spotify track IDs, the merge layer's ranking `(lines.len(), source_priority)` made longer yt_subs/description candidates beat Spotify — so Spotify lyrics rarely won in practice.

Both bugs ship together so the Spotify path actually delivers lyrics on the wall.

## Architecture

### Resolution flow (lazy, per-song)

When the lyrics worker pulls a row from a bucket and is about to call `gather_sources_impl`:

1. **Gate check.** Read `videos.spotify_track_id` and `videos.spotify_resolved_at`.
   - non-NULL `spotify_track_id` → resolved already, skip step 2.
   - NULL `spotify_track_id` AND non-NULL `spotify_resolved_at` → previously attempted (success-or-NONE recorded), skip until natural reprocess.
   - NULL on both → never tried, proceed.

2. **Claude resolve.** Single call to `AiClient` (CLIProxyAPI / Anthropic) with `(song, artist, youtube_title, youtube_id)`. Prompt explicitly asks for the canonical Spotify track ID matching the recording, or the literal string `NONE`. Constrain output: 22-char alphanumeric or `NONE`.

3. **Verify.** If Claude returned an ID, fetch from the existing public proxy (`spotify-lyrics-api-khaki.vercel.app?trackid=X`). Require `syncType=LINE_SYNCED` AND `lines.len() >= TIER1_MIN_LINES (10)`. If proxy 404 / `error:true` / unsynced / too-few-lines → treat as NONE.

4. **Persist.**
   - On success: `spotify_track_id = X`, `spotify_resolved_at = now()`.
   - On NONE / verification fail: `spotify_track_id = NULL`, `spotify_resolved_at = now()`.

5. **Continue.** `gather_sources_impl` runs as today; if `spotify_track_id` is now non-NULL, the existing Spotify fetch path emits a `tier1:spotify` candidate.

### Why lazy (not batch)

- Naturally rides the existing bucket queue. No new worker thread, no new admin endpoint.
- One Claude call per song lifetime — cached after via the `spotify_resolved_at` gate.
- The 115-song catalog backfill happens automatically as the worker drains bucket1.

### Why one timestamp (not metadata-hash)

- Re-resolution-on-metadata-change is a future enhancement. The natural retry trigger is `LYRICS_PIPELINE_VERSION` bump (which already requires user approval per `feedback_pipeline_version_approval.md`).
- Operator can force a re-resolve by `UPDATE videos SET spotify_track_id = NULL, spotify_resolved_at = NULL WHERE id = ?`. Future enhancement: an admin endpoint for that.

### Priority fix (#72)

`claude_merge::best_authoritative` currently ranks by `(lines.len(), source_priority)` — longer wins, priority is a tiebreaker. Change to `(source_priority, lines.len())` — priority first, length is a tiebreaker. Spotify (priority 4) now beats longer yt_subs (priority 1) / description (priority 0) / genius (priority 2) / lrclib (priority 3) candidates regardless of line count.

The override (priority 5) remains the absolute top, as expected.

## Components

### `crates/sp-server/src/lyrics/spotify_resolver.rs` (new)

```rust
pub struct SpotifyResolver { /* ai_client, http_client */ }

impl SpotifyResolver {
    pub async fn resolve(
        &self,
        song: &str,
        artist: &str,
        youtube_title: &str,
        youtube_id: &str,
    ) -> ResolveOutcome;
}

pub enum ResolveOutcome {
    /// Claude returned ID and proxy verified ≥10 LINE_SYNCED lines.
    Resolved(String),
    /// Claude returned NONE, OR Claude returned ID but proxy verification failed.
    NoMatch,
    /// Transport / parse error — DO NOT cache as NoMatch (will retry next time).
    Error(anyhow::Error),
}
```

Inside the worker, `process_song` calls `resolve()` on the gate-passed condition. On `Resolved(id)` → write track_id + resolved_at. On `NoMatch` → write NULL track_id + resolved_at. On `Error` → leave columns untouched (will retry next worker pass).

### `crates/sp-server/src/db/mod.rs`

Migration V18:
```sql
ALTER TABLE videos ADD COLUMN spotify_resolved_at TIMESTAMP NULL;
```

`VideoLyricsRow` gains `pub spotify_resolved_at: Option<String>` (or `Option<DateTime<Utc>>` — match how existing TIMESTAMP fields are read in this codebase). Three SELECTs in `reprocess.rs` add `v.spotify_resolved_at`. Existing `VideoLyricsRow` literal in `worker_tests.rs` adds `spotify_resolved_at: None`.

### `crates/sp-server/src/db/models.rs`

New helpers:
- `set_video_spotify_resolution(pool, video_id, track_id: Option<&str>) -> sqlx::Result<u64>` — sets both `spotify_track_id` and `spotify_resolved_at = datetime('now')` atomically.

### `crates/sp-server/src/lyrics/worker.rs`

Pre-gather hook before `gather_sources_impl` call:

```rust
// Spotify auto-resolution gate. Only runs once per song lifetime.
if row.spotify_track_id.is_none() && row.spotify_resolved_at.is_none() {
    let outcome = self.spotify_resolver.resolve(
        &row.song, &row.artist, &row.title.unwrap_or_default(), &row.youtube_id,
    ).await;
    match outcome {
        ResolveOutcome::Resolved(id) => {
            db::models::set_video_spotify_resolution(&self.pool, row.id, Some(&id)).await?;
            row.spotify_track_id = Some(id); // local copy so gather sees it
            row.spotify_resolved_at = Some(now_string());
        }
        ResolveOutcome::NoMatch => {
            db::models::set_video_spotify_resolution(&self.pool, row.id, None).await?;
            row.spotify_resolved_at = Some(now_string());
        }
        ResolveOutcome::Error(e) => {
            warn!("worker: spotify resolution error for {}: {e}", row.youtube_id);
            // Don't write resolved_at — will retry next pass.
        }
    }
}
```

### `crates/sp-server/src/lyrics/claude_merge.rs`

`best_authoritative` ranking change. The exact line is currently `.max_by_key(|c| (c.lines.len(), source_priority(&c.source)))`. Change to:

```rust
.max_by_key(|c| (source_priority(&c.source), c.lines.len()))
```

### Removals from PR #70

| File | Removal |
|---|---|
| `sp-ui/src/components/live_setlist.rs` | 🎵 button + `spotify_track_id_initial` extraction + click handler |
| `sp-ui/src/api.rs` | `patch_video_spotify_url` helper |
| `sp-ui/style.css` | `.live-setlist-btn-spotify` + `.has-spotify` rules |
| `crates/sp-server/src/api/routes.rs` | `spotify_url` field on `PatchVideoReq` + handler logic + `parse_spotify_track_id` parser |
| `crates/sp-server/src/api/routes_tests_spotify.rs` | DELETE (4 PATCH tests + helper become irrelevant) |
| `crates/sp-server/src/api/lyrics.rs` | `spotify_track_id` field on `SongListItem` + SELECT mention |
| `e2e/tests/spotify-url-input.spec.ts` | DELETE (no UI to test) |

The closed `mod tests_spotify;` declaration in `routes.rs` is also removed.

## Data flow

```
worker pulls bucket1 row
  → SpotifyResolver gate
       (NULL/NULL?)
            → Claude(song, artist, title, yt_id) → "3n3Ppam7vgaVa1iaRUc9Lp" or "NONE"
            → if ID: fetch proxy, verify LINE_SYNCED + ≥10 lines
            → persist (track_id + resolved_at) OR (NULL + resolved_at)
  → gather_sources_impl (now sees spotify_track_id if resolved)
       → SpotifyLyricsFetcher.fetch(track_id) → tier1:spotify candidate
  → claude_merge picks highest-priority candidate (Spotify wins over longer yt_subs/etc)
  → final lyrics saved
```

## Error handling

- **Claude transport failure** (HTTP error, timeout, parse error): `Error(...)` → no DB write → retry next worker pass. Don't cache transient failures as NONE.
- **Claude returned malformed output** (not 22 chars, not "NONE"): treat as NONE.
- **Proxy 404 / error:true / malformed JSON**: treat as NONE for that ID.
- **Proxy LINE_SYNCED but <10 lines**: treat as NONE (probably a false-positive Spotify match).
- **DB write failure**: log warn, continue worker (will retry on next song; song's resolution can re-run next pass).

## Tests

### Unit (`spotify_resolver.rs`)

- Prompt builder produces the documented format with all 4 fields.
- Parse Claude response: `"3n3Ppam7vgaVa1iaRUc9Lp"` → `Resolved`; `"NONE"` → `NoMatch`; `"none"` (lowercase) → `NoMatch` (case-insensitive); `"3n3Ppam7vga"` (too short) → `NoMatch`; `"   3n3Ppam7vgaVa1iaRUc9Lp   "` (whitespace) → `Resolved` (trimmed).

### Wiremock integration (`worker_tests.rs`)

Each test marked `#[serial_test::serial]` because it shares the env-var-overridable proxy base from PR #70 (`SPOTIFY_LYRICS_PROXY_BASE`). Mock both Claude and the Spotify proxy.

- `resolver_persists_track_id_on_claude_success_and_proxy_verifies` — Claude returns valid 22-char ID, proxy returns LINE_SYNCED with 12 lines → `spotify_track_id` set, `spotify_resolved_at` set, gather emits `tier1:spotify` candidate.
- `resolver_persists_null_on_claude_none` — Claude returns "NONE" → no proxy call, both columns updated (track_id NULL, resolved_at set).
- `resolver_persists_null_on_proxy_404` — Claude returns valid ID, proxy 404 → both columns updated (track_id NULL, resolved_at set).
- `resolver_persists_null_on_proxy_few_lines` — Claude returns valid ID, proxy LINE_SYNCED with 3 lines → both columns updated (track_id NULL, resolved_at set).
- `resolver_skips_when_already_resolved` — pre-fill `spotify_track_id = X`, run worker, assert no Claude call (use mock counter).
- `resolver_skips_when_previously_no_match` — pre-fill `spotify_resolved_at` non-NULL with `spotify_track_id = NULL`, run worker, assert no Claude call.
- `resolver_does_not_persist_on_transport_error` — wiremock returns 500 for Claude → both columns stay NULL → next worker pass retries.

### Unit (`claude_merge.rs`)

- `best_authoritative_ranks_spotify_over_longer_yt_subs` — fixture: yt_subs candidate with 50 lines, tier1:spotify candidate with 20 lines → Spotify wins.
- `best_authoritative_keeps_override_top` — override (priority 5) with 5 lines beats Spotify (priority 4) with 30 lines.
- `best_authoritative_breaks_tie_by_lines` — two equal-priority candidates → longer wins (existing behavior preserved within a priority bucket).

### E2E

- DELETE `e2e/tests/spotify-url-input.spec.ts` (no UI to test).
- No new E2E. The auto-resolver is a server-side worker concern — wiremock unit-style integration tests are the right level. Visible production effect is that bucket1 songs end up with `lyrics_source = tier1:spotify` for songs Claude resolves correctly.

## Migration impact

- **V18** adds one TIMESTAMP column. Idempotent (no defaults that need backfilling — NULL is the natural unresolved state).
- **No `LYRICS_PIPELINE_VERSION` bump.** Per `feedback_pipeline_version_approval.md`.
- **Existing 5 spotify-source songs** keep their manually-entered IDs. The gate (track_id non-NULL) skips them, so no Claude calls for those.
- **115 missing-lyrics songs** become eligible for Claude resolution as they're processed.

## Cost / quota

- Per-song Claude call. CLIProxyAPI runs on the user's Anthropic Max plan (not metered) per `feedback_cliproxyapi_model.md`.
- Per `feedback_active_monitoring.md`, no rate-limit-related delays needed.
- 240 songs × 1 call ≈ 240 calls one-time, then 0 ongoing (cached). New songs entering catalog incur 1 call each.

## Non-goals

- No batch admin endpoint for forced bulk re-resolution. YAGNI.
- No metadata-change hash detection. If operator updates song/artist, they re-process the song manually (or wait for next version bump).
- No Spotify Web API direct integration. We rely on Claude + the public proxy.
- No "force re-resolve" UI button. Operator can clear the columns via direct DB access if needed.
- No SongListItem visibility into spotify_track_id (the UI is gone).

## Risks

- **Claude wrong answer:** Claude picks a wrong canonical track (e.g., a remaster instead of the original). Verification gate (≥10 LINE_SYNCED lines) catches obvious mismatches but not subtle ones. Mitigation: trust Claude's accuracy on first attempt; if production shows systematic wrong matches, tighten the prompt.
- **Proxy availability:** `spotify-lyrics-api-khaki.vercel.app` is third-party. If down: resolution falls into Error path → no DB write → retried next worker pass. No cascading failure.
- **Catalog 115-song burst:** When the iteration starts, all 115 missing songs in bucket1 will trigger Claude calls in sequence. Worker is single-song serial, so calls are spaced naturally. No quota concern (Max plan).
