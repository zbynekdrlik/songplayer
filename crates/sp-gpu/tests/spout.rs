//! WARP proof of the `SP-program-MAX` Spout sender (#223 S1b), and of the
//! FHD program's `SP-program` sender (#239) on the 1920×1080 target of the
//! same compositor (#251 step 2: two senders, one per target).
//!
//! Each test draws on WARP (windows-latest has no GPU), sends the render
//! target through the vendored Spout2 SDK 2.007.017, and reads Spout's
//! registry the way a receiver does (`sp_gpu::spout_sender_names` /
//! `spout_sender_info`): the names map, then the sender's own map. The
//! shared texture is opened on a SECOND WARP device, as Arena opens it on
//! its own device, and read back.
//!
//! Spout's registry is machine-wide and a sender's first send races any
//! other's clean-up of the list (Spout's own `CleanSenders`), so the tests
//! take one lock and each uses its own sender name; only one test uses
//! each production name. Windows only; a capability that is missing FAILS
//! here, nothing is skipped.

#![cfg(windows)]

mod common;

use std::ffi::{CString, c_void};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use common::locks::{GiveBack, Hold, TestMutex, abandon, this_process_holder, wait_elsewhere};
use common::{
    BLACK, H, W, assert_matches_reference, assert_matches_reference_in, pattern, picture, warp,
};
use sp_gpu::spout_state::{
    ACCESS_HELD, MAX_ACCESS_HELD, MAX_UNREADABLE, NOT_LISTED, Registration, TAKEN, UNREADABLE,
};
use sp_gpu::{
    Composition, Compositor, FHD_HEIGHT, FHD_TARGET, FHD_WIDTH, GpuError, NAME_SLOT_LEN,
    SENDER_NAMES_MAP, SPOUT_FHD_SENDER_NAME, SPOUT_SENDER_NAME, SharedTextureInfo, SpoutSender,
    TargetId, mapped_len, read_shared_texture, spout_sender_info, spout_sender_names, unpad_rows,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_SHARED, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::System::Memory::{
    CreateFileMappingA, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    PAGE_READWRITE, UnmapViewOfFile,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueA};
use windows::core::{PCSTR, s};

/// `DXGI_FORMAT_B8G8R8A8_UNORM` (87), as Spout's registry stores it.
const BGRA: u32 = DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32;

/// One Spout test at a time (see the module doc).
static SPOUT: Mutex<()> = Mutex::new(());

fn one_at_a_time() -> MutexGuard<'static, ()> {
    SPOUT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Compose `composition` and read the render target back. The readback's
/// `Map` also waits for every command queued before it on the device, the
/// Spout copy of an earlier `send` included.
fn compose(compositor: &mut Compositor, composition: &Composition<'_>) -> Vec<u8> {
    compositor.compose(composition).expect("compose on WARP");
    compositor.read_back().expect("read back on WARP")
}

fn names() -> Vec<String> {
    spout_sender_names().expect("Spout's names map reads")
}

fn info(name: &str) -> Option<SharedTextureInfo> {
    spout_sender_info(name).unwrap_or_else(|e| panic!("{name}'s map reads: {e}"))
}

fn listed(name: &str) -> usize {
    names().iter().filter(|entry| *entry == name).count()
}

/// A second WARP device: the receiver's side, independent of the
/// compositor's device.
fn receiver_device() -> (ID3D11Device, ID3D11DeviceContext) {
    let mut device = None;
    let mut context = None;
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
            Some(&mut context),
        )
    }
    .expect("a second WARP device");
    (
        device.expect("D3D11CreateDevice gave a device"),
        context.expect("D3D11CreateDevice gave a context"),
    )
}

/// Spout's shared texture, opened on `device` from the handle in the
/// sender's map, as a receiver opens it.
fn open_shared(device: &ID3D11Device, info: &SharedTextureInfo) -> ID3D11Texture2D {
    let handle = HANDLE(info.share_handle_value() as *mut c_void);
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.OpenSharedResource(handle, &mut texture) }.unwrap_or_else(|e| {
        panic!(
            "a second WARP device must open Spout's shared texture (handle {:#x}): {e}",
            info.share_handle
        )
    });
    texture.expect("OpenSharedResource gave a texture")
}

