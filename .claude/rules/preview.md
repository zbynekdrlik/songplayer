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
- The pure parts — the arg builder, the ladder selection, the `-encoders` parse —
  are Linux unit-tested. The child/TCP/feeder/monitor lifecycle is
  `mutants::skip` glue (box-verified) but compiles cross-platform.

## fMP4 relay late-join contract (`fmp4_relay.rs`)

`BoxSplitter` (pure, Linux-tested: split reads across buffer boundaries, 64-bit
`largesize`, poison on corrupt size) turns the child's stdout into ONE
`RelayChunk::Init` (`ftyp`+`moov`) then one `RelayChunk::Fragment` per
`[styp?][sidx?]moof mdat`. `FragmentRelay` caches the init segment and broadcasts
fragments; a late joiner receives the cached init THEN the next fragment (every
fragment is keyframe-aligned under `+frag_keyframe`), and a viewer that falls
behind the broadcast backlog is dropped (never blocks the reader).

## MSE shim contract (sp-ui, #178 Round 2)

`preview_player.js` = a MediaSource + SourceBuffer shim,
`video/mp4; codecs="avc1.42E01E, mp4a.40.2"`: append the init segment, then queue
fragments; live-edge chase when `buffered.end − currentTime > 2 s`; evict > 30 s
behind; start MUTED, one click to unmute (Chrome's autoplay gesture rule).
Wrapped by `components/preview_video.rs`; `playlist_card.rs` shows the stream
`<video data-testid="preview-video">` while Playing, with a fullscreen button.
The JPEG `<img>` path is REMOVED from the card once the stream works (no dual path
in the card).

## chrome-channel E2E note (#178 Round 2)

H.264/AAC are ABSENT from Playwright's bundled Chromium, so the preview E2E runs
in a Playwright project with `channel: 'chrome'` (GitHub ubuntu runners ship
google-chrome). The mock (`e2e/mock-api.mjs`) serves a canned tiny fMP4 (init +
fragments) over the WS; the test asserts `<video>` `readyState ≥ 3`, `currentTime`
advances, the unmute click works, and zero console errors. Post-deploy: the
`e2e/post-deploy*` spec asserts the card `<video>` reaches `readyState ≥ 3` and
advances, following the suite's existing scene-restore discipline (never switch
the OBS program scene beyond what the suite already does).
