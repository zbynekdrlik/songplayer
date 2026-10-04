//! The compositor off Windows: there is no Direct3D 11, so it is never
//! built. It has the portable part of the Windows API (`new`, `new_warp`,
//! `compose`, `read_back`, `adapter`), so a cross-platform caller compiles
//! everywhere. The Direct3D accessors (`device`, `render_target`,
//! `shared_handle`) and `adapters()` are Windows-only.

use crate::adapter::AdapterInfo;
use crate::composition::Composition;
use crate::error::GpuError;
use crate::stats::ComposeStats;

/// Off Windows the compositor cannot be built: [`Compositor::new`] and
/// [`Compositor::new_warp`] report [`GpuError::Unsupported`]. The type has
/// no values, so its methods can never run.
#[derive(Debug)]
pub enum Compositor {}

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
        match *self {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    pub fn read_back(&mut self) -> Result<Vec<u8>, GpuError> {
        match *self {}
    }

    /// Never runs (no value exists). `mutants::skip`: as `compose`.
    #[cfg_attr(test, mutants::skip)]
    pub fn adapter(&self) -> &AdapterInfo {
        match *self {}
    }
}

#[cfg(test)]
mod tests {
    use super::Compositor;
    use crate::error::GpuError;

    #[test]
    fn off_windows_no_compositor_can_be_built() {
        assert_eq!(Compositor::new().unwrap_err(), GpuError::Unsupported);
        assert_eq!(Compositor::new_warp().unwrap_err(), GpuError::Unsupported);
    }
}
