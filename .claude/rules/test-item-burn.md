---
paths:
  - "crates/sp-server/src/playback/program_burn*.rs"
  - "crates/sp-server/src/playback/program_item*.rs"
  - "crates/sp-server/src/playback/program_output_tests_burn.rs"
  - "crates/sp-server/src/playback/program_output_ndi*.rs"
  - "crates/sp-server/src/playback/handle_pipeline_event_tests_item.rs"
  - "crates/sp-server/src/test_item*.rs"
  - "crates/sp-server/src/api/test_item*.rs"
  - "crates/sp-server/src/api/program_tests_burn.rs"
  - "e2e/post-deploy-test-item.spec.ts"
  - "scripts/wall_clip.py"
  - "scripts/wall_frames.py"
  - "scripts/wall_analysis.py"
  - "scripts/tests/test_wall_clip.py"
  - "scripts/tests/test_wall_*.py"
  - "scripts/tests/wall_fakes.py"
  - "e2e/wall-frames-gate*.ts"
  - "e2e/post-deploy-wall-frames.spec.ts"
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
- #279 P1b: from the GPU readback `SP-program` is UYVY (`U Y0 V Y1` per
  pixel pair, rows of `width · 2` bytes), so the delay line
  (`program_output_ndi.rs`, `ProgramOutput::burn` with
  `PixelFormat::Uyvy`) burns with `burned_uyvy(video, width, height,
  frame, gen_ts_ns)`: the same `geometry` and modules, into a COPY
  (`paint_uyvy`: the luma bytes of the square by module, 16 / 235; the U
  and V of every pair the square covers 128 — at a 1 px module the square
  ends on an odd pixel, and that pair's chroma goes neutral too). The burn
  goes out with the boundary it shows: its item frame and wire stamp
  travel with the held pair, one slot late. `program_burn_tests_uyvy.rs`:
  the bytes pixel by pixel, the same luma as the NV12 burn, and an rqrr
  round trip reading every odd byte.
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

### `scripts/wall_frames.py` (the reader) + `scripts/wall_analysis.py`

`wall_frames.py` reads each picture (`read_frame`) and holds the capture
glue and the CLI; `wall_analysis.py` is the pure analysis of a run of
readings (`analyse`, the pacer's rule, the capture's timing). Run it from
the repo checkout's `scripts/` (on the box: the runner's checkout): it
imports `wall_clip`, `wall_analysis` and `av_sync_check` (with its
`av_sync_drift` / `av_sync_warp`).


- Per picture: 8 sampled columns per cell (`CELL_SAMPLES`, over the
  central half, at the cells' fractions of `--x-range`;
  `sample_columns` → `read_columns`), the rows whose references read black /
  white with contrast > 25 % of full scale and every bit between them, a
  run of ≥ 3 such rows = one strip; its core rows normalised by its own
  references. One grey cell (level 0.04–0.96) → `(i, i + 1, w)`, `w` the
  LATER frame's weight in Q8, rounded (the program's `FrameBlend` unit).
  Never guessed: `no_strip`, `multi` (≥ 2 grey cells), `non_adjacent`,
  `disagree` (the bands read positions more than `SAME_PICTURE` apart: a
  torn picture).
