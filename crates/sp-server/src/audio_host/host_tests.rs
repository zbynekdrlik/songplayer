//! #233: SongPlayer's client and the ASIO host's frame loop, end to end over
//! an in-memory pipe (`tokio::io::duplex`): an output added reaches the
//! host, its blocks reach the host's output in order on the host's
//! timeline, the host's status comes back as the output's status, a real
//! worker over the scripted driver plays them, and either side's end is
//! seen by the other.
//!
//! The pipe's tasks run on their own multi-thread runtime; the test thread
//! makes the blocking reads (`take_block`) and bounded waits (`wait_until`,
//! 20 s), never a wall-time threshold.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::audio_host::client::{HostClient, run_forwarder};
use crate::audio_host::host::{HostEnd, HostEnv, HostState, REPORT_QUEUE, on_frame, serve};
use crate::audio_host::protocol::{
    BlockFrame, Control, Frame, PROTOCOL, Report, read_frame, write_frame,
};
use crate::audio_host::wire_status::WireSnapshot;
use crate::playback::asio_format::AsioSample;
use crate::playback::asio_out::fake::FakeDevice;
use crate::playback::asio_out::{
    AsioDevice, AsioOut, AsioSnapshot, AsioSpec, AsioStatus, Opened, Started, run_asio_worker,
};
use crate::playback::asio_state::{DeviceEvents, Reason};
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::audio_out_queue::Take;
use crate::playback::program_max_worker::tests::wait_until;
use crate::playback::vban_out::VbanClock;

/// D(3) − D(1): the offset of SongPlayer at K 3 over a host at K 1.
const TWO_SLOTS: i64 = 666_666;
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

/// The host's outputs as its starter saw them.
type Built = Arc<Mutex<Vec<Arc<AsioOut>>>>;

/// A host whose starter records each output (no worker), at fleet `slots`.
fn recording_env(started: &Built, slots: i64) -> HostEnv {
    let started = started.clone();
    HostEnv {
        start: Box::new(move |out: Arc<AsioOut>| started.lock().unwrap().push(out)),
        now_100ns: Box::new(|| 0),
        fleet_slots: Box::new(move || slots),
        status_every: Duration::from_millis(20),
    }
}

type Served = JoinHandle<(HostEnd, Vec<Arc<AsioOut>>)>;

fn spawn_host(rt: &Runtime, (reader, writer): Halves, env: HostEnv) -> Served {
    let (tx, rx) = mpsc::channel(REPORT_QUEUE);
    rt.spawn(serve(reader, writer, env, rx, tx))
}

fn spawn_session(
    rt: &Runtime,
    client: &Arc<HostClient>,
    (reader, writer): Halves,
) -> JoinHandle<String> {
    let client = client.clone();
    rt.spawn(async move { client.session(reader, writer).await })
}

fn spawn_forwarder(
    client: &Arc<HostClient>,
    handle: u64,
    out: &Arc<AsioOut>,
) -> std::thread::JoinHandle<()> {
    let (client, out) = (client.clone(), out.clone());
    std::thread::spawn(move || run_forwarder(&client, handle, &out))
}

fn first_started(started: &Built) -> Arc<AsioOut> {
    wait_until("the host built the output", || {
        !started.lock().unwrap().is_empty()
    });
    started.lock().unwrap()[0].clone()
}

fn ramp(k: i64) -> Arc<[f32]> {
    (0..3200).map(|i| (k * 3200 + i) as f32 * 1e-6).collect()
}

