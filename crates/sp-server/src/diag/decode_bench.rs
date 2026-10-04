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
//! This file is the cross-platform part, Linux-tested:
//!
//! - [`DecodeBench`]: the sample dir and the one-run-at-a-time gate;
//! - [`bench_file_name`] and [`bench_seconds`]: the request's checks;
//! - [`measure`]: the timing loop, generic over [`VideoStream`];
//! - [`BenchReport`]: the stats ([`DecodeUs`]) and D2's gate ([`Budget`]).
//!
//! The Windows half (open the reader, log, the thread body) is
//! `decode_bench_mf.rs`.

use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::http::StatusCode;
use serde::Serialize;
use sp_decoder::VideoStream;

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
    /// A bench whose samples are in `dir`.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            busy: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The bench beside the DB: `<data dir>/bench`, which on the box is
    /// `C:\ProgramData\SongPlayer\bench`. It is outside the media cache, so
    /// startup's cache self-heal never touches a sample.
    pub fn beside_db(db_path: &Path) -> Self {
        let data_dir = db_path.parent().unwrap_or_else(|| Path::new("."));
        Self::new(data_dir.join("bench"))
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
/// an NTFS stream) or control character, no `..`, and is one plain path
/// component. Anything else could reach outside the sample dir.
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
    let mut parts = Path::new(name).components();
    let one_plain_part =
        matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none();
    if !one_plain_part {
        return Err("file must be a bare file name");
    }
    Ok(name)
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
        let ring: VecDeque<u64> = samples.iter().copied().collect();
        Self {
            mean: samples.iter().sum::<u64>() / samples.len() as u64,
            p50: percentile_ceil(&ring, 50),
            p99: percentile_ceil(&ring, 99),
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
    /// The source rate, `(num, den)`.
    pub frame_rate: (u32, u32),
    /// The native subtype's text (`AV01`, `VP90`, `H264`), if MF told it.
    pub codec: Option<String>,
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
        Self {
            file: file.to_string(),
            width,
            height,
            stride: run.first_picture.map(|p| p.stride),
            codec: facts.codec,
            source_fps: source_fps(facts.frame_rate),
            frames: run.decode_us.len() as u64,
            wall_ms: run.wall_us / 1_000,
            open_ms,
            ended: run.end,
            error: run.error,
            thread_priority,
            decode_us: DecodeUs::of(&run.decode_us),
            budget: Budget::check(facts.frame_rate, &run.decode_us),
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
            wall_ms: 0,
            open_ms,
            ended: BenchEnd::Error,
            error: Some(error),
            thread_priority,
            decode_us: DecodeUs::default(),
            budget: None,
        }
    }

    /// 200 for a run that ended cleanly, 500 when the decoder failed.
    pub fn status(&self) -> StatusCode {
        if self.error.is_some() {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::OK
        }
    }

    /// The text of the run's end log line.
    pub fn summary(&self) -> String {
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
            "file={} {}x{} codec={} frames={} wall_ms={} ended={:?} \
             mean_us={} p50_us={} p99_us={} max_us={} {budget}{error}",
            self.file,
            self.width,
            self.height,
            self.codec.as_deref().unwrap_or("?"),
            self.frames,
            self.wall_ms,
            self.ended,
            self.decode_us.mean,
            self.decode_us.p50,
            self.decode_us.p99,
            self.decode_us.max,
        )
    }
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
    /// The run's thread did not start, or ended without a report (500).
    Failed(String),
}

/// Run one bench of `path` on its own decode thread and wait for the
/// report. `slot` is moved to the thread and dropped there before the
/// report is sent, so the bench is free by the time the caller answers.
///
/// mutants::skip: `cfg(windows)`, so the Linux mutation runner never
/// compiles it and every mutant would build and survive. The Windows job's
/// router tests (`api/diag_tests.rs`) run it on the real decoder.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub async fn run(path: PathBuf, file: String, max_wall: Duration, slot: BenchSlot) -> BenchOutcome {
    use crate::playback::decode_thread::spawn_decode_thread;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = spawn_decode_thread("decode-bench".to_string(), move || {
        let report = super::decode_bench_mf::bench_file(&path, &file, max_wall);
        drop(slot);
        let _ = tx.send(report);
    });
    if let Err(e) = spawned {
        return BenchOutcome::Failed(format!("the decode-bench thread did not start: {e}"));
    }
    match rx.await {
        Ok(report) => BenchOutcome::Report(Box::new(report)),
        Err(_) => BenchOutcome::Failed("the decode-bench thread ended without a report".into()),
    }
}

/// Without Media Foundation there is no decoder to measure (501).
#[cfg(not(windows))]
pub async fn run(
    _path: PathBuf,
    _file: String,
    _max_wall: Duration,
    _slot: BenchSlot,
) -> BenchOutcome {
    BenchOutcome::Unsupported
}

#[cfg(test)]
#[path = "decode_bench_tests.rs"]
mod tests;
