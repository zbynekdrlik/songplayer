//! #279 P3a: the GPU-resident reader's copy path, on WARP.
//!
//! A DXVA decoder hands each picture over as a sample with one
//! `MFCreateDXGISurfaceBuffer` buffer over a SLICE of its texture array. A
//! `GpuResident` reader whose producer is not retired does not read that
//! slice back: `DxgiSurface::copy_into` copies it (`GetResource` +
//! `GetSubresourceIndex`) into a slot of the pipeline's ring
//! (`sp_gpu::ProducerGpu::copy_from`); a retired producer's slice is read
//! back (`DxgiSurface::picture`, `SurfaceUse::of`). CI has no GPU decoder
//! and its WARP refuses the video device (`video-decode.md`), so
//! `copy_texture_as_decoded_sample` (doc-hidden) wraps a slice of an array
//! the test made on the producer's own device exactly as a decoder wraps its
//! output, and runs the reader's own choice. The ring frame is then drawn in
//! place by a compositor on ANOTHER WARP device and must equal the same
//! picture uploaded (and differ from the other slice's). WARP makes an NV12
//! texture ARRAY of at most 2 slices (`gpu-max.md`), so the array has 2. A
//! refusal fails the test; nothing is skipped.
//!
//! The retired test reads back slice 1 of that 2-slice SRV-bound array
//! through Media Foundation's staging copy, which no CI run has proven on
//! WARP yet (the single-texture readback in `mf_hw_decode.rs` reads
//! subresource 0). If WARP refuses it, read the HRESULT in the panic; never
//! switch the test to slice 0, which would lose the which-slice proof.

#![cfg(windows)]

use std::io::Write;

use sp_decoder::{DecodePath, copy_texture_as_decoded_sample};
use sp_gpu::{Composition, Compositor, FHD_FIT, Nv12Picture, ProducerGpu, Side};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

/// The picture a decoder hands over, and its 16-row aligned surface.
const PICTURE: (u32, u32) = (640, 360);
const SURFACE: (u32, u32) = (640, 368);

/// A WARP answer for the CI log, past libtest's capture.
fn log(line: String) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// A `PICTURE`-sized NV12 picture packed at its own width (luma rows, then
/// the interleaved UV rows), its waves shifted by `seed`.
fn picture_bytes(seed: u32) -> Vec<u8> {
    let (w, h) = (PICTURE.0 as usize, PICTURE.1 as usize);
    let mut bytes = Vec::with_capacity(w * h * 3 / 2);
    for y in 0..h {
        for x in 0..w {
            bytes.push(16 + ((3 * x + 5 * y + seed as usize) % 220) as u8);
        }
    }
    for y in 0..h / 2 {
        for x in 0..w {
            bytes.push(16 + ((7 * x + 2 * y + 3 * seed as usize) % 225) as u8);
        }
    }
    bytes
}

/// `picture` laid into a `SURFACE`-sized NV12 slice: its rows on top, the
/// padding rows 0, the UV plane after ALL the surface's luma rows.
fn slice(picture: &[u8]) -> Vec<u8> {
    let (w, h, sh) = (PICTURE.0 as usize, PICTURE.1 as usize, SURFACE.1 as usize);
    let mut bytes = vec![0u8; w * (sh + sh / 2)];
    bytes[..w * h].copy_from_slice(&picture[..w * h]);
    bytes[w * sh..w * sh + w * h / 2].copy_from_slice(&picture[w * h..]);
    bytes
}

/// An NV12 texture array of `slices`, made with their bytes on `device`
/// (shader-resource bound with initial data: the NV12 textures WARP makes).
fn texture_array(device: &ID3D11Device, slices: &[Vec<u8>]) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: SURFACE.0,
        Height: SURFACE.1,
        MipLevels: 1,
        ArraySize: slices.len() as u32,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init: Vec<D3D11_SUBRESOURCE_DATA> = slices
        .iter()
        .map(|bytes| D3D11_SUBRESOURCE_DATA {
            pSysMem: bytes.as_ptr().cast(),
            SysMemPitch: SURFACE.0,
            SysMemSlicePitch: 0,
        })
        .collect();
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, Some(init.as_ptr()), Some(&mut texture)) }
        .unwrap_or_else(|e| panic!("WARP must make a 2-slice NV12 array: {e}"));
    texture.expect("CreateTexture2D gave a texture")
}

/// `picture` uploaded and drawn on `compositor`, read back.
fn uploaded(compositor: &mut Compositor, id: u64, picture: &[u8]) -> Vec<u8> {
    let side = Side::Cpu(Nv12Picture {
        id,
        width: PICTURE.0,
        height: PICTURE.1,
        stride: PICTURE.0,
        data: picture,
    });
    compositor
        .compose(&Composition::Picture(side))
        .unwrap_or_else(|e| panic!("compose an upload on WARP: {e}"));
    compositor.read_back().expect("read back on WARP")
}

/// The largest difference of two read-backs of the same size.
fn worst(a: &[u8], b: &[u8]) -> u8 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0)
}

