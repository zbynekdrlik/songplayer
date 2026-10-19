//! The compositor and the Spout sender off Windows: there is no Direct3D 11
//! and no Spout, so neither is ever built. They have the portable part of
//! the Windows API (`new`, `with_size`, `new_warp`, `compose`, `read_back`,
//! `adapter`, `size`, #251 step 2's `add_target` / `remove_target` /
//! `target_size` / `draw_us` / `read_back_target`, #279 P1a's
//! `enable_uyvy` / `disable_uyvy` / `read_uyvy`; the sender's `new`,
//! `new_fhd`, `for_target`, `send`, `size`, `name`, `registration`; the
//! registry readers), so a cross-platform caller
//! compiles everywhere. The Direct3D accessors (`device`, `render_target`,
//! `shared_handle`), the test constructors `new_on_listed_adapter` /
//! `new_warp_with_size` / `with_name`, `adapters()` and the
//! tests' second-device readback `read_shared_texture` (#223 S2) are
//! Windows-only. The vblank tracker (#223 follow-up) has no output to wait
//! on here: `VblankTracker::start` reports `Unsupported`. Nor (#243
//! follow-up) has Spout's sender list: `spout_list_holder` and
//! `ListSentinel::start` report `Unsupported`. Nor (#279 P0) is there a GPU
//! to probe: `probe_gpu` reports `Unsupported` (the tests' `probe_warp` /
//! `probe_on_listed_adapter` are Windows-only).

use std::convert::Infallible;
use std::marker::PhantomData;
use std::time::Instant;

use crate::adapter::AdapterInfo;
use crate::composition::{Composition, Fit, Target};
use crate::error::GpuError;
use crate::probe::GpuProbe;
use crate::ring::RingStats;
use crate::spout::SharedTextureInfo;
use crate::spout_lock::{LockHolder, SentinelEnd};
use crate::spout_state::Registration;
use crate::stats::{ComposeStats, SpoutSendStats};
use crate::targets::TargetId;
use crate::vblank::{OutputInfo, VblankGrid, VblankState};

/// Off Windows the compositor cannot be built: [`Compositor::new`] and
/// [`Compositor::new_warp`] report [`GpuError::Unsupported`]. The type has
/// no values (it holds an `Infallible`), so its methods can never run. Like
/// the Windows type it is neither `Send` nor `Sync`, so code that would
/// move it across threads fails on Linux too, not only on Windows.
#[derive(Debug)]
pub struct Compositor(Infallible, PhantomData<*const ()>);

impl Compositor {
    /// [`GpuError::Unsupported`]: no Direct3D 11 off Windows.
    pub fn new() -> Result<Self, GpuError> {
        Err(GpuError::Unsupported)
    }

    /// [`GpuError::Unsupported`]: no Direct3D 11 off Windows (#239: the
    /// `SP-program` sender's 1920×1080 compositor neither, whatever its
    /// #267 [`Fit`]).
    pub fn with_size(_width: u32, _height: u32, _fit: Fit) -> Result<Self, GpuError> {
        Err(GpuError::Unsupported)
    }

    /// [`GpuError::Unsupported`]: no WARP off Windows.
    pub fn new_warp() -> Result<Self, GpuError> {
        Err(GpuError::Unsupported)
    }

    /// Never runs (no value exists). `mutants::skip`: an uninhabited
    /// receiver, so no test can call it.
    #[cfg_attr(test, mutants::skip)]
    pub fn compose(&mut self, _composition: &Composition<'_>) -> Result<ComposeStats, GpuError> {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    pub fn read_back(&mut self) -> Result<Vec<u8>, GpuError> {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    pub fn adapter(&self) -> &AdapterInfo {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    pub fn size(&self) -> (u32, u32) {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    pub fn add_target(&mut self, _target: Target) -> Result<TargetId, GpuError> {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    pub fn remove_target(&mut self, _id: TargetId) -> bool {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    pub fn target_size(&self, _id: TargetId) -> Option<(u32, u32)> {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    pub fn draw_us(&self, _id: TargetId) -> Option<u64> {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    pub fn read_back_target(&mut self, _id: TargetId) -> Result<Vec<u8>, GpuError> {
        match self.0 {}
    }

    /// #279 P1a. Never runs (no value exists). `mutants::skip`: as
    /// `compose`.
    #[cfg_attr(test, mutants::skip)]
    pub fn enable_uyvy(&mut self, _target: TargetId) -> Result<(), GpuError> {
        match self.0 {}
    }

