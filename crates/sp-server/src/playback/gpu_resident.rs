//! #279 P3a: GPU-resident decode in sp-server: the precondition, each
//! pipeline's producer lease, and the process-wide table of the pipelines
//! that decode on the GPU. Design: #279 comment 6244390810 (main session),
//! plan `docs/superpowers/plans/2026-10-16-gpu-video-path.md` §5.1-§5.3.
//!
//! - **The precondition** ([`Resident::of`]): `video_decode` = `gpu` takes
//!   effect only while NDI `SP-program` comes from the GPU
//!   (`program_ndi_source` = `gpu`), because the CPU canvas shows a GPU
//!   picture as black (`Picture::cpu_for`'s miss). Otherwise a song opens
//!   in today's mode (`video_hw_decode`'s), and
//!   `status.video_decode.gpu_resident_blocked` says why
//!   ([`BLOCKED_NDI_CPU`]); each change of that state is logged once
//!   ([`resident_log`]: a WARN into a block).
//! - **The producer lease** ([`Lease`], [`PipelineGpu`]): one producer
//!   device and ring per pipeline (`sp_gpu::ProducerGpu`), made at the
//!   pipeline's first GPU-resident open, kept across its songs, let go after
//!   [`PRODUCER_IDLE_RELEASE`] (10 min) with no such open, and when the
//!   pipeline ends (it lives in the pipeline's `PipelineOutput`). A producer
//!   whose ring died (its device was lost, or a slot's key abandoned) is
//!   dropped at the next open, and a new one is made only
//!   [`PRODUCER_REBUILD_BACKOFF`] (30 s) later; a failed build waits the
//!   same. Meanwhile the songs open in the base mode. The song whose copy
//!   met the loss goes on in software by the reader's own fall back.
//! - **The table** ([`pipelines`]): per playlist, whether its pipeline
//!   decodes its song on the GPU (the decode producer reports each change of
//!   a frame's kind, [`note_song_path`]) and its producer's ring:
//!   `status.video_decode.pipelines` and the `/api/v1/ndi/health` rows'
//!   `gpu_decoded` (the Player's preview slot) read it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sp_core::config::{NdiSource, VideoDecode};
use sp_decoder::DecodeMode;
use sp_gpu::{ProducerGpu, RingStats};
use tracing::{debug, info, warn};

use crate::playback::video_decode::HwDecodeSetting;

/// `status.video_decode.gpu_resident_blocked` while `video_decode` = `gpu`
/// waits for NDI `SP-program` to come from the GPU.
pub const BLOCKED_NDI_CPU: &str = "ndi_source_cpu";

/// A pipeline's producer is let go after this long with no GPU-resident
/// open.
pub const PRODUCER_IDLE_RELEASE: Duration = Duration::from_secs(600);

/// After a lost producer device or a failed build, no new producer before
/// this.
pub const PRODUCER_REBUILD_BACKOFF: Duration = Duration::from_secs(30);

/// Whether a song opened now decodes GPU-resident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resident {
    /// `video_decode` = `software`.
    Off,
    /// `video_decode` = `gpu` and NDI from the GPU.
    On,
    /// `video_decode` = `gpu`, but blocked (why).
    Blocked(&'static str),
}

impl Resident {
    /// The precondition: `gpu` takes effect only while NDI `SP-program`
    /// comes from the GPU readback.
    pub fn of(decode: VideoDecode, ndi: NdiSource) -> Self {
        match (decode, ndi) {
            (VideoDecode::Software, _) => Resident::Off,
            (VideoDecode::Gpu, NdiSource::Gpu) => Resident::On,
            (VideoDecode::Gpu, NdiSource::Cpu) => Resident::Blocked(BLOCKED_NDI_CPU),
        }
    }

    /// Why `gpu` does not take effect, if it is blocked.
    pub fn blocked(self) -> Option<&'static str> {
        match self {
            Resident::Blocked(why) => Some(why),
            Resident::Off | Resident::On => None,
        }
    }
}

/// What a change of the precondition logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidentLog {
    /// No change.
    Quiet,
    /// It went (or stays, for another reason) blocked: a WARN with why.
    Warn(&'static str),
    /// It went on or off: an INFO.
    Info,
}

/// The line a change from `before` to `after` logs: one per change.
pub fn resident_log(before: Resident, after: Resident) -> ResidentLog {
    if before == after {
        return ResidentLog::Quiet;
    }
    match after {
        Resident::Blocked(why) => ResidentLog::Warn(why),
        Resident::Off | Resident::On => ResidentLog::Info,
    }
}

