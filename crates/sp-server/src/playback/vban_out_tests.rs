//! #210 VBAN output: the hand-off queue (drop-oldest overflow), the paced
//! schedule on a fake clock (exact 100 ns send instants), disabled / no-target
//! silence, the frame counter across blocks + a cut + standby through a real
//! `ProgramOutput`, the settings load, DNS resolution, and a real loopback UDP
//! round trip. `FakeClock` / `RecordingSink` / `active_config` are shared with
//! `api/program_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "vban_out_tests.rs"] pub(crate) mod tests;`.

use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramJob};
use crate::playback::program_output::ProgramOutput;
use crate::playback::submit_handoff::SubmitJob;
use crate::playback::vban_packet::VBAN_BLOCK_SAMPLES;
use crate::playback::vban_packet::tests::{parse_packet, ramp_block};
use crate::playback::vban_packet::{VBAN_SEND_LATENCY_100NS, f32_to_int24, packet_offset_100ns};
use crate::playback::wallclock::WallClock;
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

const D: i64 = 17_900_000_000_000_000;
const L: i64 = VBAN_SEND_LATENCY_100NS;

/// A settable wall: `sleep_100ns` advances it and is recorded.
pub(crate) struct FakeClock {
    pub now: Arc<AtomicI64>,
    pub sleeps: Vec<i64>,
}

impl FakeClock {
    pub fn at(t: i64) -> Self {
        Self {
            now: Arc::new(AtomicI64::new(t)),
            sleeps: Vec::new(),
        }
    }
}

