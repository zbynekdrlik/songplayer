//! Which GPU adapter the compositor runs on (pure).
//!
//! The box has an RTX 3070 Ti and two virtual display adapters, and DXGI
//! always lists the Microsoft Basic Render Driver (WARP) too, last. The
//! compositor takes the largest dedicated video memory that is not a
//! software adapter (#223 revision 2, D7). An adapter with no dedicated
//! video memory (a virtual display adapter) is never a candidate: it cannot
//! be "the largest", and the 4K canvas must not land on a renderer that
//! borrows system memory.

/// One DXGI adapter as [`pick_adapter`] sees it (`DXGI_ADAPTER_DESC1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
    /// The adapter's description, e.g. "NVIDIA GeForce RTX 3070 Ti".
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    /// Dedicated video memory, in bytes.
    pub dedicated_video_memory: u64,
    /// `DXGI_ADAPTER_FLAG_SOFTWARE` is set.
    pub software_flag: bool,
}

/// Microsoft's PCI vendor id.
pub const MICROSOFT_VENDOR_ID: u32 = 0x1414;

/// The Microsoft Basic Render Driver's device id (WARP as an adapter).
pub const BASIC_RENDER_DEVICE_ID: u32 = 0x8c;

impl AdapterInfo {
    /// A software rasterizer: DXGI flags it, or it is the Microsoft Basic
    /// Render Driver (vendor 0x1414, device 0x8C), flagged or not.
    pub fn is_software(&self) -> bool {
        self.software_flag
            || (self.vendor_id == MICROSOFT_VENDOR_ID && self.device_id == BASIC_RENDER_DEVICE_ID)
    }
}

/// The index of the adapter to compose on: the largest dedicated video
/// memory among the adapters that are not software and have some, the first
/// one listed on a tie. `None` when there is no such adapter (a box with no
/// GPU: the compositor then reports `GpuError::NoAdapter`, it never falls
/// back to WARP on its own).
pub fn pick_adapter(adapters: &[AdapterInfo]) -> Option<usize> {
    let mut best: Option<(usize, u64)> = None;
    for (index, adapter) in adapters.iter().enumerate() {
        let memory = adapter.dedicated_video_memory;
        if adapter.is_software() || memory == 0 {
            continue;
        }
        if best.is_none_or(|(_, most)| memory > most) {
            best = Some((index, memory));
        }
    }
    best.map(|(index, _)| index)
}

/// An adapter description from DXGI's fixed UTF-16 buffer: the text up to
/// the first NUL (the whole buffer when there is none).
pub fn adapter_name(raw: &[u16]) -> String {
    let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..len])
}

#[cfg(test)]
#[path = "adapter_tests.rs"]
mod tests;