/// Log a change of the precondition ([`resident_log`]). Glue: the decision
/// is tested, the lines are the logger's.
#[cfg_attr(test, mutants::skip)]
pub fn log_resident(change: ResidentLog, after: Resident) {
    match change {
        ResidentLog::Quiet => {}
        ResidentLog::Warn(why) => warn!(
            blocked = why,
            "video decode: video_decode=gpu does not take effect - NDI SP-program comes from \
             the CPU canvas, which shows a GPU picture as black; the songs decode as before \
             (#279 P3a)"
        ),
        ResidentLog::Info => info!(
            resident = ?after,
            "video decode: GPU-resident decode is now {} (from the next song, #279 P3a)",
            if after == Resident::On { "on" } else { "off" }
        ),
    }
}

/// What a pipeline's lease needs of its producer.
pub trait Producer: Send + Sync + 'static {
    /// Whether its ring is dead (its device was lost, or a slot's key
    /// abandoned).
    fn lost(&self) -> bool;
    /// Its ring's counters.
    fn ring(&self) -> RingStats;
}

impl Producer for ProducerGpu {
    /// Glue: `ProducerGpu`'s own (no producer exists off Windows).
    #[cfg_attr(test, mutants::skip)]
    fn lost(&self) -> bool {
        self.ring_dead()
    }

    /// Glue, as `lost`.
    #[cfg_attr(test, mutants::skip)]
    fn ring(&self) -> RingStats {
        self.stats()
    }
}

/// What a song's open got of its pipeline's lease.
#[derive(Debug)]
pub enum Leased<T> {
    /// The producer kept from an earlier song.
    Kept(Arc<T>),
    /// A producer made now.
    Made(Arc<T>),
    /// No producer: the backoff after a lost device or a failed build runs.
    Waiting { retry_in: Duration },
    /// The producer's device was lost: dropped; a new one after the
    /// backoff.
    Lost,
    /// The build failed (why); tried again after the backoff.
    Failed(String),
}

impl<T> Leased<T> {
    /// The producer the song decodes on, if it got one.
    pub fn producer(&self) -> Option<&Arc<T>> {
        match self {
            Leased::Kept(producer) | Leased::Made(producer) => Some(producer),
            Leased::Waiting { .. } | Leased::Lost | Leased::Failed(_) => None,
        }
    }
}

/// A pipeline's producer: held across its songs, let go when idle, made
/// again after a backoff when its device was lost or its build failed.
#[derive(Debug)]
pub struct Lease<T> {
    held: Option<Arc<T>>,
    /// The last GPU-resident open on the held producer.
    last_open: Option<Instant>,
    /// No build before this.
    retry_at: Option<Instant>,
}

impl<T> Default for Lease<T> {
    fn default() -> Self {
        Self {
            held: None,
            last_open: None,
            retry_at: None,
        }
    }
}

impl<T: Producer> Lease<T> {
    /// The producer for a GPU-resident open at `now`: the held one while its
    /// ring lives; a dead one is dropped (`Lost`, the backoff starts); with
    /// none held, `make` builds one unless the backoff runs (`Waiting`); a
    /// failed build starts the backoff (`Failed`).
    pub fn take(&mut self, now: Instant, make: impl FnOnce() -> Result<T, String>) -> Leased<T> {
        if let Some(held) = &self.held {
            if !held.lost() {
                self.last_open = Some(now);
                return Leased::Kept(Arc::clone(held));
            }
            self.held = None;
            self.retry_at = Some(now + PRODUCER_REBUILD_BACKOFF);
            return Leased::Lost;
        }
        if let Some(at) = self.retry_at
            && now < at
        {
            return Leased::Waiting {
                retry_in: at.saturating_duration_since(now),
            };
        }
        match make() {
            Ok(producer) => {
                let producer = Arc::new(producer);
                self.held = Some(Arc::clone(&producer));
                self.last_open = Some(now);
                self.retry_at = None;
                Leased::Made(producer)
            }
            Err(why) => {
                self.retry_at = Some(now + PRODUCER_REBUILD_BACKOFF);
                Leased::Failed(why)
            }
        }
    }

    /// Let go of the held producer once [`PRODUCER_IDLE_RELEASE`] passed
    /// since its last open; whether it did.
    pub fn release_idle(&mut self, now: Instant) -> bool {
        let idle = self
            .last_open
            .is_some_and(|at| now.saturating_duration_since(at) >= PRODUCER_IDLE_RELEASE);
        if idle && self.held.is_some() {
            self.held = None;
            return true;
        }
        false
    }