impl VbanClock for FakeClock {
    fn now_100ns(&mut self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
    fn sleep_100ns(&mut self, d_100ns: i64) {
        self.sleeps.push(d_100ns);
        self.now.fetch_add(d_100ns, Ordering::SeqCst);
    }
}

/// Records every packet with the fake clock's time; `fail_to` refuses one
/// address.
pub(crate) struct RecordingSink {
    pub now: Arc<AtomicI64>,
    pub sent: Vec<(i64, SocketAddr, Vec<u8>)>,
    pub fail_to: Option<SocketAddr>,
}

impl RecordingSink {
    pub fn on(clock: &FakeClock) -> Self {
        Self {
            now: clock.now.clone(),
            sent: Vec::new(),
            fail_to: None,
        }
    }
}

impl VbanSink for RecordingSink {
    fn send_packet(&mut self, packet: &[u8], addr: SocketAddr) -> io::Result<usize> {
        if self.fail_to == Some(addr) {
            return Err(io::Error::other("refused"));
        }
        self.sent
            .push((self.now.load(Ordering::SeqCst), addr, packet.to_vec()));
        Ok(packet.len())
    }
}

/// An enabled config (stream `sp-program`) sending to `addrs`.
pub(crate) fn active_config(addrs: &[&str]) -> VbanConfig {
    let settings = VbanSettings {
        enabled: true,
        stream_name: "sp-program".into(),
        targets: addrs.join(","),
    };
    let targets = addrs
        .iter()
        .map(|a| VbanTarget {
            spec: a.to_string(),
            addr: Some(a.parse().unwrap()),
            error: None,
        })
        .collect();
    VbanConfig::new(&settings, targets)
}

fn frame(data: Vec<f32>, channels: u32, sample_rate: u32) -> AudioFrame {
    AudioFrame {
        data,
        channels,
        sample_rate,
        timecode_100ns: None,
    }
}

fn block(due: i64, v: f32) -> VbanBlock {
    VbanBlock {
        due_100ns: due,
        samples: Some(vec![v; VBAN_BLOCK_SAMPLES]),
        substituted: false,
    }
}

fn out_with(cfg: VbanConfig) -> VbanOut {
    let out = VbanOut::new();
    out.set_config(cfg);
    out
}

// --- the block hand-off ---------------------------------------------------

#[test]
fn a_program_frame_moves_in_and_anything_else_is_substituted_silence() {
    let data = vec![0.5f32; 3200];
    let ptr = data.as_ptr();
    let b = VbanBlock::from_frames(7, vec![frame(data, 2, 48_000)]);
    assert_eq!(b.due_100ns, 7);
    assert!(!b.substituted);
    let samples = b.samples.expect("one program block is kept");
    assert_eq!(samples.as_ptr(), ptr, "moved, not copied");

    let cases = [
        (vec![], "no frame"),
        (
            vec![
                frame(vec![0.5; 3200], 2, 48_000),
                frame(vec![0.5; 3200], 2, 48_000),
            ],
            "two frames",
        ),
        (vec![frame(vec![0.5; 3200], 1, 48_000)], "mono"),
        (vec![frame(vec![0.5; 3200], 2, 44_100)], "44.1 kHz"),
        (vec![frame(vec![0.5; 3198], 2, 48_000)], "short"),
    ];
    for (frames, what) in cases {
        let b = VbanBlock::from_frames(9, frames);
        assert_eq!(b.samples, None, "{what}");
        assert!(b.substituted, "{what}");
        assert_eq!(b.due_100ns, 9);
    }
    let s = VbanBlock::silence(11);
    assert_eq!((s.due_100ns, s.samples, s.substituted), (11, None, false));
}

#[test]
fn an_overflow_drops_the_oldest_block_and_counts_it() {
    let out = VbanOut::new();
    for due in 1..=10 {
        out.push(block(due, 0.0));
    }
    assert_eq!(out.queued(), 10, "the bound holds 10");
    assert_eq!(out.status().blocks_dropped, 0);
    out.push(block(11, 0.0));
    assert_eq!(out.queued(), 10);
    assert_eq!(out.status().blocks_dropped, 1);
    let mut dues = Vec::new();
    while let VbanTake::Block(b) = out.take_timeout(Duration::ZERO) {
        dues.push(b.due_100ns);
    }
    assert_eq!(dues, (2..=11).collect::<Vec<i64>>(), "block 1 was dropped");
    assert_eq!(VBAN_QUEUE_BOUND, 10);
}

#[test]
fn take_waits_for_a_block_returns_idle_on_timeout_and_drains_before_stopped() {
    let out = Arc::new(VbanOut::new());
    let t = Instant::now();
    assert_eq!(out.take_timeout(Duration::from_millis(100)), VbanTake::Idle);
    assert!(t.elapsed() >= Duration::from_millis(80), "it waited");

    let pusher = out.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        pusher.push(block(5, 0.0));
    });
    let t = Instant::now();
    assert_eq!(
        out.take_timeout(Duration::from_secs(10)),
        VbanTake::Block(block(5, 0.0))
    );
    assert!(t.elapsed() < Duration::from_secs(5), "woken by the push");
    h.join().unwrap();

    out.push(block(6, 0.0));
    let t = Instant::now();
    assert_eq!(
        out.take_timeout(Duration::from_secs(10)),
        VbanTake::Block(block(6, 0.0))
    );
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "a queued block is taken at once"
    );

    out.push(block(7, 0.0));
    out.stop();
    assert_eq!(
        out.take_timeout(Duration::from_secs(10)),
        VbanTake::Block(block(7, 0.0))
    );
    let t = Instant::now();
    assert_eq!(out.take_timeout(Duration::from_secs(10)), VbanTake::Stopped);
    assert!(t.elapsed() < Duration::from_secs(5));
}

// --- the paced sender -------------------------------------------------------

