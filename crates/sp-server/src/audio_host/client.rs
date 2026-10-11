//! #233: SongPlayer's side of the ASIO host's pipe.
//!
//! [`HostClient`] keeps every ASIO output SongPlayer runs (its own side, an
//! `AsioOut` built from the entry, as before) under a handle, and the link
//! to the host's current session:
//!
//! - [`HostClient::attach`] registers an output (the outputs task's starter,
//!   `client_win::start_remote`, then starts its forwarder): linked, the host
//!   is told at once (an add, on the control queue); not linked, the output
//!   shows why the host is down, if it is known to be.
//! - [`run_forwarder`] (one thread per output, as the worker was) takes the
//!   output's queue in order and hands each block to [`HostClient::send_block`]
//!   (never waits: a full data queue or a down host drops the block,
//!   counted); its stop (a removal, a replace, the shutdown) detaches the
//!   output — the host is told to discard its side.
//! - [`HostClient::session`] runs one connection: the hello and every
//!   registered output's add are queued BEFORE the link is published (so no
//!   block can precede its output's add), one writer sends the control queue
//!   before the data queue (biased), one reader applies the host's status
//!   reports to the outputs and re-logs its log lines. When either side
//!   ends, the link goes and every output reads waiting, `host_down`.
//!
//! The supervisor that starts the host process and runs the sessions with
//! the restart backoff (`restart.rs`) is [`supervise`]; the Windows glue
//! that spawns `sp-audio-host.exe` is `client_win.rs`.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, mpsc};
use tracing::{debug, error, info, trace, warn};

use crate::audio_host::protocol::{
    BlockFrame, Control, Frame, HostLevel, PROTOCOL, Report, read_frame, write_frame,
};
use crate::audio_host::restart::after_host_end;
use crate::playback::asio_out::AsioOut;
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::audio_out_queue::{Take, lock};
use crate::playback::vban_out::should_log;

/// The data queue's bound, frames (~4 s of blocks for two outputs).
pub const DATA_QUEUE: usize = 256;
/// How long a forwarder waits for a block before it looks again.
pub const FORWARD_POLL: Duration = Duration::from_millis(100);

/// One session's queues to the writer.
struct Link {
    ctrl: mpsc::UnboundedSender<Frame>,
    data: mpsc::Sender<Frame>,
}

#[derive(Default)]
struct Inner {
    next: u64,
    outs: BTreeMap<u64, Arc<AsioOut>>,
    link: Option<Link>,
    /// Why the host is down, once a start failed or a session ended
    /// (`None` before the first try and while linked).
    down: Option<String>,
    /// Blocks no session took (the host down, or its data queue full).
    unsent: u64,
}

/// SongPlayer's side of the ASIO host (module doc).
pub struct HostClient {
    inner: Mutex<Inner>,
    wake: Notify,
    /// SongPlayer's fleet relabel (`fleet_shift::global().slots()`).
    fleet_slots: fn() -> i64,
}

impl HostClient {
    pub fn new(fleet_slots: fn() -> i64) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            wake: Notify::new(),
            fleet_slots,
        }
    }

    /// Register `out`; returns its handle (module doc).
    pub fn attach(&self, out: Arc<AsioOut>) -> u64 {
        let mut inner = lock(&self.inner);
        inner.next += 1;
        let handle = inner.next;
        if let Some(link) = &inner.link {
            let _ = link.ctrl.send(Frame::Control(Control::Add {
                handle,
                spec: out.spec(),
            }));
        } else if let Some(why) = &inner.down {
            out.set_host_down(why);
        }
        inner.outs.insert(handle, out);
        drop(inner);
        self.wake.notify_one();
        handle
    }

    /// Forget output `handle`; a linked host discards its side.
    pub fn detach(&self, handle: u64) {
        let mut inner = lock(&self.inner);
        if inner.outs.remove(&handle).is_some()
            && let Some(link) = &inner.link
        {
            let _ = link.ctrl.send(Frame::Control(Control::Remove { handle }));
        }
    }

    /// The outputs registered now.
    pub fn outputs(&self) -> usize {
        lock(&self.inner).outs.len()
    }

    /// Blocks no session took so far.
    pub fn unsent_blocks(&self) -> u64 {
        lock(&self.inner).unsent
    }

    /// Hand output `handle`'s block to the session (never waits). `false`:
    /// no session took it (the host is down, or its queue is full).
    pub fn send_block(&self, handle: u64, block: ProgramBlock) -> bool {
        let frame = Frame::Block(BlockFrame::of(handle, block, (self.fleet_slots)()));
        let mut inner = lock(&self.inner);
        let sent = inner
            .link
            .as_ref()
            .is_some_and(|link| link.data.try_send(frame).is_ok());
        if !sent {
            inner.unsent += 1;
            let unsent = inner.unsent;
            drop(inner);
            log_unsent(handle, unsent);
        }
        sent
    }

    /// Wait until at least one output is registered.
    pub async fn outputs_wanted(&self) {
        while self.outputs() == 0 {
            self.wake.notified().await;
        }
    }

    /// The host is down (why): every output waits for it, and so does one
    /// attached until the next session.
    pub fn mark_down(&self, why: &str) {
        let mut inner = lock(&self.inner);
        inner.down = Some(why.to_string());
        let outs: Vec<Arc<AsioOut>> = inner.outs.values().cloned().collect();
        drop(inner);
        for out in outs {
            out.set_host_down(why);
        }
    }

    /// One host report.
    pub fn on_report(&self, report: Report) {
        match report {
            Report::Status { handle, snapshot } => {
                let out = lock(&self.inner).outs.get(&handle).cloned();
                if let Some(out) = out {
                    out.apply_remote(snapshot.into_snapshot());
                }
            }
            Report::Log { level, line } => emit_host_log(level, &line),
        }
    }

    /// Run one session over the host's pipe (module doc); returns why it
    /// ended. Every output reads waiting, `host_down`, afterwards.
    pub async fn session<R, W>(&self, mut reader: R, mut writer: W) -> String
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let (ctrl, mut ctrl_rx) = mpsc::unbounded_channel();
        let (data, mut data_rx) = mpsc::channel(DATA_QUEUE);
        {
            let mut inner = lock(&self.inner);
            let _ = ctrl.send(Frame::Control(Control::Hello { protocol: PROTOCOL }));
            for (&handle, out) in &inner.outs {
                let _ = ctrl.send(Frame::Control(Control::Add {
                    handle,
                    spec: out.spec(),
                }));
            }
            inner.link = Some(Link { ctrl, data });
            inner.down = None;
        }
        let why = tokio::select! {
            why = write_link(&mut writer, &mut ctrl_rx, &mut data_rx) => why,
            why = self.read_reports(&mut reader) => why,
        };
        lock(&self.inner).link = None;
        self.mark_down(&why);
        why
    }

    async fn read_reports<R: AsyncRead + Unpin>(&self, reader: &mut R) -> String {
        loop {
            match read_frame(reader).await {
                Ok(Some(Frame::Report(report))) => self.on_report(report),
                Ok(Some(_)) => return "the ASIO host sent a frame only SongPlayer sends".into(),
                Ok(None) => return "the ASIO host closed its pipe".into(),
                Err(e) => return format!("reading the ASIO host's pipe failed: {e}"),
            }
        }
    }
}

