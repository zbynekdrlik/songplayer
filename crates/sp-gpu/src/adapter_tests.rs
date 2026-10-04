//! Tests for the adapter choice (#223 S1a).

use super::{AdapterInfo, adapter_name, pick_adapter};

const GIB: u64 = 1024 * 1024 * 1024;

fn adapter(name: &str, vendor_id: u32, device_id: u32, memory: u64, software: bool) -> AdapterInfo {
    AdapterInfo {
        name: name.to_string(),
        vendor_id,
        device_id,
        dedicated_video_memory: memory,
        software_flag: software,
    }
}

fn rtx_3070_ti() -> AdapterInfo {
    adapter("NVIDIA GeForce RTX 3070 Ti", 0x10de, 0x2482, 8 * GIB, false)
}

fn basic_render_driver() -> AdapterInfo {
    adapter("Microsoft Basic Render Driver", 0x1414, 0x8c, 0, true)
}

/// The box's shape (#223 revision 2, "GPU: RTX 3070 Ti, plus 2 virtual
/// display adapters"), with DXGI's Basic Render Driver last. The virtual
/// adapters' names and ids are illustrative (S2's telemetry records the
/// box's real list); what matters is what they lack: dedicated memory.
fn box_shape() -> Vec<AdapterInfo> {
    vec![
        adapter("virtual display adapter 1", 0x1b36, 0x0100, 0, false),
        adapter("virtual display adapter 2", 0x1b36, 0x0100, 0, false),
        rtx_3070_ti(),
        basic_render_driver(),
    ]
}

#[test]
fn on_the_box_shape_the_rtx_is_picked() {
    assert_eq!(pick_adapter(&box_shape()), Some(2));
}

#[test]
fn a_software_adapter_is_never_picked_even_with_the_most_memory() {
    let adapters = [
        adapter("a software rasterizer", 0x10de, 0x1234, 16 * GIB, true),
        rtx_3070_ti(),
    ];
    assert_eq!(pick_adapter(&adapters), Some(1));
}

#[test]
fn the_basic_render_driver_is_software_whether_flagged_or_not() {
    assert!(basic_render_driver().is_software());
    let unflagged = adapter("Microsoft Basic Render Driver", 0x1414, 0x8c, 0, false);
    assert!(unflagged.is_software());
    let flagged = adapter("a flagged adapter", 0x10de, 0x2482, 8 * GIB, true);
    assert!(flagged.is_software());
    // Another Microsoft device, and another vendor's device 0x8C, are not.
    assert!(!adapter("a Microsoft adapter", 0x1414, 0x8e, 0, false).is_software());
    assert!(!adapter("device 0x8C", 0x10de, 0x8c, 8 * GIB, false).is_software());
    assert!(!rtx_3070_ti().is_software());
}

#[test]
fn an_adapter_with_no_dedicated_memory_is_no_candidate() {
    let virtual_only = [
        adapter("virtual display adapter", 0x1b36, 0x0100, 0, false),
        basic_render_driver(),
    ];
    assert_eq!(pick_adapter(&virtual_only), None);
    assert_eq!(pick_adapter(&[]), None);
    // A single non-software adapter with memory is picked.
    assert_eq!(pick_adapter(&[rtx_3070_ti()]), Some(0));
}

#[test]
fn the_largest_dedicated_memory_wins_in_either_order() {
    let small = adapter("an integrated GPU", 0x8086, 0x9a49, GIB / 8, false);
    assert_eq!(pick_adapter(&[small.clone(), rtx_3070_ti()]), Some(1));
    assert_eq!(pick_adapter(&[rtx_3070_ti(), small]), Some(0));
}

#[test]
fn a_tie_keeps_the_first_listed() {
    let twin = adapter("a second RTX 3070 Ti", 0x10de, 0x2482, 8 * GIB, false);
    assert_eq!(pick_adapter(&[rtx_3070_ti(), twin]), Some(0));
}

#[test]
fn the_adapter_name_ends_at_the_first_nul() {
    let mut raw = [0u16; 16];
    for (slot, unit) in raw.iter_mut().zip("RTX 3070".encode_utf16()) {
        *slot = unit;
    }
    raw[10] = u16::from(b'x'); // garbage after the NUL
    assert_eq!(adapter_name(&raw), "RTX 3070");
    let whole: Vec<u16> = "no terminator".encode_utf16().collect();
    assert_eq!(adapter_name(&whole), "no terminator");
    assert_eq!(adapter_name(&[0u16; 4]), "");
}
