//! #233: the ASIO host's pipe protocol (SongPlayer ⇄ `sp-audio-host`).
//!
//! One local named pipe per host connection carries length-prefixed
//! frames: a little-endian `u32` body length (1 ..= [`MAX_FRAME_BYTES`],
//! checked BEFORE the body is allocated), then the body, whose first byte
//! is its kind:
//!
//! - 1, control (SongPlayer → host): a typed JSON [`Control`] — the hello
//!   (the protocol version and SongPlayer's first time [`Anchor`]), an
//!   output added (its handle and [`AsioSpec`]), an output removed.
//! - 2, a program block (SongPlayer → host), a fixed binary layout: the
//!   output's handle (`u64`), the block's boundary on SongPlayer's program
//!   timeline (`i64`, 100 ns, untouched by the host), SongPlayer's time
//!   anchor taken when it wrote the block (`i64` timeline + `i64` monotonic
//!   counter, `remote_clock.rs`: the host's ONLY time source), the flags
//!   (`u8`: 1 = samples follow, 2 = substituted), then exactly 3 200
//!   little-endian `f32` (one 48 kHz stereo 1 600-frame block) when samples
//!   follow and nothing else when not.
//! - 3, a report (host → SongPlayer): a typed JSON [`Report`] — an output's
//!   status (`wire_status.rs`, ~2 Hz), one of the host's log lines, the
//!   host's "alive" (every 500 ms, so a silent host is noticed), or why
//!   the host ends the session.
//!
//! Every JSON body is decoded into a typed struct, never through
//! `serde_json::Value` (`rust-workspace.md`: `raw_value` is on). A malformed
//! frame ends the session (the reader stops at the first one; the stream
//! cannot be trusted after it).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::audio_host::remote_clock::Anchor;
use crate::audio_host::wire_status::WireSnapshot;
use crate::playback::asio_out::AsioSpec;
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::vban_packet::VBAN_BLOCK_SAMPLES;

/// The protocol both sides speak (the hello carries it; they ship together).
pub const PROTOCOL: u32 = 2;
/// The largest frame body (a block is 12 834 bytes, a status ~2 KiB).
pub const MAX_FRAME_BYTES: usize = 65_536;
/// The longest host log line forwarded (bytes, cut on a character boundary).
pub const MAX_LOG_LINE_BYTES: usize = 4_096;

const KIND_CONTROL: u8 = 1;
const KIND_BLOCK: u8 = 2;
const KIND_REPORT: u8 = 3;
/// A block's head: handle, boundary, the anchor's timeline and counter
/// (8 bytes each), flags.
const BLOCK_HEAD_BYTES: usize = 33;
const FLAG_SAMPLES: u8 = 0x01;
const FLAG_SUBSTITUTED: u8 = 0x02;
/// Every flag bit this version knows (`FLAG_SAMPLES | FLAG_SUBSTITUTED`,
/// pinned by a test: a `|` of disjoint bits would be an equivalent mutant).
const KNOWN_FLAGS: u8 = 0x03;
/// The samples of one block, bytes.
const SAMPLES_BYTES: usize = VBAN_BLOCK_SAMPLES * 4;

/// SongPlayer → host control.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Control {
    Hello { protocol: u32, anchor: Anchor },
    Add { handle: u64, spec: AsioSpec },
    Remove { handle: u64 },
}

/// A host log line's level (`log_forward.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// Host → SongPlayer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Report {
    /// Boxed: a status is ~400 bytes, a log line ~30
    /// (`clippy::large_enum_variant`).
    Status {
        handle: u64,
        snapshot: Box<WireSnapshot>,
    },
    Log {
        level: HostLevel,
        line: String,
    },
    /// Sent every status tick, outputs or none: a session that hears nothing
    /// for `client::REPORT_SILENCE` ends (a hung host is restarted).
    Alive,
    /// The host ends the session: why (a frame broke the protocol). The
    /// last frame it writes.
    Ending {
        why: String,
    },
}

