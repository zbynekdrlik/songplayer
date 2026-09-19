//! Fragmented-MP4 box splitter + broadcast relay for the live preview stream
//! (#178).
//!
//! The bundled `ffmpeg` child writes a fragmented MP4 on its stdout:
//! `ftyp` + `moov` (the **init segment** an MSE `SourceBuffer` needs first),
//! then a repeating run of media fragments (`[styp?] [sidx?] moof mdat`). This
//! module owns two things, split so the parsing is pure and Linux-tested:
//!
//! * [`BoxSplitter`] — a pure, allocation-bounded state machine that turns the
//!   child's raw byte stream (arriving in arbitrary-sized reads) into
//!   [`RelayChunk`]s: exactly one [`RelayChunk::Init`] once the `moov` is seen,
//!   then one [`RelayChunk::Fragment`] per completed media fragment. It handles
//!   box headers split across read boundaries and 64-bit `largesize` boxes.
//! * [`FragmentRelay`] — the broadcast glue: caches the init segment and
//!   re-broadcasts every fragment over a `tokio::sync::broadcast` channel so a
//!   late-joining viewer receives the init segment then the next fragment
//!   (every media fragment begins on a keyframe under `+frag_keyframe`), and a
//!   viewer that falls behind is dropped by the broadcast channel rather than
//!   ever blocking the reader.

use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

/// One top-level chunk emitted by [`BoxSplitter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayChunk {
    /// The init segment (`ftyp` + `moov`), emitted exactly once.
    Init(Vec<u8>),
    /// One media fragment (`[styp?] [sidx?] moof mdat`), keyframe-aligned.
    Fragment(Vec<u8>),
}

/// ISO-BMFF box-length header for the buffered prefix, if a full header is
/// present. Returns `(header_len, total_box_len, four_cc)`.
///
/// * A 32-bit `size` of `1` selects the 64-bit `largesize` that follows the
///   type (header grows to 16 bytes).
/// * `size == 0` ("box extends to EOF") and any `size` smaller than its own
///   header are malformed for a fragmented-MP4 stream — reported as
///   [`BoxHeader::Corrupt`] so the caller stops rather than looping forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoxHeader {
    /// Not enough bytes buffered yet to read the full header.
    NeedMore,
    /// A parsed header: `header_len` bytes of header, `total_len` bytes total.
    Parsed {
        header_len: usize,
        total_len: u64,
        four_cc: [u8; 4],
    },
    /// A structurally impossible length — the stream is unusable.
    Corrupt,
}

/// A single box — or the init / fragment accumulator — larger than this is
/// treated as corrupt (#178 item 14). The child's fMP4 boxes are tiny (a 640×360
/// fragment is tens of KB), so a declared size above 16 MiB is a corrupt or
/// hostile stream, never legitimate; poisoning stops the splitter rather than
/// trusting a size up to 2^64 and allocating gigabytes.
const MAX_BOX_BYTES: u64 = 16 * 1024 * 1024;

/// Whether `len` bytes exceed the 16 MiB box/accumulator cap (exactly the cap is
/// accepted). One helper so the box-size and accumulator checks share the bound.
fn exceeds_box_cap(len: u64) -> bool {
    len > MAX_BOX_BYTES
}

/// Parse the box header at the front of `buf` without consuming it.
fn parse_box_header(buf: &[u8]) -> BoxHeader {
    if buf.len() < 8 {
        return BoxHeader::NeedMore;
    }
    let size32 = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let four_cc = [buf[4], buf[5], buf[6], buf[7]];
    if size32 == 1 {
        // 64-bit largesize occupies bytes 8..16.
        if buf.len() < 16 {
            return BoxHeader::NeedMore;
        }
        let large = u64::from_be_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        if large < 16 || exceeds_box_cap(large) {
            return BoxHeader::Corrupt;
        }
        BoxHeader::Parsed {
            header_len: 16,
            total_len: large,
            four_cc,
        }
    } else if size32 < 8 {
        // 0 = "to EOF" (never in fragmented output); < 8 cannot hold a header.
        BoxHeader::Corrupt
    } else if exceeds_box_cap(size32 as u64) {
        BoxHeader::Corrupt
    } else {
        BoxHeader::Parsed {
            header_len: 8,
            total_len: size32 as u64,
            four_cc,
        }
    }
}

/// Pure state machine that groups a fragmented-MP4 byte stream into an init
/// segment and keyframe-aligned media fragments.
#[derive(Debug, Default)]
pub struct BoxSplitter {
    /// Unparsed bytes carried over from previous pushes.
    buf: Vec<u8>,
    /// Accumulated init segment (`ftyp` + `moov`) until `moov` completes.
    init: Vec<u8>,
    /// Whether the init segment has been emitted.
    init_done: bool,
    /// Accumulating boxes of the current media fragment (closed on `mdat`).
    frag: Vec<u8>,
    /// Set once a corrupt header is seen — the splitter then emits nothing.
    poisoned: bool,
}

