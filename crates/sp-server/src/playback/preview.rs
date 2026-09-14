//! Live low-res video preview tap (#15, part 2).
//!
//! Each playback pipeline gets a [`PreviewTap`] (a cheap `Arc` handle). The
//! Windows decode loops (`pipeline::decode_and_send` +
//! `pipeline_paced::decode_and_send_paced`) offer every decoded NV12 frame to
//! the tap via [`PreviewTap::try_offer`] BEFORE the frame is submitted to NDI.
//! The offer is engineered to NEVER touch the NDI submit / genlock / pacing
//! path and NEVER block the decode thread:
//!
//! * With no viewer, `try_offer` is a couple of relaxed atomic loads + return
//!   — no lock, no allocation, never blocks (a recent `GET .../preview.jpg`
//!   marks a viewer with a TTL).
//! * With a viewer, at most one frame per `min_ingest_interval` is accepted:
//!   the NV12 frame is nearest-neighbour downscaled to a small packed-RGB
//!   frame ON the decode thread (cheap, no DCT) and stored latest-wins in a
//!   single-slot inbox. If the background encoder is busy, the frame is
//!   DROPPED (`try_lock` — never blocks).
//! * A separate low-priority worker thread does the expensive JPEG encode and
//!   publishes the bytes as [`PreviewShared::latest_jpeg`], which the route
//!   `GET /api/v1/playback/{playlist_id}/preview.jpg` serves (or `204` when
//!   idle).
//!
//! Mirrors the shared-registry pattern of [`super::ndi_burn::NdiBurnRegistry`]:
//! [`PreviewRegistry`] maps `playlist_id` → `PreviewTap`, is created once in
//! `lib.rs::start`, shared with `AppState` (route reader) and the playback
//! engine (registers a tap per pipeline at spawn).
//!
//! The tap + downscale + encode are fully cross-platform and unit-tested on
//! Linux with synthetic frames; only the decode loops that CALL `try_offer`
//! are Windows-only.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Instant;

use tracing::{debug, info};

/// Default max preview width in px. Height follows the source aspect ratio.
pub const DEFAULT_MAX_WIDTH: u32 = 320;
/// Default minimum interval between accepted frames (5 fps ingest ceiling).
pub const DEFAULT_MIN_INGEST_INTERVAL_MS: u64 = 200;
/// Default viewer TTL: a `GET .../preview.jpg` keeps the tap "subscribed" for
/// this long. Idle pipelines cost one atomic load per decoded frame.
pub const DEFAULT_VIEWER_TTL_MS: u64 = 3_000;
/// Default JPEG quality (0..=100). Low — this is a small monitoring preview.
pub const DEFAULT_JPEG_QUALITY: u8 = 60;

/// A downscaled packed-RGB (3 bytes/px) frame awaiting JPEG encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPreviewFrame {
    pub width: u32,
    pub height: u32,
    /// Packed RGB8, `width * height * 3` bytes, row-major, no padding.
    pub rgb: Vec<u8>,
}

/// Tunables for a preview tap.
#[derive(Debug, Clone, Copy)]
pub struct PreviewConfig {
    pub max_width: u32,
    pub min_ingest_interval_ms: u64,
    pub viewer_ttl_ms: u64,
    pub jpeg_quality: u8,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            max_width: DEFAULT_MAX_WIDTH,
            min_ingest_interval_ms: DEFAULT_MIN_INGEST_INTERVAL_MS,
            viewer_ttl_ms: DEFAULT_VIEWER_TTL_MS,
            jpeg_quality: DEFAULT_JPEG_QUALITY,
        }
    }
}

/// Single-slot inbox handed from the decode thread to the encoder worker.
struct Inbox {
    /// Latest downscaled frame waiting to be encoded (latest-wins).
    pending: Option<RawPreviewFrame>,
    /// The worker is currently encoding — new offers are dropped.
    busy: bool,
}

/// Shared state behind a [`PreviewTap`]. All the logic lives here so tests can
/// drive it directly without spawning the worker thread.
pub struct PreviewShared {
    cfg: PreviewConfig,
    /// Human label for logs (e.g. `"playlist-3"`).
    label: String,
    /// Monotonic origin for the millisecond clocks below.
    origin: Instant,
    /// Last `GET .../preview.jpg` time (ms since `origin`); `0` = never.
    last_request_ms: AtomicU64,
    /// Last time a frame was accepted into the inbox (ms since `origin`).
    last_offer_ms: AtomicU64,
    /// Whether we last logged the tap as subscribed (edge-logging dedup).
    was_subscribed: AtomicBool,
    inbox: Mutex<Inbox>,
    cv: Condvar,
    latest_jpeg: Mutex<Option<Vec<u8>>>,
}

