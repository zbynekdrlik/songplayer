//! #233: the ASIO host's pipe protocol — every frame round-trips, a
//! malformed or oversized one is refused (its length before anything is
//! allocated), and the reader tells a clean end from a cut frame.

use std::sync::Arc;

use tokio::io::AsyncWriteExt;

use super::*;
use crate::audio_host::remote_clock::Anchor;
use crate::audio_host::wire_status::WireSnapshot;
use crate::playback::asio_out::{AsioSnapshot, AsioSpec, AsioStatus};
use crate::playback::audio_out_block::ProgramBlock;

fn spec() -> AsioSpec {
    AsioSpec {
        id: "out-2".into(),
        driver: "Waves SoundGrid".into(),
        channels: [6, 7],
        delay_ms: 40,
        loop_check: true,
    }
}

fn ramp() -> Arc<[f32]> {
    (0..3200).map(|i| i as f32 * 0.25 - 400.0).collect()
}

fn anchor() -> Anchor {
    Anchor {
        timeline_100ns: 17_900_000_000_000_007,
        mono_100ns: -123_456_789,
    }
}

fn round_trip(frame: &Frame) -> Frame {
    let bytes = encode(frame);
    let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    assert_eq!(len, bytes.len() - 4, "the prefix is the body's length");
    decode_body(&bytes[4..]).unwrap()
}

#[test]
fn control_frames_round_trip() {
    for c in [
        Control::Hello {
            protocol: PROTOCOL,
            anchor: anchor(),
        },
        Control::Add {
            handle: 9,
            spec: spec(),
        },
        Control::Remove { handle: u64::MAX },
    ] {
        let frame = Frame::Control(c);
        assert_eq!(round_trip(&frame), frame);
    }
}

#[test]
fn a_block_round_trips_with_its_samples_or_as_silence() {
    let with = Frame::Block(BlockFrame {
        handle: 0x0102_0304_0506_0708,
        due_100ns: -17_900_000_000_000_123,
        anchor: anchor(),
        samples: Some(ramp()),
        substituted: false,
    });
    let bytes = encode(&with);
    assert_eq!(bytes.len(), 4 + 1 + 33 + 12_800);
    assert_eq!(bytes.len(), BLOCK_FRAME_BYTES);
    assert_eq!(
        BLOCK_FRAME_BYTES,
        PREFIX_BYTES + 1 + BLOCK_HEAD_BYTES + SAMPLES_BYTES
    );
    assert_eq!(round_trip(&with), with);
    let silence = Frame::Block(BlockFrame {
        handle: 3,
        due_100ns: 42,
        anchor: Anchor {
            timeline_100ns: 9,
            mono_100ns: 8,
        },
        samples: None,
        substituted: true,
    });
    assert_eq!(encode(&silence).len(), 4 + 1 + 33);
    assert_eq!(round_trip(&silence), silence);
    let plain = Frame::Block(BlockFrame {
        substituted: false,
        ..BlockFrame::of(3, ProgramBlock::silence(42))
    });
    assert_eq!(round_trip(&plain), plain);
}

/// A block's samples that are not one program block travel as silence,
/// marked substituted; one program block travels whole.
#[test]
fn a_block_of_the_wrong_length_travels_as_marked_silence() {
    let short = ProgramBlock {
        due_100ns: 5,
        samples: Some(vec![0.5; 3198].into()),
        substituted: false,
    };
    let f = BlockFrame::of(1, short);
    assert_eq!(
        (f.handle, f.due_100ns, f.anchor, f.samples, f.substituted),
        (1, 5, Anchor::default(), None, true)
    );
    let whole = ProgramBlock {
        due_100ns: 6,
        samples: Some(ramp()),
        substituted: false,
    };
    let f = BlockFrame::of(1, whole);
    assert_eq!((f.samples, f.substituted), (Some(ramp()), false));
    let marked = ProgramBlock {
        substituted: true,
        ..ProgramBlock::silence(7)
    };
    let f = BlockFrame::of(1, marked);
    assert_eq!((f.samples, f.substituted), (None, true));
    let f = BlockFrame::of(1, ProgramBlock::silence(8));
    assert_eq!((f.samples, f.substituted), (None, false));
}

