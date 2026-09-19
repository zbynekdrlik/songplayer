//! Bundled-`ffmpeg` encoder child for the live preview stream (#178).
//!
//! On the first WS viewer, [`ensure_running`] spawns ONE `ffmpeg` child per
//! watched pipeline that reads the raw tapped NV12 video and post-mix f32 audio
//! over two loopback TCP listeners, encodes H.264 (hardware where available) +
//! AAC, and muxes fragmented MP4 to its stdout — which a reader thread splits
//! (`fmp4_relay`) and broadcasts to the viewers. The child is killed once the
//! last viewer has been gone for [`VIEWER_TTL`].
//!
//! The pure parts — the encoder ladder, the `-encoders` probe parse, and the
//! exact argument vector — are Linux unit-tested. The child/TCP/feeder/monitor
//! lifecycle is Windows-runtime glue (`mutants::skip`, box-verified), but is
//! written cross-platform so it compiles and links on the Linux CI.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use super::fmp4_relay::BoxSplitter;
use super::preview_stream::{OUT_H, OUT_W, StreamShared};

/// Encoder preference ladder: hardware first, software last.
pub const ENCODER_LADDER: [&str; 4] = ["h264_nvenc", "h264_qsv", "h264_amf", "libx264"];

/// How long after the last viewer leaves before the child is killed.
pub const VIEWER_TTL: Duration = Duration::from_secs(5);

/// The encoder chosen for this process (probed once), exposed on `/api/v1/status`.
static CHOSEN_ENCODER: OnceLock<String> = OnceLock::new();

/// The encoder that actually WORKS at runtime, pinned once a hardware encoder
/// has proven broken and we fell back to `libx264` (#178 box: the bundled
/// ffmpeg's `h264_nvenc` needs an Nvidia driver newer than the box has — it
/// selects at probe time but fails to open at encode time, producing 0 bytes).
/// Once set it overrides the probe result for every subsequent child, so the
/// broken hardware encoder is not retried on every viewer.
static WORKING_ENCODER: Mutex<Option<String>> = Mutex::new(None);

/// The runtime-pinned working encoder, if a fallback has happened.
#[cfg_attr(test, mutants::skip)]
fn pinned_encoder() -> Option<String> {
    WORKING_ENCODER.lock().ok().and_then(|g| g.clone())
}

/// The encoder this process is using: the runtime-pinned working one if a
/// hardware encoder proved broken, else the probe selection. Exposed on
/// `/api/v1/status`.
#[cfg_attr(test, mutants::skip)]
pub fn chosen_encoder() -> Option<String> {
    pinned_encoder().or_else(|| CHOSEN_ENCODER.get().cloned())
}

/// Pick the first ladder encoder present in `available`, else `libx264` (the
/// GPL build's guaranteed software encoder).
pub fn select_encoder(available: &[String]) -> String {
    for cand in ENCODER_LADDER {
        if available.iter().any(|a| a == cand) {
            return cand.to_string();
        }
    }
    "libx264".to_string()
}

/// Extract the ladder encoders present in `ffmpeg -encoders` output. Each line
/// is `<flags> <name> <description>`; we keep names that are in [`ENCODER_LADDER`].
pub fn parse_available_encoders(encoders_stdout: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in encoders_stdout.lines() {
        // The name is the second whitespace-separated token (after the flag col).
        if let Some(name) = line.split_whitespace().nth(1) {
            if ENCODER_LADDER.contains(&name) && !found.iter().any(|f| f == name) {
                found.push(name.to_string());
            }
        }
    }
    found
}

