//! The obs-websocket 5 wire protocol, SERVER side (#213) — pure
//! (de)serialization, no I/O.
//!
//! Source: obsproject/obs-websocket `docs/generated/protocol.md` (rpcVersion 1)
//! and the bitfocus `companion-module-obs-studio` source (v3.15.3 + 4.0 beta),
//! read before this was written (#213 comment 5850492736).
//!
//! - Framing: `{"op": n, "d": {...}}` JSON text frames over the
//!   `obswebsocket.json` subprotocol. obs-websocket-js REQUIRES the server to
//!   echo it ("Server sent no subprotocol" otherwise), see
//!   [`negotiate_subprotocol`].
//! - Handshake: `Hello` (op 0, with `authentication` when a password is set) →
//!   `Identify` (op 1) → `Identified` (op 2); later `Reidentify` (op 3) →
//!   `Identified`. The auth string is the client's own algorithm,
//!   [`crate::obs::compute_auth`] (`base64(sha256(base64(sha256(password +
//!   salt)) + challenge))`), compared in constant time.
//! - Requests: `Request` (op 6) → `RequestResponse` (op 7), `RequestBatch`
//!   (op 8) → `RequestBatchResponse` (op 9, executed serially in order).
//! - Events: `Event` (op 5) to identified clients subscribed to its intent.
//!
//! Which request is answered how is [`route`]: a few natively, the scene/input
//! list getters forwarded to cg OBS, `SetCurrentProgramScene` forwarded AND
//! mapped onto the program bus, anything else a well-formed
//! [`STATUS_UNKNOWN_REQUEST_TYPE`] error (Companion treats a failed request as
//! "no data" and stays connected — only `GetVersion` and
//! `GetStudioModeEnabled` must succeed, which is why they are native).

use serde_json::{Value, json};

use crate::obs::compute_auth;

/// The only RPC version (obs-websocket 5).
pub const RPC_VERSION: u64 = 2;
/// The JSON subprotocol (`Sec-WebSocket-Protocol`).
pub const SUBPROTOCOL_JSON: &str = "obswebsocket.json";
/// The obs-websocket protocol level this facade speaks (the rpcVersion-1
/// subset defined in 5.0.0; scene data is passed through from cg OBS).
pub const OBS_WEBSOCKET_VERSION: &str = "5.0.0";

/// `WebSocketOpCode`.
pub const OP_HELLO: u64 = 0;
pub const OP_IDENTIFY: u64 = 1;
pub const OP_IDENTIFIED: u64 = 2;
pub const OP_REIDENTIFY: u64 = 3;
pub const OP_EVENT: u64 = 5;
pub const OP_REQUEST: u64 = 6;
pub const OP_REQUEST_RESPONSE: u64 = 7;
pub const OP_REQUEST_BATCH: u64 = 8;
pub const OP_REQUEST_BATCH_RESPONSE: u64 = 9;

/// `WebSocketCloseCode`.
pub const CLOSE_MESSAGE_DECODE_ERROR: u16 = 4002;
pub const CLOSE_MISSING_DATA_FIELD: u16 = 4003;
pub const CLOSE_UNKNOWN_OP_CODE: u16 = 4006;
pub const CLOSE_NOT_IDENTIFIED: u16 = 4007;
pub const CLOSE_ALREADY_IDENTIFIED: u16 = 4008;
pub const CLOSE_AUTHENTICATION_FAILED: u16 = 4009;
pub const CLOSE_UNSUPPORTED_RPC_VERSION: u16 = 4010;

/// `RequestStatus`.
pub const STATUS_SUCCESS: u16 = 100;
pub const STATUS_MISSING_REQUEST_TYPE: u16 = 203;
pub const STATUS_UNKNOWN_REQUEST_TYPE: u16 = 204;
pub const STATUS_GENERIC_ERROR: u16 = 205;
pub const STATUS_NOT_READY: u16 = 207;
pub const STATUS_MISSING_REQUEST_FIELD: u16 = 300;

/// `EventSubscription::Scenes` (1 << 2).
pub const EVENT_SCENES: u64 = 4;
/// `EventSubscription::All` of rpcVersion 1 before Canvases (bits 0..=10) —
/// the default when an `Identify` / `Reidentify` names no subscriptions.
pub const EVENT_ALL: u64 = 0x7FF;

/// The close reasons Companion's status mapping matches on (obs-websocket's
/// own wording): "missing an `authentication` string" → "Missing password",
/// "Authentication failed" → an authentication failure.
pub const REASON_AUTH_MISSING: &str = "Your payload's data is missing an `authentication` string, however authentication is required.";
pub const REASON_AUTH_FAILED: &str = "Authentication failed.";