/// The session's writer: the control queue first, then the data queue.
async fn write_link<W: AsyncWrite + Unpin>(
    writer: &mut W,
    ctrl: &mut mpsc::UnboundedReceiver<Frame>,
    data: &mut mpsc::Receiver<Frame>,
) -> String {
    loop {
        let frame = tokio::select! {
            biased;
            Some(frame) = ctrl.recv() => frame,
            Some(frame) = data.recv() => frame,
            else => return "the session's queues closed".into(),
        };
        if let Err(e) = write_frame(writer, &frame).await {
            return format!("writing the ASIO host's pipe failed: {e}");
        }
    }
}

/// An output's forwarder (module doc): its queue to the host, in order,
/// until the output stops; then it is detached.
pub fn run_forwarder(client: &HostClient, handle: u64, out: &AsioOut) {
    let running = out.run_guard();
    loop {
        match out.take_block(FORWARD_POLL) {
            Take::Block(block) => {
                client.send_block(handle, block);
            }
            Take::Idle => {}
            Take::Stopped => break,
        }
    }
    client.detach(handle);
    drop(running);
    info!(id = %out.id(), handle, "asio output: forwarder stopped");
}

/// Start the host and run its sessions, for as long as the process lives:
/// wait for an output, `launch` the host (its pipe's two halves and a guard
/// that ends the process when dropped), serve the session, then mark the
/// outputs down and wait the backoff (`restart.rs`) before the next start.
#[cfg_attr(test, mutants::skip)] // a restart loop around the tested session and backoff
pub async fn supervise<L, F, R, W, G>(client: Arc<HostClient>, launch: L)
where
    L: Fn() -> F,
    F: Future<Output = Result<(R, W, G), String>>,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut failures = 0;
    loop {
        client.outputs_wanted().await;
        let started = Instant::now();
        let why = match launch().await {
            Ok((reader, writer, guard)) => {
                info!("asio host: connected - every ASIO output runs in sp-audio-host");
                let why = client.session(reader, writer).await;
                drop(guard);
                why
            }
            Err(why) => why,
        };
        let ran = i64::try_from(started.elapsed().as_nanos() / 100).unwrap_or(i64::MAX);
        let (now_failures, wait) = after_host_end(failures, ran);
        failures = now_failures;
        warn!(
            %why,
            failures,
            retry_in_s = wait / 10_000_000,
            "asio host: not running - the ASIO outputs wait; it is started again after the backoff"
        );
        client.mark_down(&why);
        tokio::time::sleep(Duration::from_nanos(wait as u64 * 100)).await;
    }
}

/// A block no session took: the first and every 1000th WARNed.
#[cfg_attr(test, mutants::skip)] // logging only; the count is tested (`unsent_blocks`)
fn log_unsent(handle: u64, unsent: u64) {
    if should_log(unsent) {
        warn!(
            handle,
            unsent,
            "asio output: a block was not sent to the ASIO host (it is down, or its queue is full)"
        );
    }
}

/// One of the host's log lines, re-logged in SongPlayer's log at its level.
#[cfg_attr(test, mutants::skip)] // logging only
fn emit_host_log(level: HostLevel, line: &str) {
    match level {
        HostLevel::Error => error!(target: "sp_audio_host", "{line}"),
        HostLevel::Warn => warn!(target: "sp_audio_host", "{line}"),
        HostLevel::Info => info!(target: "sp_audio_host", "{line}"),
        HostLevel::Debug => debug!(target: "sp_audio_host", "{line}"),
        HostLevel::Trace => trace!(target: "sp_audio_host", "{line}"),
    }
}