/// Build the exact `ffmpeg` argument vector. `video_port` / `audio_port` are the
/// loopback listeners the child connects back to; `encoder` is the `-c:v` codec.
/// `libx264` additionally gets `-preset ultrafast -tune zerolatency` for the
/// low-latency software path. `lead_ms` compensates the decode-seam A/V offset
/// (#178 round 2): on the SDK-clocked path the #192 lookahead makes tapped audio
/// LEAD video by `lead_ms`, so `-itsoffset <lead_ms/1000>` is inserted BEFORE the
/// audio input to delay it back into sync; `lead_ms == 0` (paced path) omits it.
pub fn build_ffmpeg_args(
    video_port: u16,
    audio_port: u16,
    encoder: &str,
    lead_ms: u32,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        // Video input: raw NV12 at the fixed canvas size, wall-clock stamped.
        "-use_wallclock_as_timestamps".into(),
        "1".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "nv12".into(),
        "-s".into(),
        format!("{OUT_W}x{OUT_H}"),
        "-i".into(),
        format!("tcp://127.0.0.1:{video_port}"),
    ];
    // #178 round 2 A/V-sync: on the SDK-clocked path the #192 decoder lookahead
    // makes the tapped audio LEAD the video by `lead_ms`, so delay the audio
    // input by that much with `-itsoffset` (an INPUT option, placed before the
    // audio `-i`). Matched (not `> 0`) so no `!= 0` equivalent mutant survives:
    // lead 0 (paced path) emits NO flag; any other value emits the seconds form.
    match lead_ms {
        0 => {}
        ms => {
            a.push("-itsoffset".into());
            a.push(format!("{:.3}", ms as f64 / 1000.0));
        }
    }
    a.extend([
        // Audio input: interleaved f32, 48 kHz stereo, wall-clock stamped.
        "-use_wallclock_as_timestamps".to_string(),
        "1".to_string(),
        "-f".to_string(),
        "f32le".to_string(),
        "-ar".to_string(),
        "48000".to_string(),
        "-ac".to_string(),
        "2".to_string(),
        "-i".to_string(),
        format!("tcp://127.0.0.1:{audio_port}"),
        // Video encode.
        "-c:v".to_string(),
        encoder.to_string(),
    ]);
    if encoder == "libx264" {
        a.push("-preset".into());
        a.push("ultrafast".into());
        a.push("-tune".into());
        a.push("zerolatency".into());
    }
    a.extend(
        [
            "-b:v",
            "1200k",
            "-g",
            "25",
            "-fps_mode",
            "cfr",
            "-r",
            "25",
            // Audio encode.
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            // Fragmented MP4 to stdout — keyframe-aligned fragments for MSE.
            "-movflags",
            "+frag_keyframe+empty_moov+default_base_moof",
            "-frag_duration",
            "500000",
            "-f",
            "mp4",
            "pipe:1",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    a
}

/// Probe the bundled ffmpeg's encoder list once and cache the selected encoder.
#[cfg_attr(test, mutants::skip)]
fn probe_and_select(ffmpeg: &Path) -> String {
    CHOSEN_ENCODER
        .get_or_init(|| {
            let out = Command::new(ffmpeg)
                .args(["-hide_banner", "-encoders"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output();
            let enc = match out {
                Ok(o) => {
                    let text = String::from_utf8_lossy(&o.stdout);
                    let avail = parse_available_encoders(&text);
                    select_encoder(&avail)
                }
                Err(e) => {
                    warn!(error = %e, "preview-encoder: -encoders probe failed, defaulting to libx264");
                    "libx264".to_string()
                }
            };
            info!(encoder = %enc, "preview-encoder: selected H.264 encoder");
            enc
        })
        .clone()
}

/// Ensure an encoder child is running for `shared`. Idempotent: only the caller
/// that wins the running-flag actually spawns the supervisor thread.
#[cfg_attr(test, mutants::skip)]
pub fn ensure_running(shared: Arc<StreamShared>, ffmpeg: std::path::PathBuf) {
    if !shared.try_claim_encoder() {
        return; // already running
    }
    // A previously-pinned working encoder (a hardware encoder proved broken and
    // we fell back) skips the broken probe selection entirely (#178 box).
    let encoder = pinned_encoder().unwrap_or_else(|| probe_and_select(&ffmpeg));
    let thread_shared = shared.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("preview-enc-{}", shared.label()))
        .spawn(move || {
            supervise(thread_shared.clone(), &ffmpeg, &encoder);
            thread_shared.release_encoder();
        });
    if let Err(e) = spawned {
        warn!(error = %e, "preview-encoder: failed to spawn supervisor thread");
        shared.release_encoder();
    }
}

/// Own the child + feeders + reader for one run, until the viewers are gone
/// past the TTL or the child exits. Falls back to `libx264` once if a hardware
/// encoder child dies before producing the init segment.
#[cfg_attr(test, mutants::skip)]
fn supervise(shared: Arc<StreamShared>, ffmpeg: &Path, encoder: &str) {
    let mut encoder = encoder.to_string();
    let mut allow_fallback = encoder != "libx264";
    loop {
        match run_child(&shared, ffmpeg, &encoder) {
            RunOutcome::ViewersGone => {
                info!(
                    label = shared.label(),
                    "preview-encoder: last viewer gone, child stopped"
                );
                return;
            }
            RunOutcome::ChildExitedNoInit if allow_fallback => {
                warn!(
                    label = shared.label(),
                    encoder = %encoder,
                    "preview-encoder: child produced no stream, falling back to libx264"
                );
                encoder = "libx264".to_string();
                allow_fallback = false;
                // Pin libx264 so every subsequent child (this pipeline or any
                // other) skips the broken hardware encoder instead of eating a
                // failed attempt per viewer (#178 box: nvenc driver too old).
                if let Ok(mut g) = WORKING_ENCODER.lock() {
                    *g = Some("libx264".to_string());
                }
            }
            RunOutcome::ChildExitedNoInit | RunOutcome::ChildExited => {
                warn!(
                    label = shared.label(),
                    "preview-encoder: child exited, stopping"
                );
                return;
            }
        }
    }
}

enum RunOutcome {
    ViewersGone,
    ChildExited,
    ChildExitedNoInit,
}

/// One child run: bind loopback listeners, spawn ffmpeg, accept its two
/// connections, feed video+audio, split its stdout into the relay, and monitor
/// for the viewer-TTL / child exit.
#[cfg_attr(test, mutants::skip)]
fn run_child(shared: &Arc<StreamShared>, ffmpeg: &Path, encoder: &str) -> RunOutcome {
    let v_listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            warn!(error = %e, "preview-encoder: bind video listener failed");
            return RunOutcome::ChildExited;
        }
    };
    let a_listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            warn!(error = %e, "preview-encoder: bind audio listener failed");
            return RunOutcome::ChildExited;
        }
    };
    let v_port = match v_listener.local_addr() {
        Ok(a) => a.port(),
        Err(_) => return RunOutcome::ChildExited,
    };
    let a_port = match a_listener.local_addr() {
        Ok(a) => a.port(),
        Err(_) => return RunOutcome::ChildExited,
    };

    let args = build_ffmpeg_args(v_port, a_port, encoder, shared.lead_ms());
    let mut cmd = Command::new(ffmpeg);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    apply_windows_flags(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "preview-encoder: ffmpeg spawn failed");
            return RunOutcome::ChildExited;
        }
    };
    info!(
        label = shared.label(),
        encoder, v_port, a_port, "preview-encoder: child started"
    );

    // ffmpeg (as TCP client) opens its inputs SEQUENTIALLY: it connects input 0
    // (the rawvideo tcp), and `find_stream_info` reads several frames from it
    // BEFORE it opens and connects input 1 (the audio tcp). So the video feeder
    // must start the MOMENT the video socket connects — otherwise ffmpeg blocks
    // probing an empty video input, never connects the audio input, the audio
    // accept times out, and the child produces no init segment. That was the box
    // deadlock behind every "child did not connect its inputs" (#178, first live
    // box run: both nvenc and libx264 failed identically at the 5 s accept).
    let shutdown = Arc::new(AtomicBool::new(false));
    let v_sock = match accept_with_deadline(&v_listener, Duration::from_secs(5)) {
        Some(s) => s,
        None => {
            warn!(
                label = shared.label(),
                "preview-encoder: child did not connect its video input"
            );
            let _ = child.kill();
            let _ = child.wait();
            return RunOutcome::ChildExitedNoInit;
        }
    };
    let v_feeder = spawn_video_feeder(shared.clone(), v_sock, shutdown.clone());
    let a_sock = match accept_with_deadline(&a_listener, Duration::from_secs(5)) {
        Some(s) => s,
        None => {
            warn!(
                label = shared.label(),
                "preview-encoder: child did not connect its audio input"
            );
            shutdown.store(true, Ordering::Relaxed);
            let _ = child.kill();
            let _ = child.wait();
            let _ = v_feeder.join();
            return RunOutcome::ChildExitedNoInit;
        }
    };
    let a_feeder = spawn_audio_feeder(shared.clone(), a_sock, shutdown.clone());
    // Per-child "did THIS child produce any stdout" — the relay's cached init
    // segment persists across children, so it cannot be the failed-child signal:
    // a broken hardware child (nvenc that never opens its encoder) produces ZERO
    // bytes yet the relay still holds a prior working child's init, which made
    // the old `relay().init().is_some()` check misread it as a clean exit and
    // skip the libx264 fallback (#178 box).
    let produced = Arc::new(AtomicBool::new(false));
    let reader = spawn_stdout_reader(shared.clone(), child.stdout.take(), produced.clone());

    let outcome = monitor_loop(shared, &mut child, &produced);

    // Tear down: stop feeders, kill child, join everything.
    shutdown.store(true, Ordering::Relaxed);
    let _ = child.kill();
    let _ = child.wait();
    let _ = v_feeder.join();
    let _ = a_feeder.join();
    let _ = reader.join();
    outcome
}

