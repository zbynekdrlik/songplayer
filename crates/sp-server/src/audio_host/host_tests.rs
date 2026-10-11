//! #233: SongPlayer's client and the ASIO host's frame loop, end to end over
//! an in-memory pipe (`tokio::io::duplex`): an output added reaches the
//! host, its blocks reach the host's output in order with their boundary
//! untouched, the host's clock reads SongPlayer's anchors, the host's status
//! (and a worker's start error) comes back as the output's, a real worker
//! over the scripted driver plays them, and either side's end — or a host's
//! silence — is seen by the other.
//!
//! The pipe's tasks run on their own multi-thread runtime; the test thread
//! makes the blocking reads (`take_block`) and bounded waits (`wait_until`,
//! 20 s), never a wall-time threshold.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::audio_host::client::{HostClient, run_forwarder};
use crate::audio_host::host::{
    HostEnd, HostEnv, HostState, REPORT_QUEUE, on_frame, serve, status_reports,
};
use crate::audio_host::mono::mono_100ns;
use crate::audio_host::protocol::{
    BlockFrame, Control, Frame, HostLevel, PROTOCOL, Report, read_frame, write_frame,
};
use crate::audio_host::remote_clock::{Anchor, Mono, RemoteClock};
use crate::audio_host::wire_status::WireSnapshot;
use crate::playback::asio_format::AsioSample;
use crate::playback::asio_out::fake::FakeDevice;
use crate::playback::asio_out::{
    AsioDevice, AsioOut, AsioSnapshot, AsioSpec, AsioStatus, Opened, Started, run_asio_worker,
};
use crate::playback::asio_state::{DeviceEvents, Reason};
use crate::playback::audio_out::{OutputSink, RunningOutput};
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::audio_out_queue::Take;
use crate::playback::program_max_worker::tests::wait_until;

const WAIT: Duration = Duration::from_secs(20);

fn spec() -> AsioSpec {
    AsioSpec {
        id: "out-2".into(),
        driver: "Waves SoundGrid".into(),
        channels: [0, 1],
        delay_ms: 0,
    }
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

type Halves = (ReadHalf<DuplexStream>, WriteHalf<DuplexStream>);

/// A pipe: SongPlayer's end, the host's end.
fn pipe() -> (Halves, Halves) {
    let (sp, host) = tokio::io::duplex(1 << 20);
    (tokio::io::split(sp), tokio::io::split(host))
}

/// The host's outputs and their clocks as its starter got them.
#[derive(Default)]
struct Rec {
    outs: Mutex<Vec<Arc<AsioOut>>>,
    clocks: Mutex<Vec<RemoteClock>>,
}

type Built = Arc<Rec>;

/// A counter the test sets (the host's monotonic counter).
fn counter(at: i64) -> (Arc<AtomicI64>, Mono) {
    let value = Arc::new(AtomicI64::new(at));
    let read = value.clone();
    (value, Arc::new(move || read.load(Ordering::SeqCst)))
}

/// A host whose starter records each output and its clock (no worker), on
/// the counter `mono`.
fn recording_env(rec: &Built, mono: Mono) -> HostEnv {
    let rec = rec.clone();
    HostEnv {
        start: Box::new(move |out: Arc<AsioOut>, clock: RemoteClock| {
            rec.outs.lock().unwrap().push(out);
            rec.clocks.lock().unwrap().push(clock);
        }),
        mono,
        status_every: Duration::from_millis(20),
    }
}

type Served = JoinHandle<(HostEnd, Vec<Arc<AsioOut>>)>;

fn spawn_host(rt: &Runtime, (reader, writer): Halves, env: HostEnv) -> Served {
    let (tx, rx) = mpsc::channel(REPORT_QUEUE);
    rt.spawn(serve(reader, writer, env, rx, tx))
}

/// SongPlayer's anchors 0, 1, 2 … : timeline 5 000 000 + 1 000·n at counter
/// 100 + n.
fn counting_anchors() -> impl FnMut() -> Anchor + Send + 'static {
    let mut n = 0;
    move || {
        let a = Anchor {
            timeline_100ns: 5_000_000 + 1_000 * n,
            mono_100ns: 100 + n,
        };
        n += 1;
        a
    }
}

