//! #233: the ASIO host's side of the pipe (`sp-audio-host`, which runs
//! every ASIO output and spawns no process).
//!
//! [`serve`] reads SongPlayer's frames in order, one at a time (a read is
//! never raced against anything, so no frame is ever cut): the hello first
//! (its protocol must be this one), then outputs added — each an `AsioOut`
//! built from its spec and its worker started (`HostEnv::start`: the real
//! `asio_win` worker in the host, a recorder or a fake-device worker in the
//! tests) — blocks pushed into their output's queue with the stamp moved
//! onto the host's timeline (`stamp.rs`), and outputs removed (discarded,
//! as the outputs task discards a replaced one). A status task reports
//! every output's snapshot every `status_every` (~2 Hz), and the reports
//! (statuses and the host's log lines, `log_forward.rs`) go out through ONE
//! writer task. The pipe's end (SongPlayer closed it or exited) or a
//! malformed frame ends the session: every output is discarded (its worker
//! releases its driver) and [`serve`] returns them, so the host's main can
//! wait for the releases before it exits.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::audio_host::protocol::{Control, Frame, PROTOCOL, Report, read_frame, write_frame};
use crate::audio_host::stamp::StampMap;
use crate::audio_host::wire_status::WireSnapshot;
use crate::playback::asio_out::AsioOut;
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::audio_out_queue::lock;

/// How often the host reports its outputs' status.
pub const STATUS_EVERY: Duration = Duration::from_millis(500);
/// The reports waiting for the writer (statuses and log lines).
pub const REPORT_QUEUE: usize = 256;

/// Starts an added output's worker.
pub type StartWorker = dyn Fn(Arc<AsioOut>) + Send + Sync;

/// What the host's frame loop needs from its process.
pub struct HostEnv {
    pub start: Box<StartWorker>,
    /// The host's clock for the stamp map's settling (100 ns).
    pub now_100ns: Box<dyn Fn() -> i64 + Send + Sync>,
    /// The host's fleet relabel (whole slots, `fleet_shift::global`).
    pub fleet_slots: Box<dyn Fn() -> i64 + Send + Sync>,
    pub status_every: Duration,
}

/// Why a session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostEnd {
    /// SongPlayer closed the pipe (or exited).
    Eof,
    /// A frame could not be read, or broke the protocol.
    Failed(String),
}

/// The outputs of one session, by SongPlayer's handle.
pub type Outputs = Arc<Mutex<BTreeMap<u64, Arc<AsioOut>>>>;

/// The frame loop's state.
pub struct HostState {
    hello: bool,
    outs: Outputs,
    stamps: StampMap,
}

impl HostState {
    pub fn new(outs: Outputs) -> Self {
        Self {
            hello: false,
            outs,
            stamps: StampMap::default(),
        }
    }

    /// The session's outputs (shared with its status task).
    pub fn outputs(&self) -> Outputs {
        self.outs.clone()
    }
}

/// One frame from SongPlayer; an `Err` ends the session (why).
pub fn on_frame(state: &mut HostState, env: &HostEnv, frame: Frame) -> Result<(), String> {
    match frame {
        Frame::Control(Control::Hello { protocol }) => {
            if protocol != PROTOCOL {
                return Err(format!(
                    "SongPlayer speaks protocol {protocol}, this host {PROTOCOL}"
                ));
            }
            state.hello = true;
            Ok(())
        }
        _ if !state.hello => Err("a frame before the hello".into()),
        Frame::Control(Control::Add { handle, spec }) => {
            info!(handle, id = %spec.id, driver = %spec.driver, "asio host: output added");
            let out = Arc::new(AsioOut::from_spec(&spec));
            let old = lock(&state.outs).insert(handle, out.clone());
            if let Some(old) = old {
                old.discard();
            }
            (env.start)(out);
            Ok(())
        }
        Frame::Control(Control::Remove { handle }) => {
            if let Some(out) = lock(&state.outs).remove(&handle) {
                info!(handle, id = %out.id(), "asio host: output removed");
                out.discard();
            }
            Ok(())
        }
        Frame::Block(b) => {
            let due = state.stamps.map(
                b.due_100ns,
                b.fleet_slots,
                (env.fleet_slots)(),
                (env.now_100ns)(),
            );
            let out = lock(&state.outs).get(&b.handle).cloned();
            if let Some(out) = out {
                out.push(ProgramBlock {
                    due_100ns: due,
                    samples: b.samples,
                    substituted: b.substituted,
                });
            }
            Ok(())
        }
        Frame::Report(_) => Err("SongPlayer sent a report frame".into()),
    }
}

/// Every output's status now, in handle order.
pub fn status_reports(outs: &Outputs) -> Vec<Report> {
    lock(outs)
        .iter()
        .map(|(&handle, out)| Report::Status {
            handle,
            snapshot: Box::new(WireSnapshot::from(&out.snapshot())),
        })
        .collect()
}

/// Serve one session (module doc). `reports` / `report_tx` are the report
/// queue: the host's log layer holds another sender of it. Returns why it
/// ended and the outputs it discarded.
pub async fn serve<R, W>(
    mut reader: R,
    writer: W,
    env: HostEnv,
    reports: mpsc::Receiver<Report>,
    report_tx: mpsc::Sender<Report>,
) -> (HostEnd, Vec<Arc<AsioOut>>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let outs: Outputs = Arc::default();
    let writer = tokio::spawn(write_reports(writer, reports));
    let status = tokio::spawn(report_status(outs.clone(), report_tx, env.status_every));
    let mut state = HostState::new(outs.clone());
    let end = loop {
        match read_frame(&mut reader).await {
            Ok(None) => break HostEnd::Eof,
            Err(e) => break HostEnd::Failed(e.to_string()),
            Ok(Some(frame)) => {
                if let Err(why) = on_frame(&mut state, &env, frame) {
                    break HostEnd::Failed(why);
                }
            }
        }
    };
    status.abort();
    writer.abort();
    let discarded: Vec<Arc<AsioOut>> = {
        let mut held = lock(&outs);
        let all = held.values().cloned().collect();
        held.clear();
        all
    };
    for out in &discarded {
        out.discard();
    }
    if let HostEnd::Failed(why) = &end {
        warn!(%why, "asio host: the session ended on a protocol error");
    }
    (end, discarded)
}

/// The report writer: every queued report, in order, until the pipe fails
/// (or the session aborts it).
async fn write_reports<W: AsyncWrite + Unpin>(mut writer: W, mut reports: mpsc::Receiver<Report>) {
    while let Some(report) = reports.recv().await {
        if write_frame(&mut writer, &Frame::Report(report))
            .await
            .is_err()
        {
            return;
        }
    }
}

/// Every `every`, every output's status into the report queue.
async fn report_status(outs: Outputs, tx: mpsc::Sender<Report>, every: Duration) {
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        for report in status_reports(&outs) {
            if tx.send(report).await.is_err() {
                return;
            }
        }
    }
}
