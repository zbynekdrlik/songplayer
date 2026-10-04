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

## #224 — follow a dantesync date step at the boundary it lands, every audio block on its boundary (0.69.0-dev.10)
- Design 5890605448 (Approach 1). `WallClock::tick` probes every boundary after the unchanged 100-frame resample: one bracketed read against `Anchor::line_at` (the anchor line extended back through a hold); pure `wallclock_anchor.rs::decide_step_probe` — ≤ 2 ms quiet (the resample slews), > 2 ms from a > 200 µs bracket rejected, else a same-tick full anchor sample, narrow + ±1 ms (`same_step`, shared with `decide_anchor_step`) → ONE follow (step ahead / ONE hold of the whole step, against the frozen wall). A resample-armed 1 ms counts toward 2 ms and into the total; a follow restarts the resample count. Telemetry `wall_anchor_probes_rejected` / `wall_anchor_detect_to_follow_us`; INFO `wallclock: UTC step followed at once by the boundary probe (#224)`.
- Audio: pacer (`service` / `service_standby` / standby pair), SP-program's own standby + mix blocks and the NDI input block are stamped on their boundary, never the emit instant (a catch-up burst used to bunch). `ProgramOutput::submit(job)` / `NdiInput::service(B, bus)` lost the emit-instant param.
- Commits: RED f9b31847 → GREEN f667449f, refactor e79df7f7, docs 055d5f9e; review r1 RED a1639700 → GREEN b7ca7dba (frames reset, armed-in-threshold, resample-follow detect time, submitter pin); r2 RED 5117ce52 → GREEN 091dedb4 (`SettableClock` on one synthetic line — its real-`Instant` pairing made the probe follow phantom steps); r3 test 595d93c3 + docs. Tests: `wallclock_tests_probe.rs`, `wallclock_tests_probe_rule.rs`, `pacer_tests_wall_anchor.rs` (+90 ms burst at 151/152, −1.5 s = 1.5 s + one slot). 3 fresh-context review rounds, own Python models + fuzz, 63 diff mutants mapped (5 unviable, 58 killed).
- Not local-verifiable (Tier-0): the build, the tests, the box follow at a real date step, the A/V gate across a step — the main session's integration + box check.

