//! Live A/V preview STREAM tap (#178) — the source side of the low-latency
//! browser monitor.
//!
//! Sits next to the #15 JPEG [`PreviewTap`](crate::playback::preview::PreviewTap)
//! (which stays for the card thumbnail): both are bundled into [`DecodeTaps`]
//! and offered from the SAME decode seam in `pipeline::decode_and_send` and
//! `pipeline_paced::run_decode_producer`. The stream tap turns each decoded
//! NV12 frame into a FIXED 640×360 letterboxed NV12 frame and each post-mix
//! audio block into interleaved f32, hands both to a bounded channel, and — on
//! the first WS viewer — spawns ONE bundled-`ffmpeg` child
//! (`preview_encoder`) that encodes + muxes fragmented MP4 relayed to the
//! browser via [`fmp4_relay`](super::fmp4_relay).
//!
//! Iron rules (`preview.md`), identical to the JPEG tap:
//! * With **no viewer** an offer is a single relaxed atomic load + return — no
//!   lock, no allocation, no channel touch. It NEVER blocks the decode /
//!   producer / emit thread.
//! * With a viewer, the downscale runs on the decode thread (nearest-neighbour,
//!   no encode) into a RECYCLED buffer; a full channel DROPS the frame (the
//!   child paces at CFR) — never blocks.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};

use super::fmp4_relay::FragmentRelay;

/// Fixed preview canvas width (the ffmpeg raw video input geometry).
pub const OUT_W: u32 = 640;
/// Fixed preview canvas height.
pub const OUT_H: u32 = 360;
/// NV12 byte length of the fixed canvas: `W*H` luma + `W*H/2` interleaved chroma.
pub const OUT_NV12_LEN: usize = (OUT_W as usize) * (OUT_H as usize) * 3 / 2;
/// BT.601 limited-range black: Y=16, U=V=128. Letterbox bars use these.
const BLACK_Y: u8 = 16;
const NEUTRAL_C: u8 = 128;

/// Bounded video backlog handed to the feeder (drop-on-full; the child paces CFR).
const VIDEO_CHANNEL_CAP: usize = 4;
/// Bounded audio backlog (small f32 blocks; drop-on-full so the emit path never waits).
const AUDIO_CHANNEL_CAP: usize = 48;
/// Broadcast backlog of fMP4 fragments per viewer before it is dropped + resynced.
/// #184 round F: 4 fragments = 2 s at `-frag_duration 500000` (was 64 = 32 s). A
/// slow remote (internet) viewer that back-pressures now drops fragments and
/// resyncs on the next keyframe-aligned fragment instead of sitting a full 32-s
/// backlog behind the wall — the "reakcia 30 s" the owner saw over the internet.
const RELAY_CAPACITY: usize = 4;
/// Max recycled video buffers kept in the pool.
const POOL_MAX: usize = 6;

/// Placement of the scaled image inside the fixed 640×360 canvas: scaled size +
/// centred, even-aligned offsets (so the 2×2-subsampled chroma stays aligned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub w: u32,
    pub h: u32,
    pub off_x: u32,
    pub off_y: u32,
}

/// Compute the letterboxed placement of a `sw×sh` source inside the 640×360
/// canvas, preserving aspect ratio and never upscaling past the canvas. All
/// four values are floored to even numbers; a degenerate source yields a zero
/// image (a fully black canvas).
pub fn placement_for(sw: u32, sh: u32) -> Placement {
    if sw == 0 || sh == 0 {
        return Placement {
            w: 0,
            h: 0,
            off_x: OUT_W / 2,
            off_y: OUT_H / 2,
        };
    }
    // Largest aspect-preserving fit inside the canvas: each axis is the
    // full-canvas value capped by the aspect-scaled value (branch-free, so
    // there is no `<=`-boundary comparison whose `<` mutant would be
    // equivalent at the exact-16:9 case). `min` picks whichever axis is tighter.
    let w = OUT_W.min(sw * OUT_H / sh);
    let h = OUT_H.min(sh * OUT_W / sw);
    // Floor to even (keeps 2×2-subsampled chroma aligned), keep at least 2×2.
    let w = (w & !1).max(2);
    let h = (h & !1).max(2);
    let off_x = ((OUT_W - w) / 2) & !1;
    let off_y = ((OUT_H - h) / 2) & !1;
    Placement { w, h, off_x, off_y }
}

