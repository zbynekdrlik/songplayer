//! WARP proof of the picture ring (#279 P2, design comment 6237269720,
//! plan §5.3 / §6): a PRODUCER device writes a ring slot and the
//! compositor's OWN device draws it in place, under the slot's keyed mutex,
//! with no upload.
//!
//! - the ring's real copy code (`PictureRing::copy_from`) on a
//!   decoder-like source: a slice of an NV12 texture ARRAY of four, each
//!   slice taller than its picture (a decoder's 16-row aligned surface), so
//!   the subresource index, the visible box and the chroma plane's place
//!   are all exercised;
//! - each draw compared with the CPU reference of the SAME picture
//!   uploaded (`common::assert_matches_reference_in`, one code per quad);
//! - a slot opened once and held again, a fade of an uploaded picture and a
//!   ring slot, two slots in one compose;
//! - a slot another device holds is drawn black and counted, and drawn
//!   again once let go; a full ring waits its bound, then refuses; a new
//!   size is written over a free slot of the other, made again.
//!
//! #279 P3a: `ProducerGpu` (a pipeline's producer device and ring) copies
//! a decoder-like slice into its ring, times and counts it, and its frame
//! is drawn in place; `ProducerGpu::new` takes the picked adapter or none.
//!
//! P0's probe proved NV12 sharing by NT handle + keyed mutex on two WARP
//! devices (`gpu_resident_ok`, #279 comment 6217311216). Windows only: a
//! refusal FAILS here, nothing is skipped.

#![cfg(windows)]

mod common;

use std::ffi::c_void;
use std::io::Write;

use common::{assert_matches_reference_in, pattern, picture, warp_fhd};
use sp_gpu::{
    ComposeStats, Composition, Compositor, FHD_HEIGHT, FHD_TARGET, FHD_WIDTH, GpuError, GpuFrame,
    PictureRing, ProducerGpu, RING_WAIT, Side,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice,
    ID3D11Device, ID3D11Device1, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;
use windows::core::Interface;

const FHD: (u32, u32) = (FHD_WIDTH, FHD_HEIGHT);

/// The compositor's key: the writer releases it, the compositor takes it.
const KEY: u64 = 1;

/// A ring of `capacity` slots on a WARP producer device of its own.
fn ring(capacity: usize) -> PictureRing {
    PictureRing::new_warp(capacity)
        .unwrap_or_else(|e| panic!("WARP must make the ring's producer device: {e}"))
}

/// A WARP device of the test's own (a third side holding a slot's mutex).
fn warp_device() -> ID3D11Device {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_WARP,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0][..]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .expect("a WARP device");
    device.expect("D3D11CreateDevice gave a device")
}

/// One `width`×`height` surface of `format` (NV12: `texture` = its aligned
/// size), as a decoder hands its pictures over: an array of
/// `slices.len()` slices, each made with its bytes (NV12 initial data: the
/// UV plane after ALL the luma rows, `width × height`). Shader-resource
/// bound and made with initial data: the two NV12 textures P0's probe made
/// on WARP, in one (a decoder's own surfaces are decoder-bound, which WARP
/// cannot make). WARP makes an NV12 ARRAY of at most 2 slices, at any size
/// (3 or more: E_INVALIDARG; measured with a WARP device on SNV, 17.10.2026,
/// and CI 39066250195), so an array here has 2.
fn texture(
    device: &ID3D11Device,
    format: DXGI_FORMAT,
    (width, height): (u32, u32),
    slices: &[Vec<u8>],
) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: slices.len() as u32,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let pitch = if format == DXGI_FORMAT_NV12 {
        width
    } else {
        width * 4
    };
    let init: Vec<D3D11_SUBRESOURCE_DATA> = slices
        .iter()
        .map(|bytes| D3D11_SUBRESOURCE_DATA {
            pSysMem: bytes.as_ptr().cast(),
            SysMemPitch: pitch,
            SysMemSlicePitch: 0,
        })
        .collect();
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, Some(init.as_ptr()), Some(&mut texture)) }
        .unwrap_or_else(|e| panic!("WARP must make a {width}x{height} texture array: {e}"));
    texture.expect("CreateTexture2D gave a texture")
}