#[test]
fn reports_round_trip() {
    let snapshot = AsioSnapshot {
        state: "running",
        reason: None,
        status: AsioStatus {
            driver: "DVS".into(),
            driver_rate: 96_000,
            sample_type: "Int32LSB",
            ppm: 1.25,
            ..AsioStatus::default()
        },
        blocks_sent: 3,
        blocks_dropped: 1,
    };
    for r in [
        Report::Status {
            handle: 2,
            snapshot: Box::new(WireSnapshot::of(&snapshot, Some("it panicked".into()))),
        },
        Report::Log {
            level: HostLevel::Warn,
            line: "asio output: a resync".into(),
        },
        Report::Alive,
        Report::Ending {
            why: "a frame before the hello".into(),
        },
    ] {
        let frame = Frame::Report(r);
        assert_eq!(round_trip(&frame), frame);
    }
}

#[test]
fn the_known_flags_are_the_two_flags() {
    assert_eq!(KNOWN_FLAGS, FLAG_SAMPLES | FLAG_SUBSTITUTED);
    assert_eq!((FLAG_SAMPLES, FLAG_SUBSTITUTED), (1, 2));
}

/// A block body with its flags byte set to `flags` and `tail` after it.
fn block_body(flags: u8, tail: usize) -> Vec<u8> {
    let mut b = vec![2u8];
    b.extend_from_slice(&[0u8; 32]);
    b.push(flags);
    b.extend(std::iter::repeat_n(0u8, tail));
    b
}

#[test]
fn a_malformed_block_is_refused() {
    let short = [2u8; 33];
    assert_eq!(
        decode_body(&short),
        Err(ProtoError::BadBlock("its head is short"))
    );
    assert_eq!(
        decode_body(&block_body(0x04, 0)),
        Err(ProtoError::BadBlock("an unknown flag"))
    );
    assert_eq!(
        decode_body(&block_body(0x81, 12_800)),
        Err(ProtoError::BadBlock("an unknown flag"))
    );
    assert_eq!(
        decode_body(&block_body(0x01, 12_796)),
        Err(ProtoError::BadBlock("not one block of samples"))
    );
    assert_eq!(
        decode_body(&block_body(0x01, 12_804)),
        Err(ProtoError::BadBlock("not one block of samples"))
    );
    assert_eq!(
        decode_body(&block_body(0x02, 4)),
        Err(ProtoError::BadBlock("samples with no samples flag"))
    );
    // Every flag this version knows decodes.
    for (flags, tail) in [(0x00, 0), (0x01, 12_800), (0x02, 0), (0x03, 12_800)] {
        let Ok(Frame::Block(b)) = decode_body(&block_body(flags, tail)) else {
            panic!("flags {flags} refused");
        };
        assert_eq!(
            (b.samples.is_some(), b.substituted),
            (flags & 1 != 0, flags & 2 != 0),
            "flags {flags}"
        );
    }
}