impl PreviewShared {
    fn new(cfg: PreviewConfig, label: String) -> Self {
        Self {
            cfg,
            label,
            origin: Instant::now(),
            last_request_ms: AtomicU64::new(0),
            last_offer_ms: AtomicU64::new(0),
            was_subscribed: AtomicBool::new(false),
            inbox: Mutex::new(Inbox {
                pending: None,
                busy: false,
            }),
            cv: Condvar::new(),
            latest_jpeg: Mutex::new(None),
        }
    }

    /// Milliseconds since the tap was created, offset by 1 so it is never 0:
    /// `last_request_ms` / `last_offer_ms` use 0 as their "never" sentinel,
    /// and a viewer request in the first millisecond after creation (the unit
    /// tests, or a dashboard already polling when a pipeline spawns) must
    /// still count as subscribed.
    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64 + 1
    }

    /// Record a viewer request (a `GET .../preview.jpg`). Logs the
    /// subscribe edge once.
    fn note_viewer_request(&self) {
        self.last_request_ms.store(self.now_ms(), Ordering::Relaxed);
        if !self.was_subscribed.swap(true, Ordering::Relaxed) {
            info!(label = %self.label, "preview subscribed (viewer polling)");
        }
    }

    /// Whether a viewer polled within the TTL.
    fn is_subscribed(&self) -> bool {
        let last = self.last_request_ms.load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        self.now_ms().saturating_sub(last) < self.cfg.viewer_ttl_ms
    }

    /// Log the unsubscribe edge once the viewer has gone stale. Load-first so
    /// the steady no-viewer path is a single relaxed load with no write — the
    /// `swap` (and log) fire only on the actual active→stale edge.
    fn note_unsubscribe_if_stale(&self) {
        if self.was_subscribed.load(Ordering::Relaxed)
            && !self.is_subscribed()
            && self.was_subscribed.swap(false, Ordering::Relaxed)
        {
            info!(label = %self.label, "preview unsubscribed (viewer gone)");
        }
    }

    /// Decode-thread hot path: offer one decoded NV12 frame. Returns
    /// immediately (never blocks) when there is no viewer, the ingest rate is
    /// not yet due, or the encoder is busy — see the module docs.
    pub fn offer(&self, width: u32, height: u32, stride: u32, nv12: &[u8]) {
        self.note_unsubscribe_if_stale();
        if !self.is_subscribed() {
            return;
        }
        let now = self.now_ms();
        let last = self.last_offer_ms.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < self.cfg.min_ingest_interval_ms {
            return;
        }
        // Cheap busy/lock early-out BEFORE spending on the downscale — if the
        // worker holds the lock or is mid-encode, drop this frame. `try_lock`
        // NEVER blocks the decode thread.
        {
            match self.inbox.try_lock() {
                Ok(g) if !g.busy => {}
                _ => {
                    debug!(label = %self.label, "preview: dropped frame (encoder busy)");
                    return;
                }
            }
        }
        // Downscale off-lock on the decode thread (nearest-neighbour, no DCT).
        let frame = match downscale_nv12_to_rgb(width, height, stride, nv12, self.cfg.max_width) {
            Some(f) => f,
            None => return,
        };
        let mut ib = match self.inbox.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if ib.busy {
            return;
        }
        ib.pending = Some(frame); // latest-wins
        drop(ib);
        self.last_offer_ms.store(now, Ordering::Relaxed);
        self.cv.notify_one();
    }

    /// Worker body: take the pending frame (if any), encode it off-lock, and
    /// publish `latest_jpeg`. Returns `true` iff a frame was encoded. Factored
    /// out so tests drive it deterministically without the worker thread.
    pub fn encode_pending_once(&self) -> bool {
        let frame = {
            let mut ib = match self.inbox.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            match ib.pending.take() {
                Some(f) => {
                    ib.busy = true;
                    f
                }
                None => return false,
            }
        };
        let encoded = encode_jpeg_rgb(&frame, self.cfg.jpeg_quality);
        {
            let mut ib = match self.inbox.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            ib.busy = false;
        }
        match encoded {
            Ok(bytes) => {
                if let Ok(mut slot) = self.latest_jpeg.lock() {
                    *slot = Some(bytes);
                }
                true
            }
            Err(e) => {
                debug!(label = %self.label, error = %e, "preview: JPEG encode failed");
                true
            }
        }
    }

    /// Current published JPEG bytes, if any.
    pub fn latest_jpeg(&self) -> Option<Vec<u8>> {
        self.latest_jpeg.lock().ok().and_then(|s| s.clone())
    }
}