- **One picture = within `SAME_PICTURE`** (8 Q8 + the 0.04 level edge):
  the program's own 8-bit blend (`nv12_mix.rs`, `(f(256 − w) + tw + 128)
  >> 8` on 16 / 235) rounds a weight near 0 or 256 to either side of the
  edge, so one picture can read grey or plain. Two boundaries are ≥ 0.8
  frames apart.
- `analyse`, per step vs `fps / 30`: slack = 8 Q8 + the larger of the
  pacer's dead band (2 ms × fps: a boundary inside it shows the plain
  frame) and the level edge; ≥ 2 grid steps = lost boundaries, off the grid
  = cadence error, back = jump back; frames no boundary showed = lost
  frames. Weights vs the pacer's rule (`genlock.md`, "BLENDED") at the grid
  phase fitted per play (a loop or a jump back starts one; median, + half
  a Q8 step for the floor), against every place the reader may read the
  weight (`readable_positions`): 25 on 30 from 0 = 0, 213, 170, 128, 85,
  42 (read 0, 213, 169, 127, 85, 42 through the program's byte blend);
  24 = 0, 204, 153, 102, 51; 30 / 15 / 50 = none. The dead band is the
  pacer's `BLEND_DEAD_BAND_100NS`, read from `pacer_blend.rs` by a test.
  Review rounds 1–2: `test_every_grid_phase_…` sweeps 512 phases per frame
  through the program's byte blend at 24 / 25 fps (and 30): clean.
- `distinct_boundaries` (#279 P3a's A/B, `video-decode.md`): the distinct
  pictures the boundaries showed, numbered on across the clip's loops, a
  run within `SAME_PICTURE` of a picture shown before counted once
  (`wall_analysis.distinct_boundaries`). The program's boundaries over the
  capture (1 200 per 40 s on the grid; at a 24 / 25 fps loop seam the
  clamp shows ONE picture for two boundaries) minus it are the pictures
  the wall did not show or the capture did not see: a jump back re-shows a picture
  and adds nothing, and a capture that is dead or freezes to its end
  counts every picture it did not show. `boundaries` (one per run) counts
  every re-show again, and `lost_boundaries` sees only skips between two
  readings and counts the step out of a back as one too. A run is one
  picture within `SAME_PICTURE` of its FIRST reading, as `boundaries`
  groups it. It holds away from a loop seam: the new play's open hold
  (`loop_seams.hold_s`) repeats pictures, and a back across the seam
  reads as one more loop (the A/B's span holds no seam). Hand mutants of
  it: 27 of 33 killed. The six left are equivalent: the loop label counted
  down, scaled or subtracted (any label whose loops lie at least
  `n_frames` apart); `>=` for `>`, since positions are multiples of 1/256
  and `SAME_PICTURE` is 18.24/256; and `abs()` over a sorted difference.
  The edge is pinned: weight 238 against the plain next frame is one
  picture, 237 is two.
- **What a clean verdict means (the owner's ruling, #272 comment
  6170760353):** the wall's judder is intermittent, so a clean verdict
  covers only the span captured. A short take (a post-deploy gate of N
  seconds) is a regression check, never proof that the path is
  frame-exact; frame-ID takes meant to characterise the wall run LONG.
  `MIN_BOUNDARIES` is the floor under which a verdict cannot even be
  clean, not a length that proves anything.
- **The capture's timing is read as BRACKETS (review round 5, which
  replaced rounds 2–4's blind-span rules; round 6 gave them an error
  term):** each picture has a time and the span right before it the
  capture could not see (`blind`: dxcam, since the poll before it —
  `poll_blind`; an empty poll proves nothing new was presented; a
  recording, since the picture before it — `capture_blind`). A boundary
  was surely on screen at each of its pictures; it began after the last
  look that still showed the boundary before it and at or before its
  first picture, and ended after its own last look and at or before the
  next boundary's first picture (`held_range` → the refresh counts it can
  have had, `lo..hi`, `hi` open at a capture edge; the capture's last
  boundary is held to the capture's end only when no unreadable picture
  follows it, else to the look before them — locked, its own last picture,
  round 7). dxcam stamps a poll BEFORE its grab: a picture's content was on
  screen between that stamp and the end of its acquire (`Timing.late`,
  stamped right after dxcam's `AcquireNextFrame` through its duplicator's
  `update_frame`, else after the whole grab), so a bound "at or before the
  picture" takes the window's end (`Timing.by`) and a hidden refresh the
  span to it (round 7: a grab preempted 5–60 ms between stamp and acquire
  read a clean wall as judder). Every span between two instants is taken as short and as
  long as the instants' error and the rate's tolerance allow
  (`Timing.fewest` / `most`): each instant may be off by `error` — dxcam
  `POLL_TIME_ERROR` 0.5 ms, a recording its pts' own quantization
  (`pts_jitter`: how far its regular intervals stray from their mean — a
  1/10000 s timebase moves a 30 fps frame between 33.3 and 33.4 ms; at
  least `PTS_TIME_ERROR`) — and the wall's period is known to
  `RATE_TOLERANCE` (0.15 %: a nominal 60 for a 60/1.001 wall). Round 6
  measured why: with exact arithmetic, quantized pts and a 0.1 % rate
  mismatch read clean walls as judder. A `--locked` recording (each
  picture one wall refresh: the wall's own signal at its rate) changes
  content ON a refresh: whole periods, closed bounds. It is checked
  (`check_locked`, else exit 5): every interval a whole number of periods
  within a quarter, the rate to `RATE_TOLERANCE` (round 7: a 120 fps camera
  given `--locked` read 1699 uneven boundaries). A recorder clocked apart
  from the signal at the same nominal rate passes that check and drifts
  against the wall, read as judder: `--locked` needs a recorder clocked by
  the signal itself (a capture card on the wall's output), never an OBS or
  software recording at the nominal rate.
- **What the capture did not see excuses at most one boundary per
  refresh a boundary takes, no more.** Unseen between two boundaries
  (`unseen_refreshes`) = the unreadable pictures between them plus the
  refreshes a blind span before each picture can have hidden whole
  (`hidden_refreshes`: `ceil(longest / period) − 1`, the span as long and
  the period as short as error and tolerance allow, so an excuse errs to
  `unverified`, never to lost — round 5: a 2.02-period dxcam gap holds 3
  presents; round 6: subtracting the pts' jitter turned a slow camera's own
  skips into losses; locked: the frames missing from the gap). A step's
  excused boundaries = unseen ÷ the fewest refreshes a boundary is shown
  (60 Hz on 30: 2; 50 Hz: 1 — a boundary shown fewer is judder anyway, and
  reads lost when a gap hid it); they and the frames they could show (up
  to the last one's position, counted from the blend it follows, in the
  clip's own step) are `unverified`; the rest is lost; a cadence error is
  never excused.
- **Judder**: every boundary must be on screen for the refreshes the
  WALL's rate gives on the grid (`refreshes`: 60 Hz on 30 = 2; a rate the
  grid does not divide allows the two around it). `--refresh-hz` = the
  wall's own rate is REQUIRED (round 5: a rate read from a 30 fps camera
  called a juddering 60 Hz wall clean; dxcam's 1 ms polls read 58.8 Hz for
  60); a rate below the grid is refused, exit 5. A boundary is judged when
  its bracket proves it: every possible count expected (ok), or none
  (`uneven`, in the verdict — a count already too high is judder whatever
  the rest, so a stall is judder wherever it lies: inside a partly read
  boundary, at the capture's first boundary, or its last one held to the
  capture's end). An inner boundary whose bracket stays open between an
  expected and an unexpected count — the capture too coarse for the wall,
  an unreadable picture or a blind span beside it — is `unresolved`, and a
  clean verdict needs none: what the capture did not resolve could hide
  judder (round 5: a 60 fps camera on a 75 Hz wall left a 4-refresh
  boundary open; round 6: a stall of the box hid each judder from dxcam
  too). A bracket no count fits (`lo > hi`) is the capture's fault, not
  the wall's: `inconsistent`, incomplete, never judder (round 7). A
  camera's exposure lasts: one taken across a boundary change reads a
  position between the two, so in a recording (not dxcam, not `--locked`)
  a lone decoded picture between its neighbours and off the grid is
  `mixed`, unreadable (`mixed_exposures`; round 7: a 120 fps camera read
  hundreds of cadence errors on a clean wall). In practice: dxcam polling
  on time or a signal-clocked `--locked` recording prove every count; a
  camera at ≥ 2× the wall's rate with a short shutter mostly does; a camera
  at the wall's rate unlocked, or slower, mostly cannot (incomplete, never
  judder). **What dxcam sees is DWM's composition, not the wall's
  scanout**: on SNV DWM composes at 60.0000 Hz while the wall's own vblank
  is 59.979 Hz (`gpu-max.md`), so dxcam on SNV takes `--refresh-hz 60` and
  cannot see that wall's beat against the grid; a camera on the wall can
  (one boundary a refresh short or long every 1 / |refresh − 2 × 30| s, the
  reader reports it as judder because the eye sees it). Known limit: at a
  rate the grid does not divide (50 / 75 / 144 Hz) the counts accept both
  neighbours, so a one-refresh slip that keeps them in {2, 3} is not seen
  (SNV and PP run 60 Hz, where the count is exact; checking each
  boundary's start against the fitted grid would close it). Fewer new
  pictures than half the grid's slots over the capture's span = `frozen` —
  for dxcam only, whose pictures are the wall's presents (a recording keeps
  its frames whatever the wall does; a slow one is merely coarse).
- The clip LOOPS (`step_count`, `seam_steps`): the song's last boundary
  shows the clip's last frame PLAIN wherever it lies in it (no next frame
  to blend: the pacer clamps), so a step into it may be any whole number of
  grid steps that lands in that frame, and the loop's step counts the
  song's boundaries left after the one before it (from that boundary's
  LAST reading: a weight near 256 and the clamped frame read as one
  picture) plus the new play's from frame 0. A wrap is a step back to the start no further from the end than
  `WRAP_FRAMES` plus what the capture could have missed. Losses and cadence
  at the seam are judged like any step's (round 6: the old seam rules
  excused one lost boundary, and read a capture blind at the 24 / 25 fps
  clamp as a cadence error). The seam is spared only its timing — the
  song's last boundary and the new play's first may be held up to
  `SEAM_HOLD_S` longer (1 s: the new play's open; round 5: a 5 s freeze
  there read clean), never shown shorter (round 6: a 1 + 3 pair there read
  clean; the new play's second boundary is the cadence again). Known
  limit (round 7, not closed): at 24 / 25 fps, where two boundaries fall
  in the clamped last frame, the song's last run is judged against ONE
  boundary's refreshes, so a wall skipping one of the two (the new play one
  boundary early) is not seen. `loop_seams.hold_s` is the longest hold, the box's own measure
  of the loop's restart — and its weights (`loop_seams.weight_mismatches`:
  the clamped frame has none). A jump to 0 from mid-clip is a jump back.
- **Verdict**: `defects` (any loss, jump back, cadence error, weight
  mismatch, judder, a frozen wall, or no boundary) → exit 1; else
  `incomplete` — `clean` needs positive evidence: unverified steps, under
  95 % of the pictures decoded (`MIN_COVERAGE`), no refresh rate, any
  `unresolved` or `inconsistent` boundary, or fewer boundaries judged for judder than
  `--min-boundaries` (default `MIN_BOUNDARIES` 300 = 10 s on the grid:
  round 4, a 0.3 s capture of a wall losing a boundary every second read
  clean; every inner boundary is judged or unresolved, so only the
  capture's first and last boundary — judged only when held too long — and
  the seam's stay out) → exit 3, never clean;
  else `clean` → exit 0; nothing decoded → exit 4; the capture or its
  reading failed (ffmpeg, dxcam, a rate below the grid, a dxcam present
  rate that is no whole number of `--refresh-hz` refreshes, or off it by
  more than `RATE_TOLERANCE` over the whole capture — `check_present_rate`,
  on the intervals between successive pictures timed within a tenth of a
  refresh, `TIMED_SHARE`, poll gap plus acquire window — or fewer than 10
  such intervals, so the rate cannot be checked (an unchecked wrong rate
  reads as judder or losses, round 7: grabs over ~15 ms at 60 Hz, or any
  wall over 100 Hz with 1 ms polls), a `--locked` recording that is no
  recording of the wall's own signal) → exit 5 with the error on stderr; a usage error → 2
  (argparse). A broken capture never reads as a broken wall.
- Capture glue: `--video FILE --refresh-hz R [--locked] [--width W]`
  (`read_video`: `av_sync_check.gray_video_args` — the A/V gate's own
  `decode_video` command line, `-fps_mode passthrough` so every stored
  frame is read as stored, each frame's pts from `showinfo`
  (`av_sync_check.showinfo_pts`) — and the size through
  `av_sync_check.probe_video_size`: the box has no ffprobe; streamed a
  picture at a time, never the whole 4K recording; ffmpeg killed and
  reaped when a read fails) or `--dxcam OUTPUT --refresh-hz R
  [--region L,T,R,B] --seconds S` (`capture_dxcam`, Windows, the box's
  `ops\probe-venv`; a picture's time is its poll's start, when dxcam
  acquires the newest frame; `output_color="BGRA"`: no colour conversion,
  so dxcam 0.3+ never loads OpenCV — dxcam 0.0.5 imports it at module load
  whatever the colour, so the venv needs dxcam ≥ 0.3 or `opencv-python`:
  the main session checks the box): every poll timed, only each picture's
  sampled columns kept as luma (~170 KB at 1080 rows, ~345 KB at 2160) and
  decoded after the capture, an empty poll sleeps 1 ms (Python 3.11+ sleeps
  on a high-resolution timer there). `--fps` takes 24 / 25 / 30.
- Measured on dev1 (#272 comment 6164962209): every frame of the three
  clips reads back (also through `read_video`'s pts path: 1440 / 1500 /
  1800, clean); real decoded frames blended 25 on 30, cut to an SNV
  quarter band, ×4 and noised read 0, 214, 170, 127, 85, 41 (±1 Q8).
- The clip's song / artist go to the wall's title clip like any song's
  (nothing treats the test playlist apart there): a strip the title covers
  fails its reference check and the consensus takes the other bands; a
  picture with every visible strip covered is `no_strip`, its steps
  `unverified` (one boundary per picture), never clean. Whether a test
  item should write the wall title at all is open (a follow-up candidate
  on #272).

## The post-deploy wall frame gate (#272 lane 2, `e2e/post-deploy-wall-frames.spec.ts`)

The owner (16.10.2026, during an event with the wall juddering): no CI run
may pass while the LED wall judders. Every other gate reads SongPlayer's
own counters. The MAX gate checks the send and compose phases, and
`post-deploy-wall.spec.ts` checks Arena's display setup. Nothing read the
picture, so deploys went green over a juddering wall. Design: #272 comment
6220915104.

- **What it does (SNV only).** `post-deploy-pp.config.ts` lists its specs
  by name and does not take this one.
  - It reads `GET /api/v1/test-item`; the clips must be imported.
  - For `wall-30`, then `wall-25`: `/start {"clip"}`, then a program cut
    to the test playlist. It waits until `on_air_item.youtube_id` is
    `wallclip-30` / `wallclip-25` with `position_ms` ≥ 3000 (≤ 25 s).
  - Then 40 s of `scripts/wall_frames.py --fps <fps> --dxcam
    $SP_WALL_DXCAM_OUTPUT --refresh-hz 60`.
  - It runs on the e2e venv (`SP_AVSYNC_PYTHON`, which ci.yml also gives
    `dxcam==0.3.0` + `comtypes==1.4.17` with numpy kept at its pin). The
    runner runs in the desktop session 1, so desktop duplication works.
  - `afterAll` cuts back to the source it found, then `/start` + `/stop`
    with no body.
- **The verdict** is `e2e/wall-frames-gate.ts::wallFramesFailures`, unit
  tests in the mock suite (`wall-frames-gate.spec.ts`).
  - Only exit 0 with verdict `clean` passes.
  - `defects` names every counter: judder (`refreshes.uneven` of `judged`),
    jumps back, lost frames and boundaries, cadence, weights, frozen.
  - `incomplete` (exit 3) fails as INCOMPLETE, never as judder or clean.
  - Exit 4 = the wall does not show SongPlayer's program (Arena's clip on
    another source, MAX off with the clip on MAX, the clip not on air).
  - Exit 5 = the capture failed.
- **It is RED while the wall judders.** That is the point; never loosen it
  to make a deploy green.
- **What it does not see.** dxcam reads DWM's composition, stage 1, not the
  physical wall after Surround and the LED processor. A clean take covers
  only its 40 s, and the judder comes in waves.
- dxcam's output on SNV: `Output[1]` 7680×1080 (`Output[0]` is the 4K
  primary). Read it with `python -c "import dxcam;
  print(dxcam.output_info())"`.
