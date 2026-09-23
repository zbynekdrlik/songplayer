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

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use super::fmp4_relay::BoxSplitter;
use super::preview_stream::{
    OUT_H, OUT_W, PREVIEW_AUDIO_FRAMES_PER_MS, StreamShared, align_block, align_timeout,
    audio_preroll_samples, block_tail_range,
};

/// Encoder preference ladder: hardware first, software last.
pub const ENCODER_LADDER: [&str; 4] = ["h264_nvenc", "h264_qsv", "h264_amf", "libx264"];

/// How long after the last viewer leaves before the child is killed.
pub const VIEWER_TTL: Duration = Duration::from_secs(5);

/// Most encoder-child restarts allowed inside [`RESTART_WINDOW_MS`] (#178 item 12).
const RESTART_MAX: usize = 3;
/// Rolling window (ms) for the restart budget.
const RESTART_WINDOW_MS: u64 = 60_000;

/// Bounds encoder-child restarts to at most [`RESTART_MAX`] within a rolling
/// [`RESTART_WINDOW_MS`] (#178 item 12). A child that dies while viewers are
/// connected is respawned, but a crash LOOP (broken input, unusable encoder)
/// must not spin ffmpeg forever — once the budget is spent the supervisor gives
/// up and closes the stream so viewers see the end. Pure + Linux-unit-tested.
#[derive(Debug, Default)]
pub struct RestartBudget {
    /// Monotonic ms timestamps of recent restarts, within the rolling window.
    restarts_ms: VecDeque<u64>,
}

impl RestartBudget {
    /// Record a restart at `now_ms` and return whether it is ALLOWED (still
    /// within budget). Evicts restarts older than the rolling window first.
    pub fn allow(&mut self, now_ms: u64) -> bool {
        while let Some(&front) = self.restarts_ms.front() {
            if now_ms.saturating_sub(front) >= RESTART_WINDOW_MS {
                self.restarts_ms.pop_front();
            } else {
                break;
            }
        }
        if self.restarts_ms.len() >= RESTART_MAX {
            return false;
        }
        self.restarts_ms.push_back(now_ms);
        true
    }
}

/// Kills + waits the ffmpeg child on drop, so EVERY early return / propagated
/// spawn error / panic path in `run_child` tears the child down — no orphaned
/// ffmpeg process (#178 item 11).
struct ChildGuard(Child);

