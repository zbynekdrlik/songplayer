//! One remote-control client session (#213): the obs-websocket 5 handshake,
//! requests + batches, cg OBS's re-emitted scene list, and (#221 L3) the
//! facade's own events: SongPlayer's program feedback and the transition
//! events (`studio_events`).
//!
//! #221 L2b: the handshake picks the session's encoding (`codec::Codec`: JSON
//! text or MessagePack binary — Companion speaks msgpack), and EVERY message
//! of the session goes through it, both ways: Hello, Identified, responses,
//! batch responses, events. A frame of the other kind closes it with 4002.
//!
//! Requests of one session run strictly in order — load-bearing since #221:
//! a transition switches to the preview the same client set just before.
//! Scene presses are additionally serialized across ALL sessions by the
//! program switch (`playback::program_switch`, the bus's `switch_order`).
//!
//! #221: each session keeps its OWN preview scene (`SetCurrentPreviewScene`;
//! until it sets one, the preview is the program scene). Companion's
//! preview → transition pair and another client (the post-deploy E2E driver)
//! can never trigger each other's preview.

use std::net::SocketAddr;
use std::sync::Arc;

use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::broadcast::error::RecvError;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tracing::{debug, info, warn};

use super::codec::Codec;
use super::protocol::{
    self, AuthChallenge, ClientMessage, CloseReason, EVENT_SCENES, Reply, RequestItem, Route,
    STATUS_GENERIC_ERROR, STATUS_MISSING_REQUEST_TYPE, STATUS_NOT_READY,
    STATUS_UNKNOWN_REQUEST_TYPE,
};
use super::studio_events::{self, FacadeEvent};
use super::{Facade, MAX_MESSAGE_BYTES, MAX_SESSIONS, clip};
use crate::obs::ObsEvent;
use crate::playback::program_on_air::program_scene_name;
use crate::playback::program_switch::{SwitchCtx, Switched, Via, switch_scene};

type WsWrite = SplitSink<WebSocketStream<TcpStream>, Message>;

/// The comment of a request cg OBS could not answer.
pub(crate) const NOT_READY_COMMENT: &str = "cg OBS is not reachable from SongPlayer right now";

