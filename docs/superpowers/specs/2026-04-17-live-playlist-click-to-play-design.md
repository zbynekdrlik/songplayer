# Live Playlist / Click-to-Play — Design Spec

**Date:** 2026-04-17
**Status:** Approved for implementation
**Urgency:** Live youth event TONIGHT

## Goal

Add a single pre-created "live" playlist (`ytlive`, NDI `SP-live`) whose contents are manually curated by the operator from the existing song catalog, and let the operator click any song in that playlist to jump to it instantly. Songs with lyrics are filterable in the picker. Downloads, YouTube sync, and metadata extraction are all bypassed because songs are references to files already present in the cache.

## Context

SongPlayer today manages six YouTube-backed playlists. Each playlist:

1. Has a YouTube playlist URL; sync worker pulls the video list.
2. Has a download worker that fetches + normalizes each new video.
3. Has a `VideoSelector` that picks the next video at random from unplayed items.
4. Has an NDI output + OBS scene; scene detection drives Play/Pause.

For a live youth event the operator needs to:

- Build an ad-hoc set list from songs already downloaded (cross-playlist).
- Click a song → it plays immediately on the event's NDI output (operator does not want to switch OBS scenes mid-event).
- Continue through the set list (skip/prev) without downloads or YouTube sync.

The spec introduces a second *kind* of playlist — **custom** — that reuses every other part of the playback stack (scene detection, NDI sender, title delivery, Resolume, etc.).

## Architecture

### Schema — migration V13

The existing migration chain ends at V12 (see `crates/sp-server/src/db/mod.rs`). This spec adds V13.

Important schema facts confirmed from the current code:

- `playlists.youtube_url` is `TEXT NOT NULL`; there is **no** `obs_scene_name` column (V3 dropped it). OBS scene matching runs through NDI-source discovery (`crates/sp-server/src/obs/ndi_discovery.rs`) — a playlist's `ndi_output_name` is matched against the stream name inside OBS scene inputs of kind `ndi_source`.
- The `sp_core::models::Playlist` struct must gain two new fields to mirror the new columns.

```sql
-- V13: introduce custom playlists ("live set" style).
ALTER TABLE playlists ADD COLUMN kind TEXT NOT NULL DEFAULT 'youtube';
ALTER TABLE playlists ADD COLUMN current_position INTEGER NOT NULL DEFAULT 0;

CREATE TABLE playlist_items (
    playlist_id INTEGER NOT NULL,
    video_id INTEGER NOT NULL,
    position INTEGER NOT NULL,
    added_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    PRIMARY KEY (playlist_id, position),
    FOREIGN KEY (playlist_id) REFERENCES playlists(id) ON DELETE CASCADE,
    FOREIGN KEY (video_id) REFERENCES videos(id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX idx_playlist_items_playlist_video
    ON playlist_items (playlist_id, video_id);

-- Pre-create the single custom "ytlive" playlist. is_active=1 so scene
-- detection runs for it like any other playlist. youtube_url stores the
-- empty-string sentinel (the column is NOT NULL; avoiding a table
-- recreate to change nullability keeps the migration safe).
INSERT OR IGNORE INTO playlists
    (name, youtube_url, ndi_output_name, playback_mode, is_active, kind)
VALUES
    ('ytlive', '', 'SP-live', 'continuous', 1, 'custom');
```

`kind='youtube'` is the default; existing rows keep the same behavior.

`current_position` is only meaningful for custom playlists. It stores the last-played index so `Skip` advances and `PlayVideo` jumps correctly.

### Backend

**Worker exclusions (custom playlists skipped):**

- `playlist_sync` worker: add `AND kind='youtube'` to its active-playlist query in `crates/sp-server/src/playlist/sync.rs` — no YouTube fetch for custom.
- `download_worker`: untouched. Videos linked via `playlist_items` are references; they were downloaded under their *home* youtube playlist and stay there. The download worker walks the `videos` table by `playlist_id`, and custom playlists have no `videos` rows pointing at them.
- `reprocess_worker`: unchanged — walks videos by `videos.id`, playlist-type-agnostic.

**Video selection branches by kind** — `crates/sp-server/src/playlist/selector.rs::VideoSelector::select_next`:

