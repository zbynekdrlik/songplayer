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
- **Deferred (needs a server change, out of this lane's scope — returned as a
  #181 follow-up candidate for the supervisor to file):** the design also wants the
  dub mixer on a normal playlist card whose now-playing video has
  `dub_status = ready`. The dashboard `now_playing` WS payload (`NowPlayingInfo`)
  does NOT carry `dub_status`, so that placement needs the server to add it to the
  now-playing message first (a `ServerMsg::NowPlaying` + `NowPlayingInfo.dub_status`
  change). The dub mixer ships on the Dabing page only until that follow-up lands.

## One capability = one component on every page (#194)

The app is ONE application and must behave the same on every page. A capability
that appears on more than one page is ONE shared component, rendered identically
everywhere — never a page-local re-implementation. Three playback UIs, three
transport vocabularies, seek-only-on-Live and preview-only-on-Dashboard were the
exact divergence #194 fixed. The owner's rule: "je to jednotna aplikacia a mala
by sa aj spravat jednotne".

**The playback surface is `components/player.rs::Player(playlist_id)`** — the ONE
composition of now-playing (title/artist/state), the on/off-program badge, the
seek bar (`player-seek` + `player-back10`/`player-fwd10`, backed by the pure
`sp_core::seek_model` helpers), transport (`player-prev`/`player-playpause`
toggle/`player-skip`), the mode select (`player-mode`), the click-to-start live
A/V preview slot (`PreviewVideo`, behaviour unchanged), and the mixer slot
(the shared `Mixer` via the karaoke adapter for a song, the dub adapter for a
dub video — chosen from the PLAYING item: the now-playing `video_id` looked up in
`store.dabing`, dub row → `DubMixer`, else `KaraokeMixer`). It is rendered by the
Dashboard card, the Live page and the Dabing page. There is no page-local
now-playing / transport / seek / preview widget — deleting them (not keeping them
beside the shared one) is part of adding the shared one.

**Testid convention: `<component>-<part>`, set INSIDE the shared component,
NEVER injected by the caller.** So every page that shows a capability exposes the
SAME ids. The Player sets all `player-*` ids itself; the preview keeps its own
`preview-*` ids; the mixer adapters keep `karaoke-*` / `dub-*` ids. A test that
asserts on a capability locates it by the shared id and it resolves on every page
that shows it.

**Slovak is the ONE operator-UI language** — labels, buttons, tooltips,
empty/error/loading text. The only exceptions are the fleet genlock vocabulary
(`LOCKED`/`DEGRADED`/`UNLOCKED`/`GENLOCK OFF`) and proper names (OBS, NDI,
Resolume, SongPlayer). Mixed English/Slovak in one surface is a review reject.

**On/off-program is derived from `store.ndi_health`** (the health snapshot maps a
Playing-but-off-program pipeline to `Paused`, so `state == "Playing"` means the
wall shows this output) — no server change, no new field. A dub prepared on the
Dabing playlist plays OFF-program and reads "○ Mimo programu".

### The shared row / chips / import contracts (#194 round 2)

One song looks and behaves the same on every page because ONE component renders
it. Never re-implement a row, a status marker, or an import field per page.

- **`components/song_row.rs::SongRow`** is the ONE row: a `<div class="song-row"
  data-testid="song-row" data-video-id=…>` with an optional primary play action
  (`data-testid="song-row-play"`, glyph `▶`, disabled via `play_ready=false`,
  same Slovak tooltip everywhere), a `song-row-title` (title — artist), the shared
  `StatusChips`, and a page-supplied actions slot (`children`). Every page passes
  ONLY the chips it has data for and ONLY the actions it needs; the play action is
  omitted where the page has none (the catalog uses "+ Pridať" instead). It is a
  `<div>` flex row, not a `<table>` row — the Dashboard/Live/catalog lists that
  used `<table>` are now `<div class="song-list">` of `SongRow`s.
- **`components/status_chips.rs::StatusChips`** renders a `Vec<ChipView>` as
  `<span class="status-chips" data-testid="status-chips">` with per-chip
  `data-testid` = the kind (`chip-stems`/`chip-text`/`chip-dub`/`chip-file`) and
  class `status-chip chip-<tone>`. A `ChipView` is a `StatusChip` + an optional
  longer `title` tooltip (e.g. the dabing chain folds into the dub chip's tooltip;
  the lyrics source+quality fold into the text chip's tooltip).
- **The label/tone vocabulary is PURE, in `sp_core::status_chip`** (WASM-safe,
  unit-tested, mutation-gated — sp-ui has no unit-test job). sp-ui builds chips
  via `stems_chip` / `text_chip` / `dub_chip` / `file_chip` and renders whatever
  they return. NEVER write a second glyph/word table in sp-ui (the old
  `video_list_stems::stems_glyph` + `karaoke_mixer::state_label` did, and diverged
  — both now defer to the shared model).

  | Chip | Wire field(s) | States → Slovak label |
  |---|---|---|
  | `chip-file` | `cached`, `normalized` | normalized→`stiahnuté` · cached→`sťahuje sa` · else→`chýba` |
  | `chip-stems` | `stems_state` (+ opt. queue pos) | ready→`hotové` · processing→`spracúva sa` · queued→`vo fronte (N.)` · unavailable/absent→`nedostupné` · failed→`chyba` |
  | `chip-text` | lyrics `has_lyrics`/`lyrics_reference`/`is_stale` | reference→`★ overený` · stale→`zastaraný` · has-lyrics→`základný` · none→`chýba` |
  | `chip-dub` | dub `chain_state`/`dub_status` | ready→`hotový` · failed→`chyba` · queued→`vo fronte` · none/absent→`—` · mid-chain→`beží` |

  The Dashboard videos payload does NOT carry the lyrics text-state, so
  `video_list` JOINS `GET /api/v1/lyrics/songs?playlist_id=<id>` by `video_id` to
  build its `chip-text` (the same join `live_setlist` already does) — a frontend
  join, no server change. Fold the join into the ONE `videos.set(...)` so the
  `<For>` key can include the text fields (avoids the stale-`<For>` trap).
- **`components/import_box.rs::ImportBox`** is the ONE paste-URL box
  (`data-testid="import-box"` / `import-input` / `import-btn` / `import-status`),
  Slovak ("Pridať" / "Pridávam…" / "Vlož URL videa …"), parameterised only by
  `ImportTarget::{Playlist(id), Dabing}`. Live + Dabing use it identically.

### Two round-2 CI traps (learned #194 r2)

- **Gating the Player mixer slot on `has_content()` removes the
  `karaoke-now-playing` state line when idle.** That line (the #177 contract read
  by `post-deploy.spec.ts`) lives INSIDE the `KaraokeMixer`, which only mounts
  when a song plays. When nothing plays the slot collapses to
  `player-mixer-idle` ("Mixér — nič nehrá") and `karaoke-now-playing` is absent.
  Any spec asserting that line must accept EITHER surface (`.or()` +
  branch on `count()`), because the box is often idle during CI.
- **A Playwright test that DELIBERATELY forces a non-2xx (e.g. `/__mock/fail-mode`
  to 500 to exercise `player-error`) makes the browser log a
  `"Failed to load resource: … 500"` console error.** The shared zero-console
  `afterEach` then fails. Strip that intentional entry from the collected
  messages in the test's `finally` (or add it to that test's allow-list) — it is
  the point of the test, not a bug.
- **`Option::<T>::new()` does not exist** — `Option` has no `new`; seed an
  `RwSignal<Option<T>>` with `None::<T>`. The TIER-0 no-compile box can't catch
  it; it fails Build WASM (`E0599`).

### Cross-page review checklist (run for every lane that touches `sp-ui`)

Before a `sp-ui` lane is done, the integration review does a CROSS-PAGE pass, not
only a code pass:

1. Screenshots of ALL pages (Dashboard, Live, Lyrics, Dabing, Settings) from the
   mock build (`trunk build` → `node e2e/mock-api.mjs` → Playwright at 1400×1000).
2. Same capability = same component, same place, same testids, same labels, same
   language on every page it appears.
3. No page-local widget for a capability another page already renders (a new
   page-local widget for an existing capability is a design REJECT).
4. Zero console errors on every page.
5. The verdict line: `🏛 Architektúra: <code area> — OK · UI-konzistencia — OK | REWORK`.

## A live data tick must update TEXT ONLY — never re-create a child component (#194)

The owner hit this on the box: a Dabing dub video "len pada a nespusta sa to"
and the mixer faders "sa nedali normalne hybat". Both were the SAME class of
bug — a now-playing POSITION tick (arriving ~2×/s over the WS) re-created a live
child under the operator, tearing down its preview WebSocket and resetting a
mid-drag fader. Two rules prevent it:

### Rule 1 — a slot closure re-runs on every dependency it READS; read only Memos

A `{move || …}` view closure re-runs whenever ANY signal it reads fires — and a
signal fires on EVERY `.set()`, even to an unchanged value. So:

- A **plain closure** derivation (`let has_content = move || store.now_playing.
  get()…`) inlined into a slot closure makes that slot RE-SUBSCRIBE to
  `store.now_playing` and RE-RUN on every position tick → its child is
  re-created twice a second. `player.rs`'s mixer slot did exactly this and the
  faders reset mid-drag.
- Convert every bool/enum derivation a slot closure reads to a **`Memo`**
  (`let has_content = Memo::new(move |_| …)`). A `Memo` only PROPAGATES when its
  value actually changes, so a position tick (which flips nothing) never re-runs
  the slot. The child re-creates ONLY when the identity it keys on changes (the
  mixer keys on `mixer_choice: Memo<Option<DubRow>>` — a by-value `DubMixer`
  MUST re-create when the playing video changes; `KaraokeMixer` takes only
  `playlist_id` and reads now-playing reactively, so it stays mounted and just
  updates its text).
- The **preview** slot is keyed on the constant `playlist_id`, NOT the video id:
  the preview WS/encoder child is per-PLAYLIST and continuous (preview.md), so it
  must survive both position ticks AND song changes; it re-creates only when
  `is_decoding`/`preview_on` flip (both `Memo`/intentional signals).
- A parent that MOUNTS a shared child from a signal (`pages/dabing.rs` mounts
  `<Player playlist_id=id>` from `dabing_pid`) must (a) only `.set()` that signal
  when the value CHANGES (`if sig.get_untracked() != new { sig.set(new) }` — a
  poll that re-sets an unchanged id every tick re-creates the whole child), AND
  (b) mount through a `Memo<Option<i64>>` so an accidental same-value fire from
  anywhere still can't re-create it.

### Rule 2 — every draggable control needs a pointer DRAG GATE

A live value binding (`prop:value=move || live()`) FIGHTS a drag: a tick
re-applies the live value and snaps the thumb / fader back under the finger. Gate
it with a `dragging` signal:

- `dragging: RwSignal<bool>` set on `on:pointerdown`/`on:touchstart`, cleared on
  `on:pointerup`/`on:touchend`/`on:pointercancel` AND `on:change`.
- `prop:value` (and the readout) read a PURE gate helper —
  `seek_model::seek_display_ms(dragging, dragged, live)` /
  `mixer_model::fader_display_pct(dragging, dragged, live)` (`if dragging {
  dragged } else { live }`) — so while dragging the DRAGGED value wins and no
  live update (a store `Effect`, a re-load) can overwrite it.
- `on:input` while dragging updates ONLY the local dragged signal (never the
  shared `gain`/position — that's what an external sync would fight); commit
  EXACTLY ONCE on release via `on:change` (which sets the shared signal + fires
  the single POST/PATCH). The pure helpers live in `sp_core` (unit-tested +
  mutation-gated — sp-ui has no unit-test job).
- **`on:change` MUST commit the DRAG SIGNAL, never `event_target_value` (the
  DOM).** A real browser fires `pointerup` (which clears `dragging`) BEFORE
  `change` on a slider release, so by the time `change` runs, `prop:value` has
  already re-applied the live value and SNAPPED the DOM back — reading the DOM
  there commits the live position / pre-drag gain, silently losing the drag.
  Read the pending drag signal (`seek_drag_ms.get_untracked()` /
  `drag_pct.get_untracked()`) instead; make `on:input` record it on EVERY input
  (drag and keyboard) so the same commit path serves both. **An e2e that
  dispatches `change` while still "dragging" will pass on broken code** — the
  drag specs MUST dispatch `pointerup` → `change` in that (real) order.
- Clear `dragging` on `pointerup`/`touchend`/`pointercancel` (a no-move click
  fires no `change`, so the release handlers are the only reliable un-stick), and
  read a page-owned signal in any `spawn_local` follow-up with `try_get_untracked`
  (a plain read after navigation panics).
- The pointer/touch pair can't share ONE closure — `on:pointerdown` gets a
  `PointerEvent`, `on:touchstart` a `TouchEvent`, so a single `move |_|` closure
  would fix its param type on first use and fail the second. Inline a separate
  closure per event (they capture only `Copy` signals, so this is cheap).

### Rule 3 — a real-user interaction spec MUST run with the mock TICKING

The suite stayed green while the app felt broken because the mock never advanced
now-playing DURING a drag/preview. Any spec that asserts an interaction
(drag/seek/preview persistence) must turn on the mock's 500 ms position tick
(`POST /__mock/tick {enabled, items:[{playlist_id, video_id, duration_ms,
state:"Playing"}]}`) and turn it OFF in `afterEach` (it is global in-memory
state; a single worker runs files serially, so a leaked ON breaks later specs).
The preview WS only opens under branded `chrome` (bundled Chromium lacks
H.264/AAC → the MSE shim's `addSourceBuffer` throws and `_openWs` is skipped), so
the WS-opened-once/never-closed proof lives in `preview.spec.ts` (the `chrome`
project); `player-liveness.spec.ts` (chromium) proves element-IDENTITY stability
(`elementHandle().isConnected`) + the drag gates, which are codec-independent.

## The final #194 shared component set (round 3c) — this IS the reality

The whole-app unification (#194) is complete. Every capability that appears on
more than one page is ONE shared component; there are no page-local
re-implementations left. The removed components (`now_playing_card.rs`,
`playback_controls.rs`, `import_url_box.rs`, `video_list_stems`'s glyph table,
`karaoke_control.rs`, `obs_status.rs`, `resolume_health.rs`, `lan_address.rs`,
`playlist_selector.rs`, `karaoke_panel.rs`, `lyrics_scroller.rs`) are GONE — do
not re-create any of them beside the shared one.

| Capability | Shared component | Testid(s) |
|---|---|---|
| playback surface | `player.rs::Player(playlist_id)` | `player`, `player-*`, nests preview `preview-*` + mixer `karaoke-*`/`dub-*` + lyrics `lyrics-view` |
| one song row | `song_row.rs::SongRow` | `song-row`, `song-row-title`, `song-row-play` |
| status vocabulary | `status_chips.rs::StatusChips` (pure `sp_core::status_chip`) | `status-chips`, `chip-file`/`chip-stems`/`chip-text`/`chip-dub` |
| paste-URL import | `import_box.rs::ImportBox(ImportTarget)` | `import-box`, `import-input`, `import-btn`, `import-status` |
| status strip (every page) | `health_bar.rs::HealthBar` (pure `sp_core::health`) | `health-ws`/`health-obs`/`health-genlock`/`health-resolume`/`health-tools`/`health-lan`/`health-version` (nests `version`) |
| loading / empty / error | `state_block.rs::StateBlock{Loading,Empty,Error}` | `state-loading`/`state-empty`/`state-error` |
| playlist chooser (every page) | `playlist_picker.rs::PlaylistPicker(kinds?)` | `playlist-picker`, `playlist-picker-item`, `playlist-picker-select`, `playlist-picker-list` |
| lyrics surface | `lyrics_view.rs::LyricsView(playlist_id?, video_id?)` | `lyrics-view` |
| one poll helper | `store::poll_into` / `store::poll_value` | — |

### `PlaylistPicker` — one chooser + one selection state (#194 r3c)

`playlist_picker.rs::PlaylistPicker` is the ONE playlist chooser, on Dashboard,
Live and Lyrics. Selection lives in `store.selected_playlist` (+ the
`components/selection.rs` helpers). An optional `kinds: Vec<String>` prop
restricts the list by playlist `kind` — Live passes `["custom"]` so only the
live-kind playlist shows and is pre-selected THROUGH the picker (via
`selection::selection_or_first_of_kind` — never a hardcoded `name == "ytlive"`
lookup). The CSS class names stay `playlist-selector-*` (so the historical
`.playlist-selector-row .lock-badge` style + a couple of class-based post-deploy
locators keep resolving) while the testids are `playlist-picker*`. Both `<For>`
(desktop list + mobile `<select>`) read ONLY `store.playlists` (#170: a
position tick never re-orders the rows).

### `LyricsView` — ONE tappable lyrics surface everywhere (#194 r3c)

`lyrics_view.rs::LyricsView` replaces BOTH `karaoke_panel.rs` (the Dashboard's
4-line preview) AND `lyrics_scroller.rs` (the Live tappable list) with a SINGLE
scrollable, tappable line list — one surface on every page (the "jednotná
aplikácia" rule; a compact-vs-scroll `mode` split would itself be the per-page
inconsistency the ticket fixes, and the Live tap-to-seek must not be lost —
`tests/live-mobile.spec.ts` pins it).

- It renders the song's `LyricsTrack` (from `api::get_video_lyrics`) as `<li>`
  `.lyr-line` buttons; the CURRENT line is highlighted by a `Memo` of the live
  position (the pure `sp_core::lyrics::LyricsTrack::current_line_index` = last
  line whose `start_ms <= pos`, held through gaps — RED→GREEN + mutation-gated);
  tapping a line seeks. Empty / no-track → the shared `StateBlock` (`Žiadny
  text`). Root class `.lyrics-view-scroll`, `data-testid="lyrics-view"`.
- Inputs: `playlist_id` (the Player passes it — the current video + seek target
  come from `store.now_playing[playlist_id]`) OR `video_id` (the Lyrics details
  view passes it explicitly; the seek target is whichever playlist plays it).
- Reactivity: `effective_vid` is a `Memo` so the fetch Effect re-runs only when
  the video changes (never per tick); the `<ol>` is built inside a closure that
  reads only `track` (rebuilt on a song change, never on a tick); a position tick
  flips only the highlighted `<li>` class. The `lyrics-view` root is stable.
- Rendered in the `Player`'s lyrics slot (`.player-lyrics`) on Dashboard, Live and
  Dabing (a dub's subtitles are just its `LyricsTrack`), and in the Lyrics details
  view.

### One operator language: Slovak (the `slovak-only.spec.ts` gate, #194 r3c)

`e2e/slovak-only.spec.ts` asserts that NONE of the audit's English UI-chrome
strings appears as EXACT visible text inside `main.content` on all five pages
(the navbar tabs + the HealthBar sit outside `main.content` and carry only the
allowed product/technical names — OBS/NDI/Resolume/SongPlayer/WS/LAN + the
genlock LOCKED/DEGRADED/UNLOCKED/GENLOCK OFF vocabulary). Exact-text matching
keeps song titles / playlist names (data that merely CONTAINS an English word)
from tripping it. When you add a new UI string, it is Slovak, or the gate fails.

## Navigation tabs are part of the Slovak-only surface (#194 round-3c review)
The `nav.navbar` buttons read `Prehľad · Naživo · Texty · Dabing · Nastavenia` and
carry `data-testid="nav-dashboard|nav-live|nav-lyrics|nav-dabing|nav-settings"`.
Specs click tabs by testid, never by text; `e2e/slovak-only.spec.ts` scans the
tabs too (`BANNED_NAV`). Only product/technical names (OBS, NDI, Resolume,
SongPlayer, WS, LAN, the genlock words) stay English anywhere in the chrome.
