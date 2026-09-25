# Autopilot log

One terse line per issue/round: decisions, key commits, verification.

- **#196 round 1 (restart-safe NDI, server)** — v0.60.0-dev.7. Deterministic
  restart-safe sender identity: port-availability wait + serialized id-order
  `send_create` (`playback/startup_senders.rs`, `runtime_pipeline.rs::ensure_pipeline_inner`);
  advertised-URL getter (`sp_ndi::source_url::parse_source_url`,
  `NDIlib_send_get_source_name` FFI) surfaced as `sender_url` on
  `/api/v1/ndi/health`; no dark-wall ladder for an output with no OBS input
  (`effective_dark_reason` + `PlaybackEngine::output_has_obs_input`). Commits:
  RED c92bb6f → GREEN 80e2467, fixes ec92122 / 50989f2 / eda1a88. CI green incl.
  Windows Build + Deploy + E2E (run 35473914064). Box-verified: no dark wall
  after the deploy restart (on-program SP-slow connections=2, all outputs 2–4).
  FINDING: `NDIlib_send_get_source_name().p_url_address` is empty for a local
  sender → `sender_url` reads null on the box; name→port visibility needs a
  different mechanism (NDIlib_find / own listen ports) — folded into round 2.
  REMAINING: round 2 (item 4 self-check, item 6 E2E one-restart, HealthBar,
  playbook), the sender_url mechanism revision, and the 10-restart box acceptance.

## #184 round G1 — the mixer console remembers faders PER ITEM KIND (song vs dub)
- Problem: round G's ONE global fader memory made a dub mixed to `Len dabing`
  (0,1,1) mute the next SONG's vocals (`stream_gains_song((0,1,1))=[0,0,1]`), and
  a song at `Plný mix` (1,1) double the next DUB's voices
  (`stream_gains_dub((1,1,1))=[1,0,0,1]`). Fix (design comment 5769481895,
  Approach 1): two remembered consoles selected by the playing item's KIND, same
  strip / same API.
- `sp_core::mixer_model`: `MixKind{Song,Dub}`, `MixConsole{song,dub,active}` +
  `active_faders`/`with_active_faders`/`select` + `mix_kind_for_dub` (pure, exact-
  value tests). RED (dub default 1,1,1) → GREEN (0,1,1).
- `stems/control.rs::MixControl`: two fader-atomic triples + an `active` atomic;
  `set_faders` writes the active memory, `select_kind`/`kind`/`console` new; boot
  reads five `mix_song_*`/`mix_dub_*` keys. RED (set_faders wrote song always) →
  GREEN (match on active kind).
- `playback/mix.rs::select_mix_kind_for_video` (from `broadcast_now_playing_on_start`)
  = Dub when `stems::dub_path(audio).exists()` (the reader's own readiness), else Song.
- DB V28 (mod.rs + mod_tests_v28.rs): derive `mix_song_*` from round-G `mix_*`,
  seed `mix_dub_*` = (0,1,1), delete the old globals; `apply_upto` test helper keeps
  V27's test isolated. RED (dub vokaly seeded 1) → GREEN (0).
- `api/mix.rs`: `GET /mix` adds `"kind"`, `PATCH` persists the active kind's keys.
  RED (`kind_str` swapped) → GREEN. mix_tests.rs: reset_console + kind tests.
- UI `live_mixer.rs`: reload Effect also tracks an `is_dub` Memo so the strip snaps
  to the other memory on a kind flip. E2E `dabing-mixer.spec.ts`: Dashboard + Live
  scenarios (dub mix survives a song); mock carries two memories + `kind`.
- Version 0.64.0-dev.4. Ships with round G in ONE deploy (base 34dfd28, unpushed).

- 2026-09-22 · #185 (Dabing D6 cross-worker priority gate) — delivered by #184 round G0.1 (merge 4b8731b), released in 0.64.0 via PR #205 (merge 6916856); box: dub wait 2.9 h → ~13 s. #200 closed as overcome (fix 435b3ac in 0.63.0). #184 stays open on the owner's wall re-acceptance (validator PARTIAL).

- 2026-09-23 · #207 phase-1 lane (v0.65.0-dev.5): commit observability + settings-driven mimalloc purge delay + fallible decoder frame alloc. RED 48f581f → GREEN 145ee9c.
  - A: new `lyrics/host_commit.rs` — per-minute `host: commit committed_mb=… limit_mb=… free_mb=… pagefile_used_mb=… free_phys_mb=…` (GlobalMemoryStatusEx+GetPerformanceInfo, cfg(windows)) + `GET /api/v1/status.commit` (HostCommitStatus). Logger spawned in lib.rs::start.
  - B: `heavy_purge_delay_ms` setting → `MIMALLOC_PURGE_DELAY` via `heavy_alloc_env(i64)` + `Containment.purge_delay_ms` (default -1; 0..=600000 valid). `heavy child contained` line gains `purge_delay_ms=`.
  - C: `frame_pool::try_take`/`FrameAllocFailed`/`alloc_exact` + `DecoderError::FrameAlloc`; mf_reader uses try_take (Unlock-on-error); `playback/frame_alloc.rs` maps it to a dropped frame + rate-limited WARN + `frames_dropped_alloc=` on the loop-stats line (pipeline match-guard, line-neutral).
  - Phases 2 (box purge-delay measurement) + 3 (zombie/section commit hunt) remain the main session's.
