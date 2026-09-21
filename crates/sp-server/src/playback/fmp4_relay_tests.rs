//! Unit tests for the pure fMP4 box splitter + broadcast relay (#178). All
//! Linux-runnable — synthetic box streams, no ffmpeg child.

use super::*;

/// Build a 32-bit-size top-level box: `[size:u32][cc][payload]`.
fn box32(cc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let total = 8 + payload.len();
    let mut v = Vec::with_capacity(total);
    v.extend_from_slice(&(total as u32).to_be_bytes());
    v.extend_from_slice(cc);
    v.extend_from_slice(payload);
    v
}

/// Build a 64-bit-largesize top-level box: `[1:u32][cc][largesize:u64][payload]`.
fn box64(cc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let total = 16 + payload.len();
    let mut v = Vec::with_capacity(total);
    v.extend_from_slice(&1u32.to_be_bytes());
    v.extend_from_slice(cc);
    v.extend_from_slice(&(total as u64).to_be_bytes());
    v.extend_from_slice(payload);
    v
}

#[test]
fn header_needs_more_until_8_bytes() {
    assert_eq!(parse_box_header(&[]), BoxHeader::NeedMore);
    assert_eq!(parse_box_header(&[0, 0, 0]), BoxHeader::NeedMore);
    // 7 bytes: still short of the 8-byte minimum header.
    assert_eq!(
        parse_box_header(&[0, 0, 0, 16, b'f', b't', b'y']),
        BoxHeader::NeedMore
    );
}

#[test]
fn header_parses_32bit_size_and_fourcc_exactly() {
    let b = box32(b"ftyp", &[9, 9, 9, 9]); // total 12
    match parse_box_header(&b) {
        BoxHeader::Parsed {
            header_len,
            total_len,
            four_cc,
        } => {
            assert_eq!(header_len, 8);
            assert_eq!(total_len, 12);
            assert_eq!(&four_cc, b"ftyp");
        }
        other => panic!("expected Parsed, got {other:?}"),
    }
}

#[test]
fn header_parses_64bit_largesize() {
    let b = box64(b"mdat", &[1, 2, 3, 4, 5, 6]); // total 22
    // Only 15 bytes → still NeedMore (largesize occupies bytes 8..16).
    assert_eq!(parse_box_header(&b[..15]), BoxHeader::NeedMore);
    match parse_box_header(&b) {
        BoxHeader::Parsed {
            header_len,
            total_len,
            four_cc,
        } => {
            assert_eq!(header_len, 16);
            assert_eq!(total_len, 22);
            assert_eq!(&four_cc, b"mdat");
        }
        other => panic!("expected Parsed, got {other:?}"),
    }
}

#[test]
fn header_accepts_minimal_boxes_at_the_size_boundaries() {
    // A header-only 32-bit box (size exactly 8) is valid — pins `size32 < 8`.
    let b = box32(b"free", &[]);
    assert_eq!(b.len(), 8);
    match parse_box_header(&b) {
        BoxHeader::Parsed {
            header_len,
            total_len,
            ..
        } => {
            assert_eq!(header_len, 8);
            assert_eq!(total_len, 8);
        }
        other => panic!("size-8 box must parse, got {other:?}"),
    }
    // A header-only 64-bit box (largesize exactly 16) is valid — pins `large < 16`.
    let b = box64(b"mdat", &[]);
    assert_eq!(b.len(), 16);
    match parse_box_header(&b) {
        BoxHeader::Parsed {
            header_len,
            total_len,
            ..
        } => {
            assert_eq!(header_len, 16);
            assert_eq!(total_len, 16);
        }
        other => panic!("largesize-16 box must parse, got {other:?}"),
    }
}