    /// The producer held now.
    pub fn held(&self) -> Option<&Arc<T>> {
        self.held.as_ref()
    }
}

/// A pipeline's GPU-resident side: its lease, and its row in the table
/// (forgotten when the pipeline ends: this lives in its `PipelineOutput`).
#[derive(Debug)]
pub struct PipelineGpu {
    playlist_id: i64,
    lease: Lease<ProducerGpu>,
}

impl PipelineGpu {
    /// Playlist `playlist_id`'s, with no producer yet.
    pub fn new(playlist_id: i64) -> Self {
        Self {
            playlist_id,
            lease: Lease::default(),
        }
    }

    /// The mode the pipeline's next song opens in, from `setting` (the
    /// process's, read at each song's open): GPU-resident on the lease's
    /// producer while the precondition holds and the lease gives one, else
    /// `video_hw_decode`'s. Glue: the production producer
    /// (`ProducerGpu::new`, which cannot be built off Windows) over
    /// [`decode_mode_with`](Self::decode_mode_with).
    #[cfg_attr(test, mutants::skip)]
    pub fn decode_mode(&mut self, setting: &HwDecodeSetting, now: Instant) -> DecodeMode {
        self.decode_mode_with(setting, now, || {
            ProducerGpu::new().map_err(|e| e.to_string())
        })
    }

    /// [`decode_mode`](Self::decode_mode) with the producer made by `make`.
    pub fn decode_mode_with(
        &mut self,
        setting: &HwDecodeSetting,
        now: Instant,
        make: impl FnOnce() -> Result<ProducerGpu, String>,
    ) -> DecodeMode {
        let base = setting.mode();
        if setting.resident() != Resident::On {
            return base;
        }
        let leased = self.lease.take(now, make);
        log_leased(self.playlist_id, &leased);
        pipelines().set_producer(self.playlist_id, self.lease.held());
        match leased.producer() {
            Some(producer) => DecodeMode::GpuResident(Arc::clone(producer)),
            None => base,
        }
    }

    /// Let go of the producer when it was idle long enough
    /// ([`Lease::release_idle`]); the idle fill asks at its heartbeats.
    pub fn release_idle(&mut self, now: Instant) {
        if self.lease.release_idle(now) {
            info!(
                playlist_id = self.playlist_id,
                "gpu resident: the producer device let go - no GPU-resident song for 10 min \
                 (#279 P3a)"
            );
            pipelines().set_producer::<ProducerGpu>(self.playlist_id, None);
        }
    }
}

impl Drop for PipelineGpu {
    fn drop(&mut self) {
        pipelines().forget(self.playlist_id);
    }
}

/// Log what a song's open got of its lease. Glue: the decision is
/// [`Lease::take`]'s.
#[cfg_attr(test, mutants::skip)]
fn log_leased<T: std::fmt::Debug>(playlist_id: i64, leased: &Leased<T>) {
    match leased {
        Leased::Kept(_) => {}
        Leased::Made(producer) => info!(
            playlist_id,
            producer = ?producer,
            "gpu resident: the pipeline's producer device made (#279 P3a)"
        ),
        Leased::Waiting { retry_in } => debug!(
            playlist_id,
            retry_in_s = retry_in.as_secs(),
            "gpu resident: no producer device yet - this song decodes as before"
        ),
        Leased::Lost => warn!(
            playlist_id,
            "gpu resident: the producer device was lost - this song decodes as before, a new \
             device in 30 s (#279 P3a)"
        ),
        Leased::Failed(why) => warn!(
            playlist_id,
            %why,
            "gpu resident: the producer device could not be made - this song decodes as \
             before, tried again in 30 s (#279 P3a)"
        ),
    }
}

/// A ring's counters on `status.video_decode.pipelines[].ring`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RingStatus {
    /// Slots that hold a texture.
    pub slots: usize,
    /// Slots a frame refers to.
    pub in_use: usize,
    /// Copies that found every slot in use and waited.
    pub waits: u64,
    /// Copies that waited the whole bound and failed.
    pub full: u64,
    /// The ring is dead (its pipeline's songs decode as before).
    pub dead: bool,
    /// The last copies' p99, µs.
    pub copy_us_p99: Option<u64>,
    /// The slots' VRAM, MB (10^6 bytes, rounded).
    pub vram_mb: u64,
}