#[test]
fn packet_k_goes_out_at_due_plus_l_plus_k_240ths_exactly() {
    let out = out_with(active_config(&["10.0.0.1:6980"]));
    let mut clock = FakeClock::at(D);
    let mut sink = RecordingSink::on(&clock);
    let mut sender = VbanSender::default();
    assert_eq!(
        sender.send_block(&out, &block(D, 0.5), &mut sink, &mut clock),
        8
    );
    let times: Vec<i64> = sink.sent.iter().map(|s| s.0).collect();
    let expected: Vec<i64> = (0..8).map(|k| D + L + packet_offset_100ns(k)).collect();
    assert_eq!(times, expected);
    assert_eq!(
        clock.sleeps,
        vec![
            333_333, 41_666, 41_667, 41_667, 41_666, 41_667, 41_667, 41_666
        ],
        "one wait per packet, never a burst"
    );
    let st = out.status();
    assert_eq!(st.packets_sent, 8);
    assert_eq!(st.late_sends, 0);
    assert_eq!(st.send_interval_p99_us, 4_166);
    assert_eq!(st.frame_counter, 7);
    assert_eq!(st.send_errors, 0);
    let a: SocketAddr = "10.0.0.1:6980".parse().unwrap();
    assert!(sink.sent.iter().all(|s| s.1 == a));
    assert_eq!(sender.next_counter(), 8);

    // The next boundary's block continues the stream one slot later.
    let next = D + 333_333;
    assert_eq!(
        sender.send_block(&out, &block(next, 0.5), &mut sink, &mut clock),
        8
    );
    assert_eq!(sink.sent[8].0, next + L);
    assert_eq!(sink.sent[8].0 - sink.sent[7].0, 41_667);
    let counters: Vec<u32> = sink
        .sent
        .iter()
        .map(|s| parse_packet(&s.2).counter)
        .collect();
    assert_eq!(counters, (0..16).collect::<Vec<u32>>());
    assert_eq!(out.status().send_interval_p99_us, 4_166);
}

#[test]
fn a_packet_more_than_2_ms_after_its_due_time_is_a_late_send() {
    let out = out_with(active_config(&["10.0.0.1:6980"]));
    let mut clock = FakeClock::at(D + L + 20_001);
    let mut sink = RecordingSink::on(&clock);
    let mut sender = VbanSender::default();
    sender.send_block(&out, &block(D, 0.5), &mut sink, &mut clock);
    assert_eq!(out.status().late_sends, 1, "only packet 0 was late");
    assert_eq!(clock.sleeps[0], 41_666 - 20_001, "packet 1 is on time");
    assert_eq!(clock.sleeps.len(), 7, "no wait for the late packet");

    let out = out_with(active_config(&["10.0.0.1:6980"]));
    let mut clock = FakeClock::at(D + L + 20_000);
    let mut sink = RecordingSink::on(&clock);
    VbanSender::default().send_block(&out, &block(D, 0.5), &mut sink, &mut clock);
    assert_eq!(out.status().late_sends, 0, "exactly 2 ms is not late");
}

#[test]
fn the_wait_is_clamped_to_zero_and_to_the_max() {
    assert_eq!(plan_wait_100ns(100, 105), 5);
    assert_eq!(plan_wait_100ns(110, 105), 0);
    assert_eq!(plan_wait_100ns(0, 1_000_000_000), VBAN_MAX_WAIT_100NS);
    assert_eq!(VBAN_MAX_WAIT_100NS, 1_333_332);
    assert_eq!(interval_us(1_000, 42_666), 4_166);
    assert_eq!(interval_us(42_666, 1_000), 0, "a backward read is 0");
}

#[test]
fn disabled_or_no_resolved_target_sends_nothing() {
    let unresolved = {
        let mut cfg = active_config(&["10.0.0.1:6980"]);
        cfg.targets[0].addr = None;
        cfg
    };
    let disabled = {
        let mut cfg = active_config(&["10.0.0.1:6980"]);
        cfg.enabled = false;
        cfg
    };
    let no_targets = active_config(&[]);
    for (cfg, what) in [
        (VbanConfig::default(), "default"),
        (disabled, "disabled"),
        (no_targets, "no targets"),
        (unresolved, "unresolved"),
    ] {
        assert!(!cfg.is_active(), "{what}");
        let out = out_with(cfg);
        let mut clock = FakeClock::at(D);
        let mut sink = RecordingSink::on(&clock);
        let mut sender = VbanSender::default();
        assert_eq!(
            sender.send_block(&out, &block(D, 0.5), &mut sink, &mut clock),
            0
        );
        assert!(sink.sent.is_empty(), "{what}");
        assert!(clock.sleeps.is_empty(), "{what}: no pacing either");
        assert_eq!(out.status().packets_sent, 0, "{what}");
        assert_eq!(sender.next_counter(), 0, "{what}: the counter did not move");
    }
    assert!(active_config(&["10.0.0.1:6980"]).is_active());
}