#[test]
fn header_flags_corrupt_sizes() {
    // size 0 ("to EOF") and size < 8 are unusable for a fragmented stream.
    assert_eq!(
        parse_box_header(&[0, 0, 0, 0, b'f', b'r', b'e', b'e']),
        BoxHeader::Corrupt
    );
    assert_eq!(
        parse_box_header(&[0, 0, 0, 7, b'f', b'r', b'e', b'e']),
        BoxHeader::Corrupt
    );
    // 64-bit largesize smaller than its own 16-byte header is corrupt.
    let mut b = 1u32.to_be_bytes().to_vec();
    b.extend_from_slice(b"mdat");
    b.extend_from_slice(&8u64.to_be_bytes());
    assert_eq!(parse_box_header(&b), BoxHeader::Corrupt);
}

#[test]
fn emits_init_after_ftyp_moov_then_one_fragment_per_moof_mdat() {
    let mut s = BoxSplitter::new();
    let mut stream = Vec::new();
    stream.extend(box32(b"ftyp", b"isom"));
    stream.extend(box32(b"moov", b"MOOVDATA"));
    stream.extend(box32(b"moof", b"MOOF1"));
    stream.extend(box32(b"mdat", b"MDAT1"));
    stream.extend(box32(b"moof", b"MOOF2"));
    stream.extend(box32(b"mdat", b"MDAT2"));

    let chunks = s.push(&stream);
    // Init = ftyp + moov concatenated; then two fragments.
    let mut expected_init = box32(b"ftyp", b"isom");
    expected_init.extend(box32(b"moov", b"MOOVDATA"));
    let mut frag1 = box32(b"moof", b"MOOF1");
    frag1.extend(box32(b"mdat", b"MDAT1"));
    let mut frag2 = box32(b"moof", b"MOOF2");
    frag2.extend(box32(b"mdat", b"MDAT2"));

    assert_eq!(
        chunks,
        vec![
            RelayChunk::Init(expected_init),
            RelayChunk::Fragment(frag1),
            RelayChunk::Fragment(frag2),
        ]
    );
}

#[test]
fn reassembles_boxes_split_across_read_boundaries() {
    let mut s = BoxSplitter::new();
    let mut stream = Vec::new();
    stream.extend(box32(b"ftyp", b"isom"));
    stream.extend(box32(b"moov", b"MOOVDATA"));
    stream.extend(box32(b"moof", b"MOOF1"));
    stream.extend(box32(b"mdat", b"MDAT1"));

    // Feed one byte at a time — the header AND payload split on every boundary.
    let mut chunks = Vec::new();
    for b in &stream {
        chunks.extend(s.push(&[*b]));
    }
    let mut expected_init = box32(b"ftyp", b"isom");
    expected_init.extend(box32(b"moov", b"MOOVDATA"));
    let mut frag1 = box32(b"moof", b"MOOF1");
    frag1.extend(box32(b"mdat", b"MDAT1"));
    assert_eq!(
        chunks,
        vec![RelayChunk::Init(expected_init), RelayChunk::Fragment(frag1)]
    );
}

#[test]
fn styp_and_sidx_ride_with_the_following_fragment() {
    let mut s = BoxSplitter::new();
    let mut stream = Vec::new();
    stream.extend(box32(b"ftyp", b"isom"));
    stream.extend(box32(b"moov", b"M"));
    // A fragment prefixed by styp + sidx must be ONE fragment, closed by mdat.
    stream.extend(box32(b"styp", b"msdh"));
    stream.extend(box32(b"sidx", b"SIDX"));
    stream.extend(box32(b"moof", b"MOOF"));
    stream.extend(box32(b"mdat", b"MDAT"));

    let chunks = s.push(&stream);
    let mut frag = box32(b"styp", b"msdh");
    frag.extend(box32(b"sidx", b"SIDX"));
    frag.extend(box32(b"moof", b"MOOF"));
    frag.extend(box32(b"mdat", b"MDAT"));
    let init = {
        let mut i = box32(b"ftyp", b"isom");
        i.extend(box32(b"moov", b"M"));
        i
    };
    assert_eq!(
        chunks,
        vec![RelayChunk::Init(init), RelayChunk::Fragment(frag)]
    );
}