/// The blocks a forwarder takes from SongPlayer's output reach the host's
/// output in order, their samples whole, their boundaries moved onto the
/// host's timeline (SongPlayer at fleet K 3, the host at K 1); the output's
/// stop removes it on the host (its queue stopped).
#[test]
fn blocks_reach_the_hosts_output_in_order_on_its_timeline() {
    let rt = runtime();
    let (sp, host) = pipe();
    let started = Built::default();
    let served = spawn_host(&rt, host, recording_env(&started, 1));
    let client = Arc::new(HostClient::new(|| 3));
    let out = Arc::new(AsioOut::from_spec(&spec()));
    let handle = client.attach(out.clone());
    let _session = spawn_session(&rt, &client, sp);
    let host_out = first_started(&started);
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
        assert_eq!(b.due_100ns, 1_000_000 + k + TWO_SLOTS, "block {k}");
        assert_eq!(b.samples, (k != 2).then(|| ramp(k)), "block {k}");
        assert_eq!(b.substituted, k == 4, "block {k}");
    }
    out.stop();
    forwarder.join().unwrap();
    assert_eq!(client.outputs(), 0, "the stopped output is detached");
    assert_eq!(
        host_out.take_block(WAIT),
        Take::Stopped,
        "the host discarded its side"
    );
    assert_eq!(client.unsent_blocks(), 0);
    drop(served);
}

/// The host's status of an output is SongPlayer's status of it.
#[test]
fn the_hosts_status_is_the_outputs_status_in_songplayer() {
    let rt = runtime();
    let (sp, host) = pipe();
    let started = Built::default();
    let _served = spawn_host(&rt, host, recording_env(&started, 0));
    let client = Arc::new(HostClient::new(|| 0));
    let out = Arc::new(AsioOut::from_spec(&spec()));
    client.attach(out.clone());
    let _session = spawn_session(&rt, &client, sp);
    let host_out = first_started(&started);
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
    host_out.set_host_down("a probe");
    wait_until("the next status too", || {
        out.snapshot().reason == Some(Reason::HostDown("a probe".into()))
    });
}

/// SongPlayer's end of the pipe gone (it exited): the host's loop ends and
/// discards every output, so each worker releases its driver.
#[test]
fn songplayers_end_ends_the_host_and_discards_its_outputs() {
    let rt = runtime();
    let (sp, host) = pipe();
    let started = Built::default();
    let served = spawn_host(&rt, host, recording_env(&started, 0));
    let client = Arc::new(HostClient::new(|| 0));
    client.attach(Arc::new(AsioOut::from_spec(&spec())));
    let session = spawn_session(&rt, &client, sp);
    let host_out = first_started(&started);
    session.abort();
    let (end, discarded) = rt.block_on(served).unwrap();
    assert_eq!(end, HostEnd::Eof);
    assert_eq!(discarded.len(), 1);
    assert!(Arc::ptr_eq(&discarded[0], &host_out));
    assert_eq!(host_out.take_block(WAIT), Take::Stopped);
}

