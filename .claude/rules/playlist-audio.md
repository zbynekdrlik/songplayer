---
paths:
  - "crates/sp-core/src/audio_fx*.rs"
  - "crates/sp-server/src/playback/playlist_fx*.rs"
  - "crates/sp-server/src/playback/pipeline_paced.rs"
  - "crates/sp-server/src/api/playlist_audio*.rs"
  - "crates/sp-server/src/db/models_playlist_fx*.rs"
  - "crates/sp-server/src/db/mod_tests_v33.rs"
  - "sp-ui/src/components/playlist_audio*.rs"
  - "sp-ui/src/components/playlist_card.rs"
  - "e2e/mock-api.mjs"
  - "e2e/playlist-audio.spec.ts"
  - "e2e/post-deploy-playlist-audio.spec.ts"
---

# A playlist's own volume + EQ (#242; the panel rebuilt live, #278)

The owner's ruling (#242): every playlist has its OWN volume (dB) and its
OWN parametric EQ, set per playlist and per node on the Dashboard. No
seeded values (a new playlist and every migrated one start untouched), no
fixed curve from cg OBS's old filters, no "one pinned song" (PP's 90s is a
separate one-song playlist in mode Single).

## The pieces

- **Model + maths, `sp_core::audio_fx`** (WASM-safe, unit-tested,
  mutation-gated — sp-ui has no unit-test job). `PlaylistFx { gain_db, eq:
  Vec<EqBand> }`, `EqBand { kind, freq_hz, gain_db, q, enabled }`,
  `deny_unknown_fields`. Limits: volume and band gain −30..=+12 dB, 20..=20 000
  Hz, Q 0.1..=10, at most 8 bands; `validate` → `FxError` (English
  `Display` for the API, `sk()` for the dashboard, bands counted from 1).
  RBJ biquads (`coefficients`, freq clamped to 0.49 × rate), `response_db`
  (volume + every ENABLED band), `curve` / `curve_path` (log axis, the box
  `CURVE_DB_MIN` −36 .. `CURVE_DB_MAX` +18 dB, clamped).
- **Rows, V33**: `playlists.audio_gain_db REAL NOT NULL DEFAULT 0` +
  `audio_eq TEXT NOT NULL DEFAULT '[]'`. `models_playlist_fx`: a row whose
  JSON does not read (or fails `validate`) plays untouched, with a WARN.
- **Live register, `playback::playlist_fx`**: one `FxSlot` per playlist id
  (settings + an `AtomicU64` generation). Startup `load_all`s every row
  BEFORE `create_startup_pipelines`. The API writes the row FIRST, then
  `global().set`s it.
- **Where it applies**: the decode thread wraps each song's audio stream
  right after the stem mix (`pipeline_paced::run_decode_producer`,
  `playlist_fx::wrap`), so the preview tap, the pacer, the program bus and
  every audio output (NDI, VBAN, ASIO) get the processed audio, and a cut or fade between playlists mixes processed
  audio. The wrapper polls its slot's generation on EVERY chunk: a change
  reaches the playing song at its next chunk.
- **DSP, `playback::playlist_fx_dsp::FxProcessor`**: transposed direct form
  II per channel, f64 state. A volume change ramps over `FX_RAMP_FRAMES`
  (2400 = 50 ms at 48 kHz) by a counter and lands exactly; a change of the
  filters CROSSFADES the old cascade into the new one over the same 2400
  frames (no click, no state reset); a change that lands while a fade
  still runs WAITS for its end (`pending`, the latest wins: cutting a fade
  short jumps); a volume-only change keeps the filter state. The default sound (0 dB, no enabled band) is BIT-IDENTICAL
  passthrough — never multiply by 1.0 "to be safe" outside a ramp.