/// Cheap, cloneable handle to a per-pipeline preview tap.
#[derive(Clone)]
pub struct PreviewTap {
    shared: Arc<PreviewShared>,
}

impl PreviewTap {
    /// Create a tap and spawn its background encoder worker thread.
    pub fn new(cfg: PreviewConfig, label: String) -> Self {
        let shared = Arc::new(PreviewShared::new(cfg, label));
        spawn_worker(shared.clone());
        Self { shared }
    }

    /// Decode-thread hot path — see [`PreviewShared::offer`].
    pub fn try_offer(&self, width: u32, height: u32, stride: u32, nv12: &[u8]) {
        self.shared.offer(width, height, stride, nv12);
    }

    /// Route: record a viewer request (a `GET .../preview.jpg`).
    pub fn note_viewer_request(&self) {
        self.shared.note_viewer_request();
    }

    /// Route: current published JPEG bytes, if any.
    pub fn latest_jpeg(&self) -> Option<Vec<u8>> {
        self.shared.latest_jpeg()
    }

    /// Test-only: publish JPEG bytes directly, so a route-layer test can
    /// exercise the HTTP contract without depending on the async worker
    /// thread's timing (the real offer→downscale→encode path is covered by
    /// this module's own unit tests).
    #[cfg(test)]
    pub(crate) fn set_latest_jpeg_for_test(&self, bytes: Vec<u8>) {
        if let Ok(mut slot) = self.shared.latest_jpeg.lock() {
            *slot = Some(bytes);
        }
    }
}