/// Serve one client until it disconnects or the listener stops.
pub(crate) async fn run(stream: TcpStream, peer: SocketAddr, facade: Arc<Facade>) {
    // ONE deadline for the whole unidentified phase — the WebSocket handshake
    // AND the `Identify` — so an idle socket is never kept.
    let identify_deadline = tokio::time::Instant::now() + facade.identify_timeout;
    // #221 L2b: the session's encoding, set by the handshake below.
    let mut codec = Codec::Json;
    // #221: the session's slot of `MAX_SESSIONS`, taken by the handshake below
    // and held until the session ends; `over_cap`: no slot was free.
    let mut slot: Option<OwnedSemaphorePermit> = None;
    let mut over_cap = false;
    // tungstenite's `Callback` fixes this `Result<Response, ErrorResponse>`
    // shape (an http `Response`, > 128 B), so the size lint cannot be met here.
    #[allow(clippy::result_large_err)]
    let callback = |req: &Request, mut resp: Response| {
        let offered = req
            .headers()
            .get(SEC_WEBSOCKET_PROTOCOL)
            .and_then(|v| v.to_str().ok());
        let negotiated = protocol::negotiate_subprotocol(offered);
        let Some(chosen) = Codec::for_subprotocol(negotiated) else {
            // The HTTP error logged below says only "400"; the offer is what a
            // cutover diagnosis needs (#221 comment 5881650057).
            info!(%peer, offered = ?offered.map(clip), "remote: neither obs-websocket subprotocol offered");
            return Err(refused());
        };
        let Ok(permit) = Arc::clone(&facade.sessions).try_acquire_owned() else {
            over_cap = true;
            return Err(refused_over_cap(&facade, peer));
        };
        slot = Some(permit);
        codec = chosen;
        if let Some(name) = negotiated.echo() {
            let echo = HeaderValue::from_static(name);
            resp.headers_mut().insert(SEC_WEBSOCKET_PROTOCOL, echo);
        }
        Ok(resp)
    };
    // A frame or message over 1 MiB is refused before it is buffered.
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let accepted = tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(config));
    let ws = match tokio::time::timeout_at(identify_deadline, accepted).await {
        Ok(Ok(ws)) => ws,
        Ok(Err(e)) => {
            log_refused_handshake(peer, &e, over_cap);
            return;
        }
        Err(_) => {
            info!(%peer, "remote: no WebSocket handshake in time — dropped");
            return;
        }
    };
    let _client = facade.shared().client_connected();
    // Declared after `_client`, so it is dropped first: once `remote.clients`
    // no longer counts this session, its slot is free.
    let _slot = slot;
    info!(%peer, encoding = ?codec, "remote: client connected");
    let (mut write, mut read) = ws.split();
    let mut events = facade.upstream.subscribe();
    let mut own_events = facade.events.subscribe();
    let mut session = Session {
        facade: &facade,
        auth: facade.password.as_ref().map(|_| AuthChallenge::random()),
        identified: false,
        subscriptions: 0,
        preview: None,
    };
    if send(&mut write, codec, &protocol::hello(session.auth.as_ref()))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            // Events first: a cg OBS event that arrived before a client message is
            // delivered under the subscriptions that were active when it arrived.
            // The identify deadline comes before the client's messages, so a
            // stream of pings cannot keep an unidentified session alive.
            biased;
            event = events.recv() => match event {
                Ok(ObsEvent::Raw { event_type, event_data }) => {
                    if let Some(msg) = session.event(&event_type, &event_data)
                        && send(&mut write, codec, &msg).await.is_err()
                    {
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => warn!(%peer, n, "remote: the client missed cg OBS events"),
                Err(RecvError::Closed) => break,
            },
            // #221 L3: SongPlayer's own program feedback and transition events.
            event = own_events.recv() => match event {
                Ok(event) => {
                    if let Some(msg) = session.own_event(&event)
                        && send(&mut write, codec, &msg).await.is_err()
                    {
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => warn!(%peer, n, "remote: the client missed SongPlayer's own events"),
                Err(RecvError::Closed) => break,
            },
            _ = tokio::time::sleep_until(identify_deadline), if !session.identified => {
                info!(%peer, "remote: no Identify in time — closing the session");
                close(&mut write, &protocol::IDENTIFY_TIMED_OUT).await;
                break;
            }
            incoming = read.next() => {
                // #221 L2b: decoded by the session's codec; a frame of the
                // other encoding, or one that does not decode, is a 4002.
                let decoded = match incoming {
                    Some(Ok(Message::Text(text))) => codec.decode_text(&text),
                    Some(Ok(Message::Binary(bytes))) => codec.decode_binary(&bytes),
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => continue, // ping / pong / raw frame
                    Some(Err(e)) => {
                        debug!(%peer, %e, "remote: reading from the client failed");
                        break;
                    }
                };
                let step = match decoded {
                    Ok(msg) => session.on_message(msg).await,
                    Err(reason) => Step::Close(reason),
                };
                match step {
                    Step::Send(msgs) => {
                        if send_all(&mut write, codec, &msgs).await.is_err() {
                            break;
                        }
                    }
                    Step::Close(reason) => {
                        info!(%peer, code = reason.code, reason = reason.reason, "remote: closing the session");
                        close(&mut write, &reason).await;
                        break;
                    }
                }
            }
        }
    }
    info!(%peer, "remote: client disconnected");
}

/// The handshake answer to a client that offered subprotocols, but neither of
/// obs-websocket's two.
fn refused() -> ErrorResponse {
    let mut refused = ErrorResponse::new(Some(
        "SongPlayer's remote control speaks obswebsocket.json or obswebsocket.msgpack only"
            .to_string(),
    ));
    *refused.status_mut() = StatusCode::BAD_REQUEST;
    refused
}

/// The log line of a refused handshake. An over-cap refusal (`over_cap`) was
/// already logged, rate-limited, by [`refused_over_cap`]. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_refused_handshake(
    peer: SocketAddr,
    e: &tokio_tungstenite::tungstenite::Error,
    over_cap: bool,
) {
    if !over_cap {
        info!(%peer, %e, "remote: WebSocket handshake refused");
    }
}