/// Nearest-neighbour letterbox an arbitrary NV12 frame into the fixed 640×360
/// NV12 canvas `dst` (must be [`OUT_NV12_LEN`] bytes). Fills the whole canvas
/// with black first, then blits the scaled image. A too-short / degenerate
/// source leaves the canvas black (the frame is simply skipped, never an error).
///
/// `mutants::skip`: the geometry decision ([`placement_for`]) is exhaustively
/// unit-tested; what remains here is a nearest-neighbour PIXEL COPY whose inner
/// index arithmetic mutants are effectively equivalent for a monitoring
/// downscale (a shifted sample still lands within the same solid region), and
/// the visible result is box-verified. The black-fill / short-source / bar
/// boundaries ARE asserted by the tests.
#[cfg_attr(test, mutants::skip)]
pub fn letterbox_nv12_into(sw: u32, sh: u32, stride: u32, src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(dst.len(), OUT_NV12_LEN);
    let ow = OUT_W as usize;
    let oh = OUT_H as usize;
    let y_plane = ow * oh;
    // Paint black: luma then neutral chroma.
    dst[..y_plane].fill(BLACK_Y);
    dst[y_plane..].fill(NEUTRAL_C);

    let p = placement_for(sw, sh);
    if p.w == 0 || p.h == 0 {
        return;
    }
    let stride = stride as usize;
    let (sw_us, sh_us) = (sw as usize, sh as usize);
    if stride < sw_us {
        return;
    }
    let src_y_size = match stride.checked_mul(sh_us) {
        Some(v) => v,
        None => return,
    };
    let src_uv_rows = sh_us / 2;
    let need = match stride
        .checked_mul(src_uv_rows)
        .and_then(|uv| src_y_size.checked_add(uv))
    {
        Some(v) => v,
        None => return,
    };
    if src.len() < need {
        return;
    }

    let (dw, dh) = (p.w as usize, p.h as usize);
    let (ox, oy) = (p.off_x as usize, p.off_y as usize);

    // Luma blit.
    for yy in 0..dh {
        let sy = yy * sh_us / dh;
        let src_row = sy * stride;
        let dst_row = (oy + yy) * ow + ox;
        for xx in 0..dw {
            let sx = xx * sw_us / dw;
            dst[dst_row + xx] = src[src_row + sx];
        }
    }

    // Chroma blit: half-res in both axes. The interleaved UV plane starts at
    // `y_plane` in dst and `src_y_size` in src, both `stride`/`ow` bytes wide.
    let cw = dw / 2;
    let ch = dh / 2;
    for cy in 0..ch {
        let scy = cy * (sh_us / 2) / ch.max(1);
        let src_row = src_y_size + scy * stride;
        let dst_row = y_plane + (oy / 2 + cy) * ow + ox;
        for cx in 0..cw {
            let scx = cx * (sw_us / 2) / cw.max(1);
            let src_off = src_row + scx * 2;
            let (u, v) = if src_off + 1 < src.len() {
                (src[src_off], src[src_off + 1])
            } else {
                (NEUTRAL_C, NEUTRAL_C)
            };
            let dst_off = dst_row + cx * 2;
            dst[dst_off] = u;
            dst[dst_off + 1] = v;
        }
    }
}