#[test]
fn a_decoders_slice_is_copied_into_the_ring_and_draws_as_its_picture() {
    let producer =
        ProducerGpu::new_warp().unwrap_or_else(|e| panic!("WARP must make the producer: {e}"));
    let pictures = [picture_bytes(7), picture_bytes(120)];
    let slices: Vec<Vec<u8>> = pictures.iter().map(|p| slice(p.as_slice())).collect();
    let array = texture_array(producer.device(), &slices);

    let surfaced = copy_texture_as_decoded_sample(&producer, &array, 1, PICTURE.0, PICTURE.1)
        .unwrap_or_else(|e| panic!("the copy of slice 1 into the ring: {e}"));
    assert!(surfaced.data.is_empty(), "no bytes read back");
    assert_eq!(surfaced.stride, PICTURE.0, "one chroma row");
    let path = surfaced.path;
    let frame = surfaced.gpu.expect("a live producer copies into its ring");
    assert_eq!(
        (frame.width(), frame.height(), frame.texture_size()),
        (PICTURE.0, PICTURE.1, PICTURE),
        "the visible box only, never the surface's padding"
    );
    assert_eq!(
        path,
        DecodePath::Software,
        "a texture without D3D11_BIND_DECODER is no decoder's output"
    );
    let stats = producer.stats();
    log(format!("the GPU-resident copy on WARP: {stats:?}"));
    assert_eq!((stats.slots, stats.in_use), (1, 1));
    assert!(stats.copy_us_p99.is_some(), "the copy was timed");

    let mut compositor = Compositor::new_warp_with_size(1920, 1080, FHD_FIT)
        .unwrap_or_else(|e| panic!("WARP must build the compositor: {e}"));
    let drawn = compositor
        .compose(&Composition::Picture(Side::Gpu(&frame)))
        .unwrap_or_else(|e| panic!("compose the ring frame on WARP: {e}"));
    assert_eq!((drawn.gpu_sides, drawn.uploads), (1, 0), "sampled in place");
    let from_ring = compositor.read_back().expect("read back on WARP");
    let slice_1 = uploaded(&mut compositor, 1, &pictures[1]);
    let slice_0 = uploaded(&mut compositor, 2, &pictures[0]);
    let (same, other) = (worst(&from_ring, &slice_1), worst(&from_ring, &slice_0));
    log(format!(
        "the ring frame against slice 1 uploaded: {same}, against slice 0: {other}"
    ));
    assert!(same <= 1, "the ring frame draws as slice 1 (worst {same})");
    assert!(other > 8, "and not as slice 0 (worst {other})");

    drop(frame);
    assert_eq!(producer.stats().in_use, 0, "the slot freed with its frame");
}

/// A picture the surface cannot hold is a decode error the reader falls
/// back on (`BufferLock`, "GPU ring: …"), and takes no slot.
#[test]
fn a_picture_larger_than_its_surface_is_refused_before_any_slot() {
    let producer =
        ProducerGpu::new_warp().unwrap_or_else(|e| panic!("WARP must make the producer: {e}"));
    let pictures = [picture_bytes(1), picture_bytes(2)];
    let slices: Vec<Vec<u8>> = pictures.iter().map(|p| slice(p.as_slice())).collect();
    let array = texture_array(producer.device(), &slices);
    let refused = copy_texture_as_decoded_sample(&producer, &array, 0, PICTURE.0, SURFACE.1 + 2);
    let error = match refused {
        Ok(surfaced) => panic!("a picture over its surface was copied: {surfaced:?}"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("GPU ring"), "{error}");
    assert_eq!(producer.stats().slots, 0, "no slot taken");
}

/// #279 P3a review round 3: a retired producer's surface is read back, not
/// copied: the reader's own choice (`DxgiSurface::picture`,
/// `SurfaceUse::of`) on the path a GPU-resident reader's picture takes. Its
/// bytes are the slice's picture, and no ring slot is taken.
#[test]
fn a_retired_producers_surface_is_read_back_not_copied() {
    let producer =
        ProducerGpu::new_warp().unwrap_or_else(|e| panic!("WARP must make the producer: {e}"));
    let pictures = [picture_bytes(11), picture_bytes(77)];
    let slices: Vec<Vec<u8>> = pictures.iter().map(|p| slice(p.as_slice())).collect();
    let array = texture_array(producer.device(), &slices);
    producer.retire();
    let surfaced = copy_texture_as_decoded_sample(&producer, &array, 1, PICTURE.0, PICTURE.1)
        .unwrap_or_else(|e| panic!("the readback of slice 1: {e}"));
    assert!(
        surfaced.gpu.is_none(),
        "a retired producer takes no ring slot"
    );
    assert_eq!(producer.stats().slots, 0, "no slot made");
    assert_eq!(surfaced.stride, PICTURE.0);
    assert_eq!(surfaced.path, DecodePath::Software, "no decoder texture");
    assert_eq!(surfaced.data.len(), pictures[1].len());
    let first_off = surfaced
        .data
        .iter()
        .zip(&pictures[1])
        .position(|(got, want)| got != want);
    assert_eq!(
        first_off, None,
        "slice 1's picture, read back byte for byte"
    );
}
