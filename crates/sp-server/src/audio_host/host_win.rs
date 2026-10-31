//! #233: the ASIO host process's Windows glue (`sp-audio-host.exe`; out of
//! the mutation gate, `.cargo/mutants.toml`: pipe, priority and thread
//! calls no Linux runner can make; the frame loop is the Linux-tested
//! `host.rs`).
//!
//! The host spawns NO process (a scanned rule, `no_spawn_tests.rs`): that
//! is the point of it. It writes its crash record and its log into the data
//! dir (`launch::host_data_dir`) — the log also goes to SongPlayer over the
//! pipe (`log_forward.rs`), and stays in the file while the pipe is down —,
//! runs at HIGH priority (like SongPlayer, #203) with every output's worker
//! on its own thread as before (`asio_win::spawn_asio_thread`, the driver's
//! callback thread its own) on SongPlayer's timeline (`remote_clock.rs`: the
//! host makes no wall, so it never relabels on its own), opens SongPlayer's
//! pipe and serves it. Its log filter is SongPlayer's (`info,sp_server=debug`,
//! or `RUST_LOG`). At the pipe's end every output is discarded and its
//! worker given up to 2 s to release its driver before the process exits.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::audio_host::host::{HostEnd, HostEnv, REPORT_QUEUE, STATUS_EVERY, serve};
use crate::audio_host::launch::{HOST_LOG, HOST_PANIC_LOG, host_data_dir};
use crate::audio_host::log_forward::PipeLogs;
use crate::audio_host::mono::mono_100ns;
use crate::audio_host::protocol::Report;
use crate::audio_host::remote_clock::RemoteClock;
use crate::log_rotate::{ROTATE_AT_BYTES, RotatingFile};
use crate::playback::asio_out::AsioOut;

/// How long the host keeps trying to open a busy pipe.
const OPEN_WAIT: Duration = Duration::from_secs(5);
/// How long the workers get to release their drivers at the end.
const RELEASE_WAIT: Duration = Duration::from_secs(2);

/// The host process (module doc); the exit code.
pub fn run(pipe: &str) -> i32 {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = host_data_dir(std::env::var_os("ProgramData"), &exe_dir);
    let _ = std::fs::create_dir_all(&dir);
    crate::install_panic_hook(dir.join(HOST_PANIC_LOG));
    let (report_tx, reports) = mpsc::channel(REPORT_QUEUE);
    init_logging(&dir.join(HOST_LOG), report_tx.clone());
    info!(
        version = env!("CARGO_PKG_VERSION"),
        pipe, "sp-audio-host started - it hosts SongPlayer's ASIO outputs and spawns no process"
    );
    set_high_priority();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(%e, "sp-audio-host: no runtime");
            return 1;
        }
    };
    let served = runtime.block_on(async {
        let pipe = open(pipe).await?;
        let (reader, writer) = tokio::io::split(pipe);
        Ok::<_, String>(serve(reader, writer, env(), reports, report_tx).await)
    });
    match served {
        Ok((end, outs)) => {
            wait_released(&outs);
            match end {
                HostEnd::Eof => {
                    info!("sp-audio-host: SongPlayer closed the pipe - exiting");
                    0
                }
                HostEnd::Failed(why) => {
                    error!(%why, "sp-audio-host: the session failed - exiting");
                    1
                }
            }
        }
        Err(why) => {
            error!(%why, "sp-audio-host: could not open SongPlayer's pipe");
            1
        }
    }
}

/// The real worker starter and the box's counter.
fn env() -> HostEnv {
    HostEnv {
        start: Box::new(|out: Arc<AsioOut>, clock: RemoteClock| {
            let id = out.id().to_string();
            crate::playback::asio_win::spawn_asio_thread(out, id, clock);
        }),
        mono: Arc::new(mono_100ns),
        status_every: STATUS_EVERY,
    }
}

/// Open SongPlayer's pipe (retried while it is busy, bounded).
async fn open(pipe: &str) -> Result<NamedPipeClient, String> {
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
    let deadline = Instant::now() + OPEN_WAIT;
    loop {
        match ClientOptions::new().open(pipe) {
            Ok(client) => return Ok(client),
            Err(e)
                if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(format!("opening {pipe} failed: {e}")),
        }
    }
}

/// The host's log: its file (kept while the pipe is down; #261: rotated to
/// `.1` at 10 MiB, at the start and during a run) and the pipe.
fn init_logging(path: &Path, reports: mpsc::Sender<Report>) {
    let file = RotatingFile::open(path, ROTATE_AT_BYTES);
    let file_layer = file
        .ok()
        .map(|f| fmt::layer().with_writer(Mutex::new(f)).with_ansi(false));
    // SongPlayer re-logs a pipe line at its level: the line carries none.
    let pipe_layer = fmt::layer()
        .with_writer(PipeLogs::new(reports))
        .with_ansi(false)
        .with_level(false)
        .without_time();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sp_server=debug"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(pipe_layer)
        .try_init();
}

/// The host runs at HIGH priority, like SongPlayer (#203).
fn set_high_priority() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, HIGH_PRIORITY_CLASS, SetPriorityClass,
    };
    // SAFETY: the current-process pseudo-handle; only this process's class.
    let ok = unsafe { SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS) };
    if ok == 0 {
        warn!("sp-audio-host: could not set HIGH_PRIORITY_CLASS");
    } else {
        info!("sp-audio-host: priority class HIGH");
    }
}

/// Give the discarded outputs' workers up to [`RELEASE_WAIT`] to release
/// their drivers (each worker clears its running flag when done).
fn wait_released(outs: &[Arc<AsioOut>]) {
    let deadline = Instant::now() + RELEASE_WAIT;
    while outs.iter().any(|o| o.is_running()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let left = outs.iter().filter(|o| o.is_running()).count();
    if left > 0 {
        warn!(
            left,
            "sp-audio-host: exiting while a driver is still being released"
        );
    }
}
