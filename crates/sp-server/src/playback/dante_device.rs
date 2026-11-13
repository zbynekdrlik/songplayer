//! #285: the native Dante output. SongPlayer's own TX-only Dante device
//! (dstack's `dlink`, threads in this process, no driver and no DVS) behind
//! the [`AsioDevice`] seam, so `asio_out`'s worker, its ring, its drift servo
//! and its status serve it unchanged: the stack's `dstack-engine` thread calls
//! [`RingProcess`] once per block as a driver's buffer switch would, on the
//! Dante media clock.
//!
//! Two TX channels on dlink's defaults: 24-bit, block 128, a 15 ms budget, a
//! pump margin of 2/5 of the budget, no real-time setup. The worker writes the
//! program's 48 kHz; the device runs at the network's rate and dlink converts
//! on the device's clock (`Config::network_rate`, #285 step 2, dstack #90).
//! The stack pulls and never stretches time; the servo keeps the ring's fill.
//! Without PTP the pump paces itself and drops its outputs, and the device
//! keeps announcing: the stack starts sending once the clock is ready.
//!
//! `Link::start` sets up and starts in one call, so a device the stack refuses
//! (a name, an address no adapter holds) fails the seam's `start`, not `open`.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use dlink::Clock as ClockSource;
use dlink::{Block, Config, HostError, Link, Map, Media, Place, Ports, Process, Stopped};
use dstack::ctl;
use serde::Deserialize;
use sp_core::audio_outputs::{DanteDest, OutputEntry, PROGRAM_RATE, effective_rate};
use sp_core::dante_status::DanteFigures;
use tracing::{info, warn};

use crate::playback::asio_format::AsioSample;
use crate::playback::asio_hold::{DriverHold, DriverHolds};
use crate::playback::asio_out::{AsioDevice, Opened, Started};
use crate::playback::asio_state::{DeviceEvents, Reason};