/// `texture`'s pixels read on `device`: 3840×2160 BGRA rows, packed.
fn read_on(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
) -> Vec<u8> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: W,
        Height: H,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }
        .expect("a staging texture on the receiver's device");
    let staging = staging.expect("CreateTexture2D gave a texture");
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        context.CopyResource(&staging, texture);
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
    }
    .expect("map the receiver's staging copy");
    let (pitch, row, rows) = (mapped.RowPitch as usize, W as usize * 4, H as usize);
    let len = mapped_len(pitch, row, rows).expect("a pitch of at least a row");
    assert!(!mapped.pData.is_null(), "the mapping has data");
    // SAFETY: the staging texture is mapped, `rows` rows `pitch` bytes apart,
    // until the Unmap below.
    let bytes = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>().cast_const(), len) };
    let packed = unpad_rows(bytes, pitch, row, rows).expect("whole rows");
    unsafe { context.Unmap(&staging, 0) };
    packed
}

/// Spout's default sender list size, with no `MaxSenders` registry value
/// (windows-latest has none).
const SPOUT_MAX_SENDERS: usize = 64;

/// A shared-memory map `name` of `size` bytes, as Spout makes its maps.
fn new_map(name: &str, size: u32) -> HANDLE {
    let c_name = CString::new(name).expect("a name without NUL");
    unsafe {
        CreateFileMappingA(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            size,
            PCSTR(c_name.as_ptr().cast()),
        )
    }
    .unwrap_or_else(|e| panic!("the map {name}: {e}"))
}

