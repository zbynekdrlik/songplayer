//! #233: the ASIO host's side of the pipe (`sp-audio-host`, which runs
//! every ASIO output and spawns no process).
//!
//! [`serve`] reads SongPlayer's frames in order, one at a time (a read is
//! never raced against anything, so no frame is ever cut): the hello first
//! (its protocol must be this one; its time anchor starts the session's
//! clock), then outputs added — each an `AsioOut` built from its spec and its
//! worker started on a [`RemoteClock`] (`HostEnv::start`: the real `asio_win`
//! worker in the host, a recorder or a fake-device worker in the tests) —
//! blocks pushed into their output's queue with their boundary untouched and
//! their anchor taken as the session's latest (`remote_clock.rs`: the host
//! has no wall of its own, so no relabel of its own), and outputs removed
//! (discarded, as the outputs task discards a replaced one). A status task
//! reports "alive" and every output's snapshot with its start error every
//! `status_every` (~2 Hz), and the reports (statuses and the host's log
//! lines, `log_forward.rs`) go out through ONE writer task. The pipe's end
//! (SongPlayer closed it or exited) or a malformed frame ends the session —
//! a malformed one with an `Ending` report naming why, written before the
//! writer stops: every output is discarded (its worker releases its driver)
//! and [`serve`] returns them, so the host's main can wait for the releases
//! before it exits.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::audio_host::protocol::{Control, Frame, PROTOCOL, Report, read_frame, write_frame};
use crate::audio_host::remote_clock::{Mono, RemoteClock, SharedAnchor};
use crate::audio_host::wire_status::WireSnapshot;
use crate::playback::asio_out::AsioOut;
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::audio_out_queue::lock;

/// How often the host reports its outputs' status (and that it is alive).
pub const STATUS_EVERY: Duration = Duration::from_millis(500);
/// The reports waiting for the writer (statuses and log lines).
pub const REPORT_QUEUE: usize = 256;
/// How long the writer gets to write the `Ending` report.
pub const ENDING_WAIT: Duration = Duration::from_secs(1);

/// Starts an added output's worker on SongPlayer's timeline.
pub type StartWorker = dyn Fn(Arc<AsioOut>, RemoteClock) + Send + Sync;

/// What the host's frame loop needs from its process.
pub struct HostEnv {
    pub start: Box<StartWorker>,
    /// The box's monotonic counter (`mono::mono_100ns`).
    pub mono: Mono,
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
    anchor: Arc<SharedAnchor>,
}

impl HostState {
    pub fn new(outs: Outputs) -> Self {
        Self {
            hello: false,
            outs,
            anchor: Arc::default(),
        }
    }

    /// The session's outputs (shared with its status task).
    pub fn outputs(&self) -> Outputs {
        self.outs.clone()
    }

    /// The session's latest time anchor (every worker's clock reads it).
    pub fn anchor(&self) -> Arc<SharedAnchor> {
        self.anchor.clone()
    }
}

/// One frame from SongPlayer; an `Err` ends the session (why).
pub fn on_frame(state: &mut HostState, env: &HostEnv, frame: Frame) -> Result<(), String> {
    match frame {
        Frame::Control(Control::Hello { protocol, anchor }) => {
            if protocol != PROTOCOL {
                return Err(format!(
                    "SongPlayer speaks protocol {protocol}, this host {PROTOCOL}"
                ));
            }
            state.anchor.set(anchor);
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
            (env.start)(out, RemoteClock::new(state.anchor(), env.mono.clone()));
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
            state.anchor.set(b.anchor);
            let out = lock(&state.outs).get(&b.handle).cloned();
            if let Some(out) = out {
                out.push(ProgramBlock {
                    due_100ns: b.due_100ns,
                    samples: b.samples,
                    substituted: b.substituted,
                });
            }
            Ok(())
        }
        Frame::Report(_) => Err("SongPlayer sent a report frame".into()),
    }
}

/// "Alive", then every output's status and start error now, in handle order.
pub fn status_reports(outs: &Outputs) -> Vec<Report> {
    let statuses = lock(outs)
        .iter()
        .map(|(&handle, out)| Report::Status {
            handle,
            snapshot: Box::new(WireSnapshot::of(&out.snapshot(), out.start_error())),
        })
        .collect::<Vec<_>>();
    [vec![Report::Alive], statuses].concat()
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
    let mut writer = tokio::spawn(write_reports(writer, reports));
    let status = tokio::spawn(report_status(
        outs.clone(),
        report_tx.clone(),
        env.status_every,
    ));
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
    if let HostEnd::Failed(why) = &end {
        warn!(%why, "asio host: the session ended on a protocol error");
        // Tell SongPlayer why, before the writer stops (bounded: a stuck
        // pipe never holds the end).
        let ending = report_tx.send(Report::Ending { why: why.clone() });
        let _ = tokio::time::timeout(ENDING_WAIT, ending).await;
        let _ = tokio::time::timeout(ENDING_WAIT, &mut writer).await;
    }
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
    (end, discarded)
}

/// The report writer: every queued report, in order, until the pipe fails,
/// the session's `Ending` is written, or the session aborts it.
async fn write_reports<W: AsyncWrite + Unpin>(mut writer: W, mut reports: mpsc::Receiver<Report>) {
    while let Some(report) = reports.recv().await {
        let ending = matches!(report, Report::Ending { .. });
        if write_frame(&mut writer, &Frame::Report(report))
            .await
            .is_err()
        {
            return;
        }
        if ending {
            return;
        }
    }
}

/// Every `every`, "alive" and every output's status into the report queue.
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
