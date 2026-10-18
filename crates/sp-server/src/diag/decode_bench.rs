//! `decode-bench` (#223 S0): what one picture costs SongPlayer's REAL video
//! decoder. The main session measures a 4K AV1 / VP9 sample on the box
//! against revision 2's D2 budget (mean ≤ 50 % of 1/f) before any 4K
//! download is enabled.
//!
//! `POST /api/v1/diag/decode-bench {"file": "<name>", "seconds": 1..=15}`
//! (`api::diag`) decodes `<data dir>/bench/<name>` through
//! `sp_decoder::MediaFoundationVideoReader`, the reader the paced producer
//! uses. The run has its own thread, started like the producer's
//! (`playback::decode_thread`), is unpaced (as fast as it decodes), and
//! stops after `seconds` of wall time or at the end of the stream.
//!
//! #223 S3b: an optional `"hw": true` opens the reader in `Hardware` mode
//! (Media Foundation's decoder on the GPU), so the box measures hardware
//! against software on the same samples; the report says which path really
//! decoded ([`DecodeFacts`]).
//!
//! #279 P3a: an optional `"resident": true` opens it GPU-resident instead,
//! on a producer device of the run's own (`sp_gpu::ProducerGpu`, made on
//! the production adapter like a pipeline's): every GPU-decoded picture is
//! copied into the producer's ring and never comes to RAM, so the timings
//! are what a GPU-resident song pays (the decode and the ring copy). The
//! report adds `gpu_resident` and the ring after the run ([`BenchMode`],
//! [`decode_mode`]).
//!
//! This file is the cross-platform part, Linux-tested:
//!
//! - [`DecodeBench`]: the sample dir and the one-run-at-a-time gate;
//! - [`bench_file_name`] and [`bench_seconds`]: the request's checks;
//! - [`measure`]: the timing loop, generic over [`VideoStream`];
//! - [`BenchReport`]: the stats ([`DecodeUs`]) and D2's gate ([`Budget`]).
//!
//! The Windows half (open the reader, log, the thread body) is
//! `decode_bench_mf.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use sp_decoder::{DecodeMode, DecodePath, VideoStream};
use sp_gpu::{GpuError, ProducerGpu, RingStats};

use crate::playback::decode_thread::spawn_decode_thread;
use crate::playback::gpu_resident::RingStatus;
use crate::playback::loop_stats::percentile_ceil;

/// The longest run a request may ask for, in seconds.
pub const MAX_SECONDS: u64 = 15;

/// The longest file name a request may give, in bytes.
pub const MAX_NAME_BYTES: usize = 255;

/// The sample dir, and the gate that lets one run at a time.
#[derive(Debug)]
pub struct DecodeBench {
    dir: PathBuf,
    busy: Arc<AtomicBool>,
}

/// Why a request names no file the bench can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BenchFileError {
    /// Not a bare file name (400). The text says which rule it broke.
    BadName(&'static str),
    /// No such file in the sample dir (404).
    Missing(PathBuf),
}

impl DecodeBench {
    /// A bench whose samples are in `dir`: `<data dir>/bench`
    /// (`ServerConfig::data_dir`), which on the box is
    /// `C:\ProgramData\SongPlayer\bench`. It is outside the media cache, so
    /// startup's cache self-heal never touches a sample.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            busy: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The sample dir.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The sample a request names, or why there is none to run.
    pub fn resolve(&self, name: &str) -> Result<PathBuf, BenchFileError> {
        let name = bench_file_name(name).map_err(BenchFileError::BadName)?;
        let path = self.dir.join(name);
        if path.is_file() {
            Ok(path)
        } else {
            Err(BenchFileError::Missing(path))
        }
    }

    /// Take the bench for one run, or `None` while another run holds it.
    pub fn try_start(&self) -> Option<BenchSlot> {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(BenchSlot {
            busy: Arc::clone(&self.busy),
        })
    }
}

/// One run's hold on the bench. Dropping it frees the bench. The run's
/// thread owns it, so the bench stays taken until the decoding stops, even
/// when the HTTP caller has gone away, and is freed on a panic too.
#[derive(Debug)]
pub struct BenchSlot {
    busy: Arc<AtomicBool>,
}

impl Drop for BenchSlot {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
    }
}

