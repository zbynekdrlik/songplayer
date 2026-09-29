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

use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
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
/// equivalent (malformed, nested [`MAX_DEPTH`] deep, a map key that is not a
/// string, binary / extension data, or bytes after the value).
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
            Self::Json => serde_json::from_str::<PlainValue>(text)
                .map(|PlainValue(value)| value)
                .map_err(|_| JSON_UNDECODABLE),
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
/// `from_msgpack` refuses trailing bytes too). A map key must be a string (a
/// `bin` key holding UTF-8 is read as that string — serde's `String` accepts
/// it, harmlessly; obs-websocket would refuse it), and binary / extension
/// data as a value has no JSON equivalent, so it is refused.
fn decode_msgpack(bytes: &[u8]) -> Option<Value> {
    let mut de = rmp_serde::Deserializer::new(bytes);
    de.set_max_depth(MAX_DEPTH);
    let PlainValue(value) = PlainValue::deserialize(&mut de).ok()?;
    de.get_ref().is_empty().then_some(value)
}

/// A decoded message as a `serde_json::Value`, built with EVERY map key as a
/// plain string. serde_json's `raw_value` feature is always on in this build
/// (sp-server's `api/preview.rs` enables it, and so do axum's `json` and
/// sqlx-core through feature unification), and with it `Value`'s own
/// `Deserialize` treats a map whose first key is
/// `$serde_json::private::RawValue` as a raw value and re-parses its string
/// as JSON with a FRESH 128-level budget, so strings nested that way would
/// take a session's stack far deeper than [`MAX_DEPTH`] (#221 review round
/// 1). Here nothing is re-parsed: a frame nests exactly as deep as its
/// deserializer allows (serde_json's and rmp-serde's limit, 128).
struct PlainValue(Value);

impl<'de> Deserialize<'de> for PlainValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(PlainVisitor).map(PlainValue)
    }
}

/// Builds a [`PlainValue`]. Both deserializers reach every JSON value through
/// these methods (serde forwards the smaller ints / `f32` / borrowed strings);
/// binary / extension data has no JSON value and is refused.
struct PlainVisitor;

impl<'de> Visitor<'de> for PlainVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    /// NaN / an infinity is `null`, as in serde_json.
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    /// mutants::skip — its only mutant, `Ok(Default::default())`, IS
    /// `Value::Null` (equivalent); a null is pinned by the round-trip tests.
    #[cfg_attr(test, mutants::skip)]
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(PlainValue(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some((key, PlainValue(value))) = map.next_entry::<String, PlainValue>()? {
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;
