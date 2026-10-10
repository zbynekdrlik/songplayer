---
paths:
  - "crates/sp-server/src/playback/program_burn*.rs"
  - "crates/sp-server/src/playback/program_item*.rs"
  - "crates/sp-server/src/playback/program_output_tests_burn.rs"
  - "crates/sp-server/src/playback/handle_pipeline_event_tests_item.rs"
  - "crates/sp-server/src/test_item*.rs"
  - "crates/sp-server/src/api/test_item*.rs"
  - "crates/sp-server/src/api/program_tests_burn.rs"
  - "e2e/post-deploy-test-item.spec.ts"
---

# The SP-program burn 911014, the on-air item, the local test item (#228)

camera-box measures the CG segments (SongPlayer → cg OBS → strih → stream →
YouTube) with SongPlayer's origin burn and its own measurement clip. Design
record: #228 comment 6091235661. Parts "the A/V gate waits for the probe's
audio" and "SongPlayer holds the rig lease" stay parked on camera-box.

## The burn (`playback/program_burn.rs`, pure)

- Payload = #151's: `P{run}.{frame}.{gen_ts_ns}.{crc32}`, CRC-32/ISO-HDLC of
  the dotted body (#151's fixture `eval/fixtures/burn/sp-burn-1080p.txt` is
  pinned). `{frame}` = the on-air item's frame index from its frame 0 on the
  30 fps grid, `{gen_ts_ns}` = the boundary's WIRE stamp × 100.
- **Its place is NOT #151's.** #151's bottom-right `(1578, 738, 302, 302)` IS
  camera-box's `BurnSlot::BottomRight` (`src/burn_regions.rs`; the five
  slots fill the bottom band), and the clip's two QR images sit in the top
  band (`[147, 813)` / `[1107, 1773)` × `[24, 690)`, measured on its frame
  1800). So: TOP-RIGHT, a fixed version 3 QR at level M (every payload fits,
  the longest is 48 characters), 29 modules + a 4-module quiet zone = 37,
  one module = height / 270 (4 px on 1080), 3 modules of margin:
  `(1760, 12, 148, 148)`, dark modules from x 1776 (right of the clip's right
  QR image). Only its light quiet zone overlaps the clip's white quiet zone
  (13 px, light on light). Tell camera-box when this moves: its decoder
  finds 911014 by a top-right crop (no slot for it in `burn_regions.rs`).
- The paint: luma 16 / 235, chroma 128, nothing outside the square, into a
  COPY (`burned`): the canvas picture can be the source's own allocation,
  which `SP-program-MAX` and the Spout FHD sender hold — they never carry it.
- `program_burn_tests.rs` decodes the painted canvas with rqrr 0.9.3
  (camera-box's decoder; dev-dependency WITHOUT default features, or it
  drags `image` into the lock) on white, grey and black, and next to two
  clip-like QRs.

## The item on air (`playback/program_item.rs`)

- A live paced pair carries `SubmitJob::media_pts_100ns` (the pacer's 0-based
  pts, `from_paced` with `live`); every standby / fill / NDI input pair has
  `None`. A new `SubmitJob` literal needs the field (≈ 20 test sites).
- The engine marks every play's media clock (`Started` / `Seeked`
  `position_ms`, `handle_pipeline_event.rs::mark_item`), on air or not.
- The sender's `ItemTrack` (per served boundary: its source, its media pts,
  its wire stamp, the source's newest mark): media = start + pts, frame =
  `grid_frame(media)`, `started_at` = wire − media at the first frame after a
  mark or after the pts went back (a loop); another source ends the item, a
  frameless boundary (paused, fill, pre-roll) keeps it. A fade follows its
  incoming side. Marks and the publish are `try_lock`ed: the sender never
  waits. Known edge: the first frames of a resume/seek can arrive before its
  mark (the trace's race) and are numbered on the old start.
- `ProgramItem` on `ProgramBus::item()`: the burn switch (`POST
  /api/v1/program/burn {"on"}` → `{"burn_on"}`, in memory, default OFF),
  `burned_boundaries`, and `on_air_item` on `GET /api/v1/program` (+ the
  video's `youtube_id` / `title` from the store).
- Only a boundary that shows an item frame is burned: standby black, fills,
  the pre-roll and a paused picture never are.

## The test item (`test_item.rs`, `api/test_item.rs`)

- A playlist of kind `test` (free text, no migration): mode `loop`, NDI name
  `SP-test` (scene `sp-test`, pressed through the facade), one video
  `measure-v01` (`metadata_source = 'manual'`). Never synced (startup sync =
  `kind = 'youtube'`); hidden from `GET /api/v1/playlists` (dashboard,
  Program control, Live page; while on program the dashboard names it
  `#<id>`).
- **Every worker queue and its dashboard count carries
  `test_item::not_test_item!()`** (lyrics `queue_sql`, stems
  `STEM_ELIGIBLE_PRED` + `get_next_video_for_stems` + `count_stems_progress`,
  dub `get_next_dub_job`, `REPAIR_QUEUE_WHERE`, `DOWNLOAD_DUE`,
  `api/lyrics.rs::fetch_queue_counts`). A NEW queue adds it too and a row in
  `test_item_tests_queues.rs`. The column is unqualified on purpose: it reads
  the innermost `videos` (bare, `v`, or a correlated `s`). Use it through
  `concat!` (a macro, so a const can carry it).
- Import: `POST /api/v1/test-item/import {"file"}` reads `<data
  dir>/bench/<file>`, checks the pinned sha256 (`TEST_CLIP_SHA256`, a byte
  array: a 64-hex string trips the staging hook), runs ffmpeg twice into
  temp names (video `-c:v copy`; audio `-c:a flac -sample_fmt s32`, NO
  filter / `-ar` / `-ac` / gain), renames both into the split layout under
  `cache::SONG_FILES`, upserts the row, `EnsurePipeline`. `already` when the
  row records both files on disk. 400 / 404 / 422 (both hashes) / 503 / 500.
- `POST /api/v1/test-item/start` = `PlayVideo {position_ms: 0}`, `/stop` =
  Pause; `GET /api/v1/test-item` = ids, scene, clip name, sha256.

## Box procedure (main session, once per box)

1. Put camera-box's clip on the box as
   `C:\ProgramData\SongPlayer\bench\measurement-clip-v1-128s.mp4` (dev1:
   `~/.claude/work-products/issue-1404/measurement-clip-v1-128s.mp4`,
   20 784 509 B, sha256 `a0118ad7 74191253 b7dad88f 4548ba7e 89a9975f
   6466782b 089424a0 f770a748`).
2. `POST http://<box>:8920/api/v1/test-item/import` with
   `{"file":"measurement-clip-v1-128s.mp4"}` → 200 `imported` (again:
   `already`); the log line `test item: the measurement clip is the test
   item`.
3. `GET /api/v1/test-item` → `imported: true`; `/api/v1/ndi/health` has an
   `SP-test` row.

## Tests

`program_burn_tests.rs` (payload, place vs the slots and the clip, paint,
copy, rqrr round trips, the notice), `program_item_tests.rs`,
`program_output_tests_burn.rs` (FHD canvas over the mock: off = the same
allocation, on = the burn in a copy, the source untouched, standby unburned,
a fade, the published item), `handle_pipeline_event_tests_item.rs`,
`api/program_tests_burn.rs`, `test_item_tests.rs` (args, sha gate, import
with a fake transcoder), `test_item_tests_queues.rs`, `api/test_item_tests.rs`
and the read-only `e2e/post-deploy-test-item.spec.ts` (never turns the burn
on, never starts the item, never cuts).