/// `name` if it is a bare file name, else which rule it breaks. A bare
/// name is 1 to [`MAX_NAME_BYTES`] bytes, has no `/`, `\`, `:` (a drive or
/// an NTFS stream) or control character, and no `..`, so it cannot reach
/// outside the sample dir. It must also name a FILE on Windows: no device
/// name ([`is_device_name`]) and no trailing `.` or space, which Windows
/// strips (`x.mp4.` opens `x.mp4`; `.` is the dir itself). What passes is
/// one plain path component on both platforms (the tests check it).
pub fn bench_file_name(name: &str) -> Result<&str, &'static str> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err("file must be a file name of 1 to 255 bytes");
    }
    if name.contains("..") {
        return Err("file must not contain \"..\"");
    }
    if name
        .chars()
        .any(|c| matches!(c, '/' | '\\' | ':') || c.is_control())
    {
        return Err("file must be a bare file name: no /, \\, : or control characters");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err("file must not end with a dot or a space");
    }
    if is_device_name(name) {
        return Err("file must not be a Windows device name (CON, NUL, COM1, …)");
    }
    Ok(name)
}

/// Whether Windows reads `name` as a device, not a file: `CON`, `PRN`,
/// `AUX`, `NUL`, `CONIN$`, `CONOUT$`, `COM0`–`COM9` and `LPT0`–`LPT9`
/// (with the superscript `¹²³` forms), in any case, also with an extension
/// (`nul.mp4`) or spaces before it. `dir\NUL` is the null device wherever
/// `dir` is.
pub fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default();
    let stem = stem.trim_end_matches(' ').to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    match stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
    {
        Some(port) => matches!(
            port,
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        ),
        None => false,
    }
}

/// A request's `seconds` as the run's wall-time bound: 1 to [`MAX_SECONDS`].
pub fn bench_seconds(seconds: u64) -> Result<Duration, &'static str> {
    if seconds == 0 || seconds > MAX_SECONDS {
        return Err("seconds must be 1 to 15");
    }
    Ok(Duration::from_secs(seconds))
}

/// Why a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchEnd {
    /// The stream had no more pictures.
    EndOfStream,
    /// The wall-time bound was reached.
    TimeLimit,
    /// The decoder failed, opening the file or decoding it. `error` says how.
    Error,
}

/// A picture's layout, as the reader hands it over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureLayout {
    pub width: u32,
    pub height: u32,
    /// The Y plane's row stride, in bytes.
    pub stride: u32,
}

/// What one timing loop saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchRun {
    /// The µs of each `next_frame` call that returned a picture, in order.
    pub decode_us: Vec<u64>,
    /// The first picture's layout.
    pub first_picture: Option<PictureLayout>,
    /// The run's clock when it ended, µs.
    pub wall_us: u64,
    pub end: BenchEnd,
    /// The decoder's error, when one ended the run.
    pub error: Option<String>,
}

/// Decode `stream` as fast as it decodes, until it ends, fails, or the run's
/// clock reaches `max_wall`, timing every `next_frame` call that returns a
/// picture. A call that ends or fails the run is not a sample.
///
/// `elapsed` is the run's clock, the time since the run began: the bench
/// thread passes `Instant::elapsed`, a test a scripted clock. A picture's
/// buffer goes back to the frame pool at once, as playback's last owner
/// returns it, so the next picture is copied into a recycled buffer as it
/// is in playback.
pub fn measure<S, C>(stream: &mut S, max_wall: Duration, mut elapsed: C) -> BenchRun
where
    S: VideoStream + ?Sized,
    C: FnMut() -> Duration,
{
    let mut decode_us = Vec::new();
    let mut first_picture = None;
    loop {
        let before = elapsed();
        if before >= max_wall {
            return BenchRun {
                decode_us,
                first_picture,
                wall_us: micros(before),
                end: BenchEnd::TimeLimit,
                error: None,
            };
        }
        let result = stream.next_frame();
        let after = elapsed();
        match result {
            Ok(Some(picture)) => {
                decode_us.push(micros(after.saturating_sub(before)));
                first_picture.get_or_insert(PictureLayout {
                    width: picture.width,
                    height: picture.height,
                    stride: picture.stride,
                });
                sp_decoder::frame_pool::recycle(picture.data);
            }
            Ok(None) => {
                return BenchRun {
                    decode_us,
                    first_picture,
                    wall_us: micros(after),
                    end: BenchEnd::EndOfStream,
                    error: None,
                };
            }
            Err(e) => {
                return BenchRun {
                    decode_us,
                    first_picture,
                    wall_us: micros(after),
                    end: BenchEnd::Error,
                    error: Some(e.to_string()),
                };
            }
        }
    }
}

