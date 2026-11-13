//! #285: the Dante output's device against the real stack, on this
//! machine's own clock (`ClockSource::System`), no network PTP.

use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dstack::flows::client::Client;
use dstack::flows::control::FlowRequest;
use dstack::media::packet;
use sp_core::audio_outputs::RateChoice;

use super::*;
use crate::playback::asio_out::{AsioOut, run_asio_worker};
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::vban_out::VbanClock;

/// One device at a time in this process: the device's hold (`HELD`) has one
/// name. Ports never collide: each test that starts a device runs it on a
/// slot of its own ([`claim_slot`]).
static ONE_DEVICE: Mutex<()> = Mutex::new(());
const BOUND: Duration = Duration::from_secs(20);

fn routed_ipv4() -> Ipv4Addr {
    let probe = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    probe.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).unwrap();
    match probe.local_addr().unwrap() {
        SocketAddr::V4(a) => *a.ip(),
        other => panic!("no routed IPv4: {other}"),
    }
}

/// Dante's default ARC port: the output's, and on dev1 another account's.
const DANTE_ARC_PORT: u16 = 4440;
/// The device slots a test may take (dstack's numbering: slot k = ARC
/// 4440 + k, CMC 8800 + k, flows 4455 + k, process id k; ARC stays below
/// flows 4455 up to slot 14). Slot 0, Dante's defaults, is never a test's.
const TEST_SLOTS: std::ops::RangeInclusive<u16> = 1..=14;
/// Slot k is claimed across processes by holding UDP 127.0.0.1:(base + k):
/// nextest runs every test in its own process and the mutation gate two
/// runs at once, so an in-process lock serializes nothing (#323).
const CLAIM_PORT_BASE: u16 = 29_700;
/// How long a test waits for a slot the other test processes hold.
const CLAIM_WAIT: Duration = Duration::from_secs(60);

/// A device slot this test holds until it is dropped.
struct Slot {
    ip: Ipv4Addr,
    ports: dlink::Ports,
    id: u16,
    _claim: UdpSocket,
}

impl Slot {
    /// The first port of the slot's TX flow block (dstack #264: RX
    /// 29800 + 64k, TX 32 above it), where a granted flow sends from.
    fn tx_base(id: u16) -> u16 {
        29_800 + 64 * id + 32
    }
}

/// Whether nothing holds UDP `port`, on `ip` (another device of this PC)
/// or on the wildcard address (another program): dstack's own probe
/// (`dstack_endpoint::slots::binds_with`). The probes close at once.
fn port_free(ip: Ipv4Addr, port: u16) -> bool {
    UdpSocket::bind((ip, port)).is_ok() && UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok()
}

