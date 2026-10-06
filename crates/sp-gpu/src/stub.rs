//! The compositor and the Spout sender off Windows: there is no Direct3D 11
//! and no Spout, so neither is ever built. They have the portable part of
//! the Windows API (`new`, `new_warp`, `compose`, `read_back`, `adapter`;
//! the sender's `new`, `send`, `size`, `name`, `registration`; the registry
//! readers), so a cross-platform caller compiles everywhere. The Direct3D
//! accessors (`device`, `render_target`, `shared_handle`), the test
//! constructors `new_on_listed_adapter` / `with_name`, `adapters()` and the
//! tests' second-device readback `read_shared_texture` (#223 S2) are
//! Windows-only.

use std::convert::Infallible;
use std::marker::PhantomData;

use crate::adapter::AdapterInfo;
use crate::composition::Composition;
use crate::error::GpuError;
use crate::spout::SharedTextureInfo;
use crate::spout_state::Registration;
use crate::stats::{ComposeStats, SpoutSendStats};

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

    /// Never runs (no value exists). `mutants::skip`: as `new`.
    #[cfg_attr(test, mutants::skip)]
    pub fn send(&mut self) -> Result<SpoutSendStats, GpuError> {
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

/// [`GpuError::Unsupported`]: no Spout off Windows.
pub fn spout_sender_names() -> Result<Vec<String>, GpuError> {
    Err(GpuError::Unsupported)
}

/// [`GpuError::Unsupported`]: no Spout off Windows.
pub fn spout_sender_info(_name: &str) -> Result<Option<SharedTextureInfo>, GpuError> {
    Err(GpuError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::{Compositor, spout_sender_info, spout_sender_names};
    use crate::error::GpuError;

    #[test]
    fn off_windows_no_compositor_can_be_built() {
        assert_eq!(Compositor::new().unwrap_err(), GpuError::Unsupported);
        assert_eq!(Compositor::new_warp().unwrap_err(), GpuError::Unsupported);
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