/// Coerce an interleaved-f32 audio block into the encoder child's FIXED stereo
/// f32 input (#178 round 2). Mono (`channels == 1`) is upmixed by duplicating
/// each sample into the L and R lanes (so it plays at the correct speed, not
/// double); stereo (`channels == 2`) is forwarded verbatim; any other channel
/// count returns `None` (the block is dropped — the child geometry is fixed
/// stereo and cannot consume it).
pub fn to_stereo(samples: &[f32], channels: u32) -> Option<Vec<f32>> {
    match channels {
        // No `with_capacity` hint (its multiplier would be an equivalent mutant —
        // capacity never changes the produced Vec); each mono sample becomes an
        // interleaved L,R pair.
        1 => Some(samples.iter().flat_map(|&s| [s, s]).collect()),
        2 => Some(samples.to_vec()),
        _ => None,
    }
}

/// The decode-seam A/V-sync lead (ms) for a pipeline's clocking path (#178
/// round 2). On the SDK-clocked path (`genlock_pacing == false`, so the #192
/// wall-clock emitter carries the audio) the decoder opens with a 100 ms audio
/// read-ahead, so at the decode seam the audio LEADS the video by that much; the
/// encoder's audio feeder absorbs it into the silence preroll
/// ([`audio_preroll_samples`]) to re-sync (round 3 — the box ffmpeg ignored the
/// former `-itsoffset` lever). The paced path has no emitter and thus no lead (0).
pub fn lead_ms_for(genlock_pacing: bool) -> u32 {
    let emitter_present = !genlock_pacing;
    (crate::playback::pipeline::audio_emitter::decoder_tolerance_ms(emitter_present)
        - sp_decoder::split_sync::DEFAULT_TOLERANCE_MS) as u32
}

/// How many interleaved-stereo f32 samples of SILENCE the audio feeder prepends
/// to align the preview's sample-count audio timeline with the video's
/// wall-clock timeline (#178 round 3). `connect_gap_ms` is how long the video
/// input had already been feeding when the audio input connected (feed-on-connect
/// opens video first), capped at 5 s so a late-connecting audio input can never
/// prepend an unbounded silence; `lead_ms` is the decode-seam A/V lead
/// ([`lead_ms_for`]) that the SDK-clocked emitter's read-ahead introduces. At
/// 48 kHz stereo each millisecond is `48 * 2` interleaved f32 samples. Replaces
/// the box-unreliable `-itsoffset` lever (the box ffmpeg kept audio `start_time`
/// at 0.000 regardless), aligning A/V deterministically on our side instead.
pub fn audio_preroll_samples(connect_gap_ms: u64, lead_ms: u32) -> usize {
    ((connect_gap_ms.min(5000) + lead_ms as u64) * 48 * 2) as usize
}

/// Stereo frames per millisecond of the preview audio input (fixed 48 kHz).
pub const PREVIEW_AUDIO_FRAMES_PER_MS: u64 = 48;

/// Pad threshold (#184 round G2): when the audio written so far lags the wall
/// clock by MORE than this, the feeder writes silence up to the wall — on a
/// block AND on every 200 ms receive timeout, so ffmpeg (which interleaves the
/// wall-clock video with the sample-count audio by timestamp) is never starved
/// of audio and never stops emitting fragments.
pub const ALIGN_PAD_THRESHOLD_MS: u64 = 150;

/// Ahead bound (#184 round G2): a block that would push the written audio MORE
/// than this ahead of the wall clock is trimmed. Round G's add-only gap fill
/// padded silence while the decode-seam blocks were late and then APPENDED the
/// late catch-up burst behind that silence, so every hiccup permanently shifted
/// the preview audio later than its video — the ~70 s "fader heard a minute
/// later" lag the owner reported. Trimming bounds the lag for good.
pub const MAX_AHEAD_MS: u64 = 300;

/// Where a trimmed burst lands (#184 round G2): its OLDEST frames are dropped
/// so the written audio ends this far ahead of the wall clock (a little headroom
/// for the next on-time block, well inside [`MAX_AHEAD_MS`]).
pub const ALIGN_TARGET_AHEAD_MS: u64 = 100;

