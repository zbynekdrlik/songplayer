---
paths:
  - "crates/sp-server/src/video_upgrade/**"
  - "crates/sp-server/src/api/video_upgrade*.rs"
  - "crates/sp-server/src/db/mod_tests_v35.rs"
  - "crates/sp-core/src/video_upgrade_view*.rs"
  - "sp-ui/src/components/video_settings.rs"
  - "e2e/settings-video.spec.ts"
---

# The in-place video upgrade (#223 S11, design comment 6103060545)

A cached song's `{base}_video.mp4` is replaced by a taller stream under the
SAME name. The audio, stems, dub, lyrics and every row's paths stay as they
are, and `normalized` is never reset (revision 2's D9; V4's reset is the
rejected precedent).

## One song (`video_upgrade::run`)

1. **Resolve.** `probe::resolve`: yt-dlp at the video stage with
   `format_spec(format::live_cap)` and `FORMAT_PROBE_PRINT`. It downloads
   nothing and has no yt-dlp lock, like the probe.
2. **Old facts.** `steps::Real::facts` runs `facts_of` through Media
   Foundation, in the playback mode (`video_decode::global().mode()`), on a
   decode thread (`spawn_decode_thread`, COM's STA rule). It reads:
   - the first picture's size and time;
   - the rate and the length;
   - one picture `NEAR_END_MS` (2 s) before the end.

   The facts come from the reader, not from V34: V34 is NULL for every song
   cached before it.
3. **Better?** `better`: more rows than the cached height. MF may pad a
   height to 16 rows (1080 → 1088), so a stream within the padding is no
   upgrade. If it is not better: `no_better`, and nothing is downloaded.
4. **Download.** `-f <the resolved format_id>`, exactly that stream, into
   `{id}_video_upgrade_temp.mp4`. It uses `ytdlp_video_args` and holds the
   process's one yt-dlp lock (`downloader::ytdlp_lock()`, shared with the
   download worker and the self-update). The download is bounded at 30 min.
   The startup sweep removes a temp a crash left (`cache::is_download_temp`).
5. **Verify.** `verify` checks five things:
   - the rows asked, within `ROW_PAD`;
   - more rows than the cached video;
   - a length within 1000 ms of the audio FLAC's (Symphonia);
   - the near-end picture decoded;
   - the first picture within one frame period of the old one's. The lyrics
     and the title follow the audio clock, so a moved start would shift the
     picture against them.
6. **Swap.** `swap::swap` runs under `cache::SONG_FILES`. Every row of the
   video that names a video file must name the checked one, else `Refused`.
   Then:
   - a stale `<name>.prev` is removed;
   - `hard_link(name → name.prev)`;
   - `rename(temp → name)`, one replacing rename.

   A refused rename (a player holds the file) is `Busy`: the link is removed
   again and nothing changes. Or the player keeps reading the old data,
   which `.prev` still holds.
7. **Record.** `format::record` writes V34 on every row of the video, and
   `video_upgrade::record` writes V35 (`video_upgrade_cap`,
   `video_upgrade_state`, `video_upgrade_at`). The cap is written only for a
   settled check (`upgraded`, `no_better`). A `busy` or `failed: …` check
   keeps the stored cap, so it is checked again.

Every outcome except `upgraded` removes the temp.

## The route (`api/video_upgrade.rs`, the pilot)

`POST /api/v1/video-upgrade {"youtube_id"}` answers:

- **200:** the report `{youtube_id, cap, outcome, old, resolved, new, error,
  elapsed_ms}`;
- **400:** not a YouTube id (`tools::is_yt_id`, the value trimmed);
- **501:** off Windows;
- **503:** before the tools are ready;
- **409:** while another upgrade runs (one static slot per process).

The upgrade runs as its own task, so a client that hangs up never stops it
half-way. Allow minutes for a 4K download:

