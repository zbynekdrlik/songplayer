//! A Media Foundation video subtype as readable text (#223 S0).
//!
//! The decode bench reports which codec a file really decodes with, so a 4K
//! sample measured as "AV1" is not an H.264 file yt-dlp picked instead. MF's
//! compressed video subtypes are FourCC GUIDs (`DEFINE_MEDIATYPE_GUID`):
//! `XXXXXXXX-0000-0010-8000-00AA00389B71`, with the FourCC in `Data1`, first
//! character in the low byte. So `MFVideoFormat_AV1` reads `AV01`,
//! `MFVideoFormat_VP90` reads `VP90`, and `MFVideoFormat_H264` reads `H264`.
//!
//! Pure and cross-platform, so Linux tests cover it. The Windows reader only
//! reads the GUID (`MediaFoundationVideoReader::codec`).

/// `Data2`, `Data3` and `Data4` of every FourCC-based media subtype GUID.
const FOURCC_DATA2: u16 = 0x0000;
const FOURCC_DATA3: u16 = 0x0010;
const FOURCC_DATA4: [u8; 8] = [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71];

/// The subtype GUID's FourCC (`AV01`, `VP90`, `H264`, `HEVC`, …) when it is a
/// FourCC GUID whose four bytes are printable ASCII, else the whole GUID as
/// text (`8D2FD10B-5841-4A6B-8905-588FEC1ADED9`), so an unknown subtype is
/// still named exactly.
pub fn subtype_name(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> String {
    let fourcc = data1.to_le_bytes();
    let is_fourcc_guid = data2 == FOURCC_DATA2 && data3 == FOURCC_DATA3 && data4 == FOURCC_DATA4;
    if is_fourcc_guid && fourcc.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        return fourcc.iter().map(|&b| char::from(b)).collect();
    }
    let [a, b, c, d, e, f, g, h] = data4;
    format!(
        "{data1:08X}-{data2:04X}-{data3:04X}-{a:02X}{b:02X}-\
         {c:02X}{d:02X}{e:02X}{f:02X}{g:02X}{h:02X}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `FCC('abcd')`: the first character in the low byte, as MF builds it.
    fn fcc(code: &[u8; 4]) -> u32 {
        u32::from_le_bytes(*code)
    }

    #[test]
    fn fourcc_subtypes_read_as_their_codec() {
        for code in [b"AV01", b"VP90", b"H264", b"HEVC", b"NV12"] {
            let name = subtype_name(fcc(code), FOURCC_DATA2, FOURCC_DATA3, FOURCC_DATA4);
            assert_eq!(name.as_bytes(), &code[..]);
        }
        // A space is a legal FourCC character and is kept.
        let name = subtype_name(fcc(b"MP4 "), FOURCC_DATA2, FOURCC_DATA3, FOURCC_DATA4);
        assert_eq!(name, "MP4 ");
    }

    #[test]
    fn a_guid_that_is_not_fourcc_based_is_named_whole() {
        // MFVideoFormat_MPEG2: {E06D8026-DB46-11CF-B4D1-00805F6CBBEA}.
        let data4 = [0xB4, 0xD1, 0x00, 0x80, 0x5F, 0x6C, 0xBB, 0xEA];
        let name = subtype_name(0xE06D_8026, 0xDB46, 0x11CF, data4);
        assert_eq!(name, "E06D8026-DB46-11CF-B4D1-00805F6CBBEA");
    }

    #[test]
    fn each_tail_field_must_match_for_a_fourcc_reading() {
        let h264 = fcc(b"H264");
        let whole = |d2: u16, d3: u16, d4: [u8; 8]| subtype_name(h264, d2, d3, d4);
        assert_eq!(
            whole(0x0001, FOURCC_DATA3, FOURCC_DATA4),
            "34363248-0001-0010-8000-00AA00389B71"
        );
        assert_eq!(
            whole(FOURCC_DATA2, 0x0011, FOURCC_DATA4),
            "34363248-0000-0011-8000-00AA00389B71"
        );
        let mut d4 = FOURCC_DATA4;
        d4[7] = 0x72;
        assert_eq!(
            whole(FOURCC_DATA2, FOURCC_DATA3, d4),
            "34363248-0000-0010-8000-00AA00389B72"
        );
    }

    #[test]
    fn a_fourcc_guid_with_unprintable_bytes_is_named_whole() {
        // D3DFMT-style numeric subtypes (e.g. 0x00000016) share the FourCC
        // tail but are not text.
        let name = subtype_name(0x0000_0016, FOURCC_DATA2, FOURCC_DATA3, FOURCC_DATA4);
        assert_eq!(name, "00000016-0000-0010-8000-00AA00389B71");
        // One unprintable byte among printable ones is enough.
        let name = subtype_name(fcc(b"AV0\x01"), FOURCC_DATA2, FOURCC_DATA3, FOURCC_DATA4);
        assert_eq!(name, "01305641-0000-0010-8000-00AA00389B71");
    }
}