impl BoxSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the splitter has seen a corrupt / oversized box and stopped
    /// emitting (#178 item 14).
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Feed the next raw read from the child. Returns the chunks that completed.
    pub fn push(&mut self, data: &[u8]) -> Vec<RelayChunk> {
        let mut out = Vec::new();
        if self.poisoned {
            return out;
        }
        self.buf.extend_from_slice(data);
        loop {
            match parse_box_header(&self.buf) {
                BoxHeader::NeedMore => break,
                BoxHeader::Corrupt => {
                    self.poisoned = true;
                    break;
                }
                BoxHeader::Parsed {
                    total_len, four_cc, ..
                } => {
                    let total = total_len as usize;
                    if self.buf.len() < total {
                        break; // wait for the rest of this box
                    }
                    let box_bytes = self.buf.drain(..total).collect::<Vec<u8>>();
                    self.route_box(four_cc, box_bytes, &mut out);
                }
            }
        }
        out
    }

    /// Classify one complete top-level box and accumulate/emit accordingly.
    fn route_box(&mut self, four_cc: [u8; 4], mut box_bytes: Vec<u8>, out: &mut Vec<RelayChunk>) {
        if !self.init_done {
            // Everything up to and including `moov` is the init segment.
            self.init.append(&mut box_bytes);
            // An init accumulator larger than the cap (many boxes with no `moov`)
            // is a corrupt stream — poison rather than grow unbounded (#178 item 14).
            if exceeds_box_cap(self.init.len() as u64) {
                self.poisoned = true;
                self.init.clear();
                return;
            }
            if &four_cc == b"moov" {
                self.init_done = true;
                out.push(RelayChunk::Init(std::mem::take(&mut self.init)));
            }
            return;
        }
        // Post-init: accumulate boxes into the current fragment; an `mdat`
        // closes it (a fragment is `[styp?] [sidx?] moof mdat`).
        self.frag.append(&mut box_bytes);
        // A fragment accumulator over the cap (no `mdat` closing it) is corrupt.
        if exceeds_box_cap(self.frag.len() as u64) {
            self.poisoned = true;
            self.frag.clear();
            return;
        }
        if &four_cc == b"mdat" {
            out.push(RelayChunk::Fragment(std::mem::take(&mut self.frag)));
        }
    }
}

/// Shared, broadcast-backed relay of the current pipeline's fMP4 stream.
///
/// The reader thread drives [`FragmentRelay::ingest`] with the splitter's
/// chunks; each WS viewer takes an [`init`](FragmentRelay::init) snapshot and a
/// [`subscribe`](FragmentRelay::subscribe) receiver.
pub struct FragmentRelay {
    /// Cached init segment (`ftyp` + `moov`), set once the child emits it.
    init: Mutex<Option<Arc<[u8]>>>,
    /// Broadcast of media fragments to all current viewers. Behind a `Mutex` so
    /// [`close`](Self::close) can DROP and replace the sender, making every
    /// current receiver see `Closed` (#178 item 12).
    tx: Mutex<broadcast::Sender<Arc<[u8]>>>,
    /// Backlog capacity, kept so `close` can build a fresh channel.
    capacity: usize,
}

impl FragmentRelay {
    /// `capacity` bounds the per-viewer backlog; a viewer that falls further
    /// behind is dropped (`RecvError::Lagged`) and resyncs on the next
    /// keyframe-aligned fragment — it never blocks the reader.
    pub fn new(capacity: usize) -> Arc<Self> {
        let cap = capacity.max(1);
        let (tx, _rx) = broadcast::channel(cap);
        Arc::new(Self {
            init: Mutex::new(None),
            tx: Mutex::new(tx),
            capacity: cap,
        })
    }

    /// Feed one splitter chunk into the relay (reader thread).
    pub fn ingest(&self, chunk: RelayChunk) {
        match chunk {
            RelayChunk::Init(bytes) => {
                let arc: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
                if let Ok(mut slot) = self.init.lock() {
                    *slot = Some(arc);
                }
            }
            RelayChunk::Fragment(bytes) => {
                let arc: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
                // Err = no current receivers; that is fine (nobody watching).
                if let Ok(tx) = self.tx.lock() {
                    let _ = tx.send(arc);
                }
            }
        }
    }

    /// The cached init segment, if the child has produced its `moov` yet.
    pub fn init(&self) -> Option<Arc<[u8]>> {
        self.init.lock().ok().and_then(|s| s.clone())
    }

    /// A fresh fragment receiver for a joining viewer.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<[u8]>> {
        self.tx.lock().unwrap().subscribe()
    }

    /// Number of current viewers (broadcast receivers).
    pub fn viewer_count(&self) -> usize {
        self.tx.lock().unwrap().receiver_count()
    }

    /// Clear the cached init segment (#178 item 17) — called at each child's
    /// START and STOP so a stale init from a previous child is never served to a
    /// new child's late joiner (which must wait for the NEW init).
    pub fn reset(&self) {
        if let Ok(mut slot) = self.init.lock() {
            *slot = None;
        }
    }

    /// End the stream (#178 item 12): clear the cached init and DROP the current
    /// broadcast sender so every connected viewer's `recv()` returns `Closed`
    /// (their WS handler then closes the socket). A fresh sender takes any later
    /// subscribers. Called by the supervisor when it gives up restarting a
    /// repeatedly-dying child.
    pub fn close(&self) {
        if let Ok(mut slot) = self.init.lock() {
            *slot = None;
        }
        let (tx, _rx) = broadcast::channel(self.capacity);
        if let Ok(mut g) = self.tx.lock() {
            *g = tx; // old sender dropped here → current receivers see Closed
        }
    }
}

#[cfg(test)]
#[path = "fmp4_relay_tests.rs"]
mod tests;