/// The bytes of one NV12 surface of `surface` size whose top-left
/// `picture` is the pattern picture `(stride, data)` (its rows packed at
/// the surface's width) and whose padding rows and columns are 0: the luma
/// rows, then the UV plane at `width × height` of the SURFACE.
fn surface((w, h): (u32, u32), (sw, sh): (u32, u32), (stride, data): (u32, &[u8])) -> Vec<u8> {
    let (w, h, sw, sh, stride) = (
        w as usize,
        h as usize,
        sw as usize,
        sh as usize,
        stride as usize,
    );
    let mut bytes = vec![0u8; sw * (sh + sh / 2)];
    for y in 0..h {
        bytes[y * sw..y * sw + w].copy_from_slice(&data[y * stride..y * stride + w]);
    }
    for y in 0..h.div_ceil(2) {
        let (to, from) = (sw * sh + y * sw, stride * h + y * stride);
        bytes[to..to + w].copy_from_slice(&data[from..from + w]);
    }
    bytes
}

/// A single-slice NV12 source of the pattern picture `(stride, data)`, its
/// own size.
fn nv12_source(
    ring: &PictureRing,
    size: (u32, u32),
    (stride, data): (u32, &[u8]),
) -> ID3D11Texture2D {
    let bytes = surface(size, size, (stride, data));
    texture(ring.device(), DXGI_FORMAT_NV12, size, &[bytes])
}

/// Compose `composition` on the FHD compositor and read its target back.
fn compose(compositor: &mut Compositor, composition: &Composition<'_>) -> (ComposeStats, Vec<u8>) {
    let stats = compositor
        .compose(composition)
        .unwrap_or_else(|e| panic!("compose on WARP: {e}"));
    let bgra = compositor.read_back().expect("read back on WARP");
    (stats, bgra)
}

/// A WARP answer for the CI log, past libtest's capture.
fn log(line: String) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

#[test]
fn a_slot_written_on_one_device_is_drawn_in_place_on_another() {
    let mut ring = ring(4);
    let (stride, data) = pattern(1280, 720, 3);
    let source = nv12_source(&ring, (1280, 720), (stride, &data[..]));
    let frame = ring
        .copy_from(&source, 0, (1280, 720))
        .unwrap_or_else(|e| panic!("the copy into a ring slot: {e}"));
    assert_eq!(
        (frame.width(), frame.height(), frame.texture_size()),
        (1280, 720, (1280, 720))
    );
    assert_ne!(frame.shared_handle(), 0, "the slot's NT handle");
    let mut compositor = warp_fhd();
    let drawn = Composition::Picture(Side::Gpu(&frame));
    let (stats, bgra) = compose(&mut compositor, &drawn);
    log(format!(
        "a ring slot drawn on another WARP device: {stats:?}"
    ));
    assert_eq!(
        (
            stats.gpu_sides,
            stats.gpu_opens,
            stats.uploads,
            stats.upload_bytes
        ),
        (1, 1, 0, 0),
        "sampled in place, opened once, nothing uploaded"
    );
    assert_eq!(
        (
            stats.km_acquire_timeouts,
            stats.abandoned,
            stats.open_failures
        ),
        (0, 0, 0)
    );
    let cpu = Composition::Picture(picture(1, 1280, 720, stride, &data));
    let layers = cpu.layers_on(FHD_TARGET);
    assert_matches_reference_in(&bgra, &layers, FHD, "a ring slot, 1280x720 in 1920x1080");
    let (again, bgra) = compose(&mut compositor, &drawn);
    assert_eq!(
        (again.gpu_sides, again.gpu_opens, again.uploads),
        (1, 0, 0),
        "the slot stays open; its mutex is taken again"
    );
    assert_matches_reference_in(&bgra, &layers, FHD, "the same slot, again");
    assert!(!frame.ring_dead());
}

