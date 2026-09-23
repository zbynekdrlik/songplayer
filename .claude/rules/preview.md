---
paths:
  - "crates/sp-server/src/playback/preview.rs"
  - "crates/sp-server/src/playback/preview_stream.rs"
  - "crates/sp-server/src/playback/preview_encoder.rs"
  - "crates/sp-server/src/playback/fmp4_relay.rs"
  - "crates/sp-server/src/playback/pipeline.rs"
  - "crates/sp-server/src/playback/pipeline_paced.rs"
  - "crates/sp-server/src/api/preview.rs"
  - "sp-ui/src/components/playlist_card.rs"
  - "sp-ui/src/components/preview_video.rs"
  - "sp-ui/preview_player.js"
  - "e2e/post-deploy-preview.spec.ts"
  - "e2e/post-deploy-dabing.spec.ts"
  - "e2e/audio-helpers.mjs"
  - "e2e/post-deploy-owner-path.spec.ts"
---

# Dashboard preview — the JPEG thumbnail (#15) AND the live A/V stream (#178)

The dashboard playlist card has TWO preview surfaces sampled from the
already-decoded output. The governing constraint (owner rescope 2026-09-13,
genlock #146–#151) is unchanged: a preview must **never** touch the NDI submit /
pacing / genlock path, add latency, or drop wall fps.

- **#15 JPEG thumbnail** — an opportunistic ≤ 5 fps still, no audio. Kept as the
  fallback thumbnail; served by `GET /api/v1/playback/{id}/preview.jpg`.
- **#178 live A/V stream** — a usable remote monitor (25 fps target, ≥ 15 floor,
  audible WALL MIX incl. karaoke/dub) over `GET /api/v1/playback/{id}/preview.ws`
  (fragmented MP4 over WebSocket → MSE `<video>`). On-demand only: zero cost when
  nobody is watching.

## The offer seam (both taps, ONE call)

`preview.rs` owns the JPEG `PreviewTap` + the `PreviewRegistry`; the registry
ALSO owns the #178 `StreamTap` per playlist (`register_taps` / `stream`) so no
second registry is threaded through the near-1000-line `mod.rs`/`lib.rs`. The two
taps are bundled into `preview_stream::DecodeTaps` and passed as the single
tap-parameter slot through `PlaybackPipeline::spawn` → `run_loop` →
`run_loop_windows` → the two decode fns.

Each decode loop makes exactly ONE `taps.offer_frame(&video_frame, &audio_frames)`
call, BEFORE the NDI submit / `#192` audio-emitter push consume the frame:

- `pipeline::decode_and_send` (SDK-clocked path) — offer, then
  `push_or_collect_audio`, then `submit_nv12`.
- `pipeline_paced::run_decode_producer` (paced path) — offer on the PRODUCER
  thread (off the emit/submit path entirely), before `to_paced_frame`.