/// A duration in whole µs, saturating.
fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// Per-picture decode cost, µs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DecodeUs {
    /// Floored.
    pub mean: u64,
    pub p50: u64,
    pub p99: u64,
    pub max: u64,
}

impl DecodeUs {
    /// The mean, the nearest-rank p50 and p99 (`loop_stats::percentile_ceil`,
    /// the rule the loop-stats line uses) and the max. All 0 for no sample.
    pub fn of(samples: &[u64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        Self {
            mean: samples.iter().sum::<u64>() / samples.len() as u64,
            p50: percentile_ceil(samples, 50),
            p99: percentile_ceil(samples, 99),
            max: samples.iter().copied().max().unwrap_or(0),
        }
    }
}

/// Revision 2's D2 gate: a decoder keeps up with a source of rate f only if
/// its mean cost per picture is at most half the frame period 1/f.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Budget {
    /// 1/f, rounded to the nearest µs (33 367 at 29.97 fps).
    pub frame_period_us: u64,
    /// The mean is over half of 1/f, so the run FAILS the gate.
    pub mean_over_half_period: bool,
}

impl Budget {
    /// The gate for a source rate `num / den` over a run's samples. `None`
    /// when the rate is unknown or no picture was decoded. The comparison is
    /// exact, on the samples' sum, not on the floored mean.
    pub fn check((num, den): (u32, u32), samples: &[u64]) -> Option<Self> {
        if num == 0 || den == 0 || samples.is_empty() {
            return None;
        }
        let (num, den) = (u128::from(num), u128::from(den));
        let frame_period_us = (1_000_000 * den + num / 2) / num;
        let sum: u128 = samples.iter().map(|&us| u128::from(us)).sum();
        let count = samples.len() as u128;
        // mean > 1/(2f) <=> sum / count > 10^6 * den / (2 * num)
        //               <=> 2 * sum * num > 10^6 * den * count
        let mean_over_half_period = 2 * sum * num > 1_000_000 * den * count;
        Some(Self {
            frame_period_us: u64::try_from(frame_period_us).unwrap_or(u64::MAX),
            mean_over_half_period,
        })
    }
}

/// What the bench thread reads from the opened reader.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamFacts {
    /// The stream's size as negotiated at open.
    pub width: u32,
    pub height: u32,
    /// The source rate, `(num, den)`. `None` when MF reports none: the
    /// reader then plays at a 29.97 fps guess, and the gate is not judged
    /// against a guess.
    pub frame_rate: Option<(u32, u32)>,
    /// The native subtype's text (`AV01`, `VP90`, `H264`), if MF told it.
    pub codec: Option<String>,
}

/// The decode mode a run asks for: `"hw": true` (#223 S3b) or
/// `"resident": true` (#279 P3a), never both (`gpu_probe::ask`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BenchMode {
    /// Media Foundation's software decoder.
    #[default]
    Software,
    /// The GPU decoder, each picture read back to RAM (`DecodeMode::Hardware`).
    Hardware,
    /// #279 P3a: the GPU decoder, each picture copied into a producer's ring
    /// (`DecodeMode::GpuResident`).
    Resident,
}

impl BenchMode {
    /// The mode of a request's two flags (`resident` first).
    pub fn of(hw: bool, resident: bool) -> Self {
        if resident {
            Self::Resident
        } else if hw {
            Self::Hardware
        } else {
            Self::Software
        }
    }

    /// The run decodes on the GPU.
    pub fn on_gpu(self) -> bool {
        self != Self::Software
    }

    /// The mode's name in the logs (`DecodeMode::as_str`'s).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Hardware => "hardware",
            Self::Resident => "gpu_resident",
        }
    }
}

/// The reader's mode for a run, and the producer a resident run decodes on.
pub type RunMode = (DecodeMode, Option<Arc<ProducerGpu>>);

/// #279 P3a: the reader's mode for a run of `mode`. A resident run makes its
/// producer device (`make`: `sp_gpu::ProducerGpu::new`, the production
/// adapter) and keeps it, to read its ring after the run; `make` runs for
/// no other mode.
pub fn decode_mode<M>(mode: BenchMode, make: M) -> Result<RunMode, GpuError>
where
    M: FnOnce() -> Result<ProducerGpu, GpuError>,
{
    match mode {
        BenchMode::Software => Ok((DecodeMode::Software, None)),
        BenchMode::Hardware => Ok((DecodeMode::Hardware, None)),
        BenchMode::Resident => {
            let producer = Arc::new(make()?);
            let mode = DecodeMode::GpuResident(Arc::clone(&producer));
            Ok((mode, Some(producer)))
        }
    }
}

