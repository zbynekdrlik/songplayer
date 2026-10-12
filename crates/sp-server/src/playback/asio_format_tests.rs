//! #233: the driver's sample types (ASIOSampleType codes, little-endian only)
//! and how the program's L/R are written into them: symmetric full scale,
//! clamped, NaN silent; every other channel zeroed; an underrun's missing
//! frames zeroed.

use super::*;

#[test]
fn the_supported_codes_and_the_refused_ones() {
    let table = [
        (16, AsioSample::Int16, "Int16LSB", 2),
        (17, AsioSample::Int24, "Int24LSB", 3),
        (18, AsioSample::Int32, "Int32LSB", 4),
        (19, AsioSample::Float32, "Float32LSB", 4),
        (24, AsioSample::Int32In16, "Int32LSB16", 4),
        (25, AsioSample::Int32In18, "Int32LSB18", 4),
        (26, AsioSample::Int32In20, "Int32LSB20", 4),
        (27, AsioSample::Int32In24, "Int32LSB24", 4),
    ];
    for (code, sample, name, bytes) in table {
        assert_eq!(AsioSample::from_code(code), Ok(sample));
        assert_eq!((sample.name(), sample.bytes()), (name, bytes));
    }
    // Float64 (20), the MSB types (0-11), DSD (32, 33, 40) and unknown codes.
    for code in [0, 2, 8, 11, 15, 20, 21, 23, 28, 32, 33, 40, -1] {
        assert_eq!(AsioSample::from_code(code), Err(code));
    }
    assert_eq!(
        unsupported_sample_text(20),
        "the driver's sample type 20 is not supported (Int16/24/32LSB, Float32LSB, Int32LSB16-24)"
    );
}

/// `x` on the left channel (the right one carries −x) as one sample.
fn one(sample: AsioSample, x: f32) -> Vec<u8> {
    let mut dst = vec![0xAA; sample.bytes()];
    sample.encode(&[x, -x], 0, &mut dst);
    dst
}

#[test]
fn each_type_encodes_exactly() {
    assert_eq!(one(AsioSample::Int16, 1.0), 32_767i16.to_le_bytes());
    assert_eq!(one(AsioSample::Int16, -1.0), (-32_767i16).to_le_bytes());
    assert_eq!(one(AsioSample::Int16, 0.5), 16_384i16.to_le_bytes());
    assert_eq!(one(AsioSample::Int24, 0.5), [0x00, 0x00, 0x40], "4_194_304");
    assert_eq!(
        one(AsioSample::Int24, -1.0),
        [0x01, 0x00, 0x80],
        "−8_388_607"
    );
    assert_eq!(one(AsioSample::Int32, 1.0), 2_147_483_647i32.to_le_bytes());
    assert_eq!(
        one(AsioSample::Int32, -0.5),
        (-1_073_741_824i32).to_le_bytes()
    );
    assert_eq!(one(AsioSample::Int32In24, 0.5), 4_194_304i32.to_le_bytes());
    assert_eq!(
        one(AsioSample::Int32In24, -1.0),
        (-8_388_607i32).to_le_bytes()
    );
    assert_eq!(one(AsioSample::Int32In20, 1.0), 524_287i32.to_le_bytes());
    assert_eq!(one(AsioSample::Int32In18, 1.0), 131_071i32.to_le_bytes());
    assert_eq!(one(AsioSample::Int32In16, -1.0), (-32_767i32).to_le_bytes());
    assert_eq!(one(AsioSample::Float32, 0.25), 0.25f32.to_le_bytes());
    assert_eq!(
        one(AsioSample::Float32, 1.5),
        1.0f32.to_le_bytes(),
        "clamped"
    );
    assert_eq!(
        one(AsioSample::Int16, -3.0),
        (-32_767i16).to_le_bytes(),
        "clamped"
    );
    assert_eq!(
        one(AsioSample::Int24, f32::NAN),
        [0, 0, 0],
        "NaN is silence"
    );
    assert_eq!(one(AsioSample::Float32, f32::INFINITY), [0, 0, 0, 0]);
}

#[test]
fn the_right_channel_is_read_from_the_second_sample() {
    let mut dst = vec![0u8; 4];
    AsioSample::Int16.encode(&[0.5, -0.5, 0.25, -0.25], 1, &mut dst);
    assert_eq!(&dst[..2], &(-16_384i16).to_le_bytes());
    assert_eq!(&dst[2..], &(-8_192i16).to_le_bytes());
}

#[test]
fn fill_channel_writes_the_source_zeroes_the_rest_and_the_missing_frames() {
    let stereo = [0.5f32, -0.5, 0.5, -0.5, 0.5, -0.5]; // 3 frames of 4
    let mut left = vec![0xAAu8; 4 * 3];
    fill_channel(AsioSample::Int24, &stereo, Some(0), &mut left);
    assert_eq!(&left[..9], &[0x00, 0x00, 0x40].repeat(3)[..]);
    assert_eq!(&left[9..], &[0, 0, 0], "the underrun's frame is silence");
    let mut right = vec![0xAAu8; 4 * 2];
    fill_channel(AsioSample::Int16, &stereo, Some(1), &mut right);
    assert_eq!(&right[..6], &(-16_384i16).to_le_bytes().repeat(3)[..]);
    assert_eq!(&right[6..], &[0, 0]);
    let mut other = vec![0xAAu8; 12];
    fill_channel(AsioSample::Int24, &stereo, None, &mut other);
    assert!(other.iter().all(|&b| b == 0), "an unused channel is silent");
    let mut empty = vec![0xAAu8; 8];
    fill_channel(AsioSample::Float32, &[], Some(0), &mut empty);
    assert!(empty.iter().all(|&b| b == 0), "a dry ring is silence");
    // A ring that gave more than the half-buffer holds: only what fits.
    let mut short = vec![0xAAu8; 2 * 3];
    fill_channel(AsioSample::Int24, &stereo, Some(0), &mut short);
    assert_eq!(&short[..], &[0x00, 0x00, 0x40].repeat(2)[..]);
}

