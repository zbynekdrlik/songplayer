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
  - "scripts/wall_clip.py"
  - "scripts/wall_frames.py"
  - "scripts/tests/test_wall_clip.py"
  - "scripts/tests/test_wall_frames.py"
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

## The clip list and the wall clips (#272 lane 1)

The owner (14.10.2026): whether the wall gets every frame, in order, is
MEASURED with frame ids, never judged by eye. Design record: #272 comment
6164497836.

- **`test_item_clips.rs`: a pinned LIST** — `CLIPS` = camera-box's clip
  (`CAMERA_BOX`, the default; #228's row `measure-v01` unchanged), then
  `wall-24` / `wall-25` / `wall-30` (rows `wallclip-24/25/30`, files
  `wall-clip-v1-{fps}fps-60s.mp4`, artist `SongPlayer`, 60 s). Each is a
  ROW of the one test playlist, never a playlist of its own (a scene in the
  catalog and an idle pipeline per clip on the boxes); `not_test_item!`
  goes by the playlist's kind, so every row stays out of every queue.
  sha256s are byte arrays, like camera-box's.
- `import(file, clips)` imports a file as the listed clip whose sha256 it
  has, under that clip's names; `import_listed` takes each clip from the
  bench under its OWN file name (`absent` is no error; `wrong_file` names
  the sha256 found; `failed` the error).
- Routes: `POST /start` with no body / `{}` = camera-box's clip,
  `{"clip": "<name>"}` = that clip, `400` with a fixed text for an unknown
  name (`UNKNOWN_CLIP`) or a body that does not read (`START_BODY_REFUSED`,
  unknown fields refused), `404` before its import. `/stop` pauses the test
  playlist with ANY clip imported. `/import {"file"}` as #228 (any listed
  clip; the 422 adds `expected_any`); `/import` with no body / `{}` = the
  list import: `200 {clips: [{name, file, outcome, found?, error?}]}`,
  `422` when a bench file is not its clip, `500` when one failed, `503`
  before ffmpeg. `GET` keeps every #228 field
  (camera-box's) and adds `clips: [{name, file, fps, sha256, imported}]`.
  Bodies are bytes → typed structs (`body_or_default`), never
  `serde_json::Value`.
- **Loop's memory keeps the clip started LAST**: a press of `sp-test` with
  no start plays THAT clip (before #272 it could only be camera-box's;
  whether camera-box's own protocol starts its clip first is not
  verified). So every wall run ends with `/start` + `/stop` with no body,
  which leaves camera-box's clip as the one a bare press plays.

### `scripts/wall_clip.py` (the generator, deterministic)

- 20 equal cells across the frame: margin `W // 48`, cell
  `(W − 2·margin) // 20` (184 px at 3840; any width that is a multiple of
  960 keeps the same fractions, which the reader samples by); cells 0 / 19
  black, 1 / 18 white (the references), 2–17 the index in reflected Gray
  code, MSB first, white = 1. Two consecutive indices differ in ONE cell:
  a 2-frame blend shows one grey cell whose level is the weight.
- 8 bands; per band the strip at 10–40 % of its height, the digits
  (`0037  25 FPS`, a 5×7 numpy font, inside the strip's inner cells) at
  52–88 %. Any quarter of the height holds a whole strip row (pinned at
  4K). No QR: `qrcode` is not in CI's eval env.
- Raw yuv420p (luma 16 / 235 / background 40, chroma 128) into libx264 on
  ONE thread, `veryfast` crf 16, GOP = fps, no B-frames, BT.709 tv tags,
  plus a silent 48 kHz stereo AAC track (the import splits it as
  camera-box's), `+bitexact`, no metadata: the same ffmpeg gives the same
  bytes (dev1 regenerated the 25 fps clip byte-identical). A changed
  picture = bump `CLIP_VERSION` (new file names) and re-pin the sha256s.

### `scripts/wall_frames.py` (the reader)

- Per picture: 8 sampled columns per cell (`CELL_SAMPLES`, over the
  central half, at the cells' fractions of `--x-range`;
  `sample_columns` → `read_columns`), the rows whose references read black /
  white with contrast > 25 % of full scale and every bit between them, a
  run of ≥ 3 such rows = one strip; its core rows normalised by its own
  references. One grey cell (level 0.04–0.96) → `(i, i + 1, w)`, `w` the
  LATER frame's weight in Q8, rounded (the program's `FrameBlend` unit).
  Never guessed: `no_strip`, `multi` (≥ 2 grey cells), `non_adjacent`,
  `disagree` (the bands read positions > 8 Q8 apart: a torn picture).
- `analyse`: readings within 8 Q8 of the one before = one boundary;
  each step vs `fps / 30`, with a slack of 8 Q8 + the larger of the
  pacer's dead band (2 ms × fps: a boundary inside it shows the plain
  frame) and the reader's level edge (a weight within 0.04 of 256 reads as
  the next frame) — review round 1 found every phase 0.03–0.05 of a frame
  read as cadence errors without it, `test_every_grid_phase_…` sweeps 100
  phases at 24 / 25 / 30 fps: ≥ 2 grid steps = lost boundaries, off the
  grid = cadence error, back = jump back; frames no boundary showed = lost
  frames. A step across an unreadable picture or a capture gap (an
  interval over 1.5 × the median) is `unverified` (steps, frames), never
  lost: the capture did not see it. Every boundary must be shown for the
  refreshes the capture rate gives on the grid (`refreshes`: 60 Hz on 30 =
  2; a rate the grid does not divide allows the two around it; from
  `--refresh-hz` or the capture times) — another count is `uneven`
  (judder), in the verdict; the capture's first and last boundary, one
  with an unreadable picture in or beside it and the loop seam are not
  judged. The clip LOOPS: a wrap is only the last
  3 frames → the first 3; the wrap step ± 1 are the loop seam, reported in
  `loop_seams` and kept out of the verdict (a jump to 0 from mid-clip is a
  jump back). Weights vs the pacer's rule (`genlock.md`, "BLENDED") at the
  grid phase fitted per pass (median, + half a Q8 step for the floor),
  compared with where the READER reads the pacer's weight
  (`shown_position`: one within the edge reads plain): 25 on 30 from 0 =
  0, 213, 170, 128, 85, 42; 24 = 0, 204, 153, 102, 51; 30 / 15 / 50 =
  none. The dead band is the pacer's `BLEND_DEAD_BAND_100NS`, read from
  `pacer_blend.rs` by `test_the_reader_s_dead_band_is_the_pacer_s`.
