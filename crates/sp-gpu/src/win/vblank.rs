//! The thread that measures the wall output's refresh (#223 follow-up,
//! 9.10.2026): `IDXGIOutput::WaitForVBlank` in a loop on the output
//! `pick_output` chooses on the adapter `pick_adapter` chooses (the
//! compositor's), every wake-up counted into a `VblankFit`, the fitted grid
//! published for the `program-max` thread. Every decision is
//! `crate::vblank`'s, tested on Linux; this file only calls DXGI and Win32.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::{info, warn};
use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_NOT_FOUND, IDXGIOutput};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};

use super::device::list;
use super::failed;
use crate::adapter::{adapter_name, pick_adapter};
use crate::error::GpuError;
use crate::vblank::{OutputInfo, Seen, VblankFit, VblankGrid, grid_is_fresh, pick_output};

/// The wait after a failed `WaitForVBlank` before the next one.
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(1);

/// The sleep after an early wake-up (a wait that returned at once), so a
/// wait that keeps returning at once never spins a core.
const AFTER_EARLY: Duration = Duration::from_millis(1);

/// What the thread last measured.
#[derive(Default)]
struct Published {
    grid: Option<VblankGrid>,
    /// When it last counted a refresh.
    seen: Option<Instant>,
}

struct Shared {
    stop: AtomicBool,
    published: Mutex<Published>,
}

/// The wall output's refresh, measured on its own thread
/// (`program-max-vblank`, time-critical priority) until dropped.
pub struct VblankTracker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    output: OutputInfo,
}

impl VblankTracker {
    /// Start the thread on the output `pick_output` chooses; an error when
    /// there is no hardware adapter, no attached output or no thread.
    pub fn start() -> Result<Self, GpuError> {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            published: Mutex::new(Published::default()),
        });
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("program-max-vblank".into())
            .spawn(move || run(&thread_shared, &ready_tx))
            .map_err(|e| GpuError::Thread(e.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok(output)) => Ok(Self {
                shared,
                thread: Some(thread),
                output,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(GpuError::Thread(
                    "it ended before it opened an output".to_string(),
                ))
            }
        }
    }

    /// The output it measures.
    pub fn output(&self) -> &OutputInfo {
        &self.output
    }

    /// The refresh grid at `now`: `None` before the fit has enough
    /// refreshes, or when none was counted for `VBLANK_STALE`.
    pub fn grid(&self, now: Instant) -> Option<VblankGrid> {
        let published = self
            .shared
            .published
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let seen = published.seen?;
        if grid_is_fresh(seen, now) {
            published.grid
        } else {
            None
        }
    }
}

impl Drop for VblankTracker {
    /// Stop the thread (it ends within one refresh, or one error retry).
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The thread: open the output, report it on `ready`, then wait for each
/// refresh and publish the fitted grid until stopped.
fn run(shared: &Shared, ready: &mpsc::Sender<Result<OutputInfo, GpuError>>) {
    if let Err(e) = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) }
    {
        warn!(error = %e, "program max: the vblank thread runs at normal priority");
    }
    let (output, info) = match open_output() {
        Ok(opened) => opened,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(info.clone()));
    let label = info.label();
    let mut fit = VblankFit::default();
    let mut failing = false;
    let mut measured = false;
    while !shared.stop.load(Ordering::Relaxed) {
        if let Err(e) = unsafe { output.WaitForVBlank() } {
            if !failing {
                warn!(output = %label, error = %e, "program max: WaitForVBlank failed");
            }
            failing = true;
            std::thread::sleep(RETRY_AFTER_ERROR);
            continue;
        }
        if failing {
            info!(output = %label, "program max: WaitForVBlank works again");
            failing = false;
        }
        let now = Instant::now();
        if fit.observe(now) == Seen::Early {
            std::thread::sleep(AFTER_EARLY);
            continue;
        }
        let grid = fit.grid();
        if let Some(grid) = grid
            && !measured
        {
            let hz = 1e9 / grid.period.as_nanos() as f64;
            info!(output = %label, hz, "program max: the output's refresh is measured");
            measured = true;
        }
        let mut published = shared.published.lock().unwrap_or_else(|p| p.into_inner());
        published.grid = grid;
        published.seen = Some(now);
    }
    info!(
        output = %label,
        missed = fit.missed(),
        early = fit.early(),
        "program max: the vblank thread stopped"
    );
}

/// The output `pick_output` chooses on the adapter `pick_adapter` chooses
/// (the compositor's rule), with what it read; every output is logged.
fn open_output() -> Result<(IDXGIOutput, OutputInfo), GpuError> {
    let mut adapters = list()?;
    let infos: Vec<_> = adapters.iter().map(|(_, info)| info.clone()).collect();
    let picked = pick_adapter(&infos).ok_or(GpuError::NoAdapter)?;
    let (adapter, adapter_info) = adapters.swap_remove(picked);
    let mut outputs = Vec::new();
    for index in 0u32.. {
        let output = match unsafe { adapter.EnumOutputs(index) } {
            Ok(output) => output,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(failed("EnumOutputs", &e)),
        };
        let desc = unsafe { output.GetDesc() }.map_err(|e| failed("IDXGIOutput::GetDesc", &e))?;
        let rect = desc.DesktopCoordinates;
        let info = OutputInfo {
            name: adapter_name(&desc.DeviceName),
            left: rect.left,
            top: rect.top,
            width: u32::try_from(rect.right - rect.left).unwrap_or(0),
            height: u32::try_from(rect.bottom - rect.top).unwrap_or(0),
            attached: desc.AttachedToDesktop.as_bool(),
        };
        info!(
            index,
            output = %info.label(),
            left = info.left,
            top = info.top,
            attached = info.attached,
            adapter = %adapter_info.name,
            "program max: display output"
        );
        outputs.push((output, info));
    }
    let infos: Vec<_> = outputs.iter().map(|(_, info)| info.clone()).collect();
    let picked = pick_output(&infos).ok_or(GpuError::NoOutput)?;
    Ok(outputs.swap_remove(picked))
}