/// The session opens with the hello, then every output's add; the host's
/// end of the pipe gone: the session ends, every output reads waiting,
/// `host_down`, and one attached afterwards too.
#[test]
fn the_hosts_end_leaves_every_output_waiting_for_it() {
    let rt = runtime();
    let (sp, (mut reader, writer)) = pipe();
    let client = Arc::new(HostClient::new(|| 0));
    let out = Arc::new(AsioOut::from_spec(&spec()));
    let handle = client.attach(out.clone());
    assert_eq!(out.snapshot().state, "opening", "no host is known down yet");
    let session = spawn_session(&rt, &client, sp);
    let first = rt.block_on(read_frame(&mut reader)).unwrap();
    assert_eq!(
        first,
        Some(Frame::Control(Control::Hello { protocol: PROTOCOL }))
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
    assert!(!client.send_block(handle, ProgramBlock::silence(1)));
    assert_eq!(client.unsent_blocks(), 2);
}

/// The supervisor starts the host only once an output is wanted: with none
/// attached the wait holds, an attach ends it.
#[tokio::test]
async fn the_host_is_wanted_once_an_output_is_attached() {
    let client = HostClient::new(|| 0);
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
    let client = Arc::new(HostClient::new(|| 0));
    let session = spawn_session(&rt, &client, sp);
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

fn state_env(started: &Built) -> (HostState, HostEnv) {
    (HostState::new(Arc::default()), recording_env(started, 0))
}

fn hello() -> Frame {
    Frame::Control(Control::Hello { protocol: PROTOCOL })
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
    let started = Built::default();
    let (mut s, env) = state_env(&started);
    let block = Frame::Block(BlockFrame::of(1, ProgramBlock::silence(0), 0));
    assert_eq!(
        on_frame(&mut s, &env, block.clone()),
        Err("a frame before the hello".into())
    );
    assert_eq!(
        on_frame(&mut s, &env, add(1)),
        Err("a frame before the hello".into())
    );
    assert_eq!(
        on_frame(&mut s, &env, Frame::Control(Control::Hello { protocol: 2 })),
        Err(format!(
            "SongPlayer speaks protocol 2, this host {PROTOCOL}"
        ))
    );
    assert_eq!(on_frame(&mut s, &env, hello()), Ok(()));
    assert_eq!(
        on_frame(&mut s, &env, block),
        Ok(()),
        "an unknown handle's block is dropped"
    );
    let report = Frame::Report(Report::Log {
        level: crate::audio_host::protocol::HostLevel::Info,
        line: "x".into(),
    });
    assert_eq!(
        on_frame(&mut s, &env, report),
        Err("SongPlayer sent a report frame".into())
    );
    assert!(started.lock().unwrap().is_empty());
}

/// An add starts a worker; a second add under the same handle discards the
/// first; a remove discards and forgets it.
#[test]
fn the_host_adds_replaces_and_removes_outputs() {
    let started = Built::default();
    let (mut s, env) = state_env(&started);
    on_frame(&mut s, &env, hello()).unwrap();
    on_frame(&mut s, &env, add(7)).unwrap();
    on_frame(&mut s, &env, add(7)).unwrap();
    let (first, second) = {
        let v = started.lock().unwrap();
        assert_eq!(v.len(), 2);
        (v[0].clone(), v[1].clone())
    };
    assert_eq!(first.take_block(Duration::ZERO), Take::Stopped);
    assert_eq!(second.take_block(Duration::ZERO), Take::Idle);
    let reports = crate::audio_host::host::status_reports(&s.outputs());
    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0],
        Report::Status {
            handle: 7,
            snapshot: WireSnapshot::from(&second.snapshot()),
        }
    );
    on_frame(&mut s, &env, Frame::Control(Control::Remove { handle: 7 })).unwrap();
    assert_eq!(second.take_block(Duration::ZERO), Take::Stopped);
    assert!(crate::audio_host::host::status_reports(&s.outputs()).is_empty());
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

/// The program wall of this test: real time from a fixed instant.
struct TestWall(Instant);

impl TestWall {
    fn now(&self) -> i64 {
        17_900_000_000_000_000 + (self.0.elapsed().as_nanos() / 100) as i64
    }
}

impl VbanClock for TestWall {
    fn now_100ns(&mut self) -> i64 {
        self.now()
    }
    fn sleep_100ns(&mut self, d_100ns: i64) {
        std::thread::sleep(Duration::from_nanos(d_100ns.max(0) as u64 * 100));
    }
    fn slew_owed_100ns(&self) -> i64 {
        0
    }
}

/// The real worker in the host, over the scripted driver: SongPlayer's
/// blocks are played on the card, and SongPlayer's output reads what the
/// worker reads — running at the driver's rate and sample type.
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
        start: Box::new(move |out: Arc<AsioOut>| {
            seen.lock().unwrap().push(out.clone());
            let played = card.clone();
            std::thread::spawn(move || {
                let mut device = AutoCard {
                    inner: FakeDevice::answering(vec![]),
                    played,
                };
                let mut wall = TestWall(base);
                run_asio_worker(&out, &mut device, &mut wall);
            });
        }),
        now_100ns: Box::new(|| 0),
        fleet_slots: Box::new(|| 0),
        status_every: Duration::from_millis(20),
    };
    let served = spawn_host(&rt, host, env);
    let client = Arc::new(HostClient::new(|| 0));
    let out = Arc::new(AsioOut::from_spec(&spec()));
    let handle = client.attach(out.clone());
    let _session = spawn_session(&rt, &client, sp);
    let host_out = first_started(&workers);
    let forwarder = spawn_forwarder(&client, handle, &out);
    let wall = TestWall(base);
    let mut pushed = 0;
    let deadline = Instant::now() + WAIT;
    let played_them =
        || out.snapshot().blocks_sent >= 20 && played.lock().unwrap().iter().any(|s| s.abs() > 0.2);
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