    /// #279 P1a. Never runs (no value exists). `mutants::skip`: as
    /// `compose`.
    #[cfg_attr(test, mutants::skip)]
    pub fn disable_uyvy(&mut self) -> bool {
        match self.0 {}
    }

    /// #279 P1a. Never runs (no value exists). `mutants::skip`: as
    /// `compose`. `ptr_arg`: the signature is the Windows one, which
    /// clears and refills the caller's `Vec`.
    #[cfg_attr(test, mutants::skip)]
    #[allow(clippy::ptr_arg)]
    pub fn read_uyvy(&mut self, _out: &mut Vec<u8>) -> Result<bool, GpuError> {
        match self.0 {}
    }
}

/// Off Windows there is no Spout sender: it needs a [`Compositor`], which
/// cannot be built. The type has no values, so its methods can never run;
/// like the Windows type it is neither `Send` nor `Sync`.
#[derive(Debug)]
pub struct SpoutSender(Infallible, PhantomData<*const ()>);

impl SpoutSender {
    /// Never runs: no [`Compositor`] exists to pass. `mutants::skip`: an
    /// uninhabited argument, so no test can call it.
    #[cfg_attr(test, mutants::skip)]
    pub fn new(compositor: &Compositor) -> Result<Self, GpuError> {
        match compositor.0 {}
    }

    /// Never runs: no [`Compositor`] exists to pass. `mutants::skip`: as
    /// `new`.
    #[cfg_attr(test, mutants::skip)]
    pub fn new_fhd(compositor: &Compositor, _target: TargetId) -> Result<Self, GpuError> {
        match compositor.0 {}
    }