#[test]
fn a_slot_is_copied_out_of_a_slice_of_a_decoder_like_texture_array() {
    let mut ring = ring(4);
    let (picture_size, surface_size) = ((640, 360), (640, 368));
    let pictures: Vec<(u32, Vec<u8>)> = [0, 80]
        .into_iter()
        .map(|seed| pattern(picture_size.0, picture_size.1, seed))
        .collect();
    let slices: Vec<Vec<u8>> = pictures
        .iter()
        .map(|(stride, data)| surface(picture_size, surface_size, (*stride, &data[..])))
        .collect();
    let array = texture(ring.device(), DXGI_FORMAT_NV12, surface_size, &slices);
    let frame = ring
        .copy_from(&array, 1, picture_size)
        .unwrap_or_else(|e| panic!("the copy of slice 1: {e}"));
    assert_eq!((frame.width(), frame.height()), picture_size);
    assert_eq!(frame.texture_size(), picture_size, "the visible box only");
    let mut compositor = warp_fhd();
    let (stats, bgra) = compose(&mut compositor, &Composition::Picture(Side::Gpu(&frame)));
    assert_eq!((stats.gpu_sides, stats.uploads), (1, 0));
    let (stride, data) = &pictures[1];
    let cpu = Composition::Picture(picture(1, picture_size.0, picture_size.1, *stride, data));
    assert_matches_reference_in(
        &bgra,
        &cpu.layers_on(FHD_TARGET),
        FHD,
        "slice 1 of a 2-slice 640x368 array, its 640x360 box",
    );
}

/// #279 P3a: the producer's own copy path (what sp-decoder's GPU-resident
/// reader calls per picture): slice 1 of a decoder-like 2-slice array made
/// on the producer device, into a ring slot, timed and counted, then drawn
/// in place on the compositor's device within S1a's reference.
#[test]
fn a_producer_copies_a_slice_into_its_ring_and_counts_it() {
    let producer =
        ProducerGpu::new_warp().unwrap_or_else(|e| panic!("WARP must make the producer: {e}"));
    let (picture_size, surface_size) = ((640, 360), (640, 368));
    let pictures: Vec<(u32, Vec<u8>)> = [5, 90]
        .into_iter()
        .map(|seed| pattern(picture_size.0, picture_size.1, seed))
        .collect();
    let slices: Vec<Vec<u8>> = pictures
        .iter()
        .map(|(stride, data)| surface(picture_size, surface_size, (*stride, &data[..])))
        .collect();
    let array = texture(producer.device(), DXGI_FORMAT_NV12, surface_size, &slices);
    let frame = producer
        .copy_from(&array, 1, picture_size)
        .unwrap_or_else(|e| panic!("the producer's copy of slice 1: {e}"));
    let stats = producer.stats();
    log(format!(
        "a producer's copy on WARP: {stats:?} ({producer:?})"
    ));
    assert_eq!((stats.slots, stats.in_use, stats.waits), (1, 1, 0));
    assert!(stats.copy_us_p99.is_some(), "the copy was timed");
    assert_eq!(stats.vram_bytes, 640 * 360 * 3 / 2, "one 640x360 NV12 slot");
    assert!(!producer.ring_dead());
    let mut compositor = warp_fhd();
    let (drawn, bgra) = compose(&mut compositor, &Composition::Picture(Side::Gpu(&frame)));
    assert_eq!((drawn.gpu_sides, drawn.uploads), (1, 0), "sampled in place");
    let (stride, data) = &pictures[1];
    let cpu = Composition::Picture(picture(1, picture_size.0, picture_size.1, *stride, data));
    assert_matches_reference_in(
        &bgra,
        &cpu.layers_on(FHD_TARGET),
        FHD,
        "the producer's slot of slice 1",
    );
    drop(frame);
    assert_eq!(producer.stats().in_use, 0, "the slot freed with its frame");
}