/// What the reader says about its decode path after the run (#223 S3b).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecodeFacts {
    /// What the request asked for.
    pub mode: BenchMode,
    /// The path of the last picture (`None`: no picture).
    pub path: Option<DecodePath>,
    /// How often a picture's path differed from the one before it.
    pub path_changes: u32,
    /// The adapter the reader's GPU path opened on.
    pub adapter: Option<String>,
    /// Why the file left the GPU path (`"open: …"` / `"mid-stream: …"`).
    pub fallback: Option<String>,
    /// #282: the decode cap the run opened under (`max_resolution`'s rule,
    /// the producer's).
    pub max_height: Option<u32>,
    /// #282: the stream's own size when the run decoded it scaled to the
    /// cap (the report's `width` / `height` are then the scaled size).
    pub scaled_from: Option<(u32, u32)>,
    /// #279 P3a: a resident run's ring after the run (`None`: no producer).
    pub ring: Option<RingStats>,
}

/// One run's answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BenchReport {
    pub file: String,
    /// The first picture's size, else the stream's, else 0 (did not open).
    pub width: u32,
    pub height: u32,
    /// The first picture's Y-plane row stride, bytes.
    pub stride: Option<u32>,
    pub codec: Option<String>,
    pub source_fps: Option<f64>,
    /// Pictures decoded: the samples behind `decode_us`.
    pub frames: u64,
    /// The first picture's call, µs. It carries Media Foundation's start-up
    /// (the source starts, the decoder fills), so `decode_us.max` is often
    /// this one and not a steady-state spike. From 100 pictures on,
    /// `decode_us.p99` is the steady-state spike; below that it is the max.
    pub first_us: Option<u64>,
    /// The floored mean of every picture after the first: the decoder's
    /// steady state, without that start-up. `None` below two pictures.
    /// Playback's pre-roll absorbs the start-up, so a borderline gate
    /// verdict is re-judged on this figure.
    pub steady_mean_us: Option<u64>,
    /// The decode loop's wall time (opening excluded), floored ms.
    pub wall_ms: u64,
    /// The reader's open, ms.
    pub open_ms: u64,
    pub ended: BenchEnd,
    pub error: Option<String>,
    /// `GetThreadPriority` of the bench thread (0 = NORMAL, as the paced
    /// decode producer's). Windows only.
    pub thread_priority: Option<i32>,
    pub decode_us: DecodeUs,
    pub budget: Option<Budget>,
    /// #223 S3b: the request asked for hardware decode (`"hw": true`).
    pub hw_requested: bool,
    /// The path the LAST picture came out of: `"hardware"` (the GPU
    /// decoder's surfaces, `D3D11_BIND_DECODER`) or `"software"`; `null`
    /// with no picture. The run's timings are one path's only when
    /// `path_changes` is 0.
    pub decode_path: Option<&'static str>,
    /// How often a picture's path differed from the one before it during
    /// the run (a mid-stream fall back, or Media Foundation's decoder
    /// changing its mind): 0 = one path.
    pub path_changes: u32,
    /// The GPU's adapter name, only when `decode_path` is `"hardware"`.
    pub adapter: Option<String>,
    /// Why the file left the GPU path, if it did.
    pub hw_fallback: Option<String>,
    /// #282: the decode cap the run opened under (rows).
    pub max_height: Option<u32>,
    /// #282: the stream's own `[width, height]` when the run decoded it
    /// scaled to the cap; `null` at its own size. A scaled run's
    /// `decode_path` `"hardware"` says the picture lay on the GPU, not that
    /// NVDEC decoded it: judge that by the timings.
    pub scaled_from: Option<(u32, u32)>,
    /// #279 P3a: the request asked for a GPU-resident run (`"resident":
    /// true`; `hw_requested` is then true too). Its `decode_path`
    /// `"hardware"` = the pictures were copied into the ring.
    pub gpu_resident: bool,
    /// #279 P3a: a resident run's ring after the run (its slots, waits,
    /// copy p99, VRAM); `null` for any other run, or when the producer
    /// device could not be made.
    pub ring: Option<RingStatus>,
}

