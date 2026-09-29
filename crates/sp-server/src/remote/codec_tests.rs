//! #221 L2b `remote/codec.rs`: a session's encoding, JSON text or
//! MessagePack binary. The MessagePack fixtures are independent of rmp-serde:
//! the client bytes were produced by `@msgpack/msgpack` 2.8.0 (the encoder of
//! obs-websocket-js 5.0.8's msgpack build, i.e. Companion's), and the
//! server bytes are written out by hand from the MessagePack spec.
//! Wired via `#[cfg(test)] #[path = "codec_tests.rs"] mod tests;`.

use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use super::*;
use crate::remote::protocol;

// The markers are escapes, the map keys / strings plain text: `\x82` fixmap(2),
// `\xa2` fixstr(2), `\x01` positive fixint 1, `\xc0` nil, `\xcd` uint16.

/// `@msgpack/msgpack` `encode({op: 1, d: {rpcVersion: 1, eventSubscriptions:
/// 2047}})` — obs-websocket-js's Identify (2047 is a uint16, `cd 07 ff`).
const COMPANION_IDENTIFY: &[u8] =
    b"\x82\xa2op\x01\xa1d\x82\xaarpcVersion\x01\xb2eventSubscriptions\xcd\x07\xff";
/// `@msgpack/msgpack` `encode({op: 6, d: {requestId: "1", requestType:
/// "GetVersion", requestData: undefined}})` — obs-websocket-js's `call`
/// without data: `undefined` is encoded as nil (`c0`).
const COMPANION_GET_VERSION: &[u8] =
    b"\x82\xa2op\x06\xa1d\x83\xa9requestId\xa11\xabrequestType\xaaGetVersion\xabrequestData\xc0";
/// `Identified` as the facade must send it: fixmap(2) { "d": fixmap(1) {
/// "negotiatedRpcVersion" (fixstr 20): 1 }, "op": 2 } — maps keyed by
/// strings (a struct-as-array encoding would start with `\x92`), minimal
/// ints, keys in serde_json's sorted-map order (no `preserve_order` in the
/// workspace; a decoder does not care about the order).
const IDENTIFIED: &[u8] = b"\x82\xa1d\x81\xb4negotiatedRpcVersion\x01\xa2op\x02";

/// `depth` arrays, each holding the next, the innermost empty.
fn nested_arrays(depth: usize) -> Vec<u8> {
    let mut b = vec![0x91; depth - 1]; // fixarray(1)
    b.push(0x90); // fixarray(0)
    b
}

#[test]
fn each_negotiated_subprotocol_has_its_codec() {
    assert_eq!(Codec::for_subprotocol(Subprotocol::Json), Some(Codec::Json));
    assert_eq!(
        Codec::for_subprotocol(Subprotocol::Default),
        Some(Codec::Json)
    );
    assert_eq!(
        Codec::for_subprotocol(Subprotocol::MsgPack),
        Some(Codec::MsgPack)
    );
    assert_eq!(Codec::for_subprotocol(Subprotocol::Unsupported), None);
}

#[test]
fn a_json_session_sends_text_and_a_msgpack_session_sends_binary() {
    let identified = protocol::identified();
    assert_eq!(
        Codec::Json.encode(&identified).unwrap(),
        Message::Text(identified.to_string().into())
    );
    assert_eq!(
        Codec::MsgPack.encode(&identified).unwrap(),
        Message::Binary(IDENTIFIED.to_vec().into())
    );
}

#[test]
fn a_msgpack_message_round_trips_every_json_value_kind() {
    let msg = json!({ "op": 7, "d": {
        "requestType": "GetVersion",
        "requestStatus": { "result": true, "code": 100 },
        "responseData": {
            "supportedImageFormats": [],
            "availableRequests": ["GetVersion", "GetSceneList"],
            "negative": -3,
            "fraction": 0.5,
            "large": 4_294_967_296_u64,
            "none": null,
            "text": "OBS manuál",
        },
    }});
    let Message::Binary(frame) = Codec::MsgPack.encode(&msg).unwrap() else {
        panic!("a msgpack session sends binary frames");
    };
    assert_eq!(Codec::MsgPack.decode_binary(&frame), Ok(msg));
}

