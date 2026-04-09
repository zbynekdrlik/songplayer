# Resolume Title Delivery — Design Spec

**Date:** 2026-04-10
**Goal:** Fix the SongPlayer Resolume integration so titles are displayed on Resolume Arena clips during video playback, matching legacy Python title timing behavior.

## Current State

The Resolume module exists but has critical bugs preventing any title delivery:

1. **Text param discovery broken** — looks for `sourceparams.Text1.id` but Resolume Arena 7.23.2 returns the key `Text` with `valuetype: "ParamText"`
2. **Registry unused** — `_resolume_registry` is created at startup but never connected to the playback engine
3. **Token scheme wrong** — code expects `#song-name-a`, `#artist-name-a`, `#song-clear` (A/B crossfade) but Resolume has single clips like `#spfast-title`
4. **No DNS resolution** — hostname-based connections (e.g., `resolume.lan`) need DNS resolution with IP caching and `Host` header (required by Resolume)

## Design

### Token Pattern

One clip per playlist. Token format: `#sp{playlist_short_name}-title`

| Playlist | Token | Resolume Clip |
|----------|-------|---------------|
| ytfast | `#spfast-title` | Already exists |
| ytwarmup | `#spwarmup-title` | Create in Resolume |
| ytpresence | `#sppresence-title` | Create in Resolume |
| ytslow | `#spslow-title` | Create in Resolume |
| yt90s | `#sp90s-title` | Create in Resolume |
| ytworship | `#spworship-title` | Create in Resolume |

Each playlist stores its Resolume token in the `playlists` table (new column: `resolume_title_token TEXT`).

### Text Format

`Song - Artist` — matching the legacy Python `media_control.py:update_text_source_content()`.

```
if song and artist: "Song - Artist"
elif song: "Song"
elif artist: "Artist"
else: ""
```

If `gemini_failed` is true, append ` ⚠`.

### Title Timing (matches legacy Python)

- **Show:** 1.5 seconds after video starts
- **Hide:** 3.5 seconds before video ends

### Fade Mechanism

No `/disconnect` endpoint exists in Resolume Arena REST API. Instead, use clip video opacity:

- **Fade in:** Step opacity from 0.0 → 1.0 over 1 second (20 steps × 50ms), matching legacy Python behavior
- **Fade out:** Step opacity from 1.0 → 0.0 over 1 second (20 steps × 50ms)

API: `PUT /api/v1/composition/clips/by-id/{clip_id}` with body `{"video":{"opacity":{"value": N}}}`

Sequence:
1. Song starts → wait 1.5s
2. Set text: `PUT /api/v1/parameter/by-id/{param_id}` with `{"value": "Song - Artist"}`
3. Wait 35ms for Resolume to process text texture
4. Fade in: step opacity 0→1 over 1s
5. At `duration_ms - 3500`: fade out opacity 1→0 over 1s
6. After fade-out completes: clear text to empty string

### Text Param Discovery Fix

Adopt presenter's approach — scan `video.sourceparams` entries for `valuetype == "ParamText"`:

```rust
fn extract_text_param_id(clip: &Value) -> Option<i64> {
    let params = clip["video"]["sourceparams"].as_object()?;
    for (_key, param) in params {
        if param["valuetype"].as_str() == Some("ParamText") {
            return param["id"].as_i64();
        }
    }
    None
}
```

### DNS Resolution (from presenter)

When the host is a hostname (not an IP literal):
1. Resolve via `tokio::net::lookup_host()`, prefer IPv4
2. Cache resolved IP for 5 minutes
3. Use resolved IP in the URL: `http://{resolved_ip}:{port}/api/v1/...`
4. Add original hostname as `Host` header (required by Resolume to accept the request)

When the host is an IP literal: use directly, no `Host` header.

### Wiring: Registry → Playback Engine

1. `ResolumeRegistry` stored in `AppState` (remove `_` prefix)
2. Playback engine receives `Arc<RwLock<ResolumeRegistry>>` or a `mpsc::Sender<ResolumeCommand>` to send commands
3. On `PipelineEvent::Started { duration_ms }`:
   - Spawn title-show task: sleep 1.5s → `ShowTitle { playlist_id, song, artist }`
   - Spawn title-hide task: sleep `duration_ms - 3500 - 1500` → `HideTitle { playlist_id }`
4. On `PipelineEvent::Ended` or skip: cancel pending title tasks, send `HideTitle` immediately

### New ResolumeCommand Variants

```rust
enum ResolumeCommand {
    ShowTitle { playlist_id: i64, song: String, artist: String },
    HideTitle { playlist_id: i64 },
    RefreshMapping,
    Shutdown,
}
```

The old `UpdateTitle`/`ClearTitle` are replaced with `ShowTitle`/`HideTitle` which include the fade logic.

### Database Changes

Add column to `playlists` table:
```sql
ALTER TABLE playlists ADD COLUMN resolume_title_token TEXT NOT NULL DEFAULT '';
```

This is a production project (deployed on win-resolume) — use incremental migration.

### Files Changed

| File | Changes |
|------|---------|
| `sp-server/src/resolume/driver.rs` | Fix `parse_composition()` text param discovery; add DNS resolution + Host header + caching; add `set_clip_opacity()` and `fade_opacity()` methods |
| `sp-server/src/resolume/handlers.rs` | Replace A/B crossfade with show/hide + opacity fade; format title text |
| `sp-server/src/resolume/mod.rs` | New command variants (`ShowTitle`/`HideTitle`); remove old `UpdateTitle`/`ClearTitle` |
| `sp-server/src/lib.rs` | Wire registry into AppState; pass to playback engine |
| `sp-server/src/playback/mod.rs` | Send `ShowTitle`/`HideTitle` commands on pipeline events; cancel timers on skip/stop |
| `sp-server/src/db/mod.rs` | Add migration V3 for `resolume_title_token` column |
| `sp-core/src/models.rs` | Add `resolume_title_token` field to `Playlist` |

### Test Plan

1. **Unit tests:** text param discovery with real Resolume JSON structure, DNS resolution logic, title text formatting, opacity stepping math
2. **Integration tests:** wiremock-based tests for show/hide sequences verifying correct PUT/POST order and payloads
3. **E2E (CI on win-resolume):** seed a playlist with `resolume_title_token = "#spfast-title"`, trigger play, verify via Resolume API that text was set and opacity changed