impl BenchReport {
    /// The report of a run the reader opened for.
    pub fn from_run(
        file: &str,
        facts: StreamFacts,
        run: BenchRun,
        open_ms: u64,
        thread_priority: Option<i32>,
    ) -> Self {
        let (width, height) = match run.first_picture {
            Some(p) => (p.width, p.height),
            None => (facts.width, facts.height),
        };
        // An unknown rate reads as 0/0: no fps and no gate.
        let frame_rate = facts.frame_rate.unwrap_or_default();
        Self {
            file: file.to_string(),
            width,
            height,
            stride: run.first_picture.map(|p| p.stride),
            codec: facts.codec,
            source_fps: source_fps(frame_rate),
            frames: run.decode_us.len() as u64,
            first_us: run.decode_us.first().copied(),
            steady_mean_us: steady_mean_us(&run.decode_us),
            wall_ms: run.wall_us / 1_000,
            open_ms,
            ended: run.end,
            error: run.error,
            thread_priority,
            decode_us: DecodeUs::of(&run.decode_us),
            budget: Budget::check(frame_rate, &run.decode_us),
            hw_requested: false,
            decode_path: None,
            adapter: None,
            hw_fallback: None,
            path_changes: 0,
            max_height: None,
            scaled_from: None,
            gpu_resident: false,
            ring: None,
        }
    }

    /// The report with the reader's decode path (#223 S3b). The adapter is
    /// named only for a hardware path: a file that fell back, or that Media
    /// Foundation decoded in software on the GPU's device, did not decode on
    /// it.
    pub fn with_decode(self, decode: DecodeFacts) -> Self {
        let hardware = decode.path == Some(DecodePath::Hardware);
        Self {
            hw_requested: decode.mode.on_gpu(),
            gpu_resident: decode.mode == BenchMode::Resident,
            ring: decode.ring.map(RingStatus::of),
            decode_path: decode.path.map(DecodePath::as_str),
            adapter: decode.adapter.filter(|_| hardware),
            hw_fallback: decode.fallback,
            path_changes: decode.path_changes,
            max_height: decode.max_height,
            scaled_from: decode.scaled_from,
            ..self
        }
    }

    /// The report of a file the reader could not open.
    pub fn open_failed(
        file: &str,
        error: String,
        open_ms: u64,
        thread_priority: Option<i32>,
    ) -> Self {
        Self {
            file: file.to_string(),
            width: 0,
            height: 0,
            stride: None,
            codec: None,
            source_fps: None,
            frames: 0,
            first_us: None,
            steady_mean_us: None,
            wall_ms: 0,
            open_ms,
            ended: BenchEnd::Error,
            error: Some(error),
            thread_priority,
            decode_us: DecodeUs::default(),
            budget: None,
            hw_requested: false,
            decode_path: None,
            adapter: None,
            hw_fallback: None,
            path_changes: 0,
            max_height: None,
            scaled_from: None,
            gpu_resident: false,
            ring: None,
        }
    }

    /// The decoder failed (at open or mid-run): the route answers 500.
    pub fn failed(&self) -> bool {
        self.error.is_some()
    }

    /// The text of the run's end log line: the stream (size, codec, fps,
    /// the open), the run and its stats, the gate, and the error if any.
    pub fn summary(&self) -> String {
        let fps = match self.source_fps {
            Some(fps) => format!("{fps:.3}"),
            None => "?".to_string(),
        };
        let budget = match self.budget {
            Some(b) => format!(
                "frame_period_us={} mean_over_half_period={}",
                b.frame_period_us, b.mean_over_half_period
            ),
            None => "budget=unknown".to_string(),
        };
        let error = match &self.error {
            Some(e) => format!(" error={e}"),
            None => String::new(),
        };
        format!(
            "file={} {}x{} codec={} fps={fps} open_ms={} frames={} wall_ms={} ended={:?} \
             mean_us={} p50_us={} p99_us={} max_us={} {budget}{}{}{error}",
            self.file,
            self.width,
            self.height,
            self.codec.as_deref().unwrap_or("?"),
            self.open_ms,
            self.frames,
            self.wall_ms,
            self.ended,
            self.decode_us.mean,
            self.decode_us.p50,
            self.decode_us.p99,
            self.decode_us.max,
            self.hw_summary(),
            self.scaled_summary(),
        )
    }

    /// The end line's scaled part (#282), only for a run decoded scaled to
    /// the cap, so an unscaled run's line is unchanged.
    fn scaled_summary(&self) -> String {
        match self.scaled_from {
            Some((width, height)) => format!(
                " scaled_from={width}x{height} max_height={}",
                self.max_height.unwrap_or_default()
            ),
            None => String::new(),
        }
    }

