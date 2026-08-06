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

## Root `cargo fmt` / `cargo clippy` do NOT check `sp-ui` or `src-tauri`

Both are excluded from the root `Cargo.toml` workspace (different toolchain
needs — WASM / Tauri bundler). CI's `lint` job runs `cargo fmt --all --
--check` and `cargo clippy --workspace -- -D warnings` from the repo root,
so neither ever touches `sp-ui/` or `src-tauri/`. `build-wasm` only runs
`trunk build --release` — no fmt/clippy step at all for sp-ui.

Consequence: `sp-ui` has accumulated real rustfmt drift across many
pre-existing files (verified 2026-08-06: `cd sp-ui && cargo fmt --all --
--check` fails on ~10 files unrelated to any single change).

**Never run a blanket `cd sp-ui && cargo fmt --all`** — it silently rewrites
every drifted file into your diff. Instead, format-check ONLY the files you
touched: `rustfmt --edition 2024 --check src/components/foo.rs
src/components/bar.rs`. Same idea for clippy — scope to what you changed
(`cargo clippy --target wasm32-unknown-unknown` and grep the output for your
files) rather than treating pre-existing warnings elsewhere as yours to fix.
