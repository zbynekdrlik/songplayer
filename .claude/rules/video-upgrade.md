---
paths:
  - "crates/sp-server/src/video_upgrade/**"
  - "crates/sp-server/src/api/video_upgrade*.rs"
  - "crates/sp-server/src/db/mod_tests_v35.rs"
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
  - active playlists first, then the lowest row;
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

## Not yet (S12b)

- `.prev` retention: until the first `Started` after the upgrade, or 14
  days, within a 15 GiB budget (oldest first). Until then, `.prev` files
  stay in the cache.
- Rollback on an open failure of an upgraded song (`.prev` back under the
  name, `rolled_back`, settled).

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