```bash
curl -s -m 2000 -X POST http://10.77.9.201:8920/api/v1/video-upgrade \
  -H 'content-type: application/json' -d '{"youtube_id":"PySFfTurafA"}'
```

Logs: INFO `video upgrade: start` and `video upgrade: done`, or a WARN
`video upgrade: not upgraded: <why>`.

## The worker (S12a, `video_upgrade/worker.rs`, design comment 6103547599)

- **Switch:** `video_upgrade_enabled`, ON only for exactly `"true"`
  (`sp_core::config::video_upgrade_enabled`), read at every tick. OFF by
  default on every node; it goes ON at SNV only once S12b (the safety net)
  is there.
- **Tick:** every 60 s; the first comes 5 min after the start. The switch is
  read FIRST: while it is off, nothing else is asked (no hold noted, no
  query). Then the pure `decide` checks, in order:
  - the background hold (`Job::VideoUpgrade`, "video_upgrade");
  - a due download (`DOWNLOAD_DUE`: new songs come first);
  - a bot-check pause;
  - 120 s since the last START;
  - the cache volume's free space ≥ 50 GiB (`disk::free_bytes`,
    `GetDiskFreeSpaceExW`; none off Windows).
- **Pick:** `next_song` takes one YouTube id that is downloaded and whose
  `video_upgrade_cap` is NULL or under the live cap:
  - a `busy` one only 10 min after its `video_upgrade_at`, a `failed: …`
    one only after 6 h;
  - only songs of active playlists (#261: `p.is_active = 1`, and the
    status counts too: `db::active_playlist!`), the lowest row first;
  - never the test item.
- **Run:** S11's `run`, its yt-dlp at BELOW_NORMAL (ffmpeg inherits the
  class).
- **Outcomes:**
  - a video that fails `verify` is `refused: …` and settled at the cap:
    never downloaded again at it, since the same stream fails the same way;
  - `failed: …` (a step, the rows) stays transient;
  - a failure whose text names YouTube's bot check (`bot_check`: sign in,
    HTTP 429, try again later) pauses every upgrade for 6 h, in memory.
- **`GET /api/v1/video-upgrade`:** `{enabled, cap, pending, upgraded,
  no_better, refused, failed, busy, paused_until_ms, waiting, last}`.
  - The counts are per YouTube id (`worker::counts`).
  - `waiting` is the last tick's `Skip`: `off`, `held`, `download_due`,
    `paused`, `spacing`, `low_disk`, `no_disk_reading`, `no_tools`,
    `nothing_to_do`.
  - The loop and the route share ONE `worker::global()` state.
- Logs: INFO `video upgrade worker: waiting` when the reason changes, DEBUG
  while it stays; INFO `start` / `done`; WARN `not upgraded: <why>`, or the
  bot-check WARN.
- Tests: `worker_tests.rs`, with the shared `test_rig.rs` (the cached song
  and the scripted `Fake` steps that `mod_tests.rs` uses too).

## The safety net (S12b)

- **Retention** (`retention.rs`). The worker sweeps every tick, switch on or
  off. A `.prev` is deleted when:
  - it is an orphan: no row's video is its name without `.prev`, because
    of a rename since;
  - the song has `Started` since its upgrade: `play_history.played_at`
    (recorded at `Started`) ≥ `video_upgrade_at`, compared in whole seconds;
  - 14 days have passed since the upgrade;
  - it is among the oldest while all kept `.prev` files exceed 15 GiB.

  The pure `plan` decides; `sweep` deletes under `SONG_FILES`. The startup
  cache scan leaves `.prev` files alone: none of its patterns match one.
- **Rollback** (`rollback.rs`). The engine's `video_failed` spawns
  `after_failed_open(row)`; the engine never waits on the file lock. If the
  row's state is `upgraded` and its `.prev` exists:
  - `.prev` is renamed back over the name under `SONG_FILES`;
  - V35 on every row becomes `rolled_back`, settled at the live cap (never
    upgraded again at it);
  - V34 becomes NULL.

  The engine's own retry then plays the old video. Any other failed open
  changes nothing. Test:
  `failure_retry_tests.rs::a_failed_open_of_an_upgraded_song_puts_its_old_video_back`.
