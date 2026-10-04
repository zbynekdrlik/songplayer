//! Tests for Spout's sender rules and registry formats (#223 S1b). The byte
//! fixtures are laid out the way Spout 2.007.017 writes them
//! (`spoutSenderNames::writeBufferFromSenderSet`, `SetSenderInfo`).

use super::{
    HRESULT_FILE_NOT_FOUND, NAME_SLOT_LEN, SENDER_NAMES_MAP, SHARED_TEXTURE_INFO_LEN,
    SPOUT_NAME_MAX_LEN, SPOUT_SENDER_NAME, SharedTextureInfo, check_sender_name, is_not_found,
    map_mutex_name, parse_sender_names, status, status_result,
};
use crate::error::GpuError;

/// A names map of `slots` slots holding `names` from the first slot on
/// (each NUL-terminated), the rest zero, as Spout writes it.
fn names_map(names: &[&[u8]], slots: usize) -> Vec<u8> {
    let mut map = vec![0u8; slots * NAME_SLOT_LEN];
    for (i, name) in names.iter().enumerate() {
        map[i * NAME_SLOT_LEN..i * NAME_SLOT_LEN + name.len()].copy_from_slice(name);
    }
    map
}

/// The refusal reason of `check_sender_name(name)`.
fn refused(name: &str) -> &'static str {
    match check_sender_name(name) {
        Err(GpuError::SpoutName { name: got, reason }) => {
            assert_eq!(got, name, "the error names the refused name");
            reason
        }
        other => panic!("{name:?} must be refused, got {other:?}"),
    }
}

#[test]
fn the_sender_is_sp_program_max_and_spout_can_carry_it() {
    assert_eq!(SPOUT_SENDER_NAME, "SP-program-MAX");
    let name = check_sender_name(SPOUT_SENDER_NAME).expect("the production name");
    assert_eq!(name.as_bytes(), b"SP-program-MAX");
}

#[test]
fn a_name_is_one_to_228_bytes_of_printable_ascii() {
    // The demo sender's name has a space (#223 M0); ' ' and '~' are the ends
    // of the printable range.
    for name in ["Spout Sender", " ", "~", "a"] {
        let c_name = check_sender_name(name).unwrap_or_else(|e| panic!("{name:?}: {e}"));
        assert_eq!(c_name.as_bytes(), name.as_bytes());
    }
    // 255 bytes, less `_<n>` (11) and `_Count_Semaphore` (16).
    assert_eq!(SPOUT_NAME_MAX_LEN, 228);
    let longest = "x".repeat(SPOUT_NAME_MAX_LEN);
    assert_eq!(
        check_sender_name(&longest)
            .expect("228 bytes")
            .as_bytes()
            .len(),
        228
    );
}

#[test]
fn a_name_spout_cannot_carry_is_refused_with_its_reason() {
    assert_eq!(refused(""), "empty");
    assert_eq!(refused(&"x".repeat(229)), "longer than 228 bytes");
    for name in ["tab\there", "\u{1f}", "del\u{7f}", "é", "a\0b"] {
        assert_eq!(refused(name), "not printable ASCII", "{name:?}");
    }
    for name in ["Local\\SP-program-MAX", "\\", "end\\"] {
        assert_eq!(
            refused(name),
            "holds a backslash (no kernel object name may)",
            "{name:?}"
        );
    }
}

#[test]
fn a_map_is_guarded_by_its_name_and_mutex() {
    assert_eq!(SENDER_NAMES_MAP, "SpoutSenderNames");
    assert_eq!(map_mutex_name(SENDER_NAMES_MAP), "SpoutSenderNames_mutex");
    assert_eq!(map_mutex_name("SP-program-MAX"), "SP-program-MAX_mutex");
}

#[test]
fn the_names_map_lists_each_slot_up_to_the_first_empty_one() {
    assert_eq!(NAME_SLOT_LEN, 256);
    let map = names_map(&[b"Arena", b"SP-program-MAX", b"Spout Sender"], 64);
    assert_eq!(
        parse_sender_names(&map),
        ["Arena", "SP-program-MAX", "Spout Sender"]
    );
    // A stale name after the terminating empty slot is not listed (Spout
    // stops at the first empty slot).
    let mut stale = names_map(&[b"A", b"", b"B"], 4);
    stale[2 * NAME_SLOT_LEN] = b'B';
    assert_eq!(parse_sender_names(&stale), ["A"]);
    assert!(parse_sender_names(&names_map(&[], 64)).is_empty());
    assert!(parse_sender_names(&[]).is_empty());
}

#[test]
fn a_name_fills_its_slot_up_to_the_last_byte_but_one() {
    let longest = vec![b'n'; NAME_SLOT_LEN - 1];
    let map = names_map(&[longest.as_slice(), b"next"], 3);
    let names = parse_sender_names(&map);
    assert_eq!(names.len(), 2);
    assert_eq!(names[0].len(), 255);
    assert_eq!(names[1], "next");
}

#[test]
fn a_slot_spout_would_not_read_ends_the_list() {
    // A first byte of 0x80 or more is a negative `char`: Spout stops there.
    for first in [0x80u8, 0xC3, 0xFF] {
        let mut map = names_map(&[b"A", b"?bc", b"C"], 3);
        map[NAME_SLOT_LEN] = first;
        assert_eq!(parse_sender_names(&map), ["A"], "first byte {first:#x}");
    }
    // 0x7F (DEL) is still a positive `char`: listed.
    let mut map = names_map(&[b"A", b"?bc"], 3);
    map[NAME_SLOT_LEN] = 0x7F;
    assert_eq!(parse_sender_names(&map), ["A", "\u{7f}bc"]);
    // A slot with no NUL in its 256 bytes: Spout's strncpy_s refuses it.
    let mut map = names_map(&[b"A"], 3);
    map[NAME_SLOT_LEN..2 * NAME_SLOT_LEN].fill(b'z');
    map[2 * NAME_SLOT_LEN] = b'C';
    assert_eq!(parse_sender_names(&map), ["A"]);
}

