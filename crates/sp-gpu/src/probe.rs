//! #279 P0: what the GPU-resident video path needs from the device model,
//! asked of the GPU itself (pure: the report, the verdict and every rule
//! that reads an answer; the Direct3D calls are `win/probe.rs`).
//!
//! The design of epic #279 (its plan §4.2–§4.4; the P0 design comment
//! 6215400662) decodes each playlist on its own producer device into shared
//! NV12 textures and composes them on ONE compositor device. That rests on
//! five capabilities, each probed on its own on two devices of one adapter
//! (device A = a producer, device B = the compositor):
//!
//! 1. `extended_resource_sharing`: `CheckFeatureSupport(D3D11_OPTIONS)`
//!    says `ExtendedResourceSharing` on both devices (shared Texture2D of
//!    more formats, NV12 among them; Chromium checks the same flag);
//! 2. `nv12_shared_nthandle`: an NV12 texture made on A with
//!    `SHARED_NTHANDLE | SHARED_KEYEDMUTEX`, its NT handle
//!    (`IDXGIResource1::CreateSharedHandle`) opened on B
//!    (`ID3D11Device1::OpenSharedResource1`);
//! 3. `nv12_keyed_mutex_roundtrip`: A acquires key 0 with a bounded wait,
//!    writes the texture ([`probe_picture`]), releases key 1; B acquires key
//!    1 with a 0 ms timeout (the compositor never blocks), copies the
//!    texture to a staging copy and reads it back, releases key 1: the
//!    bytes must be the ones A wrote ([`content_step`]), so a driver that
//!    opens the handle but shares no content, or whose mutex does not order
//!    the copy, fails;
//! 4. `nv12_srv_r8_r8g8`: R8 (luma) and R8G8 (chroma) shader views on the
//!    NV12 texture B opened, the views the compositor samples;
//! 5. `uyvy_target_and_staging_do_not_wait`: on B, an R8G8B8A8 960×1080
//!    render target (the UYVY pass of P1a), cleared, copied to a staging
//!    texture, the copy waited for, then mapped with
//!    `D3D11_MAP_FLAG_DO_NOT_WAIT`: it must map (never
//!    `DXGI_ERROR_WAS_STILL_DRAWING`) and read the cleared colour.
//!
//! Every step answers `Ok` or a [`ProbeError`] naming its call and HRESULT;
//! none is skipped. [`ProbeReport::verdict`] reads them: every one `Ok` =
//! [`Verdict::GpuResidentOk`], else [`Verdict::NeedsFallback`] with the
//! first failed step in the report's order (the design's R1 fallback).

use serde::{Serialize, Serializer};

use crate::adapter::AdapterInfo;
use crate::error::GpuError;

/// `S_OK`.
pub const S_OK: u32 = 0;
/// `WAIT_ABANDONED`: a keyed mutex whose holder released no key before it
/// went away (the surface must be made again). A success code, so the
/// `windows` crate's `AcquireSync` would read it as `Ok`.
pub const WAIT_ABANDONED: u32 = 0x80;
/// `WAIT_TIMEOUT`: a keyed mutex not released with the key in time. A
/// success code too.
pub const WAIT_TIMEOUT: u32 = 0x102;
/// `DXGI_ERROR_WAS_STILL_DRAWING`: a `DO_NOT_WAIT` map of a resource the
/// GPU still writes.
pub const DXGI_ERROR_WAS_STILL_DRAWING: u32 = 0x887A_000A;
/// `DXGI_ERROR_UNSUPPORTED`: the driver does not do it.
pub const DXGI_ERROR_UNSUPPORTED: u32 = 0x887A_0004;
/// `E_INVALIDARG`: Direct3D refused a description (e.g. a format it cannot
/// share or view that way).
pub const E_INVALIDARG: u32 = 0x8007_0057;