fn spawn_session<A>(
    rt: &Runtime,
    client: &Arc<HostClient>,
    (reader, writer): Halves,
    mut anchor: A,
    silence: Duration,
) -> JoinHandle<String>
where
    A: FnMut() -> Anchor + Send + 'static,
{
    let client = client.clone();
    rt.spawn(async move { client.session(reader, writer, &mut anchor, silence).await })
}

fn spawn_forwarder(
    client: &Arc<HostClient>,
    handle: u64,
    out: &Arc<AsioOut>,
) -> std::thread::JoinHandle<()> {
    let (client, out) = (client.clone(), out.clone());
    std::thread::spawn(move || run_forwarder(&client, handle, &out))
}

fn first_started(rec: &Built) -> Arc<AsioOut> {
    wait_until("the host built the output", || {
        !rec.outs.lock().unwrap().is_empty()
    });
    rec.outs.lock().unwrap()[0].clone()
}

fn ramp(k: i64) -> Arc<[f32]> {
    (0..3200).map(|i| (k * 3200 + i) as f32 * 1e-6).collect()
}

/// The blocks a forwarder takes from SongPlayer's output reach the host's
/// output in order, their samples whole and their boundary UNTOUCHED; the
/// host's clock reads SongPlayer's latest anchor (the sixth block's) on its
/// own counter; the output's stop removes it on the host.
#[test]
fn blocks_reach_the_hosts_output_in_order_on_songplayers_timeline() {
    let rt = runtime();
    let (sp, host) = pipe();
    let rec = Built::default();
    let (_, mono) = counter(1_000);
    let served = spawn_host(&rt, host, recording_env(&rec, mono));
    let client = Arc::new(HostClient::default());
    let out = Arc::new(AsioOut::from_spec(&spec()));
    let handle = client.attach(out.clone());
    let _session = spawn_session(&rt, &client, sp, counting_anchors(), WAIT);
    let host_out = first_started(&rec);
    assert_eq!(
        host_out.spec(),
        spec(),
        "the host built it from the same spec"
    );
    assert_eq!(client.outputs(), 1);
    let forwarder = spawn_forwarder(&client, handle, &out);
    for k in 0..6 {
        let samples = (k != 2).then(|| ramp(k));
        out.push(ProgramBlock {
            due_100ns: 1_000_000 + k,
            samples,
            substituted: k == 4,
        });
    }
    for k in 0..6 {
        let Take::Block(b) = host_out.take_block(WAIT) else {
            panic!("block {k} did not reach the host");
        };
        assert_eq!(b.due_100ns, 1_000_000 + k, "block {k}: its boundary as sent");
        assert_eq!(b.samples, (k != 2).then(|| ramp(k)), "block {k}");
        assert_eq!(b.substituted, k == 4, "block {k}");
    }
    // The hello took anchor 0, the six blocks 1 to 6: 5 006 000 at 106,
    // read at the host's 1 000.
    let mut clock = rec.clocks.lock().unwrap().remove(0);
    assert_eq!(clock.read(), 5_006_000 + (1_000 - 106));
    out.stop();
    forwarder.join().unwrap();
    assert_eq!(client.outputs(), 0, "the stopped output is detached");
    assert_eq!(
        host_out.take_block(WAIT),
        Take::Stopped,
        "the host discarded its side"
    );
    assert_eq!(out.snapshot().blocks_dropped, 0);
    drop(served);
}