#[test]
fn handles_64bit_mdat_fragment() {
    let mut s = BoxSplitter::new();
    let mut stream = Vec::new();
    stream.extend(box32(b"ftyp", b"isom"));
    stream.extend(box32(b"moov", b"M"));
    stream.extend(box32(b"moof", b"MOOF"));
    stream.extend(box64(b"mdat", &[7u8; 40])); // large payload via 64-bit size

    let chunks = s.push(&stream);
    let mut frag = box32(b"moof", b"MOOF");
    frag.extend(box64(b"mdat", &[7u8; 40]));
    assert!(matches!(&chunks[0], RelayChunk::Init(_)));
    assert_eq!(chunks[1], RelayChunk::Fragment(frag));
    assert_eq!(chunks.len(), 2);
}

#[test]
fn corrupt_size_poisons_and_stops_emitting() {
    let mut s = BoxSplitter::new();
    // A zero-size box up front cannot be parsed → nothing ever emitted, and no
    // infinite loop.
    let chunks = s.push(&[0, 0, 0, 0, b'f', b'r', b'e', b'e', 1, 2, 3, 4]);
    assert!(chunks.is_empty());
    // A subsequent valid stream is ignored (splitter stays poisoned).
    let mut more = box32(b"ftyp", b"isom");
    more.extend(box32(b"moov", b"M"));
    assert!(s.push(&more).is_empty());
}

#[test]
fn partial_box_waits_without_consuming() {
    let mut s = BoxSplitter::new();
    // ftyp complete; moov header says 8+8=16 bytes but only 4 payload bytes given.
    let mut stream = box32(b"ftyp", b"isom");
    let mut moov = 16u32.to_be_bytes().to_vec();
    moov.extend_from_slice(b"moov");
    moov.extend_from_slice(&[1, 2, 3, 4]); // only 4 of 8 payload bytes
    stream.extend(moov);
    // No init yet — moov is incomplete.
    assert!(s.push(&stream).is_empty());
    // Deliver the remaining 4 payload bytes → init completes.
    let chunks = s.push(&[5, 6, 7, 8]);
    assert_eq!(chunks.len(), 1);
    assert!(matches!(&chunks[0], RelayChunk::Init(_)));
}

#[test]
fn relay_caches_init_and_broadcasts_fragments_to_current_viewers() {
    let relay = FragmentRelay::new(8);
    assert!(relay.init().is_none());
    assert_eq!(relay.viewer_count(), 0);

    relay.ingest(RelayChunk::Init(vec![1, 2, 3]));
    assert_eq!(relay.init().as_deref(), Some(&[1u8, 2, 3][..]));

    // A viewer that subscribes before a fragment is sent receives it.
    let mut rx = relay.subscribe();
    assert_eq!(relay.viewer_count(), 1);
    relay.ingest(RelayChunk::Fragment(vec![9, 9]));
    assert_eq!(rx.try_recv().unwrap().as_ref(), &[9u8, 9][..]);
}

// ── #178 review fixes (items 12, 14, 17) ─────────────────────────────────────

#[test]
fn box_size_at_16mib_is_accepted_but_one_over_poisons_32bit() {
    let cap = 16 * 1024 * 1024u32;
    // Exactly 16 MiB is accepted (header parses, waits for the body).
    let mut hdr = cap.to_be_bytes().to_vec();
    hdr.extend_from_slice(b"moov");
    match parse_box_header(&hdr) {
        BoxHeader::Parsed { total_len, .. } => assert_eq!(total_len, cap as u64),
        other => panic!("16 MiB box must parse, got {other:?}"),
    }
    let mut s = BoxSplitter::new();
    assert!(s.push(&hdr).is_empty(), "waiting for the 16 MiB body");
    assert!(!s.is_poisoned(), "exactly 16 MiB is accepted");
    // 16 MiB + 1 poisons at the header.
    let mut over = (cap + 1).to_be_bytes().to_vec();
    over.extend_from_slice(b"moov");
    assert_eq!(parse_box_header(&over), BoxHeader::Corrupt);
    let mut s2 = BoxSplitter::new();
    assert!(s2.push(&over).is_empty());
    assert!(s2.is_poisoned(), "16 MiB + 1 poisons (32-bit form)");
}

