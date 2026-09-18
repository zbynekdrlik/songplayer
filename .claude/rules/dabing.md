---
paths:
  - crates/sp-server/src/db/models_dabing.rs
  - crates/sp-server/src/api/dabing.rs
  - crates/sp-server/src/api/routes_import.rs
  - crates/sp-server/src/startup_dabing.rs
  - sp-ui/src/pages/dabing.rs
  - sp-ui/src/components/dabing_list.rs
  - sp-ui/src/components/dub_toggle.rs
---

# Dabing (dubbing) feature — data model + section contract (#180 D1, epic #174)

The dubbing feature ("Dabing" — the word "sermon" appears NOWHERE in code/UI,
owner ruling #174) makes any video dubbable and adds an optimized **Dabing**
section for fast priority adds. Binding spec:
`docs/superpowers/specs/2026-09-17-dubbing-feature-design.md` (v2); plan:
`docs/superpowers/plans/2026-09-17-dubbing-feature.md` (lanes D1–D6).

## Data model (main decision: columns on `videos`, NOT a `dub_tracks` table)

Dub state lives as **columns on `videos`** (migration V26), mirroring the V24
stems pattern — one row per video, no joins. Spec §3's separate `dub_tracks`
table was **rejected** (two patterns for the same per-video processing state is
the worse maintenance deal). V26 columns:
`dub_requested`, `dub_status` (`none|queued|stems|transcript|translation|synth|
ready|failed`), `dub_file_path`, `dub_engine`, `dub_voice_ref_path`,
`dub_mix_ratio` (REAL, default 1.0), `dub_error`, `dub_attempts`,
`dub_next_attempt_at`, `dub_requested_at`, plus `stem_manual_priority` (the
stems worker's manual bucket — D3 uses it).

## Query surface — `db/models_dabing.rs` (declared `pub mod` in `db/mod.rs`)

- `set_dub_requested(pool, id, requested)` — requesting sets `dub_status='queued'`
  + stamps `dub_requested_at` + raises BOTH `stem_manual_priority` AND
  `lyrics_manual_priority` to 1 (so the chain's inputs jump their queues, spec
  §4). Un-requesting sets `dub_status='none'`, `dub_requested=0`, and LEAVES the
  priority flags (the video may still want stems/lyrics).
- `list_dub_videos(pool)` — `dub_requested=1`, newest first
  (`dub_requested_at DESC, id DESC`).
- `set_dub_mix_ratio(pool, id, ratio)` — clamps to `0.0..=1.0` (NaN → 1.0),
  persists, returns the clamped value.
- `dub_chain_state(dub_status, stem_status, lyrics_present) -> DubChainState`
  (PURE, unit-tested). `dub_status` is authoritative for the named stages; in the
  pre-transcript region (`queued`/`none`) it refines by artefacts: lyrics present
  ⇒ Transcript, else stems done ⇒ Stems, else Queued. Never overrides an explicit
  later status. `DubRow.chain_state` carries the resolved wire string so the UI
  does NOT duplicate the logic. The D4 write-side selectors
  (`get_next_dub_job`, `mark_dub_*`, `record_dub_deferral`) belong to D4, not D1.

## Import must pass the cookie gate (#180 addendum)

The Dabing/manual import metadata fetch is cookie-gated exactly like the download
path. `downloader/tools.rs::fetch_video_metadata(ytdlp_path, url, cookies)` takes
an optional cookie jar and threads `--cookies` (pure `metadata_args` helper,
unit-tested) before the URL. The shared import core `api/routes_import.rs::
import_video_core` resolves the jar as `cache_dir.parent()/cookies.txt` (the app
always sets `cache_dir = <data_dir>/cache`, so the parent IS the data dir where
the operator drops `cookies.txt`) and passes it when it exists. Both
`POST /api/v1/videos/import` and `POST /api/v1/dabing/import` reuse this core
(extracted OUT of `api/routes.rs` to respect its 1000-line cap). If the box's
`cookies.txt` has expired the import fails "Sign in to confirm you're not a bot"
— that is an ops step (`.claude/rules/youtube-cookies.md`), never hidden.

## API — `api/dabing.rs` (registered in `api/mod.rs`)

`GET /api/v1/dabing` → `{playlist_id, videos:[DubRow…]}`;
`POST /api/v1/dabing/import {url}` (import into the seeded Dabing playlist, then
`set_dub_requested(true)`); `PATCH /api/v1/videos/{id}/dub {requested}` (204/404);
`PATCH /api/v1/videos/{id}/dub-mix {ratio}` (200 + clamped `{ratio}` — D4 will
also push it to the live `DubControl`; D1 only persists).

## Seed — `startup_dabing.rs::ensure_dabing_playlist_exists`

Idempotent (`WHERE NOT EXISTS … kind='dabing'`), `name='Dabing'`,
`ndi_output_name='SP-dabing'`, `kind='dabing'`, `is_active=1`. Mirrors
`ensure_live_playlist_exists`; called from `lib.rs::start()`. Split into a sibling
`#[path]` module so `startup.rs` stays under the 1000-line cap. The OBS scene
`sp-dabing` is created by hand in D5 — legacy `yt*` scenes are NEVER touched.

## UI

Nav entry "Dabing" (`app.rs::Page::Dabing`, `/dabing`). `pages/dabing.rs` = a
paste field (`POST /api/v1/dabing/import`) + a 2 s poll filling
`store.dabing: RwSignal<Vec<DubRow>>` (page-owned loop, `try_get_untracked` on the
cancel flag per `sp-ui-frontend.md`). `components/dabing_list.rs` renders rows
newest-first with the glyph row `stiahnuté → stemy → prepis → preklad → dabing →
pripravené` (or `chyba: <krok>`) + a **Prehrať** button (reuses
`api::post_live_play_video` — it already accepts any playlist id).
`components/dub_toggle.rs` is the per-row Dabing toggle placed in `video_list.rs`
(any playlist). The `videos` list payload gains `dub_requested` + `dub_status`
(additive, `#[serde(default)]`, populated in `db::models::row_to_video`).

## Cap discipline hit here (do not repeat the mistake)

`db/models.rs` (998) and `lib.rs` (999) were AT the cap. Adding an AppState
`data_dir` field overflowed `lib.rs`, so the cookie jar is derived from
`cache_dir.parent()` instead (no new AppState field). Keep new server code in
sibling `#[path]`/`pub mod` modules; never grow `routes.rs`/`models.rs`/`lib.rs`/
`worker.rs`/`playback/mod.rs`.