/// A slot of the test's own on `ip`: claimed, its ports free. Waits while
/// the other test processes hold every slot; the panic names the ports.
fn claim_slot(ip: Ipv4Addr) -> Slot {
    let since = Instant::now();
    loop {
        for id in TEST_SLOTS {
            let Ok(claim) = UdpSocket::bind((Ipv4Addr::LOCALHOST, CLAIM_PORT_BASE + id)) else {
                continue;
            };
            let ports = dlink::Ports {
                arc: DANTE_ARC_PORT + id,
                cmc: 8800 + id,
                flows: 4455 + id,
            };
            let all = [ports.arc, ports.cmc, ports.flows, Slot::tx_base(id)];
            if all.iter().all(|&port| port_free(ip, port)) {
                return Slot {
                    ip,
                    ports,
                    id,
                    _claim: claim,
                };
            }
        }
        assert!(
            since.elapsed() < CLAIM_WAIT,
            "no Dante test slot free on {ip} for {CLAIM_WAIT:?}: slots {TEST_SLOTS:?} \
             (ARC 4441-4454, CMC 8801-8814, flows 4456-4469, claims on 127.0.0.1:{}-{}) \
             are held by other test processes or taken by another program",
            CLAIM_PORT_BASE + TEST_SLOTS.start(),
            CLAIM_PORT_BASE + TEST_SLOTS.end()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn params(dir: &Path, ip: Ipv4Addr) -> DanteParams {
    DanteParams {
        device: "SongPlayer-test".into(),
        ip,
        rate: 48_000,
        network_rate: 96_000,
        dir: dir.to_path_buf(),
        clock: ClockSource::System,
        ports: None,
    }
}

/// A test device on `slot`: its ports, its process id and a name of its own.
fn on_slot(dir: &Path, slot: &Slot) -> DanteParams {
    DanteParams {
        device: format!("SongPlayer-test-{}", slot.id),
        ports: Some((slot.ports, slot.id)),
        ..params(dir, slot.ip)
    }
}

fn block_of(process: &mut RingProcess, frames: usize) -> Vec<f64> {
    let mut output = vec![9.0; frames * 2];
    process.process(&mut Block::new(frames, &[], &mut output));
    output
}

#[test]
fn a_block_takes_interleaved_stereo_into_its_two_channels() {
    let (mut tx, rx) = rtrb::RingBuffer::new(32);
    for i in 0..8 {
        tx.push(i as f32).unwrap();
    }
    let counts = Arc::new(Counts::default());
    let mut process = RingProcess::new(rx, Arc::clone(&counts));
    let out = block_of(&mut process, 4);
    assert_eq!(out, [0.0, 2.0, 4.0, 6.0, 1.0, 3.0, 5.0, 7.0]);
    assert_eq!(counts.consumed.load(Ordering::Relaxed), 4);
    assert_eq!(counts.blocks.load(Ordering::Relaxed), 1);
    assert_eq!(counts.underruns.load(Ordering::Relaxed), 0);
}

#[test]
fn a_short_ring_is_silence_and_an_underrun_only_once_primed() {
    let (mut tx, rx) = rtrb::RingBuffer::new(32);
    let counts = Arc::new(Counts::default());
    let mut process = RingProcess::new(rx, Arc::clone(&counts));
    assert_eq!(block_of(&mut process, 2), [0.0; 4]);
    assert_eq!(counts.underruns.load(Ordering::Relaxed), 0);
    counts.primed.store(true, Ordering::Relaxed);
    tx.push(0.5).unwrap();
    tx.push(0.25).unwrap();
    assert_eq!(block_of(&mut process, 2), [0.5, 0.0, 0.25, 0.0]);
    assert_eq!(counts.underruns.load(Ordering::Relaxed), 1);
    assert_eq!(counts.consumed.load(Ordering::Relaxed), 4);
}

/// dstack's numbers for program audio: a 15 ms budget for the whole TX
/// path (dlink v0.6.28+), block 128, the device's latency and margin as
/// dlink's own `Config::split` sizes them.
#[test]
fn the_device_is_two_tx_no_rx_on_dlinks_defaults_at_the_network_rate() {
    let cfg = params(Path::new("x"), Ipv4Addr::LOCALHOST)
        .config()
        .device()
        .unwrap();
    assert_eq!(cfg.name, "SongPlayer-test");
    assert!(cfg.rx.is_empty());
    assert_eq!(cfg.tx.len(), 2);
    // The device runs at the network's rate; the app writes the program's.
    assert_eq!(cfg.sample_rate, 96_000);
    let config = params(Path::new("x"), Ipv4Addr::LOCALHOST).config();
    assert_eq!((config.device_rate(), config.converts()), (96_000, true));
    assert_eq!(cfg.encoding, 24);
    assert_eq!(cfg.block, 128);
    // dlink's own split of its 15 ms budget, whatever its release sizes:
    // the device runs exactly the latency and margin dlink chose.
    let split = config.split().unwrap();
    assert_eq!(cfg.latency_ns, split.latency_ns);
    assert_eq!(cfg.work_margin, split.margin);
    assert!(split.tx_path_ns <= dlink::REFERENCE_PATH_NS);
    assert_eq!(
        (cfg.manufacturer.as_str(), cfg.model.as_str()),
        ("New Level", "SongPlayer")
    );
}

/// What Dante Controller set survives: the state file stays where it was.
#[test]
fn the_state_file_stays_in_the_devices_directory() {
    let dir = PathBuf::from("C:/ProgramData/SongPlayer/dante");
    let cfg = params(&dir, Ipv4Addr::LOCALHOST).config().device().unwrap();
    assert_eq!(cfg.state_file, dir.join("state.toml"));
}

/// #323: a device given a slot binds that slot's ports with its process
/// id; the output's own (`ports: None`) keeps Dante's defaults.
#[test]
fn a_device_on_a_slot_takes_its_ports_and_process_id() {
    let on = DanteParams {
        ports: Some((
            dlink::Ports {
                arc: 4443,
                cmc: 8803,
                flows: 4458,
            },
            3,
        )),
        ..params(Path::new("x"), Ipv4Addr::LOCALHOST)
    };
    let cfg = on.config().device().unwrap();
    assert_eq!(
        (cfg.arc_port, cfg.cmc_port, cfg.flows_port, cfg.process_id),
        (4443, 8803, 4458, 3)
    );
    let plain = params(Path::new("x"), Ipv4Addr::LOCALHOST)
        .config()
        .device()
        .unwrap();
    assert_eq!(
        (
            plain.arc_port,
            plain.cmc_port,
            plain.flows_port,
            plain.process_id
        ),
        (DANTE_ARC_PORT, 8800, 4455, 0)
    );
}

/// #323: a test device never needs Dante's default ports. On dev1 another
/// account's device holds UDP 4440, and parallel test processes would each
/// bind it: here it is held (by this test, or already by another program),
/// and the device starts on its own slot all the same.
#[test]
fn a_test_device_starts_while_dantes_default_arc_port_is_taken() {
    let _one = ONE_DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let ip = routed_ipv4();
    let _holder = UdpSocket::bind((ip, DANTE_ARC_PORT));
    assert!(!port_free(ip, DANTE_ARC_PORT), "UDP 4440 is taken");
    let slot = claim_slot(ip);
    let mut device = DanteDevice::new(on_slot(dir.path(), &slot));
    device.open("", [0, 1]).unwrap();
    let (_tx, rx) = rtrb::RingBuffer::new(1024);
    let started = device.start(rx).err();
    assert_eq!(started, None, "the device on slot {}", slot.id);
    device.close();
}

#[test]
fn an_entry_builds_at_the_network_rate_or_its_own() {
    let dest = DanteDest {
        device: "SongPlayer".into(),
        ip: "10.77.9.210".into(),
    };
    let mut entry = OutputEntry::dante("out-1", "Dante", dest);
    let p = DanteParams::for_entry(&entry, 96_000).unwrap();
    // #285 step 2: the worker writes the program's 48 kHz, dlink converts
    // to the network's 96 kHz on the device's clock.
    assert_eq!((p.rate, p.network_rate), (48_000, 96_000));
    assert_eq!(p.ip, Ipv4Addr::new(10, 77, 9, 210));
    assert_eq!(p.clock, ClockSource::Dsync);
    // dlink's whole TX path is its 15 ms budget: 720 frames at 48 kHz,
    // the converter's delay included.
    let lat = p.latency_frames();
    assert!((718..=720).contains(&lat), "latency {lat}");
    entry.rate = RateChoice::Fixed(48_000);
    let same = DanteParams::for_entry(&entry, 96_000).unwrap();
    assert_eq!((same.rate, same.network_rate), (48_000, 48_000));
    assert!(!same.config().converts());
    assert!((718..=720).contains(&same.latency_frames()));
}

#[test]
fn a_vban_entry_is_no_dante_device() {
    let entry = OutputEntry {
        dante: None,
        ..OutputEntry::dante(
            "out-1",
            "D",
            DanteDest {
                device: "a".into(),
                ip: "10.0.0.1".into(),
            },
        )
    };
    assert_eq!(
        DanteParams::for_entry(&entry, 48_000),
        Err("not a Dante entry".to_string())
    );
}

#[test]
fn the_stacks_status_line_reads_as_the_figures() {
    let line = r#"{"clock":{"ready":1,"epoch":1},"tx":[
        {"flow":1,"live":1,"late_bins":[5,1,0,0,0,0],"late_max_us":300,"send_max_us":20},
        {"flow":2,"live":0,"late_bins":[1,1,1,0,0,2],"late_max_us":7000,"send_max_us":10}
        ],"engine":{"late":1,"missed":2,"overruns":3}}"#;
    let f = figures_of(line).unwrap();
    assert!(f.clock_ready);
    assert_eq!(f.live_flows, 1);
    assert_eq!(f.late_bins, [6, 2, 1, 0, 0, 2]);
    assert_eq!((f.late_max_us, f.send_max_us), (7000, 20));
    assert_eq!((f.pump_late, f.pump_missed, f.pump_overruns), (1, 2, 3));
    assert_eq!(
        figures_of(r#"{"clock":{"ready":0}}"#).unwrap().live_flows,
        0
    );
    assert_eq!(figures_of("error: nothing"), None);
}

#[test]
fn the_link_reads_the_running_stack_and_nothing_after_the_close() {
    let _one = ONE_DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let slot = claim_slot(routed_ipv4());
    let link = DanteLink::default();
    assert_eq!(link.figures(), None);
    let mut device = DanteDevice::linked(on_slot(dir.path(), &slot), link.clone());
    device.open("", [0, 1]).unwrap();
    assert_eq!(link.figures(), None, "not started yet");
    let (_tx, rx) = rtrb::RingBuffer::new(1024);
    device.start(rx).unwrap();
    let figures = link.figures().expect("the running stack answers");
    assert!(figures.clock_ready, "the system clock is ready at once");
    assert_eq!(figures.live_flows, 0);
    device.close();
    assert_eq!(link.figures(), None);
}

#[test]
fn an_output_with_no_device_running_shows_no_dante_figures() {
    use crate::playback::audio_out::{OutputSink, RunningOutput};
    let entry = OutputEntry::dante(
        "out-1",
        "Dante",
        DanteDest {
            device: "SongPlayer".into(),
            ip: "10.0.0.5".into(),
        },
    );
    let out = RunningOutput {
        built_rate: 96_000,
        sink: Some(OutputSink::Dante(
            Arc::new(AsioOut::for_dante(&entry).unwrap()),
            DanteParams::for_entry(&entry, 96_000).unwrap(),
            DanteLink::default(),
        )),
        error: None,
        entry,
    };
    let status = out.status(96_000);
    assert_eq!((status.kind, status.dante), ("dante", None));
    let json = serde_json::to_string(&out.status(96_000)).unwrap();
    assert!(!json.contains("\"dante\":"), "{json}");
}

#[test]
fn an_address_no_adapter_holds_fails_the_start_and_names_no_panic() {
    let _one = ONE_DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    // No such address on this machine.
    let mut device = DanteDevice::new(params(dir.path(), Ipv4Addr::new(192, 0, 2, 77)));
    device.open("", [0, 1]).unwrap();
    let (_tx, rx) = rtrb::RingBuffer::new(1024);
    assert!(device.start(rx).is_err());
    device.close();
}

/// A rebuilt entry's successor starts while its predecessor still stops: it
/// must not bind the Dante ports or announce the name until the old one is out.
#[test]
fn a_successor_waits_until_its_predecessor_closed() {
    let _one = ONE_DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let slot = claim_slot(routed_ipv4());
    let mut old = DanteDevice::new(on_slot(dir.path(), &slot));
    old.open("", [0, 1]).unwrap();
    let (_tx, rx) = rtrb::RingBuffer::new(1024);
    old.start(rx).unwrap();

    let mut successor = DanteDevice::new(on_slot(dir.path(), &slot));
    assert_eq!(successor.open("", [0, 1]).err(), Some(Reason::Held));

    old.close();
    successor.open("", [0, 1]).unwrap();
    let (_tx, rx) = rtrb::RingBuffer::new(1024);
    successor.start(rx).unwrap();
    successor.close();
}

/// A worker's panic drops the device: the stack must not run on beside the
/// output `RunGuard` rebuilds.
#[test]
fn dropping_a_running_device_stops_its_stack() {
    let _one = ONE_DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let slot = claim_slot(routed_ipv4());
    let mut old = DanteDevice::new(on_slot(dir.path(), &slot));
    old.open("", [0, 1]).unwrap();
    let (_tx, rx) = rtrb::RingBuffer::new(1024);
    old.start(rx).unwrap();
    drop(old);

    let mut successor = DanteDevice::new(on_slot(dir.path(), &slot));
    successor.open("", [0, 1]).unwrap();
    let (_tx, rx) = rtrb::RingBuffer::new(1024);
    successor.start(rx).unwrap();
    successor.close();
}

/// A start the stack refused is closed by the worker, and the next try opens.
#[test]
fn a_failed_start_gives_the_hold_back_at_its_close() {
    let _one = ONE_DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let mut device = DanteDevice::new(params(dir.path(), Ipv4Addr::new(192, 0, 2, 77)));
    device.open("", [0, 1]).unwrap();
    let (_tx, rx) = rtrb::RingBuffer::new(1024);
    assert!(device.start(rx).is_err());
    device.close();
    device.open("", [0, 1]).unwrap();
    device.close();
}

#[test]
fn a_name_the_stack_refuses_fails_the_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = params(dir.path(), Ipv4Addr::LOCALHOST);
    p.device = "-bad".into();
    let refused = DanteDevice::new(p).open("", [0, 1]).err();
    assert!(matches!(refused, Some(Reason::Refused(_))), "{refused:?}");
}

/// A receiver of ours asks for the flow and reads the program's samples.
#[test]
fn the_device_serves_a_receiver_the_rings_audio() {
    let _one = ONE_DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let slot = claim_slot(routed_ipv4());
    let ip = slot.ip;
    let p = on_slot(dir.path(), &slot);
    let latency = u32::try_from(p.latency_frames()).unwrap();
    let link = DanteLink::default();
    let mut device = DanteDevice::linked(p, link.clone());
    let opened = device.open("", [0, 1]).expect("the device set up");
    // The worker writes 48 kHz: the 128-frame device block is 64 of its frames.
    assert_eq!(opened.rate, 48_000.0);
    assert_eq!(opened.buffer_frames, 64);
    let (mut tx, rx) = rtrb::RingBuffer::new(1 << 16);
    assert_eq!(
        device
            .start(rx)
            .expect("the device started")
            .output_latency_frames,
        latency
    );
    device.mark_primed();
    let audio = UdpSocket::bind((ip, 0)).unwrap();
    audio
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let SocketAddr::V4(at) = audio.local_addr().unwrap() else {
        panic!("IPv4")
    };
    let req = FlowRequest {
        rx_host: "test-receiver".to_string(),
        rx_flow_name: "program".to_string(),
        sample_rate: 96_000,
        bits: 24,
        channels: vec![Some(1), Some(2)],
        fpp: 32,
        rx: at,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let asked = Instant::now();
    let at_flows = SocketAddrV4::new(ip, slot.ports.flows);
    let granted = rt.block_on(Client::new(ip).request_flow(at_flows, &req));
    // #302: a refused or unanswered request names how long it waited and
    // what the device did meanwhile (dev CI 43121764441: Err(Timeout)).
    assert!(
        granted.is_ok(),
        "the flow: {granted:?} after {:?} (consumed {} frames; stack: {:?})",
        asked.elapsed(),
        device.consumed_frames(),
        link.figures()
    );
    let start = Instant::now();
    let mut buf = [0u8; 2048];
    let (mut left, mut right) = (false, false);
    while !(left && right) && start.elapsed() < BOUND {
        while tx.slots() >= 2 && tx.slots() > (1 << 16) - 4096 {
            tx.push(0.5).unwrap();
            tx.push(-0.25).unwrap();
        }
        // #302: a read that times out names what the stack did meanwhile
        // (its clock, the flows a receiver holds, the packets it sent by
        // lateness, the pump), so a one-off on a CI runner can be told.
        let n = match audio.recv(&mut buf) {
            Ok(n) => n,
            Err(e) => panic!(
                "audio from the device: {e} after {:?} (granted at 0; consumed {} frames; stack: {:?})",
                start.elapsed(),
                device.consumed_frames(),
                link.figures()
            ),
        };
        packet::read_frames(&buf[..n], 2, 3, |_, slot, x| {
            let x = x >> 8;
            left |= slot == 0 && (x - (1 << 22)).abs() < 8;
            right |= slot == 1 && (x + (1 << 21)).abs() < 8;
        })
        .expect("two 24-bit slots");
    }
    assert!(left && right, "both channels on the wire");
    assert!(device.consumed_frames() >= 64);
    let events = device.poll();
    assert!(events.callbacks > 0);
    assert_eq!(
        events.late_callbacks, 0,
        "a wake within the 5 ms margin is no late callback"
    );
    assert!(!device.poll().reset);
    device.close();
    assert!(device.open("", [0, 1]).is_ok(), "the ports are free again");
    device.close();
}

struct TestClock(Instant);

impl VbanClock for TestClock {
    fn now_100ns(&mut self) -> i64 {
        self.0.elapsed().as_nanos() as i64 / 100
    }

    fn sleep_100ns(&mut self, d_100ns: i64) {
        std::thread::sleep(Duration::from_nanos(d_100ns.max(0) as u64 * 100));
    }

    fn slew_owed_100ns(&self) -> i64 {
        0
    }
}

/// The whole path of a program block: the output's queue, the ASIO worker
/// with its servo and resampler, the ring, the stack, the wire.
#[test]
fn a_program_block_reaches_the_wire_through_the_asio_worker() {
    let _one = ONE_DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let own = claim_slot(routed_ipv4());
    let ip = own.ip;
    let entry = OutputEntry::dante(
        "out-1",
        "Dante",
        DanteDest {
            device: "SongPlayer-test".into(),
            ip: ip.to_string(),
        },
    );
    let out = Arc::new(AsioOut::for_dante(&entry).unwrap());
    let start = Instant::now();
    let worker = {
        let out = Arc::clone(&out);
        let params = on_slot(dir.path(), &own);
        std::thread::spawn(move || {
            let mut device = DanteDevice::new(params);
            run_asio_worker(&out, &mut device, &mut TestClock(start));
        })
    };
    let audio = UdpSocket::bind((ip, 0)).unwrap();
    audio
        .set_read_timeout(Some(Duration::from_millis(5)))
        .unwrap();
    let SocketAddr::V4(at) = audio.local_addr().unwrap() else {
        panic!("IPv4")
    };
    let req = FlowRequest {
        rx_host: "test-receiver".to_string(),
        rx_flow_name: "program".to_string(),
        sample_rate: 96_000,
        bits: 24,
        channels: vec![Some(1), Some(2)],
        fpp: 32,
        rx: at,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let samples: Arc<[f32]> = (0..3200)
        .map(|i| if i % 2 == 0 { 0.5 } else { -0.25 })
        .collect();
    const SLOT_100NS: i64 = 333_333;
    let mut slot = 0_i64;
    let mut granted = false;
    let mut heard = false;
    let mut buf = [0u8; 2048];
    while !heard && start.elapsed() < BOUND {
        while slot * SLOT_100NS < start.elapsed().as_nanos() as i64 / 100 + SLOT_100NS {
            out.push(ProgramBlock {
                due_100ns: slot * SLOT_100NS,
                samples: Some(Arc::clone(&samples)),
                substituted: false,
            });
            slot += 1;
        }
        if !granted {
            let client = Client::new(ip);
            let flow = client.request_flow(SocketAddrV4::new(ip, own.ports.flows), &req);
            granted = rt.block_on(flow).is_ok();
        }
        if let Ok(n) = audio.recv(&mut buf) {
            packet::read_frames(&buf[..n], 2, 3, |_, slot, x| {
                heard |= slot == 0 && x >> 8 > 1 << 21;
            })
            .expect("two 24-bit slots");
        }
    }
    out.discard();
    worker.join().unwrap();
    assert!(heard, "the program on the wire: {:?}", out.snapshot());
}

/// A receiver subscribed to `01@resolume-snv` finds the device under DVS's
/// channel names, so DVS is replaced with no change on any receiver.
#[test]
fn the_channels_carry_dvs_s_names() {
    let cfg = params(Path::new("x"), Ipv4Addr::LOCALHOST)
        .config()
        .device()
        .unwrap();
    assert_eq!(cfg.tx, ["01", "02"]);
}