/// #221: the handshake answer while `MAX_SESSIONS` sessions are open — HTTP
/// 503, counted as `remote.refused_over_cap`, logged at most once per
/// `REFUSAL_LOG_INTERVAL` (a client retrying every few seconds must not flood
/// the log).
fn refused_over_cap(facade: &Facade, peer: SocketAddr) -> ErrorResponse {
    let (refused, log) = facade
        .shared()
        .note_refused_over_cap(std::time::Instant::now());
    if log {
        info!(
            %peer,
            max_sessions = MAX_SESSIONS,
            refused_total = refused,
            "remote: handshake refused with 503 — every session slot is taken (logged at most once per 10 s)"
        );
    }
    let mut refusal = ErrorResponse::new(Some(format!(
        "SongPlayer's remote control serves at most {MAX_SESSIONS} clients at once"
    )));
    *refusal.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    refusal
}

/// Send one message in the session's encoding (`codec`). `Err` ends the
/// session: the socket failed, or the codec could not encode the message
/// (never for a `serde_json::Value`, see `Codec::encode`).
async fn send(write: &mut WsWrite, codec: Codec, msg: &Value) -> Result<(), ()> {
    let frame = codec.encode(msg).map_err(|e| {
        warn!(%e, ?codec, "remote: a message could not be encoded — ending the session");
    })?;
    write
        .send(frame)
        .await
        .map_err(|e| debug!(%e, "remote: writing to the client failed"))
}

async fn send_all(write: &mut WsWrite, codec: Codec, msgs: &[Value]) -> Result<(), ()> {
    for msg in msgs {
        send(write, codec, msg).await?;
    }
    Ok(())
}

async fn close(write: &mut WsWrite, reason: &CloseReason) {
    let frame = CloseFrame {
        code: CloseCode::from(reason.code),
        reason: reason.reason.into(),
    };
    let _ = write.send(Message::Close(Some(frame))).await;
}

/// What the session does with one client message.
pub(crate) enum Step {
    Send(Vec<Value>),
    Close(CloseReason),
}

/// One client's protocol state.
pub(crate) struct Session<'a> {
    pub(crate) facade: &'a Facade,
    /// This session's challenge (a password is set).
    pub(crate) auth: Option<AuthChallenge>,
    pub(crate) identified: bool,
    /// The `eventSubscriptions` bitmask, 0 until `Identify` — so no event
    /// reaches a client before it identified.
    pub(crate) subscriptions: u64,
    /// #221: this client's preview scene; `None` until it sets one (the
    /// preview then reads as the program scene).
    pub(crate) preview: Option<String>,
}