/// One program block for one output.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockFrame {
    pub handle: u64,
    /// The boundary on SongPlayer's program timeline, 100 ns.
    pub due_100ns: i64,
    /// SongPlayer's time anchor when it wrote the block (`remote_clock.rs`;
    /// the writer sets it, [`BlockFrame::of`] leaves it zero).
    pub anchor: Anchor,
    /// 3 200 interleaved stereo samples; `None` = silence.
    pub samples: Option<Arc<[f32]>>,
    pub substituted: bool,
}

impl BlockFrame {
    /// `block` for output `handle`; samples that are not one program block
    /// travel as silence, marked substituted (the worker plays such a
    /// block as silence anyway).
    pub fn of(handle: u64, block: ProgramBlock) -> Self {
        let wrong = block
            .samples
            .as_ref()
            .is_some_and(|s| s.len() != VBAN_BLOCK_SAMPLES);
        Self {
            handle,
            due_100ns: block.due_100ns,
            anchor: Anchor::default(),
            samples: if wrong { None } else { block.samples },
            substituted: block.substituted || wrong,
        }
    }
}

/// One frame of either direction.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Control(Control),
    Block(BlockFrame),
    Report(Report),
}

/// Why a frame could not be read or decoded.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ProtoError {
    #[error("a frame body of {0} bytes (at most 65536)")]
    TooLarge(usize),
    #[error("an empty frame")]
    Empty,
    #[error("an unknown frame kind {0}")]
    UnknownKind(u8),
    #[error("a malformed block frame: {0}")]
    BadBlock(&'static str),
    #[error("a malformed frame: {0}")]
    BadJson(String),
    #[error("the pipe ended inside a frame")]
    Truncated,
    #[error("the pipe: {0}")]
    Io(String),
}

/// A frame body's length, checked before anything is allocated for it.
pub fn check_len(len: usize) -> Result<usize, ProtoError> {
    match len {
        0 => Err(ProtoError::Empty),
        n if n > MAX_FRAME_BYTES => Err(ProtoError::TooLarge(n)),
        n => Ok(n),
    }
}

/// The bytes before a frame's body: its length.
const PREFIX_BYTES: usize = 4;
/// A whole block frame on the pipe with its samples: the prefix, the kind,
/// the head, the samples (a literal: a capacity has no observable formula;
/// pinned against its parts by a test).
const BLOCK_FRAME_BYTES: usize = 12_838;

/// `frame` as it goes on the pipe: its length, then its body, built in ONE
/// buffer (the length written over its placeholder at the end).
pub fn encode(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::with_capacity(BLOCK_FRAME_BYTES);
    out.extend_from_slice(&[0; PREFIX_BYTES]);
    match frame {
        Frame::Control(c) => json_into(&mut out, KIND_CONTROL, c),
        Frame::Block(b) => block_into(&mut out, b),
        Frame::Report(r) => json_into(&mut out, KIND_REPORT, r),
    }
    let body = (out.len() - PREFIX_BYTES) as u32;
    out[..PREFIX_BYTES].copy_from_slice(&body.to_le_bytes());
    out
}

/// A JSON body after the prefix. These types serialize infallibly (no map
/// keys, every float finite: `wire_status.rs`), so a failure is a bug: an
/// empty body, which the reader refuses as [`ProtoError::Empty`].
fn json_into<T: Serialize>(out: &mut Vec<u8>, kind: u8, value: &T) {
    out.push(kind);
    if serde_json::to_writer(&mut *out, value).is_err() {
        out.truncate(PREFIX_BYTES);
    }
}