- **API**: `GET /api/v1/playlists/{id}/audio` → `{gain_db, eq, generation}`;
  `PUT` (raw `Bytes` → `serde_json::from_slice::<PlaylistFx>`, so a bad body
  is 400 `audio body: …`, a limit 400 with `FxError`'s text) → 204 with no
  body (`put_json_empty` on the dashboard); 404 for an unknown playlist,
  nothing written on any refusal.
- **Dashboard (#278 rebuilt it; design comment 6221171333, the owner's
  ROZHODNUTÉ 6220698978: "the EQ processes correctly, the UI is the
  problem")**: the collapsed "Zvuk playlistu" panel on the playlist card,
  under the Player, modelled on iemmixer's `iem-ui/src/components/eq_modal.rs`.
  It loads on first open and shows its fields ONLY once the load landed
  ("Načítavam…" before: an edit made earlier was overwritten by the load,
  CI run 37975959175; `/__mock/playlist-audio-delay` reproduces it).
  - `playlist_audio.rs`: the state (`EqEdit`, all `Copy` handles: `fx`, a
    band id per `fx.eq` entry in `ids`, the cards' `order`, the send gate,
    the status), the volume row, "Pridať pásmo" (one button per type,
    `playlist-audio-add-<kind>`), the status line. `playlist_audio_curve.rs`:
    the SVG. `playlist_audio_band.rs`: one card.
  - **Per-type defaults, ONE table** `sp_core::audio_fx::default_band`:
    high-pass 80 Hz, low shelf 120 Hz, peak 1 kHz Q 1, high shelf 8 kHz,
    low-pass 16 kHz, the rest Q 0,707, 0 dB, on. A type change is
    `with_kind`: the new type's frequency and Q, the gain and on / off kept.
    (Before #278 every new band was a 1 kHz peak: a high-pass made from it
    cut at 1 kHz, the owner's measurement.)
  - **The curve**: `viewBox` 800×300 (`audio_fx_view::BOX_W/BOX_H`), the
    card's width, `aspect-ratio` kept, `preserveAspectRatio="none"`; the
    box stays −36…+18 dB (`CURVE_DB_MIN/MAX`). Grid `FREQ_GRID` (20 Hz …
    20 kHz) + `DB_GRID` (every 6 dB, the 0 dB line brighter); the summed
    curve `curve_path` + its fill `fill_path` (closed to 0 dB); each
    ENABLED band's own curve `band_path` (its filter alone, no volume; off
    = `d=""`) in its type's colour (`--eq-kind` on `.kind-<name>`); one
    handle per band at `handle_xy` (a high-/low-pass on the 0 dB line).
  - **A handle's drag**: `pointerdown` captures the pointer on the circle
    (`set_pointer_capture`), `pointermove` maps the pointer into the box
    (`to_box` over the SVG's own `getBoundingClientRect`: the SVG has NO
    border or padding, its frame is an `outline` drawn inside, else the
    mapping is off by the border and a level drag reads −0,1 dB) and sets
    the band
    to `dragged(..)` (frequency always, gain only for a shelf / peak, both
    snapped); `pointerup` / `pointercancel` / `lostpointercapture` end it
    once (`finish`). The wheel over a handle: `wheel_q` (×1,1 per notch of
    100, up narrows) through the handle's own `WheelTurn`, which adds up
    small events until they reach the next 0,01 (a touchpad's few pixels
    never stick, nor move Q faster than a mouse wheel; a turn the other
    way starts afresh, so a turn past a limit is never owed); a direct,
    non-passive listener, so `preventDefault` keeps the page still.
  - **The cards** go by frequency, a tie by iemmixer's type rank
    (`display_order`), numbered like their handles. The order is taken at
    the load, an add, a remove and every gesture's end (`EqEdit::finish`),
    NEVER during a drag; cards and handles are `<For>`s keyed by the band
    id, so a new order MOVES a card (the same element, the spec pins it)
    and an edit re-creates nothing. A moved card's focused control loses
    the focus (the browser re-inserts it), so `finish` gives it back in
    the next animation frame, with `preventScroll` (web-sys
    `FocusOptions`): the page never jumps. A card reads its band through a
    `Memo`.
    Every `<option>` carries a reactive `selected` (the #233 trap).
  - **Sliders**: frequency and Q are log sliders over 0…1000
    (`freq_slider`/`slider_freq`, `q_slider`/`slider_q`); gain and volume
    are native ranges over −30…+12 dB, step 0,1. Values shown in Slovak
    (`freq_label` "1,25 kHz", `gain_label` "+2,5 dB" / "0,0 dB" / "−3,5 dB",
    `q_label` "Q 0,71"); what is shown is what is sent (`snap_freq`: three
    significant digits, `snap_gain` 0,1 dB, `snap_q` 0,01). A double-click
    on a row (`band-<field>-value`, `playlist-audio-gain-value`) resets
    that value to the type's default (the volume to 0 dB); the greyed gain
    row of a high-/low-pass resets nothing (it keeps the gain it does not
    use for a later shelf or peak).
  - **Live, no "Použiť"**: every change is `PUT …/audio` of the settings
    AS THEY ARE at the send. The rule is the pure
    `sp_core::audio_fx_send::SendGate` (`changed` / `flush` / `landed` /
    `tick` → `Idle` / `Send` / `WaitMs`): at most one send per 50 ms
    (`THROTTLE_MS`, from the last send's start), never a second request in
    flight, a gesture's end (`finish`: a release, a switch, a type, a reset,
    an add, a remove) sends at once. A slider commits on `pointerup` AND
    `change` (#200: never on `change` alone); a lost release costs nothing,
    the armed timer still sends the last value. An answer is awaited
    `ANSWER_TIMEOUT_MS` (10 s), then given up (`SendGate::gave_up`:
    the settings as they are go out again, since what the server holds
    is unknown; "Uloženie zlyhalo: server neodpovedá — skúšam znova"), so
    a server that never answers cannot hold the gate in flight. A request
    given up can still land later and briefly hold an older value, until
    the re-send lands. `validate` runs before
    every send (`FxError::sk` on the status line). Status line: "Ukladám…"
    while busy, then "Uložené", or "Uloženie zlyhalo: …".
  - The playing song follows each PUT through the DSP's 50 ms crossfade;
    back-to-back PUTs queue in its `pending` (the latest wins).

## Tests

- `audio_fx_tests.rs` (+ #278: the wire names, labels, `default_band`,
  `with_kind`), `audio_fx_view_tests.rs` and `audio_fx_send_tests.rs`
  (#278: every pin from a scratch model of the same formulas / the gate,
  each listed mutant mapped to a killing test), `models_playlist_fx_tests.rs`, `mod_tests_v33.rs`,
  `playlist_fx_dsp_tests.rs` (measured gain of each band type against
  `response_db`, ±0.05 dB; ramp; crossfade formula; bit identity),
  `playlist_fx_tests.rs` (a change while a song plays reaches its next
  chunk), `playlist_audio_tests.rs` (through the real router).
- Mock E2E `e2e/playlist-audio.spec.ts` (#278, viewport 1600×1000, REAL
  mouse drags of the handles and sliders, the wheel, double-clicks; it
  asserts the visible labels, the cards' `data-*` numbers and the PUT
  bodies the mock received; a keyboard edit that moves a card keeps its
  focus; a refusal, a 500 and an answer that never comes each say so).
  Mock knobs `/__mock/playlist-audio{,-reset,-set,-delay,-put-delay,-put-status}`
  (a reset also makes a delayed PUT that arrived before it store nothing):
  `/__mock/playlist-audio` returns `{puts: [{id, body, at, done}], last,
  stored}` (arrival / answer on the mock's clock: the drag test holds the
  button down after 20 quick moves and needs the latest position on the
  server BEFORE the release, in fewer than 20 PUTs ≥ 40 ms apart; with
  `-put-delay` every PUT starts after the one before was answered); the
  mock answers PUT 204 and 400 with the server's texts.
- Post-deploy `e2e/post-deploy-playlist-audio.spec.ts` (SNV; not in PP's
  subset), the owner's own path (#278 review round 1): picks a playlist
  that is not decoding (health `transport`) with room for a band, checks
  the panel shows its stored sound (the volume label, one card per band by
  its `data-index` with its numbers, one handle per band) and that opening
  it saved nothing; then "+ Pásmo (zvon)" through the panel (a 1 kHz 0 dB
  peak: a UNITY filter, the RBJ peak at 0 dB has b = a, so its TDF-II
  stage passes every sample; on a playlist with enabled bands it is, as
  for any EQ edit, the 50 ms crossfade into a fresh cascade of the same
  filters) must reach the row at once (generation +1),
  and its "Odstrániť" must bring the row back exactly as stored
  (generation +2); `finally` PUTs the stored sound back if the row
  differs. That unity peak is the only other value a gate may ever write
  on a box: never any audible one.