/// The requests forwarded to cg OBS verbatim (the scene/input list getters).
pub const FORWARDED_REQUESTS: [&str; 5] = [
    "GetSceneList",
    "GetCurrentProgramScene",
    "GetInputList",
    "GetSceneItemList",
    "GetGroupSceneItemList",
];

/// Why the server closes a session: an obs-websocket close code + its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseReason {
    pub code: u16,
    pub reason: &'static str,
}

impl CloseReason {
    const fn new(code: u16, reason: &'static str) -> Self {
        Self { code, reason }
    }
}

/// The `Sec-WebSocket-Protocol` answer for a client's offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subprotocol {
    /// `obswebsocket.json` was offered: echo it.
    Json,
    /// Nothing was offered: JSON by default, no header.
    Default,
    /// Only other encodings (`obswebsocket.msgpack`) were offered: reject.
    Unsupported,
}

/// Pick the subprotocol for a comma-separated `Sec-WebSocket-Protocol` offer.
pub fn negotiate_subprotocol(offered: Option<&str>) -> Subprotocol {
    let Some(offered) = offered.filter(|o| !o.trim().is_empty()) else {
        return Subprotocol::Default;
    };
    if offered.split(',').any(|p| p.trim() == SUBPROTOCOL_JSON) {
        Subprotocol::Json
    } else {
        Subprotocol::Unsupported
    }
}

/// The auth challenge + salt one session's `Hello` carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthChallenge {
    pub challenge: String,
    pub salt: String,
}

impl AuthChallenge {
    /// A fresh random challenge and salt (32 random bytes each, base64).
    pub fn random() -> Self {
        Self {
            challenge: random_base64(),
            salt: random_base64(),
        }
    }

    /// Whether `provided` is the `authentication` string a client knowing
    /// `password` computes for this challenge (constant-time compare).
    pub fn accepts(&self, password: &str, provided: &str) -> bool {
        let expected = compute_auth(password, &self.salt, &self.challenge);
        constant_time_eq(expected.as_bytes(), provided.as_bytes())
    }
}

fn random_base64() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Equal length and equal bytes, without an early exit on the first mismatch.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The facade's identity in `Hello` / `GetVersion` (`obsVersion`).
pub fn server_version() -> String {
    format!("SongPlayer {}", sp_core::config::VERSION)
}

/// The `Hello` sent right after the upgrade.
pub fn hello(auth: Option<&AuthChallenge>) -> Value {
    let mut d = json!({
        "obsStudioVersion": server_version(),
        "obsWebSocketVersion": OBS_WEBSOCKET_VERSION,
        "rpcVersion": RPC_VERSION,
    });
    if let Some(a) = auth {
        d["authentication"] = json!({ "challenge": a.challenge, "salt": a.salt });
    }
    json!({ "op": OP_HELLO, "d": d })
}

/// The `Identified` answering an `Identify` or a `Reidentify`.
pub fn identified() -> Value {
    json!({ "op": OP_IDENTIFIED, "d": { "negotiatedRpcVersion": RPC_VERSION } })
}

/// A client message the server acts on.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    Identify {
        rpc_version: u64,
        authentication: Option<String>,
        event_subscriptions: u64,
    },
    Reidentify {
        event_subscriptions: u64,
    },
    Request(RequestItem),
    Batch {
        request_id: String,
        halt_on_failure: bool,
        requests: Vec<RequestItem>,
    },
}

/// One request: a single `Request` (type + id always present) or a batch
/// entry (both optional there; a missing type is answered with
/// [`STATUS_MISSING_REQUEST_TYPE`]).
#[derive(Debug, Clone, PartialEq)]
pub struct RequestItem {
    pub request_type: Option<String>,
    pub request_id: Option<String>,
    pub request_data: Option<Value>,
}

impl RequestItem {
    fn from_json(d: &Value) -> Self {
        Self {
            request_type: str_field(d, "requestType"),
            request_id: str_field(d, "requestId"),
            request_data: d.get("requestData").cloned(),
        }
    }
}

fn str_field(d: &Value, key: &str) -> Option<String> {
    d.get(key).and_then(Value::as_str).map(str::to_string)
}

fn subscriptions(d: &Value) -> u64 {
    d.get("eventSubscriptions")
        .and_then(Value::as_u64)
        .unwrap_or(EVENT_ALL)
}