impl Session<'_> {
    /// Act on one decoded client message (#221 L2b: JSON or MessagePack).
    pub(crate) async fn on_message(&mut self, msg: Value) -> Step {
        let msg = match protocol::parse_client_message(msg) {
            Ok(msg) => msg,
            Err(reason) => return Step::Close(reason),
        };
        match msg {
            ClientMessage::Identify {
                rpc_version,
                authentication,
                event_subscriptions,
            } => {
                if self.identified {
                    return Step::Close(protocol::ALREADY_IDENTIFIED);
                }
                let auth = self.auth.as_ref().zip(self.facade.password.as_deref());
                let checked =
                    protocol::check_identify(rpc_version, authentication.as_deref(), auth);
                if let Err(reason) = checked {
                    return Step::Close(reason);
                }
                self.identified = true;
                self.subscriptions = event_subscriptions;
                Step::Send(vec![protocol::identified()])
            }
            _ if !self.identified => Step::Close(protocol::NOT_IDENTIFIED),
            ClientMessage::Reidentify {
                event_subscriptions,
            } => {
                if let Some(subscriptions) = event_subscriptions {
                    self.subscriptions = subscriptions;
                }
                Step::Send(vec![protocol::identified()])
            }
            ClientMessage::Request(item) => {
                let mut events = Vec::new();
                let reply = self.execute(&item, &mut events).await;
                let request_type = item.request_type.as_deref().unwrap_or_default();
                let request_id = item.request_id.as_deref().unwrap_or_default();
                let mut msgs = vec![protocol::request_response(request_type, request_id, &reply)];
                msgs.append(&mut events);
                Step::Send(msgs)
            }
            ClientMessage::Batch {
                request_id,
                halt_on_failure,
                requests,
            } => {
                let mut events = Vec::new();
                let mut results = Vec::with_capacity(requests.len());
                for item in &requests {
                    let reply = self.execute(item, &mut events).await;
                    results.push(protocol::batch_result(item, &reply));
                    if halt_on_failure && !reply.succeeded() {
                        break;
                    }
                }
                let mut msgs = vec![protocol::batch_response(&request_id, results)];
                msgs.append(&mut events);
                Step::Send(msgs)
            }
        }
    }

    /// Answer one request (a single one or a batch entry). The events it
    /// causes for THIS client (#221: `CurrentPreviewSceneChanged`) go to
    /// `events`, sent after the response.
    pub(crate) async fn execute(&mut self, item: &RequestItem, events: &mut Vec<Value>) -> Reply {
        let facade = self.facade;
        let Some(request_type) = item.request_type.as_deref() else {
            return Reply::error(
                STATUS_MISSING_REQUEST_TYPE,
                "Your request is missing a `requestType`.",
            );
        };
        facade.shared().record_request(request_type);
        let data = item.request_data.as_ref();
        match protocol::route(request_type) {
            Route::Native(reply) => reply,
            Route::Forward => forward(facade, request_type, item.request_data.clone()).await,
            // #221 lane 2: cg OBS's list, SongPlayer's program and preview;
            // #245: with Blank in it.
            Route::SceneList => {
                let mut reply = forward(facade, request_type, item.request_data.clone()).await;
                if reply.succeeded()
                    && let Some(data) = reply.data.as_mut()
                {
                    let program = program_scene_name(&facade.bus.on_air_now());
                    let preview = self.preview_given(program.clone());
                    protocol::with_blank_scene(data);
                    protocol::with_songplayer_scenes(data, program.as_deref(), preview.as_deref());
                }
                reply
            }
            Route::SetProgramScene => match protocol::scene_name(data) {
                Ok(scene) => switch(facade, &scene, Via::Program).await,
                Err(reply) => reply,
            },
            // #221 L3: SP-program's scene, never cg OBS's.
            Route::GetProgramScene => match program_scene_name(&facade.bus.on_air_now()) {
                Some(scene) => Reply::ok(Some(protocol::program_scene_data(&scene))),
                None => protocol::nothing_on_program(),
            },
            Route::SetPreviewScene => match protocol::scene_name(data) {
                Ok(scene) => self.set_preview(scene, events),
                Err(reply) => reply,
            },
            Route::GetPreviewScene => match self.preview_scene() {
                Some(scene) => Reply::ok(Some(protocol::preview_scene_data(&scene))),
                None => protocol::no_scene(),
            },
            // ALWAYS a switch, also to the scene already on program: a
            // same-source cut is a bus no-op whose publication still counts.
            Route::TriggerTransition => match self.preview_scene() {
                Some(scene) => switch(facade, &scene, Via::Transition).await,
                None => protocol::no_scene(),
            },
            Route::SetTransitionDuration => match protocol::transition_duration(data) {
                Ok(ms) => {
                    facade.shared().record_transition_duration(ms);
                    info!(
                        ms,
                        "remote: transition duration received — not applied (the program transition is the Settings value)"
                    );
                    Reply::ok(None)
                }
                Err(reply) => reply,
            },
            Route::Unsupported => {
                if facade.shared().note_unsupported(request_type) {
                    info!(
                        request_type = %clip(request_type),
                        "remote: a client asked for a request the facade does not serve (logged once per type)"
                    );
                }
                Reply::error(
                    STATUS_UNKNOWN_REQUEST_TYPE,
                    &protocol::unsupported_comment(request_type),
                )
            }
        }
    }

    /// This client's preview scene: the one it set, else the program scene
    /// (`program_scene_name`), else none.
    fn preview_scene(&self) -> Option<String> {
        self.preview_given(program_scene_name(&self.facade.bus.on_air_now()))
    }

    /// This client's preview scene with SP-program's scene `program`: the
    /// one it set, else `program`.
    fn preview_given(&self, program: Option<String>) -> Option<String> {
        self.preview.clone().or(program)
    }

    /// `SetCurrentPreviewScene`: store it and, for a client subscribed to
    /// Scenes, queue `CurrentPreviewSceneChanged`. No validation: a playlist
    /// scene is known from the catalog, and cg OBS validates a manual one at
    /// the transition.
    fn set_preview(&mut self, scene: String, events: &mut Vec<Value>) -> Reply {
        if protocol::subscribed(self.subscriptions, EVENT_SCENES) {
            let data = json!({ "sceneName": scene });
            events.push(protocol::event(
                "CurrentPreviewSceneChanged",
                EVENT_SCENES,
                &data,
            ));
        }
        self.preview = Some(scene);
        Reply::ok(None)
    }

    /// The `Event` for one cg OBS event, when it is re-emitted and this
    /// client subscribed to its intent.
    pub(crate) fn event(&self, event_type: &str, data: &Value) -> Option<Value> {
        let intent = protocol::passthrough_intent(event_type)?;
        protocol::subscribed(self.subscriptions, intent)
            .then(|| protocol::event(event_type, intent, data))
    }

    /// #221 L3: the `Event` for one of the facade's own events, when this
    /// client subscribed to its intent.
    pub(crate) fn own_event(&self, event: &FacadeEvent) -> Option<Value> {
        protocol::subscribed(self.subscriptions, event.intent)
            .then(|| protocol::event(event.event_type, event.intent, &event.data))
    }
}

