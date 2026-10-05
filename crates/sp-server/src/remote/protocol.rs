//! The obs-websocket 5 wire protocol, SERVER side (#213) — pure
//! (de)serialization, no I/O.
//!
//! Source: obsproject/obs-websocket `docs/generated/protocol.md` (rpcVersion 1)
//! and the bitfocus `companion-module-obs-studio` source (v3.15.3 + 4.0 beta),
//! read before this was written (#213 comment 5850492736).
//!
//! - Framing: `{"op": n, "d": {...}}` message objects, as JSON text frames
//!   over `obswebsocket.json` or (#221 L2b) MessagePack binary frames over
//!   `obswebsocket.msgpack` — the encoding is `remote::codec`, one per session.
//!   obs-websocket-js REQUIRES the server to echo the subprotocol it offered
//!   ("Server sent no subprotocol" / "an invalid subprotocol" otherwise), and
//!   in Node (Companion) it offers only msgpack, see
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
//! Which request is answered how is [`route`]: a few natively (studio mode ON,
//! the per-session preview and, #221 L3, `GetCurrentProgramScene` from
//! SongPlayer's own program), the scene/input list getters forwarded to cg
//! OBS (#221 lane 2: `GetSceneList`'s program and preview scene replaced by
//! SongPlayer's, [`with_songplayer_scenes`]), a scene press
//! (`SetCurrentProgramScene`, `TriggerStudioModeTransition`) through the
//! program switch, anything else a well-formed
//! [`STATUS_UNKNOWN_REQUEST_TYPE`] error (Companion treats a failed request as
//! "no data" and stays connected — only `GetVersion` and
//! `GetStudioModeEnabled` must succeed, which is why they are native).
//!
//! Of cg OBS's events only `SceneListChanged` is passed through
//! ([`passthrough_intent`]); the program feedback and the transition events
//! are SongPlayer's own (#221 L3, `remote::studio_events`).

use serde_json::{Value, json};

use crate::obs::compute_auth;

/// The only RPC version (obs-websocket 5).
pub const RPC_VERSION: u64 = 1;
/// The JSON subprotocol (`Sec-WebSocket-Protocol`).
pub const SUBPROTOCOL_JSON: &str = "obswebsocket.json";
/// #221 L2b: the MessagePack subprotocol — the only one obs-websocket-js
/// offers in Node, so Companion's.
pub const SUBPROTOCOL_MSGPACK: &str = "obswebsocket.msgpack";
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
pub const STATUS_INVALID_REQUEST_FIELD_TYPE: u16 = 401;
pub const STATUS_REQUEST_FIELD_OUT_OF_RANGE: u16 = 402;
pub const STATUS_INVALID_RESOURCE_STATE: u16 = 604;

/// `SetCurrentSceneTransitionDuration`'s bounds, ms (obs-websocket's own).
pub const TRANSITION_DURATION_MIN_MS: u32 = 50;
pub const TRANSITION_DURATION_MAX_MS: u32 = 20_000;

/// `EventSubscription::Scenes` (1 << 2).
pub const EVENT_SCENES: u64 = 4;
/// `EventSubscription::Transitions` (1 << 4): `SceneTransitionStarted` /
/// `SceneTransitionEnded` (#221 L3).
pub const EVENT_TRANSITIONS: u64 = 16;
/// `EventSubscription::All` of rpcVersion 1 before Canvases (bits 0..=10) —
/// a session's subscriptions when its `Identify` names none.
pub const EVENT_ALL: u64 = 0x7FF;

/// The close reasons Companion's status mapping matches on (obs-websocket's
/// own wording): "missing an `authentication` string" → "Missing password",
/// "Authentication failed" → an authentication failure.
pub const REASON_AUTH_MISSING: &str = "Your payload's data is missing an `authentication` string, however authentication is required.";
pub const REASON_AUTH_FAILED: &str = "Authentication failed.";

