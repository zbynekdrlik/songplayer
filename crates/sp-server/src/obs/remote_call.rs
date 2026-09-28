//! #213: the remote-control facade's calls to cg OBS (`ObsCommand::Remote`).
//!
//! The facade (`crate::remote`) never opens a second connection to cg OBS. It
//! hands a [`RemoteCall`] to SongPlayer's existing OBS client, whose connection
//! loop passes it to this connection's ONE forwarder, [`run_calls`], on its
//! own write half + dispatcher: [`RemoteCall::Request`] forwards one
//! obs-websocket request verbatim and replies with the op=7 `d` object
//! (`requestStatus` + `responseData`) — the scene list and program scene
//! reach Companion 1:1, a manual scene press and a playlist press's mirror
//! switch cg OBS. #221 deleted the scene → playlists lookup: the switch
//! decides from SongPlayer's own playlists (`playback::scene_catalog`).
//!
//! #221: the calls go out IN QUEUE ORDER. The forwarder writes each request
//! frame before it takes the next call, and only the wait for an answer runs
//! beside the later calls. A task per call (the #213 shape) could start the
//! newest first — a multi-thread tokio worker runs the task spawned last from
//! its LIFO slot — so a playlist press's mirror, which the facade does not
//! await, could reach cg OBS after a later manual press's forward, leaving cg
//! OBS on the older scene.
//!
//! The facade waits for a reply only for a bounded time. A call whose requester
//! already gave up (`reply.is_closed()`, e.g. queued while cg OBS was away) is
//! dropped unexecuted: a stale `SetCurrentProgramScene` must never switch cg OBS
//! seconds after the button press was answered as "not ready".

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use crate::obs::SharedWrite;
use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher};

/// One call of the remote-control facade to cg OBS.
#[derive(Debug)]
pub enum RemoteCall {
    /// Forward `requestType` + `requestData` verbatim. The reply is the op=7
    /// `d` object, `None` when cg OBS did not answer in time.
    Request {
        request_type: String,
        request_data: Option<Value>,
        reply: oneshot::Sender<Option<Value>>,
    },
}

/// The connection's forwarder (one task per OBS connection, in the
/// connection's task set): run the facade's calls in queue order until the
/// connection loop drops its sender or the task is aborted on a disconnect.
pub async fn run_calls(
    write: SharedWrite,
    dispatcher: Dispatcher,
    mut calls: mpsc::UnboundedReceiver<RemoteCall>,
) {
    while let Some(call) = calls.recv().await {
        send_call(&write, &dispatcher, call).await;
    }
}

/// Write one call's request frame (skipped when its requester gave up), and
/// leave the wait for cg OBS's answer to a task of its own.
async fn send_call(write: &SharedWrite, dispatcher: &Dispatcher, call: RemoteCall) {
    let RemoteCall::Request {
        request_type,
        request_data,
        reply,
    } = call;
    if reply.is_closed() {
        debug!(request_type, "remote: the caller gave up — not forwarded");
        return;
    }
    let req_id = uuid::Uuid::new_v4().to_string();
    let msg = forward_request(&request_type, &req_id, request_data);
    let frame = Message::Text(msg.to_string().into());
    match dispatcher.send(write, req_id.clone(), frame).await {
        Ok(rx) => {
            let dispatcher = dispatcher.clone();
            tokio::spawn(async move {
                let d = match dispatcher.wait(&req_id, rx, DEFAULT_RESPONSE_TIMEOUT).await {
                    Ok(mut response) => response.get_mut("d").map(Value::take),
                    Err(e) => {
                        warn!(request_type, %e, "remote: cg OBS did not answer a forwarded request");
                        None
                    }
                };
                let _ = reply.send(d);
            });
        }
        Err(e) => {
            warn!(request_type, %e, "remote: forwarding a request to cg OBS failed");
            let _ = reply.send(None);
        }
    }
}

