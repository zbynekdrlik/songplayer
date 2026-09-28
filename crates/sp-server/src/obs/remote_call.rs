//! #213: the remote-control facade's calls to cg OBS (`ObsCommand::Remote`).
//!
//! The facade (`crate::remote`) never opens a second connection to cg OBS. It
//! hands a [`RemoteCall`] to SongPlayer's existing OBS client, whose connection
//! loop runs it here on its own write half + dispatcher:
//!
//! - [`RemoteCall::Request`] forwards one obs-websocket request verbatim and
//!   replies with the op=7 `d` object (`requestStatus` + `responseData`) —
//!   the scene list and program scene reach Companion 1:1;
//! - [`RemoteCall::ScenePlaylists`] asks which playlists a scene shows, with the
//!   SAME [`check_scene_items`] SongPlayer's own scene detection uses, over the
//!   same `NdiSourceMap` — so "the scene shows exactly one playlist" means what
//!   the scene-go-on playback path means by it.
//!
//! The facade waits for a reply only for a bounded time. A call whose requester
//! already gave up (`reply.is_closed()`, e.g. queued while cg OBS was away) is
//! dropped unexecuted: a stale `SetCurrentProgramScene` must never switch cg OBS
//! seconds after the button press was answered as "not ready".

use std::collections::HashSet;

use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher};
use crate::obs::scene::check_scene_items;
use crate::obs::{NdiSourceMap, SharedWrite};

/// One call of the remote-control facade to cg OBS.
#[derive(Debug)]
pub enum RemoteCall {
    /// Forward `requestType` + `requestData` verbatim. The reply is the op=7
    /// `d` object, `None` when cg OBS did not answer in time.
    Request {
        request_type: String,
        request_data: Option<serde_json::Value>,
        reply: oneshot::Sender<Option<serde_json::Value>>,
    },
    /// The playlist ids whose SongPlayer NDI source is in `scene` (nested
    /// scenes and groups included). A failed lookup gets no reply (#218).
    ScenePlaylists {
        scene: String,
        reply: oneshot::Sender<HashSet<i64>>,
    },
}

/// Run one [`RemoteCall`] on the OBS connection (a task spawned by the
/// connection loop).
pub async fn run(
    write: SharedWrite,
    dispatcher: Dispatcher,
    ndi_sources: NdiSourceMap,
    call: RemoteCall,
) {
    match call {
        RemoteCall::Request {
            request_type,
            request_data,
            reply,
        } => {
            if reply.is_closed() {
                debug!(request_type, "remote: the caller gave up — not forwarded");
                return;
            }
            let d = forward(&write, &dispatcher, &request_type, request_data).await;
            let _ = reply.send(d);
        }
        RemoteCall::ScenePlaylists { scene, reply } => {
            if reply.is_closed() {
                debug!(scene, "remote: the caller gave up — scene not looked up");
                return;
            }
            let map = ndi_sources.read().await;
            let lookup = check_scene_items(&write, &dispatcher, &scene, &map).await;
            drop(map);
            match lookup {
                Ok(ids) => {
                    let _ = reply.send(ids);
                }
                // #218: a failed lookup is not "no playlist". No reply: the
                // facade reads it as a lookup that got no answer.
                Err(e) => warn!(scene, error = %e, "remote: the scene's playlist lookup failed"),
            }
        }
    }
}

/// Send one request to cg OBS and return its op=7 `d` object.
async fn forward(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    request_type: &str,
    request_data: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    let req_id = uuid::Uuid::new_v4().to_string();
    let msg = forward_request(request_type, &req_id, request_data);
    match dispatcher
        .send_and_await(
            write,
            req_id,
            Message::Text(msg.to_string().into()),
            DEFAULT_RESPONSE_TIMEOUT,
        )
        .await
    {
        Ok(mut response) => response.get_mut("d").map(serde_json::Value::take),
        Err(e) => {
            warn!(request_type, %e, "remote: forwarding a request to cg OBS failed");
            None
        }
    }
}

/// The op=6 request forwarded to cg OBS under SongPlayer's own request id.
pub fn forward_request(
    request_type: &str,
    request_id: &str,
    request_data: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut d = serde_json::json!({
        "requestType": request_type,
        "requestId": request_id,
    });
    if let Some(data) = request_data {
        d["requestData"] = data;
    }
    serde_json::json!({ "op": 6, "d": d })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_request_carries_the_type_our_id_and_the_data_verbatim() {
        let data = serde_json::json!({ "sceneName": "sp-fast" });
        let msg = forward_request("SetCurrentProgramScene", "id-1", Some(data.clone()));
        assert_eq!(msg["op"], 6);
        assert_eq!(msg["d"]["requestType"], "SetCurrentProgramScene");
        assert_eq!(msg["d"]["requestId"], "id-1");
        assert_eq!(msg["d"]["requestData"], data);
    }

    #[test]
    fn forward_request_without_data_has_no_request_data_key() {
        let msg = forward_request("GetSceneList", "id-2", None);
        assert_eq!(msg["d"]["requestType"], "GetSceneList");
        assert!(msg["d"].get("requestData").is_none());
    }

    /// A call whose requester already gave up never reaches cg OBS — the
    /// first request a real WebSocket peer receives is the LIVE call run after
    /// two abandoned ones (a stale `SetCurrentProgramScene` must never switch
    /// cg OBS late). Deterministic: the abandoned calls are awaited first.
    #[tokio::test]
    async fn an_abandoned_call_is_never_sent_and_a_live_one_is() {
        use futures::StreamExt;
        use std::time::Duration;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cg_obs = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            loop {
                match ws.next().await {
                    Some(Ok(Message::Text(text))) => {
                        return serde_json::from_str::<serde_json::Value>(&text).unwrap();
                    }
                    Some(Ok(_)) => {}
                    other => panic!("cg OBS got no request: {other:?}"),
                }
            }
        });
        let (client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .unwrap();
        let (write, _read) = client.split();
        let write: SharedWrite = std::sync::Arc::new(tokio::sync::Mutex::new(write));
        let dispatcher = Dispatcher::new();
        let map: NdiSourceMap = Default::default();

        let (reply, rx) = oneshot::channel();
        drop(rx);
        let stale = RemoteCall::Request {
            request_type: "SetCurrentProgramScene".to_string(),
            request_data: Some(serde_json::json!({ "sceneName": "stale" })),
            reply,
        };
        run(write.clone(), dispatcher.clone(), map.clone(), stale).await;
        let (reply, rx) = oneshot::channel();
        drop(rx);
        let lookup = RemoteCall::ScenePlaylists {
            scene: "stale".to_string(),
            reply,
        };
        run(write.clone(), dispatcher.clone(), map.clone(), lookup).await;

        let (reply, _rx) = oneshot::channel();
        let live = RemoteCall::Request {
            request_type: "GetSceneList".to_string(),
            request_data: None,
            reply,
        };
        let live = tokio::spawn(run(write, dispatcher, map, live));
        let first = tokio::time::timeout(Duration::from_secs(10), cg_obs)
            .await
            .expect("cg OBS received nothing within 10 s")
            .unwrap();
        assert_eq!(first["op"], 6);
        assert_eq!(first["d"]["requestType"], "GetSceneList");
        live.abort();
    }
}