#[test]
fn box_size_at_16mib_is_accepted_but_one_over_poisons_largesize() {
    let cap = 16 * 1024 * 1024u64;
    let mut hdr = 1u32.to_be_bytes().to_vec(); // size32 == 1 → largesize
    hdr.extend_from_slice(b"moov");
    hdr.extend_from_slice(&cap.to_be_bytes());
    match parse_box_header(&hdr) {
        BoxHeader::Parsed { total_len, .. } => assert_eq!(total_len, cap),
        other => panic!("16 MiB largesize must parse, got {other:?}"),
    }
    let mut ok = BoxSplitter::new();
    assert!(ok.push(&hdr).is_empty(), "waiting for the 16 MiB body");
    assert!(!ok.is_poisoned(), "exactly 16 MiB largesize is accepted");
    let mut over = 1u32.to_be_bytes().to_vec();
    over.extend_from_slice(b"moov");
    over.extend_from_slice(&(cap + 1).to_be_bytes());
    assert_eq!(parse_box_header(&over), BoxHeader::Corrupt);
    let mut s = BoxSplitter::new();
    assert!(s.push(&over).is_empty());
    assert!(s.is_poisoned(), "16 MiB + 1 poisons (largesize form)");
}

#[test]
fn init_accumulator_over_16mib_poisons() {
    let mut s = BoxSplitter::new();
    // A 16 MiB 'ftyp' box: accepted (exactly at the cap), accumulates into init.
    let big = box32(b"ftyp", &vec![0u8; 16 * 1024 * 1024 - 8]); // total = 16 MiB
    assert!(s.push(&big).is_empty(), "no init emitted yet (no moov)");
    assert!(!s.is_poisoned(), "a 16 MiB box is accepted");
    // One more small box tips the init accumulator over 16 MiB → poison.
    let more = box32(b"free", &[0u8; 4]);
    assert!(s.push(&more).is_empty());
    assert!(s.is_poisoned(), "init accumulator over 16 MiB poisons");
}

#[test]
fn fragment_accumulator_over_16mib_poisons() {
    let mut s = BoxSplitter::new();
    // Complete the init so subsequent boxes accumulate into the fragment.
    let mut init = box32(b"ftyp", b"isom");
    init.extend(box32(b"moov", b"M"));
    assert!(matches!(s.push(&init).as_slice(), [RelayChunk::Init(_)]));
    // A 16 MiB 'moof' (no mdat yet) accumulates into frag — accepted at the cap.
    let big = box32(b"moof", &vec![0u8; 16 * 1024 * 1024 - 8]);
    assert!(s.push(&big).is_empty(), "no fragment yet (no mdat)");
    assert!(!s.is_poisoned(), "16 MiB moof accepted");
    // One more box tips the fragment accumulator over 16 MiB → poison.
    assert!(s.push(&box32(b"free", &[0u8; 4])).is_empty());
    assert!(s.is_poisoned(), "fragment accumulator over 16 MiB poisons");
}

#[test]
fn reset_clears_cached_init_so_a_late_joiner_waits_for_the_new_one() {
    let relay = FragmentRelay::new(8);
    relay.ingest(RelayChunk::Init(b"INIT_A".to_vec()));
    assert_eq!(relay.init().as_deref(), Some(&b"INIT_A"[..]));
    relay.reset();
    assert!(relay.init().is_none(), "reset clears the cached init");
    // A new child's init replaces it (a late joiner now gets the NEW one).
    relay.ingest(RelayChunk::Init(b"INIT_B".to_vec()));
    assert_eq!(relay.init().as_deref(), Some(&b"INIT_B"[..]));
}