#[test]
fn each_output_channel_plays_left_right_or_nothing() {
    assert_eq!(source_of(0, 0, 1), Some(0));
    assert_eq!(source_of(1, 0, 1), Some(1));
    assert_eq!(source_of(5, 2, 5), Some(1));
    assert_eq!(source_of(2, 2, 5), Some(0));
    assert_eq!(source_of(3, 2, 5), None);
    assert_eq!(source_of(0, 3, 2), None);
}

/// Every sample type a driver can run.
const ALL: [AsioSample; 8] = [
    AsioSample::Int16,
    AsioSample::Int24,
    AsioSample::Int32,
    AsioSample::Float32,
    AsioSample::Int32In16,
    AsioSample::Int32In18,
    AsioSample::Int32In20,
    AsioSample::Int32In24,
];

/// #233 (the SoundGrid loop check): a sample reads back as the value it
/// was written from, within one step of its type's full scale.
#[test]
fn each_type_reads_back_what_it_wrote() {
    for sample in ALL {
        let step = sample.full_scale().map_or(0.0, |full| 1.0 / full);
        for x in [0.5f32, -0.5, 0.999, -1.0, 0.0, 0.123_456, -0.000_1] {
            let mut bytes = vec![0u8; sample.bytes()];
            sample.encode(&[x, 0.0], 0, &mut bytes);
            let back = sample.decode(&bytes);
            assert!(
                f64::from((back - x).abs()) <= step,
                "{sample:?}: {x} read back as {back}"
            );
        }
    }
}

/// The exact values: an integer over its symmetric full scale, a 24-bit
/// word sign-extended, a float unclamped, a non-finite float silent.
#[test]
fn each_type_decodes_exactly() {
    let f = |v: f64, full: f64| (v / full) as f32;
    assert_eq!(
        AsioSample::Int16.decode(&16_384i16.to_le_bytes()),
        f(16_384.0, 32_767.0)
    );
    assert_eq!(AsioSample::Int16.decode(&(-32_767i16).to_le_bytes()), -1.0);
    assert_eq!(
        AsioSample::Int24.decode(&[0x00, 0x00, 0x40]),
        f(4_194_304.0, 8_388_607.0)
    );
    assert_eq!(AsioSample::Int24.decode(&[0x01, 0x00, 0x80]), -1.0);
    assert_eq!(
        AsioSample::Int24.decode(&[0xFF, 0xFF, 0xFF]),
        f(-1.0, 8_388_607.0),
        "sign-extended"
    );
    assert_eq!(AsioSample::Int32.decode(&i32::MAX.to_le_bytes()), 1.0);
    assert_eq!(
        AsioSample::Int32.decode(&(-1_073_741_824i32).to_le_bytes()),
        f(-1_073_741_824.0, 2_147_483_647.0)
    );
    assert_eq!(
        AsioSample::Int32In24.decode(&(-4_194_304i32).to_le_bytes()),
        f(-4_194_304.0, 8_388_607.0)
    );
    assert_eq!(AsioSample::Int32In20.decode(&524_287i32.to_le_bytes()), 1.0);
    assert_eq!(
        AsioSample::Int32In18.decode(&65_536i32.to_le_bytes()),
        f(65_536.0, 131_071.0)
    );
    assert_eq!(
        AsioSample::Int32In16.decode(&(-32_767i32).to_le_bytes()),
        -1.0
    );
    assert_eq!(AsioSample::Float32.decode(&0.25f32.to_le_bytes()), 0.25);
    assert_eq!(
        AsioSample::Float32.decode(&1.5f32.to_le_bytes()),
        1.5,
        "never clamped"
    );
    assert_eq!(AsioSample::Float32.decode(&f32::NAN.to_le_bytes()), 0.0);
    assert_eq!(
        AsioSample::Float32.decode(&f32::NEG_INFINITY.to_le_bytes()),
        0.0
    );
}

/// `read_channel` fills its slot of each frame and nothing else; the
/// frames its buffer does not hold read as silence.
#[test]
fn read_channel_fills_its_slot_and_zeroes_the_missing_frames() {
    // Three frames of three channels, every slot marked.
    let mut dst = vec![9.0f32; 9];
    let src: Vec<u8> = [16_384i16, -16_384]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    read_channel(AsioSample::Int16, &src, &mut dst, 1, 3);
    let half = (16_384.0f64 / 32_767.0) as f32;
    assert_eq!(dst, vec![9.0, half, 9.0, 9.0, -half, 9.0, 9.0, 0.0, 9.0]);
    // A missing buffer is silence in its slot only.
    let mut dst = vec![9.0f32; 4];
    read_channel(AsioSample::Int24, &[], &mut dst, 0, 2);
    assert_eq!(dst, vec![0.0, 9.0, 0.0, 9.0]);
    // A buffer longer than the frames: only what fits.
    let mut dst = vec![9.0f32; 2];
    let src: Vec<u8> = [0.25f32, 0.5, 0.75]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    read_channel(AsioSample::Float32, &src, &mut dst, 1, 2);
    assert_eq!(dst, vec![9.0, 0.25]);
}
