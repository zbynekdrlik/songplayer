//! #285: the native Dante output. SongPlayer's own TX-only Dante device
//! (dstack's `dante-stack`, threads in this process, no driver and no DVS)
//! behind the [`AsioDevice`] seam, so `asio_out`'s worker, its ring, its
//! drift servo and its status serve it unchanged: the stack's `dante-engine`
//! thread calls [`RingProcess`] once per block as a driver's buffer switch
//! would, on the Dante media clock.
//!
//! Two TX channels at the network rate, 24-bit, block 128, latency 10 ms.
//! The stack pulls and never stretches time; the servo keeps the ring's fill.
//! Without PTP the pump paces itself and drops its outputs, and the device
//! keeps announcing: the stack starts sending once the clock is ready.
//!
//! The production gate (#285): dstack's problem A is open, so no Dante output
//! is enabled on SNV or PP before its tag.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use dante_audio_io::{Block, Process};
use dante_stack::ctl;
use dante_stack::daemon::media::Media;
use dante_stack::daemon::{Options, Place};
use dante_stack::host::{self, ClockSource, Host, HostError, Prepared, Signals};
use dante_stack::media::source::Source;
use dante_stack::stream::{self, Map, Pump, PumpStats, Timing};
use serde::Deserialize;
use sp_core::audio_outputs::{DanteDest, OutputEntry, effective_rate};
use sp_core::dante_status::DanteFigures;
use tracing::{info, warn};

use crate::playback::asio_format::AsioSample;
use crate::playback::asio_out::{AsioDevice, Opened, Started};
use crate::playback::asio_state::{DeviceEvents, Reason};

/// Frames per block of the stack's pump.
pub const DANTE_BLOCK: u32 = 128;
/// The latency the device advertises: program audio does not chase the
/// lowest (ruling on #285), and a late TX thread stayed clean at 10 ms.
pub const DANTE_LATENCY_NS: u64 = 10_000_000;
/// The pump's work margin, samples: two blocks (the stack asks at least one).
pub const DANTE_WORK_MARGIN: u32 = 2 * DANTE_BLOCK;
const DANTE_CHANNELS: usize = 2;
const STOP_WAIT: Duration = Duration::from_secs(5);

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Where the device keeps its config and state (startup, once).
pub fn set_data_dir(dir: PathBuf) {
    let _ = DATA_DIR.set(dir);
}

/// What the device is built from.
#[derive(Clone, Debug, PartialEq)]
pub struct DanteParams {
    pub device: String,
    pub ip: Ipv4Addr,
    pub rate: u32,
    pub dir: PathBuf,
    pub clock: ClockSource,
}

impl DanteParams {
    /// The params of a validated entry on a network at `network_rate`.
    pub fn for_entry(entry: &OutputEntry, network_rate: u32) -> Result<Self, String> {
        let DanteDest { device, ip } = entry
            .dante
            .clone()
            .ok_or_else(|| "not a Dante entry".to_string())?;
        let ip = ip.parse().map_err(|_| "not an IPv4 address".to_string())?;
        let base = DATA_DIR.get().map_or(Path::new("."), PathBuf::as_path);
        Ok(Self {
            device,
            ip,
            rate: effective_rate(entry.rate, network_rate),
            dir: base.join("dante"),
            clock: ClockSource::Dantesync,
        })
    }

    fn latency_frames(&self) -> u64 {
        u64::from(self.rate) * DANTE_LATENCY_NS / 1_000_000_000
    }

    fn config_path(&self) -> PathBuf {
        self.dir.join("device.toml")
    }

    /// The device's config for the stack. The name is validated, the state
    /// file's path is written as a TOML string.
    pub fn config_text(&self) -> String {
        let state = self.dir.join("state.toml");
        let state = state.to_string_lossy().replace('\\', "\\\\");
        format!(
            "name = \"{}\"\nmanufacturer = \"New Level\"\nmodel = \"SongPlayer\"\n\
             rx = []\ntx = [\"Program-L\", \"Program-R\"]\nsample_rate = {}\nencoding = 24\n\
             latency_ns = {DANTE_LATENCY_NS}\nblock = {DANTE_BLOCK}\n\
             work_margin = {DANTE_WORK_MARGIN}\nstate_file = \"{state}\"\n",
            self.device, self.rate
        )
    }
}

