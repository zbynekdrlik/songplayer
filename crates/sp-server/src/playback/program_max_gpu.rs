//! The GPU the `program-max` thread drives (#223 S2), as traits, so every
//! decision of the worker (`program_max_worker.rs`) runs on Linux over a
//! fake; [`SpoutGpu`] is the production one (`sp-gpu`'s `Compositor` on the
//! picked adapter and its `SpoutSender`s). Moved out of
//! `program_max_worker.rs` for the 1000-line cap (#251 step 2).
//!
//! #251 step 2: ONE compositor (one device) draws both outputs: MAX's
//! primary 3840×2160 target and, while the FHD Spout sender runs, the
//! `SP-program` target (`FHD_TARGET`, [`MaxCompositor::add_target`]). One
//! compose uploads each picture once and draws every target; each target
//! has its own Spout sender ([`MaxGpu::sender`] for MAX's,
//! [`MaxGpu::fhd_sender`] for the FHD target).

use sp_gpu::{ComposeStats, Composition, GpuError, SpoutSendStats, Target, TargetId};

/// The compositor half of the GPU: draw one boundary into every target.
pub trait MaxCompositor {
    /// Draw one boundary into every target it holds, each picture uploaded
    /// once (#251 step 2).
    fn compose(&mut self, composition: &Composition<'_>) -> Result<ComposeStats, GpuError>;
    /// The adapter it runs on (the telemetry's `adapter`).
    fn adapter(&self) -> String;
    /// #251 step 2: add a render target (the FHD program's `FHD_TARGET`),
    /// drawn by every compose from the next one on.
    fn add_target(&mut self, target: Target) -> Result<TargetId, GpuError>;
    /// #251 step 2: draw the target `target` no more.
    fn remove_target(&mut self, target: TargetId);
    /// #251 step 2: the target `target`'s own part of the last compose's
    /// draw, µs (0 when it was not drawn).
    fn draw_us(&self, target: TargetId) -> u64;
}

/// The Spout half: share a target as it is now.
pub trait MaxSender {
    fn send(&mut self) -> Result<SpoutSendStats, GpuError>;
    /// #239: the size Spout's registry lists the sender at (what a receiver
    /// opens); `None` before its first send went out or while the registry
    /// cannot be read.
    fn listed_size(&self) -> Option<(u32, u32)>;
}

/// What builds the compositor and its senders, on the `program-max`
/// thread: MAX's (of the primary target) and (#239) the `SP-program`
/// sender's (#251 step 2: of the FHD target on the same compositor).
pub trait MaxGpu {
    type Compositor: MaxCompositor;
    type Sender: MaxSender;
    fn compositor(&mut self) -> Result<Self::Compositor, GpuError>;
    fn sender(&mut self, compositor: &Self::Compositor) -> Result<Self::Sender, GpuError>;
    /// #239: the sender `SP-program` of the compositor's FHD target `target`.
    fn fhd_sender(
        &mut self,
        compositor: &Self::Compositor,
        target: TargetId,
    ) -> Result<Self::Sender, GpuError>;
}

/// The production GPU: `sp_gpu::Compositor::new` (the largest hardware
/// adapter, never WARP; #267: each picture fills the wall's width,
/// `MAX_FIT`) and the sender `SP-program-MAX`; #239 / #251 step 2: the
/// sender `SP-program` of the 1920×1080 target added to the same compositor
/// (aspect-fitted like the NDI `SP-program`, `FHD_FIT`).
#[derive(Clone, Copy, Debug, Default)]
pub struct SpoutGpu;

impl MaxGpu for SpoutGpu {
    type Compositor = sp_gpu::Compositor;
    type Sender = sp_gpu::SpoutSender;

    fn compositor(&mut self) -> Result<sp_gpu::Compositor, GpuError> {
        sp_gpu::Compositor::new()
    }

    /// `mutants::skip`: it needs a compositor, which off Windows (the
    /// mutation runner) cannot exist; the WARP test drives the real sender.
    #[cfg_attr(test, mutants::skip)]
    fn sender(&mut self, compositor: &sp_gpu::Compositor) -> Result<sp_gpu::SpoutSender, GpuError> {
        sp_gpu::SpoutSender::new(compositor)
    }

    /// `mutants::skip`: as `sender`.
    #[cfg_attr(test, mutants::skip)]
    fn fhd_sender(
        &mut self,
        compositor: &sp_gpu::Compositor,
        target: TargetId,
    ) -> Result<sp_gpu::SpoutSender, GpuError> {
        sp_gpu::SpoutSender::new_fhd(compositor, target)
    }
}

impl MaxCompositor for sp_gpu::Compositor {
    /// `mutants::skip`: one call; off Windows no compositor exists to call
    /// it on (`sp-gpu`'s stub is uninhabited).
    #[cfg_attr(test, mutants::skip)]
    fn compose(&mut self, composition: &Composition<'_>) -> Result<ComposeStats, GpuError> {
        sp_gpu::Compositor::compose(self, composition)
    }

    /// `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    fn adapter(&self) -> String {
        sp_gpu::Compositor::adapter(self).name.clone()
    }

    /// `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    fn add_target(&mut self, target: Target) -> Result<TargetId, GpuError> {
        sp_gpu::Compositor::add_target(self, target)
    }

    /// `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    fn remove_target(&mut self, target: TargetId) {
        sp_gpu::Compositor::remove_target(self, target);
    }

    /// `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    fn draw_us(&self, target: TargetId) -> u64 {
        sp_gpu::Compositor::draw_us(self, target).unwrap_or_default()
    }
}

impl MaxSender for sp_gpu::SpoutSender {
    /// `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    fn send(&mut self) -> Result<SpoutSendStats, GpuError> {
        sp_gpu::SpoutSender::send(self)
    }

    /// Spout's registry entry of the sender's name, read as a receiver
    /// does. `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    fn listed_size(&self) -> Option<(u32, u32)> {
        let info = sp_gpu::spout_sender_info(self.name()).ok()??;
        Some((info.width, info.height))
    }
}

/// Whether `error` refuses a Spout sender for good: drop it, and a new one
/// only after the backoff.
pub fn is_refusal(error: &GpuError) -> bool {
    matches!(
        error,
        GpuError::SpoutNotRegistered { .. } | GpuError::SpoutNameTaken { .. }
    )
}