/// The op=6 request forwarded to cg OBS under SongPlayer's own request id.
pub fn forward_request(request_type: &str, request_id: &str, request_data: Option<Value>) -> Value {
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
    use std::net::SocketAddr;
    use std::time::Duration;

    use futures::StreamExt;
    use tokio::task::JoinHandle;

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

    /// A WebSocket peer standing in for cg OBS: the first `n` request frames
    /// it receives (it answers none).
    async fn cg_obs_receiving(n: usize) -> (SocketAddr, JoinHandle<Vec<Value>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut frames = Vec::new();
            while frames.len() < n {
                match ws.next().await {
                    Some(Ok(Message::Text(text))) => {
                        frames.push(serde_json::from_str::<Value>(&text).unwrap());
                    }
                    Some(Ok(_)) => {}
                    other => panic!("cg OBS got {} of {n} requests: {other:?}", frames.len()),
                }
            }
            frames
        });
        (addr, received)
    }

    /// The write half of a client connected to `addr`.
    async fn write_half(addr: SocketAddr) -> SharedWrite {
        let (client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .unwrap();
        let (write, _read) = client.split();
        std::sync::Arc::new(tokio::sync::Mutex::new(write))
    }

    fn switch_to(scene: &str) -> (RemoteCall, oneshot::Receiver<Option<Value>>) {
        let (reply, rx) = oneshot::channel();
        let call = RemoteCall::Request {
            request_type: "SetCurrentProgramScene".to_string(),
            request_data: Some(serde_json::json!({ "sceneName": scene })),
            reply,
        };
        (call, rx)
    }

    /// #221: calls queued back to back reach cg OBS in queue order — a
    /// mirrored playlist press never after a later press.
    #[tokio::test]
    async fn the_calls_reach_cg_obs_in_queue_order() {
        let scenes = ["sp-fast", "Blank", "sp-slow", "Trailer", "sp-90s"];
        let (addr, received) = cg_obs_receiving(scenes.len()).await;
        let write = write_half(addr).await;
        let (tx, calls) = mpsc::unbounded_channel();
        let mut replies = Vec::new();
        for scene in scenes {
            let (call, rx) = switch_to(scene);
            tx.send(call).unwrap();
            replies.push(rx); // kept: a call whose requester gave up is skipped
        }
        drop(tx);
        run_calls(write, Dispatcher::new(), calls).await;
        let frames = tokio::time::timeout(Duration::from_secs(10), received)
            .await
            .expect("cg OBS received the requests within 10 s")
            .unwrap();
        let order: Vec<&str> = frames
            .iter()
            .map(|f| f["d"]["requestData"]["sceneName"].as_str().unwrap())
            .collect();
        assert_eq!(order, scenes);
        drop(replies);
    }

    /// A call whose requester already gave up never reaches cg OBS — the
    /// first request a real WebSocket peer receives is the LIVE call queued
    /// after an abandoned one (a stale `SetCurrentProgramScene` must never
    /// switch cg OBS late).
    #[tokio::test]
    async fn an_abandoned_call_is_never_sent_and_a_live_one_is() {
        let (addr, received) = cg_obs_receiving(1).await;
        let write = write_half(addr).await;
        let (tx, calls) = mpsc::unbounded_channel();
        let (stale, rx) = switch_to("stale");
        drop(rx);
        tx.send(stale).unwrap();
        let (reply, _live_rx) = oneshot::channel();
        tx.send(RemoteCall::Request {
            request_type: "GetSceneList".to_string(),
            request_data: None,
            reply,
        })
        .unwrap();
        drop(tx);
        run_calls(write, Dispatcher::new(), calls).await;
        let first = tokio::time::timeout(Duration::from_secs(10), received)
            .await
            .expect("cg OBS received nothing within 10 s")
            .unwrap();
        assert_eq!(first[0]["op"], 6);
        assert_eq!(first[0]["d"]["requestType"], "GetSceneList");
    }
}