    /// The end line's hardware part (#223 S3b), only for a `"hw": true`
    /// run, so a software run's line is the S0 baseline's; #279 P3a: a
    /// resident run's adds its ring.
    fn hw_summary(&self) -> String {
        if !self.hw_requested {
            return String::new();
        }
        let line = format!(
            " hw=requested decode_path={} path_changes={} adapter={} hw_fallback={}",
            self.decode_path.unwrap_or("?"),
            self.path_changes,
            self.adapter.as_deref().unwrap_or("-"),
            self.hw_fallback.as_deref().unwrap_or("-"),
        );
        if self.gpu_resident {
            line + &ring_summary(self.ring.as_ref())
        } else {
            line
        }
    }
}

/// #279 P3a: a resident run's ring on the end line.
fn ring_summary(ring: Option<&RingStatus>) -> String {
    match ring {
        Some(r) => format!(
            " gpu_resident ring_slots={} ring_waits={} ring_full={} ring_dead={} \
             copy_us_p99={} vram_mb={}",
            r.slots,
            r.waits,
            r.full,
            r.dead,
            r.copy_us_p99
                .map_or_else(|| "-".to_string(), |us| us.to_string()),
            r.vram_mb,
        ),
        None => " gpu_resident ring=-".to_string(),
    }
}

/// The floored mean of the samples after the first, `None` below two.
fn steady_mean_us(samples: &[u64]) -> Option<u64> {
    let steady = samples.get(1..)?;
    if steady.is_empty() {
        return None;
    }
    Some(steady.iter().sum::<u64>() / steady.len() as u64)
}

/// `num / den` frames per second, `None` when either is 0.
fn source_fps((num, den): (u32, u32)) -> Option<f64> {
    if num == 0 || den == 0 {
        None
    } else {
        Some(f64::from(num) / f64::from(den))
    }
}

/// What the handler gets back from one run.
#[derive(Debug)]
pub enum BenchOutcome {
    /// The run's report: 200, or 500 when the decoder failed.
    Report(Box<BenchReport>),
    /// This build has no Media Foundation (501).
    Unsupported,
    /// The run's thread did not start, panicked, or ended without a report
    /// (500).
    Failed(String),
}

/// Run `body` on its own `decode-bench` thread, started like the paced
/// decode producer's (`spawn_decode_thread`), and wait for its report.
///
/// The thread holds `slot` and drops it BEFORE it sends the outcome, so
/// the bench is free by the time the caller answers, also when `body`
/// panics. A panic is caught there (the panic hook has already logged it)
/// and comes back as `Failed`.
pub async fn run_on_decode_thread<B>(slot: BenchSlot, body: B) -> BenchOutcome
where
    B: FnOnce() -> BenchReport + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = spawn_decode_thread("decode-bench".to_string(), move || {
        let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        drop(slot);
        let _ = tx.send(report);
    });
    if let Err(e) = spawned {
        return BenchOutcome::Failed(format!("the decode-bench thread did not start: {e}"));
    }
    match rx.await {
        Ok(Ok(report)) => BenchOutcome::Report(Box::new(report)),
        Ok(Err(_)) => BenchOutcome::Failed("the decode-bench thread panicked".into()),
        Err(_) => BenchOutcome::Failed("the decode-bench thread ended without a report".into()),
    }
}

/// Run one bench of `path` with the real reader
/// (`decode_bench_mf::bench_file`) on its own decode thread.
///
/// mutants::skip: a `cfg(windows)` one-liner the Linux mutation runner
/// never compiles, so every mutant would build and survive. The thread
/// handling is [`run_on_decode_thread`], Linux-tested; the Windows job's
/// router tests (`api/diag_tests.rs`) run this on the real decoder.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub async fn run(
    path: PathBuf,
    file: String,
    max_wall: Duration,
    mode: BenchMode,
    slot: BenchSlot,
) -> BenchOutcome {
    run_on_decode_thread(slot, move || {
        super::decode_bench_mf::bench_file(&path, &file, max_wall, mode)
    })
    .await
}

/// Without Media Foundation there is no decoder to measure (501).
#[cfg(not(windows))]
pub async fn run(
    _path: PathBuf,
    _file: String,
    _max_wall: Duration,
    _mode: BenchMode,
    _slot: BenchSlot,
) -> BenchOutcome {
    BenchOutcome::Unsupported
}

#[cfg(test)]
#[path = "decode_bench_tests.rs"]
mod tests;