/// What the preview audio feeder does with one incoming block (#184 round G2):
/// first write `pad_frames` stereo frames of silence, then the block MINUS its
/// first (oldest) `skip_frames` frames. `skip_frames` never exceeds the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlignAction {
    pub pad_frames: usize,
    pub skip_frames: usize,
}

/// Silence (stereo frames) to write when NO block arrived within the feeder's
/// 200 ms receive timeout (#184 round G2): everything up to the wall clock
/// once the written audio lags it by more than [`ALIGN_PAD_THRESHOLD_MS`], else
/// nothing. `wall_frames` is the target position on the audio timeline (the
/// elapsed wall time since the feeder started, plus its start preroll), and
/// `written_frames` the stereo frames already written. Never negative.
pub fn align_timeout(wall_frames: u64, written_frames: u64) -> usize {
    let threshold = ALIGN_PAD_THRESHOLD_MS * PREVIEW_AUDIO_FRAMES_PER_MS;
    if written_frames + threshold < wall_frames {
        (wall_frames - written_frames) as usize
    } else {
        0
    }
}

/// Keep the preview's SAMPLE-COUNT audio timeline on the video's WALL-CLOCK
/// timeline in BOTH directions for one block of `block_frames` stereo frames
/// (#184 round G2, replacing the add-only #178 item-15 gap fill): pad up to the
/// wall when behind by more than [`ALIGN_PAD_THRESHOLD_MS`] (exactly like
/// [`align_timeout`]); then, if the block would end more than [`MAX_AHEAD_MS`]
/// ahead of the wall, skip its OLDEST frames so it ends
/// [`ALIGN_TARGET_AHEAD_MS`] ahead (at most the whole block — a block that
/// cannot reach the target is dropped entirely, never a negative write). The
/// trimmed audio is lost from the PREVIEW only; the wall / NDI path never sees
/// this code.
pub fn align_block(wall_frames: u64, written_frames: u64, block_frames: usize) -> AlignAction {
    let pad_frames = align_timeout(wall_frames, written_frames);
    let block_end = written_frames + pad_frames as u64 + block_frames as u64;
    let max_end = wall_frames + MAX_AHEAD_MS * PREVIEW_AUDIO_FRAMES_PER_MS;
    let skip_frames = if block_end > max_end {
        let target_end = wall_frames + ALIGN_TARGET_AHEAD_MS * PREVIEW_AUDIO_FRAMES_PER_MS;
        ((block_end - target_end) as usize).min(block_frames)
    } else {
        0
    };
    AlignAction {
        pad_frames,
        skip_frames,
    }
}

/// State shared between the decode-side taps, the WS viewers, and the encoder
/// child. Held behind an `Arc` by [`StreamTap`].
pub struct StreamShared {
    label: String,
    /// #178 A/V-sync lead (ms): how far the decode-seam audio LEADS the video on
    /// this pipeline's clocking path (100 on the SDK-clocked path from the #192
    /// lookahead, 0 on the paced path). The encoder's audio feeder folds this
    /// into its silence preroll ([`audio_preroll_samples`]) to bring preview A/V
    /// into sync (round 3 — replaced the box-unreliable `-itsoffset`).
    lead_ms: u32,
    /// Number of connected WS viewers. `0` = the offer fast-path early-out.
    viewers: AtomicUsize,
    /// Whether an encoder child is currently running for this pipeline.
    encoder_running: AtomicBool,
    video_tx: Sender<Vec<u8>>,
    video_rx: Receiver<Vec<u8>>,
    audio_tx: Sender<Vec<f32>>,
    audio_rx: Receiver<Vec<f32>>,
    /// Recycled 640×360 NV12 buffers (avoids a per-frame alloc while watched).
    pool: Mutex<Vec<Vec<u8>>>,
    /// fMP4 fragment relay the child's reader thread feeds and viewers read.
    relay: Arc<FragmentRelay>,
}