/// The fields of the stack's status line that `outputs[i].dante` shows (its
/// flags are 0 or 1).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StatusLine {
    clock: ClockLine,
    tx: Vec<FlowLine>,
    engine: EngineLine,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ClockLine {
    ready: u8,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FlowLine {
    live: u8,
    late_bins: Vec<u64>,
    late_max_us: u64,
    send_max_us: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct EngineLine {
    late: u64,
    missed: u64,
    overruns: u64,
}

/// The stack's status line (`ctl::answer("status", ..)`) as the dashboard's
/// figures: its TX flows' bins summed, their maxima taken, a flow that ended
/// included. `None` for a line that does not read.
pub fn figures_of(line: &str) -> Option<DanteFigures> {
    let line: StatusLine = serde_json::from_str(line).ok()?;
    let mut f = DanteFigures {
        clock_ready: line.clock.ready != 0,
        pump_late: line.engine.late,
        pump_missed: line.engine.missed,
        pump_overruns: line.engine.overruns,
        ..DanteFigures::default()
    };
    for flow in &line.tx {
        f.live_flows += u32::from(flow.live != 0);
        f.late_max_us = f.late_max_us.max(flow.late_max_us);
        f.send_max_us = f.send_max_us.max(flow.send_max_us);
        if f.late_bins.len() < flow.late_bins.len() {
            f.late_bins.resize(flow.late_bins.len(), 0);
        }
        for (sum, n) in f.late_bins.iter_mut().zip(&flow.late_bins) {
            *sum += n;
        }
    }
    Some(f)
}

/// The running device's media, for the status: the worker sets it at the
/// start and clears it at the close, `GET /api/v1/program` reads it. Nothing
/// on the audio path takes this lock.
#[derive(Clone, Default)]
pub struct DanteLink(Arc<Mutex<Option<Arc<Media>>>>);

impl DanteLink {
    fn set(&self, media: Option<Arc<Media>>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = media;
    }

    /// The stack's figures now; `None` while no device runs.
    pub fn figures(&self) -> Option<DanteFigures> {
        let media = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()?;
        figures_of(&ctl::answer("status", &media))
    }
}

/// What the media thread counts and the worker reads.
#[derive(Debug, Default)]
pub struct Counts {
    consumed: AtomicU64,
    underruns: AtomicU64,
    blocks: AtomicU64,
    primed: AtomicBool,
}

/// The stack's callback: the ring's interleaved stereo into the two outputs.
/// Allocates, locks and logs nothing, as a buffer switch.
pub struct RingProcess {
    ring: rtrb::Consumer<f32>,
    scratch: Vec<f32>,
    counts: Arc<Counts>,
}

impl RingProcess {
    pub fn new(ring: rtrb::Consumer<f32>, counts: Arc<Counts>) -> Self {
        Self {
            ring,
            scratch: vec![0.0; DANTE_BLOCK as usize * DANTE_CHANNELS],
            counts,
        }
    }
}

impl Process for RingProcess {
    fn process(&mut self, block: &mut Block<'_>) {
        let frames = block.frames();
        let want = (frames * DANTE_CHANNELS).min(self.scratch.len());
        let got = self
            .ring
            .pop_partial_slice(&mut self.scratch[..want])
            .0
            .len();
        self.scratch[got..want].fill(0.0);
        for ch in 0..DANTE_CHANNELS {
            let out = block.output(ch);
            for (i, sample) in out.iter_mut().enumerate() {
                *sample = f64::from(
                    self.scratch
                        .get(i * DANTE_CHANNELS + ch)
                        .copied()
                        .unwrap_or(0.0),
                );
            }
        }
        if got < want && self.counts.primed.load(Ordering::Relaxed) {
            self.counts.underruns.fetch_add(1, Ordering::Relaxed);
        }
        self.counts
            .consumed
            .fetch_add(frames as u64, Ordering::Relaxed);
        self.counts.blocks.fetch_add(1, Ordering::Relaxed);
    }
}

fn reason(e: HostError) -> Reason {
    match e {
        HostError::Setup(text) => Reason::Refused(text),
        HostError::Run(text) => Reason::Failed(text),
    }
}

/// A running device.
struct Running {
    host: Host,
    pump: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    stats: Arc<PumpStats>,
}

/// The output's device on its worker thread.
pub struct DanteDevice {
    params: DanteParams,
    prepared: Option<Prepared>,
    running: Option<Running>,
    counts: Arc<Counts>,
    link: DanteLink,
}

impl DanteDevice {
    pub fn new(params: DanteParams) -> Self {
        Self::linked(params, DanteLink::default())
    }

    /// A device whose status reads through `link`.
    pub fn linked(params: DanteParams, link: DanteLink) -> Self {
        Self {
            params,
            prepared: None,
            running: None,
            counts: Arc::default(),
            link,
        }
    }

    fn prepare(&self) -> Result<Prepared, Reason> {
        let p = &self.params;
        std::fs::create_dir_all(&p.dir)
            .and_then(|()| std::fs::write(p.config_path(), p.config_text()))
            .map_err(|e| Reason::Failed(format!("the Dante device's config: {e}")))?;
        let opts = Options {
            config: p.config_path(),
            place: Place::Ip(p.ip),
            control: p.dir.join("control.sock"),
        };
        host::prepare_with(&opts, &[0, 1], p.clock).map_err(reason)
    }
}

impl AsioDevice for DanteDevice {
    fn open(&mut self, _driver: &str, _channels: [u32; 2]) -> Result<Opened, Reason> {
        self.prepared = Some(self.prepare()?);
        Ok(Opened {
            rate: f64::from(self.params.rate),
            buffer_frames: DANTE_BLOCK,
            out_channels: DANTE_CHANNELS as u32,
            sample: AsioSample::Float32,
        })
    }

    fn start(&mut self, ring: rtrb::Consumer<f32>) -> Result<Started, Reason> {
        let prepared = self
            .prepared
            .take()
            .ok_or_else(|| Reason::Failed("the Dante device is not open".into()))?;
        let media = prepared.media();
        let cfg = prepared.config();
        let timing = Timing {
            block: u64::from(DANTE_BLOCK),
            playout: self.params.latency_frames(),
            margin: cfg.work_margin,
        };
        let bits = cfg.encoding;
        for ch in 0..DANTE_CHANNELS {
            if !media.tx.set_source(ch, Source::Engine) {
                return Err(Reason::Failed("the Dante device has no engine ring".into()));
            }
        }
        self.counts = Arc::default();
        let stats = Arc::new(PumpStats::new(timing, self.params.rate));
        media.attach_engine(Arc::clone(&stats));
        let map = Map {
            rx: Vec::new(),
            tx: vec![0, 1],
        };
        let process = RingProcess::new(ring, Arc::clone(&self.counts));
        let pump = Pump::new(
            process,
            map,
            timing,
            bits,
            Arc::clone(&media.rx),
            Arc::clone(&media.tx),
            Arc::clone(&stats),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let thread = stream::spawn(Arc::clone(&media.clock), pump, Arc::clone(&stop))
            .map_err(|e| Reason::Failed(format!("the Dante pump: {e}")))?;
        let signals = Signals::Report(Arc::new(AtomicBool::new(false)));
        let host = match prepared.start(signals) {
            Ok(host) => host,
            Err(e) => {
                stop.store(true, Ordering::Release);
                let _ = thread.join();
                return Err(reason(e));
            }
        };
        info!(rate = self.params.rate, "dante output: device announced");
        self.link.set(Some(media));
        self.running = Some(Running {
            host,
            pump: thread,
            stop,
            stats,
        });
        Ok(Started {
            output_latency_frames: self.output_latency_frames(),
            loop_inputs: 0,
        })
    }

    fn poll(&mut self) -> DeviceEvents {
        let mut events = DeviceEvents {
            callbacks: self.counts.blocks.load(Ordering::Relaxed),
            ..DeviceEvents::default()
        };
        if let Some(run) = &self.running {
            let stream = run.stats.stream_stats();
            events.overloads = stream.overruns;
            events.late_callbacks = stream.late;
            if let Some(why) = run.host.ended() {
                warn!(%why, "dante output: the device ended");
                events.reset = true;
            }
            if stream.faulted {
                warn!("dante output: the callback panicked");
                events.reset = true;
            }
        }
        events
    }

    fn consumed_frames(&self) -> u64 {
        self.counts.consumed.load(Ordering::Relaxed)
    }

    fn underruns(&self) -> u64 {
        self.counts.underruns.load(Ordering::Relaxed)
    }

    fn mark_primed(&mut self) {
        self.counts.primed.store(true, Ordering::Relaxed);
    }

    fn output_latency_frames(&self) -> u32 {
        u32::try_from(self.params.latency_frames()).unwrap_or(u32::MAX)
    }

    fn close(&mut self) {
        self.link.set(None);
        self.prepared = None;
        if let Some(run) = self.running.take() {
            run.stop.store(true, Ordering::Release);
            if run.pump.join().is_err() {
                warn!("dante output: the pump thread panicked");
            }
            let stopped = run.host.stop(STOP_WAIT);
            info!(?stopped, "dante output: device stopped");
        }
    }
}

#[cfg(test)]
#[path = "dante_device_tests.rs"]
mod tests;