## #136 reopen — a song's files move as ONE set; stems/dub left under an old name are re-linked; `stems_state` ready = the files exist (0.69.0-dev.11)
- Design 5894034820 (Approach 1). `downloader::cache` owns a song's COMPLETE set: `derived_files(audio)` = stems + dub + transcripts. `rename_song_files` moves it as one unit through `move_as_unit` (a `FileOps` seam; a replaced file is set aside as `<name>[.N].replaced` and given back on rollback; a stat error rolls back, never "absent"; a case-only rename is never set aside). `MoveFailed{error, stuck}` + `in_effect_after_failure` record where each file really is.
- `ReprocessWorker` reads the recorded set under `cache::SONG_FILES` and writes the moved paths to EVERY row that recorded it. `song_relink` re-links, under the same lock, at startup (`relink_derived_files`, from `self_heal_cache`) and after every stem/dub job (`relink_song(…, written_for)`, where the job's own output wins). Stems that no name holds → `requeue_lost_stems`. A ready dub no name holds → WARN + count, never reset.
- `self_heal_cache` keeps orphan halves that a row records, and WARNs leftover `.replaced` files. `remove_duplicates` keeps the dub for adoption. `StemsState` ready = `stems_on_disk(audio_file_path)`. Tripwire WARN: `stems_left_behind`.
- Commits: bump dace8f99; fixtures f61d4abd; RED 21ed5dd3 → GREEN 88cb6d6e. Review rounds 1–11 (11 fresh-context passes), each with RED→GREEN: r1 d6c286f3→9db97605, r2 96b09660→02247965, r3 8c9fda75→3d20949e, r4 9e0eaa27→6e49bc45, r5 d0a054f8→330bb94d, r6 b2ce3ff4→6dbdd3d8. Exceptions, where the fix and its tests landed in one commit: r7 3da3cf26, r9 fad3261d; 8ab5ef9c is typed test but carries a production change.
- Not local-verifiable (Tier-0): the build, the tests (incl. the Windows-only case-only RED), the mutation gate, the box repair (~99 songs re-linked at the first start).

## #144 — lyrics text quality: two-way gate, covers by title, caption lines (0.69.0-dev.12, lane worktree-agent-aa554183c4d277cac)
- Design 5896270815 (Approach 1); Anchors-confirmed 5899031536 (deviations: LCS for the sung direction, not the anchor walk; the title search in the worker after isolation); measurement 5899043518.
- Gate: RED ab30a907 (3 tests, old gate Pass) → GREEN e0befa19 (`sung_coverage.rs`, 0.55 / 25 s, `GateStats` + audit, PASS audit). Captions: RED 78a2ea49 → GREEN 22cf0475 (`parse_json3` one line per sung line). One transcript: 3900193f (`worker_text_tiers.rs`, worker.rs 1000 → ~910). Title search: e1abf8eb (`lrclib_search.rs`, `genius::search_by_title_at`, `title_search.rs`, `text_candidate.rs`).
- Review r1 (0/3/10): keep-repeats RED 5a61c4fe → GREEN 1a8908b7 (cleanup caches v3); eac052fc (reference choice, `transcript_cache.rs`, next-best, audit label). r2 (0/1/8): b22f0ee1 (transcript serves one pass, priority-pick comparison, `should_title_search`). r3 (0/1/5): dba298c5 (typed test, carries small production changes: retire-wiring test, stale gate audit removal, idle gate on empty transcript).
- r4 (0/2/3): one end-of-pass retire point (`ends_the_pass`, tested), the offline test also pins the stale-audit removals; the reprocess set widened (below); rule-file fixes.
- NO LYRICS_PIPELINE_VERSION bump. After deploy: targeted manual_priority reprocess of every `yt_subs*` row, EVERY ★ row, i.e. `lyrics_source LIKE '%+mtl@rev1%'` (description, genius, lrclib, yt_subs, override, tier1:spotify — the gate changed for every source, and genius / plain-lrclib texts were cleaned by the deduping prompt; `_cleaned_v3` re-cleans them) and every base-tier `gemini-3-5-transcribe*` row.
- Not local-verifiable (Tier-0): the build, the tests, the mutation gate, the box behaviour.

## #224 part 2 — a fleet date step relabels time, it does not move content (0.69.0-dev.13, lane worktree-agent-a582e3b6406c36bed)
- Design 5899388193 (camera-box confirmed 5900288123, measurements 5898834252); STEP 0 5902159041; Anchors-confirmed 5902707039; review records 5903529410 / 5903909747 / 5904175799 / 5904380082.
- `fleet_shift.rs`: N = ⌊S/P⌋ relabel + remainder r; the process-wide `FleetShift` registry (epochs, K_F, the published `FleetLine`, `join`); `WallClock` reads the timeline `UTC − D(K_w)`, `regrid` moves it by r only, `rejoin` after > 10 s idle, no resample inside a hold; wire edge `floor(b + D(K_F))` in `submit_frame_at_boundary_owned` (audio moved by the video's relabel), `submit_nv12` `+ D(K_F)`, the #192 emitter `t + D(K_F)`; VBAN (`vban_clock.rs`) owes the line's net movement per tick at 40 ppm, over one slot + 3 ms taken at once; readers comparing realtime with stamps read the timeline; status/log stamps on the wire.
- Commits: bump 6355b373; RED b65d78e1 + 55ee7674 → GREEN 5aba5918; docs 4d8954c2; d0f00ce9. r1 (3🔴 4🟡): RED 913d3974 → GREEN 19216077, docs 3c86fe3b; own fuzz: RED a494fb72 → GREEN 4847c146 (3 ms residue), docs 559b64bd, 624ffd28. r2 (2🔴 1🟡): RED 10cc290c → GREEN 2e2b3709, cfdf40fd, 576622be. r3 (0🔴 3🟡): RED d0a72f38 → GREEN b68731fd, docs af4e0597. r4: 0🔴 0🟡, docs 7716757c.
- Decisions: K_F = the registry's K; no min() clamp at the wire; holds by the timeline's movement; timeline anchor to `read_100ns`; adoption = closest run within 3 ms (twice a wall's worst line error), a step within 3 ms of nothing registered is local (N = 0); a wall idle > 10 s or built now JOINS the fleet's published line (never registers its drift); VBAN 40 ppm (≤ 47 keeps ±100 ppm), owes the line's net per-tick movement; NDI input follows its wall (one boundary ≤ r early).
- Residuals (documented): a tick's net movement over the cap taken whole; an armed 1 ms whose follow lands a tick later; a wide own-sample anchor registers its error (adopted fleet-wide); test system walls share the global registry.
- Not local-verifiable (Tier-0): the build, the tests, the mutation gate, the box behaviour (a controlled camera-box step of each sign).

## #144 fix — ONE per-song reprocess path; the per-video route that blanked the wall's lyrics is deleted (0.69.0-dev.14, lane worktree-agent-a8369167e4a677b16)

- Cause: `POST /api/v1/videos/{id}/lyrics/reprocess` → `reset_video_lyrics` set `has_lyrics = 0, lyrics_source = NULL`, and the loader serves nothing for `has_lyrics = 0`. The #144 rollout queued 280 songs through it, and 211 songs showed no lyrics on the wall for ~6 h (comment 5905405307).
- Fix: route, handler `reprocess_video_lyrics`, `reset_video_lyrics` and its own test deleted. `POST /api/v1/lyrics/reprocess {video_ids}` (manual priority, keeps the served lyrics while the song waits) is the one per-song path.
- Commits: bump 62c92a2c; RED 7c7c094d (`a_per_video_reprocess_request_never_blanks_the_served_lyrics` fails: has_lyrics 0, by code walk; pin `the_one_reprocess_path_keeps_the_served_lyrics_and_sets_manual_priority`) → GREEN 38711fdc; docs c4400c8a.
- Review r1 (0🔴 2🟡 3🔵): docs 99e08626 (one PER-SONG path; the bulk sweeps `reprocess-all-stale` / `reprocess-catalog-with-new-gate` share its queue; the rule also loads on `lyrics/worker.rs` + `playback/lyrics_loader.rs`); 534969cb drops the tautological `reprocess_video_ids_sets_manual_priority`.
- Review r2 (0🔴 0🟡 4🔵): 8aaea84e (per-song wording in the pins; drops the all-stale SQL-copy twin `reprocess_all_stale_only_flags_stale_rows`); docs (the quarantine path added to the gap, the real "Spracovať všetky zastarané" label).
- Known gap, not in this lane (follow-up candidate for the supervisor): a re-run that fails still blanks a served song, on two paths.
  - An error: `lyrics/worker.rs`'s `Err` arm calls `mark_video_lyrics(false, Some("no_source"), …)`, and the buckets skip `no_source` at the current version.
  - An empty transcript: the base tier quarantines the song and deletes `<yt>_lyrics.json`.
  - The choice between deferring and keeping the served lyrics is a design decision.
- Not local-verifiable (Tier-0): the build and the tests (CI).

## Release 0.69.0 blockers, lane B — #144, #136, #145, #222 (0.69.0-dev.15, lane worktree-agent-a85875f5fac8a8afa)

- Design 5906414112 (issue 222, same on 145), findings 5905727755 (🟡3, 🔵4-8, the #144/#222 items of 🔵11), ROZHODNUTÉ 5905945274 (item 1). STEP 0: 144 c5906592936, 136 c5906593907, 145 c5906594595, 222 c5906595662. Anchors-confirmed: 144 c5906604555, 136 c5906605153, 145 c5906605749, 222 c5906606456.
- #144 a failed / empty re-run never darkens a served song: refactor f3d856b1 (`fail_song`, `quarantine_empty_transcript`), RED adea2370 → GREEN d93cd502 (`serves_lyrics` = has_lyrics 1 + `<yt>_lyrics.json`; `keep_served_lyrics` → `record_served_lyrics_failure`: attempts + backoff via `record_lyrics_deferral`, manual priority 0; `next_backoff` shared with `defer_song`); docs 87db9b14 (CLAUDE.md bump list = owner approval, else targeted reprocess; skill stale lines; gather.rs log). r1: RED 2c121449 → GREEN eadeb0ee (the failed pass stamps `lyrics_processed_at`, full-mix once a day).
- #136 🟡 correction final: RED 12837986 → GREEN 4b257e97 (PATCH song/artist → `gemini_failed = 0, metadata_source = 'manual'`; `REPAIR_QUEUE_WHERE` excludes manual). r2: RED 3636fe13 → GREEN 9205bf3c (`reprocess_one` re-checks `REPAIR_QUEUE_WHERE` under `SONG_FILES` → `LeftQueue`; `patch_video` title UPDATE under the same lock). r3: lock pins in 9dbe792b.
- #136 🔵 FLAC E2E: 193f6c81 (`e2e/cache-layout.ts` + mock-suite spec; a split song = no complete pair + one lone video + one lone audio half).
- #136 🔵 rename race: RED a42ca156 (structural order pins) → GREEN 94110d67 (`song_input::job_input` after the heavy slot, under `SONG_FILES`; no audio = no-penalty 10-min recheck). r1: RED 52b06bea → GREEN 1d263195 (`Found` enum, WARN names the path, dub `dub_error`). r2: sp-ui `chain_detail` shows a live chain's `dub_error` (e2e/dabing.spec.ts). r3: RED 9dbe792b → GREEN 4623df80 (found audio clears the note; `set_dub_requested(true)` clears `dub_error`).
- #145 README: 5f981557 (rollback = 8.0.4's own backup only; 6.9.27 502 / 7.3.1 400 cannot serve `claude-opus-5-5`; real backup paths; probe example).
- #222 Presenter dedup on the whole payload: refactor 9eb197be (key type = `PresenterLines`) → RED 17d1d54b → GREEN 5e14f7e0; docs 656c8363 (lyrics-display.md: SK checked against `MAX_CHARS`; counts 137/68/53, 41/79/1).
- Reviews (fresh-context, read-only): r1 0🔴0🟡6🔵, r2 0🔴1🟡1🔵, r3 0🔴1🟡1🔵, r4 0🔴0🟡0🔵.
- Kept per ROZHODNUTÉ, documented (lyrics-reference-text.md): a current-version manual row leaves the queue after one failed attempt; stale rows retried ≤ daily; attempts feed the #171 full-mix fallback; the 30-min cap stays policy.
- Follow-up candidates (supervisor): a title correction applies to ONE row (the same video in another playlist, and a re-download, still overwrite it; metadata-providers.md).
- Not local-verifiable (Tier-0): the build, the Rust tests, clippy, the mutation gate, the Playwright suites (the cache-layout rule was run in a node model, 7/7).

## #210 — the FOH VBAN feed never waits for its boundary's video side; the program boundary's timing (0.69.0-dev.17, lane worktree-agent-a8f38ede150873ea0)

- Design 5911744233 (main), findings 5907620763 / 5907883948. STEP 0 c5912914057, Anchors-confirmed c5912922321.
- RED d569c00a (`program_output_tests_order.rs`, `HookedNdi` holds the pair's first NDI send behind a gate: the VBAN queue must already have the block — forwarded / standby / mix) → GREEN a9b9aae2 (feed before the NDI submit; `VbanBlock::copied`; the mix's crossfaded block before its picture) → refactor d06de3cc (`from_frames` deleted).
- Telemetry 9f9a365e: pure `program_output_timing.rs` (`BoundaryMarks` → `ready_late_us` / `vban_feed_late_us` / `submit_us`; two 1800-boundary buckets; strictly-over-5-ms counts; one WARN per 5 s over 10 ms with `suppressed`), `ProgramOutput::serve(job, now)`, `health.timing` on `/api/v1/program`, e2e mock mirrored.
- Reviews (fresh-context, read-only): r1 0🔴2🟡4🔵 (fixed 9177cd49 + e2d1bf18: `serve` = `split` → `feed_vban` → `submit_video`, the order test claims only the held point, the acceptance diffs the counters, `end_mix_run` after the submit, a neither-side test), r2 0🔴0🟡2🔵 (b5172508), r3 0🔴0🟡2🔵 (7507058c: the date-step caveat), r4 0🔴0🟡2🔵 (d793b8ed, docs only).
- Integration note: origin/dev moved to `chore: release 0.69.0` during the lane; VERSION 0.69.0-dev.17 (as dispatched) needs the re-bump at integration.
- Not local-verifiable (Tier-0): the build, the Rust tests, clippy, the mutation gate (69 mutants listed, each mapped to a killer or unviable by four review rounds); the box acceptance (15 min dev1 capture, `health.timing` counter diff) is the main session's.

## #225 — the Player never claims a state it was not told; the badge reads the WS state (0.70.0-dev.5, lane worktree-agent-ad996fd54da6a9402)

- Design 5917562242 (main, Approach 1). STEP 0 c5975001630; Anchors-confirmed c5975004341, adapted: the replay re-tells the engine's last broadcast, not the 5 s health sample; it tells every playlist; pure `sp_core::player_view`.
- Server: `playback/dashboard_replay.rs`, a process-global record that the engine's one send point `send_dashboard` writes.
  - The send sites: `broadcast_state` (incl. SetMode and `remove_pipeline`), Started, and the position ticks.
  - `on_connect_replay` covers every DB id (`db/models_playlists.rs`): a non-Idle playlist gets its last NowPlaying first, then its state; the rest an explicit Idle; the engine's mode.
  - A lagged client resubscribes, then gets the replay.
- UI: `NowPlayingInfo.state_known`; `player_view` covers the view, title, label, badge, toggle and mode.
  - The store forgets on every socket open/close; a live Idle drops the song.
  - The auto-select waits for `selection::states_known`.
  - The preview Effect waits for the state.
- Commits: refactor bb3bc6b8 (the seam); RED d0f580e3 → GREEN f23fcfaf. Review rounds, each RED → GREEN:
  - r1: f963fd2e → 24f37f5e;
  - r2: 1d47116a → d17f9943;
  - r3: 7ff74cd6 → 8b560c26;
  - r4: 63448628 → 79e6ce1c.
- Tests:
  - `tests_ws_replay.rs` (4);
  - `dashboard_replay_tests.rs` (10);
  - `models_playlists.rs` (1);
  - `player_view` (7);
  - `e2e/player-known-state.spec.ts` (7, MutationObserver logs). The mock: `sendReplay`, `sendLive` (the replay-first order), `/__mock/ws-replay {delay_ms, now_playing_delay_ms, no_song}`, `/__mock/ws-drop`. Box specs use `NOT_A_SONG`.
- Reviews (fresh-context, read-only):
  - r1 0🔴 2🟡 9🔵: DB mode → engine mode + SetMode broadcast; E2E windows from the log.
  - r2 0🔴 2🟡 7🔵: forget on reconnect; Idle drops the song; removed pipeline told Idle; lagged replay.
  - r3 1🔴 1🟡 4🔵: the mock's live messages overtook the replay; preview + selection through a reconnect.
  - r4 0🔴 2🟡 6🔵: toggle/mode neutral until known; forget + Started pinned; resubscribe; `states_known`.
- Follow-up candidate (supervisor): the engine ignores `playlists.playback_mode` and `PUT …/mode` is never persisted.
- Not local-verifiable (Tier-0): the build, the Rust tests, clippy, the mutation gate (35+ mutants mapped by four review rounds), the new UI's E2E (the 7 new specs fail on the old dist; the rest of the suite passes with the new mock).