#[test]
fn companions_msgpack_messages_decode_to_the_same_objects() {
    assert_eq!(
        Codec::MsgPack.decode_binary(COMPANION_IDENTIFY),
        Ok(json!({ "op": 1, "d": { "rpcVersion": 1, "eventSubscriptions": 2047 } }))
    );
    // `requestData: undefined` arrives as null, which no request reads as data.
    assert_eq!(
        Codec::MsgPack.decode_binary(COMPANION_GET_VERSION),
        Ok(json!({ "op": 6, "d": {
            "requestId": "1",
            "requestType": "GetVersion",
            "requestData": null,
        }}))
    );
    // The same message as JSON text decodes to the same object.
    assert_eq!(
        Codec::Json.decode_text(r#"{"op":1,"d":{"rpcVersion":1,"eventSubscriptions":2047}}"#),
        Codec::MsgPack.decode_binary(COMPANION_IDENTIFY)
    );
}

#[test]
fn a_frame_of_the_other_encoding_is_obs_websockets_4002() {
    assert_eq!(
        Codec::MsgPack.decode_text(r#"{"op":1,"d":{"rpcVersion":1}}"#),
        Err(TEXT_ON_MSGPACK)
    );
    assert_eq!(
        Codec::Json.decode_binary(COMPANION_IDENTIFY),
        Err(BINARY_ON_JSON)
    );
    for reason in [TEXT_ON_MSGPACK, BINARY_ON_JSON] {
        assert_eq!(reason.code, 4002);
    }
    assert_eq!(
        TEXT_ON_MSGPACK.reason,
        "Your session encoding is set to MsgPack, but a text message was received."
    );
    assert_eq!(
        BINARY_ON_JSON.reason,
        "Your session encoding is set to Json, but a binary message was received."
    );
}

#[test]
fn a_frame_that_does_not_decode_is_4002() {
    assert_eq!(Codec::Json.decode_text("{"), Err(JSON_UNDECODABLE));
    assert_eq!(JSON_UNDECODABLE.code, 4002);
    assert_eq!(MSGPACK_UNDECODABLE.code, 4002);
    fn undecodable(b: &[u8]) -> bool {
        Codec::MsgPack.decode_binary(b) == Err(MSGPACK_UNDECODABLE)
    }
    // Empty, a reserved marker (`c1`), and a map cut short.
    assert!(undecodable(&[]));
    assert!(undecodable(&[0xc1]));
    let identify = COMPANION_IDENTIFY.to_vec();
    assert!(undecodable(&identify[..identify.len() - 1]));
    // ONE value per frame: a byte after it is refused (a whole message
    // followed by another nil) — the message alone decodes.
    assert!(Codec::MsgPack.decode_binary(&identify).is_ok());
    let mut trailing = identify;
    trailing.push(0xc0);
    assert!(undecodable(&trailing));
    // No JSON value: an integer map key (`{1: 2}`), binary data (`c4 01 ff`),
    // an extension (fixext1 `d4 01 00`).
    assert!(undecodable(&[0x81, 0x01, 0x02]));
    assert!(undecodable(&[0xc4, 0x01, 0xff]));
    assert!(undecodable(&[0xd4, 0x01, 0x00]));
}

#[test]
fn msgpack_nesting_is_bounded_like_serde_json() {
    // serde_json's parser takes 127 nested arrays and refuses 128.
    let json = |depth: usize| format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    assert!(serde_json::from_str::<Value>(&json(MAX_DEPTH - 1)).is_ok());
    assert!(serde_json::from_str::<Value>(&json(MAX_DEPTH)).is_err());
    // The msgpack decoder takes and refuses exactly the same.
    let decoded = Codec::MsgPack.decode_binary(&nested_arrays(MAX_DEPTH - 1));
    assert_eq!(decoded, Codec::Json.decode_text(&json(MAX_DEPTH - 1)));
    assert_eq!(
        Codec::MsgPack.decode_binary(&nested_arrays(MAX_DEPTH)),
        Err(MSGPACK_UNDECODABLE)
    );
    // A frame of nothing but nested arrays (under the 1 MiB bound) is refused,
    // never followed down a million levels.
    assert_eq!(
        Codec::MsgPack.decode_binary(&nested_arrays(1_000_000)),
        Err(MSGPACK_UNDECODABLE)
    );
}