/// Spout's `HKCU\Software\Leading Edge\Spout` value `MaxSenders`, if set.
fn max_senders_setting() -> Option<u32> {
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let result = unsafe {
        RegGetValueA(
            HKEY_CURRENT_USER,
            s!("Software\\Leading Edge\\Spout"),
            s!("MaxSenders"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    if result == ERROR_SUCCESS {
        Some(value)
    } else {
        assert_eq!(result, ERROR_FILE_NOT_FOUND, "read Spout's MaxSenders");
        None
    }
}

/// Fake senders in Spout's list, as other programs would put them there:
/// `count` names in `SpoutSenderNames` (64 makes the list FULL), each with
/// its own info map so Spout's clean-up keeps them. The list is emptied and
/// every map closed on drop (on a failed assertion too).
///
/// It writes Spout's machine-wide list, so it refuses to run where any
/// sender is listed or Spout's list size is not its default 64: CI's
/// windows-latest, never a box where Spout is in use.
struct FakeSenders {
    names_map: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    infos: Vec<HANDLE>,
    names: Vec<String>,
}

impl FakeSenders {
    fn new(count: usize) -> Self {
        assert!(
            names().is_empty(),
            "fake senders are only written where no Spout sender is listed: {:?}",
            names()
        );
        let setting = max_senders_setting();
        assert!(
            setting.is_none_or(|max| max as usize == SPOUT_MAX_SENDERS),
            "Spout's MaxSenders is {setting:?}, not its default {SPOUT_MAX_SENDERS}"
        );
        assert!(count <= SPOUT_MAX_SENDERS, "{count} fake senders");
        let size = SPOUT_MAX_SENDERS * NAME_SLOT_LEN;
        let names_map = new_map(SENDER_NAMES_MAP, size as u32);
        let view = unsafe { MapViewOfFile(names_map, FILE_MAP_ALL_ACCESS, 0, 0, size) };
        assert!(!view.Value.is_null(), "map Spout's names list");
        let names: Vec<String> = (0..count)
            .map(|i| format!("sp-gpu test filler {i:02}"))
            .collect();
        // SAFETY: the view maps `size` writable bytes until it is unmapped
        // in Drop.
        let slots = unsafe { std::slice::from_raw_parts_mut(view.Value.cast::<u8>(), size) };
        slots.fill(0);
        for (slot, name) in slots.chunks_exact_mut(NAME_SLOT_LEN).zip(&names) {
            slot[..name.len()].copy_from_slice(name.as_bytes());
        }
        let infos = names.iter().map(|name| new_map(name, 4096)).collect();
        Self {
            names_map,
            view,
            infos,
            names,
        }
    }
}

impl Drop for FakeSenders {
    fn drop(&mut self) {
        // Empty the list first, in case anything else keeps the map open.
        // SAFETY: the view is still mapped for the whole list.
        unsafe {
            std::ptr::write_bytes(
                self.view.Value.cast::<u8>(),
                0,
                SPOUT_MAX_SENDERS * NAME_SLOT_LEN,
            );
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.names_map);
            for info in &self.infos {
                let _ = CloseHandle(*info);
            }
        }
    }
}

/// `got` is byte for byte the compositor's frame `drawn`.
fn assert_same_frame(got: &[u8], drawn: &[u8], what: &str) {
    assert_eq!(got.len(), drawn.len(), "{what}: frame size");
    let differing = got.iter().zip(drawn).filter(|(g, d)| g != d).count();
    let first = got.iter().zip(drawn).position(|(g, d)| g != d);
    assert_eq!(
        differing, 0,
        "{what}: {differing} bytes differ from the compositor's frame, first at byte {first:?}"
    );
}

#[test]
fn a_sent_frame_registers_sp_program_max_at_4k_and_drop_unregisters_it() {
    let _one = one_at_a_time();
    let mut compositor = warp();
    let mut sender = SpoutSender::new(&compositor).expect("the SP-program-MAX sender");
    assert_eq!(sender.name(), SPOUT_SENDER_NAME);
    assert_eq!(sender.registration(), Registration::Fresh);
    // Spout registers a sender at its first send, not before.
    assert_eq!(listed(SPOUT_SENDER_NAME), 0, "{:?}", names());
    assert_eq!(info(SPOUT_SENDER_NAME), None);
    assert_eq!(sender.size(), (0, 0));

    let (stride, data) = pattern(1920, 1080, 3);
    compose(
        &mut compositor,
        &Composition::Picture(picture(1, 1920, 1080, stride, &data)),
    );
    let stats = sender.send().expect("send on WARP");
    // The first send creates Spout's 4K shared texture: it takes time.
    assert!(stats.send_us > 0, "{stats:?}");
    assert_eq!(sender.registration(), Registration::Confirmed);

    assert_eq!(listed(SPOUT_SENDER_NAME), 1, "{:?}", names());
    let entry = info(SPOUT_SENDER_NAME).expect("the sender's map exists");
    assert_eq!((entry.width, entry.height), (W, H), "{entry:?}");
    assert_eq!(entry.format, BGRA, "{entry:?}");
    assert_eq!(entry.usage, 0, "{entry:?}");
    assert_ne!(entry.share_handle, 0, "{entry:?}");
    // The map names this test process as the sender's host.
    let exe = std::env::current_exe().expect("the test's exe");
    let host = Path::new(&entry.host_path)
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase());
    let own = exe
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase());
    assert_eq!(host, own, "{entry:?}");
    assert_eq!(sender.size(), (W, H));

    // A second frame updates the same registration.
    sender.send().expect("send again");
    assert_eq!(listed(SPOUT_SENDER_NAME), 1);
    assert_eq!(info(SPOUT_SENDER_NAME), Some(entry));

    drop(sender);
    assert_eq!(listed(SPOUT_SENDER_NAME), 0, "{:?}", names());
    assert_eq!(info(SPOUT_SENDER_NAME), None, "the sender's map is gone");
}