impl RingStatus {
    /// The status of `stats`.
    pub fn of(stats: RingStats) -> Self {
        Self {
            slots: stats.slots,
            in_use: stats.in_use,
            waits: stats.waits,
            full: stats.full,
            dead: stats.dead,
            copy_us_p99: stats.copy_us_p99,
            vram_mb: (stats.vram_bytes + 500_000) / 1_000_000,
        }
    }
}

/// One pipeline on `status.video_decode.pipelines`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PipelineDecode {
    pub playlist_id: i64,
    /// Its song's last decoded picture lay on the GPU.
    pub gpu_decoding: bool,
    /// Its producer's ring, while it holds one.
    pub ring: Option<RingStatus>,
}

/// A pipeline's row in the table.
#[derive(Default)]
struct Row {
    gpu_decoding: bool,
    producer: Option<Weak<dyn Producer>>,
}

/// The pipelines that decode on the GPU or hold a producer, by playlist.
pub struct Pipelines(Mutex<BTreeMap<i64, Row>>);

static PIPELINES: Pipelines = Pipelines(Mutex::new(BTreeMap::new()));

/// The process's table.
pub fn pipelines() -> &'static Pipelines {
    &PIPELINES
}

impl Pipelines {
    fn rows(&self) -> MutexGuard<'_, BTreeMap<i64, Row>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Playlist `playlist_id`'s song now decodes on the GPU (`true`) or not.
    pub fn set_gpu_decoding(&self, playlist_id: i64, gpu: bool) {
        self.rows().entry(playlist_id).or_default().gpu_decoding = gpu;
    }

    /// Playlist `playlist_id`'s producer is `producer` now (`None`: none).
    pub fn set_producer<T: Producer>(&self, playlist_id: i64, producer: Option<&Arc<T>>) {
        let weak = producer.map(|p| {
            let shared: Arc<dyn Producer> = Arc::clone(p) as Arc<dyn Producer>;
            Arc::downgrade(&shared)
        });
        self.rows().entry(playlist_id).or_default().producer = weak;
    }

    /// Playlist `playlist_id`'s pipeline ended.
    pub fn forget(&self, playlist_id: i64) {
        self.rows().remove(&playlist_id);
    }

    /// Whether playlist `playlist_id`'s song decodes on the GPU.
    pub fn gpu_decoding(&self, playlist_id: i64) -> bool {
        self.rows()
            .get(&playlist_id)
            .is_some_and(|row| row.gpu_decoding)
    }

    /// Every pipeline that decodes on the GPU or holds a live producer, by
    /// playlist id, with its ring's counters (read after the table's lock).
    pub fn status(&self) -> Vec<PipelineDecode> {
        let rows: Vec<(i64, bool, Option<Arc<dyn Producer>>)> = self
            .rows()
            .iter()
            .map(|(&id, row)| {
                let live = row.producer.as_ref().and_then(Weak::upgrade);
                (id, row.gpu_decoding, live)
            })
            .collect();
        rows.into_iter()
            .filter(|(_, gpu, live)| *gpu || live.is_some())
            .map(|(playlist_id, gpu_decoding, live)| PipelineDecode {
                playlist_id,
                gpu_decoding,
                ring: live.map(|p| RingStatus::of(p.ring())),
            })
            .collect()
    }
}

/// The decode producer's report of a song's frame kind (#279 P3a,
/// `DecodeProducer::take_kind_change`): its first picture's (`first`), or a
/// change of it mid-song (a fall back to software). Writes the table and
/// logs one INFO per song (the path, the codec, the size, the ring), a WARN
/// for a change mid-song.
pub fn note_song_path(
    playlist_id: i64,
    gpu: bool,
    first: bool,
    codec: Option<&str>,
    (width, height): (u32, u32),
    ring: Option<RingStats>,
) {
    pipelines().set_gpu_decoding(playlist_id, gpu);
    let path = if gpu { "gpu_resident" } else { "cpu" };
    if first {
        info!(
            playlist_id,
            path,
            codec = codec.unwrap_or("?"),
            width,
            height,
            ring = ?ring,
            "paced producer: the song's pictures (#279 P3a: gpu_resident = they stay on the GPU)"
        );
    } else {
        warn!(
            playlist_id,
            path,
            codec = codec.unwrap_or("?"),
            ring = ?ring,
            "paced producer: the song's pictures changed kind mid-song (a fall back to software)"
        );
    }
}

#[cfg(test)]
#[path = "gpu_resident_tests.rs"]
mod tests;