#[test]
fn a_frame_of_no_known_kind_or_json_is_refused() {
    assert_eq!(decode_body(&[]), Err(ProtoError::Empty));
    assert_eq!(decode_body(&[9, 1, 2]), Err(ProtoError::UnknownKind(9)));
    assert_eq!(decode_body(&[0]), Err(ProtoError::UnknownKind(0)));
    for kind in [1u8, 3] {
        let mut body = vec![kind];
        body.extend_from_slice(br#"{"op":"launch","handle":1}"#);
        assert!(
            matches!(decode_body(&body), Err(ProtoError::BadJson(_))),
            "kind {kind}"
        );
    }
    // A control frame is no report and the other way round.
    let mut body = vec![3u8];
    body.extend_from_slice(br#"{"op":"remove","handle":1}"#);
    assert!(matches!(decode_body(&body), Err(ProtoError::BadJson(_))));
}

#[test]
fn a_frame_length_is_checked_before_anything_is_read() {
    assert_eq!(check_len(0), Err(ProtoError::Empty));
    assert_eq!(check_len(1), Ok(1));
    assert_eq!(check_len(MAX_FRAME_BYTES), Ok(MAX_FRAME_BYTES));
    assert_eq!(
        check_len(MAX_FRAME_BYTES + 1),
        Err(ProtoError::TooLarge(65_537))
    );
    assert_eq!(MAX_FRAME_BYTES, 65_536);
}

/// The reader refuses an oversized length at once (it reads and allocates
/// no body, the pipe still open), ends cleanly between frames, and calls a
/// frame cut inside its header or body truncated.
#[tokio::test]
async fn the_reader_bounds_a_frame_and_tells_a_clean_end_from_a_cut_one() {
    let (mut tx, mut rx) = tokio::io::duplex(1 << 16);
    tx.write_all(&(1u32 << 31).to_le_bytes()).await.unwrap();
    assert_eq!(
        read_frame(&mut rx).await,
        Err(ProtoError::TooLarge(1 << 31))
    );
    drop(tx);

    let (mut tx, mut rx) = tokio::io::duplex(1 << 16);
    let hello = Frame::Control(Control::Hello {
        protocol: PROTOCOL,
        anchor: anchor(),
    });
    let block = Frame::Block(BlockFrame::of(4, ProgramBlock::silence(1)));
    write_frame(&mut tx, &hello).await.unwrap();
    write_frame(&mut tx, &block).await.unwrap();
    drop(tx);
    assert_eq!(read_frame(&mut rx).await, Ok(Some(hello)));
    assert_eq!(read_frame(&mut rx).await, Ok(Some(block)));
    assert_eq!(read_frame(&mut rx).await, Ok(None));

    let (mut tx, mut rx) = tokio::io::duplex(1 << 16);
    tx.write_all(&[5, 0]).await.unwrap();
    drop(tx);
    assert_eq!(read_frame(&mut rx).await, Err(ProtoError::Truncated));

    let (mut tx, mut rx) = tokio::io::duplex(1 << 16);
    tx.write_all(&[5, 0, 0, 0, 1, 2]).await.unwrap();
    drop(tx);
    assert_eq!(read_frame(&mut rx).await, Err(ProtoError::Truncated));

    let (mut tx, mut rx) = tokio::io::duplex(1 << 16);
    tx.write_all(&[0, 0, 0, 0]).await.unwrap();
    assert_eq!(read_frame(&mut rx).await, Err(ProtoError::Empty));
}

/// A pipe that fails to read.
struct Broken;

impl tokio::io::AsyncRead for Broken {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "the pipe broke",
        )))
    }
}

/// A read that fails is an error, never a clean end.
#[tokio::test]
async fn a_failed_read_is_an_error_not_an_end() {
    assert_eq!(
        read_frame(&mut Broken).await,
        Err(ProtoError::Io("the pipe broke".into()))
    );
}

#[test]
fn a_log_line_is_one_line_cut_on_a_character() {
    assert_eq!(log_line(b"a line\r\n"), "a line");
    assert_eq!(log_line(b"x"), "x");
    assert_eq!(log_line(b""), "");
    let ascii = [b'a'; MAX_LOG_LINE_BYTES + 10];
    assert_eq!(log_line(&ascii).len(), MAX_LOG_LINE_BYTES);
    let exact = [b'b'; MAX_LOG_LINE_BYTES];
    assert_eq!(log_line(&exact).len(), MAX_LOG_LINE_BYTES);
    // "č" is two bytes: one that would end past the cap is left out whole.
    let mut wide = vec![b'c'; MAX_LOG_LINE_BYTES - 1];
    wide.extend_from_slice("čď".as_bytes());
    let cut = log_line(&wide);
    assert_eq!(cut.len(), MAX_LOG_LINE_BYTES - 1);
    assert!(cut.ends_with('c'));
    let mut fits = vec![b'c'; MAX_LOG_LINE_BYTES - 2];
    fits.extend_from_slice("čď".as_bytes());
    let cut = log_line(&fits);
    assert_eq!(cut.len(), MAX_LOG_LINE_BYTES);
    assert!(cut.ends_with('č'));
    assert_eq!(log_line(&[0xff, b'a']), "\u{fffd}a");
}
