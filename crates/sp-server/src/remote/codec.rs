//! The message ENCODING of one remote-control session (#221 L2b) — pure, no
//! I/O.
//!
//! obs-websocket 5 has two subprotocols that carry the SAME message objects:
//! - `obswebsocket.json`: JSON in WebSocket TEXT frames;
//! - `obswebsocket.msgpack`: MessagePack in BINARY frames, maps keyed by
//!   strings (obs-websocket itself encodes with nlohmann's `json::to_msgpack`).
//!
//! Companion's obs-studio module (v3.15.3) runs in Node, where its
//! `obs-websocket-js` import resolves to the msgpack build: it offers ONLY
//! `obswebsocket.msgpack` (#221 comment 5881650057 — the #213 design assumed
//! JSON). The session picks its codec once, at the handshake
//! (`protocol::negotiate_subprotocol` → [`Codec::for_subprotocol`]), and
//! every message of the session goes through it, both ways. A frame of the
//! other kind, or one that does not decode, is closed with 4002
//! (`MessageDecodeError`) and obs-websocket's wording (for an undecodable
//! frame, its reason without the parser's error appended).
//!
//! The 1 MiB bound (`remote::MAX_MESSAGE_BYTES`) is the WebSocket layer's, so
//! it holds for both encodings before a frame reaches the codec.

use serde::Deserialize;
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

use super::protocol::{CLOSE_MESSAGE_DECODE_ERROR, CloseReason, Subprotocol};

/// The nesting a MessagePack message must stay below: serde_json's own
/// recursion limit, so both encodings refuse the same messages, and a 1 MiB
/// frame of nested arrays never recurses a session's stack away (rmp-serde's
/// default is 1024).
pub const MAX_DEPTH: usize = 128;

/// A JSON session's frame that is not JSON. obs-websocket's reason starts
/// the same and appends the parser's error; here the reason is fixed (a
/// `CloseReason` is `&'static str`), as for [`MSGPACK_UNDECODABLE`].
pub const JSON_UNDECODABLE: CloseReason = CloseReason {
    code: CLOSE_MESSAGE_DECODE_ERROR,
    reason: "Unable to decode Json.",
};
/// A msgpack session's frame that is not ONE MessagePack value with a JSON
/// equivalent (malformed, nested [`MAX_DEPTH`] deep, a non-string map key,
/// binary / extension data, or bytes after the value).
pub const MSGPACK_UNDECODABLE: CloseReason = CloseReason {
    code: CLOSE_MESSAGE_DECODE_ERROR,
    reason: "Unable to decode MsgPack.",
};
/// A binary frame on a JSON session (obs-websocket's wording).
pub const BINARY_ON_JSON: CloseReason = CloseReason {
    code: CLOSE_MESSAGE_DECODE_ERROR,
    reason: "Your session encoding is set to Json, but a binary message was received.",
};
/// A text frame on a msgpack session (obs-websocket's wording).
pub const TEXT_ON_MSGPACK: CloseReason = CloseReason {
    code: CLOSE_MESSAGE_DECODE_ERROR,
    reason: "Your session encoding is set to MsgPack, but a text message was received.",
};

/// How one session's messages are framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// `obswebsocket.json` (also a client that offered nothing).
    Json,
    /// `obswebsocket.msgpack`.
    MsgPack,
}

impl Codec {
    /// The codec of a negotiated subprotocol; `None` refuses the handshake.
    pub fn for_subprotocol(subprotocol: Subprotocol) -> Option<Self> {
        match subprotocol {
            Subprotocol::Json | Subprotocol::Default => Some(Self::Json),
            Subprotocol::MsgPack => Some(Self::MsgPack),
            Subprotocol::Unsupported => None,
        }
    }

    /// The frame of one outgoing message: its JSON text, or its MessagePack
    /// bytes (`to_vec_named`: maps keyed by strings, like obs-websocket's).
    /// A `serde_json::Value` always encodes (rmp-serde 1.3 fails only when a
    /// `Serialize` impl reports an error or the writer fails, and neither a
    /// `Value` nor a `Vec` does); the `Result` lets the session end instead
    /// of panicking if a later rmp-serde ever does.
    pub fn encode(self, msg: &Value) -> Result<Message, rmp_serde::encode::Error> {
        match self {
            Self::Json => Ok(Message::Text(msg.to_string().into())),
            Self::MsgPack => {
                rmp_serde::to_vec_named(msg).map(|bytes| Message::Binary(bytes.into()))
            }
        }
    }

    /// One incoming TEXT frame, or the close obs-websocket answers it with.
    pub fn decode_text(self, text: &str) -> Result<Value, CloseReason> {
        match self {
            Self::Json => serde_json::from_str(text).map_err(|_| JSON_UNDECODABLE),
            Self::MsgPack => Err(TEXT_ON_MSGPACK),
        }
    }

    /// One incoming BINARY frame, or the close obs-websocket answers it with.
    pub fn decode_binary(self, bytes: &[u8]) -> Result<Value, CloseReason> {
        match self {
            Self::Json => Err(BINARY_ON_JSON),
            Self::MsgPack => decode_msgpack(bytes).ok_or(MSGPACK_UNDECODABLE),
        }
    }
}

/// ONE MessagePack value as a JSON value: nested less than [`MAX_DEPTH`]
/// deep (127 arrays / maps pass and 128 do not, exactly like serde_json's
/// parser), and every byte of the frame belongs to it (obs-websocket's strict
/// `from_msgpack` refuses trailing bytes too). A map key must be a string and
/// binary / extension data has no JSON value, so both are refused.
fn decode_msgpack(bytes: &[u8]) -> Option<Value> {
    let mut de = rmp_serde::Deserializer::new(bytes);
    de.set_max_depth(MAX_DEPTH);
    let value = Value::deserialize(&mut de).ok()?;
    de.get_ref().is_empty().then_some(value)
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;