/// The requests forwarded to cg OBS verbatim (the scene/input list getters).
/// #221 L3: `GetCurrentProgramScene` is no longer one — SP-program's scene is
/// SongPlayer's own.
pub const FORWARDED_REQUESTS: [&str; 4] = [
    "GetSceneList",
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
    /// `obswebsocket.json` was offered (alone or next to msgpack): echo it.
    Json,
    /// #221 L2b: `obswebsocket.msgpack` was offered and `obswebsocket.json`
    /// was not: echo it (Companion, obs-websocket-js in Node).
    MsgPack,
    /// Nothing was offered: JSON by default, no header.
    Default,
    /// Only protocols other than obs-websocket's two were offered: reject.
    Unsupported,
}

impl Subprotocol {
    /// The `Sec-WebSocket-Protocol` the server echoes: the one it picked,
    /// none when the client offered nothing (or was refused).
    pub fn echo(self) -> Option<&'static str> {
        match self {
            Self::Json => Some(SUBPROTOCOL_JSON),
            Self::MsgPack => Some(SUBPROTOCOL_MSGPACK),
            Self::Default | Self::Unsupported => None,
        }
    }
}

/// Pick the subprotocol for a comma-separated `Sec-WebSocket-Protocol` offer:
/// JSON whenever it is offered (the main session's decision, #221 L2b: a
/// JSON client stays exactly as before), else msgpack whenever it is offered
/// (also next to an unknown protocol, as obs-websocket takes the first
/// encoding it knows), else nothing to agree on.
pub fn negotiate_subprotocol(offered: Option<&str>) -> Subprotocol {
    let Some(offered) = offered.filter(|o| !o.trim().is_empty()) else {
        return Subprotocol::Default;
    };
    let offers = |name: &str| offered.split(',').any(|p| p.trim() == name);
    if offers(SUBPROTOCOL_JSON) {
        Subprotocol::Json
    } else if offers(SUBPROTOCOL_MSGPACK) {
        Subprotocol::MsgPack
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
        let expected = compute_auth(password, &self.challenge, &self.salt);
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
    /// `None` keeps the current subscriptions (obs-websocket's
    /// `SetSessionParameters` changes them only when the field is present).
    Reidentify {
        event_subscriptions: Option<u64>,
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

fn subscriptions(d: &Value) -> Option<u64> {
    d.get("eventSubscriptions").and_then(Value::as_u64)
}

/// Parse one decoded client message (#221 L2b: JSON or MessagePack, decoded
/// by the session's `remote::codec`), or the close obs-websocket answers a
/// malformed one with.
pub fn parse_client_message(msg: Value) -> Result<ClientMessage, CloseReason> {
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
            event_subscriptions: subscriptions(d).unwrap_or(EVENT_ALL),
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

/// The close for a decoded message that is not an object (obs-websocket's
/// wording). #221 L2b: a frame that does not decode at all, or a frame of
/// the other encoding, is closed by `remote::codec`, also with 4002.
pub fn decode_error() -> CloseReason {
    CloseReason::new(CLOSE_MESSAGE_DECODE_ERROR, "You sent a non-object payload.")
}

/// The close for anything but `Identify` before the session is identified.
pub const NOT_IDENTIFIED: CloseReason = CloseReason::new(
    CLOSE_NOT_IDENTIFIED,
    "You must send an `Identify` before anything else.",
);
/// The close for a client that did not identify within
/// `remote::IDENTIFY_TIMEOUT` (an idle unauthenticated socket is not kept).
pub const IDENTIFY_TIMED_OUT: CloseReason =
    CloseReason::new(CLOSE_NOT_IDENTIFIED, "No `Identify` arrived in time.");
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
/// subscribed to: only the scene list. #221 L3: cg OBS's
/// `CurrentProgramSceneChanged` is NOT passed through — Companion's program
/// feedback is SongPlayer's own program (`remote::studio_events`).
pub fn passthrough_intent(event_type: &str) -> Option<u64> {
    match event_type {
        "SceneListChanged" => Some(EVENT_SCENES),
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
    /// #221 lane 2: `GetSceneList`, forwarded to cg OBS, its program and
    /// preview scene SongPlayer's ([`with_songplayer_scenes`]).
    SceneList,
    /// A scene press: switch `SP-program` to `sceneName`.
    SetProgramScene,
    /// #221 L3: SP-program's scene (the one resolver), never cg OBS's.
    GetProgramScene,
    /// #221: set this session's preview scene.
    SetPreviewScene,
    /// #221: this session's preview scene (initially the program scene).
    GetPreviewScene,
    /// #221: a scene press of this session's preview scene.
    TriggerTransition,
    /// #221: validated and acknowledged, never applied.
    SetTransitionDuration,
    /// A well-formed [`STATUS_UNKNOWN_REQUEST_TYPE`] error.
    Unsupported,
}

/// Route one request type.
pub fn route(request_type: &str) -> Route {
    match request_type {
        "GetVersion" => Route::Native(Reply::ok(Some(version_data()))),
        // #221: studio mode ON — Companion v3.15.3 sends
        // `TriggerStudioModeTransition` only while it caches studio mode as
        // on (read at connect; `StudioModeStateChanged` is never emitted).
        "GetStudioModeEnabled" => {
            Route::Native(Reply::ok(Some(json!({ "studioModeEnabled": true }))))
        }
        "SetCurrentProgramScene" => Route::SetProgramScene,
        "GetCurrentProgramScene" => Route::GetProgramScene,
        "SetCurrentPreviewScene" => Route::SetPreviewScene,
        "GetCurrentPreviewScene" => Route::GetPreviewScene,
        "TriggerStudioModeTransition" => Route::TriggerTransition,
        "SetCurrentSceneTransitionDuration" => Route::SetTransitionDuration,
        // Keep in sync with FORWARDED_REQUESTS (pinned by a test).
        "GetSceneList" => Route::SceneList,
        "GetInputList" | "GetSceneItemList" | "GetGroupSceneItemList" => Route::Forward,
        _ => Route::Unsupported,
    }
}

/// Every request type the facade serves (`GetVersion.availableRequests`).
pub fn available_requests() -> Vec<&'static str> {
    let mut all = vec![
        "GetVersion",
        "GetStudioModeEnabled",
        "SetCurrentProgramScene",
        "GetCurrentProgramScene",
        "SetCurrentPreviewScene",
        "GetCurrentPreviewScene",
        "TriggerStudioModeTransition",
        "SetCurrentSceneTransitionDuration",
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

/// A scene request's `sceneName`, or the [`STATUS_MISSING_REQUEST_FIELD`]
/// error (a `sceneUuid` alone is not served: the switch is by scene name, and
/// Companion always sends the name).
pub fn scene_name(data: Option<&Value>) -> Result<String, Reply> {
    data.and_then(|d| d.get("sceneName"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            Reply::error(
                STATUS_MISSING_REQUEST_FIELD,
                "Your request is missing `sceneName` (the remote control switches by scene name).",
            )
        })
}

/// `GetCurrentPreviewScene`'s response data: the 5.x `sceneName` (what
/// Companion reads) and the 5.0 `currentPreviewSceneName`.
pub fn preview_scene_data(scene: &str) -> Value {
    json!({ "sceneName": scene, "currentPreviewSceneName": scene })
}

/// #221 L3: `GetCurrentProgramScene`'s response data, SP-program's scene: the
/// 5.x `sceneName` and the 5.0 `currentProgramSceneName` (what the post-deploy
/// E2E driver reads).
pub fn program_scene_data(scene: &str) -> Value {
    json!({ "sceneName": scene, "currentProgramSceneName": scene })
}

/// #221 lane 2 (ROZHODNUTÉ 6002459249): a forwarded `GetSceneList` answer's
/// `responseData`, its program and preview scene made SongPlayer's.
/// Companion's feedback at connect comes from it (v3.15.3 `buildSceneList`:
/// `currentProgramSceneName` → `scene_active`, `currentPreviewSceneName` →
/// `scene_preview`), and cg OBS's own program is not SP-program's whenever a
/// playlist is on it. So `currentProgramSceneName` = SP-program's scene
/// (`program`; `None` = nothing on it → null, never cg OBS's) and
/// `currentPreviewSceneName` = this session's preview (`preview`), each
/// `…Uuid` = cg OBS's uuid of the scene of that name in the answer's own
/// `scenes` (null when it lists none, e.g. "OBS manuál"). The rest is cg
/// OBS's, verbatim; data that is not an object is left as it is.
pub fn with_songplayer_scenes(data: &mut Value, program: Option<&str>, preview: Option<&str>) {
    let program_uuid = scene_uuid(data, program);
    let preview_uuid = scene_uuid(data, preview);
    let Some(fields) = data.as_object_mut() else {
        return;
    };
    fields.insert("currentProgramSceneName".into(), json!(program));
    fields.insert("currentProgramSceneUuid".into(), program_uuid);
    fields.insert("currentPreviewSceneName".into(), json!(preview));
    fields.insert("currentPreviewSceneUuid".into(), preview_uuid);
}

/// The `sceneUuid` of the scene named `name` in a `GetSceneList` answer's
/// `scenes`, else null.
fn scene_uuid(data: &Value, name: Option<&str>) -> Value {
    let found = name.and_then(|name| {
        data["scenes"]
            .as_array()?
            .iter()
            .find(|scene| scene["sceneName"] == name)?
            .get("sceneUuid")
            .cloned()
    });
    found.unwrap_or(Value::Null)
}

/// #221 L3: the error of `GetCurrentProgramScene` while SP-program has no
/// scene name: nothing is on it, or a playlist whose catalog names no scene
/// (604 `InvalidResourceState`, like [`no_scene`]).
pub fn nothing_on_program() -> Reply {
    Reply::error(STATUS_INVALID_RESOURCE_STATE, "Nothing is on SP-program.")
}

/// The error of a preview or transition request with no preview set and
/// nothing on `SP-program` (obs-websocket's 604 `InvalidResourceState`).
pub fn no_scene() -> Reply {
    Reply::error(
        STATUS_INVALID_RESOURCE_STATE,
        "No preview scene is set and nothing is on SP-program.",
    )
}

/// `SetCurrentSceneTransitionDuration`'s `transitionDuration`, validated
/// like obs-websocket: missing (or null) → [`STATUS_MISSING_REQUEST_FIELD`],
/// not a number → [`STATUS_INVALID_REQUEST_FIELD_TYPE`], outside
/// 50..=20000 ms → [`STATUS_REQUEST_FIELD_OUT_OF_RANGE`]. A fraction is
/// truncated to whole ms. One difference, as the design record specifies: a
/// request with no `requestData` at all is also 300 (obs-websocket answers
/// 301 `MissingRequestData`; Companion always sends the data).
pub fn transition_duration(data: Option<&Value>) -> Result<u32, Reply> {
    let value = data
        .and_then(|d| d.get("transitionDuration"))
        .filter(|v| !v.is_null());
    let Some(value) = value else {
        return Err(Reply::error(
            STATUS_MISSING_REQUEST_FIELD,
            "Your request is missing `transitionDuration`.",
        ));
    };
    let Some(ms) = value.as_f64() else {
        return Err(Reply::error(
            STATUS_INVALID_REQUEST_FIELD_TYPE,
            "The field value of `transitionDuration` must be a number.",
        ));
    };
    if ms < f64::from(TRANSITION_DURATION_MIN_MS) {
        return Err(Reply::error(
            STATUS_REQUEST_FIELD_OUT_OF_RANGE,
            "The field value of `transitionDuration` is below the minimum of `50`",
        ));
    }
    if ms > f64::from(TRANSITION_DURATION_MAX_MS) {
        return Err(Reply::error(
            STATUS_REQUEST_FIELD_OUT_OF_RANGE,
            "The field value of `transitionDuration` is above the maximum of `20000`",
        ));
    }
    Ok(ms as u32)
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
