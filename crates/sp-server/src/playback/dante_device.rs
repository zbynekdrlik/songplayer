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

use std::net::{Ipv4Addr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use dlink::Clock as ClockSource;
use dlink::{Block, Config, HostError, Link, Map, Media, Place, Ports, Process, Stopped};
use dstack::ctl;
use serde::Deserialize;
use sp_core::audio_outputs::{DanteDest, OutputEntry, PROGRAM_RATE, effective_rate};
use sp_core::dante_status::{DanteFigures, LateWake, MediaThread};
use tracing::{info, warn};

use crate::playback::asio_format::AsioSample;
use crate::playback::asio_hold::{DriverHold, DriverHolds};
use crate::playback::asio_out::{AsioDevice, Opened, Started};
use crate::playback::asio_state::{DeviceEvents, Reason};
use crate::playback::dante_slots;

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
    /// The device's control ports and process id. `None` (the output's):
    /// `open` takes the first dstack slot free on its address (#324,
    /// `dante_slots`), Dante's defaults (ARC 4440, CMC 8800, flows 4455,
    /// process 0) when none is. A test device takes a slot of its own
    /// (#323), so the devices of parallel test processes never meet.
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
        self.config_on(self.ports)
    }

    /// [`Self::config`] on `ports` (`None`: Dante's defaults).
    fn config_on(&self, ports: Option<(Ports, u16)>) -> Config {
        let config = Config::new(self.device.clone(), self.dir.join("state.toml"))
            .tx(["01", "02"])
            .sample_rate(self.rate)
            .network_rate(self.network_rate)
            .product("New Level", "SongPlayer");
        match ports {
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
    rebootstraps: u64,
    rebootstraps_late: u64,
    rebootstraps_clock_back: u64,
    rebootstraps_reanchor: u64,
    rebootstraps_retime: u64,
    wake_behind_max_us: u64,
    wake_behind_minute_max_us: Option<u64>,
    wake_behind_buckets: Vec<u64>,
    late_wakes: u64,
    late_wakes_recent: Vec<WakeLine>,
    threads: Vec<MediaThread>,
}

/// One of the pump's late wakes (its `rebootstrap` flag 0 or 1).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WakeLine {
    unix_ms: u64,
    behind_us: u64,
    rebootstrap: u8,
}

/// The stack's status line (`ctl::status`, what `Link::status` returns) as
/// the dashboard's figures: its TX flows' bins summed, their maxima taken, a
/// flow that ended included. `None` for a line that does not read.
pub fn figures_of(line: &str) -> Option<DanteFigures> {
    let line: StatusLine = serde_json::from_str(line).ok()?;
    let e = line.engine;
    let mut f = DanteFigures {
        clock_ready: line.clock.ready != 0,
        pump_late: e.late,
        pump_missed: e.missed,
        pump_overruns: e.overruns,
        pump_rebootstraps: e.rebootstraps,
        pump_rebootstraps_late: e.rebootstraps_late,
        pump_rebootstraps_clock_back: e.rebootstraps_clock_back,
        pump_rebootstraps_reanchor: e.rebootstraps_reanchor,
        pump_rebootstraps_retime: e.rebootstraps_retime,
        wake_behind_max_us: e.wake_behind_max_us,
        wake_behind_minute_max_us: e.wake_behind_minute_max_us,
        wake_behind_buckets: e.wake_behind_buckets,
        late_wakes: e.late_wakes,
        late_wakes_recent: e
            .late_wakes_recent
            .into_iter()
            .map(|w| LateWake {
                unix_ms: w.unix_ms,
                behind_us: w.behind_us,
                rebootstrap: w.rebootstrap != 0,
            })
            .collect(),
        threads: e.threads,
        ..DanteFigures::default()
    };
    for flow in &line.tx {
        f.live_flows += u32::from(flow.live != 0);
        f.late_max_us = f.late_max_us.max(flow.late_max_us);
        f.send_max_us = f.send_max_us.max(flow.send_max_us);
        // As long as the longest flow's (a `<` guard here had an
        // equivalent `<=` mutant: resizing to the same length is a no-op).
        let bins = f.late_bins.len().max(flow.late_bins.len());
        f.late_bins.resize(bins, 0);
        for (sum, n) in f.late_bins.iter_mut().zip(&flow.late_bins) {
            *sum += n;
        }
    }
    Some(f)
}

/// The running device's media and its ports, for the status: the worker
/// sets them at the start and clears them at the close, `GET
/// /api/v1/program` reads them. Nothing on the audio path takes this lock.
#[derive(Clone, Default)]
pub struct DanteLink(Arc<Mutex<Option<Running>>>);

/// A running device's media and its control ports + process id.
type Running = (Arc<Media>, (Ports, u16));

impl DanteLink {
    fn set(&self, running: Option<Running>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = running;
    }

    /// The stack's figures now, with the device's ports; `None` while no
    /// device runs.
    pub fn figures(&self) -> Option<DanteFigures> {
        let (media, ports) = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()?;
        figures_of(&ctl::status(&media)).map(|f| with_ports(f, ports))
    }
}