    /// Never runs: no [`Compositor`] exists to pass. `mutants::skip`: as
    /// `new`.
    #[cfg_attr(test, mutants::skip)]
    pub fn for_target(
        compositor: &Compositor,
        _target: TargetId,
        _name: &str,
    ) -> Result<Self, GpuError> {
        match compositor.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `new`.
    #[cfg_attr(test, mutants::skip)]
    pub fn send(&mut self) -> Result<SpoutSendStats, GpuError> {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `new`.
    #[cfg_attr(test, mutants::skip)]
    pub fn set_send_mode(&mut self, _mode: sp_core::config::SpoutSend) {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `new`.
    #[cfg_attr(test, mutants::skip)]
    pub fn size(&self) -> (u32, u32) {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `new`.
    #[cfg_attr(test, mutants::skip)]
    pub fn name(&self) -> &str {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `new`.
    #[cfg_attr(test, mutants::skip)]
    pub fn registration(&self) -> Registration {
        match self.0 {}
    }
}

/// Off Windows there is no DXGI output to wait on: [`VblankTracker::start`]
/// reports [`GpuError::Unsupported`] and the type has no values.
#[derive(Debug)]
pub struct VblankTracker(Infallible);

impl VblankTracker {
    /// [`GpuError::Unsupported`]: no `WaitForVBlank` off Windows.
    pub fn start() -> Result<Self, GpuError> {
        Err(GpuError::Unsupported)
    }

    /// Never runs (no value exists). `mutants::skip`: an uninhabited
    /// receiver, so no test can call it.
    #[cfg_attr(test, mutants::skip)]
    pub fn output(&self) -> OutputInfo {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `output`.
    #[cfg_attr(test, mutants::skip)]
    pub fn grid(&self, _now: Instant) -> Option<VblankGrid> {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `output`.
    #[cfg_attr(test, mutants::skip)]
    pub fn state(&self, _now: Instant) -> VblankState {
        match self.0 {}
    }
}

/// #243 follow-up: off Windows there is no Spout list to wait on:
/// [`ListSentinel::start`] reports [`GpuError::Unsupported`] and the type
/// has no values.
#[derive(Debug)]
pub struct ListSentinel(Infallible);

impl ListSentinel {
    /// [`GpuError::Unsupported`]: no Spout off Windows.
    pub fn start() -> Result<Self, GpuError> {
        Err(GpuError::Unsupported)
    }

    /// Never runs (no value exists). `mutants::skip`: an uninhabited
    /// receiver, so no test can call it.
    #[cfg_attr(test, mutants::skip)]
    pub fn ended(&self) -> Option<SentinelEnd> {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `ended`.
    #[cfg_attr(test, mutants::skip)]
    pub fn stop(&mut self) {
        match self.0 {}
    }
}

/// #243 follow-up: [`GpuError::Unsupported`]: no Spout off Windows.
pub fn spout_list_holder() -> Result<Option<LockHolder>, GpuError> {
    Err(GpuError::Unsupported)
}

/// [`GpuError::Unsupported`]: no Spout off Windows.
pub fn spout_sender_names() -> Result<Vec<String>, GpuError> {
    Err(GpuError::Unsupported)
}

/// [`GpuError::Unsupported`]: no Spout off Windows.
pub fn spout_sender_info(_name: &str) -> Result<Option<SharedTextureInfo>, GpuError> {
    Err(GpuError::Unsupported)
}

/// #279 P0: [`GpuError::Unsupported`]: no Direct3D to probe off Windows.
pub fn probe_gpu() -> Result<GpuProbe, GpuError> {
    Err(GpuError::Unsupported)
}

/// #279 P3a: off Windows there is no producer device: [`ProducerGpu::new`]
/// reports [`GpuError::Unsupported`]. The type has no values, so its
/// methods never run; it is `Send + Sync`, like the Windows type (a
/// pipeline's lease shares it with its songs' decode threads).
#[derive(Debug)]
pub struct ProducerGpu(Infallible);

impl ProducerGpu {
    /// [`GpuError::Unsupported`]: no Direct3D 11 off Windows.
    pub fn new() -> Result<Self, GpuError> {
        Err(GpuError::Unsupported)
    }

    /// Never runs (no value exists). `mutants::skip`: an uninhabited
    /// receiver, so no test can call it.
    #[cfg_attr(test, mutants::skip)]
    pub fn adapter(&self) -> &AdapterInfo {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `adapter`.
    #[cfg_attr(test, mutants::skip)]
    pub fn stats(&self) -> RingStats {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `adapter`.
    #[cfg_attr(test, mutants::skip)]
    pub fn ring_dead(&self) -> bool {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `adapter`.
    #[cfg_attr(test, mutants::skip)]
    pub fn retire(&self) {
        match self.0 {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `adapter`.
    #[cfg_attr(test, mutants::skip)]
    pub fn retired(&self) -> bool {
        match self.0 {}
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Compositor, ListSentinel, ProducerGpu, VblankTracker, probe_gpu, spout_list_holder,
        spout_sender_info, spout_sender_names,
    };
    use crate::error::GpuError;

    #[test]
    fn off_windows_no_producer_device_can_be_built() {
        assert_eq!(ProducerGpu::new().unwrap_err(), GpuError::Unsupported);
    }

    /// A pipeline's lease shares its producer across threads (#279 P3a).
    #[test]
    fn the_producer_type_is_send_and_sync_like_the_windows_one() {
        fn shared<T: Send + Sync>() {}
        shared::<ProducerGpu>();
    }

    #[test]
    fn off_windows_the_gpu_cannot_be_probed() {
        assert_eq!(probe_gpu(), Err(GpuError::Unsupported));
    }

    #[test]
    fn off_windows_spout_s_list_is_neither_read_nor_waited_on() {
        assert_eq!(spout_list_holder(), Err(GpuError::Unsupported));
        assert_eq!(ListSentinel::start().unwrap_err(), GpuError::Unsupported);
    }

    #[test]
    fn off_windows_no_compositor_can_be_built() {
        assert_eq!(Compositor::new().unwrap_err(), GpuError::Unsupported);
        assert_eq!(Compositor::new_warp().unwrap_err(), GpuError::Unsupported);
        assert_eq!(
            Compositor::with_size(crate::FHD_WIDTH, crate::FHD_HEIGHT, crate::FHD_FIT).unwrap_err(),
            GpuError::Unsupported,
            "#239: nor the SP-program sender's"
        );
    }

    #[test]
    fn off_windows_no_vblank_tracker_starts() {
        assert_eq!(VblankTracker::start().unwrap_err(), GpuError::Unsupported);
    }

    #[test]
    fn off_windows_spout_s_registry_is_unsupported() {
        assert_eq!(spout_sender_names(), Err(GpuError::Unsupported));
        assert_eq!(
            spout_sender_info(crate::SPOUT_SENDER_NAME),
            Err(GpuError::Unsupported)
        );
    }
}