- Capture glue (untested, tiny): `--video FILE [--width W] [--refresh-hz
  R]` (ffmpeg, grey, `-fps_mode passthrough` so every stored frame is read
  as stored, the size read by `av_sync_check.probe_video_size`: the box has
  no ffprobe; `video_read_args` is tested) or `--dxcam OUTPUT [--region
  L,T,R,B] --seconds S` (Windows, the box's `ops\probe-venv`): only each
  picture's sampled columns are kept (~170 KB at 1080 rows) and decoded
  after the capture, a poll with no new picture sleeps 1 ms (Python 3.11+
  sleeps on a high-resolution timer there); `capture` reports its own
  intervals and gaps. JSON on stdout; exit 0 clean / 1 defects / 2
  nothing decoded.
- Measured on dev1 (#272 comment 6164962209): every frame of the three
  clips reads back; real decoded frames blended 25 on 30, cut to an SNV
  quarter band, ×4 and noised read 0, 214, 170, 127, 85, 41 (±1 Q8).
- The clip's song / artist go to the wall's title clip like any song's
  (nothing treats the test playlist apart there): a strip the title covers
  fails its reference check and the consensus takes the other bands; a
  picture with every visible strip covered is `no_strip`, its steps
  `unverified`, never lost. Whether a test item should write the wall
  title at all is open (a follow-up candidate on #272).

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
4. #272, the wall clips: copy `wall-clip-v1-{24,25,30}fps-60s.mp4` (dev1
   `~/.claude/work-products/issue-272/`, sizes and sha256s in
   `test_item_clips.rs`) into the same bench dir, then `POST
   /api/v1/test-item/import` with NO body → 200, each listed clip
   `imported` / `already` (`absent` for a file not copied, never an
   error); `GET /api/v1/test-item` → `clips[*].imported`.

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

#272: `test_item_tests.rs` also pins the clip list (camera-box's clip
unchanged, the wall clips' generated sha256s, unique ids / files / hashes),
`clip_named`, each clip's cache names, an import as the matching clip, one
playlist for every clip, `find_any`'s order and `import_listed`'s five
outcomes; `api/test_item_tests.rs` the GET's `clips`, a start by name / by
default / refused / 404, a stop with any clip, the list import (absent,
`wrong_file`, `503`, unreadable bodies), its status and pipeline rules; the
post-deploy spec reads the list. The scripts: `scripts/tests/test_wall_clip.py`
(Gray code, layout, any quarter band, frames, the pinned encode, a failed
encode leaving no ffmpeg and no partial file) and
`scripts/tests/test_wall_frames.py` (blends, bands, ×4 + noise, the named
failures, the pacer's rule and its dead band tied to `pacer_blend.rs`,
`analyse` with loops / losses / jumps / weights / unverified steps / judder,
every grid phase at 24 / 25 / 30 fps, end to end, the video read's args,
`main`'s exit codes), both numpy-only for CI's Eval Checks job,
whose ruff lists name both scripts.