/// Spawn the low-priority background encoder worker. It blocks on the condvar
/// while nothing is pending, so an idle tap costs no CPU.
fn spawn_worker(shared: Arc<PreviewShared>) {
    let _ = std::thread::Builder::new()
        .name("preview-encoder".into())
        .spawn(move || {
            loop {
                {
                    let mut ib = match shared.inbox.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    while ib.pending.is_none() {
                        ib = match shared.cv.wait(ib) {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                    }
                }
                shared.encode_pending_once();
            }
        });
}

/// Nearest-neighbour downscale an NV12 frame to a small packed-RGB frame.
///
/// NV12 layout: a `stride`-byte-wide Y plane of `height` rows, immediately
/// followed by an interleaved UV plane of `height / 2` rows (also `stride`
/// bytes wide, U and V bytes alternating). The output preserves the source
/// aspect ratio and never upscales (output width is `min(max_width, width)`).
/// Returns `None` for degenerate dimensions or a too-short buffer (the frame
/// is simply dropped from the preview — never a decode error).
pub fn downscale_nv12_to_rgb(
    width: u32,
    height: u32,
    stride: u32,
    nv12: &[u8],
    max_width: u32,
) -> Option<RawPreviewFrame> {
    if width == 0 || height == 0 || stride < width || max_width == 0 {
        return None;
    }
    let stride = stride as usize;
    let sw = width as usize;
    let sh = height as usize;
    let y_size = stride.checked_mul(sh)?;
    let uv_rows = sh / 2;
    let uv_size = stride.checked_mul(uv_rows)?;
    let need = y_size.checked_add(uv_size)?;
    if nv12.len() < need {
        return None;
    }

    let out_w = max_width.min(width) as usize;
    // Preserve aspect ratio, at least 1px tall.
    let out_h = (((sh * out_w) + sw / 2) / sw).max(1);

    let mut rgb = vec![0u8; out_w * out_h * 3];
    for oy in 0..out_h {
        let sy = (oy * sh) / out_h;
        let y_row = sy * stride;
        let uv_row = y_size + (sy / 2) * stride;
        for ox in 0..out_w {
            let sx = (ox * sw) / out_w;
            let y = nv12[y_row + sx] as i32;
            // Chroma is sub-sampled 2x2; the U/V pair sits at even column
            // (sx & !1) within the UV row.
            let uv_col = (sx & !1) + uv_row;
            let (u, v) = if uv_col + 1 < nv12.len() {
                (nv12[uv_col] as i32, nv12[uv_col + 1] as i32)
            } else {
                (128, 128)
            };
            let (r, g, b) = yuv_to_rgb(y, u, v);
            let o = (oy * out_w + ox) * 3;
            rgb[o] = r;
            rgb[o + 1] = g;
            rgb[o + 2] = b;
        }
    }
    Some(RawPreviewFrame {
        width: out_w as u32,
        height: out_h as u32,
        rgb,
    })
}

/// BT.601 limited-range YUV → RGB (integer approximation). Good enough for a
/// small monitoring preview.
#[inline]
fn yuv_to_rgb(y: i32, u: i32, v: i32) -> (u8, u8, u8) {
    let c = y - 16;
    let d = u - 128;
    let e = v - 128;
    let r = (298 * c + 409 * e + 128) >> 8;
    let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
    let b = (298 * c + 516 * d + 128) >> 8;
    (
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
    )
}

/// Encode a packed-RGB frame to JPEG bytes.
pub fn encode_jpeg_rgb(frame: &RawPreviewFrame, quality: u8) -> Result<Vec<u8>, String> {
    use jpeg_encoder::{ColorType, Encoder};
    let mut buf = Vec::new();
    let encoder = Encoder::new(&mut buf, quality);
    encoder
        .encode(
            &frame.rgb,
            frame.width as u16,
            frame.height as u16,
            ColorType::Rgb,
        )
        .map_err(|e| e.to_string())?;
    Ok(buf)
}

/// Shared registry of per-playlist preview taps. Mirrors
/// [`super::ndi_burn::NdiBurnRegistry`]: created once in `lib.rs::start`,
/// shared with `AppState` (route) and the playback engine (spawn).
pub struct PreviewRegistry {
    cfg: PreviewConfig,
    taps: RwLock<HashMap<i64, PreviewTap>>,
}

impl PreviewRegistry {
    pub fn new() -> Self {
        Self::with_config(PreviewConfig::default())
    }

    pub fn with_config(cfg: PreviewConfig) -> Self {
        Self {
            cfg,
            taps: RwLock::new(HashMap::new()),
        }
    }

    /// Register (or reuse) the tap for `playlist_id`. Idempotent — a
    /// re-register (e.g. a re-spawned pipeline) returns the existing tap
    /// rather than leaking a second worker thread.
    pub fn register(&self, playlist_id: i64) -> PreviewTap {
        if let Some(tap) = self
            .taps
            .read()
            .ok()
            .and_then(|m| m.get(&playlist_id).cloned())
        {
            return tap;
        }
        let tap = PreviewTap::new(self.cfg, format!("playlist-{playlist_id}"));
        if let Ok(mut map) = self.taps.write() {
            // Double-checked: another thread may have inserted meanwhile.
            return map.entry(playlist_id).or_insert(tap).clone();
        }
        tap
    }

    /// Look up the tap for `playlist_id` (route side).
    pub fn get(&self, playlist_id: i64) -> Option<PreviewTap> {
        self.taps
            .read()
            .ok()
            .and_then(|m| m.get(&playlist_id).cloned())
    }
}

impl Default for PreviewRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a solid-grey NV12 frame (`width`×`height`, `stride`≥`width`).
    fn grey_nv12(_width: usize, height: usize, stride: usize) -> Vec<u8> {
        let y_size = stride * height;
        let uv_size = stride * (height / 2);
        let mut v = vec![0u8; y_size + uv_size];
        for b in v.iter_mut().take(y_size) {
            *b = 128; // mid Y
        }
        for b in v.iter_mut().skip(y_size) {
            *b = 128; // neutral chroma
        }
        v
    }

    fn shared(cfg: PreviewConfig) -> PreviewShared {
        PreviewShared::new(cfg, "test".into())
    }

    #[test]
    fn downscale_preserves_aspect_and_never_upscales() {
        let nv12 = grey_nv12(1920, 1080, 1920);
        let f = downscale_nv12_to_rgb(1920, 1080, 1920, &nv12, 320).unwrap();
        assert_eq!((f.width, f.height), (320, 180), "16:9 downscale geometry");
        assert_eq!(f.rgb.len(), 320 * 180 * 3);

        // max_width larger than source: no upscale.
        let small = grey_nv12(4, 2, 4);
        let f2 = downscale_nv12_to_rgb(4, 2, 4, &small, 320).unwrap();
        assert_eq!((f2.width, f2.height), (4, 2));
    }

    #[test]
    fn downscale_honours_stride_padding() {
        // stride > width — the extra padding bytes must not corrupt geometry.
        let nv12 = grey_nv12(6, 4, 8);
        let f = downscale_nv12_to_rgb(6, 4, 8, &nv12, 3).unwrap();
        assert_eq!((f.width, f.height), (3, 2));
        assert_eq!(f.rgb.len(), 3 * 2 * 3);
    }

    #[test]
    fn downscale_rejects_degenerate_and_short_buffers() {
        assert!(downscale_nv12_to_rgb(0, 10, 10, &[0; 200], 320).is_none());
        assert!(downscale_nv12_to_rgb(10, 0, 10, &[0; 200], 320).is_none());
        assert!(
            downscale_nv12_to_rgb(10, 10, 8, &[0; 200], 320).is_none(),
            "stride<width"
        );
        // Buffer far too short for the declared dims.
        assert!(downscale_nv12_to_rgb(1920, 1080, 1920, &[0; 10], 320).is_none());
    }

    #[test]
    fn encode_produces_valid_jpeg_bytes() {
        let f = RawPreviewFrame {
            width: 8,
            height: 4,
            rgb: vec![120u8; 8 * 4 * 3],
        };
        let jpeg = encode_jpeg_rgb(&f, 60).unwrap();
        // SOI marker + non-trivial length.
        assert!(jpeg.len() > 100, "jpeg should be non-trivial");
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8], "JPEG SOI magic");
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9], "JPEG EOI magic");
    }

    #[test]
    fn no_encode_when_unsubscribed() {
        let s = shared(PreviewConfig::default());
        let nv12 = grey_nv12(64, 64, 64);
        // No viewer noted → offer is a no-op, nothing queued.
        s.offer(64, 64, 64, &nv12);
        assert!(
            s.inbox.lock().unwrap().pending.is_none(),
            "unsubscribed offer must not queue a frame"
        );
        assert!(!s.encode_pending_once(), "nothing to encode");
        assert!(s.latest_jpeg().is_none());
    }

    #[test]
    fn subscribed_offer_queues_then_encodes() {
        let s = shared(PreviewConfig::default());
        let nv12 = grey_nv12(64, 64, 64);
        s.note_viewer_request();
        s.offer(64, 64, 64, &nv12);
        assert!(
            s.inbox.lock().unwrap().pending.is_some(),
            "subscribed offer must queue a frame"
        );
        assert!(s.encode_pending_once(), "a queued frame is encoded");
        let jpeg = s.latest_jpeg().expect("jpeg published");
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
        // Inbox drained + not busy after encode.
        let ib = s.inbox.lock().unwrap();
        assert!(ib.pending.is_none());
        assert!(!ib.busy);
    }

    #[test]
    fn offer_keeps_only_the_latest_frame() {
        // Zero ingest interval so both offers pass the rate gate.
        let cfg = PreviewConfig {
            min_ingest_interval_ms: 0,
            ..PreviewConfig::default()
        };
        let s = shared(cfg);
        s.note_viewer_request();
        let a = grey_nv12(4, 2, 4);
        let mut b = grey_nv12(6, 2, 6); // different dims → distinguishable
        b[0] = 200;
        s.offer(4, 2, 4, &a);
        s.offer(6, 2, 6, &b);
        let pending = s.inbox.lock().unwrap().pending.clone().unwrap();
        assert_eq!(pending.width, 6, "the later offer wins (latest-wins)");
    }

    #[test]
    fn offer_returns_immediately_and_drops_when_encoder_busy() {
        let cfg = PreviewConfig {
            min_ingest_interval_ms: 0,
            ..PreviewConfig::default()
        };
        let s = shared(cfg);
        s.note_viewer_request();
        // Simulate the worker mid-encode.
        s.inbox.lock().unwrap().busy = true;
        let nv12 = grey_nv12(64, 64, 64);
        s.offer(64, 64, 64, &nv12); // must return without blocking
        let ib = s.inbox.lock().unwrap();
        assert!(
            ib.pending.is_none(),
            "a frame offered while the encoder is busy is dropped, not queued"
        );
        assert!(ib.busy, "busy flag untouched by the dropped offer");
    }

    #[test]
    fn ingest_rate_limits_within_the_interval() {
        let cfg = PreviewConfig {
            min_ingest_interval_ms: 10_000, // effectively "once"
            ..PreviewConfig::default()
        };
        let s = shared(cfg);
        s.note_viewer_request();
        let nv12 = grey_nv12(4, 2, 4);
        s.offer(4, 2, 4, &nv12);
        assert!(s.inbox.lock().unwrap().pending.is_some());
        // Drain it, then a second immediate offer is rate-gated out.
        s.inbox.lock().unwrap().pending = None;
        s.offer(4, 2, 4, &nv12);
        assert!(
            s.inbox.lock().unwrap().pending.is_none(),
            "a second offer within the ingest interval is dropped"
        );
    }

    #[test]
    fn registry_register_is_idempotent_and_get_finds_it() {
        let reg = PreviewRegistry::new();
        let a = reg.register(7);
        let b = reg.register(7);
        // Same underlying shared state (same worker), not a second tap.
        assert!(Arc::ptr_eq(&a.shared, &b.shared));
        assert!(reg.get(7).is_some());
        assert!(reg.get(999).is_none());
    }
}