/// #239: the FHD program's sender: the sender `SP-program` of the
/// 1920×1080 target (#251 step 2: added to MAX's compositor) registers at
/// its first send, Spout lists it at 1920×1080 (Arena: `SPOUT_SP-program`),
/// its shared texture read on a second device holds that target's frame,
/// and `Drop` unlists it.
#[test]
fn the_fhd_target_sends_as_sp_program() {
    let _one = one_at_a_time();
    let mut compositor = warp();
    let fhd = compositor.add_target(FHD_TARGET).expect("the FHD target");
    let mut sender = SpoutSender::new_fhd(&compositor, fhd).expect("the SP-program sender");
    assert_eq!(sender.name(), SPOUT_FHD_SENDER_NAME);
    assert_eq!(sender.name(), "SP-program");
    assert_eq!(listed(SPOUT_FHD_SENDER_NAME), 0, "{:?}", names());
    assert_eq!(info(SPOUT_FHD_SENDER_NAME), None);

    // A 4:3 picture: bars left and right in the 1920×1080 target.
    let (stride, data) = pattern(1440, 1080, 27);
    let frame = Composition::Picture(picture(1, 1440, 1080, stride, &data));
    compositor.compose(&frame).expect("compose on WARP");
    let stats = sender.send().expect("send on WARP");
    assert!(stats.send_us > 0, "{stats:?}");
    assert_eq!(sender.registration(), Registration::Confirmed);
    // The readback's Map waits for Spout's copy, queued on the same context.
    let drawn = compositor.read_back_target(fhd).expect("read back");

    assert_eq!(listed(SPOUT_FHD_SENDER_NAME), 1, "{:?}", names());
    assert_eq!(listed(SPOUT_SENDER_NAME), 0, "the MAX name is not taken");
    let entry = info(SPOUT_FHD_SENDER_NAME).expect("the sender's map exists");
    assert_eq!(
        (entry.width, entry.height),
        (FHD_WIDTH, FHD_HEIGHT),
        "{entry:?}"
    );
    assert_eq!(entry.format, BGRA, "{entry:?}");
    assert_ne!(entry.share_handle, 0, "{entry:?}");
    assert_eq!(sender.size(), (FHD_WIDTH, FHD_HEIGHT));

    let received = read_shared_texture(&entry).expect("a second WARP device reads it");
    assert_same_frame(&received, &drawn, "SP-program through Spout");
    assert_matches_reference_in(
        &received,
        &frame.layers_on(FHD_TARGET),
        (FHD_WIDTH, FHD_HEIGHT),
        "SP-program through Spout",
    );
    assert_eq!(&received[..4], &BLACK, "the bar at (0, 0)");

    drop(sender);
    assert_eq!(listed(SPOUT_FHD_SENDER_NAME), 0, "{:?}", names());
    assert_eq!(
        info(SPOUT_FHD_SENDER_NAME),
        None,
        "the sender's map is gone"
    );
}

/// #251 step 2: TWO senders on ONE compositor, one per target. One compose
/// (one upload) fills both; Spout lists each sender at its own target's
/// size, and each shared texture, read on a second device, holds its own
/// target's frame (MAX fills its width with the 4:3 picture, `SP-program`
/// keeps its bars). A target the compositor does not hold is refused
/// before Spout.
#[test]
fn two_senders_on_one_compositor_are_listed_with_their_own_sizes() {
    let _one = one_at_a_time();
    let (max_name, fhd_name) = ("sp-gpu test MAX target", "sp-gpu test FHD target");
    let mut compositor = warp();
    let fhd = compositor.add_target(FHD_TARGET).expect("the FHD target");
    let mut max_sender =
        SpoutSender::for_target(&compositor, TargetId::PRIMARY, max_name).expect("MAX's sender");
    let mut fhd_sender =
        SpoutSender::for_target(&compositor, fhd, fhd_name).expect("the FHD target's sender");

    let (stride, data) = pattern(1440, 1080, 33);
    let frame = Composition::Picture(picture(1, 1440, 1080, stride, &data));
    let stats = compositor.compose(&frame).expect("compose both targets");
    assert_eq!(stats.uploads, 1, "one upload for both targets");
    max_sender.send().expect("send MAX's target");
    fhd_sender.send().expect("send the FHD target");

    assert_eq!(
        (listed(max_name), listed(fhd_name)),
        (1, 1),
        "{:?}",
        names()
    );
    let max_entry = info(max_name).expect("MAX's map");
    let fhd_entry = info(fhd_name).expect("the FHD sender's map");
    assert_eq!((max_entry.width, max_entry.height), (W, H), "{max_entry:?}");
    assert_eq!(
        (fhd_entry.width, fhd_entry.height),
        (FHD_WIDTH, FHD_HEIGHT),
        "{fhd_entry:?}"
    );
    assert_eq!((max_entry.format, fhd_entry.format), (BGRA, BGRA));
    assert_eq!(max_sender.size(), (W, H));
    assert_eq!(fhd_sender.size(), (FHD_WIDTH, FHD_HEIGHT));

    let max_drawn = compositor.read_back().expect("read back MAX's target");
    let fhd_drawn = compositor.read_back_target(fhd).expect("read back FHD's");
    let received = read_shared_texture(&max_entry).expect("a second device reads MAX");
    assert_same_frame(&received, &max_drawn, "MAX's target through Spout");
    assert_matches_reference(&received, &frame.layers(), "MAX's target through Spout");
    assert_ne!(&received[..4], &BLACK, "MAX fills its width: no bar");
    let received = read_shared_texture(&fhd_entry).expect("a second device reads FHD");
    assert_same_frame(&received, &fhd_drawn, "the FHD target through Spout");
    assert_matches_reference_in(
        &received,
        &frame.layers_on(FHD_TARGET),
        (FHD_WIDTH, FHD_HEIGHT),
        "the FHD target through Spout",
    );
    assert_eq!(&received[..4], &BLACK, "the FHD target's bar at (0, 0)");

    assert!(compositor.remove_target(fhd));
    assert!(matches!(
        SpoutSender::for_target(&compositor, fhd, "sp-gpu test removed target"),
        Err(GpuError::NoObject { .. })
    ));
    drop(fhd_sender);
    drop(max_sender);
    assert_eq!(
        (listed(max_name), listed(fhd_name)),
        (0, 0),
        "{:?}",
        names()
    );
}

