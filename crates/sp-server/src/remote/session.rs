//! One remote-control client session (#213): the obs-websocket 5 handshake,
//! requests + batches, and the re-emitted cg OBS events.
//!
//! Requests of one session run strictly in order (Companion correlates by
//! `requestId`, so this is only a latency choice); remote scene presses are
//! additionally serialized across ALL sessions by `Facade::cut_order`.

use std::net::SocketAddr;
use std::sync::Arc;

use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message};
use tracing::{debug, info, warn};

use super::map::{self, KeepReason};
use super::protocol::{
    self, AuthChallenge, ClientMessage, CloseReason, Reply, RequestItem, Route,
    STATUS_GENERIC_ERROR, STATUS_MISSING_REQUEST_FIELD, STATUS_MISSING_REQUEST_TYPE,
    STATUS_NOT_READY, STATUS_UNKNOWN_REQUEST_TYPE, Subprotocol,
};
use super::{Facade, RemoteCut, now_ms};
use crate::obs::ObsEvent;
use crate::playback::ndi_input::load_input_settings;
use crate::playback::program_bus::persist_and_cut;

type WsWrite = SplitSink<WebSocketStream<TcpStream>, Message>;

/// The comment of a request cg OBS could not answer.
pub(crate) const NOT_READY_COMMENT: &str = "cg OBS is not reachable from SongPlayer right now";