- **It grabs one strip band, never the whole output** (`SP_WALL_REGION`,
  ci.yml: `0,160,7680,312`). A full 7680×1080 grab is too slow to time
  each refresh: the reader exits 5 ("only 2 intervals timed within
  1.67 ms"). At Arena's mapping on 16.10.2026 the wall clip's strips land
  on rows 170–308, 315–332 (a cut band) and 715–880 of that output; the
  band 160–312 polls at 16.6 ms with no blind picture. Find the rows
  again after any change to Arena's wall mapping: one `camera.grab()` of
  the output while a wall clip plays, then `wall_frames.strip_rows` on
  `cell_profile(sample_columns(picture, (0, 1)))`.
- **A manual measurement on the box checks the rig lease FIRST, and
  before each round** (17.10.2026: an overnight layer A/B cut SP-program
  and bypassed Arena layers for 6 min inside camera-box's 8 h av-soak,
  whose lease the deploy honoured). It does not apply to a read-only
  capture. Any script that cuts the program, switches an Arena source or
  bypasses a layer runs `diag\wall-bridge\lease_free.ps1` first. That is
  `scripts/rig_lease_gate.py` with `--max-wait-seconds 0`: exit 0 = free,
  3 = another repo holds it. The script does not start while the lease
  is held, and stops and restores when another repo takes it mid-run
  (`layer_ab.ps1`, `test0_rot.ps1`). Tell the holder's session (ListAgents
  → SendMessage) what overlapped, if anything did.