#[test]
fn close_drops_the_sender_so_viewers_see_closed_and_clears_init() {
    use tokio::sync::broadcast::error::TryRecvError;
    let relay = FragmentRelay::new(8);
    relay.ingest(RelayChunk::Init(vec![1, 2, 3]));
    let mut rx = relay.subscribe();
    assert!(relay.init().is_some());
    relay.close();
    assert!(relay.init().is_none(), "close clears the cached init");
    // The connected viewer's receiver now reports Closed (its sender dropped).
    match rx.try_recv() {
        Err(TryRecvError::Closed) => {}
        other => panic!("expected Closed after relay.close(), got {other:?}"),
    }
    // A fresh subscriber uses the new channel and simply has no data yet.
    let mut rx2 = relay.subscribe();
    assert!(matches!(rx2.try_recv(), Err(TryRecvError::Empty)));
    relay.ingest(RelayChunk::Fragment(vec![7]));
    assert_eq!(rx2.try_recv().unwrap().as_ref(), &[7u8][..]);
}

#[test]
fn relay_drops_a_lagging_viewer_rather_than_blocking() {
    use tokio::sync::broadcast::error::TryRecvError;
    let relay = FragmentRelay::new(2); // tiny backlog
    let mut rx = relay.subscribe();
    // Send more fragments than the backlog holds without draining rx.
    for i in 0..5u8 {
        relay.ingest(RelayChunk::Fragment(vec![i]));
    }
    // The lagging receiver reports Lagged (dropped), never blocks the sender.
    match rx.try_recv() {
        Err(TryRecvError::Lagged(n)) => assert!(n >= 1),
        other => panic!("expected Lagged, got {other:?}"),
    }
    // After the lag it resyncs to the most recent fragments still buffered.
    assert!(rx.try_recv().is_ok());
}

#[test]
fn produced_ms_advances_500_per_fragment_and_resets_on_init() {
    // #184 round F: produced_ms = (fragments since the last Init) × 500 ms — the
    // media time the child has produced, for the lag beacon. Exact values kill
    // the ×N multiplier mutant AND the reset-on-Init mutant.
    let relay = FragmentRelay::new(4);
    assert_eq!(
        relay.produced_ms(),
        0,
        "nothing produced before any fragment"
    );
    relay.ingest(RelayChunk::Init(vec![1]));
    assert_eq!(relay.produced_ms(), 0, "Init leaves the counter at 0");
    relay.ingest(RelayChunk::Fragment(vec![9]));
    assert_eq!(relay.produced_ms(), 500, "one fragment = 500 ms of media");
    relay.ingest(RelayChunk::Fragment(vec![9]));
    assert_eq!(relay.produced_ms(), 1000, "two fragments = 1000 ms");
    // A new child's Init resets the media timeline (and the counter) to 0.
    relay.ingest(RelayChunk::Init(vec![2]));
    assert_eq!(relay.produced_ms(), 0, "a new Init resets produced_ms");
    relay.ingest(RelayChunk::Fragment(vec![9]));
    assert_eq!(relay.produced_ms(), 500, "and counting resumes from 0");
}

#[test]
fn relay_at_the_production_backlog_lags_a_viewer_five_behind() {
    // #184 round F: at the shipped backlog of 4 fragments (RELAY_CAPACITY, = 2 s),
    // a viewer that falls 5 fragments behind without draining is dropped
    // (`Lagged`) and resyncs — it can never sit a full 32-s backlog behind the
    // wall. Same shape as the `new(2)` tiny-backlog test above, at the real cap.
    use tokio::sync::broadcast::error::TryRecvError;
    let relay = FragmentRelay::new(4);
    let mut rx = relay.subscribe();
    for i in 0..5u8 {
        relay.ingest(RelayChunk::Fragment(vec![i]));
    }
    match rx.try_recv() {
        Err(TryRecvError::Lagged(n)) => assert!(n >= 1, "at least one fragment dropped"),
        other => panic!("expected Lagged at cap 4 with 5 sent, got {other:?}"),
    }
    // It resyncs to the newest fragments still in the 4-deep backlog.
    assert!(rx.try_recv().is_ok());
}
