# Universal `#sp-title` Title Redesign

**Date:** 2026-04-10
**Goal:** Replace per-playlist Resolume tokens and per-playlist OBS text sources with a single universal `#sp-title` identifier that drives all Resolume clips bearing that tag (across multiple layers/columns/decks) and the single OBS `#sp-title` text source in the `CG OVERLAY` fallback scene. Remove the `⚠` Gemini-failed indicator entirely.

## Context

The previous design (spec `2026-04-10-resolume-title-delivery-design.md`) introduced per-playlist tokens like `#spwarmup-title`, `#spfast-title`, etc. and per-playlist OBS text sources (`ytfast_title`, `sp-fast_title`, …). Testing on the live `win-resolume` machine revealed three problems:

1. **Tight coupling to playlist identity** — every new playlist or every new Resolume deck layout requires creating and configuring a matching clip token, which does not scale and blocks designers from placing the title clip freely across multiple columns/decks.
2. **Mojibake of the `⚠` indicator** — Resolume's Text Block renderer corrupted the UTF-8 warning symbol because it uses a different font/encoding path than OBS's GDI+ text source. The legacy Python solution only worked because OBS rasterized the character into the NDI video before it reached the LED wall; Resolume REST-API-driven text has no equivalent font fallback.
3. **Title duplication in OBS sp-* scenes** — the sp-* scenes were carrying their own `sp-*_title` text source per playlist, which the user no longer wants (titles belong to Resolume; OBS should only hold a fallback).

## Design

### One universal identifier

`#sp-title` — one string constant used both as:

- A **Resolume clip name tag**: any clip containing `#sp-title` in its display name is a title target. Designers can drop `#sp-title`-tagged clips into any layer, any column, any deck.
- An **OBS text source name**: a single text source named exactly `#sp-title`, present in the `CG OVERLAY` scene as fallback.

No per-playlist configuration anywhere. The constant lives in code.

### Data flow

When the playback engine's `Started` event fires for any playlist:

1. Wait 1.5 s (match legacy timing).
2. Read `(song, artist)` from `videos` table (no longer reading `gemini_failed`).
3. Format: `format_title_text(song, artist)` → `"Song - Artist"` / `"Song"` / `"Artist"` / empty.
4. Send `ObsCommand::SetTextSource { source_name: "#sp-title".into(), text }` to the OBS WebSocket client.
5. Send `ResolumeCommand::ShowTitle { song, artist }` to the Resolume registry.

At `duration_ms - 3500`:

1. Send `ObsCommand::SetTextSource { source_name: "#sp-title".into(), text: "".into() }`.
2. Send `ResolumeCommand::HideTitle` to Resolume registry.

### Resolume multi-clip handling

The driver's clip mapping changes from `HashMap<String, ClipInfo>` to `HashMap<String, Vec<ClipInfo>>`. `parse_composition` scans every `layers[].clips[]` entry; for every `#token` found in `name.value`, it pushes a `ClipInfo { clip_id, text_param_id }` into the vector for that token.

