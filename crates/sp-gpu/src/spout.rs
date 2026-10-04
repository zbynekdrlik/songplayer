//! Spout2's rules for a sender, and its shared-memory registry, as pure
//! functions (#223 S1b).
//!
//! `SP-program-MAX` reaches Resolume Arena as a Spout sender: the vendored
//! Spout2 SDK 2.007.017 (SpoutDX `SendTexture`, `vendor/spout2`) shares the
//! compositor's render target. Arena lists it as `SPOUT_SP-program-MAX`
//! (#223 M0). Everything decided about it lives here and in
//! [`spout_state`](crate::spout_state) (the sender's registration), tested on
//! Linux; `win/spout_sender.rs` and `win/spout_registry.rs` only call the
//! shim and Win32.
//!
//! The registry is what every Spout receiver reads:
//!
//! - the map [`SENDER_NAMES_MAP`]: one NUL-terminated name per
//!   [`NAME_SLOT_LEN`]-byte slot ([`parse_sender_names`]);
//! - one map per sender, named after it, holding its [`SharedTextureInfo`].
//!
//! Each map is read under the named mutex [`map_mutex_name`] gives it.

use std::ffi::CString;

use crate::error::GpuError;

/// The Spout sender name of `SP-program-MAX`. Resolume Arena lists the
/// sender as `SPOUT_SP-program-MAX` (category "Spout Servers", #223 M0).
pub const SPOUT_SENDER_NAME: &str = "SP-program-MAX";

/// The longest sender name Spout can carry, in bytes. A sender Spout
/// renames to `<name>_<n>` (up to 11 more bytes) gets
/// `<name>_<n>_Count_Semaphore` (16 more) built in 256 bytes with
/// `sprintf_s`, which aborts the process on overflow: 255 − 27 = 228. The
/// shim checks the same limit.
pub const SPOUT_NAME_MAX_LEN: usize = 228;

/// The shared-memory map listing every sender's name.
pub const SENDER_NAMES_MAP: &str = "SpoutSenderNames";

/// The bytes each name takes in [`SENDER_NAMES_MAP`] (`SpoutMaxSenderNameLen`).
pub const NAME_SLOT_LEN: usize = 256;

/// The size of Spout's `SharedTextureInfo`, the content of a sender's map.
pub const SHARED_TEXTURE_INFO_LEN: usize = 280;

/// Where `description` starts and ends in a `SharedTextureInfo`.
const DESCRIPTION: std::ops::Range<usize> = 20..276;

/// Why a name is refused: not printable ASCII (Spout names are 8-bit
/// `char`s other programs read in their own code page, and a first byte of
/// 0x80 or more ends Spout's list).
const NOT_PRINTABLE: &str = "not printable ASCII";

/// Why a name is refused: it holds a backslash, which no Windows kernel
/// object name (Spout's maps, mutexes, semaphores) may hold.
const BACKSLASH: &str = "holds a backslash (no kernel object name may)";

/// The shim's status codes (`src/win/spout_shim.cpp`, which must match).
/// The others (3 = the SDK reported failure, 4 = a C++ exception, 5 = a bad
/// argument) are [`GpuError::Spout`].
pub(crate) mod status {
    /// Done.
    pub const OK: i32 = 0;
    /// `claim_name`: Spout renamed the sender, a live sender holds the name.
    pub const RENAMED: i32 = 1;
}

/// `name` as the NUL-terminated string Spout takes, if Spout can carry it:
/// 1 to [`SPOUT_NAME_MAX_LEN`] bytes of printable ASCII (space included), no
/// backslash. Anything else is [`GpuError::SpoutName`]: nothing reaches
/// Spout.
pub fn check_sender_name(name: &str) -> Result<CString, GpuError> {
    let refuse = |reason| GpuError::SpoutName {
        name: name.to_owned(),
        reason,
    };
    if name.is_empty() {
        return Err(refuse("empty"));
    }
    if name.len() > SPOUT_NAME_MAX_LEN {
        return Err(refuse("longer than 228 bytes"));
    }
    if !name.bytes().all(|b| (b' '..=b'~').contains(&b)) {
        return Err(refuse(NOT_PRINTABLE));
    }
    if name.contains('\\') {
        return Err(refuse(BACKSLASH));
    }
    CString::new(name).map_err(|_| refuse(NOT_PRINTABLE))
}