#[test]
fn the_shared_texture_opens_on_a_second_device_and_holds_each_frame() {
    let _one = one_at_a_time();
    let name = "sp-gpu test second device";
    let mut compositor = warp();
    let mut sender = SpoutSender::with_name(&compositor, name).expect("a test sender");

    // A 21:9 picture: black bars top and bottom, the picture between (#267:
    // a 4:3 one fills MAX's width now, with no bar to read).
    let (stride, data) = pattern(2560, 1080, 51);
    let first = Composition::Picture(picture(1, 2560, 1080, stride, &data));
    compose(&mut compositor, &first);
    sender.send().expect("send the first frame");
    // The readback's Map waits for Spout's copy, queued on the same context.
    let drawn = compositor.read_back().expect("read back the first frame");

    let entry = info(name).expect("the sender's map exists");
    let (device, context) = receiver_device();
    let shared = open_shared(&device, &entry);
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { shared.GetDesc(&mut desc) };
    assert_eq!((desc.Width, desc.Height), (W, H));
    assert_eq!(desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM);
    assert_ne!(
        desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED.0 as u32,
        0,
        "Spout's texture is shared"
    );

    let received = read_on(&device, &context, &shared);
    assert_same_frame(&received, &drawn, "the first frame through Spout");
    assert_matches_reference(&received, &first.layers(), "the first frame through Spout");
    assert_eq!(&received[..4], &BLACK, "the bar at (0, 0)");

    // The next frame, a 16:9 picture filling the canvas, replaces it.
    let (stride, data) = pattern(1280, 720, 17);
    let second = Composition::Picture(picture(2, 1280, 720, stride, &data));
    compose(&mut compositor, &second);
    sender.send().expect("send the second frame");
    let drawn = compositor.read_back().expect("read back the second frame");
    let received = read_on(&device, &context, &shared);
    assert_same_frame(&received, &drawn, "the second frame through Spout");
    assert_matches_reference(
        &received,
        &second.layers(),
        "the second frame through Spout",
    );
}

#[test]
fn a_second_sender_with_a_listed_name_is_refused() {
    let _one = one_at_a_time();
    let name = "sp-gpu test refused";
    let mut compositor = warp();
    let mut first = SpoutSender::with_name(&compositor, name).expect("the first sender");
    let (stride, data) = pattern(1280, 720, 9);
    compose(
        &mut compositor,
        &Composition::Picture(picture(1, 1280, 720, stride, &data)),
    );
    first.send().expect("the first sender registers");
    assert_eq!(listed(name), 1);

    match SpoutSender::with_name(&compositor, name) {
        Err(GpuError::SpoutNameTaken { name: taken }) => assert_eq!(taken, name),
        other => panic!("a listed name must be refused, got {other:?}"),
    }
    // Spout's own way, a renamed `<name>_1`, never appears.
    let renamed = format!("{name}_1");
    assert_eq!(listed(&renamed), 0, "{:?}", names());
    assert_eq!(listed(name), 1);

    drop(first);
    assert_eq!(listed(name), 0, "{:?}", names());
    assert_eq!(info(name), None);
}

