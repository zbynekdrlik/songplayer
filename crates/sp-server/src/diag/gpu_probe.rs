//! #279 P0: the GPU capability probe behind `POST /api/v1/diag/decode-bench
//! {"probe": true}` (`api::diag`). It asks the box's own GPU, on two devices
//! of the adapter the compositor takes (`sp_gpu::probe_gpu`), whether the
//! GPU-resident video path's device model holds (shared NV12 by NT handle
//! and keyed mutex, its content read back across, its plane views, a
//! `DO_NOT_WAIT` readback), and names
//! every adapter DXGI lists and the adapter's video memory. It is read-only:
//! nothing of the program path is touched, its devices live only for the
//! run. It takes the decode bench's slot (one bench at a time, 409).
//!
//! This file is the cross-platform part, Linux-tested: the request's two
//! shapes ([`ask`]), the answer ([`ProbeAnswer`]) and the run on a blocking
//! thread ([`run`], the probe a parameter).

use serde::Serialize;
use sp_gpu::probe::{GpuProbe, ProbeError, ProbeReport, Verdict, Vram};
use sp_gpu::{AdapterInfo, GpuError};

use super::decode_bench::{BenchMode, BenchSlot};

/// What a `decode-bench` request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchAsk<'a> {
    /// `{"probe": true}`: the GPU capability probe.
    Probe,
    /// `{"file", "seconds", "hw"?, "resident"?}`: a decode run.
    Decode {
        file: &'a str,
        seconds: u64,
        mode: BenchMode,
    },
}

/// The request's shape, or why it has none: a probe names no file, no
/// seconds, no `hw` and no `resident`; a decode run names a file and its
/// seconds, and at most one of `hw` / `resident` (#279 P3a: two modes).
pub fn ask(
    probe: bool,
    file: Option<&str>,
    seconds: Option<u64>,
    hw: bool,
    resident: bool,
) -> Result<BenchAsk<'_>, &'static str> {
    if probe {
        if file.is_some() || seconds.is_some() || hw || resident {
            return Err("a probe takes no file, seconds, hw or resident");
        }
        return Ok(BenchAsk::Probe);
    }
    if hw && resident {
        return Err("hw and resident are two modes: send one of them");
    }
    match (file, seconds) {
        (Some(file), Some(seconds)) => {
            let mode = BenchMode::of(hw, resident);
            Ok(BenchAsk::Decode {
                file,
                seconds,
                mode,
            })
        }
        _ => Err("file and seconds are required (or \"probe\": true)"),
    }
}

/// One DXGI adapter as the answer names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdapterJson {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    /// Dedicated video memory, whole MiB.
    pub vram_mb: u64,
    /// A software rasterizer (`AdapterInfo::is_software`).
    pub software: bool,
}

impl AdapterJson {
    pub fn of(info: &AdapterInfo) -> Self {
        Self {
            name: info.name.clone(),
            vendor_id: info.vendor_id,
            device_id: info.device_id,
            vram_mb: info.dedicated_video_memory / (1024 * 1024),
            software: info.is_software(),
        }
    }
}

/// The answer: the verdict, every step, the adapter both devices ran on,
/// every adapter DXGI lists, and that adapter's local video memory (or why
/// it could not be read).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProbeAnswer {
    pub verdict: Verdict,
    pub probe: ProbeReport,
    pub adapter: AdapterJson,
    pub adapters: Vec<AdapterJson>,
    pub vram: Option<Vram>,
    pub vram_error: Option<ProbeError>,
}

impl ProbeAnswer {
    pub fn of(probe: GpuProbe) -> Self {
        Self {
            verdict: probe.report.verdict(),
            adapter: AdapterJson::of(&probe.adapter),
            adapters: probe.adapters.iter().map(AdapterJson::of).collect(),
            vram: probe.vram.ok(),
            vram_error: probe.vram.err(),
            probe: probe.report,
        }
    }

    /// The text of the run's log line: the verdict, the adapter and each
    /// step's answer.
    pub fn summary(&self) -> String {
        let verdict = match &self.verdict {
            Verdict::GpuResidentOk => "gpu_resident_ok".to_string(),
            Verdict::NeedsFallback { step, .. } => format!("needs_fallback at {step}"),
        };
        let steps: Vec<String> = self
            .probe
            .steps()
            .iter()
            .map(|(name, step)| match step {
                Ok(()) => format!("{name}=ok"),
                Err(error) => format!("{name}=[{error}]"),
            })
            .collect();
        format!(
            "verdict={verdict} adapter={:?} {}",
            self.adapter.name,
            steps.join(" ")
        )
    }
}

/// What the handler gets back from one probe.
#[derive(Debug)]
pub enum ProbeOutcome {
    /// The probe ran (200).
    Answer(Box<ProbeAnswer>),
    /// No Direct3D in this build (501).
    Unsupported,
    /// No adapter, a device that could not be made, or the thread failed
    /// (500).
    Failed(String),
}

/// Run `probe` on a blocking thread that holds `slot` and drops it before
/// the answer is back, also when `probe` panics (the panic hook has logged
/// it; it comes back as `Failed`).
pub async fn run<P>(slot: BenchSlot, probe: P) -> ProbeOutcome
where
    P: FnOnce() -> Result<GpuProbe, GpuError> + Send + 'static,
{
    let joined = tokio::task::spawn_blocking(move || {
        let answer = probe();
        drop(slot);
        answer
    })
    .await;
    match joined {
        Ok(Ok(probe)) => ProbeOutcome::Answer(Box::new(ProbeAnswer::of(probe))),
        Ok(Err(GpuError::Unsupported)) => ProbeOutcome::Unsupported,
        Ok(Err(error)) => ProbeOutcome::Failed(format!("the GPU probe failed: {error}")),
        Err(error) => ProbeOutcome::Failed(format!("the GPU probe's thread failed: {error}")),
    }
}

#[cfg(test)]
#[path = "gpu_probe_tests.rs"]
mod tests;