#[test]
fn every_packet_goes_to_every_target_and_a_failing_one_is_counted() {
    let a: SocketAddr = "10.0.0.1:6980".parse().unwrap();
    let b: SocketAddr = "10.0.0.2:6980".parse().unwrap();
    let out = out_with(active_config(&["10.0.0.1:6980", "10.0.0.2:6980"]));
    let mut clock = FakeClock::at(D);
    let mut sink = RecordingSink::on(&clock);
    sink.fail_to = Some(b);
    VbanSender::default().send_block(&out, &block(D, 0.5), &mut sink, &mut clock);
    assert_eq!(sink.sent.len(), 8, "every packet still reached a");
    assert!(sink.sent.iter().all(|s| s.1 == a));
    let st = out.status();
    assert_eq!(
        st.packets_sent, 8,
        "a packet counts once, whatever the targets"
    );
    assert_eq!(st.send_errors, 8, "one failed datagram per packet to b");

    let out = out_with(active_config(&["10.0.0.1:6980", "10.0.0.2:6980"]));
    let mut sink = RecordingSink::on(&clock);
    VbanSender::default().send_block(&out, &block(D, 0.5), &mut sink, &mut clock);
    let order: Vec<SocketAddr> = sink.sent.iter().take(4).map(|s| s.1).collect();
    assert_eq!(order, vec![a, b, a, b]);
}

#[test]
fn a_substituted_block_is_counted_even_while_disabled() {
    let out = VbanOut::new();
    let mut clock = FakeClock::at(D);
    let mut sink = RecordingSink::on(&clock);
    let b = VbanBlock::from_frames(D, Vec::new());
    VbanSender::default().send_block(&out, &b, &mut sink, &mut clock);
    VbanSender::default().send_block(&out, &VbanBlock::silence(D), &mut sink, &mut clock);
    assert_eq!(out.status().blocks_substituted, 1);
}

#[test]
fn the_p99_covers_the_last_1200_intervals() {
    let out = VbanOut::new();
    let record = |us| {
        out.record_packet(SentPacket {
            late: false,
            interval_us: Some(us),
            errors: 0,
            counter: 0,
        });
    };
    record(50_000);
    for _ in 0..10 {
        record(4_166);
    }
    assert_eq!(
        out.status().send_interval_p99_us,
        50_000,
        "11 samples: p99 = max"
    );
    for _ in 0..VBAN_INTERVAL_WINDOW {
        record(4_166);
    }
    assert_eq!(
        out.status().send_interval_p99_us,
        4_166,
        "the old gap aged out"
    );
    assert_eq!(VBAN_INTERVAL_WINDOW, 1200);
}

// --- the counter through the real program output ---------------------------

fn source_job(stamp: i64, v: f32) -> ProgramJob {
    ProgramJob::Source(SubmitJob {
        width: 4,
        height: 2,
        stride: 4,
        video: SharedFrame::new(vec![0u8; 4 * 2 * 3 / 2]),
        audio: vec![frame(vec![v; 3200], 2, 48_000)],
        video_tc_100ns: stamp,
        audio_tc_100ns: stamp + 5,
    })
}