#[test]
fn a_sender_whose_name_was_listed_since_it_was_made_is_refused_before_it_registers() {
    let _one = one_at_a_time();
    // 63 fake senders: the winner's registration makes the list FULL. Spout
    // would then not even rename the loser `<name>_1` at its first send: it
    // would open the winner's info map and write its own texture there. Only
    // the check before the first send stops that.
    let fakes = FakeSenders::new(SPOUT_MAX_SENDERS - 1);
    let name = "sp-gpu test race";
    let mut compositor = warp();
    // Neither is registered yet, so both are created.
    let mut winner = SpoutSender::with_name(&compositor, name).expect("the winner");
    let mut loser = SpoutSender::with_name(&compositor, name).expect("the loser");
    let (stride, data) = pattern(1280, 720, 21);
    compose(
        &mut compositor,
        &Composition::Picture(picture(1, 1280, 720, stride, &data)),
    );
    winner.send().expect("the winner registers the name");
    assert_eq!(winner.registration(), Registration::Confirmed);
    assert_eq!(names().len(), SPOUT_MAX_SENDERS, "the list is full");
    let winner_entry = info(name).expect("the winner's map");

    let renamed = format!("{name}_1");
    for attempt in ["first", "second"] {
        match loser.send() {
            Err(GpuError::SpoutNotRegistered {
                name: asked,
                why,
                holder,
            }) => {
                assert_eq!(asked, name);
                assert_eq!(why, TAKEN);
                assert_eq!(holder, None, "a taken name is no kept lock");
            }
            other => panic!("the {attempt} send of the loser must be refused, got {other:?}"),
        }
        assert_eq!(loser.registration(), Registration::Refused(TAKEN));
        assert_eq!(
            info(name),
            Some(winner_entry.clone()),
            "{attempt}: the winner's map is untouched"
        );
        assert_eq!(listed(&renamed), 0, "{attempt}: {:?}", names());
        assert_eq!(info(&renamed), None, "{attempt}: no `_1` map");
        assert_eq!(loser.size(), (0, 0), "{attempt}: the loser shares nothing");
    }
    assert_eq!(listed(name), 1, "the winner keeps the name");

    drop(loser);
    assert_eq!(
        listed(name),
        1,
        "dropping the loser leaves the winner listed"
    );
    assert_eq!(info(name), Some(winner_entry));
    drop(winner);
    assert_eq!(listed(name), 0, "{:?}", names());
    drop(fakes);
}

#[test]
fn the_sender_outlives_the_compositor() {
    let _one = one_at_a_time();
    let name = "sp-gpu test outlives";
    let mut compositor = warp();
    let mut sender = SpoutSender::with_name(&compositor, name).expect("a test sender");
    let (stride, data) = pattern(1280, 720, 33);
    compose(
        &mut compositor,
        &Composition::Picture(picture(1, 1280, 720, stride, &data)),
    );
    sender.send().expect("send while the compositor lives");

    // The sender holds its own device and render-target references.
    drop(compositor);
    sender.send().expect("send after the compositor is gone");
    assert_eq!(listed(name), 1);
    assert_eq!(sender.size(), (W, H));
    drop(sender);
    assert_eq!(listed(name), 0, "{:?}", names());
}

#[test]
fn a_sender_spout_does_not_list_is_refused_and_leaves_the_list_alone() {
    let _one = one_at_a_time();
    let full = FakeSenders::new(SPOUT_MAX_SENDERS);
    assert_eq!(names(), full.names, "Spout's list is full");
    let name = "sp-gpu test full list";
    let mut compositor = warp();
    // Its name is free, so it is created; Spout skips the registration of a
    // sender past its list's size, so its first send is not listed.
    let mut sender = SpoutSender::with_name(&compositor, name).expect("a test sender");
    let (stride, data) = pattern(1280, 720, 45);
    compose(
        &mut compositor,
        &Composition::Picture(picture(1, 1280, 720, stride, &data)),
    );
    for attempt in ["first", "second"] {
        match sender.send() {
            Err(GpuError::SpoutNotRegistered {
                name: asked,
                why,
                holder,
            }) => {
                assert_eq!(asked, name);
                assert_eq!(why, NOT_LISTED);
                assert_eq!(holder, None, "a full list is no kept lock");
            }
            other => {
                panic!("the {attempt} send of an unlisted sender must be refused, got {other:?}")
            }
        }
        assert_eq!(sender.registration(), Registration::Refused(NOT_LISTED));
        // Its own info map is released; the other senders are untouched.
        assert_eq!(info(name), None, "{attempt}: the sender's own map is gone");
        assert_eq!(names(), full.names, "{attempt}: the list is unchanged");
        assert_eq!(sender.size(), (0, 0), "{attempt}: it shares nothing");
    }
    drop(sender);
    assert_eq!(names(), full.names, "dropping it leaves the list alone");
    drop(full);
}