/// The host's status of an output is SongPlayer's status of it; a host
/// worker's start error becomes the output's, so the outputs task rebuilds
/// it (never "running" with frozen figures).
#[test]
fn the_hosts_status_and_start_error_are_the_outputs_in_songplayer() {
    let rt = runtime();
    let (sp, host) = pipe();
    let rec = Built::default();
    let (_, mono) = counter(0);
    let _served = spawn_host(&rt, host, recording_env(&rec, mono));
    let client = Arc::new(HostClient::default());
    let out = Arc::new(AsioOut::from_spec(&spec()));
    client.attach(out.clone());
    let _session = spawn_session(&rt, &client, sp, counting_anchors(), WAIT);
    let host_out = first_started(&rec);
    let reported = AsioSnapshot {
        state: "running",
        reason: None,
        status: AsioStatus {
            driver: "Waves SoundGrid".into(),
            driver_rate: 96_000,
            buffer_frames: 512,
            sample_type: "Int32LSB",
            ppm: -3.5,
            latency_ms: 70.6,
            resyncs: 4,
            hard_recentres: 2,
            ..AsioStatus::default()
        },
        blocks_sent: 120,
        blocks_dropped: 3,
    };
    host_out.apply_remote(reported.clone());
    wait_until("the host's status reached SongPlayer", || {
        out.snapshot() == reported
    });
    let running = RunningOutput {
        entry: sp_core::audio_outputs::OutputEntry::asio(
            "out-2",
            "SG",
            sp_core::audio_outputs::AsioDest {
                driver: "Waves SoundGrid".into(),
                channels: [0, 1],
            },
        ),
        built_rate: 0,
        sink: Some(OutputSink::Asio(out.clone())),
        error: None,
    };
    assert!(!running.start_failed());
    host_out.set_start_error("the ASIO thread stopped: it panicked".into());
    wait_until("the host worker's start error reached SongPlayer", || {
        out.start_error().is_some()
    });
    assert_eq!(
        out.start_error().as_deref(),
        Some("sp-audio-host: the ASIO thread stopped: it panicked")
    );
    assert!(running.start_failed(), "the outputs task rebuilds it");
}

/// SongPlayer's end of the pipe gone (it exited): the host's loop ends and
/// discards every output, so each worker releases its driver.
#[test]
fn songplayers_end_ends_the_host_and_discards_its_outputs() {
    let rt = runtime();
    let (sp, host) = pipe();
    let rec = Built::default();
    let (_, mono) = counter(0);
    let served = spawn_host(&rt, host, recording_env(&rec, mono));
    let client = Arc::new(HostClient::default());
    client.attach(Arc::new(AsioOut::from_spec(&spec())));
    let session = spawn_session(&rt, &client, sp, counting_anchors(), WAIT);
    let host_out = first_started(&rec);
    session.abort();
    let (end, discarded) = rt.block_on(served).unwrap();
    assert_eq!(end, HostEnd::Eof);
    assert_eq!(discarded.len(), 1);
    assert!(Arc::ptr_eq(&discarded[0], &host_out));
    assert_eq!(host_out.take_block(WAIT), Take::Stopped);
}

