---
paths:
  - "crates/sp-core/src/audio_fx*.rs"
  - "crates/sp-server/src/playback/playlist_fx*.rs"
  - "crates/sp-server/src/playback/pipeline_paced.rs"
  - "crates/sp-server/src/api/playlist_audio*.rs"
  - "crates/sp-server/src/db/models_playlist_fx*.rs"
  - "crates/sp-server/src/db/mod_tests_v33.rs"
  - "sp-ui/src/components/playlist_audio.rs"
  - "sp-ui/src/components/playlist_card.rs"
  - "e2e/mock-api.mjs"
  - "e2e/playlist-audio.spec.ts"
  - "e2e/post-deploy-playlist-audio.spec.ts"
---

# A playlist's own volume + EQ (#242)

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
- **Dashboard, `sp-ui/src/components/playlist_audio.rs`**: the collapsed
  "Zvuk playlistu" panel on the playlist card, under the Player. It loads
  on first open; "Použiť" validates with `sp_core::audio_fx::validate`
  (Slovak `sk()` on the status line) and saves nothing before the load
  landed. The band rows are rebuilt only when the band COUNT changes
  (`Memo` of `eq.len()`); every field reads its band by index, so an edit
  never re-creates the input under the operator (the Tab-focus test pins
  it). Every `<option>` carries a reactive `selected` (the #233 trap).

## Tests

- `audio_fx_tests.rs`, `models_playlist_fx_tests.rs`, `mod_tests_v33.rs`,
  `playlist_fx_dsp_tests.rs` (measured gain of each band type against
  `response_db`, ±0.05 dB; ramp; crossfade formula; bit identity),
  `playlist_fx_tests.rs` (a change while a song plays reaches its next
  chunk), `playlist_audio_tests.rs` (through the real router).
- Mock E2E `e2e/playlist-audio.spec.ts` (mock knobs
  `/__mock/playlist-audio{,-reset,-set}`; the mock answers PUT 204 and 400
  with the same texts).
- Post-deploy `e2e/post-deploy-playlist-audio.spec.ts` (SNV; not in PP's
  subset): reads the first playlist's stored sound, checks the panel shows
  it, saves it UNCHANGED (no audible change) and reads the generation moved
  by one. Never write other values on a box from a gate.