#[test]
fn the_counter_is_contiguous_across_blocks_a_cut_and_standby() {
    let out = Arc::new(out_with(active_config(&["10.0.0.1:6980"])));
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    let mut program = ProgramOutput::new(sender, 4, 2).with_vban(out.clone());
    let s = |j: i64| D + j * 333_333;
    program.submit(source_job(s(0), 0.5), s(0)); // source A
    program.submit(source_job(s(1), 0.5), s(1)); // source A
    program.submit(source_job(s(2), -0.25), s(2)); // the cut: source B
    program.submit(ProgramJob::Standby { stamp_100ns: s(3) }, s(3)); // standby
    program.submit(source_job(s(4), -0.25), s(4)); // source B
    assert_eq!(out.queued(), 5, "one block per submitted pair");
    out.stop();

    let (tx, rx) = mpsc::channel();
    let looped = out.clone();
    std::thread::spawn(move || {
        let mut clock = FakeClock::at(D);
        let mut sink = RecordingSink::on(&clock);
        let sender = run_vban_loop(&looped, &mut sink, &mut clock);
        tx.send((sink.sent, sender.next_counter())).unwrap();
    });
    let (sent, next) = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the loop drains and stops");
    assert_eq!(sent.len(), 40);
    assert_eq!(next, 40);
    let per_block = [4_194_304, 4_194_304, -2_097_152, 0, -2_097_152];
    for (i, (at, _, bytes)) in sent.iter().enumerate() {
        let (j, k) = (i / 8, i % 8);
        let p = parse_packet(bytes);
        assert_eq!(p.counter, i as u32, "contiguous");
        assert_eq!(*at, s(j as i64) + L + packet_offset_100ns(k), "on schedule");
        assert!(
            p.samples.iter().all(|&x| x == per_block[j]),
            "block {j}: the program's audio"
        );
    }
    assert_eq!(out.status().packets_sent, 40);
    assert_eq!(out.status().frame_counter, 39);
    drop(program); // the output stays alive until here
}

// --- the real socket ----------------------------------------------------------

#[test]
fn one_block_over_loopback_udp_decodes_to_8_packets_of_the_block() {
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let target = rx.local_addr().unwrap().to_string();
    let out = out_with(active_config(&[target.as_str()]));
    let mut socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let samples = ramp_block();
    let b = VbanBlock {
        due_100ns: D,
        samples: Some(samples.clone()),
        substituted: false,
    };
    let mut clock = FakeClock::at(D);
    let mut sender = VbanSender::default();
    assert_eq!(sender.send_block(&out, &b, &mut socket, &mut clock), 8);
    let mut pcm = Vec::new();
    let mut buf = [0u8; 2048];
    for k in 0..8u32 {
        let n = rx.recv(&mut buf).expect("a packet within 5 s");
        assert_eq!(n, 1228);
        let p = parse_packet(&buf[..n]);
        assert_eq!(p.counter, k);
        assert_eq!(&p.name, b"sp-program\0\0\0\0\0\0");
        pcm.extend(p.samples);
    }
    let expected: Vec<i32> = samples.iter().map(|&x| f32_to_int24(x)).collect();
    assert_eq!(pcm, expected);
    assert_eq!(out.status().packets_sent, 8);
    assert_eq!(out.status().send_errors, 0);
}

#[test]
fn the_wall_clock_reads_the_wall_and_sleeps_for_real() {
    let (wall, handle) = WallClock::settable(D);
    let mut clock = WallVbanClock::new(wall);
    assert_eq!(clock.now_100ns(), D);
    handle.set(D + 1_000_000);
    assert_eq!(clock.now_100ns(), D + 1_000_000);
    let t = Instant::now();
    clock.sleep_100ns(200_000);
    let slept = t.elapsed();
    assert!(slept >= Duration::from_millis(15), "slept {slept:?}");
    assert!(slept < Duration::from_secs(2), "slept {slept:?}");
}

// --- settings + resolution ------------------------------------------------------

