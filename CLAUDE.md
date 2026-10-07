<!-- Global rules inherited from ~/.claude/CLAUDE.md (managed by airuleset) -->
# CLAUDE.md

This file provides guidance to Claude Code when working with code in this repository.

## Playbook router

Path-scoped rules in `.claude/rules/` auto-load on their `paths:`; skills in
`.claude/skills/` load on demand.

- rust workspace (1000-line cap, cargo-fmt reorder trap, untrusted input never into `serde_json::Value`'s own Deserialize — raw_value is always on) → `.claude/rules/rust-workspace.md` (auto-loads on `crates/**/*.rs`)
- lyrics-eval backends → `.claude/rules/lyrics-eval-backends.md` (auto-loads on `eval/lyrics/**`)
- LED-wall / Presenter lyrics display plan (#217: one display line = one whole sentence, split only at 52 chars EN or SK (one wall line; the Presenter wraps at it too) / 6.5 s span / 8 s break, ≤ 0.8 s lead only into a 1.5 s pause, min 1.2 s but never > 0.4 s behind the singing, dub = Speech profile shown exactly, hold until next, 8 s break → blank; display-only, NO pipeline bump; test tracks need sentence marks; #222: the Presenter push carries EN + SK of the same plan line as `currentText`/`currentTranslation`, dedup on the WHOLE payload (current + next, EN + SK), so a repeated sentence whose next line changed is pushed again) → `.claude/rules/lyrics-display.md` (auto-loads on `lyrics/display_plan*.rs`, `lyrics/renderer.rs`, `playback/position_update.rs`, `playback/recovery.rs`, `playback/dispatch_lyrics_tests.rs`, `playback/tests_hold.rs`, `tests/fixtures/lyrics_*.json`, `dabing/subtitles*.rs`, `presenter/**`)
- Resolume host driver (#157 light `/product` poll + `decide`; #217: a map with no SongPlayer token — incl. the breaker-evicted one, or left empty by a failed fetch — is NOT READY → refetch on a 2 s tick for 120 s while `/composition` answers, then every 60 s; a push answered 404, or a failing→ok flip whose one-param probe 404s, = a stale map (Arena re-ids on relaunch) → refresh; one `RecoveryEvent` per real recovery; the DRIVER owns the wall title (`title_state.rs`: a `Resync` acts only on a difference) and the engine names the due title from the song's `TitleClock`, the timers' own instants, counted from the real start `Started` reports and re-anchored by a seek (`TitleClock::seeked`, `playback/seek.rs`); a handler resolves the endpoint ONCE and hands it to every parallel write, no write re-reads the TTL'd cache — `endpoint cache empty` is gone) → `.claude/rules/resolume-driver.md` (auto-loads on `sp-server/src/resolume/**`, `playback/{recovery,title,title_timers,handle_pipeline_event,seek}.rs`, `playback/pipeline_types*.rs`, `playback/tests_title_seek.rs`)
- videos that cannot be opened (#229: from the 3rd failed open in a row a playlist waits 5 / 30 / 120 / 300 s for ONE retry, `RetryDue(id)`, ended by any Play, a SceneOff, a skip or a start; play_history is recorded at `Started`, never at the send, so the selection leaves out the run's failed songs; the health row's `open_failures` + the Player line) → `.claude/rules/open-failures.md` (auto-loads on `playback/failure_*.rs`, `handle_pipeline_event.rs`, `engine_play.rs`, `title_timers.rs`, `state.rs`, `playlist/selector.rs`, sp-core `player_view.rs`, sp-ui `player.rs`)
- sp-ui / e2e mock gotchas (#225: the Player claims only what it was told — `state_known`, `sp_core::player_view`, "Načítavam…" until the replay, the badge = the WS state, the store forgets on every socket open/close) → `.claude/rules/sp-ui-frontend.md` (auto-loads on `sp-ui/**`, `e2e/mock-api.mjs`, `e2e/player-known-state.spec.ts`, `sp-core player_view.rs`)
- dashboard WebSocket on-connect replay (#225: every engine `PlaybackStateChanged` / `NowPlaying` goes through `send_dashboard`, recorded in `dashboard_replay::global()` then broadcast; a new client gets EVERY playlist's state — a non-Idle one's last NowPlaying first, the rest an explicit Idle — in the mode that plays (the recorded one, else the row's), never the 5 s NDI-health sample; a removed pipeline is told Idle; a lagged client is re-sent the replay; unit 2: a playlist's mode has ONE persisted truth, its row — a pipeline starts in it, every change writes the row FIRST then tells the engine, `api/routes_mode.rs` + `playlist_mode.rs`) → `.claude/rules/dashboard-ws.md` (auto-loads on `api/websocket.rs`, `api/routes_mode.rs`, `api/routes.rs`, `playback/dashboard_replay*.rs`, `playback/playlist_mode*.rs`, `tests_ws_replay.rs`, `engine_play.rs`, `position_update.rs`, `runtime_pipeline.rs`, `startup_pipelines.rs`, `db/models_playlists.rs`, `sp-core ws.rs`, `sp-ui` `store.rs`/`ws.rs`/`pages/dashboard.rs`, `e2e/player-known-state.spec.ts`)
- pipeline.rs testability → `.claude/rules/pipeline-testability.md` (auto-loads on `playback/pipeline*.rs`, `submitter.rs`)
- `SP-program-MAX` GPU compositor + Spout sender (#223 S1a/S1b, `crates/sp-gpu`: a fixed 3840×2160 BGRA render target, `MISC_SHARED` not keyed, for Spout; D3D11 on the largest non-software adapter (`pick_adapter`), or WARP in tests; Y as R8 and UV as R8G8 at the stride, the upload skipped for a resident id (ids from a counter, never an address); clear to black, then one additive quad per picture at its `sp_core::fit::aspect_fit` place, outgoing at 1 − w, then incoming at w; BT.709 limited → full rows shared by the shader and the CPU `reference`; the WARP pins in `tests/warp.rs` allow one code per quad covering the pixel (`reference::tolerance`: bars exact, a picture ±1, a fade's overlap ±2) on smooth pictures only; "whole NV12" is `sp_core::nv12`, shared with sp-server's `nv12_whole`; `src/win/` is out of the mutation gate, so its logic lives in pure modules; S1b: the Spout sender `SP-program-MAX` — Arena lists `SPOUT_SP-program-MAX` — through the vendored, unmodified Spout2 2.007.017 SpoutDX sources (`vendor/spout2`, built by `build.rs` with `cc`/MSVC on Windows only) and `src/win/spout_shim.cpp`; `SendTexture` copies the render target into Spout's own shared texture, Spout lists the sender at its first send, a listed name is REFUSED never renamed `_1` (a first send that registers nothing, or is renamed or not listed, ends the sender: drop it, make a new one after a backoff; a send that fails after registering loses one frame), the decisions are the pure `src/spout_state.rs` and the shim only primitives, names are 1..=228 printable ASCII without a backslash, the sender holds its own device and render-target references (Spout does not AddRef the device) and neither it nor the `Compositor` is `Send` (one immediate context; one SDK call at a time per process); `spout_sender_names`/`spout_sender_info` read Spout's registry like a receiver; `tests/spout.rs` opens the shared handle on a second WARP device; S2 wires both into the program output: the `SP-program` sender offers each boundary's NATIVE picture(s) after VBAN's block and before its NDI submit into a 2-deep coalescing queue (`playback/program_max.rs`), the `program-max` thread (`program_max_worker.rs`) builds the compositor + sender on itself, labels pictures by allocation from a counter, rebuilds on a lost device (a second loss in a row waits 3 s), waits 3 s after a refused sender or a failed build; setting `program_max_enabled` ON unless "false"; `max` (with the adapter) on `GET /api/v1/program`; `e2e/post-deploy-max.spec.ts` gates it live; CI checks the exe's embedded Common-Controls 6.0 manifest and the installer ships the third-party notices) → `.claude/rules/gpu-max.md` (auto-loads on `crates/sp-gpu/**`, `playback/program_max*.rs`, `program_output_tests_max.rs`, `api/program_tests_max.rs`, `e2e/post-deploy-max.spec.ts`, `e2e/max-gate*.ts`)
- diagnostic benches (#223 S0: `POST /api/v1/diag/decode-bench {"file","seconds":1..=15}` decodes `<data dir>/bench/<file>` = `C:\ProgramData\SongPlayer\bench\` through the real MF reader on a thread started like the paced producer's (`playback::decode_thread`), unpaced; reports `codec`, `decode_us {mean,p50,p99,max}` and D2's gate `budget.mean_over_half_period`; 409 while a run holds the bench, 501 off Windows) → `.claude/rules/diag-bench.md` (auto-loads on `diag/**`, `api/diag*.rs`, `playback/decode_thread.rs`, `sp-decoder subtype.rs`)
- YouTube cookie file / bot-check → `.claude/rules/youtube-cookies.md` (auto-loads on `downloader/**`, `playlist/**`)
- yt-dlp spawn env (UTF-8 titles + hide console) → `.claude/rules/yt-dlp-spawn-env.md` (auto-loads on `downloader/**`, `playlist/**`)
- genlock / NDI timecodes / dantesync (#147 + #221 lane 3: pacing is the only path — the `genlock_pacing` switch, the SDK-clocked path, the #192 audio emitter and the #151 burn overlay are deleted, and `SP-program`'s submitter is the one wire edge; #224: every WallClock follows a date step at the boundary it lands — a per-tick probe against the anchor line, confirmed in the same tick; every paced audio block is stamped on its boundary, never the emit instant; #224 part 2: a date step RELABELS — `fleet_shift.rs` splits S into N = ⌊S/slot⌋ whole slots, put on only at the submit edge `floor(b + D(K_F))`, and the remainder r the internal timeline moves (never a burst, never a pause); one process-wide registry so every wall moves by ONE N (a reading within 3 ms of the closest run is the walls' line error, never an epoch; a wall built now or idle > 10 s JOINS the fleet's published line, never reads its drift as a step); VBAN slews the line's net movement per tick (r, or a residue hold) at 40 ppm, over a slot taken at once; #150: the 60 s lock window starts over when `decoding` flips or `PacingStats::seeks` moves, a seek counted at its first new frame) → `.claude/rules/genlock.md` (auto-loads on `sp-core genlock*`, `playback/{wallclock,clock_health,pacer,submitter,paced_,pipeline_paced,fleet_shift,lock_state,pacing_stats}*`, `sp-ndi/**`)
- dashboard preview — #15 JPEG thumbnail + #178 live A/V stream (never touch the program path; the decode-seam lead is 210 ms, `decode_seam_lead_ms`; #221: the encoder's video and audio count ONE monotonic clock — never `-use_wallclock_as_timestamps`, a clock step must never move one input; the video feeder writes the newest picture per 40 ms slot (a picture due for a decided slot is never replaced by a later one: pending canvases are a queue) and NOTHING while paused (a starved encoder freezes pixel-exact, a repeated picture is re-encoded and breathes), the next picture fills the gap, ≤ 250 slots, `preview_video_clock.rs`) → `.claude/rules/preview.md` (auto-loads on `playback/{preview,preview_stream,preview_encoder,preview_audio_hold,preview_video_clock,fmp4_relay,pipeline,pipeline_paced,nv12_fit}*`, `scripts/preview_latency_repro.py`, `api/preview.rs`, `sp-ui/.../playlist_card.rs`, `sp-ui/.../preview_video.rs`, `sp-ui/preview_player.js`)
- a song's file set (#136: video + audio + every file named after the audio — stems, dub, transcripts — renamed as ONE unit via `cache::rename_song_files`, re-linked by `song_relink` at startup and after every stem / dub job, a stem / dub job re-reads its input after the heavy slot (`song_input`, no audio = a no-penalty 10-min recheck), `stems_state` ready = the derived files on disk; the renamers: the metadata repair, an operator's title correction, a download corrected while it ran) → `.claude/rules/song-files.md` (auto-loads on `downloader/cache*.rs`, `reprocess/**`, `startup.rs`, `song_relink*.rs`, `song_input*.rs`, `stems/{mod,worker}.rs`, `dabing/worker.rs`, `lyrics/idle_gate_abort.rs`, `lyrics/g35t_probe*.rs`, `db/models_stems*.rs`, `metadata/manual*.rs`, `tests/startup_migration.rs`, `e2e/cache-layout*.ts`, `e2e/post-deploy-flac.spec.ts`)
- karaoke stem separation + audio-reader post-seek trim (#148 v3) + the stem mix's peak limiter (#184: ceiling 0.98, stereo-linked, instant attack, 50 ms release, bit-identical at rest — replaces the ±1.0 clamp) → `.claude/rules/karaoke-stems.md` (auto-loads on `stems/**`, `audio/stem_mix*.rs`, `peak_limiter*.rs`, `audio/symphonia_reader*.rs`, `split_sync*.rs`, `scripts/stem_worker.py`, `embedded_scripts.rs`, `scripts/tests/test_stem_*.py`, `scripts/tests/stem_fakes.py`); #207: the stems are published with the `win_replace` POSIX rename, which the stem worker ships next to `stem_worker.py`
- the Media Foundation video reader — software, or opt-in hardware decode (#223 S3b: `MediaFoundationVideoReader::open_with(path, DecodeMode)`; `Hardware` = `sp_gpu::VideoDevice` behind a DXGI device manager, `MF_SOURCE_READER_D3D_MANAGER`; a DXGI picture is read back with `Lock2DSize` into the software path's NV12 layout; the path is read from every picture, never assumed; a failed open or a decode error on the GPU path falls back to software for that file with a WARN; every decision in the pure `sp-decoder/src/hw_decode.rs`; setting `video_hw_decode` OFF by default, read at each song's open; `status.video_decode`; CI's WARP refuses the video device: no CI test runs the D3D path) → `.claude/rules/video-decode.md` (auto-loads on `sp-decoder/src/video/**`, `hw_decode*.rs`, `tests/mf_*.rs`, `sp-gpu` `video_device.rs`, `playback/video_decode*.rs`, `pipeline_paced.rs`)
- OBS↔NDI health / the `SP-program` receiver / post-deploy A/V lipsync + dropout gate (#147); #221 L6 deleted the OBS client's scene detection: it reads nothing of cg OBS's program, `ObsState` = connected + the #154 stream/record state, the identify subscribes Scenes | Outputs; #221 lane 3: `SP-program` is the ONE NDI sender — a playlist pipeline has none (it feeds the program bus), so the #127/#173 receiver ladder, `POST /api/v1/ndi/recover`, the NDI source map, the #196 post-restart self-check, the dark-wall reason and the burn are deleted; `/api/v1/ndi/health` = the pipelines' health with no receiver fields; the receiver that must exist is `SP-program`'s (`degraded_reason` on `GET /api/v1/program`, a WARN/INFO on its loss/return, the post-deploy E2E gate `ndi-health-gate.ts::programReceiverVerdict`), its port pinned across a restart by the #196 port wait (`startup_pipelines::wait_for_program_ports`); the A/V gate records `SP-program` through cg OBS's own probe scene "A/V gate (SP-program)" (`e2e/av-sync-probe.ts`); #221 dev.18: a freshly attached probe's video reaches the recording at once and its audio reaches cg OBS's mix with gaps until camera-box's genlock audio pairing locks (~4 s after the bind), so every take first waits for its `InputVolumeMeters` input peak above −60 dBFS for 1 s, bounded 20 s (`e2e/obs-audio-wait.ts`; the meter subscribed only around the wait; the meter is tapped BEFORE camera-box's withhold, so it proves DistroAV delivers audio, never the lock) → `.claude/rules/obs-ndi-health.md` (auto-loads on `obs/**`, `obs_bridge.rs`, `tests/{obs_reconnect,fake_obs_handshake}.rs`, `playback/ndi_health{,_expect,_log,_tests}*.rs`, `playback/startup_pipelines.rs`, `e2e/post-deploy.spec.ts`, `e2e/post-deploy-dabing.spec.ts`, `e2e/ndi-health-gate*.ts`, `e2e/post-deploy-av-sync.spec.ts`, `e2e/av-sync-{gate,probe,evidence}*.ts`, `e2e/obs-audio-wait*.ts`, `e2e/obs-driver.ts`, `scripts/av_sync_check.py`, `scripts/av_sync_drift.py`)
- obs-mcp gateway watchdog → `.claude/rules/obs-mcp-gateway.md` (auto-loads on `scripts/obs-mcp/**`)
- CLIProxyAPI / DEFAULT_AI_MODEL; #145: a refused AI call (429 / 5xx) waits the proxy's `Retry-After` (≤ 120 s) else 5 / 20 / 60 s, past its 60 s credential cooldown (`ai/retry.rs`, injected — tests use `RetryPolicy::NO_WAIT`) → `.claude/rules/ai-proxy.md` (auto-loads on `crates/sp-server/src/ai/**`, `config.rs`)
- metadata providers (#136: ONE `metadata::provider_chain` [Claude, Gemini] built once in `lib.rs` for the download worker, the reprocess worker and the API; Gemini takes the SPLIT `gemini_api_key` list, one key per request; ONE Gemini key-list contract in `crate::gemini_api` (`key_verdict` + `send_on_key`: 429 / key refusal → next key, 5xx → same key 2/4/8/16 s; lyrics + metadata rotate by it, dabing takes the first key; errors name the key index never a key, a refused body kept as the one `gemini_api::body_excerpt` — one line, redacted, then cut); metadata reads the key setting at startup (restart after a key change); `status.metadata` + `POST /api/v1/metadata/probe` + the post-deploy gate `e2e/post-deploy-metadata.spec.ts`; an operator's PATCH of song/artist = `metadata_source = 'manual'`, never repaired, applied to EVERY row of the video with its files renamed (`metadata::manual::apply_to_video`) and kept on a (re-)download (`download_title`); an artist alone for a video with no song yet is refused 400 (`refused_title`)) → `.claude/rules/metadata-providers.md` (auto-loads on `metadata/**`, `reprocess/**`, `api/metadata*.rs`, `api/routes.rs`, `api/routes_tests_patch_metadata.rs`, `gemini_api*.rs`, `lyrics/g35t_client.rs`, `e2e/post-deploy-{metadata,flac}.spec.ts`)
- crash diagnostics / panic hook → `.claude/rules/crash-diagnostics.md` (auto-loads on `panic_hook.rs`, `src-tauri/src/lib.rs`, `build.rs`)
- status locks (#144: a status lock's guard is never held across a slow await — the startup tools task publishes through `tools_ready::publish_then` and the follow-ups run with no lock held; a new dashboard socket's first frames are built under the lock and sent after it, `websocket.rs::send_status`; `GET /api/v1/status` copies each status out before its database waits) → `.claude/rules/server-startup.md` (auto-loads on `sp-server/src/lib.rs`, `tools_ready.rs`, `api/websocket.rs`, `api/routes.rs`)
- LAN sp.local mDNS advertisement → `.claude/rules/lan-mdns.md` (auto-loads on `crates/sp-server/src/mdns.rs`)
- CI workflows: runner shell traps / mutation gate / push+PR de-dup; #221 L4a: the deploy waits out another repo's dev1 rig lease before it stops SongPlayer (`scripts/rig_lease_gate.py`, 30 s polls, 60 min bound, an unreachable lease service = WARN) → `.claude/rules/ci-workflows.md` (auto-loads on `.github/workflows/**`, `.cargo/mutants.toml`, `scripts/rig_lease_gate.py`, `scripts/tests/test_rig_lease_gate.py`)
- DB migrations (manual `db/mod.rs`; per-version test isolation via `apply_upto`) → `.claude/rules/db-migrations.md` (auto-loads on `crates/sp-server/src/db/mod*.rs`)
- secret settings (#229: ONE list `sp_core::config::SECRET_SETTINGS` + any setting named `*_key` / `*_token` / `*_password` / `*_secret`; `GET /api/v1/settings` shows each non-blank one as `********`, `peers` per peer; a PATCH that sends the mask back keeps the stored value, a bad exchange setting = 400 and nothing written, success = 204 with no body (`api::patch_json_empty`); workers read the DB; eval/ops never read a key through GET) → `.claude/rules/settings-secrets.md` (auto-loads on `sp-core config.rs`, `api/settings*.rs`, `peer/config*.rs`, `sp-ui` `settings_form.rs`/`api.rs`, `e2e/mock-api.mjs`, `e2e/settings-*.spec.ts`, `e2e/post-deploy-settings-secrets.spec.ts`, `eval/dubbing/voice_band_measure.py`)
- node exchange (#229 lane 1: `node_name` / `peer_api_key` / `peers` → `peer::config::NodeConfig`, validated, read live from the DB; `GET /api/v1/exchange/status` = node, serving, pause, config error, peers without secrets; plan `docs/superpowers/plans/2026-10-06-pp-node-exchange.md`) → `.claude/rules/peer-exchange.md` (auto-loads on `peer/**`, `sp-core config.rs`)
- lyrics_worker.py tests (eval-checks CI = numpy+soundfile only; fake torch/librosa/audio_separator) → `.claude/rules/lyrics-worker-tests.md` (auto-loads on `scripts/lyrics_worker.py`, `scripts/tests/**`)
- lyrics venv bootstrap (#221: `is_ready` reports a `Readiness` reason; a timeout / CUDA / init failure is retried ~3 min before any install, only a proven import failure or a missing interpreter installs at once, a timeout never force-reinstalls torch; the A/V gate has its own `e2e\avsync_venv`) → `.claude/rules/lyrics-bootstrap.md` (auto-loads on `crates/sp-server/src/lyrics/bootstrap*.rs`)
- dabing (dubbing) eval (Soniox/Chatterbox engine gotchas) → `.claude/rules/dubbing-eval.md` (auto-loads on `eval/dubbing/**`)
- post-deploy specs: the program = ANY sp-* scene (read `program-state.ts`, explicit card, shared `box-api.ts`) → `.claude/rules/post-deploy-program-state.md` (auto-loads on `e2e/post-deploy*.spec.ts`, `e2e/program-state*.ts`, `e2e/box-api.ts`)
- program bus + NDI `SP-program` (master switcher: cut rule, reorder/fill, OnceLock install, startup order, Program control; #221 lane 3: `SP-program` is the ONLY NDI sender, a playlist's paced consumer delivers to the bus alone — `paced_output::offer_to_bus` through `InstalledBus`; #223: `SP-program` is ALWAYS 1920×1080 — every picture is the `program_canvas.rs` canvas (`PROGRAM_STANDBY_W/H`): a canvas picture passes through as the same allocation, any other is aspect-fitted into it by the fused kernel (a plain fit: one side, nothing else read), a fade is ONE pass from both sides in the canvas (#223 follow-up), the bands run on the sender's persistent `program-mix-<i>` workers (`band_pool.rs`, no thread per picture), and `health.timing.submit_us` times the fit with the NDI call; #221: the on-air watch published with `send_modify` on EVERY cut, the one scene-name resolver, the scene catalog; L4b: SongPlayer's own program is the PLAYBACK authority — on air = SP-program's playlist ALONE (B4 step 6 deleted `legacy_cg` and its union; none for "OBS manuál"), `PipelineEvent::OnProgram` with a stale check (a cut's OFF and ON from one change, OFF first), no OBS→engine bridge / `Hold::OnProgram` / `CUT_SETTLE` / startup re-mirror, a ▶ plays off program ("Hrá mimo programu"), `/api/v1/status` from the bus; release 0.69.0: ONE wall owner — SP-program's playlist — alone writes the lines, the title and the Presenter (`OnAirPlaylists::may_write_wall`), and an owner change — the new owner's ON, or an OFF — re-syncs all three (`scene_off::wall_after_owner_on`, `wall_after_scene_off`); #221 lane 2: no owner ("OBS manuál") = nobody writes, the last owner's OFF blanks the stage display too, a test of a wall write puts its playlist on air first (`put_on_air_for_test`); B4 step 6: SP-program's receiver = `degraded_reason` on `GET /api/v1/program`; a refused cut's reasons + Slovak texts = ONE vocabulary, `sp_core::program_refusal`) → `.claude/rules/program-bus.md` (auto-loads on `playback/program_{bus,on_air,output,authority,canvas}*`, `band_pool*`, `scene_catalog*`, `ndi_health_expect*`, `runtime_pipeline.rs`, `engine_play.rs`, `paced_output*.rs`, `api/program*`, `api/routes_status.rs`, `sp-ui` `program_control.rs`/`player.rs`, `sp-core` `program_refusal.rs`)
- VBAN audio out of the program (#210: pure encoder, paced 1/240 s thread, drop-oldest queue, settings task, `vban` telemetry; each boundary's block is handed to VBAN BEFORE the SP-program NDI submit, and the sender's per-boundary stage timing is `health.timing`; part 2: the one rate-limited hand-off WARN fires only over VBAN's send latency L = 66.7 ms (`vban_feed_late_over_budget`), the `vban-output` thread is an MMCSS "Pro Audio" thread at AVRT_PRIORITY_HIGH (`mmcss.rs` RAII guard, TIME_CRITICAL + WARN if refused), and it times every packet: `vban.late_events` (the last 32 over 5 ms, `{utc_ms, late_us}`), `vban.late_max_us` (60–120 s) and one WARN per 5 s over 10 ms; every program block goes through ONE `sp_decoder::PeakLimiter` after the transition crossfade and before VBAN + NDI; SongPlayer's own playlists pass bit for bit once a fade's release tail has decayed) → `.claude/rules/vban-out.md` (auto-loads on `playback/vban_*`, `mmcss*`, `stat_window*`, `program_output*`, `api/program*`, `sp-ui` `settings_form.rs`, `e2e/settings-vban.spec.ts`)
- NDI input "OBS manuál" (#212: receive half + FrameSync RAII, genlock-grid input thread, UYVY→NV12, source id -1, settings + `input` telemetry) → `.claude/rules/ndi-input.md` (auto-loads on `sp-ndi` `receive*`/`receiver*`, `playback/ndi_input*`, `program_bus*`, `api/program*`, `sp-ui` `program_control.rs`/`settings_form.rs`, `e2e/settings-ndi-input.spec.ts`)
- Companion remote control (#213: obs-websocket 5 subset on `remote_ws_port` 4456, reached through the existing OBS client, `remote` telemetry; #221: studio mode ON, a per-session preview, `TriggerStudioModeTransition` always switches, ONE switch path `playback/program_switch.rs` — a playlist scene from SongPlayer's own catalog is cut and cg OBS is told nothing (B4 step 6 deleted the mirror, the startup re-mirror and `legacy_cg`), a manual scene goes to cg OBS first then "OBS manuál", no cg scene lookup, the transition duration is acknowledged never applied; L3: `CurrentProgramSceneChanged` / `SceneTransitionStarted`/`Ended` / `GetCurrentProgramScene` are SongPlayer's own (`remote/studio_events.rs`, the on-air watch + resolver), cg OBS's program event is never passed through, the post-deploy E2E drives scenes through the facade on 4456; lane 2: the forwarded `GetSceneList` carries SP-program's scene + the session preview (Companion's connect-time feedback, `protocol::with_songplayer_scenes`); L2b: Companion speaks `obswebsocket.msgpack` (obs-websocket-js in Node) — the facade echoes msgpack when JSON is not offered, one `remote/codec.rs` codec per session for every message both ways, the E2E driver keeps the bare msgpack import; both encodings decode through `codec::PlainValue`, never `serde_json::Value`'s own Deserialize (raw_value re-parse); L4a: the dashboard cut goes through the same switch path (`switch_source`, `via=dashboard`, cg OBS told nothing; a cut to an inactive / scene-less playlist is refused 409 and its button disabled, `cut_scene` + `cut_refused`), at most 16 sessions (503 + `refused_over_cap`)) → `.claude/rules/remote-control.md` (auto-loads on `sp-server/src/remote/**`, `playback/program_switch*.rs`, `playback/scene_catalog*.rs`, `api/program_tests_switch.rs`, `obs/remote_call.rs`, `tests/remote_control.rs`, `e2e/settings-remote.spec.ts`, `e2e/obs-driver*.ts`)
- program scene transitions (#215: a cut = a window of mixed boundaries, equal-power audio + NV12 blend — #223: drawn in the 1920×1080 `SP-program` canvas — both sides fitted as they are read and blended in ONE fused pass, a row at a time, in ≤ 6 row bands on the sender's persistent `program-mix-<i>` workers (`nv12_mix.rs`, `band_pool.rs`; #223 follow-up), a fade waits ≤ 15 boundaries for the incoming source's first LIVE pair (the cue gate, `SubmitJob::live`), a Cut = zero-length window, outgoing playlist held via `hold_for` / `SceneOffDue` (held = no side effects: its end/skip pauses it, no lyrics/title writes), the transition = Nastavenia only (`fade` default / `cut`, `program_transition_settings.rs` re-reads it every 5 s; #221 L5 deleted the OBS follow, `program_follow_obs` and "podľa OBS"), `transition` telemetry) → `.claude/rules/program-transition.md` (auto-loads on `playback/program_{transition,bus,output,canvas}*`, `scene_off*`, `tests_hold.rs`, `tests_wall_owner.rs`, `handle_pipeline_event.rs`, `clear_lyrics.rs`, `engine_play.rs`, `nv12_fit.rs`, `nv12_mix*.rs`, `band_pool*.rs`, `pacer_tests_live.rs`, `api/program*`, `sp-ui` `program_control.rs`/`settings_form.rs`, `e2e/program-control.spec.ts`, `e2e/settings-program-transition.spec.ts`)
- lyrics reference text (#144: the TWO-WAY gate — `sung_coverage.rs` LCS, ≥ 0.55 of the sung words and no uncovered run > 25 s, measured; ONE g35t transcript per song taken after isolation, kept for the pass as `{yt}_g35t_words.json`, retired to `_used.json`; the title search for covers (LRCLIB ±15 s + Genius by title, Dice ≥ 0.50, vs the video's priority pick); captions one line per sung line; the scraped-lyrics cleanup keeps every repeat, caches `_cleaned_v3.json`; ONE per-song reprocess path `POST /api/v1/lyrics/reprocess {video_ids}` = manual priority; no reprocess route blanks the lyrics the wall serves — the per-video blanking route is deleted; a re-run that errors or gets an empty transcript on a SERVED song (`has_lyrics = 1` + its `_lyrics.json`) records only the attempt (backoff; the manual priority stays until its 3rd failed attempt) and keeps the lyrics, only an unserved row goes `no_source` / `asr_gap`; the live g35t gate `POST /api/v1/lyrics/g35t/probe` = ONE 20 s clip of a cached song from its first served line through the worker's own `g35t_client::transcribe_at` (same body, hint, key rotation), `e2e/post-deploy-g35t.spec.ts` fails the deploy on no words) → `.claude/rules/lyrics-reference-text.md` (auto-loads on `lyrics/{reference_gate,sung_coverage,title_search,lrclib_search,text_candidate,transcript_cache,worker_text_tiers,worker_reference,genius,description_provider,reprocess,g35t_client,g35t_probe}*`, `worker.rs`, `worker_outcome*.rs`, `worker_g35t.rs`, `orchestrator.rs`, `audit_ctx.rs`, `gather.rs`, `youtube_subs.rs`, `api/lyrics*.rs`, `playback/lyrics_loader.rs`, `e2e/post-deploy-g35t.spec.ts`, `e2e/g35t-gate*.ts`)
- Dabing feature (dub data model + section + import cookie gate + D4 dub worker/Live-Translate child/4-stream mix) → `.claude/rules/dabing.md` (auto-loads on `db/models_dabing.rs`, `api/dabing.rs`, `api/routes_import.rs`, `startup_dabing.rs`, `dabing/**`, `scripts/dub_worker.py`, `sp-ui` dabing files)

| Area | Skill | Load when |
|------|-------|-----------|
| Lyrics pipeline | `lyrics-pipeline` | lyrics processing, alignment providers, pipeline versioning, translation, Gemini/CLIProxy, reprocess |
| Wall verification | `lyrics-verify` | /lyrics-verify, wall-verify loop, catalog songs, sp-live setlist, quarantine |
| Lyrics eval | `lyrics-eval` | evaluating ASR/alignment backends, /lyrics-eval command, eval harness |
| win-resolume ops | `win-resolume-ops` | deployments, CI monitoring, Resolume diagnostics, OBS, runner health, the PP site (resolume-pp) |
| CI quality | `ci-discipline` | writing CI jobs, reviewing PRs, test design, quality gates |

## Project Overview

SongPlayer is a standalone Windows desktop application that plays YouTube playlists with loudness normalization and NDI output. Built with Rust using Tauri 2 (shell), Leptos 0.7 (WASM UI), and Axum 0.8 (embedded HTTP/WebSocket server). Videos are downloaded via yt-dlp, normalized to -14 LUFS with FFmpeg, and can be output via NDI.

## Workspace Structure

The Cargo workspace root manages 5 crates. Two additional crates are excluded from the workspace because they have different build toolchains.

```
songplayer/
├── Cargo.toml              # Workspace root (members: sp-core, sp-ndi, sp-decoder, sp-gpu, sp-server)
├── VERSION                 # Single source of truth for version (e.g. 0.1.0-dev.1)
├── scripts/
│   └── sync-version.sh    # Reads VERSION, updates all Cargo.toml + tauri.conf.json + both Cargo.lock files
├── crates/
│   ├── sp-core/          # Shared types, database (SQLite/sqlx), domain logic — WASM-safe
│   ├── sp-ndi/           # NDI output via libloading (runtime-linked, no compile-time dep)
│   ├── sp-decoder/       # Windows Media Foundation decoder (cfg(windows) only)
│   ├── sp-gpu/           # SP-program-MAX D3D11 compositor + Spout sender (Windows; Linux stub)
│   └── sp-server/        # Axum HTTP + WebSocket server, yt-dlp/FFmpeg orchestration
├── sp-ui/                # Leptos 0.7 WASM frontend (excluded from workspace, built with Trunk)
└── src-tauri/             # Tauri 2 shell (excluded from workspace, built with cargo tauri)
```

### Crate Descriptions

| Crate | Purpose |
|-------|---------|
| `sp-core` | Shared types, SQLite database via sqlx, domain models. Must be WASM-safe (no tokio, no std-only I/O). |
| `sp-ndi` | NDI SDK integration via `libloading`. Loads the NDI shared library at runtime to avoid compile-time dependency. |
| `sp-decoder` | Windows Media Foundation video decoder. Entire crate is `cfg(windows)` — will not compile on Linux. |
| `sp-gpu` | The `SP-program-MAX` compositor and its Spout sender (#223): Direct3D 11 + the vendored Spout2 SDK on Windows (WARP in tests), a stub reporting "unsupported" elsewhere. Pure decisions (adapter, layers, colour, CPU reference) are Linux-tested. |
| `sp-server` | Axum 0.8 server with HTTP REST + WebSocket. Runs yt-dlp and FFmpeg as subprocesses. Main async binary. |
| `sp-ui` | Leptos 0.7 CSR frontend compiled to WASM via Trunk. Communicates with sp-server via HTTP/WebSocket. |
| `src-tauri` | Tauri 2 application shell. Embeds `dist/` from sp-ui build and spawns sp-server in background. |

## Dev Commands

**Check the workspace (fast, no output):**
```bash
cargo check
```

**Run tests:**
```bash
cargo test
```

**Format code:**
```bash
cargo fmt --all
```

**Check formatting (for CI):**
```bash
cargo fmt --all --check
```

**Lint:**
```bash
cargo clippy -- -D warnings
```

**Build the WASM frontend (requires trunk):**
```bash
cd sp-ui && trunk build
```

**Build the full Tauri app (requires trunk output in dist/):**
```bash
cd src-tauri && cargo tauri build
```

**Sync version from VERSION file:**
```bash
./scripts/sync-version.sh
```

## Branch Strategy

Two branches: `dev` + `main`. After merge: recreate `dev` with next `-dev.N` version.

## Version Management

`VERSION` is the single source of truth. Run `./scripts/sync-version.sh` after changing it.

| Branch | VERSION format | Example |
|--------|---------------|---------|
| `dev`  | `X.Y.Z-dev.N` | `0.1.0-dev.1` |
| `main` | `X.Y.Z`       | `0.1.0` |

**Workflow:**
1. Start work on `dev` with VERSION like `0.1.0-dev.1`
2. Run `./scripts/sync-version.sh` to propagate to all Cargo.toml files
3. Before PR merge: change VERSION to `0.1.0`, run sync-version.sh
4. After merge: recreate dev with `0.2.0-dev.1`

Note: The 5 workspace crates use `version.workspace = true` — only the root `Cargo.toml`, `src-tauri/Cargo.toml`, `sp-ui/Cargo.toml`, and `src-tauri/tauri.conf.json` need updating.

## Database

SQLite via sqlx with manual migrations. Migration logic lives in `crates/sp-server/src/db/mod.rs`. No external migration files — schema is applied programmatically at startup.

- Database file: configurable path, defaults to `songplayer.db` in app data dir
- Connection type: `SqlitePool` with `sqlx::sqlite`
- No compile-time query checking (`query!` macro requires DATABASE_URL) — use `query_as` with runtime checking

## Key Patterns

**sp-core must be WASM-safe:**
- No `tokio` in sp-core (use `futures` traits only)
- No `std::fs` or OS-specific I/O
- No `cfg(windows)` — platform code belongs in sp-decoder or sp-server

**sp-decoder is Windows-only:**
- All code wrapped in `#[cfg(target_os = "windows")]` or `#[cfg(windows)]`
- Uses the `windows` crate for WMF (Windows Media Foundation) APIs
- Not compiled on Linux/macOS CI — use feature flags if needed for cross-platform CI

**NDI via libloading (runtime linking):**
- NDI SDK not required at compile time
- `sp-ndi` loads `Processing.NDI.Lib.x64.dll` at runtime via `libloading`
- Gracefully degrades if NDI is not installed

**NDI network name format:**
NDI sources on the network are advertised as `"MACHINE (stream)"` — the machine hostname that owns the sender (its Windows `COMPUTERNAME`, case-sensitive for DistroAV's re-match), a space, then the stream name in parentheses. When OBS adds an NDI source, its `ndi_source_name` input setting stores this full string (e.g. `"RESOLUME-SNV (SP-program)"`). #221 lane 3: SongPlayer's only NDI sender is `SP-program`; a playlist's `ndi_output_name` (`"SP-fast"`) is now only its scene label (the scene catalog, `/api/v1/ndi/health` `ndi_name`), never a sender. The NDI input "OBS manuál" reads the bare stream of its source with `playback/ndi_input_name.rs::extract_ndi_stream_name`; the NDI source map and the receiver ladder that matched cg OBS inputs by it are deleted.

**Split-file audio layout (FLAC pipeline):**
Each cached song is stored as two sidecar files sharing a common base name:

- `{safe_song}_{safe_artist}_{video_id}_normalized[_gf]_video.mp4` — H.264/VP9/AV1 stream-copied from YouTube, zero re-encodes.
- `{safe_song}_{safe_artist}_{video_id}_normalized[_gf]_audio.flac` — decoded from YouTube's Opus stream, 2-pass FFmpeg loudnorm at -14 LUFS, re-encoded to FLAC exactly once. Signal is lossless from this point to NDI.

The decoder split follows the file layout: `sp_decoder::MediaFoundationVideoReader` (Windows-only, hardware-accelerated MF) reads the video sidecar, and `sp_decoder::SymphoniaAudioReader` (pure Rust, cross-platform) reads the FLAC sidecar. `SplitSyncedDecoder` drives both with audio-as-master-clock at 40 ms tolerance. The `VideoStream` / `AudioStream` / `MediaStream` traits in `sp_decoder::stream` let unit tests drive the sync algorithm with mock readers on Linux.

On first boot of a new version, `sp_server::startup::self_heal_cache` walks the cache directory: any legacy single-file `.mp4` from before the FLAC migration is deleted, any orphan half-sidecars (video without audio or vice versa) are deleted unless a DB row records them (a song split across two names, #136), every complete video+audio pair is re-linked to its DB row, and the stems / dub a rename left under an old name are re-linked to the audio's name (`song_relink`, #136, `.claude/rules/song-files.md`). Migration V4 resets `normalized = 0` for every existing row so the download worker re-processes everything under the new layout.

A one-shot startup sync (`sp_server::startup::startup_sync_active_playlists`, matching legacy Python `tools.py::trigger_startup_sync`) runs for every `is_active = 1` playlist once tools are ready — this was missing from the initial Rust port and is restored alongside the FLAC migration.

**Circular import avoidance:**
Use local imports inside functions when needed to break cycles:
```rust
fn some_fn() {
    use crate::other_module::Thing;
    // ...
}
```

**Windows subprocess (hide console window):**
All `std::process::Command` calls for yt-dlp/FFmpeg must use `CREATE_NO_WINDOW`:
```rust
use std::os::windows::process::CommandExt;
command.creation_flags(0x08000000); // CREATE_NO_WINDOW
```

**Server orchestration (`sp-server/src/lib.rs`):**
The `start()` function wires all subsystems: DB, tools manager, playlist sync handler, download worker, OBS WebSocket client, playback engine, Resolume workers, reprocess worker, and Axum HTTP server. All workers receive a shutdown broadcast for graceful termination.

**API routes are under `/api/v1/`** (not `/api/`). The WASM dashboard uses relative URLs.

**Deployment target:** Windows machine `win-resolume` (10.77.9.201) running OBS Studio with NDI plugin. Installed via NSIS installer from CI artifacts. Data directory: `C:\ProgramData\SongPlayer\`.

**Follow existing patterns** from similar projects (restreamer, iem-mixer) for consistency in error handling, logging (tracing), and state management.

## Pipeline versioning (lyrics)

`crates/sp-server/src/lyrics/mod.rs::LYRICS_PIPELINE_VERSION` is a monotonic integer identifying the lyrics processing output format. Every song's lyrics JSON + DB row records the version it was produced under. On worker startup, songs with `lyrics_pipeline_version < LYRICS_PIPELINE_VERSION` are re-queued for reprocessing (stale bucket, worst-quality-first).

**Never bump the constant without the owner's explicit approval** (the
`lyrics-pipeline` skill, "Pipeline version discipline"). A bump re-runs the
whole catalog. Without that approval, re-run the songs a change affects with
the targeted reprocess `POST /api/v1/lyrics/reprocess {"video_ids":[…]}`
(manual priority; the wall keeps the old lyrics while they wait). The #144
rollout was exactly that.

**With that approval, a bump is right when:**
- Adding or removing a route / alignment stage from the worker (the #159
  one-regime cut that dropped WhisperX + asr_path was a 21→22 bump)
- Changing the mtl align invocation, the `reference_gate` thresholds, or the
  g35t base-tier grouping (gap/coalesce/sanitize) in a way that alters output
- Changing the reference-text-selection algorithm (`best_authoritative_candidate`)

**Do NOT bump for:**
- Bug fixes that produce identical output
- Refactoring, renaming, logging changes
- UI/dashboard-only changes
- Performance optimizations with identical output

**History (condensed — regimes below v22 are DELETED, kept as a one-line trail):**
- v1–v10: qwen3/autosub ensemble + Claude/Rust merge + word-timing sanitizers
  (blinking-karaoke fixes). Ensemble deleted.
- v11–v18: Gemini chunked forced-alignment era — multi-key rotation,
  CLIProxy↔direct-API flip-flops, the `lines: []` data-loss fix (v15), AutoSub
  unregistered (v16), line-level-only timing (v18, `words: None`, no synthesized
  word timings — still enforced). Gemini chunked regime deleted.
- v19: manual yt_subs short-circuit. v20: Genius text source +
  `lyrics_override_text`; aligner became WhisperX-on-Replicate with an
  AssemblyAI-U3-Pro `asr_path` fallback for no-text songs.
- v21 (#143): Lever-2 reference stage added — `mtl_aligner` force-align verified
  by a Gemini-3.5-Transcribe transcript (`reference_gate`), ship ★ on PASS, else
  fall through to the v20 routes.
- v22 (#159): **one regime.** The v20 WhisperX-on-Replicate route and the
  AssemblyAI `asr_path` route are DELETED (owner directive 2026-09-14, not "keep
  as fallback"). Two tiers now — (1) ★ tier: text + mtl force-align + g35t gate
  → mtl line timings (unchanged from v21); (2) base tier: everything else (no
  usable text, gate fail, mtl skip/error) → a Gemini-3.5-Transcribe transcript
  grouped into lines (`g35t_transcript`, source `gemini-3-5-transcribe`). One
  forced aligner (mtl), one ASR vendor (Gemini). Measured no-text quality g35t
  19.7% gold-norm ≤400ms vs the retired AssemblyAI 3.8%
  (`eval/lyrics/reports/2026-09-12-gemini-3-5-transcribe.md`). Every pre-v22 row
  re-queues (no smart-skip — that was the v18 trap); v21 mtl rows re-run to
  identical output and re-★. Full route map: the `lyrics-pipeline` skill.

## Disabled subsystems (do not re-enable without redesign)

### NDI auto-recovery (per-sender RecreateSender) — disabled v0.26.0

(#221 lane 3 retired the per-playlist NDI senders this section was written
about: `SP-program` is SongPlayer's only NDI sender now. The rule stands for
it: never recreate a sender to "recover" a receiver.)

PR #58 shipped a Tier-2 auto-recovery that, on prolonged `connections=0`, sent a `PipelineCommand::RecreateSender` to the affected pipeline thread, which destroyed the old NDI sender and created a new one with the same name. **It cannot work and was ripped out in v0.26.0.** Two structural reasons:

1. **NDI runtime mDNS binding is process-global.** The 2026-04-27 production failure cause was the NDI SDK binding its mDNS announce socket to a stale APIPA address (`169.254.144.214` — no longer present on any current adapter). Per-sender create/destroy keeps the same global socket; the new sender is also dark.
2. **Real NDI rejects two senders with the same name in one process.** Our recreate created the new sender BEFORE the old one's destroy ran (Drop is deferred to end of arm), so `send_create` returned null on the same-name conflict and the loop spammed `RecreateSender mid-decode: failed; keeping existing` every 30 s. `MockNdiBackend` did not enforce same-name uniqueness so unit tests never caught it.

Recovery from that state requires either:
- Process restart (works today; operator-only — there is no in-app /api/v1/admin/restart endpoint)
- Full NDI runtime re-init: `NDIlib_destroy()` + `NDIlib_initialize()` + recreate every sender + reconnect every receiver subscription. Coordinated, multi-pipeline, and risky enough that it needs its own design and integration test against a real Windows NDI runtime — tracked in #60.

If you find yourself adding `PipelineCommand::RecreateSender` back: stop. Check #60 first. Any per-sender recreate is structurally unable to fix the root cause.

Tier-1 visibility today: `GET /api/v1/program` names `degraded_reason` `"no NDI receiver on SP-program"` while a source is on program and `SP-program` has no receiver (a WARN on the loss, an INFO on the return), and the post-deploy E2E fails on it. That's the entire current response. (A playlist pipeline's `degraded_reason` on `/api/v1/ndi/health` is an underrun or a stalled delivery only; its old dark-wall reason went with its sender.)

## Legacy OBS YouTube Player (obsytplayer)

SongPlayer is the Rust replacement for the legacy Python OBS YouTube Player at `/home/newlevel/devel/obsytplayer/`. **Always reference the legacy code when implementing features** — it contains battle-tested logic for:

- **Metadata extraction:** `yt-player-main/metadata.py` + `yt-player-main/gemini_metadata.py` — title parsing regexes, Gemini prompt, featuring cleanup
- **Download/normalize pipeline:** `yt-player-main/playlist_manager.py` — yt-dlp format selection, FFmpeg 2-pass loudnorm, file naming conventions
- **Playback engine:** `yt-player-main/player.py` — scene detection, play/pause/skip logic, title display timing (show 1.5s after start, hide 3.5s before end)
- **Playlist sync:** `yt-player-main/playlist_manager.py` — YouTube playlist flat-download, video dedup, unplayed tracking
- **OBS integration:** `yt-player-main/obs_controller.py` — text source updates, media source path changes
- **Resolume title delivery:** `yt-player-main/resolume_controller.py` — A/B lane crossfade, clip mapping via #token tags
- **Configuration:** Each instance had its own `config.json` with playlist URL, OBS source names, Gemini API key

**Key design decisions from legacy code to preserve:**
- Loudness normalization target: -14 LUFS (FFmpeg loudnorm filter)
- yt-dlp format: `bestvideo[height<=1440]+bestaudio/best[height<=1440]`
- Title display: show artist + song 1.5s after video starts, hide 3.5s before end
- Gemini prompt asks for `{song, artist, source}` JSON; falls back to regex parser
- File naming: `{song}_{artist}_{youtube_id}_normalized.mp4` (with `_gf` suffix if Gemini failed)

**6 playlist instances being migrated:**
| Name | YouTube Playlist | OBS Scene | NDI Output |
|------|-----------------|-----------|------------|
| ytwarmup | PLFdHTR758BvcHRX3nVKMEPHuBdU75dBVE | ytwarmup | SP-warmup |
| ytpresence | PLFdHTR758BveAZ9YDY4ALy9iGxQVrkGRl | ytpresence | SP-presence |
| ytslow | PLFdHTR758Bvd9c7dKV-ZZFQ1jg30ahHFq | ytslow | SP-slow |
| yt90s | PLFdHTR758BvfM0XYF6Q2nEDnW0CqHXI17 | yt90s | SP-90s |
| ytworship | PLFdHTR758BveEaqE5BWIQI7ukkijjdbbG | ytworship | SP-worship |
| ytfast | PLFdHTR758BvdEXF1tZ_3g8glRuev6EC6U | ytfast | SP-fast |

**Coexistence strategy:** Create NEW `sp-*` scenes in OBS with NDI sources from SongPlayer. Do NOT modify existing `yt*` scenes — legacy scripts remain active until SongPlayer is verified working identically.

(#221: since lane 3 the "NDI Output" names are each playlist's scene label only — SongPlayer's one NDI output is `SP-program`, and cg OBS's `sp-*` inputs no longer have a sender.)

## CI architecture

The CI pipeline (`.github/workflows/ci.yml`) gates every push to `dev`
and `main`, plus every PR to `main`. Steady-state runtime on cache hit
is ~17 minutes for a dev push (after the 2026-04-25 lyrics-quality
removal).

**Critical path (a dev push):**

```
parallel ubuntu checks (≤56s)
  → Build WASM (~1:40)
    → Build Tauri (~9:50)             ← release compile of sp-server
      → Gate (2s)
        → Deploy win-resolume (~2:15)
          → E2E win-resolume (~3:00)
TOTAL ≈ 17 min
```

`Build Tauri` dominates because `src-tauri` is excluded from the
workspace and uses cargo's default release profile (codegen-units=16,
no LTO) — already maximally parallel for the Windows runner. Cache
(`Swatinem/rust-cache@v2`) is in place and hits reliably; the time is
genuine release optimization of sp-server's dependency graph.

**Hard rules:**

- **No sleep-based CI jobs.** Any job whose runtime is dominated by
  `sleep` / `Start-Sleep` / `time.sleep` is forbidden. If a soak window
  is needed for trend analysis, it goes into a scheduled workflow
  (cron), not the post-deploy critical path. Cron-scheduled jobs do
  not gate dev pushes.

- **Self-reported metrics are not quality gates.** A pipeline reporting
  its own `confidence` is not a quality signal. Real quality gates
  compare against ground truth (hand-verified fixtures, known-correct
  reference data, or human-perceptible behaviour exercised end-to-end
  via Playwright on the deployed target).

- **`measure_lyrics_quality.py`** stays installed on win-resolume at
  `C:\ProgramData\SongPlayer\cache\tools\measure_lyrics_quality.py` as
  an ad-hoc trend tool. It is no longer wired into CI.

- **E2E must not switch to disruptive OBS scenes.** The post-deploy
  Playwright suite shares win-resolume's OBS with the LED wall and live
  audio. The "off-program baseline" picker (`pickBaselineScene` in
  `e2e/post-deploy.spec.ts`) prefers `sp-slow` and falls back to any
  sp-* scene that isn't `sp-fast` (under test) or `sp-warmup` (sync
  tone). Non-sp scenes (such as a QR-code/sync-tone test scene) are a
  last resort only. The suite captures the program scene at start
  (`beforeAll`) and restores it at end (`afterAll`); never leave the
  wall on whatever scene the last test happened to switch to.
  The A/V gate (`post-deploy-av-sync.spec.ts`) also puts cg OBS on its own
  probe scene "A/V gate (SP-program)" for the take (#221 lane 3: it records
  `SP-program` there); `afterAll` idles the probe first, then restores the
  program scene, then cg OBS's own scene.