fn block_into(out: &mut Vec<u8>, b: &BlockFrame) {
    let mut flags = 0u8;
    if b.samples.is_some() {
        flags |= FLAG_SAMPLES;
    }
    if b.substituted {
        flags |= FLAG_SUBSTITUTED;
    }
    out.push(KIND_BLOCK);
    out.extend_from_slice(&b.handle.to_le_bytes());
    out.extend_from_slice(&b.due_100ns.to_le_bytes());
    out.extend_from_slice(&b.anchor.timeline_100ns.to_le_bytes());
    out.extend_from_slice(&b.anchor.mono_100ns.to_le_bytes());
    out.push(flags);
    for s in b.samples.iter().flat_map(|s| s.iter()) {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

/// A frame from its body (kind byte first).
pub fn decode_body(body: &[u8]) -> Result<Frame, ProtoError> {
    let (&kind, rest) = body.split_first().ok_or(ProtoError::Empty)?;
    match kind {
        KIND_CONTROL => serde_json::from_slice(rest)
            .map(Frame::Control)
            .map_err(bad_json),
        KIND_BLOCK => decode_block(rest).map(Frame::Block),
        KIND_REPORT => serde_json::from_slice(rest)
            .map(Frame::Report)
            .map_err(bad_json),
        other => Err(ProtoError::UnknownKind(other)),
    }
}

fn bad_json(e: serde_json::Error) -> ProtoError {
    ProtoError::BadJson(e.to_string())
}

fn word(b: &[u8; BLOCK_HEAD_BYTES], at: usize) -> [u8; 8] {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[at..at + 8]);
    w
}

fn decode_block(rest: &[u8]) -> Result<BlockFrame, ProtoError> {
    let Some((head, tail)) = rest.split_first_chunk::<BLOCK_HEAD_BYTES>() else {
        return Err(ProtoError::BadBlock("its head is short"));
    };
    let flags = head[BLOCK_HEAD_BYTES - 1];
    if flags & !KNOWN_FLAGS != 0 {
        return Err(ProtoError::BadBlock("an unknown flag"));
    }
    let samples = if flags & FLAG_SAMPLES != 0 {
        if tail.len() != SAMPLES_BYTES {
            return Err(ProtoError::BadBlock("not one block of samples"));
        }
        let samples: Arc<[f32]> = tail
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        Some(samples)
    } else {
        if !tail.is_empty() {
            return Err(ProtoError::BadBlock("samples with no samples flag"));
        }
        None
    };
    Ok(BlockFrame {
        handle: u64::from_le_bytes(word(head, 0)),
        due_100ns: i64::from_le_bytes(word(head, 8)),
        anchor: Anchor {
            timeline_100ns: i64::from_le_bytes(word(head, 16)),
            mono_100ns: i64::from_le_bytes(word(head, 24)),
        },
        samples,
        substituted: flags & FLAG_SUBSTITUTED != 0,
    })
}

/// A host log line as it is forwarded: valid UTF-8 (lossy), the trailing
/// line break gone, at most [`MAX_LOG_LINE_BYTES`] cut on a character
/// boundary.
pub fn log_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_end_matches(['\r', '\n']);
    // The end of the last character that still fits (no loop to spin).
    let end = text
        .char_indices()
        .map(|(at, c)| at + c.len_utf8())
        .take_while(|&end| end <= MAX_LOG_LINE_BYTES)
        .last()
        .unwrap_or(0);
    text[..end].to_string()
}

fn io_error(e: std::io::Error) -> ProtoError {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        ProtoError::Truncated
    } else {
        ProtoError::Io(e.to_string())
    }
}

/// The next frame, or `None` when the pipe ended cleanly between frames.
/// The length is checked before its body is allocated or read.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Frame>, ProtoError> {
    let first = match r.read_u8().await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(ProtoError::Io(e.to_string())),
    };
    let mut rest = [0u8; 3];
    r.read_exact(&mut rest).await.map_err(io_error)?;
    let len = check_len(u32::from_le_bytes([first, rest[0], rest[1], rest[2]]) as usize)?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.map_err(io_error)?;
    decode_body(&body).map(Some)
}

/// Write one frame and flush it.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    frame: &Frame,
) -> Result<(), ProtoError> {
    w.write_all(&encode(frame)).await.map_err(io_error)?;
    w.flush().await.map_err(io_error)
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
