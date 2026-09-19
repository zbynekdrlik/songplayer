---
paths:
  - "sp-ui/**"
  - "e2e/mock-api.mjs"
---

# sp-ui / e2e mock gotchas

## `sp_core::models::Video` has required (non-`Option`) bool fields — mock
fixtures MUST include them

`cached`, `normalized`, and `gemini_failed` are plain `bool` with **no**
`#[serde(default)]` (unlike `suppress_resolume_en` / `spotify_track_id`,
which do have it). `Option<T>` fields (`song`, `artist`, `duration_ms`) are
fine to omit from JSON — serde special-cases `Option` to default to `None`
when the key is absent — but a missing `bool` key is a hard deserialization
error.

`e2e/mock-api.mjs`'s `videos` fixture array shipped without `normalized` /
`gemini_failed` for a long time — nobody caught it because `VideoList`
(`sp-ui/src/components/video_list.rs`) was written but never actually
rendered by any page (#134 wired it in). The moment ANY component calls
`api::get::<Vec<Video>>("/api/v1/playlists/{id}/videos")`, every fixture in
that array needs every non-`Option<T>` field present or the whole fetch
silently fails to deserialize (no console error surfaces unless the caller
also logs `Err`).

**Before adding a new required (non-`Option`) field to any `sp_core::models`
struct — or before wiring a previously-unused component into a page for the
first time — grep `e2e/mock-api.mjs` for that struct's fixtures and update
them.**

## A `spawn_local` poll loop must read page-owned signals with `try_*`

`spawn_local` tasks are NOT tied to the reactive owner: a poll loop keeps
running after navigation, and its next `get()`/`get_untracked()`/`with()` on
the disposed signal panics (`Tried to access a reactive value that has already
been disposed` + `RuntimeError: unreachable` — `resolume_health.rs`, fixed
2026-09-12). `set()` is safe (`try_update` underneath); reads are not. In a
loop that outlives a mount: `if sig.try_get_untracked() != Some(false) { break }`
and `if sig.try_set(v).is_some() { break }`. The frontend spec's nav round-trip
test (7 s wait — the timer fires at 5 s) guards this.

## `<For>` keyed on `id` alone goes STALE after an in-place refresh

`<For each key=|v| v.id children=|v| {...}>` never re-runs `children` for a key
it has already seen — it only adds/removes/moves rows by key. So if the row's
cells capture the item's fields BY VALUE (e.g. `let title = v.song.clone()...`)
and the list is later REPLACED with the same ids but changed data (an in-place
`load()` re-fetch after an edit/PATCH), the row keeps showing the OLD values —
the data persisted, the DOM did not update (`video_list.rs`, #136 T1: the
save→`load()` round-trip left the corrected song/artist invisible until a full
page reload). Fixes, cheapest first: fold the mutable rendered fields into the
key (`key=|v| (v.id, v.song.clone(), v.artist.clone())`) so only the changed
row is recreated; or make the cells reactive (look the item up in the signal by
id inside a `move ||`). `download_queue.rs` dodged this only by accident — its
`<For>` lives inside a `{move || { store.get(); view!{...} }}` block that
rebuilds the whole subtree on every change. **Caveat for the key-on-content
fix:** it is safe only while the list is never mutated DURING an active edit — a
key change mid-typing recreates the `<tr>` and drops input focus. If you add
live/WebSocket refresh to a list with an inline editor, key on identity and make
cells reactive instead.

## A wide `<table>` inside a fixed-width card paints over the NEXT card and steals its clicks

`.video-list table { width: 100% }` is only a MAX preference — a table cannot
shrink below its columns' min-content width. The songs table (6 columns) is
~445px min, wider than the ~340px `.playlist-card`, and the container had no
scroll boundary, so the table overflowed the card's right edge. Because the
dashboard `.playlist-grid` packs cards side-by-side (3×376px at 1280px), the
overflowing rightmost column landed physically OVER the neighbouring
`.playlist-card` — a later DOM sibling that paints on top — and captured the
pointer, so Playwright reported `<div class="playlist-card">…</div> intercepts
pointer events` and the edit (✎) button was unclickable on any card with a
right-hand neighbour (#136 T1, dev CI run 34812659893). Fix: give the container
a scroll boundary — `.video-list { overflow-x: auto }` — so the wide table is
clipped/scrolled INSIDE its own card instead of overflowing onto siblings.
General rule: any content that can exceed a grid card's width needs its own
`overflow` boundary, or it will steal clicks from the card next to it.

## Root `cargo fmt` / `cargo clippy` do NOT check `sp-ui` or `src-tauri`

Both are excluded from the root `Cargo.toml` workspace (different toolchain
needs — WASM / Tauri bundler). CI's `lint` job runs `cargo fmt --all --
--check` and `cargo clippy --workspace -- -D warnings` from the repo root,
so neither ever touches `sp-ui/` or `src-tauri/`. `build-wasm` only runs
`trunk build --release` — no fmt/clippy step at all for sp-ui.

Consequence: `sp-ui` has accumulated real rustfmt drift across many
pre-existing files (verified 2026-08-06: `cd sp-ui && cargo fmt --all --
--check` fails on 17 files unrelated to any single change — re-check the
count if you're relying on this, it will keep drifting until someone
deliberately reformats the whole crate in its own PR).

**Never run a blanket `cd sp-ui && cargo fmt --all`** — it silently rewrites
every drifted file into your diff. Instead, format-check ONLY the files you
touched: `rustfmt --edition 2024 --check src/components/foo.rs
src/components/bar.rs`. Same idea for clippy — scope to what you changed
(`cargo clippy --target wasm32-unknown-unknown` and grep the output for your
files) rather than treating pre-existing warnings elsewhere as yours to fix.

## Layout-stable cards: reserve space, never conditionally render a container above other content

A dashboard card is a vertical stack; conditionally rendering (or early-returning
an empty view for) any block that sits ABOVE other content makes everything below
it jump when the block appears/disappears. `karaoke_panel.rs` did exactly this —
it returned `view! {}` when there was no lyric line, so the subtitles block
vanished on every pause and the card jerked up and down (#163). Fix: keep the
container ALWAYS in the DOM with a reserved `min-height` (CSS) sized for its
slots, always render each inner line slot (empty = `\u{00A0}`), and swap only the
text. The #15 preview is the good pattern to copy: `.preview-img` and
`.preview-placeholder` share one `aspect-ratio: 16/9` box so a load/unload never
shifts layout.

## Status badges only where actionable

A per-card status badge that renders the same non-actionable value on every card
(e.g. the genlock `LockBadge` showing '● UNLOCKED — pacing disabled' on all 9
cards while `genlock_pacing` is OFF) reads as N errors, not one disabled feature.
Gate it: render nothing when the state is a global no-op (`pacing.enabled ==
false`), show a per-card badge only where it is actionable (Playing/Paused
outputs), and fold the whole-box status into ONE header summary (#164,
`ndi_health.rs::should_show_lock_badge` / `global_summary`).

## Dashboard = one playlist SELECTOR + ONE work area (#165), not a grid of cards

`dashboard.rs` no longer renders `<For each=store.playlists>` of `PlaylistCard`s.
It renders a `PlaylistSelector` (left `.playlist-selector-list` rows on desktop,
a `.playlist-select-mobile` `<select>` on ≤700px — both always in the DOM, CSS
toggles) + a `PlaylistWorkspace` that reuses ONE `PlaylistCard`
(`show_badge=false`) for the selected playlist. Selection state lives in
`store.selected_playlist: RwSignal<Option<i64>>` (+ `selection_pinned`), mirrored
to the URL `?playlist=<id>` and `localStorage` via `components/selection.rs`.
Seeded once in `App()` (persisted → pinned); the dashboard's auto-follow `Effect`
defaults an UNPINNED selection to the playing playlist (first by name), so a
fresh load preselects what's playing while a reload keeps the operator's pick.
The #164 genlock badge belongs ONLY in the selector rows + the header
`GlobalLockBadge` summary — never the work area (`show_badge=false`). The
"Práve hrá" strip (`.now-playing-strip`) is always rendered with a reserved
`min-height` (layout-stable, same discipline as the karaoke panel). Any e2e that
asserts on a specific playlist's card must SELECT it first (click its
`playlist-selector-row`, or a mobile `playlist-select` option), then read the one
`playlist-workspace` card — the old per-card grid locators no longer resolve.

## A helper returning `impl IntoView` from a `&T` borrow fails E0515 inside `<For>`

Edition 2024's `impl Trait` captures ALL in-scope lifetimes, so
`fn row_view(row: &DubRow) -> impl IntoView` returns a view that BORROWS `row`
for the `&DubRow` lifetime. Call it as `{row_view(&row)}` inside a `<For>`
`children=move |row| view!{ … }` closure and rustc rejects it with
`error[E0515]: cannot return value referencing function parameter 'row'` — the
returned `view!` structure holds a reference to the closure's local `row`. The
no-compile TIER-0 box can't see it; it failed #180's Build WASM job.

Fix: pass the OWNED fields the helper needs instead of a borrow —
`fn row_view(chain_state: String, dub_error: Option<String>) -> impl IntoView`,
called `{row_view(row.chain_state.clone(), row.dub_error.clone())}`. The returned
view is then `'static`. (Alternatively `-> impl IntoView + use<>` to opt out of
lifetime capture, but owned args are clearer.) Same trap for any sp-ui helper
that takes `&SomeRow`/`&str` and returns a view used in a `<For>`/list child.

## The modern Mixer component (#181 D2) — ONE presentational widget + thin adapters

The stems (karaoke) AND dub-video controls are ONE component, not two. Do NOT add
a second mixer or restyle a per-domain panel in place (the owner: today's stems
mixer was "hrozne škaredý"; he wants ONE modern mixer everywhere).

- **`components/mixer.rs::Mixer`** is PRESENTATIONAL only: title, a state line
  (`data-testid` via `state_testid`), a `disabled_reason`, a `Vec<ChannelSpec>`
  channel strip, a `Vec<PresetSpec>` preset row (one accent on the active preset),
  and an optional `children` footer slot. When `disabled_reason` is Some+non-empty
  the root gets `mixer-locked`, which dims + `pointer-events:none` the channels and
  presets but **NEVER the footer** (the "Zaradiť do fronty" button must stay
  clickable for an unavailable song). The root class is ONE reactive closure
  (`format!("mixer {extra_class}")` + `mixer-locked`) — do not mix a dynamic
  `class=` with `class:` toggles.
- **`components/mixer_channel.rs::MixerChannel`** is one vertical fader: a ≥44px
  touch target (`writing-mode: vertical-rl; direction: rtl` — Chromium renders it
  vertical; Playwright `.fill("20")` still sets the value regardless of
  orientation), a live value readout, `on:input` updates the gain signal, `on:change`
  commits via `on_change`. **`ChannelSpec.enabled` is a `Signal<bool>`, NOT a plain
  bool** — a plain bool is captured once and would not re-disable the fader when the
  active preset changes (the karaoke `vokál` fader is off only in Plný mix, #186).
  A read-only display channel passes `enabled: Signal::derive(|| false)` + a
  `fixed_note` ("pevné" / "podklad") so the UI is honest about what it does not
  control.
- **The pure preset/fader math lives in `sp_core::mixer_model`, NOT in sp-ui.**
  sp-ui has **no unit-test job** (CI `Test` = `cargo test --workspace`, and sp-ui
  is OUTSIDE the workspace; `test-wasm` is only `cargo check -p sp-core`). So the
  mappings (`song_gains_for_preset`/`song_preset_for_gains`, dub
  `ratio_to_faders`/`faders_to_ratio`/`dub_ratio_for_preset`/`dub_preset_for_ratio`,
  the unified `gains_for_preset`/`preset_for_gains`, `channel_labels`, `presets`)
  live in `crates/sp-core/src/mixer_model.rs` (WASM-safe, covered by the workspace
  Test job + the diff-scoped mutation gate) and sp-ui imports them. Preset matching
  quantises to integer permille so it tolerates the UI's integer-percent fader
  rounding.
- **Adapters** (thin, own the API wiring): `components/karaoke_mixer.rs` (songs →
  `POST /api/v1/karaoke`; keeps the #177 state contract — the `karaoke-now-playing`
  "Stemy — …" line the post-deploy spec reads, the `karaoke-mode` presets group,
  the `karaoke-vocal-gain` fader, the "Zaradiť do fronty" enqueue) replaces the
  deleted `karaoke_control.rs` on the dashboard; `components/dub_mixer.rs` (dub
  videos → `PATCH /api/v1/videos/{id}/dub-mix`; the `dabing` fader is the live ratio
  `r`, `originál hlas` is a read-only bed display `= ratio_to_faders(r,has_stems)[0]`
  synced by an `Effect`, `ambient` is fixed) renders per row in `dabing_list.rs`.
- **Deferred (needs a server change, out of this lane's scope):** the design also
  wants the dub mixer on a normal playlist card whose now-playing video has
  `dub_status = ready`. The dashboard `now_playing` WS payload (`NowPlayingInfo`)
  does NOT carry `dub_status`, so that placement needs the server to add it to the
  now-playing message first — the dub mixer ships on the Dabing page only until then.