#[test]
fn a_name_spout_cannot_carry_never_reaches_spout() {
    let _one = one_at_a_time();
    let compositor = warp();
    let too_long = "x".repeat(229);
    for name in ["", too_long.as_str(), "naïve", "Local\\SP-program-MAX"] {
        match SpoutSender::with_name(&compositor, name) {
            Err(GpuError::SpoutName { name: refused, .. }) => assert_eq!(refused, name),
            other => panic!("{name:?} must be refused, got {other:?}"),
        }
        match spout_sender_info(name) {
            Err(GpuError::SpoutName { .. }) => {}
            other => panic!("{name:?}'s map is never opened, got {other:?}"),
        }
    }
}

#[test]
fn a_sender_that_never_existed_has_no_map() {
    let _one = one_at_a_time();
    assert_eq!(info("sp-gpu test never registered"), None);
    assert_eq!(listed("sp-gpu test never registered"), 0);
}

/// #243: a Spout list lock left abandoned (a program died holding it) is
/// handed to the next waiter, and Spout 2.007.017 never releases a lock it
/// got that way. The sender's thread gives it back after the call, so other
/// programs read the list again.
#[test]
fn a_list_lock_handed_over_abandoned_is_given_back() {
    let _one = one_at_a_time();
    let compositor = warp();
    let list = format!("{SENDER_NAMES_MAP}_mutex");
    let _alive = abandon(&list);
    let _cleanup = GiveBack(TestMutex::open(&list));
    // The open's CleanSenders is the first to wait for the list's lock.
    let sender = SpoutSender::with_name(&compositor, "SP-gpu-test-abandoned-list")
        .expect("a sender opens on an abandoned list lock");
    assert_eq!(
        wait_elsewhere(&list),
        WAIT_OBJECT_0.0,
        "another thread must take Spout's list lock at once (0x102 = the \
         sender's thread kept it, 0x80 = nothing took the abandoned lock)"
    );
    drop(sender);
}

/// #243 review: Spout's `CleanSenders` (a sender's open, and its name
/// claim) reads every listed sender's info map under that map's lock; a
/// lock it meets abandoned makes it drop that sender off the list, and
/// this thread then owns the lock. It is given back, though the name is
/// off the list by the time the call returns.
#[test]
fn a_listed_senders_map_lock_handed_over_abandoned_is_given_back() {
    let _one = one_at_a_time();
    let mut compositor = warp();
    let listed_name = "SP-gpu-test-abandoned-map";
    let mut listed_sender = SpoutSender::with_name(&compositor, listed_name).expect("a sender");
    compose(&mut compositor, &Composition::Black);
    listed_sender.send().expect("its first send lists it");
    assert_eq!(listed(listed_name), 1, "{:?}", names());
    let map_lock = format!("{listed_name}_mutex");
    let _alive = abandon(&map_lock);
    let _cleanup = GiveBack(TestMutex::open(&map_lock));
    // The next open's CleanSenders reads the listed sender's map.
    let opener = SpoutSender::with_name(&compositor, "SP-gpu-test-map-opener")
        .expect("a sender opens on an abandoned map lock");
    assert_eq!(
        wait_elsewhere(&map_lock),
        WAIT_OBJECT_0.0,
        "another thread must take the listed sender's map lock at once (0x102 \
         = the opener's thread kept it, 0x80 = nothing took the abandoned lock)"
    );
    drop(opener);
    drop(listed_sender);
}