#[tokio::test]
async fn the_settings_load_with_defaults_and_stored_values() {
    use crate::db::models::set_setting;
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    assert_eq!(
        load_vban_settings(&pool).await.unwrap(),
        VbanSettings {
            enabled: false,
            stream_name: "sp-program".into(),
            targets: String::new(),
        }
    );
    set_setting(&pool, "vban_enabled", "true").await.unwrap();
    set_setting(&pool, "vban_stream_name", "  foh-test ")
        .await
        .unwrap();
    set_setting(&pool, "vban_targets", "fohabl.lan:6980, ,lv1.lan:6980 ")
        .await
        .unwrap();
    let s = load_vban_settings(&pool).await.unwrap();
    assert!(s.enabled);
    assert_eq!(s.stream_name, "foh-test");
    assert_eq!(s.target_specs(), vec!["fohabl.lan:6980", "lv1.lan:6980"]);

    set_setting(&pool, "vban_enabled", "yes").await.unwrap();
    set_setting(&pool, "vban_stream_name", "   ").await.unwrap();
    let s = load_vban_settings(&pool).await.unwrap();
    assert!(!s.enabled, "only \"true\" enables");
    assert_eq!(s.stream_name, "sp-program", "a blank name is the default");
}

#[test]
fn at_most_8_targets_are_used() {
    let ten = VbanSettings {
        enabled: true,
        stream_name: "sp-program".into(),
        targets: (1..=10)
            .map(|i| format!("10.0.0.{i}:6980"))
            .collect::<Vec<_>>()
            .join(", "),
    };
    let specs = ten.target_specs();
    assert_eq!(specs.len(), 8);
    assert_eq!(specs[0], "10.0.0.1:6980");
    assert_eq!(specs[7], "10.0.0.8:6980");
    assert_eq!(ten.ignored_targets(), 2);
    let three = VbanSettings {
        targets: "a:1,b:2, c:3".into(),
        ..ten
    };
    assert_eq!(three.target_specs(), vec!["a:1", "b:2", "c:3"]);
    assert_eq!(three.ignored_targets(), 0);
    assert_eq!(VBAN_MAX_TARGETS, 8);
}

