---
paths:
  - "crates/sp-server/src/playback/preview.rs"
  - "crates/sp-server/src/playback/pipeline.rs"
  - "crates/sp-server/src/playback/pipeline_paced.rs"
  - "sp-ui/src/components/playlist_card.rs"
---

# Live video preview (#15 part 2) — never touch the NDI submit / genlock path

The dashboard playlist card shows a low-res live preview of the currently-playing
song. Frames are sampled OPPORTUNISTICALLY from already-decoded output — the
governing constraint (owner rescope 2026-09-13, genlock #146–#151) is that the
preview must **never** touch the NDI submit / pacing path, add latency, or drop
wall fps.

## The offer seam

`crates/sp-server/src/playback/preview.rs` owns `PreviewTap` (a cheap `Arc`
handle) + `PreviewRegistry` (per-`playlist_id`, mirrors `NdiBurnRegistry`). The
Windows decode loops offer each decoded NV12 frame to the tap **before** it is
submitted to NDI:

- `pipeline::decode_and_send` — offer right before `submitter.submit_nv12(...)`
  (the frame's `data` is moved into the submit call, so offer by `&data` first).
- `pipeline_paced::run_decode_producer` — offer **on the decode PRODUCER thread**
  (#147 producer/consumer split), right after `decoder.next_synced()` and before
  the frame is pushed to the bounded queue. This is off the emit/submit thread
  entirely (even better than the old in-`prepare` offer), so the sampling never
  touches the time-critical paced submit path.

## Iron rules

1. **`try_offer` must never block the decode thread.** With no viewer it is a
   couple of relaxed atomic loads + return (no lock, no allocation). It uses
   `try_lock` (never `lock`) on the inbox and
   drops the frame if the encoder is busy. Do NOT add a blocking call, a
   `lock()`, or any per-frame allocation on the no-viewer path.
2. **No DCT on the decode thread.** The decode thread only does a cheap
   nearest-neighbour NV12→RGB downscale; the JPEG encode runs on the tap's
   background worker thread. Never move `encode_jpeg_rgb` onto the decode thread.
3. **`pacer.rs` / `submitter.rs` / FLAC / genlock stay byte-for-byte.** The
   preview adds exactly ONE `try_offer` call per decode loop — nothing else in
   the submit/pacing machinery changes. `pacer_tests.rs`'s byte-exact legacy
   call-site guard pins this; if you touch the call site, update that guard with
   an explicit justification.
4. **Viewer TTL, not a persistent subscription.** `GET .../preview.jpg` marks a
   viewer for `viewer_ttl_ms`; the tap only samples while a viewer is polling.
   The route returns `204` when idle, `404` for an unknown playlist.

## UI

`playlist_card.rs` renders the `<img data-testid="preview-img">` ONLY while the
card's `NowPlayingInfo.state` is `Playing`, and a `preview-placeholder`
otherwise. A ~3 fps `preview_tick` signal drives the `?t=` cache-buster so the
browser re-fetches. The poll loop reads page-owned signals with `try_*`
(disposed-signal safety, see `sp-ui-frontend.md`). The mock
(`e2e/mock-api.mjs`) serves a 1×1 JPEG for playlist 1 and marks it Playing.
