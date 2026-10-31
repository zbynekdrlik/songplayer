//! #285: the Dante output's device against the real stack, on this
//! machine's own clock (`ClockSource::System`), no network PTP.

use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Mutex;
use std::time::Instant;

use dante_stack::flows::client::Client;
use dante_stack::flows::control::{FlowRequest, PORT};
use dante_stack::media::packet;
use sp_core::audio_outputs::RateChoice;

use super::*;
use crate::playback::asio_out::{AsioOut, run_asio_worker};
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::vban_out::VbanClock;

/// One device at a time: each binds the Dante ports.
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

fn params(dir: &Path, ip: Ipv4Addr) -> DanteParams {
    DanteParams {
        device: "SongPlayer-test".into(),
        ip,
        rate: 96_000,
        dir: dir.to_path_buf(),
        clock: ClockSource::System,
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

#[test]
fn a_config_the_stack_takes_two_tx_no_rx_at_the_network_rate() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dante_stack::device::DeviceConfig::from_toml(
        &params(dir.path(), Ipv4Addr::LOCALHOST).config_text(),
    )
    .unwrap();
    assert_eq!(cfg.name, "SongPlayer-test");
    assert!(cfg.rx.is_empty());
    assert_eq!(cfg.tx.len(), 2);
    assert_eq!(cfg.sample_rate, 96_000);
    assert_eq!(cfg.encoding, 24);
    assert_eq!(cfg.block, u64::from(DANTE_BLOCK));
    assert_eq!(cfg.latency_ns, DANTE_LATENCY_NS);
}

#[test]
fn a_windows_state_path_stays_one_toml_string() {
    let mut p = params(Path::new("x"), Ipv4Addr::LOCALHOST);
    p.dir = PathBuf::from(r"C:\ProgramData\SongPlayer\dante");
    assert!(
        p.config_text()
            .contains(r#"C:\\ProgramData\\SongPlayer\\dante"#)
    );
}

#[test]
fn an_entry_builds_at_the_network_rate_or_its_own() {
    let dest = DanteDest {
        device: "SongPlayer".into(),
        ip: "10.77.9.210".into(),
    };
    let mut entry = OutputEntry::dante("out-1", "Dante", dest);
    let p = DanteParams::for_entry(&entry, 96_000).unwrap();
    assert_eq!(p.rate, 96_000);
    assert_eq!(p.ip, Ipv4Addr::new(10, 77, 9, 210));
    assert_eq!(p.clock, ClockSource::Dantesync);
    assert_eq!(p.latency_frames(), 960);
    entry.rate = RateChoice::Fixed(48_000);
    assert_eq!(DanteParams::for_entry(&entry, 96_000).unwrap().rate, 48_000);
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
    let link = DanteLink::default();
    assert_eq!(link.figures(), None);
    let mut device = DanteDevice::linked(params(dir.path(), routed_ipv4()), link.clone());
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
fn a_taken_address_fails_the_open_and_names_no_panic() {
    let _one = ONE_DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    // No such address on this machine.
    let mut device = DanteDevice::new(params(dir.path(), Ipv4Addr::new(192, 0, 2, 77)));
    assert!(device.open("", [0, 1]).is_err());
    device.close();
}

/// A receiver of ours asks for the flow and reads the program's samples.
#[test]
fn the_device_serves_a_receiver_the_rings_audio() {
    let _one = ONE_DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let ip = routed_ipv4();
    let mut device = DanteDevice::new(params(dir.path(), ip));
    let opened = device.open("", [0, 1]).expect("the device set up");
    assert_eq!(opened.rate, 96_000.0);
    assert_eq!(opened.buffer_frames, DANTE_BLOCK);
    let (mut tx, rx) = rtrb::RingBuffer::new(1 << 16);
    assert_eq!(
        device
            .start(rx)
            .expect("the device started")
            .output_latency_frames,
        960
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
    let granted = rt.block_on(Client::new(ip).request_flow(SocketAddrV4::new(ip, PORT), &req));
    assert!(granted.is_ok(), "the flow: {granted:?}");
    let start = Instant::now();
    let mut buf = [0u8; 2048];
    let (mut left, mut right) = (false, false);
    while !(left && right) && start.elapsed() < BOUND {
        while tx.slots() >= 2 && tx.slots() > (1 << 16) - 4096 {
            tx.push(0.5).unwrap();
            tx.push(-0.25).unwrap();
        }
        let n = audio.recv(&mut buf).expect("audio from the device");
        packet::read_frames(&buf[..n], 2, 3, |_, slot, x| {
            let x = x >> 8;
            left |= slot == 0 && (x - (1 << 22)).abs() < 8;
            right |= slot == 1 && (x + (1 << 21)).abs() < 8;
        })
        .expect("two 24-bit slots");
    }
    assert!(left && right, "both channels on the wire");
    assert!(device.consumed_frames() >= u64::from(DANTE_BLOCK));
    assert!(device.poll().callbacks > 0);
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
    let ip = routed_ipv4();
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
        let params = params(dir.path(), ip);
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
            let flow = client.request_flow(SocketAddrV4::new(ip, PORT), &req);
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