/// The largest block the stack's pump runs (its `block` setting).
const MAX_BLOCK: usize = 256;
const DANTE_CHANNELS: usize = 2;

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
    /// The rate the worker writes: the program's (#285 step 2).
    pub rate: u32,
    /// The rate the device runs at on the network; dlink converts between
    /// the two on the device's clock when they differ.
    pub network_rate: u32,
    pub dir: PathBuf,
    pub clock: ClockSource,
    /// The device's control ports and process id, `None` for Dante's
    /// defaults (ARC 4440, CMC 8800, flows 4455, process 0): the output is
    /// the one Dante device of its address. A test device takes a slot of
    /// its own (#323), so the devices of parallel test processes never meet.
    pub ports: Option<(Ports, u16)>,
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
            rate: PROGRAM_RATE,
            network_rate: effective_rate(entry.rate, network_rate),
            dir: base.join("dante"),
            clock: ClockSource::Dsync,
            ports: None,
        })
    }

    /// dlink's TX path at the worker's rate: its work margin, the Dante
    /// latency it derives from its 15 ms reference path (`REFERENCE_PATH_NS`, v0.6.106) and the rate converter's TX
    /// delay (`Config::split`, dstack v0.6.22: the budget is the app's whole
    /// path, the owner's ruling to dstack of 3.11.2026).
    fn latency_frames(&self) -> u64 {
        let path_ns = self
            .config()
            .split()
            .map_or(dlink::REFERENCE_PATH_NS, |s| s.tx_path_ns);
        u64::from(self.rate) * path_ns / 1_000_000_000
    }

    /// The device's block at the worker's rate, frames (rounded up).
    fn app_block(&self, device_block: u64) -> u32 {
        let num = device_block * u64::from(self.rate);
        let den = u64::from(self.network_rate.max(1));
        u32::try_from(num.div_ceil(den)).unwrap_or(u32::MAX)
    }

    /// The device for the stack. Dante Controller's changes live in the
    /// state file, and the channels carry DVS's names: a receiver subscribed
    /// to `01@resolume-snv` needs no change.
    pub fn config(&self) -> Config {
        let config = Config::new(self.device.clone(), self.dir.join("state.toml"))
            .tx(["01", "02"])
            .sample_rate(self.rate)
            .network_rate(self.network_rate)
            .product("New Level", "SongPlayer");
        match self.ports {
            Some((ports, id)) => config.ports(ports).process_id(id),
            None => config,
        }
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

/// The stack's status line (`ctl::status`, what `Link::status` returns) as
/// the dashboard's figures: its TX flows' bins summed, their maxima taken, a
/// flow that ended included. `None` for a line that does not read.
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
        figures_of(&ctl::status(&media))
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
            scratch: vec![0.0; MAX_BLOCK * DANTE_CHANNELS],
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

/// The Dante device this process holds. A rebuilt entry's successor starts
/// while the old worker still stops, and one device binds the Dante ports on
/// its address and announces its name. The list allows one Dante entry
/// (`MAX_DANTE_OUTPUTS`), so the hold has one name.
static HELD: DriverHolds = DriverHolds::new();
const HOLD_NAME: &str = "dante";

/// The output's device on its worker thread.
pub struct DanteDevice {
    params: DanteParams,
    config: Option<Config>,
    running: Option<Link>,
    counts: Arc<Counts>,
    link: DanteLink,
    /// From `open` until after the stack stopped.
    hold: Option<DriverHold<'static>>,
}

impl DanteDevice {
    pub fn new(params: DanteParams) -> Self {
        Self::linked(params, DanteLink::default())
    }

    /// A device whose status reads through `link`.
    pub fn linked(params: DanteParams, link: DanteLink) -> Self {
        Self {
            params,
            config: None,
            running: None,
            counts: Arc::default(),
            link,
            hold: None,
        }
    }
}

impl AsioDevice for DanteDevice {
    fn open(&mut self, _driver: &str, _channels: [u32; 2]) -> Result<Opened, Reason> {
        std::fs::create_dir_all(&self.params.dir)
            .map_err(|e| Reason::Failed(format!("the Dante device's directory: {e}")))?;
        let config = self.params.config();
        let device = config
            .device()
            .map_err(|problems| Reason::Refused(problems.join("; ")))?;
        self.hold = Some(HELD.claim(HOLD_NAME).ok_or(Reason::Held)?);
        self.config = Some(config);
        Ok(Opened {
            rate: f64::from(self.params.rate),
            buffer_frames: self.params.app_block(device.block),
            out_channels: DANTE_CHANNELS as u32,
            sample: AsioSample::Float32,
        })
    }

    fn start(&mut self, ring: rtrb::Consumer<f32>) -> Result<Started, Reason> {
        let config = self
            .config
            .take()
            .ok_or_else(|| Reason::Failed("the Dante device is not open".into()))?;
        self.counts = Arc::default();
        let process = RingProcess::new(ring, Arc::clone(&self.counts));
        let map = Map {
            rx: Vec::new(),
            tx: vec![0, 1],
        };
        let place = Place::Ip(self.params.ip);
        let running =
            Link::start_on(&config, &place, map, process, self.params.clock).map_err(reason)?;
        info!(
            rate = self.params.rate,
            network_rate = self.params.network_rate,
            "dante output: device announced"
        );
        self.link.set(Some(Arc::clone(running.media())));
        self.running = Some(running);
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
            let view = run.media().engine().unwrap_or_default();
            events.overloads = view.overruns;
            if let Some(why) = run.ended() {
                warn!(%why, "dante output: the device ended");
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
        self.config = None;
        if let Some(run) = self.running.take() {
            let stopped = run.stop();
            info!(?stopped, "dante output: device stopped");
            if matches!(stopped, Stopped::StillRunning) {
                // Left to end, and dlink gives no handle to wait on. The output
                // must come back without a restart: a successor that finds the
                // ports still bound fails its start and retries on the backoff.
                warn!("dante output: the stack is still stopping after dlink's stop wait");
            }
        }
        // After the stop: the hold covers the ports and the name until then.
        self.hold = None;
    }
}

impl Drop for DanteDevice {
    /// A dropped `Link` does not stop its stack, so a worker's panic stops it here.
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
#[path = "dante_device_tests.rs"]
mod tests;
