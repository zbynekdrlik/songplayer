---
paths:
  - "crates/sp-server/src/background_hold*.rs"
  - "crates/sp-core/src/background_hold*.rs"
  - "crates/sp-server/src/downloader/mod.rs"
  - "crates/sp-server/src/lyrics/worker.rs"
  - "crates/sp-server/src/stems/worker.rs"
  - "crates/sp-server/src/dabing/worker.rs"
  - "crates/sp-server/src/reprocess/mod.rs"
  - "crates/sp-server/src/peer/mod.rs"
  - "crates/sp-server/src/lib.rs"
  - "crates/sp-server/src/api/routes_status*.rs"
  - "sp-ui/src/components/health_bar.rs"
  - "e2e/health-background-hold.spec.ts"
  - "e2e/obs-baseline-scene.ts"
---

# Background hold — a `sp-90s` press stops new background jobs (#230)

The owner's rule (6.10.2026, ruled 10.10.2026): when the Stream Deck's
`sp90s` scene goes on program (~1.5 min before the service) SongPlayer starts
NO new background job; `sp-slow` on program, or 4 h, ends it. A job that
already runs finishes ("blokne ďalšie spracovanie").

- **The constants are the rule, not settings:** `sp_core::background_hold`
  `HOLD_SCENE = "sp-90s"`, `RELEASE_SCENE = "sp-slow"`, `HOLD_FOR_S = 14_400`
  (scene = the playlist's NDI name lowercased, ASCII case ignored). A future
  trigger (Arena's "5min" column, not visible to SongPlayer today) is a new
  trigger, never a renamed knob.
- **ONE persisted truth:** the setting `background_hold_until_ms` (UTC unix
  ms). Held ⇔ now < it. Read live at each job start
  (`background_hold::holds(pool, Job)`, the `paid_ai::enabled` pattern); an
  unreadable or mangled value holds NOTHING (the hold guards the service, it
  never wedges processing). A restart during the service keeps the hold.
- **The watcher** (`background_hold::start`, spawned in `start_program` AFTER
  `restore_selected_source`): every `ProgramBus::on_air()` publication after
  its subscription is a cut; `sp-90s` arms now + 4 h (a re-press re-arms),
  `sp-slow` deletes the setting, the end instant deletes it. The restored
  startup source is no press (published before the subscribe). `run` takes
  `hold_for` so a test sees the end pass.
- **Every job start asks, BEFORE anything else of the job:** download
  `process_next` (before the pick and the peer ask), lyrics `process_next`
  after its kill switch (before the stand-in supersede and both translation
  passes), stems + dub `process_next` after their kill switches (dub: before
  the one-shot subtitle backfill), the metadata repair's per-row loop
  (`break`), `lib.rs`'s sync consumer (drops the request; the 10 min periodic
  sync re-enqueues) and the daily yt-dlp update (skips the tick), and the peer
  exchange through `Exchange::transfers_paused` (= `peer_transfers_paused` OR
  the hold: no fetch, 503 on every peer route, no hashing). A NEW background
  job asks it too. Startup one-shots and the playback path never ask.
- **Visible:** `GET /api/v1/background-hold` (`sp_core::background_hold::
  BackgroundHold`: held, until_utc_ms, remaining_s, the two scenes, the jobs
  that found the hold since it armed); the health bar polls it every 5 s and
  shows the amber "Pozadie: pozastavené (ešte 3 h 58 min)" only while held.
- **Tests:** the held-job set is process-wide — the module's tests hold one
  `SERIAL` lock and assert only `Job::YtdlpUpdate`, which no other test notes;
  every other module's gate test arms its own pool with
  `background_hold::hold_for_a_minute` and releases with `end_hold`, and
  shows the tick starts nothing held, then runs after the release.
- **The exchange shows why it pauses:** `Exchange::pause_reason` = the
  operator's `peer_transfers_paused` ("operator") or the hold
  ("background-hold"); the peer API's 503 names it in `x-sp-paused`, the
  exchange status keeps `transfers_paused` = the operator's own pause and
  adds `background_hold`. PP's peer gate therefore FAILS while SNV is held
  (no transfer is possible), with "paused there (HTTP 503)" — a held peer
  is a real "cannot transfer now", never skipped.
- **The gates keep off `sp-90s`:** `e2e/obs-baseline-scene.ts` disallows it
  as a baseline and its last resort takes any other scene first; PP's
  `pickPlaylistScene` shares it, so `sp-90s` is pressed only when it is the
  one playable scene. A suite's afterAll that restores the start scene
  re-presses `sp-90s` only if it was on program at the start (and the
  `sp-slow` baseline released a running hold in between).
- **Known limit:** the on-air watch keeps only the newest value; two cuts
  inside one watcher wake (`sp-90s`, then another within a DB round trip)
  count as the last one only.
- **The timed end logs INFO** itself (`release(.., timed: true)`): a read
  at the end instant no longer counts the hold as held.