/// `f` with the device's control ports and process id (#324).
fn with_ports(f: DanteFigures, (ports, id): (Ports, u16)) -> DanteFigures {
    DanteFigures {
        arc: ports.arc,
        cmc: ports.cmc,
        flows: ports.flows,
        process_id: id,
        ..f
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
        // Whole frames only (dstack#371): half a frame in the ring (its
        // writer between left and right) waits for the next block, so the
        // channels never swap.
        let whole = self.ring.slots() / DANTE_CHANNELS * DANTE_CHANNELS;
        let got = self
            .ring
            .pop_partial_slice(&mut self.scratch[..want.min(whole)])
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

/// The stack's refusal as the worker's reason, a refused port named with
/// its holder where Windows can tell (#324): `holder` answers a port's
/// `(pid, image)`.
fn reason(e: HostError, holder: impl Fn(u16) -> Option<(u32, String)>) -> Reason {
    match e {
        HostError::Setup(text) => Reason::Refused(dante_slots::holder_text(&text, holder)),
        HostError::Run(text) => Reason::Failed(dante_slots::holder_text(&text, holder)),
    }
}

/// The Dante device this process holds. A rebuilt entry's successor starts
/// while the old worker still stops, and one device binds the Dante ports on
/// its address and announces its name. The list allows one Dante entry
/// (`MAX_DANTE_OUTPUTS`), so the hold has one name.
static HELD: DriverHolds = DriverHolds::new();
const HOLD_NAME: &str = "dante";

/// The ports of a device whose stack was still stopping after dlink's stop
/// wait (#324): its successor takes the same ports, so it fails its start
/// on them and retries on the backoff until they are free, never moving to
/// another slot beside a device of ours that still announces the name.
/// Cleared by the next device that starts, or dropped when a start on them
/// is refused on a port another program holds (`keep_lingering`), so the
/// next open probes a free slot.
static LINGERING: Mutex<Option<(Ports, u16)>> = Mutex::new(None);

fn lingering() -> std::sync::MutexGuard<'static, Option<(Ports, u16)>> {
    LINGERING.lock().unwrap_or_else(|e| e.into_inner())
}

/// The output's device on its worker thread.
pub struct DanteDevice {
    params: DanteParams,
    config: Option<Config>,
    running: Option<Link>,
    counts: Arc<Counts>,
    link: DanteLink,
    /// From `open` until after the stack stopped.
    hold: Option<DriverHold<'static>>,
    /// The ports `open` chose (#324): a slot's, `None` for Dante's defaults.
    chosen: Option<(Ports, u16)>,
    /// Where they came from.
    source: dante_slots::Source,
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
            chosen: None,
            source: dante_slots::Source::Defaults,
        }
    }

    /// The control ports and process id the device opened on: the chosen
    /// slot's, else Dante's defaults (slot 0).
    pub fn ports(&self) -> (Ports, u16) {
        dante_slots::effective(self.chosen)
    }
}

impl DanteDevice {
    /// After a refused start on ports a stopping predecessor left: another
    /// program holding the refused port (`holder_pid`) drops them, so the
    /// next open probes a free slot (#324).
    fn after_refusal(&self, e: &HostError, holder_pid: impl Fn(u16) -> Option<u32>) {
        let (HostError::Setup(text) | HostError::Run(text)) = e;
        if self.source == dante_slots::Source::Lingering
            && !dante_slots::keep_lingering(text, holder_pid, std::process::id())
        {
            warn!(%text, "dante output: another program holds the ports left by the stopping device; the next open takes a free slot");
            *lingering() = None;
        }
    }

    /// The seam's `start`, `holder` answering a refused port's holder as
    /// `(pid, image)` (`dante_slots::port_holder` in production).
    fn start_with(
        &mut self,
        ring: rtrb::Consumer<f32>,
        holder: impl Fn(u16) -> Option<(u32, String)>,
    ) -> Result<Started, Reason> {
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
        let running = match Link::start_on(&config, &place, map, process, self.params.clock) {
            Ok(running) => running,
            Err(e) => {
                self.after_refusal(&e, |port| holder(port).map(|(pid, _)| pid));
                return Err(reason(e, &holder));
            }
        };
        *lingering() = None;
        let (ports, id) = self.ports();
        info!(
            rate = self.params.rate,
            network_rate = self.params.network_rate,
            arc = ports.arc,
            cmc = ports.cmc,
            flows = ports.flows,
            process_id = id,
            source = self.source.name(),
            "dante output: device announced"
        );
        self.link
            .set(Some((Arc::clone(running.media()), self.ports())));
        self.running = Some(running);
        Ok(Started {
            output_latency_frames: self.output_latency_frames(),
            loop_inputs: 0,
        })
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
        // #324: the slot is probed only now, with the hold: a predecessor of
        // ours has stopped and given its ports back (or, still stopping,
        // left them to its successor: `LINGERING`).
        let left = *lingering();
        (self.chosen, self.source) =
            dante_slots::choose(self.params.ip, self.params.ports, left, |at| {
                UdpSocket::bind(at).is_ok()
            });
        self.config = Some(self.params.config_on(self.chosen));
        Ok(Opened {
            rate: f64::from(self.params.rate),
            buffer_frames: self.params.app_block(device.block),
            out_channels: DANTE_CHANNELS as u32,
            sample: AsioSample::Float32,
        })
    }

    fn start(&mut self, ring: rtrb::Consumer<f32>) -> Result<Started, Reason> {
        let ip = self.params.ip;
        self.start_with(ring, |port| dante_slots::port_holder(ip, port))
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
                *lingering() = Some(self.ports());
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