/// #279 P3a review round 2: a retired producer says so; its device is
/// healthy, so its ring lives (`ring_dead` asks the device and finds it
/// there); the reader decides to read its pictures back.
#[test]
fn a_retired_producer_says_so_and_its_ring_lives() {
    let producer =
        ProducerGpu::new_warp().unwrap_or_else(|e| panic!("WARP must make the producer: {e}"));
    assert!(!producer.retired());
    assert!(!producer.ring_dead(), "a healthy device: the ring lives");
    producer.retire();
    assert!(producer.retired());
    assert!(!producer.ring_dead(), "retired, not dead");
    let shown = format!("{producer:?}");
    assert!(shown.contains("retired: true"), "{shown}");
}

/// #279 P3a: production's producer takes the adapter `pick_adapter`
/// chooses, never WARP on its own: `windows-latest` has none (`NoAdapter`).
#[test]
fn the_production_producer_takes_the_picked_adapter_or_none() {
    match ProducerGpu::new() {
        Ok(producer) => log(format!("a producer on the picked adapter: {producer:?}")),
        Err(GpuError::NoAdapter) => log("no adapter for a producer (no GPU)".to_string()),
        Err(e) => panic!("the picked adapter or NoAdapter, never {e}"),
    }
}

#[test]
fn a_fade_mixes_an_uploaded_picture_and_a_ring_slot() {
    let mut ring = ring(4);
    let (from_stride, from) = pattern(1280, 720, 11);
    let (to_stride, to) = pattern(1440, 1080, 77);
    let source = nv12_source(&ring, (1440, 1080), (to_stride, &to[..]));
    let frame = ring.copy_from(&source, 0, (1440, 1080)).expect("the copy");
    let mut compositor = warp_fhd();
    let mixed = Composition::Fade {
        from: Some(picture(1, 1280, 720, from_stride, &from)),
        to: Some(Side::Gpu(&frame)),
        weight_q8: 128,
    };
    let (stats, bgra) = compose(&mut compositor, &mixed);
    assert_eq!((stats.uploads, stats.gpu_sides), (1, 1));
    let cpu = Composition::Fade {
        from: Some(picture(1, 1280, 720, from_stride, &from)),
        to: Some(picture(2, 1440, 1080, to_stride, &to)),
        weight_q8: 128,
    };
    assert_matches_reference_in(
        &bgra,
        &cpu.layers_on(FHD_TARGET),
        FHD,
        "a 50 % fade, uploaded 720p into a ring slot's 4:3",
    );
}

#[test]
fn two_ring_slots_fade_in_one_compose() {
    let mut ring = ring(4);
    let (a_stride, a) = pattern(1280, 720, 5);
    let (b_stride, b) = pattern(1280, 720, 150);
    let a_frame = ring
        .copy_from(
            &nv12_source(&ring, (1280, 720), (a_stride, &a[..])),
            0,
            (1280, 720),
        )
        .expect("the first copy");
    let b_frame = ring
        .copy_from(
            &nv12_source(&ring, (1280, 720), (b_stride, &b[..])),
            0,
            (1280, 720),
        )
        .expect("the second copy");
    assert_ne!(a_frame.key(), b_frame.key(), "two slots");
    let mut compositor = warp_fhd();
    let fade = Composition::Fade {
        from: Some(Side::Gpu(&a_frame)),
        to: Some(Side::Gpu(&b_frame)),
        weight_q8: 64,
    };
    let (stats, bgra) = compose(&mut compositor, &fade);
    assert_eq!((stats.gpu_sides, stats.gpu_opens, stats.uploads), (2, 2, 0));
    let cpu = Composition::Fade {
        from: Some(picture(1, 1280, 720, a_stride, &a)),
        to: Some(picture(2, 1280, 720, b_stride, &b)),
        weight_q8: 64,
    };
    assert_matches_reference_in(&bgra, &cpu.layers_on(FHD_TARGET), FHD, "two slots, 25 %");
}