`show_title` (adapted from presenter's pattern):

1. Look up the `Vec<ClipInfo>` for `#sp-title`.
2. If empty, log a debug message and return `Ok(())` (not a warning — no clips configured is a valid state).
3. `format_title_text(song, artist)`; if empty, return.
4. Parallel set text: for every clip in the vec, PUT to its `text_param_id`. Use `futures::stream::FuturesUnordered` to run all requests concurrently.
5. `tokio::time::sleep(35ms)` for text settle.
6. Parallel opacity fade in: for each step `1..=20`, set opacity on ALL clips in parallel, then sleep `50ms`.

`hide_title` is symmetric:

1. Look up the vec, return if empty.
2. Parallel opacity fade out: for each step `(1..=20).rev()`, set opacity on all clips, sleep `50ms`.
3. Final parallel set-opacity to 0.0.
4. Parallel clear text (set empty string on all clips' `text_param_id`).

### Command enum simplification

```rust
#[derive(Debug, Clone)]
pub enum ResolumeCommand {
    ShowTitle { song: String, artist: String },
    HideTitle,
    RefreshMapping,
    Shutdown,
}
```

No `playlist_id`, no `gemini_failed`. The driver no longer loads a playlist→token map from the DB; the token is hardcoded.

### Hardcoded token constant

In `crates/sp-server/src/resolume/mod.rs` (and referenced from `driver.rs` / `handlers.rs`):

```rust
/// The single Resolume clip tag used for title delivery.
/// Any Resolume clip whose name contains this tag becomes a title target.
pub const TITLE_TOKEN: &str = "#sp-title";
```

In `crates/sp-server/src/playback/mod.rs`:

```rust
/// OBS text source name used for the fallback title display (in the
/// CG OVERLAY scene). Must match the source name in OBS exactly.
const OBS_TITLE_SOURCE: &str = "#sp-title";
```

### Database migration V3

Drop the now-unused per-playlist columns:

```sql
ALTER TABLE playlists DROP COLUMN obs_text_source;
ALTER TABLE playlists DROP COLUMN resolume_title_token;
```

SQLite 3.35+ supports `DROP COLUMN` natively. sqlx ships with a bundled SQLite that is newer than 3.35, so this works on Windows without rebuild.

Fallback (if `DROP COLUMN` fails on the deployed machine): rebuild the table (`CREATE TABLE playlists_new AS SELECT id, name, youtube_url, ndi_output_name, playback_mode, is_active, created_at, updated_at FROM playlists; DROP TABLE playlists; ALTER TABLE playlists_new RENAME TO playlists;`). We'll use the native `DROP COLUMN` form first and fall back only if CI fails.

### Remove `⚠` gemini_failed indicator

- `format_title_text` drops the `gemini_failed` parameter and the trailing `\u{26A0}` branch.
- `ShowTitle` command drops the field.
- `get_video_title_info` in playback no longer reads `gemini_failed`.
- The `gemini_failed` column remains in the `videos` table (used elsewhere by the reprocess worker); we only stop *displaying* it in titles.

### API and WASM dashboard changes

`CreatePlaylistRequest` / `UpdatePlaylistRequest` drop `obs_text_source` and `resolume_title_token` fields. The `/api/v1/playlists` GET / POST / PATCH responses stop including them. Dashboard (WASM) does not currently render these fields as editable inputs, so no Leptos changes are expected beyond possibly deleting dead code.

### CI E2E changes

In `.github/workflows/ci.yml` "Seed playlists" step:

- Remove `obs_text_source` and `resolume_title_token` from the `$playlists` hashtable.
- Remove the PATCH that sets `resolume_title_token` on existing playlists.
- Keep the verification step that queries `#sp-title` clip text — update it to look for any `#sp-title` clip (plural) and check all of them got the same text.

## Files changed

| File | Change |
|------|--------|
| `crates/sp-server/src/db/mod.rs` | Migration V3: drop `obs_text_source`, `resolume_title_token` columns |
| `crates/sp-core/src/models.rs` | Remove fields from `Playlist` |
| `crates/sp-server/src/db/models.rs` | Update `get_active_playlists` SELECT to drop columns |
| `crates/sp-server/src/api/routes.rs` | Drop fields from `CreatePlaylistRequest`, `UpdatePlaylistRequest`; drop from SELECT queries and JSON output; drop from `update_playlist` dynamic SET builder |
| `crates/sp-server/src/resolume/mod.rs` | Define `TITLE_TOKEN` const; simplify `ResolumeCommand` variants; remove `playlist_id` from `ShowTitle`/`HideTitle` |
| `crates/sp-server/src/resolume/driver.rs` | `clip_mapping: HashMap<String, Vec<ClipInfo>>`; rewrite `parse_composition`; remove `load_tokens()`; simplified `handle_command` using hardcoded token; `set_clip_opacity` operates on single clip (callers iterate) |
| `crates/sp-server/src/resolume/handlers.rs` | `format_title_text(song, artist)` (no gemini_failed); `show_title` / `hide_title` iterate vec using `FuturesUnordered` for parallel updates; no token parameter (use `TITLE_TOKEN`) |
| `crates/sp-server/src/playback/mod.rs` | Hardcoded `OBS_TITLE_SOURCE`; remove `gemini_failed` from `get_video_title_info`; use `ShowTitle { song, artist }` |
| `.github/workflows/ci.yml` | Remove per-playlist title_token/text_source seeding; update verification step to iterate all `#sp-title` clips |

## Out of scope

- Improving `gemini_failed` reporting elsewhere (dashboard indicators, reprocess worker changes). The user plans a separate redesign for metadata extraction.
- Removing title text sources from the sp-* OBS scenes — that's a manual operator action in OBS, not a code change.
- Removing the per-playlist `sp-*_title` text sources entirely from OBS — manual cleanup, not code.

## Test plan

1. **Unit tests:**
   - `parse_composition` with 3 clips all named `#sp-title` on different layers — expect `Vec<ClipInfo>` with 3 entries in mapping under key `#sp-title`.
   - `format_title_text`: 4 cases (both / song only / artist only / empty); no `gemini_failed` parameter.
   - `ResolumeCommand` variants serialize as expected.
2. **Integration tests:** wiremock-based test — spawn mock Resolume with a composition containing 2 `#sp-title` clips; issue `ShowTitle`; verify 2 parallel PUT requests to set text + 20 parallel PUT requests per fade step.
3. **Migration test:** apply V3 migration to an in-memory DB seeded with V2 data; verify `playlists` no longer has `obs_text_source` or `resolume_title_token` columns, existing rows preserved.
4. **CI E2E:** verify on real Resolume that all clips with `#sp-title` get updated text simultaneously, and OBS `#sp-title` source receives the same text.

## Verification

1. Deploy v0.8.0 to win-resolume via CI.
2. Manually create additional `#sp-title` clips in Resolume (different layers/decks).
3. Trigger play on any playlist.
4. Confirm: all `#sp-title` clips get the same `"Song - Artist"` text without `⚠`, OBS `CG OVERLAY` scene's `#sp-title` source also updated, no mojibake.
5. Trigger skip — confirm titles update on all clips.