/// The probe's NV12 texture: a small 16:9 picture of even sides (its size
/// does not change what the driver allows).
pub const PROBE_NV12_WIDTH: u32 = 256;
pub const PROBE_NV12_HEIGHT: u32 = 144;
/// The UYVY pass's render target (#279 P1a): 1920 / 2 pixel pairs
/// per row, one R8G8B8A8 texel per pair, 1080 rows.
pub const PROBE_UYVY_WIDTH: u32 = 960;
pub const PROBE_UYVY_HEIGHT: u32 = 1080;
/// The colour the probe clears the UYVY target to, and its bytes in
/// R8G8B8A8_UNORM: whole channels, so no rounding can move them.
pub const PROBE_CLEAR: [f32; 4] = [1.0, 0.0, 1.0, 1.0];
pub const PROBE_CLEAR_BYTES: [u8; 4] = [255, 0, 255, 255];
/// The keyed-mutex protocol (design §4.3): the producer writes under key 0
/// and hands the picture over as key 1.
pub const KEY_PRODUCER: u64 = 0;
pub const KEY_CONSUMER: u64 = 1;
/// The producer's acquire may wait this long (a fresh texture's key 0 is
/// free at once); the compositor's never waits.
pub const PRODUCER_ACQUIRE_MS: u32 = 1_000;
pub const CONSUMER_ACQUIRE_MS: u32 = 0;

/// Why one probe step failed: the call, its HRESULT (0 when the call
/// succeeded but its answer was "no") and a fixed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProbeError {
    pub call: &'static str,
    #[serde(serialize_with = "hex")]
    pub hresult: u32,
    pub detail: &'static str,
}

impl ProbeError {
    /// `call` failed with `hresult` (its known name in `detail`).
    pub fn failed(call: &'static str, hresult: u32) -> Self {
        Self {
            call,
            hresult,
            detail: hresult_detail(hresult),
        }
    }

    /// `call` succeeded, but its answer is no: `detail` says which.
    pub fn refused(call: &'static str, detail: &'static str) -> Self {
        Self {
            call,
            hresult: S_OK,
            detail,
        }
    }

    /// A crate error met inside a step (a shared helper's): its call and
    /// HRESULT when it has them.
    pub fn from_gpu(error: &GpuError) -> Self {
        match *error {
            GpuError::Api { call, hresult } | GpuError::DeviceLost { call, hresult } => {
                Self::failed(call, hresult)
            }
            GpuError::NoObject { call } => Self::refused(call, "gave no object"),
            GpuError::Timeout(_) => Self::refused("the GPU wait", "not done within the bound"),
            _ => Self::refused("the probe", "failed outside a Direct3D call"),
        }
    }
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} (HRESULT {:#010x})",
            self.call, self.detail, self.hresult
        )
    }
}

/// The name of an HRESULT the probe meets, or "failed".
pub fn hresult_detail(hresult: u32) -> &'static str {
    match hresult {
        DXGI_ERROR_WAS_STILL_DRAWING => "still drawing (DO_NOT_WAIT)",
        DXGI_ERROR_UNSUPPORTED => "unsupported by the driver",
        E_INVALIDARG => "invalid argument",
        WAIT_TIMEOUT => "timed out",
        WAIT_ABANDONED => "abandoned",
        _ => "failed",
    }
}

/// An HRESULT as the SDK writes it: `0x887a000a`.
fn hex<S: Serializer>(hresult: &u32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("{hresult:#010x}"))
}

/// One probe step's answer.
pub type ProbeStep = Result<(), ProbeError>;

/// A step as JSON: `{"ok": true}`, or `{"ok": false, "error": {call,
/// hresult, detail}}`.
fn step<S: Serializer>(step: &ProbeStep, s: S) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Step<'a> {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'a ProbeError>,
    }
    Step {
        ok: step.is_ok(),
        error: step.as_ref().err(),
    }
    .serialize(s)
}

/// The five steps' answers (module doc), in the order the verdict reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeReport {
    #[serde(serialize_with = "step")]
    pub extended_resource_sharing: ProbeStep,
    #[serde(serialize_with = "step")]
    pub nv12_shared_nthandle: ProbeStep,
    #[serde(serialize_with = "step")]
    pub nv12_keyed_mutex_roundtrip: ProbeStep,
    #[serde(serialize_with = "step")]
    pub nv12_srv_r8_r8g8: ProbeStep,
    #[serde(serialize_with = "step")]
    pub uyvy_target_and_staging_do_not_wait: ProbeStep,
}

/// What the probe says of the GPU-resident path on this adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verdict {
    /// Every step passed: the device model of the design holds.
    GpuResidentOk,
    /// `step` failed (the first in the report's order): the design's R1
    /// fallback applies.
    NeedsFallback {
        step: &'static str,
        error: ProbeError,
    },
}