#[test]
fn only_whole_slots_are_read() {
    // A map cut inside its second slot lists only the first.
    let map = names_map(&[b"A", b"B"], 2);
    assert_eq!(parse_sender_names(&map[..NAME_SLOT_LEN + 10]), ["A"]);
    assert!(parse_sender_names(&map[..NAME_SLOT_LEN - 1]).is_empty());
}

/// A sender's map: `SharedTextureInfo` with every field distinct, then the
/// rest of a 4 KiB page.
fn info_map(description: &[u8]) -> Vec<u8> {
    let mut map = vec![0u8; 4096];
    let words: [(usize, u32); 5] = [
        (0, 0xC000_1A42),
        (4, 3840),
        (8, 2160),
        (12, 87),
        (16, 0x0102_0304),
    ];
    for (at, value) in words {
        map[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    map[20..20 + description.len()].copy_from_slice(description);
    map[276..280].copy_from_slice(&0x4000_0000u32.to_le_bytes());
    // Past the struct: never read.
    map[280..284].copy_from_slice(&[0xEE; 4]);
    map
}

#[test]
fn a_sender_s_map_holds_its_texture_size_format_and_host() {
    assert_eq!(SHARED_TEXTURE_INFO_LEN, 280);
    let info = SharedTextureInfo::parse(&info_map(b"C:\\SongPlayer\\songplayer.exe\0junk"))
        .expect("a whole SharedTextureInfo");
    assert_eq!(
        info,
        SharedTextureInfo {
            share_handle: 0xC000_1A42,
            width: 3840,
            height: 2160,
            format: 87,
            usage: 0x0102_0304,
            host_path: "C:\\SongPlayer\\songplayer.exe".to_owned(),
            partner_id: 0x4000_0000,
        }
    );
}

#[test]
fn a_description_with_no_nul_is_read_whole() {
    let description = vec![b'p'; 256];
    let info = SharedTextureInfo::parse(&info_map(&description)).expect("whole");
    assert_eq!(info.host_path.len(), 256);
    assert_eq!(info.partner_id, 0x4000_0000);
}

#[test]
fn a_map_shorter_than_the_struct_is_no_info() {
    let map = info_map(b"x\0");
    assert!(SharedTextureInfo::parse(&map[..SHARED_TEXTURE_INFO_LEN - 1]).is_none());
    let exact = SharedTextureInfo::parse(&map[..SHARED_TEXTURE_INFO_LEN]).expect("280 bytes");
    assert_eq!(exact.width, 3840);
}

#[test]
fn the_share_handle_is_sign_extended_as_spout_opens_it() {
    let info = |share_handle| SharedTextureInfo {
        share_handle,
        width: 0,
        height: 0,
        format: 0,
        usage: 0,
        host_path: String::new(),
        partner_id: 0,
    };
    // LongToHandle((long)0xC0001A42): a negative 32-bit value, widened.
    assert_eq!(info(0xC000_1A42).share_handle_value(), -1_073_735_102);
    assert_eq!(info(0x0000_1A42).share_handle_value(), 0x1A42);
    assert_eq!(info(0x7FFF_FFFF).share_handle_value(), 0x7FFF_FFFF);
}

#[test]
fn the_shim_s_codes_map_to_their_errors() {
    assert_eq!(status_result(status::OK, "spout_sender_send", "n"), Ok(()));
    assert_eq!(
        status_result(status::NAME_TAKEN, "spout_sender_create", "SP-program-MAX"),
        Err(GpuError::SpoutNameTaken {
            name: "SP-program-MAX".to_owned()
        })
    );
    let not_registered = |code, why| {
        assert_eq!(
            status_result(code, "spout_sender_send", "SP-program-MAX"),
            Err(GpuError::SpoutNotRegistered {
                name: "SP-program-MAX".to_owned(),
                why
            }),
            "code {code}"
        );
    };
    not_registered(
        status::RENAMED,
        "another sender took the name before its first send",
    );
    not_registered(status::NOT_LISTED, "Spout's sender list is full");
    not_registered(status::FIRST_SEND_FAILED, "its first send failed");
    for code in [3, 4, 5, 99] {
        assert_eq!(
            status_result(code, "spout_sender_send", "n"),
            Err(GpuError::Spout {
                call: "spout_sender_send",
                code: code as u32
            })
        );
    }
    // The codes the shim (spout_shim.cpp) returns.
    assert_eq!(
        [
            status::OK,
            status::NAME_TAKEN,
            status::RENAMED,
            status::NOT_LISTED,
            status::FIRST_SEND_FAILED
        ],
        [0, 1, 2, 6, 7]
    );
}

#[test]
fn only_a_missing_map_reads_as_absent() {
    assert_eq!(HRESULT_FILE_NOT_FOUND, 0x8007_0002);
    assert!(is_not_found(0x8007_0002));
    // E_ACCESSDENIED, ERROR_FILE_NOT_FOUND without the HRESULT wrapping, S_OK.
    for hresult in [0x8007_0005, 2, 0] {
        assert!(!is_not_found(hresult), "{hresult:#x}");
    }
}