/// Parse one text frame, or the close obs-websocket answers a malformed one
/// with.
pub fn parse_client_message(text: &str) -> Result<ClientMessage, CloseReason> {
    let msg: Value = serde_json::from_str(text).map_err(|_| decode_error())?;
    if !msg.is_object() {
        return Err(decode_error());
    }
    let op = msg.get("op").and_then(Value::as_u64).ok_or(UNKNOWN_OP)?;
    let d = msg
        .get("d")
        .filter(|d| d.is_object())
        .ok_or(CloseReason::new(
            CLOSE_MISSING_DATA_FIELD,
            "Your payload is missing data (`d`).",
        ))?;
    match op {
        OP_IDENTIFY => Ok(ClientMessage::Identify {
            rpc_version: d
                .get("rpcVersion")
                .and_then(Value::as_u64)
                .ok_or(CloseReason::new(
                    CLOSE_MISSING_DATA_FIELD,
                    "Your payload's data is missing an `rpcVersion`.",
                ))?,
            authentication: str_field(d, "authentication"),
            event_subscriptions: subscriptions(d),
        }),
        OP_REIDENTIFY => Ok(ClientMessage::Reidentify {
            event_subscriptions: subscriptions(d),
        }),
        OP_REQUEST => {
            let item = RequestItem::from_json(d);
            if item.request_type.is_none() || item.request_id.is_none() {
                return Err(CloseReason::new(
                    CLOSE_MISSING_DATA_FIELD,
                    "Your request is missing a `requestType` or a `requestId`.",
                ));
            }
            Ok(ClientMessage::Request(item))
        }
        OP_REQUEST_BATCH => {
            let request_id = str_field(d, "requestId").ok_or(CloseReason::new(
                CLOSE_MISSING_DATA_FIELD,
                "Your request batch is missing a `requestId`.",
            ))?;
            let requests = d
                .get("requests")
                .and_then(Value::as_array)
                .ok_or(CloseReason::new(
                    CLOSE_MISSING_DATA_FIELD,
                    "Your request batch is missing a `requests` array.",
                ))?
                .iter()
                .map(RequestItem::from_json)
                .collect();
            Ok(ClientMessage::Batch {
                request_id,
                halt_on_failure: d
                    .get("haltOnFailure")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                requests,
            })
        }
        _ => Err(UNKNOWN_OP),
    }
}

const UNKNOWN_OP: CloseReason = CloseReason::new(
    CLOSE_UNKNOWN_OP_CODE,
    "The `op` is missing or not one a client may send.",
);

/// The close for a frame that is not a JSON object (or a binary frame).
pub fn decode_error() -> CloseReason {
    CloseReason::new(
        CLOSE_MESSAGE_DECODE_ERROR,
        "The message is not obswebsocket.json text.",
    )
}

/// The close for anything but `Identify` before the session is identified.
pub const NOT_IDENTIFIED: CloseReason = CloseReason::new(
    CLOSE_NOT_IDENTIFIED,
    "You must send an `Identify` before anything else.",
);
/// The close for a second `Identify`.
pub const ALREADY_IDENTIFIED: CloseReason = CloseReason::new(
    CLOSE_ALREADY_IDENTIFIED,
    "You are already identified; use `Reidentify`.",
);

/// Check an `Identify` the way obs-websocket does: the authentication first
/// (when this session requires one), then the RPC version. `auth` is this
/// session's challenge + the password it was generated for.
pub fn check_identify(
    rpc_version: u64,
    provided: Option<&str>,
    auth: Option<(&AuthChallenge, &str)>,
) -> Result<(), CloseReason> {
    if let Some((challenge, password)) = auth {
        let Some(provided) = provided else {
            return Err(CloseReason::new(
                CLOSE_AUTHENTICATION_FAILED,
                REASON_AUTH_MISSING,
            ));
        };
        if !challenge.accepts(password, provided) {
            return Err(CloseReason::new(
                CLOSE_AUTHENTICATION_FAILED,
                REASON_AUTH_FAILED,
            ));
        }
    }
    if rpc_version != RPC_VERSION {
        return Err(CloseReason::new(
            CLOSE_UNSUPPORTED_RPC_VERSION,
            "Requested an unsupported `rpcVersion` (this server speaks 1).",
        ));
    }
    Ok(())
}

/// A request's outcome: its `requestStatus` object + the optional
/// `responseData`.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub status: Value,
    pub data: Option<Value>,
}

impl Reply {
    /// `RequestStatus::Success`, with `data`.
    pub fn ok(data: Option<Value>) -> Self {
        Self {
            status: json!({ "result": true, "code": STATUS_SUCCESS }),
            data,
        }
    }

    /// A failure `code` with its comment.
    pub fn error(code: u16, comment: &str) -> Self {
        Self {
            status: json!({ "result": false, "code": code, "comment": comment }),
            data: None,
        }
    }