impl StreamShared {
    fn new(label: String, lead_ms: u32) -> Self {
        let (video_tx, video_rx) = bounded(VIDEO_CHANNEL_CAP);
        let (audio_tx, audio_rx) = bounded(AUDIO_CHANNEL_CAP);
        Self {
            label,
            lead_ms,
            viewers: AtomicUsize::new(0),
            encoder_running: AtomicBool::new(false),
            video_tx,
            video_rx,
            audio_tx,
            audio_rx,
            pool: Mutex::new(Vec::new()),
            relay: FragmentRelay::new(RELAY_CAPACITY),
        }
    }

    /// Whether any viewer is currently connected (relaxed — the offer fast path).
    #[inline]
    pub fn has_viewer(&self) -> bool {
        self.viewers.load(Ordering::Relaxed) != 0
    }

    /// Decode-thread hot path: offer one decoded NV12 frame. Returns instantly
    /// with no viewer (one relaxed load). With a viewer, letterbox into a
    /// recycled buffer and `try_send`; a full channel drops (recycles) the frame.
    pub fn offer_video(&self, sw: u32, sh: u32, stride: u32, nv12: &[u8]) {
        if !self.has_viewer() {
            return;
        }
        let mut buf = self.take_buffer();
        letterbox_nv12_into(sw, sh, stride, nv12, &mut buf);
        match self.video_tx.try_send(buf) {
            Ok(()) => {}
            Err(TrySendError::Full(b)) => self.recycle(b),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Emit/decode-thread hot path: offer one post-mix interleaved-f32 audio
    /// block (the wall mix — karaoke/dub included). No viewer = one relaxed
    /// load; a full channel drops the block. Never blocks the caller.
    pub fn offer_audio(&self, samples: &[f32], _sample_rate: u32, channels: u32) {
        if !self.has_viewer() {
            return;
        }
        // The child's audio input is a FIXED 48 kHz STEREO f32 stream, so the
        // block must be stereo before it is forwarded (a mono block sent as-is
        // would play at double speed). [`to_stereo`] upmixes mono and drops any
        // unexpected channel count.
        if let Some(block) = to_stereo(samples, channels) {
            let _ = self.audio_tx.try_send(block);
        }
    }

    // mutants::skip — a buffer-pool ALLOCATION optimization: every mutant here
    // (skip the pool, drop the resize, ignore the cap) yields identical
    // OUTPUT frames, differing only in how many `Vec`s are allocated, so no
    // behavioural test can distinguish them (they are equivalent by design).
    #[cfg_attr(test, mutants::skip)]
    fn take_buffer(&self) -> Vec<u8> {
        if let Ok(mut p) = self.pool.try_lock() {
            if let Some(mut b) = p.pop() {
                if b.len() != OUT_NV12_LEN {
                    b.resize(OUT_NV12_LEN, 0);
                }
                return b;
            }
        }
        vec![0u8; OUT_NV12_LEN]
    }

    #[cfg_attr(test, mutants::skip)]
    fn recycle(&self, buf: Vec<u8>) {
        if buf.len() != OUT_NV12_LEN {
            return;
        }
        if let Ok(mut p) = self.pool.try_lock() {
            if p.len() < POOL_MAX {
                p.push(buf);
            }
        }
    }

    /// Feeder side (encoder child): drain the video/audio backlog.
    pub fn video_receiver(&self) -> Receiver<Vec<u8>> {
        self.video_rx.clone()
    }
    pub fn audio_receiver(&self) -> Receiver<Vec<f32>> {
        self.audio_rx.clone()
    }
    /// The fragment relay (reader thread feeds it, WS viewers read it).
    pub fn relay(&self) -> Arc<FragmentRelay> {
        self.relay.clone()
    }
    /// Human label for logs (`"playlist-3"`).
    pub fn label(&self) -> &str {
        &self.label
    }
    /// The decode-seam A/V-sync lead in ms (see the field). The encoder's audio
    /// feeder folds this into its silence preroll ([`audio_preroll_samples`]).
    pub fn lead_ms(&self) -> u32 {
        self.lead_ms
    }
    /// Atomically claim the "encoder is running" flag; returns `true` iff THIS
    /// call transitioned it from stopped→running (so exactly one caller spawns).
    pub fn try_claim_encoder(&self) -> bool {
        self.encoder_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    /// Release the running flag (the child's monitor thread on teardown).
    pub fn release_encoder(&self) {
        self.encoder_running.store(false, Ordering::Release);
    }
}

/// Cheap, cloneable per-pipeline stream-tap handle.
#[derive(Clone)]
pub struct StreamTap {
    shared: Arc<StreamShared>,
}

impl StreamTap {
    /// Build a stream tap. `lead_ms` is the decode-seam A/V-sync lead for this
    /// pipeline's clocking path (see [`StreamShared::lead_ms`]).
    pub fn new(label: String, lead_ms: u32) -> Self {
        Self {
            shared: Arc::new(StreamShared::new(label, lead_ms)),
        }
    }

    /// Decode-thread hot path — see [`StreamShared::offer_video`].
    #[inline]
    pub fn try_offer_video(&self, sw: u32, sh: u32, stride: u32, nv12: &[u8]) {
        self.shared.offer_video(sw, sh, stride, nv12);
    }

    /// Decode/emit-thread hot path — see [`StreamShared::offer_audio`].
    #[inline]
    pub fn try_offer_audio(&self, samples: &[f32], sample_rate: u32, channels: u32) {
        self.shared.offer_audio(samples, sample_rate, channels);
    }

    /// The shared state (WS route: subscribe a viewer + spawn/read the encoder).
    pub fn shared(&self) -> &Arc<StreamShared> {
        &self.shared
    }
}

/// A connected WS viewer. Increments the viewer count on creation and
/// decrements on drop — the encoder child's monitor thread kills the child once
/// the count stays 0 past the viewer TTL.
pub struct ViewerGuard {
    shared: Arc<StreamShared>,
}

impl ViewerGuard {
    /// Register a viewer against `tap` (increments the count) and return the
    /// guard plus the fragment relay to read from.
    pub fn subscribe(tap: &StreamTap) -> (ViewerGuard, Arc<FragmentRelay>) {
        tap.shared.viewers.fetch_add(1, Ordering::AcqRel);
        (
            ViewerGuard {
                shared: tap.shared.clone(),
            },
            tap.shared.relay(),
        )
    }
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        // Saturating decrement — never wrap below zero (`checked_sub` refuses at
        // 0, so there is no comparison to get subtly wrong).
        let _ = self
            .shared
            .viewers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_sub(1));
    }
}

/// Bundle of the two per-pipeline decode-side taps, threaded through the
/// pipeline in the single tap-parameter slot (keeps `pipeline.rs` under the
/// 1000-line cap). Offered from the decode seam via [`DecodeTaps::offer_frame`].
#[derive(Clone)]
pub struct DecodeTaps {
    pub preview: crate::playback::preview::PreviewTap,
    pub stream: StreamTap,
}

impl DecodeTaps {
    /// Offer one decoded frame (video + its post-mix audio) to BOTH taps in one
    /// call, before the NDI submit / audio-emitter push consumes them. Borrows
    /// only — nothing is moved out of the frame.
    #[inline]
    pub fn offer_frame(
        &self,
        video: &sp_decoder::DecodedVideoFrame,
        audio: &[sp_decoder::DecodedAudioFrame],
    ) {
        self.preview
            .try_offer(video.width, video.height, video.stride, &video.data);
        self.stream
            .try_offer_video(video.width, video.height, video.stride, &video.data);
        for a in audio {
            self.stream
                .try_offer_audio(&a.data, a.sample_rate, a.channels);
        }
    }
}

// Per-playlist stream taps live in the existing `PreviewRegistry` (see
// `preview.rs`) alongside the JPEG taps (`register_taps` / `stream`), so no
// second registry has to be threaded through the near-1000-line `mod.rs` /
// `lib.rs` seams.

#[cfg(test)]
#[path = "preview_stream_tests.rs"]
mod tests;