/// The named mutex Spout guards the shared-memory map `map` with.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn map_mutex_name(map: &str) -> String {
    format!("{map}_mutex")
}

/// The names [`SENDER_NAMES_MAP`] lists, in its order, read the way Spout
/// reads it (`spoutSenderNames::readSenderSetFromBuffer`): one name per
/// [`NAME_SLOT_LEN`]-byte slot, up to its first NUL. The list ends at a slot
/// whose first byte is 0, or 0x80 or more (Spout tests a signed `char`
/// `> 0`), at a slot with no NUL in it (Spout's `strncpy_s` would hit MSVC's
/// invalid-parameter handler there, which ends the process), or at the end of
/// the map.
pub fn parse_sender_names(map: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for slot in map.chunks_exact(NAME_SLOT_LEN) {
        if slot[0] == 0 || slot[0] >= 0x80 {
            break;
        }
        let Some(end) = slot.iter().position(|&b| b == 0) else {
            break;
        };
        names.push(String::from_utf8_lossy(&slot[..end]).into_owned());
    }
    names
}

/// A sender's entry in Spout's registry: its map holds Spout's
/// `SharedTextureInfo` (`SpoutSenderNames.h`), 280 bytes, little-endian.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedTextureInfo {
    /// The shared texture's legacy DXGI handle, as 32 bits.
    pub share_handle: u32,
    /// The texture's width.
    pub width: u32,
    /// The texture's height.
    pub height: u32,
    /// The texture's `DXGI_FORMAT` (87 = `B8G8R8A8_UNORM`).
    pub format: u32,
    /// Unused by Spout 2.007 (0).
    pub usage: u32,
    /// `description`: the sending program's path, up to its first NUL.
    pub host_path: String,
    /// The sharing-mode bits (`SetSenderID`; 0 when never set).
    pub partner_id: u32,
}

impl SharedTextureInfo {
    /// The info at the start of a sender's map. `None` when the map is
    /// shorter than [`SHARED_TEXTURE_INFO_LEN`]; bytes past it are ignored
    /// (a map is at least a page).
    pub fn parse(map: &[u8]) -> Option<Self> {
        let info = map.get(..SHARED_TEXTURE_INFO_LEN)?;
        let word = |at: usize| {
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(&info[at..at + 4]);
            u32::from_le_bytes(bytes)
        };
        let description = &info[DESCRIPTION];
        let end = description
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(description.len());
        Some(Self {
            share_handle: word(0),
            width: word(4),
            height: word(8),
            format: word(12),
            usage: word(16),
            host_path: String::from_utf8_lossy(&description[..end]).into_owned(),
            partner_id: word(DESCRIPTION.end),
        })
    }

    /// The handle a receiver opens, as Spout makes it on x64
    /// (`LongToHandle((long)shareHandle)`): the 32 bits sign-extended.
    pub fn share_handle_value(&self) -> isize {
        self.share_handle as i32 as isize
    }
}

/// The result of a shim call (`spout_sender_open`, `claim_name`, `refuse`)
/// that returned `code`: `Ok` for [`status::OK`],
/// [`GpuError::SpoutNameTaken`] for [`status::RENAMED`] (`name` is the name
/// asked for), else [`GpuError::Spout`] with `call` and the code.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn status_result(code: i32, call: &'static str, name: &str) -> Result<(), GpuError> {
    match code {
        status::OK => Ok(()),
        status::RENAMED => Err(GpuError::SpoutNameTaken {
            name: name.to_owned(),
        }),
        _ => Err(GpuError::Spout {
            call,
            code: code as u32,
        }),
    }
}

/// `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)`: what opening a map that does
/// not exist fails with.
pub(crate) const HRESULT_FILE_NOT_FOUND: u32 = 0x8007_0002;

/// Whether opening a map failed only because no such map exists (no sender
/// of that name, or no Spout sender at all), which a reader reports as
/// "absent", not as an error.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_not_found(hresult: u32) -> bool {
    hresult == HRESULT_FILE_NOT_FOUND
}

#[cfg(test)]
#[path = "spout_tests.rs"]
mod tests;