    /// cg OBS's op=7 `d` object, passed through verbatim (a `d` without a
    /// `requestStatus` object is a [`STATUS_GENERIC_ERROR`]).
    pub fn from_upstream(d: &Value) -> Self {
        match d.get("requestStatus").filter(|s| s.is_object()) {
            Some(status) => Self {
                status: status.clone(),
                data: d.get("responseData").cloned(),
            },
            None => Self::error(STATUS_GENERIC_ERROR, "cg OBS sent no request status"),
        }
    }

    /// `requestStatus.result`.
    pub fn succeeded(&self) -> bool {
        self.status["result"].as_bool() == Some(true)
    }
}

/// The `RequestResponse` (op 7) for a single request.
pub fn request_response(request_type: &str, request_id: &str, reply: &Reply) -> Value {
    let mut d = json!({
        "requestType": request_type,
        "requestId": request_id,
        "requestStatus": reply.status,
    });
    if let Some(data) = &reply.data {
        d["responseData"] = data.clone();
    }
    json!({ "op": OP_REQUEST_RESPONSE, "d": d })
}

/// One entry of a `RequestBatchResponse`'s `results` (its `requestId` only
/// when the entry carried one).
pub fn batch_result(item: &RequestItem, reply: &Reply) -> Value {
    let mut r = json!({
        "requestType": item.request_type.as_deref().unwrap_or(""),
        "requestStatus": reply.status,
    });
    if let Some(id) = &item.request_id {
        r["requestId"] = json!(id);
    }
    if let Some(data) = &reply.data {
        r["responseData"] = data.clone();
    }
    r
}

/// The `RequestBatchResponse` (op 9).
pub fn batch_response(request_id: &str, results: Vec<Value>) -> Value {
    json!({
        "op": OP_REQUEST_BATCH_RESPONSE,
        "d": { "requestId": request_id, "results": results },
    })
}

/// An `Event` (op 5).
pub fn event(event_type: &str, intent: u64, data: &Value) -> Value {
    json!({
        "op": OP_EVENT,
        "d": { "eventType": event_type, "eventIntent": intent, "eventData": data },
    })
}

/// The cg OBS events the facade re-emits, with the intent a client must be
/// subscribed to (Companion's scene feedback + scene list).
pub fn passthrough_intent(event_type: &str) -> Option<u64> {
    match event_type {
        "CurrentProgramSceneChanged" | "SceneListChanged" => Some(EVENT_SCENES),
        _ => None,
    }
}

/// Whether `subscriptions` include `intent`.
pub fn subscribed(subscriptions: u64, intent: u64) -> bool {
    (subscriptions & intent) != 0
}

/// How the facade answers one request type.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// Answered by the facade itself.
    Native(Reply),
    /// Forwarded to cg OBS, its answer passed through.
    Forward,
    /// Forwarded to cg OBS AND mapped onto `SP-program`.
    SetProgramScene,
    /// A well-formed [`STATUS_UNKNOWN_REQUEST_TYPE`] error.
    Unsupported,
}

/// Route one request type.
pub fn route(request_type: &str) -> Route {
    match request_type {
        "GetVersion" => Route::Native(Reply::ok(Some(version_data()))),
        // Studio mode OFF: Companion then sends `SetCurrentProgramScene` per
        // button (preview + transition requests are not mapped).
        "GetStudioModeEnabled" => {
            Route::Native(Reply::ok(Some(json!({ "studioModeEnabled": false }))))
        }
        "SetCurrentProgramScene" => Route::SetProgramScene,
        // Keep in sync with FORWARDED_REQUESTS (pinned by a test).
        "GetSceneList"
        | "GetCurrentProgramScene"
        | "GetInputList"
        | "GetSceneItemList"
        | "GetGroupSceneItemList" => Route::Forward,
        _ => Route::Unsupported,
    }
}

/// Every request type the facade serves (`GetVersion.availableRequests`).
pub fn available_requests() -> Vec<&'static str> {
    let mut all = vec![
        "GetVersion",
        "GetStudioModeEnabled",
        "SetCurrentProgramScene",
    ];
    all.extend(FORWARDED_REQUESTS);
    all
}

/// `GetVersion`'s response data. `supportedImageFormats` MUST be an array:
/// Companion iterates it unguarded (v3 `forEach`, v4 `map`).
pub fn version_data() -> Value {
    json!({
        "obsVersion": server_version(),
        "obsWebSocketVersion": OBS_WEBSOCKET_VERSION,
        "rpcVersion": RPC_VERSION,
        "availableRequests": available_requests(),
        "supportedImageFormats": [],
        "platform": "songplayer",
        "platformDescription": "SongPlayer remote control (obs-websocket 5 subset)",
    })
}

/// The comment of an [`Route::Unsupported`] request's error.
pub fn unsupported_comment(request_type: &str) -> String {
    format!(
        "SongPlayer's remote control does not serve `{request_type}` (obs-websocket subset, #213)"
    )
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