/// The session opens with the hello (SongPlayer's first anchor), then every
/// output's add; the host's end of the pipe gone: the session ends, every
/// output reads waiting, `host_down`, and one attached afterwards too; a
/// block then finds no session.
#[test]
fn the_hosts_end_leaves_every_output_waiting_for_it() {
    let rt = runtime();
    let (sp, (mut reader, writer)) = pipe();
    let client = Arc::new(HostClient::default());
    let out = Arc::new(AsioOut::from_spec(&spec()));
    let handle = client.attach(out.clone());
    assert_eq!(out.snapshot().state, "opening", "no host is known down yet");
    let session = spawn_session(&rt, &client, sp, counting_anchors(), WAIT);
    let first = rt.block_on(read_frame(&mut reader)).unwrap();
    assert_eq!(
        first,
        Some(Frame::Control(Control::Hello {
            protocol: PROTOCOL,
            anchor: Anchor {
                timeline_100ns: 5_000_000,
                mono_100ns: 100,
            },
        }))
    );
    let second = rt.block_on(read_frame(&mut reader)).unwrap();
    assert_eq!(
        second,
        Some(Frame::Control(Control::Add {
            handle,
            spec: spec()
        }))
    );
    drop((reader, writer));
    let why = rt.block_on(session).unwrap();
    assert_eq!(why, "the ASIO host closed its pipe");
    let s = out.snapshot();
    assert_eq!(
        (s.state, s.status.reason_code),
        ("waiting", Some("host_down"))
    );
    assert_eq!(s.reason, Some(Reason::HostDown(why.clone())));
    let late = Arc::new(AsioOut::from_spec(&AsioSpec {
        id: "out-3".into(),
        driver: "DVS".into(),
        ..spec()
    }));
    assert_eq!(
        client.attach(late.clone()),
        handle + 1,
        "each output its own handle"
    );
    assert_eq!(late.snapshot().reason, Some(Reason::HostDown(why)));
    assert!(!client.send_block(handle, ProgramBlock::silence(0)));
}

/// A host that sends nothing (it reports every 500 ms) hangs: the session
/// ends after the silence bound, every output waiting `host_down`.
#[test]
fn a_silent_host_ends_the_session() {
    let rt = runtime();
    let (sp, (reader, writer)) = pipe();
    let client = Arc::new(HostClient::default());
    let out = Arc::new(AsioOut::from_spec(&spec()));
    client.attach(out.clone());
    let session = spawn_session(
        &rt,
        &client,
        sp,
        counting_anchors(),
        Duration::from_millis(50),
    );
    let why = rt.block_on(session).unwrap();
    assert_eq!(why, "the ASIO host sent nothing for 50 ms (it hangs)");
    assert_eq!(out.snapshot().reason, Some(Reason::HostDown(why)));
    drop((reader, writer));
}

/// "Alive" keeps a session open; a host's `Ending` ends it with its reason.
#[test]
fn a_hosts_ending_ends_the_session_with_its_reason() {
    let rt = runtime();
    let (sp, (reader, mut writer)) = pipe();
    let client = Arc::new(HostClient::default());
    let session = spawn_session(&rt, &client, sp, counting_anchors(), WAIT);
    for report in [
        Report::Alive,
        Report::Ending {
            why: "a frame before the hello".into(),
        },
    ] {
        rt.block_on(write_frame(&mut writer, &Frame::Report(report)))
            .unwrap();
    }
    assert_eq!(
        rt.block_on(session).unwrap(),
        "the ASIO host ended the session: a frame before the hello"
    );
    drop(reader);
}