/// A keyed mutex's `AcquireSync`, its raw HRESULT (the binding's `Result`
/// reads `WAIT_TIMEOUT` as `Ok`).
fn acquire(mutex: &IDXGIKeyedMutex, key: u64, milliseconds: u32) -> u32 {
    let hresult = unsafe {
        (Interface::vtable(mutex).AcquireSync)(Interface::as_raw(mutex), key, milliseconds)
    };
    hresult.0 as u32
}

/// `frame`'s slot opened on the test's own device, and its keyed mutex.
fn hold_elsewhere(device: &ID3D11Device, frame: &GpuFrame) -> IDXGIKeyedMutex {
    let device1: ID3D11Device1 = device.cast().expect("ID3D11Device1 on WARP");
    let handle = HANDLE(frame.shared_handle() as *mut c_void);
    let opened: ID3D11Texture2D =
        unsafe { device1.OpenSharedResource1(handle) }.expect("a third device opens the slot");
    opened.cast().expect("the opened slot's keyed mutex")
}

#[test]
fn a_slot_another_device_holds_is_drawn_black_and_counted() {
    let mut ring = ring(2);
    let (stride, data) = pattern(1280, 720, 9);
    let source = nv12_source(&ring, (1280, 720), (stride, &data[..]));
    let frame = ring.copy_from(&source, 0, (1280, 720)).expect("the copy");
    let holder = warp_device();
    let mutex = hold_elsewhere(&holder, &frame);
    assert_eq!(acquire(&mutex, KEY, 1_000), 0, "the test takes the slot");
    let mut compositor = warp_fhd();
    let drawn = Composition::Picture(Side::Gpu(&frame));
    let (busy, bgra) = compose(&mut compositor, &drawn);
    log(format!("a slot held elsewhere: {busy:?}"));
    assert_eq!(
        (busy.gpu_sides, busy.km_acquire_timeouts, busy.abandoned),
        (0, 1, 0)
    );
    assert!(
        bgra.chunks_exact(4).all(|pixel| pixel == [0, 0, 0, 255]),
        "the busy side is the black"
    );
    assert!(!frame.ring_dead(), "a busy slot only misses this draw");
    unsafe { mutex.ReleaseSync(KEY) }.expect("the test lets the slot go");
    let (stats, bgra) = compose(&mut compositor, &drawn);
    assert_eq!((stats.gpu_sides, stats.km_acquire_timeouts), (1, 0));
    let cpu = Composition::Picture(picture(1, 1280, 720, stride, &data));
    assert_matches_reference_in(&bgra, &cpu.layers_on(FHD_TARGET), FHD, "let go");
}

#[test]
fn a_full_ring_waits_its_bound_then_refuses() {
    let mut ring = ring(2);
    let (stride, data) = pattern(64, 64, 1);
    let source = nv12_source(&ring, (64, 64), (stride, &data[..]));
    let first = ring.copy_from(&source, 0, (64, 64)).expect("slot 0");
    let second = ring.copy_from(&source, 0, (64, 64)).expect("slot 1");
    let waited_ms = u64::try_from(RING_WAIT.as_millis()).expect("a short bound");
    assert_eq!(
        ring.copy_from(&source, 0, (64, 64)).map(|f| f.id()),
        Err(GpuError::RingFull { waited_ms })
    );
    let stats = ring.stats();
    assert_eq!(
        (stats.slots, stats.in_use, stats.waits, stats.full),
        (2, 2, 1, 1)
    );
    let freed = first.key();
    drop(first);
    let again = ring
        .copy_from(&source, 0, (64, 64))
        .expect("the freed slot");
    assert_eq!(again.key(), freed, "written again, its texture kept");
    assert_ne!(again.id(), second.id());
}