- **The first manual take (16.10.2026 19:00Z, `wall-30` through MAX on
  L4 / L5):** verdict `defects`: 521 jumps back and 1040 of 1708 boundaries
  held the wrong number of refreshes in 40 s. So the gate was red on the
  day it landed, as the owner saw the wall.

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
`scripts/tests/test_wall_frames.py` (reading a picture: blends, bands, ×4 +
noise, the band median, the named failures, the pacer's rule and its dead
band tied to `pacer_blend.rs`), `test_wall_frames_analyse.py` (one sample
per refresh: every grid phase through the program's byte blend at 24 / 25
/ 30 fps, loops / losses / jumps / weights / unverified steps at 60 and
50 Hz / judder / the song-end clamp at every phase, a loss at the seam, a hold over 1 s there / the
300-boundary minimum / coverage / the distinct pictures: a re-show once, one
picture read two ways, the `SAME_PICTURE` edge, a run grouped by its first
reading, a frozen tail, a dead capture, across loops (from the wrap's own
step), a skipped blended boundary), `test_wall_timing.py` (the brackets: a locked recording and dxcam on time exact, 120 / 240 fps cameras proving counts and finding judder, cameras too coarse — 60 / 59.94 / 50 / 30 fps on 60 Hz, 60 fps on 50 Hz, 120 fps on 144 Hz, 60 fps on 75 Hz — never calling judder and never calling it clean, `unresolved`, a stall at the start, middle and end, blind refreshes hiding one boundary per refresh at 60 and 50 Hz, `hidden_refreshes`, `unseen_refreshes`, `held_range`, a frozen wall (dxcam only), the judged share, the required rate; round 6: unreadable pictures after the last boundary, cameras' pts on 1/90000, 1/10000, 1/1000 and 1/600 timebases, a rate within its tolerance either way, judder a box stall hid from dxcam; round 7: a grab preempted 2–40 ms before its acquire, an impossible bracket, a locked capture ending on unreadable pictures, a camera exposure mixing two boundaries) and `test_wall_capture.py` (round 7: `--locked` checked, a rate that cannot be checked)
(`av_sync_check.gray_video_args`, `showinfo_pts`, `read_video` against a
stand-in ffmpeg — pts, a half frame, a failed exit, the kill when a read
fails —, `poll_blind`, `check_present_rate`, `main`'s exit codes for a
video and for a simulated dxcam capture — a late poll, 20 and 10 ms grabs,
a 12 ms lag, a wall that stopped, a wrong or missing `--refresh-hz`, a bad
`--region`, `--locked` with dxcam), their shared fakes in
`scripts/tests/wall_fakes.py` (the clip's frames, the program's byte blend,
the pacer's readings, the wall's refreshes as a camera samples them, a
simulated dxcam capture), all numpy-only for CI's Eval Checks job, whose
ruff lists name the three scripts. Hand mutants of `wall_frames.py` / `wall_analysis.py` (every
threshold, guard, bound and exit code; a scratch harness on dev1, not CI)
were run after each review round and the tests extended until none
survived; an equivalent one is removed from the code instead.