#[test]
fn the_loop_reports_running_while_it_runs() {
    let out = Arc::new(VbanOut::new());
    assert!(!out.is_running());
    assert!(!out.status().running);
    let (tx, rx) = mpsc::channel();
    let looped = out.clone();
    let h = std::thread::spawn(move || {
        let mut clock = FakeClock::at(D);
        let mut sink = RecordingSink::on(&clock);
        run_vban_loop(&looped, &mut sink, &mut clock);
        tx.send(()).unwrap();
    });
    let t = Instant::now();
    while !out.status().running {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "the loop never started"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    out.stop();
    rx.recv_timeout(Duration::from_secs(10))
        .expect("the loop stops");
    h.join().unwrap();
    assert!(!out.is_running(), "cleared on exit");
}

#[test]
fn a_failed_resolve_keeps_the_last_good_address() {
    let specs: Vec<String> = ["a:1", "b:2", "c:3"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let prev_b: SocketAddr = "10.0.0.2:2".parse().unwrap();
    let previous = vec![VbanTarget {
        spec: "b:2".into(),
        addr: Some(prev_b),
        error: None,
    }];
    let mut resolve = |spec: &str| -> Result<SocketAddr, String> {
        match spec {
            "a:1" => Ok("10.0.0.1:1".parse().unwrap()),
            "b:2" => Err("dns down".to_string()),
            _ => Err("nx".to_string()),
        }
    };
    let got = resolve_targets(&specs, &previous, &mut resolve);
    assert_eq!(
        got,
        vec![
            VbanTarget {
                spec: "a:1".into(),
                addr: Some("10.0.0.1:1".parse().unwrap()),
                error: None,
            },
            VbanTarget {
                spec: "b:2".into(),
                addr: Some(prev_b),
                error: Some("dns down".into()),
            },
            VbanTarget {
                spec: "c:3".into(),
                addr: None,
                error: Some("nx".into()),
            },
        ]
    );
}

#[test]
fn the_system_resolver_takes_the_first_ipv4_address() {
    let local: SocketAddr = "127.0.0.1:6980".parse().unwrap();
    assert_eq!(system_resolve("127.0.0.1:6980"), Ok(local));
    assert!(system_resolve("no-port-here").is_err());
    assert_eq!(
        system_resolve("[::1]:6980"),
        Err("no IPv4 address".to_string())
    );
    let v6: SocketAddr = "[::1]:1".parse().unwrap();
    let v4a: SocketAddr = "10.0.0.1:1".parse().unwrap();
    let v4b: SocketAddr = "10.0.0.2:1".parse().unwrap();
    assert_eq!(pick_ipv4([v6, v4a, v4b]), Some(v4a));
    assert_eq!(pick_ipv4([v6]), None);
}

#[tokio::test]
async fn resolve_config_builds_the_config_from_the_settings() {
    let settings = VbanSettings {
        enabled: true,
        stream_name: "foh-test".into(),
        targets: "127.0.0.1:6980, bogus".into(),
    };
    let cfg = resolve_config(settings, Vec::new()).await;
    assert!(cfg.enabled);
    assert_eq!(cfg.stream_name, "foh-test");
    assert_eq!(cfg.targets.len(), 2);
    let local: SocketAddr = "127.0.0.1:6980".parse().unwrap();
    assert_eq!(cfg.targets[0].addr, Some(local));
    assert_eq!(cfg.targets[1].addr, None);
    assert!(cfg.targets[1].error.is_some());
    assert!(cfg.is_active());
}

#[test]
fn the_config_carries_the_wire_name_and_the_status_its_targets() {
    let settings = VbanSettings {
        enabled: true,
        stream_name: "é-a-very-long-stream".into(),
        targets: String::new(),
    };
    let cfg = VbanConfig::new(
        &settings,
        vec![
            VbanTarget {
                spec: "fohabl.lan:6980".into(),
                addr: Some("10.77.7.30:6980".parse().unwrap()),
                error: None,
            },
            VbanTarget {
                spec: "lv1.lan:6980".into(),
                addr: None,
                error: Some("nx".into()),
            },
        ],
    );
    assert_eq!(cfg.stream_name, "_-a-very-long-st", "what goes on the wire");
    assert_eq!(&cfg.name_bytes, b"_-a-very-long-st");
    let out = out_with(cfg);
    let st = out.status();
    assert!(st.enabled);
    assert_eq!(st.stream_name, "_-a-very-long-st");
    assert_eq!(
        st.targets,
        vec![
            VbanTargetStatus {
                target: "fohabl.lan:6980".into(),
                addr: Some("10.77.7.30:6980".into()),
                error: None,
            },
            VbanTargetStatus {
                target: "lv1.lan:6980".into(),
                addr: None,
                error: Some("nx".into()),
            },
        ]
    );
    let def = VbanConfig::default();
    assert!(!def.enabled);
    assert_eq!(def.stream_name, "sp-program");
    assert!(def.targets.is_empty());
}

#[test]
fn resolve_and_log_cadences() {
    assert!(needs_resolve(false, true, None), "never resolved");
    assert!(!needs_resolve(false, true, Some(Duration::from_secs(59))));
    assert!(needs_resolve(false, true, Some(Duration::from_secs(60))));
    assert!(
        needs_resolve(true, true, Some(Duration::from_secs(1))),
        "changed"
    );
    assert!(
        needs_resolve(true, false, Some(Duration::from_secs(1))),
        "changed"
    );
    assert!(
        !needs_resolve(false, false, Some(Duration::from_secs(60))),
        "a disabled output does not re-resolve"
    );
    assert!(!needs_resolve(false, false, None));
    assert!(should_log(1));
    assert!(!should_log(2));
    assert!(!should_log(999));
    assert!(should_log(1000));
    assert!(!should_log(1001));
    assert!(should_log(2000));
}
