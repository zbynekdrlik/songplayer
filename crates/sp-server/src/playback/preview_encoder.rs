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

/// The encoder this process selected, once a preview child has been spawned.
pub fn chosen_encoder() -> Option<String> {
    CHOSEN_ENCODER.get().cloned()
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
/// low-latency software path.
pub fn build_ffmpeg_args(video_port: u16, audio_port: u16, encoder: &str) -> Vec<String> {
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
        // Audio input: interleaved f32, 48 kHz stereo, wall-clock stamped.
        "-use_wallclock_as_timestamps".into(),
        "1".into(),
        "-f".into(),
        "f32le".into(),
        "-ar".into(),
        "48000".into(),
        "-ac".into(),
        "2".into(),
        "-i".into(),
        format!("tcp://127.0.0.1:{audio_port}"),
        // Video encode.
        "-c:v".into(),
        encoder.into(),
    ];
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
    let encoder = probe_and_select(&ffmpeg);
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

    let args = build_ffmpeg_args(v_port, a_port, encoder);
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

    // ffmpeg (as TCP client) connects back to both listeners at startup.
    let v_sock = accept_with_deadline(&v_listener, Duration::from_secs(5));
    let a_sock = accept_with_deadline(&a_listener, Duration::from_secs(5));
    let (v_sock, a_sock) = match (v_sock, a_sock) {
        (Some(v), Some(a)) => (v, a),
        _ => {
            warn!(
                label = shared.label(),
                "preview-encoder: child did not connect its inputs"
            );
            let _ = child.kill();
            let _ = child.wait();
            return RunOutcome::ChildExitedNoInit;
        }
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    let v_feeder = spawn_video_feeder(shared.clone(), v_sock, shutdown.clone());
    let a_feeder = spawn_audio_feeder(shared.clone(), a_sock, shutdown.clone());
    let reader = spawn_stdout_reader(shared.clone(), child.stdout.take());

    let outcome = monitor_loop(shared, &mut child);

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
fn monitor_loop(shared: &Arc<StreamShared>, child: &mut Child) -> RunOutcome {
    let mut empty_since: Option<Instant> = None;
    loop {
        // Child exited on its own?
        if let Ok(Some(_)) = child.try_wait() {
            return if shared.relay().init().is_some() {
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