/// Watch the viewer count + child liveness. Returns when the viewers have been
/// gone past [`VIEWER_TTL`] or the child exits on its own.
#[cfg_attr(test, mutants::skip)]
fn monitor_loop(
    shared: &Arc<StreamShared>,
    child: &mut Child,
    produced: &Arc<AtomicBool>,
) -> RunOutcome {
    let mut empty_since: Option<Instant> = None;
    loop {
        // Child exited on its own?
        if let Ok(Some(_)) = child.try_wait() {
            // A child that emitted ANY stdout this run exited normally; one that
            // produced nothing (a broken encoder) is a failure eligible for the
            // libx264 fallback — regardless of the relay's cached init (#178).
            return if produced.load(Ordering::Relaxed) {
                RunOutcome::ChildExited
            } else {
                RunOutcome::ChildExitedNoInit
            };
        }
        if shared.has_viewer() {
            empty_since = None;
        } else {
            let since = *empty_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= VIEWER_TTL {
                return RunOutcome::ViewersGone;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Accept one connection with a deadline (the listener is set non-blocking so a
/// child that never starts cannot hang the supervisor).
#[cfg_attr(test, mutants::skip)]
fn accept_with_deadline(listener: &TcpListener, deadline: Duration) -> Option<TcpStream> {
    let _ = listener.set_nonblocking(true);
    let start = Instant::now();
    loop {
        match listener.accept() {
            Ok((sock, _)) => {
                let _ = sock.set_nonblocking(false);
                return Some(sock);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if start.elapsed() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// Feed tapped NV12 frames to the child's video socket until shutdown or a
/// write error (child gone).
#[cfg_attr(test, mutants::skip)]
fn spawn_video_feeder(
    shared: Arc<StreamShared>,
    mut sock: TcpStream,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("preview-vfeed".into())
        .spawn(move || {
            let rx = shared.video_receiver();
            while !shutdown.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(frame) => {
                        if sock.write_all(&frame).is_err() {
                            break;
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .expect("spawn preview video feeder")
}

/// Feed tapped interleaved-f32 audio to the child's audio socket (little-endian
/// f32 bytes) until shutdown or a write error.
#[cfg_attr(test, mutants::skip)]
fn spawn_audio_feeder(
    shared: Arc<StreamShared>,
    mut sock: TcpStream,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("preview-afeed".into())
        .spawn(move || {
            let rx = shared.audio_receiver();
            let mut bytes: Vec<u8> = Vec::new();
            while !shutdown.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(block) => {
                        bytes.clear();
                        bytes.reserve(block.len() * 4);
                        for s in &block {
                            bytes.extend_from_slice(&s.to_le_bytes());
                        }
                        if sock.write_all(&bytes).is_err() {
                            break;
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .expect("spawn preview audio feeder")
}

/// Read the child's fragmented-MP4 stdout, split it into init + fragments, and
/// feed the relay.
#[cfg_attr(test, mutants::skip)]
fn spawn_stdout_reader(
    shared: Arc<StreamShared>,
    stdout: Option<std::process::ChildStdout>,
    produced: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("preview-read".into())
        .spawn(move || {
            let mut stdout = match stdout {
                Some(s) => s,
                None => return,
            };
            let relay = shared.relay();
            let mut splitter = BoxSplitter::new();
            let mut buf = [0u8; 64 * 1024];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break, // EOF: child gone
                    Ok(n) => {
                        // Mark this child as having produced output — the
                        // failed-child signal (see monitor_loop / #178).
                        produced.store(true, Ordering::Relaxed);
                        for chunk in splitter.push(&buf[..n]) {
                            relay.ingest(chunk);
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .expect("spawn preview stdout reader")
}

/// Apply Windows-only process flags: `CREATE_NO_WINDOW` (no console popup, per
/// the subprocess rule) + `BELOW_NORMAL_PRIORITY_CLASS` (the encoder must never
/// steal CPU from the NDI submit / audio-emitter threads).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn apply_windows_flags(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
}

#[cfg(not(windows))]
#[cfg_attr(test, mutants::skip)]
fn apply_windows_flags(_cmd: &mut Command) {}

#[cfg(test)]
#[path = "preview_encoder_tests.rs"]
mod tests;