/// A frame that breaks the protocol ends the host's session, and the host
/// says why before it stops writing.
#[test]
fn the_host_says_why_it_ends_the_session() {
    let rt = runtime();
    let ((mut reader, mut writer), host) = pipe();
    let rec = Built::default();
    let (_, mono) = counter(0);
    let served = spawn_host(&rt, host, recording_env(&rec, mono));
    let block = Frame::Block(BlockFrame::of(1, ProgramBlock::silence(0)));
    rt.block_on(write_frame(&mut writer, &block)).unwrap();
    let (end, _) = rt.block_on(served).unwrap();
    assert_eq!(end, HostEnd::Failed("a frame before the hello".into()));
    let mut ending = None;
    for _ in 0..64 {
        match rt.block_on(read_frame(&mut reader)).unwrap() {
            Some(Frame::Report(Report::Alive)) => {}
            Some(Frame::Report(Report::Ending { why })) => {
                ending = Some(why);
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(ending.as_deref(), Some("a frame before the hello"));
}

/// The supervisor starts the host only once an output is wanted: with none
/// attached the wait holds, an attach ends it.
#[tokio::test]
async fn the_host_is_wanted_once_an_output_is_attached() {
    let client = HostClient::default();
    let wanted = client.outputs_wanted();
    tokio::pin!(wanted);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut wanted)
            .await
            .is_err(),
        "no output: no host wanted"
    );
    client.attach(Arc::new(AsioOut::from_spec(&spec())));
    tokio::time::timeout(WAIT, &mut wanted)
        .await
        .expect("an output attached: the host is wanted");
}

/// A host that writes a SongPlayer frame breaks the protocol: the session
/// ends with that reason.
#[test]
fn a_host_that_sends_a_control_frame_ends_the_session() {
    let rt = runtime();
    let (sp, (reader, mut writer)) = pipe();
    let client = Arc::new(HostClient::default());
    let session = spawn_session(&rt, &client, sp, counting_anchors(), WAIT);
    rt.block_on(write_frame(
        &mut writer,
        &Frame::Control(Control::Remove { handle: 1 }),
    ))
    .unwrap();
    assert_eq!(
        rt.block_on(session).unwrap(),
        "the ASIO host sent a frame only SongPlayer sends"
    );
    drop(reader);
}

/// A forwarder whose block no session takes (the host down) counts it on the
/// OUTPUT: `blocks_dropped`, as the API and the gates read it.
#[test]
fn a_block_no_session_takes_is_dropped_on_its_output() {
    let client = Arc::new(HostClient::default());
    let out = Arc::new(AsioOut::from_spec(&spec()));
    let handle = client.attach(out.clone());
    let forwarder = spawn_forwarder(&client, handle, &out);
    for k in 0..3 {
        out.push(ProgramBlock::silence(k));
    }
    wait_until("the forwarder counted the blocks", || {
        out.snapshot().blocks_dropped == 3
    });
    out.stop();
    forwarder.join().unwrap();
    assert_eq!(out.snapshot().blocks_dropped, 3);
}

fn hello(anchor: Anchor) -> Frame {
    Frame::Control(Control::Hello {
        protocol: PROTOCOL,
        anchor,
    })
}

fn add(handle: u64) -> Frame {
    Frame::Control(Control::Add {
        handle,
        spec: spec(),
    })
}

/// The hello comes first and must speak this protocol; SongPlayer never
/// sends a report.
#[test]
fn the_host_refuses_a_frame_out_of_the_protocol() {
    let rec = Built::default();
    let (_, mono) = counter(0);
    let env = recording_env(&rec, mono);
    let mut s = HostState::new(Arc::default());
    let block = Frame::Block(BlockFrame::of(1, ProgramBlock::silence(0)));
    assert_eq!(
        on_frame(&mut s, &env, block.clone()),
        Err("a frame before the hello".into())
    );
    assert_eq!(
        on_frame(&mut s, &env, add(1)),
        Err("a frame before the hello".into())
    );
    let other = Frame::Control(Control::Hello {
        protocol: 1,
        anchor: Anchor::default(),
    });
    assert_eq!(
        on_frame(&mut s, &env, other),
        Err(format!("SongPlayer speaks protocol 1, this host {PROTOCOL}"))
    );
    assert_eq!(on_frame(&mut s, &env, hello(Anchor::default())), Ok(()));
    assert_eq!(
        on_frame(&mut s, &env, block),
        Ok(()),
        "an unknown handle's block is dropped"
    );
    let report = Frame::Report(Report::Log {
        level: HostLevel::Info,
        line: "x".into(),
    });
    assert_eq!(
        on_frame(&mut s, &env, report),
        Err("SongPlayer sent a report frame".into())
    );
    assert!(rec.outs.lock().unwrap().is_empty());
}

/// The hello's and every block's anchor become the session's: every
/// worker's clock reads SongPlayer's timeline on the host's counter, and a
/// block's boundary goes into its output as sent.
#[test]
fn the_sessions_clock_is_songplayers_latest_anchor() {
    let rec = Built::default();
    let (host_mono, mono) = counter(2_000);
    let env = recording_env(&rec, mono);
    let mut s = HostState::new(Arc::default());
    let first = Anchor {
        timeline_100ns: 40_000_000,
        mono_100ns: 1_500,
    };
    on_frame(&mut s, &env, hello(first)).unwrap();
    on_frame(&mut s, &env, add(3)).unwrap();
    let mut clock = rec.clocks.lock().unwrap().remove(0);
    assert_eq!(clock.read(), 40_000_500);
    let block = BlockFrame {
        anchor: Anchor {
            timeline_100ns: 40_100_000,
            mono_100ns: 2_100,
        },
        ..BlockFrame::of(3, ProgramBlock::silence(777))
    };
    on_frame(&mut s, &env, Frame::Block(block)).unwrap();
    host_mono.store(2_300, Ordering::SeqCst);
    assert_eq!(clock.read(), 40_100_200);
    let out = rec.outs.lock().unwrap()[0].clone();
    assert_eq!(
        out.take_block(Duration::ZERO),
        Take::Block(ProgramBlock::silence(777))
    );
}

/// An add starts a worker; a second add under the same handle discards the
/// first; a remove discards and forgets it. The status reports open with
/// "alive" and carry each output's start error.
#[test]
fn the_host_adds_replaces_and_removes_outputs() {
    let rec = Built::default();
    let (_, mono) = counter(0);
    let env = recording_env(&rec, mono);
    let mut s = HostState::new(Arc::default());
    on_frame(&mut s, &env, hello(Anchor::default())).unwrap();
    on_frame(&mut s, &env, add(7)).unwrap();
    on_frame(&mut s, &env, add(7)).unwrap();
    let (first, second) = {
        let v = rec.outs.lock().unwrap();
        assert_eq!(v.len(), 2);
        (v[0].clone(), v[1].clone())
    };
    assert_eq!(first.take_block(Duration::ZERO), Take::Stopped);
    assert_eq!(second.take_block(Duration::ZERO), Take::Idle);
    second.set_start_error("spawning it failed".into());
    assert_eq!(
        status_reports(&s.outputs()),
        [
            Report::Alive,
            Report::Status {
                handle: 7,
                snapshot: Box::new(WireSnapshot::of(
                    &second.snapshot(),
                    Some("spawning it failed".into())
                )),
            }
        ]
    );
    on_frame(&mut s, &env, Frame::Control(Control::Remove { handle: 7 })).unwrap();
    assert_eq!(second.take_block(Duration::ZERO), Take::Stopped);
    assert_eq!(status_reports(&s.outputs()), [Report::Alive]);
    on_frame(&mut s, &env, Frame::Control(Control::Remove { handle: 7 })).unwrap();
}

/// A scripted driver whose card plays on its own: every poll it takes four
/// callbacks, and what it played is shared with the test.
struct AutoCard {
    inner: FakeDevice,
    played: Arc<Mutex<Vec<f32>>>,
}

impl AsioDevice for AutoCard {
    fn open(&mut self, driver: &str, channels: [u32; 2]) -> Result<Opened, Reason> {
        self.inner.open(driver, channels)
    }
    fn start(&mut self, ring: rtrb::Consumer<f32>) -> Result<Started, Reason> {
        self.inner.start(ring)
    }
    fn poll(&mut self) -> DeviceEvents {
        self.inner.drain(4);
        let played = std::mem::take(&mut self.inner.played);
        self.played.lock().unwrap().extend(played);
        self.inner.poll()
    }
    fn consumed_frames(&self) -> u64 {
        self.inner.consumed_frames()
    }
    fn underruns(&self) -> u64 {
        self.inner.underruns()
    }
    fn mark_primed(&mut self) {
        self.inner.mark_primed()
    }
    fn output_latency_frames(&self) -> u32 {
        self.inner.output_latency_frames()
    }
    fn close(&mut self) {
        self.inner.close()
    }
}

/// SongPlayer's program wall of this test: real time from a fixed instant.
struct TestWall(Instant);

impl TestWall {
    fn now(&self) -> i64 {
        17_900_000_000_000_000 + (self.0.elapsed().as_nanos() / 100) as i64
    }
}

/// The real worker in the host, over the scripted driver, on the host's
/// clock (SongPlayer's anchors on the shared counter): SongPlayer's blocks
/// are played on the card, and SongPlayer's output reads what the worker
/// reads — running at the driver's rate and sample type.
#[test]
fn the_hosts_worker_plays_songplayers_blocks_on_the_card() {
    let rt = runtime();
    let (sp, host) = pipe();
    let base = Instant::now();
    let played = Arc::new(Mutex::new(Vec::new()));
    let card = played.clone();
    let workers = Built::default();
    let seen = workers.clone();
    let env = HostEnv {
        start: Box::new(move |out: Arc<AsioOut>, mut clock: RemoteClock| {
            seen.outs.lock().unwrap().push(out.clone());
            let played = card.clone();
            std::thread::spawn(move || {
                let mut device = AutoCard {
                    inner: FakeDevice::answering(vec![]),
                    played,
                };
                run_asio_worker(&out, &mut device, &mut clock);
            });
        }),
        mono: Arc::new(mono_100ns),
        status_every: Duration::from_millis(20),
    };
    let served = spawn_host(&rt, host, env);
    let client = Arc::new(HostClient::default());
    let out = Arc::new(AsioOut::from_spec(&spec()));
    let handle = client.attach(out.clone());
    let anchor_wall = TestWall(base);
    let anchors = move || Anchor {
        timeline_100ns: anchor_wall.now(),
        mono_100ns: mono_100ns(),
    };
    let _session = spawn_session(&rt, &client, sp, anchors, WAIT);
    let host_out = first_started(&workers);
    let forwarder = spawn_forwarder(&client, handle, &out);
    let wall = TestWall(base);
    let mut pushed = 0;
    let deadline = Instant::now() + WAIT;
    let played_them = || {
        out.snapshot().blocks_sent >= 20 && played.lock().unwrap().iter().any(|s| s.abs() > 0.2)
    };
    while !played_them() {
        assert!(
            Instant::now() < deadline,
            "the card never played the blocks"
        );
        out.push(ProgramBlock {
            due_100ns: wall.now(),
            samples: Some(vec![0.25; 3200].into()),
            substituted: false,
        });
        pushed += 1;
        std::thread::sleep(Duration::from_millis(10));
    }
    let s = out.snapshot();
    assert_eq!(s.state, "running");
    assert_eq!(s.status.driver_rate, 96_000);
    assert_eq!(s.status.sample_type, AsioSample::Int32.name());
    assert_eq!(s.status.driver, "Waves SoundGrid");
    assert!(s.blocks_sent <= pushed);
    out.stop();
    forwarder.join().unwrap();
    wait_until("the worker released the driver", || !host_out.is_running());
    drop(served);
}

/// The host's code derives no time of its own: no wall, no relabel (the
/// one source of truth is SongPlayer's anchor, `remote_clock.rs`).
#[test]
fn the_host_makes_no_wall_and_reads_no_relabel() {
    let host_side = [
        ("host.rs", include_str!("host.rs")),
        ("host_win.rs", include_str!("host_win.rs")),
        ("remote_clock.rs", include_str!("remote_clock.rs")),
        ("asio_win.rs", include_str!("../playback/asio_win.rs")),
    ];
    for (file, text) in host_side {
        let code: String = text
            .lines()
            .map(|l| l.split("//").next().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        for word in ["WallClock", "WallVbanClock", "fleet_shift"] {
            assert!(!code.contains(word), "{file} names {word}");
        }
    }
}