/// Forward one request to cg OBS and pass its answer through.
async fn forward(facade: &Facade, request_type: &str, data: Option<Value>) -> Reply {
    match facade.upstream.request(request_type, data).await {
        Some(d) => Reply::from_upstream(&d),
        None => Reply::error(STATUS_NOT_READY, NOT_READY_COMMENT),
    }
}

/// A scene press through the ONE switch path (`program_switch`), answered the
/// obs-websocket way: 100 for a cut or a kept program, cg OBS's own answer
/// when it refused a manual scene, 207 when it was not reachable, 205 when
/// the store failed. #221 L3: a cut is announced as a transition
/// (`SceneTransitionStarted`, then `SceneTransitionEnded` once the window is
/// served).
async fn switch(facade: &Facade, scene: &str, via: Via) -> Reply {
    let ctx = SwitchCtx {
        pool: &facade.pool,
        bus: &facade.bus,
        upstream: &facade.upstream,
    };
    match switch_scene(&ctx, scene, via).await {
        Switched::Cut => {
            let max = facade.transition_end_max;
            studio_events::announce_transition(&facade.bus, &facade.events, max);
            Reply::ok(None)
        }
        Switched::Kept => Reply::ok(None),
        Switched::NotSwitched(Some(d)) => Reply::from_upstream(&d),
        Switched::NotSwitched(None) => Reply::error(STATUS_NOT_READY, NOT_READY_COMMENT),
        Switched::StoreFailed(e) => Reply::error(
            STATUS_GENERIC_ERROR,
            &format!("SP-program was not switched: {e}"),
        ),
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "session_tests_cap.rs"]
mod tests_cap;
#[cfg(test)]
#[path = "session_tests_feedback.rs"]
mod tests_feedback;
#[cfg(test)]
#[path = "session_tests_msgpack.rs"]
mod tests_msgpack;
#[cfg(test)]
#[path = "session_tests_studio.rs"]
mod tests_studio;
