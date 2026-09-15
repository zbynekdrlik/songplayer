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