/// Serve one client until it disconnects or the listener stops.
pub(crate) async fn run(stream: TcpStream, peer: SocketAddr, facade: Arc<Facade>) {
    let callback = |req: &Request, mut resp: Response| {
        let offered = req
            .headers()
            .get(SEC_WEBSOCKET_PROTOCOL)
            .and_then(|v| v.to_str().ok());
        match protocol::negotiate_subprotocol(offered) {
            Subprotocol::Json => {
                let json = HeaderValue::from_static(protocol::SUBPROTOCOL_JSON);
                resp.headers_mut().insert(SEC_WEBSOCKET_PROTOCOL, json);
                Ok(resp)
            }
            Subprotocol::Default => Ok(resp),
            Subprotocol::Unsupported => Err(refused()),
        }
    };
    let ws = match tokio_tungstenite::accept_hdr_async(stream, callback).await {
        Ok(ws) => ws,
        Err(e) => {
            info!(%peer, %e, "remote: WebSocket handshake refused");
            return;
        }
    };
    let _client = facade.shared().client_connected();
    info!(%peer, "remote: client connected");
    let (mut write, mut read) = ws.split();
    let mut events = facade.upstream.subscribe();
    let mut session = Session {
        facade: &facade,
        auth: facade.password.as_ref().map(|_| AuthChallenge::random()),
        identified: false,
        subscriptions: 0,
    };
    if send(&mut write, &protocol::hello(session.auth.as_ref()))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            // Events first: a cg OBS event that arrived before a client message is
            // delivered under the subscriptions that were active when it arrived.
            biased;
            event = events.recv() => match event {
                Ok(ObsEvent::Raw { event_type, event_data }) => {
                    if let Some(msg) = session.event(&event_type, &event_data)
                        && send(&mut write, &msg).await.is_err()
                    {
                        break;
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(n)) => warn!(%peer, n, "remote: the client missed cg OBS events"),
                Err(RecvError::Closed) => break,
            },
            incoming = read.next() => {
                let step = match incoming {
                    Some(Ok(Message::Text(text))) => session.on_text(&text).await,
                    Some(Ok(Message::Binary(_))) => Step::Close(protocol::decode_error()),
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => continue, // ping / pong / raw frame
                    Some(Err(e)) => {
                        debug!(%peer, %e, "remote: reading from the client failed");
                        break;
                    }
                };
                match step {
                    Step::Send(msgs) => {
                        if send_all(&mut write, &msgs).await.is_err() {
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

/// The handshake answer to a client offering only non-JSON subprotocols.
fn refused() -> ErrorResponse {
    let mut refused = ErrorResponse::new(Some(
        "SongPlayer's remote control speaks obswebsocket.json only".to_string(),
    ));
    *refused.status_mut() = StatusCode::BAD_REQUEST;
    refused
}

async fn send(write: &mut WsWrite, msg: &Value) -> Result<(), tungstenite::Error> {
    write.send(Message::Text(msg.to_string().into())).await
}

async fn send_all(write: &mut WsWrite, msgs: &[Value]) -> Result<(), tungstenite::Error> {
    for msg in msgs {
        send(write, msg).await?;
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
}

impl Session<'_> {
    /// Act on one text frame.
    pub(crate) async fn on_text(&mut self, text: &str) -> Step {
        let msg = match protocol::parse_client_message(text) {
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
                self.subscriptions = event_subscriptions;
                Step::Send(vec![protocol::identified()])
            }
            ClientMessage::Request(item) => {
                let reply = execute(self.facade, &item).await;
                let request_type = item.request_type.as_deref().unwrap_or_default();
                let request_id = item.request_id.as_deref().unwrap_or_default();
                Step::Send(vec![protocol::request_response(
                    request_type,
                    request_id,
                    &reply,
                )])
            }
            ClientMessage::Batch {
                request_id,
                halt_on_failure,
                requests,
            } => {
                let mut results = Vec::with_capacity(requests.len());
                for item in &requests {
                    let reply = execute(self.facade, item).await;
                    results.push(protocol::batch_result(item, &reply));
                    if halt_on_failure && !reply.succeeded() {
                        break;
                    }
                }
                Step::Send(vec![protocol::batch_response(&request_id, results)])
            }
        }
    }

    /// The `Event` for one cg OBS event, when it is re-emitted and this
    /// client subscribed to its intent.
    pub(crate) fn event(&self, event_type: &str, data: &Value) -> Option<Value> {
        let intent = protocol::passthrough_intent(event_type)?;
        protocol::subscribed(self.subscriptions, intent)
            .then(|| protocol::event(event_type, intent, data))
    }
}

/// Answer one request (a single one or a batch entry).
pub(crate) async fn execute(facade: &Facade, item: &RequestItem) -> Reply {
    let Some(request_type) = item.request_type.as_deref() else {
        return Reply::error(
            STATUS_MISSING_REQUEST_TYPE,
            "Your request is missing a `requestType`.",
        );
    };
    facade.shared().record_request(request_type);
    match protocol::route(request_type) {
        Route::Native(reply) => reply,
        Route::Forward => forward(facade, request_type, item.request_data.clone()).await,
        Route::SetProgramScene => set_program_scene(facade, item.request_data.clone()).await,
        Route::Unsupported => {
            if facade.shared().note_unsupported(request_type) {
                info!(
                    request_type,
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

/// Forward one request to cg OBS and pass its answer through.
async fn forward(facade: &Facade, request_type: &str, data: Option<Value>) -> Reply {
    match facade.upstream.request(request_type, data).await {
        Some(d) => Reply::from_upstream(&d),
        None => Reply::error(STATUS_NOT_READY, NOT_READY_COMMENT),
    }
}

/// `SetCurrentProgramScene(X)`: forward the switch to cg OBS, then cut
/// `SP-program` per `map::scene_action` (the playlist X shows, else "OBS
/// manuál" while it is a source, else nothing + a WARN). The client gets cg
/// OBS's own answer, or an error when the cut could not be persisted.
async fn set_program_scene(facade: &Facade, data: Option<Value>) -> Reply {
    let scene = data
        .as_ref()
        .and_then(|d| d.get("sceneName"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(scene) = scene else {
        return Reply::error(
            STATUS_MISSING_REQUEST_FIELD,
            "Your request is missing `sceneName` (the remote control cuts by scene name).",
        );
    };
    let _order = facade.cut_order.lock().await;
    let reply = forward(facade, "SetCurrentProgramScene", data).await;
    let playlists = if reply.succeeded() {
        Some(
            facade
                .upstream
                .scene_playlists(&scene)
                .await
                .unwrap_or_default(),
        )
    } else {
        None
    };
    let input_active = load_input_settings(&facade.pool)
        .await
        .is_ok_and(|s| s.active());
    let action = map::scene_action(playlists.as_ref(), input_active);
    let mut cut = RemoteCut {
        scene,
        action: action.label(),
        source: action.source(),
        reason: action.keep_reason().map(KeepReason::as_str),
        cut_boundary_100ns: None,
        at_ms: now_ms(),
    };
    let reply = match action.source() {
        None => {
            warn!(scene = %cut.scene, reason = ?cut.reason, "remote: SP-program unchanged");
            reply
        }
        Some(source) => match persist_and_cut(&facade.pool, &facade.bus, source).await {
            Ok(status) => {
                info!(
                    scene = %cut.scene,
                    source,
                    cut_boundary_100ns = ?status.cut_boundary_100ns,
                    "remote: program cut"
                );
                cut.cut_boundary_100ns = status.cut_boundary_100ns;
                reply
            }
            Err(e) => {
                warn!(
                    scene = %cut.scene,
                    source,
                    %e,
                    "remote: persisting the program source failed — nothing cut"
                );
                cut.action = "keep";
                cut.source = None;
                cut.reason = Some("persist_failed");
                Reply::error(
                    STATUS_GENERIC_ERROR,
                    &format!("cg OBS switched, but SP-program was not cut: {e}"),
                )
            }
        },
    };
    facade.shared().record_cut(cut);
    reply
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