```rust
match playlist.kind.as_str() {
    "custom" => {
        // Advance current_position, return the video_id at the new
        // position. Wrap to 0 on Loop mode. Return None when past the
        // last item in Continuous mode (stop at end of set).
    }
    _ /* youtube */ => { /* existing implementation untouched */ }
}
```

**New engine command** — `crates/sp-server/src/lib.rs::EngineCommand`:

```rust
PlayVideo {
    playlist_id: i64,
    video_id: i64,
}
```

Handled by new `PlaybackEngine::handle_play_video(playlist_id, video_id)`. Logic mirrors `handle_previous`:

1. Fetch `get_song_paths(video_id)`.
2. For custom playlists, `UPDATE playlists SET current_position = (SELECT position FROM playlist_items WHERE playlist_id=? AND video_id=?)` — so the next `Skip` advances from the right spot.
3. Send `PipelineCommand::Play` to the pipeline.
4. Broadcast `ServerMsg::PlaybackStateChanged`.
5. Push the previous video onto the history stack (for `Previous` to work).

### HTTP endpoints

All new routes live in `crates/sp-server/src/api/routes.rs`:

| Method | Path | Body | Purpose |
|--------|------|------|---------|
| `POST` | `/api/v1/playlists/{id}/items` | `{"video_id": int}` | Append video to a custom playlist; appends at `MAX(position)+1`; 409 if video already in playlist. |
| `DELETE` | `/api/v1/playlists/{id}/items/{video_id}` | — | Remove; auto-compacts positions so there are no gaps. |
| `GET` | `/api/v1/playlists/{id}/items` | — | Returns the set list as `[{position, video_id, youtube_id, song, artist, has_lyrics, source}]`. |
| `POST` | `/api/v1/playlists/{id}/play-video` | `{"video_id": int}` | Send `EngineCommand::PlayVideo`. |

All four handlers return `409` if the playlist is `kind='youtube'` (these routes are only meaningful for custom playlists).

### Frontend — new page `/live`

Route added to `sp-ui/src/app.rs`. Imports song-list components from the existing `/lyrics` page.

Layout (two columns, matches restreamer / iem-mixer dashboard style):

```
┌──────────────────────────────────┬──────────────────────────────┐
│ Catalog                          │ ytlive set list              │
│ [✓] Has lyrics only              │ ▶ 1. Song A — Artist A  ✕    │
│ [Song X] [Artist X] [✓lyr] [+]   │   2. Song B — Artist B  ✕    │
│ [Song Y] [Artist Y] [✓lyr] [+]   │   3. Song C — Artist C  ✕    │
│ [Song Z] [Artist Z]       [+]    │                              │
│ ...                              │ [▶ Play] [⏸] [⏭] [⏮]         │
└──────────────────────────────────┴──────────────────────────────┘
```

- Left column uses `GET /api/v1/lyrics/songs` (already exists — returns all videos with metadata + `has_lyrics`). The "has lyrics only" toggle is client-side filter.
- Right column uses `GET /api/v1/playlists/{ytlive_id}/items`.
- `+` button calls `POST /api/v1/playlists/{ytlive_id}/items {video_id}`.
- `✕` calls `DELETE /api/v1/playlists/{ytlive_id}/items/{video_id}`.
- `▶` on a set-list row calls `POST /api/v1/playlists/{ytlive_id}/play-video {video_id}`.
- Bottom controls reuse existing `playback_controls` component bound to `ytlive` playlist id.
- Current-playing item highlighted via the existing `NowPlaying` WS broadcast.

### OBS scene setup (done via MCP, not in code)

Before the event starts (not a code change, handled as a setup step on win-resolume via the `obs-resolume` MCP server):

1. `obs-create-scene` name `ytlive`.
2. `obs-create-input` in scene `ytlive`, kind `ndi_source`, settings containing `ndi_source_name: "<HOSTNAME> (SP-live)"`. `<HOSTNAME>` is the uppercase Windows machine name of the SongPlayer host (read via `hostname` shell before creating, or discover via the stream name of an existing `SP-*` input in an already-configured scene — e.g., `ytfast`).
3. No title input needed separately if the existing `yt-title` / per-playlist title routing is re-used. For tonight, title delivery falls through Resolume; the OBS text source is optional. If the operator wants an in-OBS title overlay, add a text input using the same naming as existing playlists.
4. Confirm: activating `ytlive` scene shows black (no video yet), and the SongPlayer dashboard shows `ytlive` as "scene on program".