impl ProbeReport {
    /// Each step with its JSON name, in the report's order.
    pub fn steps(&self) -> [(&'static str, &ProbeStep); 5] {
        [
            ("extended_resource_sharing", &self.extended_resource_sharing),
            ("nv12_shared_nthandle", &self.nv12_shared_nthandle),
            (
                "nv12_keyed_mutex_roundtrip",
                &self.nv12_keyed_mutex_roundtrip,
            ),
            ("nv12_srv_r8_r8g8", &self.nv12_srv_r8_r8g8),
            (
                "uyvy_target_and_staging_do_not_wait",
                &self.uyvy_target_and_staging_do_not_wait,
            ),
        ]
    }

    /// [`Verdict::GpuResidentOk`] when every step passed, else the first
    /// failed one.
    pub fn verdict(&self) -> Verdict {
        for (step, answer) in self.steps() {
            if let Err(error) = answer {
                return Verdict::NeedsFallback {
                    step,
                    error: *error,
                };
            }
        }
        Verdict::GpuResidentOk
    }
}

/// The probe's NV12 picture as A writes it: [`PROBE_NV12_WIDTH`] ×
/// [`PROBE_NV12_HEIGHT`], rows packed, the UV plane right after the luma;
/// byte `i` is `i mod 251`, so a stale, shifted or half-written copy reads
/// differently.
pub fn probe_picture() -> Vec<u8> {
    let len = (PROBE_NV12_WIDTH * PROBE_NV12_HEIGHT * 3 / 2) as usize;
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// What B read back of the shared texture (its NV12 rows packed; `None`
/// when the mapping gave no whole picture) against [`probe_picture`].
pub fn content_step(call: &'static str, got: Option<Vec<u8>>) -> ProbeStep {
    match got {
        None => Err(ProbeError::refused(call, "mapped no whole picture")),
        Some(got) if got != probe_picture() => {
            Err(ProbeError::refused(call, "read other bytes than A wrote"))
        }
        Some(_) => Ok(()),
    }
}

/// A feature `call` reports: `supported`, or refused "reports FALSE".
pub fn feature_step(call: &'static str, supported: bool) -> ProbeStep {
    if supported {
        Ok(())
    } else {
        Err(ProbeError::refused(call, "reports FALSE"))
    }
}

/// A keyed mutex's `AcquireSync` answer, its raw HRESULT: only `S_OK`
/// holds the key. `WAIT_TIMEOUT` and `WAIT_ABANDONED` are SUCCESS codes,
/// so they must be read here, never through the `windows` crate's
/// `Result` (it would call them `Ok`).
pub fn acquire_step(call: &'static str, hresult: u32) -> ProbeStep {
    if hresult == S_OK {
        Ok(())
    } else {
        Err(ProbeError::failed(call, hresult))
    }
}

/// The first texel of a mapped readback (`None` when the map gave no
/// pointer) against the colour the target was cleared to.
pub fn pixel_step(call: &'static str, got: Option<[u8; 4]>, want: [u8; 4]) -> ProbeStep {
    match got {
        None => Err(ProbeError::refused(call, "mapped no memory")),
        Some(got) if got != want => Err(ProbeError::refused(
            call,
            "read another colour than the clear",
        )),
        Some(_) => Ok(()),
    }
}

/// The adapter's local video memory (`IDXGIAdapter3::QueryVideoMemoryInfo`,
/// `DXGI_MEMORY_SEGMENT_GROUP_LOCAL`): what this process may use
/// (`Budget`) and uses (`CurrentUsage`), MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Vram {
    pub budget_mb: u64,
    pub used_mb: u64,
}

impl Vram {
    /// From the byte counts DXGI reports, floored to whole MiB.
    pub fn from_bytes(budget: u64, used: u64) -> Self {
        const MIB: u64 = 1024 * 1024;
        Self {
            budget_mb: budget / MIB,
            used_mb: used / MIB,
        }
    }
}

/// One probe run: the adapter both devices are on, every adapter DXGI
/// lists, the report and the adapter's video memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuProbe {
    pub adapter: AdapterInfo,
    pub adapters: Vec<AdapterInfo>,
    pub report: ProbeReport,
    pub vram: Result<Vram, ProbeError>,
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