impl ChildGuard {
    fn get(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for ChildGuard {
    #[cfg_attr(test, mutants::skip)]
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Releases the per-pipeline encoder-running flag on EVERY exit path of the
/// supervisor thread — including a panic in `supervise` — so a crashed
/// supervisor never leaves the flag stuck claimed (which would block every
/// future viewer from starting a child, #178 item 11).
struct EncoderReleaseGuard(Arc<StreamShared>);

impl Drop for EncoderReleaseGuard {
    #[cfg_attr(test, mutants::skip)]
    fn drop(&mut self) {
        self.0.release_encoder();
    }
}

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
/// low-latency software path.
///
/// #178 round 3 — ONLY the video input is wall-clock stamped. The raw f32le PCM
/// input keeps its SAMPLE-COUNT timestamps: `-use_wallclock_as_timestamps 1` on
/// the bursty PCM made the box's ffmpeg (N-123867, 2026-04) mux ZERO audio
/// packets (1 traf per moof), and an fMP4 whose audio track never fills is
/// unplayable in MSE forever (`buffered` is the track intersection). A/V is
/// aligned on OUR side instead — the audio feeder prepends silence
/// ([`audio_preroll_samples`](super::preview_stream::audio_preroll_samples)) —
/// because `-itsoffset` was not a dependable lever on the box (audio `start_time`
/// stayed 0.000). So there is NO `-itsoffset` and no `lead_ms` argument.
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
    ];
    a.extend([
        // Audio input: interleaved f32, 48 kHz stereo. NO wall-clock stamps —
        // the sample-count timestamps are what the box's ffmpeg muxes (see the
        // fn doc); the silence preroll in the audio feeder aligns it to video.
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
            // #184 round F: fit a ~1 Mb/s remote (internet) uplink. 500k video +
            // 64k audio ≈ 0.6 Mb/s, with a bounded keyframe overshoot
            // (-maxrate/-bufsize = the bitrate, so a GOP cannot burst the link).
            // Was 1200k + 128k ≈ 1.35 Mb/s, which did not fit and made the remote
            // preview back-pressure and fall a full backlog behind the wall.
            "-b:v",
            "500k",
            "-maxrate",
            "500k",
            "-bufsize",
            "500k",
            // INVARIANT (the #184 lag beacon relies on it): with -r 25 -g 25
            // -frag_duration 500000, every GOP is 25 frames = 1 s of media and
            // each media fragment is 0.5 s, so the child emits EXACTLY two
            // fragments per second — that is why FragmentRelay::produced_ms()
            // can read the media time produced as (fragments since Init) × 500 ms.
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
            "64k",
            // Fragmented MP4 to stdout — keyframe-aligned fragments for MSE.
            "-movflags",
            "+frag_keyframe+empty_moov+default_base_moof",
            "-frag_duration",
            "500000",
            // Flush every packet to the pipe. Without it ffmpeg's avio buffers
            // the moof/mdat fragments on a NON-seekable pipe output — the init
            // (empty_moov) flushes at header write so the browser reaches
            // readyState 1, but no media fragment arrives, so it never plays
            // (#178 box: to a FILE the same command produced 15 fragments).
            "-flush_packets",
            "1",
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
            // Release the encoder flag on EVERY exit path — including a panic in
            // supervise — so a crash never leaves it stuck claimed (#178 item 11).
            let _flag = EncoderReleaseGuard(thread_shared.clone());
            supervise(thread_shared, &ffmpeg, &encoder);
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
    // #178 item 12: bound respawns so a crash loop cannot spin ffmpeg forever.
    let mut budget = RestartBudget::default();
    let budget_base = Instant::now();
    loop {
        match run_child(&shared, ffmpeg, &encoder) {
            RunOutcome::ViewersGone => {
                info!(
                    label = shared.label(),
                    "preview-encoder: last viewer gone, child stopped"
                );
                shared.relay().reset();
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
            outcome @ (RunOutcome::ChildExitedNoInit | RunOutcome::ChildExited) => {
                // #178 item 12: a child that died while viewers are watching is
                // respawned within the restart budget; once the budget is spent
                // (or nobody is watching) close the relay so viewers see `Closed`
                // and their WS sockets close.
                if shared.has_viewer() && budget.allow(budget_base.elapsed().as_millis() as u64) {
                    warn!(
                        label = shared.label(),
                        "preview-encoder: child exited with viewers present — respawning"
                    );
                    // #184 round G: a child that DID stream leaves its viewers
                    // holding its init; the respawned child's init is only
                    // cached, never sent to them, so they would get fragments of
                    // a restarted timeline with no init and freeze (their pings
                    // are still answered, so nothing reconnects). Close the
                    // relay: each viewer's socket closes, the shim's `socketLost`
                    // rule reconnects it, and the new socket gets the new init.
                    // (A child that died with NO init leaves viewers still
                    // waiting in `wait_for_init` — they get the new one as is.)
                    if matches!(outcome, RunOutcome::ChildExited) {
                        shared.relay().close();
                    }
                    continue;
                }
                warn!(
                    label = shared.label(),
                    "preview-encoder: child exited, closing stream"
                );
                shared.relay().close();
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
        // #178 round 3: stop discarding ffmpeg's stderr — a reader thread logs
        // it (rate-limited) at WARN. The nvenc-driver / zero-audio-packet box
        // failures were single stderr lines that this makes visible in the log.
        .stderr(Stdio::piped());
    apply_windows_flags(&mut cmd);
    // ChildGuard kills + waits the child on EVERY exit path — including a panic
    // between here and teardown (std `Child::drop` does NOT kill it) — so ffmpeg
    // is never orphaned (#178 item 11).
    let mut child = ChildGuard(match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "preview-encoder: ffmpeg spawn failed");
            return RunOutcome::ChildExited;
        }
    });
    // #178 item 17: clear any stale cached init from a previous child at THIS
    // child's START, so a viewer joining now waits for the NEW init segment.
    shared.relay().reset();
    let stderr_reader =
        match spawn_stderr_reader(shared.label().to_string(), child.get().stderr.take()) {
            Ok(h) => h,
            Err(e) => {
                warn!(error = %e, "preview-encoder: failed to spawn stderr reader");
                let _ = child.get().kill();
                let _ = child.get().wait();
                return RunOutcome::ChildExited;
            }
        };
    info!(
        label = shared.label(),
        encoder, v_port, a_port, "preview-encoder: child started"
    );

    // #178 round 3: deterministic A/V alignment on our side (the box ffmpeg keeps
    // the PCM audio start_time at 0.000, so `-itsoffset` is not a lever). A shared
    // clock base + the wall-time of the FIRST video frame written let the audio
    // feeder prepend exactly the silence the video timeline is already ahead by.
    let clock_base = Instant::now();
    // Microseconds since `clock_base` of the first video write; 0 = none yet.
    let first_video_us = Arc::new(AtomicU64::new(0));

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
            let _ = child.get().kill();
            let _ = child.get().wait();
            let _ = stderr_reader.join();
            return RunOutcome::ChildExitedNoInit;
        }
    };
    let v_feeder = match spawn_video_feeder(
        shared.clone(),
        v_sock,
        shutdown.clone(),
        clock_base,
        first_video_us.clone(),
    ) {
        Ok(h) => h,
        Err(e) => {
            warn!(error = %e, "preview-encoder: failed to spawn video feeder");
            shutdown.store(true, Ordering::Relaxed);
            let _ = child.get().kill();
            let _ = child.get().wait();
            let _ = stderr_reader.join();
            return RunOutcome::ChildExited;
        }
    };
    let a_sock = match accept_with_deadline(&a_listener, Duration::from_secs(5)) {
        Some(s) => s,
        None => {
            warn!(
                label = shared.label(),
                "preview-encoder: child did not connect its audio input"
            );
            shutdown.store(true, Ordering::Relaxed);
            let _ = child.get().kill();
            let _ = child.get().wait();
            let _ = v_feeder.join();
            let _ = stderr_reader.join();
            return RunOutcome::ChildExitedNoInit;
        }
    };
    let a_feeder = match spawn_audio_feeder(
        shared.clone(),
        a_sock,
        shutdown.clone(),
        clock_base,
        first_video_us.clone(),
    ) {
        Ok(h) => h,
        Err(e) => {
            warn!(error = %e, "preview-encoder: failed to spawn audio feeder");
            shutdown.store(true, Ordering::Relaxed);
            let _ = child.get().kill();
            let _ = child.get().wait();
            let _ = v_feeder.join();
            let _ = stderr_reader.join();
            return RunOutcome::ChildExited;
        }
    };
    // Per-child "did THIS child produce any stdout" — the relay's cached init
    // segment persists across children, so it cannot be the failed-child signal:
    // a broken hardware child (nvenc that never opens its encoder) produces ZERO
    // bytes yet the relay still holds a prior working child's init, which made
    // the old `relay().init().is_some()` check misread it as a clean exit and
    // skip the libx264 fallback (#178 box).
    let produced = Arc::new(AtomicBool::new(false));
    let reader =
        match spawn_stdout_reader(shared.clone(), child.get().stdout.take(), produced.clone()) {
            Ok(h) => h,
            Err(e) => {
                warn!(error = %e, "preview-encoder: failed to spawn stdout reader");
                shutdown.store(true, Ordering::Relaxed);
                let _ = child.get().kill();
                let _ = child.get().wait();
                let _ = v_feeder.join();
                let _ = a_feeder.join();
                let _ = stderr_reader.join();
                return RunOutcome::ChildExited;
            }
        };

    let outcome = monitor_loop(shared, child.get(), &produced);

    // Tear down: stop feeders, kill child, join everything (incl. the stderr
    // reader, which ends at the child's stderr EOF once the child is gone).
    shutdown.store(true, Ordering::Relaxed);
    let _ = child.get().kill();
    let _ = child.get().wait();
    let _ = v_feeder.join();
    let _ = a_feeder.join();
    let _ = reader.join();
    let _ = stderr_reader.join();
    // #178 item 17: clear the cached init at child STOP so it is never served to
    // a new child's late joiner.
    shared.relay().reset();
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
/// write error (child gone). Drains any STALE queued frames on start (a previous
/// viewer's backlog would otherwise front-run the live edge) and stamps the
/// wall-time of the first successful write into `first_video_us` (0 = none yet)
/// so the audio feeder can measure how far the video timeline is ahead (#178 r3).
#[cfg_attr(test, mutants::skip)]
fn spawn_video_feeder(
    shared: Arc<StreamShared>,
    mut sock: TcpStream,
    shutdown: Arc<AtomicBool>,
    clock_base: Instant,
    first_video_us: Arc<AtomicU64>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("preview-vfeed".into())
        .spawn(move || {
            let rx = shared.video_receiver();
            // Drop any frames queued before this child connected.
            while rx.try_recv().is_ok() {}
            while !shutdown.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(frame) => {
                        if sock.write_all(&frame).is_err() {
                            break;
                        }
                        // Stamp the first successful video write (`.max(1)` so a
                        // genuine sub-µs elapse never reads back as "none yet").
                        if first_video_us.load(Ordering::Relaxed) == 0 {
                            let us = clock_base.elapsed().as_micros() as u64;
                            first_video_us.store(us.max(1), Ordering::Relaxed);
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
}

/// How often the audio feeder logs its wall-clock alignment (#184 round G2).
const AFEED_LOG_EVERY: Duration = Duration::from_secs(10);

/// Feed tapped interleaved-f32 audio to the child's audio socket (little-endian
/// f32 bytes) until shutdown or a write error. On start it DRAINS any stale
/// queued blocks, then PREPENDS silence equal to how far the video timeline is
/// already ahead (`audio_preroll_samples(connect_gap_ms, lead_ms)`) so the PCM
/// sample-count timeline starts where the video wall-clock timeline started
/// (#178 round 3 — replaces the box-unreliable `-itsoffset`).
///
/// From there on (#184 round G2) the written audio is kept on the WALL CLOCK in
/// both directions: the target is `wall_frames = preroll + elapsed since the
/// feeder started × 48 000`, and every block goes through [`align_block`] (pad
/// silence up to the wall when late, trim the OLDEST frames of a burst that
/// would run > 300 ms ahead), every 200 ms receive timeout through
/// [`align_timeout`] (silence up to the wall — ffmpeg is never starved of audio,
/// so it never stops emitting fragments while the decode seam is quiet). Logs
/// `preview-afeed: ahead_ms padded_ms skipped_ms` at INFO every 10 s.
#[cfg_attr(test, mutants::skip)]
fn spawn_audio_feeder(
    shared: Arc<StreamShared>,
    mut sock: TcpStream,
    shutdown: Arc<AtomicBool>,
    clock_base: Instant,
    first_video_us: Arc<AtomicU64>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("preview-afeed".into())
        .spawn(move || {
            let rx = shared.audio_receiver();
            // Drop any audio blocks queued before this child connected.
            while rx.try_recv().is_ok() {}
            // Silence preroll: how long the video feeder has been running before
            // this audio input connected, plus the decode-seam lead. 0 if no
            // video frame has been written yet (nothing to align against).
            let v_us = first_video_us.load(Ordering::Relaxed);
            let gap_ms = if v_us == 0 {
                0
            } else {
                (clock_base.elapsed().as_micros() as u64).saturating_sub(v_us) / 1000
            };
            let preroll = audio_preroll_samples(gap_ms, shared.lead_ms());
            // The wall target starts where the preroll ends: right after it is
            // written, written == wall (the round-3 start alignment is kept).
            let start = Instant::now();
            let base_frames = (preroll / 2) as u64; // interleaved stereo
            let wall_frames = || {
                base_frames
                    + start.elapsed().as_micros() as u64 * PREVIEW_AUDIO_FRAMES_PER_MS / 1000
            };
            if preroll > 0 && !write_silence(&mut sock, preroll) {
                return;
            }
            let mut written_frames: u64 = base_frames;
            let mut padded_frames: u64 = 0;
            let mut skipped_frames: u64 = 0;
            let mut last_log = Instant::now();
            let mut bytes: Vec<u8> = Vec::new();
            while !shutdown.load(Ordering::Relaxed) {
                let (pad, block) = match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(block) => {
                        let a = align_block(wall_frames(), written_frames, block.len() / 2);
                        (a.pad_frames, Some((block, a.skip_frames)))
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        (align_timeout(wall_frames(), written_frames), None)
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                };
                if pad > 0 {
                    if !write_silence(&mut sock, pad * 2) {
                        break;
                    }
                    written_frames += pad as u64;
                    padded_frames += pad as u64;
                }
                if let Some((block, skip)) = block {
                    // Drop the block's OLDEST `skip` frames (a late burst is
                    // trimmed, never appended behind silence); whole frames only.
                    let tail = &block[block_tail_range(skip, block.len())];
                    if !tail.is_empty() {
                        bytes.clear();
                        bytes.reserve(tail.len() * 4);
                        for s in tail {
                            bytes.extend_from_slice(&s.to_le_bytes());
                        }
                        if sock.write_all(&bytes).is_err() {
                            break;
                        }
                    }
                    written_frames += (tail.len() / 2) as u64;
                    skipped_frames += skip as u64;
                }
                if last_log.elapsed() >= AFEED_LOG_EVERY {
                    let ahead = written_frames as i64 - wall_frames() as i64;
                    let per_ms = PREVIEW_AUDIO_FRAMES_PER_MS as i64;
                    info!(
                        stream = %shared.label(),
                        "preview-afeed: ahead_ms={} padded_ms={} skipped_ms={}",
                        ahead / per_ms,
                        padded_frames / PREVIEW_AUDIO_FRAMES_PER_MS,
                        skipped_frames / PREVIEW_AUDIO_FRAMES_PER_MS
                    );
                    last_log = Instant::now();
                }
            }
        })
}

/// Write `samples` interleaved-f32 zero samples to the child's audio socket in
/// chunks of at most 32 KB (8192 f32). Returns `false` on a write error (the
/// child is gone), so the caller aborts the feeder.
#[cfg_attr(test, mutants::skip)]
fn write_silence(sock: &mut TcpStream, samples: usize) -> bool {
    const CHUNK_SAMPLES: usize = 8 * 1024; // 32 KB of f32
    let zeros = vec![0u8; CHUNK_SAMPLES * 4];
    let mut remaining = samples;
    while remaining > 0 {
        let n = remaining.min(CHUNK_SAMPLES);
        if sock.write_all(&zeros[..n * 4]).is_err() {
            return false;
        }
        remaining -= n;
    }
    true
}

/// Read the child's stderr line-by-line and log each at WARN with the stream
/// label, RATE-LIMITED to the first 20 lines per child + a final "N more
/// suppressed" — args keep `-loglevel error`, so these are genuine errors worth
/// surfacing (the nvenc-driver / zero-audio-packet box failures were single
/// stderr lines). Ends at the child's stderr EOF (the child is gone).
#[cfg_attr(test, mutants::skip)]
fn spawn_stderr_reader(
    label: String,
    stderr: Option<std::process::ChildStderr>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("preview-err".into())
        .spawn(move || {
            let stderr = match stderr {
                Some(s) => s,
                None => return,
            };
            const MAX_LINES: usize = 20;
            let mut shown = 0usize;
            let mut suppressed = 0usize;
            for line in BufReader::new(stderr).lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if line.trim().is_empty() {
                    continue;
                }
                if shown < MAX_LINES {
                    warn!(label = %label, "preview-encoder ffmpeg: {line}");
                    shown += 1;
                } else {
                    suppressed += 1;
                }
            }
            if suppressed > 0 {
                warn!(
                    label = %label,
                    suppressed,
                    "preview-encoder ffmpeg: further stderr lines suppressed"
                );
            }
        })
}

/// Read the child's fragmented-MP4 stdout, split it into init + fragments, and
/// feed the relay.
#[cfg_attr(test, mutants::skip)]
fn spawn_stdout_reader(
    shared: Arc<StreamShared>,
    stdout: Option<std::process::ChildStdout>,
    produced: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
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