- `GET /api/v1/video-upgrade` counts `rolled_back` too.

## Nastavenia "Video: sťahovanie a 4K" (S13, design comment 6104191449)

- `sp-ui/src/components/video_settings.rs`, inside the main settings form
  and saved by its Save. It has three controls:
  - the cap select `settings-max-resolution`: `""` = "Automaticky (4K s
    dekódovaním na GPU, inak 1440)", then 2160 / 1440 / 1080 / 720; a value
    set through the API shows as "N (vlastné)", never as automatic. #282:
    its label is "Najvyššie rozlíšenie (sťahovanie aj prehrávanie)" and the
    hint `settings-max-resolution-hint` says a taller video is scaled at
    playback, from the next song, without a restart (the owner, verbatim,
    17.10.2026: "musi byt sposob to zmenit", a switch per site when 4K is
    a problem; ROZHODNUTÉ on #282);
  - `settings-video-hw-decode` → `video_hw_decode`;
  - `settings-video-upgrade` → `video_upgrade_enabled`.
- Each control is sent only when it differs from what the page loaded
  (`sp_core::config::{max_resolution_to_send, video_hw_decode_to_send,
  video_upgrade_to_send}`, #229's rule).
- The status line `settings-video-upgrade-status` comes from `GET
  /api/v1/video-upgrade`, read once:
  `sp_core::video_upgrade_view::VideoUpgradeView::summary_sk`, with the
  Slovak `waiting_sk`; the cap labels come from `cap_label`.
- Mock: `e2e/mock-api.mjs` serves the status route (the switch and the cap
  follow the stored settings) and refuses a bad `max_resolution` like
  `format::checked`. Spec: `e2e/settings-video.spec.ts`.
- The outputs' half of S13 was already there: "Výstupy Spout (Resolume)",
  #239. Both outputs have fixed sizes since S1a.

## The repair of a video Media Foundation stops on (#277, `repair.rs`)

Design comment 6185256383 (Approach 1). Some YouTube H.264 streams carry
SEI NAL units after an access unit's slice data ("late SEI", ffmpeg warns
`Late SEI is not implemented`); MF's H.264 decoder hands out no picture and
reports end-of-stream (SNV: "Remembrance" 0 frames in 9 ms; remuxed with
`-bsf:v filter_units=remove_types=6`, 1790 frames in the 5 s bench).

- **Asked by a play** (`playback::early_end`, `open-failures.md`): the
  engine's `video_failed` calls `repair::ask(video row)` for an early end.
  The requests are in memory (`Requests`, one per row, and one repair per
  YouTube id per process: a failed one is logged once and never retried
  before a restart, except a `busy` one, asked again `BUSY_RETRY_MS`
  (10 min) later for one more try (a Loop playlist can hold the file;
  `busy_until`: no other row of the video runs a repair before then); a
  restart forgets them, and the next failing play asks again). The
  `done` INFO line names the remux's `new_first_ms`: with no picture of
  the old file read, the first-picture check is skipped, so a remux the
  reader reads late shows there.
- **Run by the worker:** every tick (`repair::tick`, first in `run`, before
  the upgrade) takes ONE request due, whatever `video_upgrade_enabled`
  says, behind #230's hold (`Job::VideoUpgrade`; a held request is put back
  first). No disk floor (unlike the upgrade's 50 GiB): a repair writes one
  stream copy of one song, and its `.prev` goes with S12's retention.
- **One repair** (`repair::repair`, S11-shaped): `cached` → the old facts
  (`None` when MF reads no picture) → `Steps::codec` (MF's subtype) →
  `remux_args` (H.264 only: `-map 0:v:0 -c copy -bsf:v
  filter_units=remove_types=6 -movflags +faststart`; any other codec is
  `unsupported`, nothing written: an AV1 OBU of type 6 is a frame) →
  `Steps::ffmpeg` into `{id}_video_repair_temp.mp4` → its facts →
  `verify_repair` (the checks `verify` shares via `plays_like`: the audio's
  length and the near-end picture; when MF read a picture of the old file,
  also its rows and its first picture within one frame (a stream copy
  moves neither, so with no old picture they are not compared); never
  "taller") → `swap::swap` (`.prev` kept) → V35
  `video_upgrade_state = 'repaired'` + `video_upgrade_at`, the cap KEPT
  (the song is still checked for an upgrade; `next_song` takes it like any
  state that is not busy or failed). V34 is untouched (a stream copy keeps
  the format). Every outcome but `repaired` removes the temp and records
  nothing.
- **S12 unchanged:** retention keys the `.prev` on `video_upgrade_at`
  (played since, 14 days, the budget); rollback stays for `upgraded` only
  (a repaired file's failed open would restore the file that never played).
- **At the download** (`repair::check_download`, called by
  `downloader::process_next` after the normalize, before `place_video`):
  the fresh `{id}_video_temp.mp4`'s facts; one MF reads NO picture of
  (`has_no_picture`: `NO_PICTURE`, the late-SEI case; a later stop is the
  player's, with its 10 s slack, never the facts' 2 s near-end read; an
  open error is not one) is remuxed and renamed over itself. Not repaired =
  the download fails (its reason on the row, the normal backoff, the audio
  dropped unless a row records it: `drop_attempt_audio`), so it never
  reaches a playlist. Off Windows `facts` refuses, so nothing is checked.
  A pair adopted from a peer (`peer::download::adopt`) is not checked: PP
  repairs SNV's late-SEI files at their first play.
- **The temp** is in `TEMP_RE` (`cache.rs`), swept at startup.
- **`peer_hashes`** follows by itself (keyed by path, held only while size
  and mtime match).
- **Logs:** INFO `video repair: asked` / `start` / `done …`, WARN `video
  repair: not repaired - tried again only when busy (10 min), else after a
  restart`; at a download INFO `download: the reader that plays the video
  reads no picture of it - repairing it (#277)`, ERROR `download: the video
  plays no picture and was not repaired (…)`.
- **Not live-gated:** that the box's ffmpeg has the `filter_units` bsf is
  proven by the box check below, not by a post-deploy gate (a missing one
  would WARN `the remux: ffmpeg exited …` once per video).
- **Box check** (the main session): play SNV `video_id=262` once; the
  worker's next tick (≤ 60 s; the first tick comes 5 min after a start)
  logs `done`, the next play passes 10 s, and the ledger shows no
  `underrun_end` from it.
- Tests: `repair_tests.rs` (the args, `has_no_picture`, `verify_repair`,
  whole repairs over real temp files, the peer hash entry after a swap,
  the download check, the requests, a busy retry and the worker's step);
  the Fake steps script `codec` / `remux` / `repaired`.

## The exchange

SNV's video changes under the same path. Its catalog keeps the old sha until
the hasher's next pass, so a peer's fetch in between fails closed (size / sha
check) and asks again (`peer-exchange.md`). PP keeps its own copy.

## Tests

- `video_upgrade/mod_tests.rs`: `facts_of` over a scripted `VideoStream`;
  `better` and `verify` at each bound; whole runs with a scripted `Steps`
  over real temp files and an in-memory DB (upgraded on every row,
  no_better with nothing downloaded, a failed check, busy, each failing
  step).
- `swap_tests.rs`: the file half and the rows' check.
- `api/video_upgrade_tests.rs`: the refusals through the router (501 on
  Linux, 503 on the Windows job).
- `steps.rs` and the route handler are `mutants::skip` glue.