`offer_frame` fans out to: the JPEG tap (`preview.try_offer`), the stream video
tap (`stream.try_offer_video` → NV12→NV12 letterbox into a fixed 640×360 canvas),
and the stream audio tap (`stream.try_offer_audio` per post-mix `DecodedAudioFrame`).
Tapping BOTH audio and video at this ONE decode seam keeps them offered together,
so ffmpeg's `-use_wallclock_as_timestamps` keeps A/V in sync, and it stays OFF the
TIME_CRITICAL `#192` emit thread (which must never wait). (The main design comment
named `audio_emitter.rs::emit_one_block` as the audio seam; the decode seam is the
equivalent, simpler, single-seam realization — see #178.)

## Iron rules (BOTH taps)

1. **An offer must never block the decode / producer / emit thread.** With NO
   viewer it is a single relaxed atomic load + return — no lock, no allocation,
   no channel touch (`StreamShared::has_viewer` / `PreviewShared::is_subscribed`).
2. **No encode on the decode thread.** The JPEG tap does a cheap NN downscale
   (JPEG encode is on its worker thread); the stream tap does a cheap NN NV12
   letterbox into a RECYCLED buffer (the H.264 encode is in the ffmpeg CHILD).
   Never move an encoder onto the decode thread.
3. **`pacer.rs` / `submitter.rs` / `pipeline_audio.rs` / FLAC / genlock stay
   byte-for-byte.** The preview adds exactly ONE `offer_frame` call per decode
   loop. `pacer_tests.rs`'s byte-exact legacy call-site guard pins the SDK-path
   call site; update it (with a justification) if the call site changes.
4. **A full channel DROPS the frame, never blocks.** The stream video/audio
   bounded channels drop-on-full (the child paces CFR); a lagging WS viewer is
   dropped by the broadcast relay and resyncs on the next keyframe fragment.
5. **Viewer-scoped, on-demand.** The stream encoder child spawns on the FIRST WS
   viewer and is killed `VIEWER_TTL` (5 s) after the last one leaves. The JPEG
   tap uses its own poll TTL. No viewer = no cost.

## Encoder child lifecycle (`preview_encoder.rs`)

- ONE bundled `ffmpeg` child per WATCHED pipeline. Reads raw NV12 (fixed 640×360)
  + interleaved-f32 48 kHz stereo audio over TWO loopback TCP listeners (we
  listen; ffmpeg connects back as client), encodes H.264 + AAC, muxes fragmented
  MP4 to stdout.
- Encoder ladder probed ONCE per process from `ffmpeg -encoders`:
  `h264_nvenc → h264_qsv → h264_amf → libx264` (libx264 adds
  `-preset ultrafast -tune zerolatency`). The chosen encoder is exposed on
  `/api/v1/status.preview_encoder`. Runtime fallback: a hardware child that dies
  before producing an init segment is retried once with `libx264`.
- Child is `BELOW_NORMAL_PRIORITY_CLASS | CREATE_NO_WINDOW` on Windows (never
  steal CPU from the NDI/emit threads; no console popup). NOT a heavy-slot member.
- **ffmpeg stderr is LOGGED, never discarded (#178 round 3).** `run_child` uses
  `Stdio::piped()` + a reader thread that logs each line at WARN with the stream
  label, rate-limited to the first 20 lines/child + a final "N more suppressed";
  args keep `-loglevel error`, so anything printed is a real error. Every box
  failure in this feature was a single stderr line thrown away by the old
  `Stdio::null()` — never re-add the null.
- The pure parts — the arg builder, the ladder selection, the `-encoders` parse —
  are Linux unit-tested. The child/TCP/feeder/monitor lifecycle is
  `mutants::skip` glue (box-verified) but compiles cross-platform.

### Three earlier box findings — do NOT regress them (#178)

1. **Sequential TCP inputs → feed video ON CONNECT.** ffmpeg (as TCP client)
   opens its inputs sequentially: it connects input 0 (rawvideo) and
   `find_stream_info` reads frames from it BEFORE it opens input 1 (audio). So
   the video feeder must start the moment the video socket connects, THEN accept
   audio — waiting for both accepts first deadlocks (ffmpeg blocks probing an
   empty video input, the audio accept times out, no init segment).
2. **nvenc needs Nvidia driver 570+ → libx264 pinned.** The box's bundled ffmpeg
   `h264_nvenc` needs a newer driver than win-resolume has (`Required API 13.0,
   Found 12.2`); it selects at probe time but fails to OPEN at encode time,
   producing 0 stdout bytes. Fallback keys on a PER-CHILD "produced any stdout"
   flag (the relay's init is cached across children, so it cannot be the signal)
   and pins `libx264` process-wide once a fallback happens. The real fix is
   updating the box driver to 570+ (owner/host call).
3. **`-flush_packets 1`** — without it ffmpeg buffers the moof/mdat fragments on
   the non-seekable pipe (the init flushes at header write, fragments don't).

### A/V timestamps + alignment (#178 round 3) — PCM is NEVER wall-clock stamped

**ONLY the video input carries `-use_wallclock_as_timestamps 1`. The raw f32le
PCM input keeps its SAMPLE-COUNT timestamps.** This is the round-3 box root
cause: fed the production args, the box's ffmpeg build (`N-123867`, 2026-04)
muxes **ZERO audio packets** when the bursty PCM input is wall-clock stamped
(every `moof` carries ONE `traf` — video only — though the `moov` declares two
tracks). MSE's `buffered` is the INTERSECTION of the tracks, so an empty audio
track = nothing playable, forever (`readyState 1`, empty `buffered` — the whole
round-1/2 box symptom). Dropping the flag on the PCM input → 376 audio packets in
8 s. So `build_ffmpeg_args` takes NO `lead_ms` and emits NO `-itsoffset` (on the
box the audio `start_time` stayed 0.000 regardless of `-itsoffset`, so it was
never a dependable lever). The `only_the_video_input_is_wall_clock_stamped` +
exact-vector tests pin this — never add wall-clock stamps to the PCM input.

**A/V is aligned on OUR side by a silence preroll.** The audio + video are tapped
together at the ONE decode seam, but the merged #192 emitter opens the
SDK-clocked decoder with a 100 ms audio read-ahead
(`decoder_tolerance_ms(true) == 140` vs `DEFAULT_TOLERANCE_MS == 40`), so at the
seam the `audio_frames` LEAD `video_frame` by `lead_ms` on the SDK-clocked path
(100 SDK / 0 paced; `lead_ms_for` / `StreamShared::lead_ms()`, threaded
`ensure_pipeline → register_taps → StreamTap::new`). Because the video feeder
starts on-connect BEFORE the audio input connects, the audio feeder measures how
far the video wall-clock timeline is already ahead and PREPENDS silence to match:
`audio_preroll_samples(connect_gap_ms, lead_ms) = (min(gap,5000)+lead_ms)*48*2`
interleaved-stereo f32 samples (48 kHz stereo; gap capped at 5 s; exact-value
unit-tested, no equivalent mutants). The feeders also DRAIN any stale queued
frames/blocks on start so a previous viewer's backlog never front-runs the live
edge; the video feeder stamps the wall-time of its first write for the gap.

### Fixed-stereo audio input (`to_stereo`, #178 Round 2)

The child's audio input is a FIXED 48 kHz STEREO f32 stream, so `offer_audio`
routes every post-mix block through the pure `to_stereo(samples, channels)`:
mono → each sample duplicated into L,R (a mono block forwarded verbatim would play
at DOUBLE speed); stereo → verbatim; any other channel count → dropped. Upmix uses
`flat_map` with no capacity hint (a hint multiplier would be an equivalent mutant).

## fMP4 relay late-join contract (`fmp4_relay.rs`)

`BoxSplitter` (pure, Linux-tested: split reads across buffer boundaries, 64-bit
`largesize`, poison on corrupt size) turns the child's stdout into ONE
`RelayChunk::Init` (`ftyp`+`moov`) then one `RelayChunk::Fragment` per
`[styp?][sidx?]moof mdat`. `FragmentRelay` caches the init segment and broadcasts
fragments; a late joiner receives the cached init THEN the next fragment (every
fragment is keyframe-aligned under `+frag_keyframe`), and a viewer that falls
behind the broadcast backlog is dropped (never blocks the reader).

## Release code review hardening (0.59.0, #178 items 11–17)

- **`BoxSplitter` poisons above 16 MiB (item 14).** `parse_box_header` and the
  init/fragment accumulators reject a declared size / accumulator over
  `MAX_BOX_BYTES = 16 MiB` (32-bit AND `largesize`) via the shared
  `exceeds_box_cap` — a corrupt/hostile stream can no longer make the splitter
  allocate gigabytes. `is_poisoned()` exposes the state for tests. Exactly 16 MiB
  is accepted; 16 MiB + 1 poisons.
- **`FragmentRelay::reset()` and `close()` (items 17, 12).** `tx` is now a
  `Mutex<broadcast::Sender>`. `reset()` clears the cached init and is called at
  each child's START and STOP (so a late joiner after a reset waits for the NEW
  init). `close()` clears the init AND drops the sender (replacing it with a
  fresh channel), so every connected viewer's `recv()` returns `Closed` and its
  WS handler closes the socket — the supervisor calls it when it gives up
  restarting a dying child.
- **Encoder-child restart budget + drop guards (`preview_encoder.rs`, items 11,
  12).** The feeder/reader thread spawns return `io::Result` (no more `.expect`
  panic); a `ChildGuard` kills+waits the ffmpeg child on every early return /
  panic (std `Child::drop` does NOT), and an `EncoderReleaseGuard` releases the
  `encoder_running` flag on every supervisor-thread exit incl. panic (so it can
  never stick claimed and block future viewers). `supervise` respawns a child
  that died with viewers connected, at most 3 restarts per rolling 60 s (pure
  `RestartBudget`), else `relay.close()`.
- **Preview audio continuity (item 15) — SUPERSEDED by #184 round G2.** The
  add-only `gap_fill_samples` is DELETED: it only ever prepended silence, so a
  late burst accumulated lag. See "#184 round G2" below for the two-way aligner.
- **WS keepalive + idle deadline (`api/preview.rs`, item 16).** The WS `select!`
  pings every 5 s and drops the viewer after 15 s of client silence (pure
  `is_idle`) — a half-open client can no longer keep the encoder child alive.
  Browsers auto-answer Ping with Pong, so the JS side needs no change.
- **JS append-queue cap (`sp-ui/preview_player.js`, item 13).** The MSE shim caps
  the append queue at 40 chunks (drops oldest, then clears the SourceBuffer range
  and resyncs at the next keyframe fragment) and re-pumps/maintains on a 250 ms
  interval, cleared in `destroy()`.

## MSE shim contract (sp-ui, #178 Round 2 + round-3 click-to-start)

`preview_player.js` = a MediaSource + SourceBuffer shim,
`video/mp4; codecs="avc1.42E01E, mp4a.40.2"`: append the init segment, then queue
fragments; live-edge chase when `buffered.end − currentTime > 2 s`; snap into the
buffered range when `currentTime < buffered.start(0)` (a live fragment can begin
at a non-zero media time — round 3); evict > 30 s behind. `PreviewPlayer(video,
path, startMuted)`. Wrapped by `components/preview_video.rs`.

**Round 3 — CLICK-to-start (zero cost with nobody watching).** The app's own
hidden Tauri webview is a PERMANENT viewer, so an auto-mounted preview ran an
encoder child 24/7 whenever anything played. So `playlist_card.rs` no longer
auto-mounts the `<video>` while Playing: a Playing card shows a `preview-start`
control ("▶ Živý náhľad"); clicking it (a real user gesture) mounts the
`<video data-testid="preview-video">` + player and starts it UNMUTED
(`startMuted=false`; the shim falls back to muted and keeps the `preview-unmute`
button if the browser still rejects the unmuted `play()`). A `preview-stop`
control unmounts it, and leaving the Playing state resets the card's `preview_on`
signal — both fire `on_cleanup` → `destroy()` → WS close → the encoder child
dies. Fullscreen button kept. The JPEG `<img>` path stays REMOVED from the card
(no dual path).

## chrome-channel E2E note (#178 Round 2)

H.264/AAC are ABSENT from Playwright's bundled Chromium, so the preview E2E runs
under a SEPARATE browser-channel project (bundled Chromium runs everything else):

- **Mock (`e2e/preview.spec.ts`, `playwright.config.ts` `chrome` project)** —
  `channel: 'chrome'` (GitHub ubuntu runners ship google-chrome; the CI
  `frontend-e2e` job also runs `npx playwright install chrome`). The mock
  (`e2e/mock-api.mjs`) serves a canned tiny fMP4 (`e2e/fixtures/preview-fixture.mp4`,
  init + keyframe fragments, WITH an AAC track starting at pts 0.000) over the
  `preview.ws` WebSocket (a `noServer` upgrade router dispatches `/api/v1/ws` vs
  `…/preview.ws`). The spec CLICKS `preview-start` first (round 3), then asserts
  the card `<video>` reaches `readyState ≥ 3`, `currentTime` advances, it ends up
  UNMUTED (directly or via the unmute button), the stop control removes the
  `<video>`, a Playing card mounts NO `<video>` before the click, an idle card has
  no start control, and zero console errors.
  **A project-level `testMatch`/`testIgnore` REPLACES the global one** — the
  `chromium` project must repeat the `post-deploy*` ignore alongside `preview`.
- **Post-deploy (`e2e/post-deploy-preview.spec.ts`, `post-deploy.config.ts` `edge`
  project)** — `channel: 'msedge'` (Edge is always present on the Windows box and
  carries the codecs; the box E2E job runs `npx playwright install msedge`). It
  does NOT drive OBS at all (safest for the shared live wall): it reads
  `/api/v1/status.active_playlist_ids` to confirm a playlist is on program, then
  on the auto-selected card CLICKS `preview-start`, asserts the real box-encoded
  `<video>` reaches `readyState ≥ 3`, `currentTime` advances, AND
  `webkitAudioDecodedByteCount` GROWS (the round-3 audio-track regression guard),
  then clicks stop so no encoder child is left running.

## Diagnosing a stuck preview (`readyState 1`, empty `buffered`) on the box

The init (`ftyp`+`moov`) reaching the browser while NO media fragment plays is
almost always an EMPTY track in the muxed fMP4 (MSE `buffered` is the track
intersection). Capture + inspect OUTSIDE SongPlayer:
- Capture the WS to a file (a tiny node/wscat client, or tee the child's stdout)
  and `ffprobe -show_packets <file>` — count audio vs video packets. Zero of
  either track = the empty-track bug.
- `ffprobe -show_packets -select_streams a` for the audio packet count; per-`moof`
  `traf` count must be **2** (one per track) — a single `traf` per `moof` while the
  `moov` declares two tracks is the audio-less stream.
- Reproduce with the BOX'S OWN ffmpeg binary fed the exact production args over
  TCP — the box build can differ from dev1's (the round-3 root cause: the box
  build muxed 0 audio packets only when the PCM input was `-use_wallclock_as_timestamps`).

## #184 round F — remote-uplink lag budget + the lag beacon

The owner watches the preview over the internet, and it sat ~30 s behind the
wall (every ratio/fader/seek looked 30 s late because the PICTURE was late, not
the control). Two independent defects + a missing diagnostic, all in this file's
area. Do NOT regress them:

- **`RELAY_CAPACITY = 4` fragments (2 s), NOT 64 (32 s)** (`preview_stream.rs`).
  64 × `-frag_duration 500000` = a 32-s per-viewer backlog: a viewer on a link
  slower than the stream filled it and sat a full 32 s behind FOREVER (the
  broadcast relay drops the OLDEST fragments, so the buffered end WAS the stale
  data — the shim's live-edge chase can't help, `behind` stays ~0). At 4 the slow
  viewer drops fragments and resyncs on the next keyframe fragment — choppy but
  LIVE. Never raise it back without a lag-budget reason.
- **Encoder fits ~1 Mb/s** (`preview_encoder.rs::build_ffmpeg_args`): `-b:v 500k
  -maxrate 500k -bufsize 500k` + `-b:a 64k` ≈ 0.6 Mb/s (was `1200k` + `128k` ≈
  1.35 Mb/s, which did not fit and made the remote back-pressure). 640×360 @ 25
  fps stays — it is a monitor, not a viewing screen. `-maxrate/-bufsize = the
  bitrate` bounds the keyframe overshoot so a GOP can't burst the link.
- **The "exactly two fragments per second" invariant is load-bearing.** Under
  `-r 25 -g 25 -frag_duration 500000` every GOP is 1 s and every media fragment
  is 0.5 s, so `FragmentRelay::produced_ms()` = `(fragments since Init) × 500`
  is the media time produced. If you change `-r` / `-g` / `-frag_duration`, the
  `FRAGMENT_MS = 500` constant in `fmp4_relay.rs` AND the beacon math break —
  change them together. The counter resets on `Init` (a new child restarts the
  media timeline), increments per `Fragment`.
- **The lag beacon** (`api/preview.rs::handle_preview_ws`): a 1 Hz `select!` arm
  (next to the ping arm — both inside the `mutants::skip`-excluded fn) sends the
  text frame `{"produced_ms":N}` (`beacon_frame`, a pure tested helper). The MSE
  shim (`preview_player.js`) branches on `typeof ev.data`: a STRING is the beacon
  (`JSON.parse`, non-JSON ignored — never a console error), BINARY is a fragment.
  It computes `lag_s = produced_ms/1000 − buffered_end` each pump tick and reports
  it through the `onLag` callback → `preview_video.rs` (`on_lag` prop, the wasm
  `Closure` held beside the player) → `player.rs` renders
  `<span data-testid="preview-lag">náhľad mešká N s</span>` ONLY at lag ≥ 3 s. The
  ≥ 3 s threshold + whole-second rounding is the pure `sp_core::preview_lag::
  preview_lag_display` (WASM-safe → workspace-tested + mutation-gated; sp-ui has
  no unit-test job — never inline a threshold/rounding constant in sp-ui).
- **Mock lag knob** (`e2e/mock-api.mjs` `previewWss`): sends the same beacon
  (`fragments sent × 500`) and honours `?lag_ms=<N>` on the upgrade url. The
  production `preview.ws` url stays unchanged — the shim forwards `lag_ms` ONLY
  when it is present in the PAGE's own `location.search`, and the E2E navigates
  with `/?lag_ms=30000`. The throttled box proof is
  `post-deploy-preview.spec.ts` (edge/msedge — Chromium codecs + CDP
  `Network.emulateNetworkConditions`), following `post-deploy-dabing.spec.ts` to
  drive the off-program Dabing dub for the `mixer-preset-original` dub-mix check.
- **The throttled box test is a BOUNDED, early-exit `expect.poll` — NEVER a fixed
  soak.** `post-deploy.config.ts` runs on the GATING post-deploy path (ci.yml
  `exit 1` on failure), and the repo forbids sleep-dominated gating CI ("no
  sleep-based CI jobs; a soak window goes to cron", CLAUDE.md). Proving "the media
  reaches `t0 + 15 s` within ~25 s wall" distinguishes the fix (tracks real time)
  from the bug (plateaued ~33 s behind) WITHOUT a `waitForTimeout(60_000)`. The
  full 60 s throttled soak is a MANUAL box verification (`probe-preview-throttled.mjs
  1000 100 150`), not a gating spec — do not re-add a fixed multi-second
  `waitForTimeout` to any post-deploy spec.

## #184 round G — transport lag: ping/pong RTT + dropping a backlog

The owner's fader → audible change took ~70 s through `sp.newlevel.media`. The
control path was fine; the PREVIEW TRANSPORT was the lag. On the public URL the
preview WS goes box → `cloudflared` (http2/TCP) → Cloudflare edge → back into the
same LAN. When the tunnel dips below the stream rate the backlog builds INSIDE
the tunnel, where every round-F guard is blind: `socket.send` on the box never
blocks (so the broadcast `Lagged` drop never fires), the live-edge chase measures
a `buffered.end` that is itself stale, and the lag beacon rides the same backlog
(`produced − buffered_end ≈ 0`, no badge). Do NOT regress:

- **Application-level ping/pong (the only JS-visible round trip — browsers hide
  WS control-frame pings).** From the FIRST binary frame (the init segment) the
  shim sends `{"ping": performance.now()}` every `PING_INTERVAL_MS = 1000`; the
  server's existing `socket.recv()` arm answers at once on the SAME socket with
  `{"pong": <same>}` (`api/preview.rs::pong_frame`, pure + tested; the number is
  echoed as its RAW JSON text via serde_json `raw_value` — default float parsing
  is not round-trip exact). The pong queues behind exactly the backlog the media
  is in, so `rtt = now − pong` is the real transport lag; both stamps are the
  browser's own (no clock skew). Pinging starts at the init, not at socket open:
  the server sends the init only after the encoder child produced one (a cold
  start / the libx264 fallback takes seconds) and answers pings only in its
  post-init loop — a slow start must never read as transport lag.
- **ONE lag number** (`previewLagS`, exported pure): `onLag` reports the WORST of
  the round-F beacon lag, the LAST rtt, and how long the oldest unanswered ping
  has waited (a backlog is at least that deep — the badge shows before any pong
  is back). Reported each pump tick from `_reportLag`, independent of a buffered
  range. The ≥ 3 s display threshold stays `sp_core::preview_lag_display`.
- **The reconnect rule** (`shouldReconnect`, exported pure, table-tested in
  `e2e/preview.spec.ts` node-side): reconnect when the last 2 rtts both exceed
  `RTT_RECONNECT_MS = 3000`, or a ping has waited > `NO_PONG_RECONNECT_MS = 5000`,
  or the socket is LOST (`socketLost` — it closed on its own, e.g. a server
  restart / relay close, or delivered no init within `INIT_TIMEOUT_MS = 12000`;
  the server itself gives up on the init after ~10 s), and never within the
  reconnect backoff of the last reconnect (round G2: 12 s → 24 s → 48 s, cap
  60 s — was a fixed `MIN_RECONNECT_GAP_MS = 10000`; a server that is down is
  never hammered). "No pong for 5 s" is measured from the
  oldest UNANSWERED ping, never from the last pong — a background tab whose
  timers are throttled to one tick a minute would otherwise read "no pong for
  60 s" and reconnect for nothing. The 1 Hz health/ping timer runs for each
  socket's whole life (started in `_connect`); it only SENDS pings after the init.
- **`_reconnect()`** closes the old socket (`onmessage`/`onerror`/`onclose`
  nulled first, so a late frame or close from it is ignored), clears the append
  queue, sets `_needResync`, reports lag 0, and re-opens through the ONE connect
  path `_connect(path)` — a new WebSocket is a new tunnel stream at the live
  edge; the old backlog is discarded. The encoder child survives (the new viewer
  subscribes well inside `VIEWER_TTL`). `_needResync` is consumed right before
  the next queued frame is appended — after a reconnect that is the new socket's
  INIT segment, so the old media plays until the new socket delivers, then the
  picture holds its last frame until the first new fragment (~0.5 s). The clear
  sets `_snapToStart`: the next `_maintain` with media buffered puts the playhead
  on the FIRST new sample unconditionally (a reconnect can land on a restarted
  media timeline — a new encoder child starts at 0 — behind the old playhead);
  never while the clearing `remove()` is still pending (`sb.updating`), when
  `buffered` still shows the OLD range.
- **Encoder respawn closes the relay** (`preview_encoder.rs::supervise`): a child
  that had STREAMED and died with viewers present is respawned AND
  `relay.close()`d — the viewers hold the old child's init and the new init is
  only cached, never sent to them, so they would freeze on a restarted timeline
  while their pings are still answered. Closing makes each socket close; the
  shim's `socketLost` rule reconnects it onto the new init. A child that died
  with NO init does not close (its viewers are still in `wait_for_init`).
  Box-only glue (`mutants::skip`), not Linux-testable.
- **Server log (`api/preview.rs`)**: every viewer session logs INFO on connect
  (init sent) and on disconnect with `secs` + `pongs` answered — a viewer the
  shim keeps reconnecting on a slow link shows up in the box log as a stream of
  ~10-s sessions (the shim itself may not log).
- **Mock seams** (`e2e/mock-api.mjs`): the shim forwards `?pong_delay_ms=<N>`
  from the PAGE url onto the preview WS (like `lag_ms`); the mock then delivers
  every post-init frame — fragments, beacon AND pong — N ms late (a tunnel
  backlog) on EVERY socket. The mock always echoes `{"ping":N}` → `{"pong":N}`.
  For "the first socket is bad, the reconnected one is healthy", the one-shot
  `POST /__mock/preview-fault {delay_ms?, close_after_frags?, hold_init?}`
  applies to the NEXT preview connection only; round G2 adds
  `{sequence: [fault, …]}` — one fault per next connection, in order (`{}` in
  the list = a healthy socket). `{}` clears any pending fault(s); always clear
  it in `finally`.
  `preview.spec.ts` proves: `pong_delay_ms=6000` → the badge shows on the first
  socket and a second socket opens within ~15 s (the no-pong rule); a 4 s
  one-shot backlog → reconnect via the rtt rule (no ping can wait > 5 s), the
  video plays again and the badge clears; a server-closed first socket → a
  second socket and the video plays again; no knob → ONE socket, no badge and
  ≥ 15 ping/pong pairs over 20 s.

## #184 — per-deploy pause/seek latency proof of the owner's path

`post-deploy-preview.spec.ts` (edge/msedge, off-program Dabing output) re-proves
the owner's actual complaint path every deploy, with all timings PRINTED:

- **Pause freezes the preview.** Clicking the Player's `player-playpause` posts
  `/pause`, which stops the pipeline decode → the encoder child starves → the WS
  stops → the `<video>` drains its (≤ 2 s) buffer and freezes. The proof taps the
  DECODED output directly, not the toggle text: a small offscreen-canvas
  frame-hash must go STABLE (last change within 3 s of pause, then held ≥ 2 s),
  AND a Web Audio `AnalyserNode` RMS tap on the `<video>` must drop below a quiet
  floor within 3 s (a logged audible baseline first proves the tap works). The
  `<video>` is MSE (same-origin blob), so drawing it to a canvas does NOT taint
  it — `getImageData` works; `createMediaElementSource` reads real samples.
- **A real seek lands on the target.** A `page.mouse` drag on `player-seek`
  (~+60 s) must land the bar on the committed target (the #184 pending display
  hold shows it immediately) and never drop below the pre-drag value after the
  commit. The BACKEND fast-forward is proven WITHOUT a position endpoint (none
  exists — position is WS-pushed): the bar can only EXCEED the target once the
  pending hold releases to the real live WS-fed position, so "the bar passes the
  target" is the honest backend proof (a failed seek lets the 5 s hold expire and
  the bar drops back to the stale position, never exceeding the target).
- Bounded, early-exit `expect.poll`s only (the no-soak rule above). The 2 s
  frame-hash stability window is a measurement, not a soak — it early-exits the
  moment a ≥ 2 s stable run is confirmed.

## #206 — never assert on ONE live-audio sample; control the moment + content

**The preview is 64 kb/s AAC (`preview_encoder.rs`, round F) — it carries NO
usable energy above ~12 kHz.** A "12–16 kHz band drops when the original voice
leaves" assertion read the analyser floor both times (−184 / −172 dB, random
sign) on the box. Assert a full-band RMS level change instead, and remove BOTH
voices (the original via the control under test, the dub via the API) so the
compare is speech vs the instrumental bed, not two overlapping speech tracks.

The post-deploy suite reads real audio off the preview `<video>` (RMS on the
`<video>` element, or a 12–16 kHz band via `captureStream`). Two assertions
sampled ONE moment of live content and went red on audio LUCK — 3 red E2E jobs
on 22.9.2026, same code passing and failing alternately, the deploy green each
time. The discipline, for EVERY audio assertion (`e2e/post-deploy-preview.spec.ts`,
`e2e/post-deploy-dabing.spec.ts`, and any new one):

- **Own the session + poll until AUDIBLE — never a fixed wait for "audio is
  present".** A viewer that joins a running encoder session right after a song
  restart (`switching to new song`) gets the emitter's silence padding for its
  first seconds (RMS ~0). Sample RMS on a bounded loop until it reads above the
  floor for a STREAK of N samples (`audibleStreak` in `e2e/audio-helpers.mjs`),
  then take the baseline; fail with the observed max if never audible — that is a
  real defect, not a race. The per-sample wait is only spacing; the streak is the
  synchronisation.
- **Average a band over a window — one snapshot never decides the sign.** A
  single `getFloatFrequencyData` is content luck; take N samples over ~2 s and
  average them (`averageDb` drops non-finite bins so a codec-less runner reads
  null, not NaN).
- **Content-match a before/after compare by SEEKING to the same position.**
  Comparing the band "before" and "~5 s after" a mixer change measures two
  DIFFERENT moments of live content (post-G0 a dub has stems, so vokály=0 leaves
  instrumental+dub, not dub-only). Instead: seek to a fixed T, play ~1 s past it,
  measure; make the change; assert the API shows it landed (`GET /api/v1/mix`
  `dub.vokaly === 0`); seek to the SAME T, measure again; assert the drop on the
  SAME audio. Position is WS-pushed onto `player-seek` (no position endpoint), so
  after an API seek wait two-phase: the position drops to ~T (a backward seek
  landed), THEN advances to T + 1 s — never a stale read, never a blind timeout.
- The pure decision helpers (`audibleStreak`, `averageDb`) live in
  `e2e/audio-helpers.mjs` and are unit-tested on ubuntu in `frontend.spec.ts`
  (mock-free), so the determinism is proven without the box.

## #184 round G2 — the preview audio is kept on the wall clock BOTH ways; the owner's path is the acceptance

The owner heard a fader change ~70 s late and the preview then froze into a
reconnect loop — reproduced on LAN in a real browser (#184 comment 5802408328),
so round G's transport-only diagnosis was wrong. The root cause was the audio
feeder's timing model. Do NOT regress:

- **Two timelines.** The encoder's VIDEO input is wall-clock stamped
  (`-use_wallclock_as_timestamps`); its PCM AUDIO input is SAMPLE-COUNT timed
  (round 3: the box ffmpeg muxes zero audio packets when PCM is wall-stamped —
  never change that). So preview A/V stays in sync only while the audio WRITTEN
  so far equals the wall time elapsed. Anything that makes the two diverge
  shifts the audio against the picture for the child's whole life.
- **Why add-only gap fill accumulated lag.** The decode-seam blocks
  (`StreamTap::offer_audio`, bounded channel of 48) arrive late on a loaded box
  and then in a catch-up BURST. The old `gap_fill_samples` padded silence during
  the late phase and then APPENDED the burst behind that silence — every hiccup
  moved the audio permanently later (tens of seconds after a few minutes). It
  also wrote nothing while no block arrived (the fill ran only inside the
  `Ok(block)` arm), so ffmpeg — which interleaves by timestamp — waited for audio,
  emitted no fragments, the WS dropped the idle client, and the browser looped.
- **The aligner (`preview_stream.rs`, pure + unit-tested + mutation-gated).**
  `wall_frames = preroll_frames + elapsed_since_feeder_start × 48` (per ms; the
  round-3 start preroll — connect gap + decode-seam lead — is kept, so
  written == wall right after it). `align_block(wall, written, block)` →
  `AlignAction { pad_frames, skip_frames }`: pad silence up to the wall when
  written < wall − `ALIGN_PAD_THRESHOLD_MS` (150); if the block would end more
  than `MAX_AHEAD_MS` (300) ahead, skip its OLDEST frames so it ends
  `ALIGN_TARGET_AHEAD_MS` (100) ahead (never negative, never more than the
  block). `align_timeout(wall, written)` pads up to the wall on every 200 ms
  receive timeout, so the encoder is NEVER starved. Result: |written − wall| ≤
  300 ms forever (the 1000-step property test pins it). A trimmed burst loses
  audio from the PREVIEW only — the wall/NDI path never sees this code. The
  block's written part is the pure `block_tail_range(skip_frames,
  block_samples)` — whole frames only (a lone trailing sample would swap L/R
  for the rest of the child); the feeder's frames ↔ interleaved-samples
  arithmetic lives there, not in the `mutants::skip` glue.
- **The feeder logs its alignment** at INFO every 10 s:
  `preview-afeed: ahead_ms=<written−wall> padded_ms=<cum> skipped_ms=<cum>` —
  on the box this is the first thing to read when preview audio is late/early.
  A steadily growing `skipped_ms` means the decode seam keeps bursting (a
  loaded box), which is now harmless; a large |ahead_ms| would be a regression.
- **Browser reconnect backoff (`preview_player.js`).** Reconnects back off
  `reconnectGapMs(reconnectsWithoutMedia)` = 12 s → 24 s → 48 s, cap 60 s
  (`RECONNECT_BACKOFF_BASE_MS` / `_MAX_MS`); the count resets when a socket
  delivers its first MEDIA fragment (the binary frame after its init).
  `socketLost` is a pure exported rule: waiting for the first init inside
  `INIT_TIMEOUT_MS` (12 s) is never "lost" and never lag. A replaced socket that
  is still CONNECTING is closed on `open` (`closeQuietly`) — `close()` on a
  CONNECTING socket logs "WebSocket is closed before the connection is
  established", which the owner's console showed every 12 s. Table tests in
  `e2e/preview.spec.ts` (node-side), plus a mock behaviour test that pins the
  counter WIRING: `/__mock/preview-fault {sequence:[…]}` queues one fault per
  next connection, and five sockets (hold_init, hold_init, init-then-close,
  init+2 fragments-then-close, healthy) must open 12 s, 12 s, 24 s, 12 s apart
  (an init alone does not reset the backoff; a media fragment does).
- **The owner's path is the ACCEPTANCE for every preview / mixer change**
  (owner ROZHODNUTÉ 2026-09-23: "akceptácia každého ďalšieho kola =
  post-deploy Playwright test na SKUTOČNOM boxe cestou ownera", not a mock).
  `e2e/post-deploy-owner-path.spec.ts` (`edge` project — it listens to the
  preview audio, bundled Chromium has no H.264/AAC; viewport 1600×1000 — the
  default hides the vertical faders): dashboard → the Dabing playlist row
  (`playlist-picker-item[data-playlist-id]`) → ▶ the ready dub (video 344 when
  ready) → "▶ Živý náhľad" → Web Audio RMS on the preview `<video>` → (a)
  audible (> −50 dBFS) within 10 s; (b) real-mouse drag vokály + podklad +
  dabing to 0 → < −60 dBFS within 4 s of the last PATCH (PATCH times from
  `page.on('request')`); (c) vokály back up → audible within 4 s; (d) 180 s,
  one sample per 500 ms: never silent > 5 s and ≤ 1 extra `preview.ws`
  (`page.on('websocket')`); (e) `.lyr-current` inside `.lyrics-view-scroll` at
  every sample; (f) zero console errors. Decisions need a STREAK of samples
  (`quietStreak` / `audibleStreak`, #206 discipline); `longestSilentRunMs`
  counts a missing read as silent. The 180 s window is the owner-ruled
  acceptance MEASUREMENT (every sample asserted, fail on the first violation),
  not a sleep-soak — it is the one sanctioned exception to the no-soak rule
  above; do not add other fixed soaks. The soak runs with vokály, podklad AND
  dabing dragged back up (the ready dub is mostly speech — a pause on a thin bed
  must not read as a frozen preview). It never touches OBS scenes; `afterEach`
  (runs even when the body timed out — a body `finally` does not) restores the
  dub memory (vokály 1, podklad 1, dabing as found), stops the preview, pauses
  the Dabing output, and then asserts (f) as the last assertion.