#[test]
fn a_source_the_ring_does_not_take_is_refused() {
    let mut ring = ring(2);
    let bgra = texture(
        ring.device(),
        DXGI_FORMAT_B8G8R8A8_UNORM,
        (64, 64),
        &[vec![0u8; 64 * 64 * 4]],
    );
    assert_eq!(
        ring.copy_from(&bgra, 0, (64, 64)).map(|f| f.id()),
        Err(GpuError::RingSourceFormat {
            format: DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32
        })
    );
    let (stride, data) = pattern(64, 64, 2);
    let nv12 = nv12_source(&ring, (64, 64), (stride, &data[..]));
    assert_eq!(
        ring.copy_from(&nv12, 0, (66, 64)).map(|f| f.id()),
        Err(GpuError::RingPicture {
            width: 66,
            height: 64,
            surface_width: 64,
            surface_height: 64,
        })
    );
    assert_eq!(
        ring.copy_from(&nv12, 1, (64, 64)).map(|f| f.id()),
        Err(GpuError::RingSubresource {
            subresource: 1,
            count: 1
        }),
        "a single texture has no subresource 1"
    );
    assert_eq!(ring.stats().slots, 0, "nothing taken");
}

/// A ring of two slots full of one size takes a picture of another into
/// its FREE slot, made again at the new size (a new allocation, its old
/// texture let go), and the compositor draws the new texture: the writer's
/// remake and its stale-texture rule (`ring_state::make_texture`,
/// `stale_texture`) on a real device.
#[test]
fn a_new_size_is_written_over_a_free_slot_of_the_other() {
    let mut ring = ring(2);
    let (small_stride, small) = pattern(640, 360, 21);
    let small_source = nv12_source(&ring, (640, 360), (small_stride, &small[..]));
    let kept = ring
        .copy_from(&small_source, 0, (640, 360))
        .expect("slot 0");
    let freed = ring
        .copy_from(&small_source, 0, (640, 360))
        .expect("slot 1");
    let freed_key = freed.key();
    drop(freed);
    let (big_stride, big) = pattern(1280, 720, 99);
    let big_source = nv12_source(&ring, (1280, 720), (big_stride, &big[..]));
    let remade = ring
        .copy_from(&big_source, 0, (1280, 720))
        .expect("the free slot, made again");
    assert_eq!(remade.key().slot, freed_key.slot, "the free slot");
    assert_ne!(remade.key().alloc_gen, freed_key.alloc_gen, "a new texture");
    assert_eq!(remade.texture_size(), (1280, 720));
    assert_eq!((ring.stats().slots, ring.stats().in_use), (2, 2));
    let mut compositor = warp_fhd();
    let (stats, bgra) = compose(&mut compositor, &Composition::Picture(Side::Gpu(&remade)));
    assert_eq!((stats.gpu_sides, stats.gpu_opens, stats.uploads), (1, 1, 0));
    let cpu = Composition::Picture(picture(1, 1280, 720, big_stride, &big));
    assert_matches_reference_in(
        &bgra,
        &cpu.layers_on(FHD_TARGET),
        FHD,
        "a slot made again at 1280x720",
    );
    assert!(!kept.ring_dead());
}

/// An odd picture takes a slot of even sides (its copy box rounded up
/// inside the decoder's aligned surface) and is drawn from it. Its pixels
/// are not pinned: the slot's chroma covers exactly half its columns, the
/// uploaded odd picture's ⌈w/2⌉ (a half-texel apart at the edge).
#[test]
fn an_odd_picture_takes_an_even_slot() {
    let mut ring = ring(2);
    let (stride, data) = pattern(64, 36, 4);
    let bytes = surface((64, 36), (64, 48), (stride, &data[..]));
    let source = texture(ring.device(), DXGI_FORMAT_NV12, (64, 48), &[bytes]);
    let frame = ring
        .copy_from(&source, 0, (63, 35))
        .expect("an odd picture");
    assert_eq!((frame.width(), frame.height()), (63, 35));
    assert_eq!(frame.texture_size(), (64, 36));
    let mut compositor = warp_fhd();
    let (stats, _) = compose(&mut compositor, &Composition::Picture(Side::Gpu(&frame)));
    assert_eq!((stats.gpu_sides, stats.abandoned), (1, 0));
}