Resolume needs no new setup — titles are delivered via the existing Resolume host registry, which is playlist-agnostic.

### Data flow — click-to-play

```
User clicks ▶ on set-list row 3 (video_id=42)
   │
   ▼ POST /api/v1/playlists/ytlive_id/play-video {video_id: 42}
   │
   ▼ routes::play_video → engine_tx.send(EngineCommand::PlayVideo {...})
   │
   ▼ engine.run loop → handle_play_video(ytlive_id, 42)
   │
   ├─ UPDATE playlists SET current_position = 3 WHERE id = ytlive_id
   ├─ get_song_paths(42) → (video.mp4, audio.flac)
   ├─ pipeline.send(PipelineCommand::Play {video, audio})
   └─ ws_event_tx.send(ServerMsg::PlaybackStateChanged {...})
                  │
                  ▼ dashboard highlights row 3 via WS
```

## Out of scope

- Drag-to-reorder items in the set list (delete + re-add works for tonight).
- Multiple custom playlists (one is enough).
- UI to create new custom playlists (the single `ytlive` row is pre-created in migration).
- Bulk-import from a YouTube playlist URL (manual add per song).
- Cross-playlist song count or "how often I've used this song" analytics.

## Testing

All tests live alongside existing test modules:

**Unit tests (`cargo test`):**
- Migration V13: `kind` and `current_position` columns added, `playlist_items` table created, `ytlive` row inserted with correct defaults.
- `sp_core::models::Playlist` round-trips the new `kind` and `current_position` fields via serde.
- `VideoSelector::select_next` branches on `kind`; for custom returns `playlist_items` in position order, respects Continuous/Single/Loop mode semantics.
- `handle_play_video` updates `current_position`, pushes history, sends Play, broadcasts WS.
- HTTP handlers: return 409 for youtube playlists, 200 for custom, auto-compact positions on delete, 409 on duplicate add.

**Manual smoke on win-resolume before the event:**
- Deploy build to win-resolume.
- Confirm migration V13 applied: `SELECT kind, ndi_output_name, current_position FROM playlists WHERE name='ytlive'` → `custom | SP-live | 0`.
- Create the `ytlive` OBS scene + NDI input via MCP (see OBS scene setup above).
- Add 2 songs to the live playlist via dashboard.
- Activate `ytlive` scene in OBS; expect black (no video yet).
- Click ▶ on row 1 → expect NDI video, title delivery, console clean.
- Click ▶ on row 2 → expect immediate jump, no auto-advance needed.
- Click ⏭ Skip → expect auto-advance to row 3 (if present) or Stop (if end of list in Continuous).

**Playwright E2E — NOT required before tonight.** Add after the event so the feature ships in time. Will cover: add-to-set, remove, click-to-play, skip, previous, WS highlight.

**Mutation testing:** new code should pass `cargo mutants --in-diff` with zero survivors. Handlers whose only behavior is wiring (e.g., `POST /api/v1/playlists/{id}/items` just INSERTs) may use `#[cfg_attr(test, mutants::skip)]` with a one-line justification.

## Risk and rollback

- **Risk:** custom playlist code path has bugs that crash the worker → breaks all 6 existing playlists too.
  - Mitigation: custom-path code is gated behind `kind='custom'` branch; youtube path untouched and covered by existing tests.
- **Risk:** migration V13 fails mid-run → server does not start.
  - Mitigation: migration is transactional (sqlx-level `BEGIN/COMMIT`); on failure, previous version still runs. Manual smoke on win-resolume before event confirms migration applied.
- **Rollback:** if /live page is broken but playback engine works, hit `DELETE /api/v1/playlists/{ytlive_id}/items/{video_id}` via curl or revert the UI artifact to a previous build. Backend can be reverted by rolling back migration — document the down-migration SQL in the migration file comment.