/// #243: the same for a sender's texture lock: a receiver that died
/// holding it hands it to the sender's next send, which gives it back, so
/// the next receiver can read the texture.
#[test]
fn a_texture_lock_handed_over_abandoned_is_given_back() {
    let _one = one_at_a_time();
    let mut compositor = warp();
    let name = "SP-gpu-test-abandoned-texture";
    let mut sender = SpoutSender::with_name(&compositor, name).expect("a sender");
    compose(&mut compositor, &Composition::Black);
    sender
        .send()
        .expect("the first send makes the texture lock");
    assert_eq!(sender.registration(), Registration::Confirmed);
    let access = format!("{name}_SpoutAccessMutex");
    let _alive = abandon(&access);
    let _cleanup = GiveBack(TestMutex::open(&access));
    sender
        .send()
        .expect("a send that meets the abandoned lock still returns");
    assert_eq!(
        wait_elsewhere(&access),
        WAIT_OBJECT_0.0,
        "a receiver must take the texture lock at once (0x102 = the sender's \
         thread kept it, 0x80 = the send never waited for it)"
    );
    drop(sender);
}

/// #243: a texture lock another program keeps (handed to it abandoned)
/// freezes the receivers' picture, since Spout then skips every copy. The
/// sender is refused at the `MAX_ACCESS_HELD`-th such send in a row, the
/// refusal names the holder, and the sender is off Spout's list: a new one
/// gets a fresh lock once that program's receiver lets go.
#[test]
fn a_sender_whose_texture_lock_another_thread_keeps_is_refused_naming_it() {
    let _one = one_at_a_time();
    let mut compositor = warp();
    let name = "SP-gpu-test-kept-texture";
    let mut sender = SpoutSender::with_name(&compositor, name).expect("a sender");
    compose(&mut compositor, &Composition::Black);
    sender.send().expect("the first send");
    assert_eq!(listed(name), 1, "{:?}", names());
    let hold = Hold::take(&format!("{name}_SpoutAccessMutex"));
    let mut refusal = None;
    for send in 1..=MAX_ACCESS_HELD + 1 {
        match sender.send() {
            Ok(_) => assert!(send < MAX_ACCESS_HELD, "send {send} must be refused"),
            Err(GpuError::SpoutNotRegistered {
                name: asked,
                why,
                holder,
            }) => {
                assert_eq!(asked, name);
                refusal = Some((send, why, holder));
                break;
            }
            Err(other) => panic!("send {send}: {other}"),
        }
    }
    drop(hold);
    let (send, why, holder) = refusal.expect("refused while the lock is kept");
    assert_eq!(send, MAX_ACCESS_HELD, "refused at the 30th kept send");
    assert_eq!(why, ACCESS_HELD);
    assert_eq!(
        holder.map(|holder| holder.to_ascii_lowercase()),
        Some(this_process_holder())
    );
    assert_eq!(sender.registration(), Registration::Refused(ACCESS_HELD));
    assert_eq!(listed(name), 0, "the refused sender is off Spout's list");
}

/// #243: a sender refused because Spout's list stayed unreadable names the
/// program that keeps the list's lock (on PP: Arena, for 8 hours).
#[test]
fn a_sender_refused_on_a_kept_list_names_its_holder() {
    let _one = one_at_a_time();
    let mut compositor = warp();
    let name = "SP-gpu-test-kept-list";
    let mut sender = SpoutSender::with_name(&compositor, name).expect("a sender");
    compose(&mut compositor, &Composition::Black);
    let hold = Hold::take(&format!("{SENDER_NAMES_MAP}_mutex"));
    let mut refusal = None;
    // A lost frame (`GpuError::Spout`) does not count an unreadable read.
    for send in 1..=2 * MAX_UNREADABLE + 2 {
        match sender.send() {
            Ok(_) | Err(GpuError::Spout { .. }) => {}
            Err(GpuError::SpoutNotRegistered { why, holder, .. }) => {
                refusal = Some((send, why, holder));
                break;
            }
            Err(other) => panic!("send {send}: {other}"),
        }
    }
    drop(hold);
    let (send, why, holder) = refusal.expect("refused while the list is kept");
    assert_eq!(why, UNREADABLE, "send {send}");
    assert_eq!(
        holder.map(|holder| holder.to_ascii_lowercase()),
        Some(this_process_holder())
    );
    drop(sender);
    assert_eq!(listed(name), 0, "{:?}", names());
}
