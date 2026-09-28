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
//! #221: the NEWEST scene switch always wins in cg OBS. A playlist press's
//! mirror is not awaited by the facade, so without these rules it could run
//! after a later press's switch and leave cg OBS on the older scene:
//!
//! - **In queue order.** The forwarder writes each request frame before it
//!   takes the next call. A task per call (the #213 shape) could start the
//!   newest first: a multi-thread tokio worker runs the task spawned last from
//!   its LIFO slot.
//! - **A switch runs in order.** cg OBS (obs-websocket) runs every incoming
//!   message on a thread pool with no per-client order, so the forwarder
//!   waits for a scene switch's ANSWER ([`ORDERED_REQUESTS`], at most
//!   `DEFAULT_RESPONSE_TIMEOUT`, 2 s) before it writes the next call. Every
//!   other request's answer is awaited beside the later calls.
//! - **A superseded switch is never sent.** A switch with a later, still
//!   wanted switch already queued behind it is answered with nothing and not
//!   written: while cg OBS is slow, a burst of presses cannot push the newest
//!   one past its requester's 3 s timeout (a given-up call is skipped).
//!
//! The facade waits for a reply only for a bounded time. A call whose requester
//! already gave up (`reply.is_closed()`, e.g. queued while cg OBS was away) is
//! dropped unexecuted: a stale `SetCurrentProgramScene` must never switch cg OBS
//! seconds after the button press was answered as "not ready".

use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use crate::obs::SharedWrite;
use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher};

/// The requests whose effect depends on the order cg OBS runs them in: the
/// forwarder waits for such a request's answer before it writes the next
/// call, and a later one supersedes an earlier one still queued.
pub const ORDERED_REQUESTS: [&str; 1] = ["SetCurrentProgramScene"];

/// One call of the remote-control facade to cg OBS.
#[derive(Debug)]
pub enum RemoteCall {
    /// Forward `requestType` + `requestData` verbatim. The reply is the op=7
    /// `d` object, `None` when cg OBS did not answer in time (or a later
    /// switch superseded this one).
    Request {
        request_type: String,
        request_data: Option<Value>,
        /// #221: a fire-and-forget switch whose `SP-program` cut already
        /// happened (a playlist press's mirror): it replaces an earlier switch
        /// still queued. `false` for everything the facade awaits.
        supersedes: bool,
        reply: oneshot::Sender<Option<Value>>,
    },
}

impl RemoteCall {
    /// A scene switch ([`ORDERED_REQUESTS`]).
    fn ordered(&self) -> bool {
        let RemoteCall::Request { request_type, .. } = self;
        ORDERED_REQUESTS.contains(&request_type.as_str())
    }

    /// Its requester still waits for the answer.
    fn wanted(&self) -> bool {
        let RemoteCall::Request { reply, .. } = self;
        !reply.is_closed()
    }
}

/// A new connection's forwarder: the sender the connection loop hands the
/// facade's calls to, and the task to spawn in the connection's task set
/// ([`run_calls`] with the production answer timeout).
pub fn forwarder(
    write: SharedWrite,
    dispatcher: Dispatcher,
) -> (
    mpsc::UnboundedSender<RemoteCall>,
    impl Future<Output = ()> + Send + 'static,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        tx,
        run_calls(write, dispatcher, rx, DEFAULT_RESPONSE_TIMEOUT),
    )
}

/// The connection's forwarder (one task per OBS connection): run the
/// facade's calls in queue order (module doc) until the connection loop drops
/// its sender or the task is aborted on a disconnect. `answer_timeout` bounds
/// the wait for cg OBS's answer (`DEFAULT_RESPONSE_TIMEOUT`; longer only in
/// tests, so a test never races the production 2 s).
pub async fn run_calls(
    write: SharedWrite,
    dispatcher: Dispatcher,
    mut calls: mpsc::UnboundedReceiver<RemoteCall>,
    answer_timeout: Duration,
) {
    let mut queued = VecDeque::new();
    loop {
        let call = match queued.pop_front() {
            Some(call) => call,
            None => match calls.recv().await {
                Some(call) => call,
                None => return,
            },
        };
        // Everything already queued, so a switch a later one supersedes is
        // never written.
        while let Ok(next) = calls.try_recv() {
            queued.push_back(next);
        }
        if superseded(&call, &queued) {
            let RemoteCall::Request {
                request_type,
                reply,
                ..
            } = call;
            debug!(
                request_type,
                "remote: a later switch is queued — this one is not sent"
            );
            let _ = reply.send(None);
            continue;
        }
        send_call(&write, &dispatcher, call, answer_timeout).await;
    }
}

/// `call` is a switch and a later switch whose requester still waits is
/// already queued behind it.
fn superseded(call: &RemoteCall, queued: &VecDeque<RemoteCall>) -> bool {
    call.ordered() && queued.iter().any(|later| later.ordered() && later.wanted())
}

/// Write one call's request frame (skipped when its requester gave up). A
/// scene switch ([`ORDERED_REQUESTS`]) is answered before this returns; any
/// other call's answer is awaited by a task of its own.
async fn send_call(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    call: RemoteCall,
    answer_timeout: Duration,
) {
    let RemoteCall::Request {
        request_type,
        request_data,
        reply,
        ..
    } = call;
    if reply.is_closed() {
        debug!(request_type, "remote: the caller gave up — not forwarded");
        return;
    }
    let req_id = uuid::Uuid::new_v4().to_string();
    let msg = forward_request(&request_type, &req_id, request_data);
    let frame = Message::Text(msg.to_string().into());
    let ordered = ORDERED_REQUESTS.contains(&request_type.as_str());
    match dispatcher.send(write, req_id.clone(), frame).await {
        Ok(rx) => {
            let waiting = answer(
                dispatcher.clone(),
                request_type,
                req_id,
                rx,
                reply,
                answer_timeout,
            );
            if ordered {
                waiting.await;
            } else {
                tokio::spawn(waiting);
            }
        }
        Err(e) => {
            warn!(request_type, %e, "remote: forwarding a request to cg OBS failed");
            let _ = reply.send(None);
        }
    }
}

/// Wait (at most `answer_timeout`) for cg OBS's answer to request `req_id`
/// and hand its op=7 `d` to the requester (`None` without one).
async fn answer(
    dispatcher: Dispatcher,
    request_type: String,
    req_id: String,
    rx: oneshot::Receiver<Value>,
    reply: oneshot::Sender<Option<Value>>,
    answer_timeout: Duration,
) {
    let d = match dispatcher.wait(&req_id, rx, answer_timeout).await {
        Ok(mut response) => response.get_mut("d").map(Value::take),
        Err(e) => {
            warn!(request_type, %e, "remote: cg OBS did not answer a forwarded request");
            None
        }
    };
    let _ = reply.send(d);
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

    use futures::{SinkExt, StreamExt};
    use serde_json::json;

    use super::*;

    #[test]
    fn forward_request_carries_the_type_our_id_and_the_data_verbatim() {
        let data = json!({ "sceneName": "sp-fast" });
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

    /// A WebSocket peer standing in for cg OBS: every request frame it
    /// receives goes to the returned channel as it arrives; with `answer` it
    /// also answers each one with success (op=7 under the request's id).
    async fn cg_obs(answer: bool) -> (SocketAddr, mpsc::UnboundedReceiver<Value>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                let Message::Text(text) = msg else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let answered = json!({ "op": 7, "d": {
                    "requestType": request["d"]["requestType"],
                    "requestId": request["d"]["requestId"],
                    "requestStatus": { "result": true, "code": 100 },
                }});
                if tx.send(request).is_err() {
                    break;
                }
                if answer {
                    let _ = ws.send(Message::Text(answered.to_string().into())).await;
                }
            }
        });
        (addr, rx)
    }

    /// The write half of a client connected to `addr`; its read half hands
    /// every op=7 answer to `dispatcher`, like the OBS client's reader.
    async fn connect(addr: SocketAddr, dispatcher: &Dispatcher) -> SharedWrite {
        let (client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .unwrap();
        let (write, mut read) = client.split();
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = read.next().await {
                if let Message::Text(text) = msg
                    && let Ok(answer) = serde_json::from_str::<Value>(&text)
                    && let Some(id) = answer["d"]["requestId"].as_str()
                {
                    dispatcher.complete(id, answer.clone());
                }
            }
        });
        std::sync::Arc::new(tokio::sync::Mutex::new(write))
    }

    /// The next request frame cg OBS received (10 s at most).
    async fn next_frame(frames: &mut mpsc::UnboundedReceiver<Value>) -> Value {
        tokio::time::timeout(Duration::from_secs(10), frames.recv())
            .await
            .expect("cg OBS received no request within 10 s")
            .expect("the cg OBS peer ended")
    }

    fn call(
        request_type: &str,
        scene: Option<&str>,
    ) -> (RemoteCall, oneshot::Receiver<Option<Value>>) {
        let (reply, rx) = oneshot::channel();
        let call = RemoteCall::Request {
            request_type: request_type.to_string(),
            request_data: scene.map(|s| json!({ "sceneName": s })),
            supersedes: false,
            reply,
        };
        (call, rx)
    }

    /// A switch the facade awaits (a manual press's forward).
    fn switch_to(scene: &str) -> (RemoteCall, oneshot::Receiver<Option<Value>>) {
        call("SetCurrentProgramScene", Some(scene))
    }

    /// A playlist press's mirror (fire-and-forget: it supersedes).
    fn mirror_of(scene: &str) -> (RemoteCall, oneshot::Receiver<Option<Value>>) {
        let (reply, rx) = oneshot::channel();
        let call = RemoteCall::Request {
            request_type: "SetCurrentProgramScene".to_string(),
            request_data: Some(json!({ "sceneName": scene })),
            supersedes: true,
            reply,
        };
        (call, rx)
    }

    /// The answer timeout of every forwarder test: far past any test's 10 s
    /// bound, so a wait the forwarder must not do can never pass by timing
    /// out, and no test races the production 2 s.
    const HELD: Duration = Duration::from_secs(600);

    /// #221: calls queued back to back reach cg OBS in queue order.
    #[tokio::test]
    async fn the_calls_reach_cg_obs_in_queue_order() {
        let queued = [
            ("GetSceneList", None),
            ("GetInputList", None),
            ("SetCurrentProgramScene", Some("sp-fast")),
            ("GetSceneItemList", Some("sp-fast")),
            ("GetCurrentProgramScene", None),
        ];
        let (addr, mut frames) = cg_obs(true).await;
        let dispatcher = Dispatcher::new();
        let write = connect(addr, &dispatcher).await;
        let (tx, calls) = mpsc::unbounded_channel();
        let mut replies = Vec::new();
        for (request_type, scene) in queued {
            let (queued_call, rx) = call(request_type, scene);
            tx.send(queued_call).unwrap();
            replies.push(rx); // kept: a call whose requester gave up is skipped
        }
        drop(tx);
        run_calls(write, dispatcher, calls, HELD).await;
        let mut order = Vec::new();
        for _ in queued {
            let frame = next_frame(&mut frames).await;
            order.push(frame["d"]["requestType"].as_str().unwrap().to_string());
        }
        let expected: Vec<&str> = queued
            .iter()
            .map(|&(request_type, _)| request_type)
            .collect();
        assert_eq!(order, expected);
        for rx in replies {
            let answer = rx.await.unwrap().expect("cg OBS answered");
            assert_eq!(answer["requestStatus"]["code"], 100);
        }
    }

    /// #221 review round 2: cg OBS (obs-websocket) runs each incoming message
    /// on a thread pool, so frames written in order can still RUN out of
    /// order. A scene switch is answered before the next call goes out. The
    /// "none yet" window sits under a held gate (the answer timeout is
    /// `HELD`, and only the test answers), so correct code can never fail it.
    #[tokio::test]
    async fn a_scene_switch_is_answered_before_the_next_call_goes_out() {
        let (addr, mut frames) = cg_obs(false).await;
        let dispatcher = Dispatcher::new();
        let write = connect(addr, &dispatcher).await;
        let (tx, calls) = mpsc::unbounded_channel();
        let (first, first_rx) = switch_to("sp-fast");
        let (getter, _getter_rx) = call("GetSceneList", None);
        tx.send(first).unwrap();
        tx.send(getter).unwrap();
        let forwarder = tokio::spawn(run_calls(write, dispatcher.clone(), calls, HELD));
        let frame = next_frame(&mut frames).await;
        assert_eq!(frame["d"]["requestData"]["sceneName"], "sp-fast");
        let early = tokio::time::timeout(Duration::from_millis(200), frames.recv()).await;
        assert!(
            early.is_err(),
            "the next call went out before cg OBS answered the switch: {early:?}"
        );
        // cg OBS answers the switch: the next call goes out.
        let id = frame["d"]["requestId"].as_str().unwrap().to_string();
        let answered = json!({ "op": 7, "d": {
            "requestId": id,
            "requestStatus": { "result": true, "code": 100 },
        }});
        dispatcher.complete(&id, answered);
        let answer = first_rx.await.unwrap().expect("the switch's answer");
        assert_eq!(answer["requestStatus"]["code"], 100);
        let frame = next_frame(&mut frames).await;
        assert_eq!(frame["d"]["requestType"], "GetSceneList");
        drop(tx);
        forwarder.abort();
    }

    /// A getter's answer never holds the next call back: an awaited getter
    /// would hold the switch behind it for `HELD`, far past `next_frame`'s
    /// 10 s.
    #[tokio::test]
    async fn a_getter_never_holds_the_next_call_back() {
        let (addr, mut frames) = cg_obs(false).await;
        let dispatcher = Dispatcher::new();
        let write = connect(addr, &dispatcher).await;
        let (tx, calls) = mpsc::unbounded_channel();
        let (getter, _getter_rx) = call("GetSceneList", None);
        let (switch, _switch_rx) = switch_to("sp-fast");
        tx.send(getter).unwrap();
        tx.send(switch).unwrap();
        let forwarder = tokio::spawn(run_calls(write, dispatcher, calls, HELD));
        assert_eq!(
            next_frame(&mut frames).await["d"]["requestType"],
            "GetSceneList"
        );
        // cg OBS has not answered the getter; the switch goes out anyway.
        let frame = next_frame(&mut frames).await;
        assert_eq!(frame["d"]["requestData"]["sceneName"], "sp-fast");
        drop(tx);
        forwarder.abort();
    }

    /// #221 review round 3: a switch with a later, still wanted mirror queued
    /// behind it is never sent — it is answered with nothing — so the newest
    /// press goes out at once however slowly cg OBS answers. A getter between
    /// them is still sent.
    #[tokio::test]
    async fn a_switch_a_later_one_supersedes_is_never_sent() {
        let (addr, mut frames) = cg_obs(false).await;
        let dispatcher = Dispatcher::new();
        let write = connect(addr, &dispatcher).await;
        let (tx, calls) = mpsc::unbounded_channel();
        let (older, older_rx) = switch_to("sp-fast");
        let (getter, _getter_rx) = call("GetSceneList", None);
        let (middle, middle_rx) = mirror_of("sp-slow");
        let (newest, _newest_rx) = mirror_of("Blank");
        for queued in [older, getter, middle, newest] {
            tx.send(queued).unwrap();
        }
        let forwarder = tokio::spawn(run_calls(write, dispatcher, calls, HELD));
        assert_eq!(
            next_frame(&mut frames).await["d"]["requestType"],
            "GetSceneList"
        );
        let frame = next_frame(&mut frames).await;
        assert_eq!(
            frame["d"]["requestData"]["sceneName"], "Blank",
            "only the newest switch goes out"
        );
        assert_eq!(older_rx.await.unwrap(), None, "superseded: no answer");
        assert_eq!(middle_rx.await.unwrap(), None, "superseded: no answer");
        drop(tx);
        forwarder.abort();
    }

    /// #221 review round 4: a later MANUAL forward supersedes nothing: its
    /// cut depends on cg OBS's answer, so the mirror queued before it still
    /// goes out first.
    #[tokio::test]
    async fn a_later_manual_forward_supersedes_nothing() {
        let (addr, mut frames) = cg_obs(false).await;
        let dispatcher = Dispatcher::new();
        let write = connect(addr, &dispatcher).await;
        let (tx, calls) = mpsc::unbounded_channel();
        let (mirror, _mirror_rx) = mirror_of("sp-fast");
        let (manual, _manual_rx) = switch_to("Blank");
        tx.send(mirror).unwrap();
        tx.send(manual).unwrap();
        let forwarder = tokio::spawn(run_calls(write, dispatcher, calls, HELD));
        let frame = next_frame(&mut frames).await;
        assert_eq!(frame["d"]["requestData"]["sceneName"], "sp-fast");
        drop(tx);
        forwarder.abort();
    }

    /// A later mirror whose requester already gave up supersedes nothing: the
    /// earlier, still wanted switch goes out.
    #[tokio::test]
    async fn an_abandoned_later_switch_supersedes_nothing() {
        let (addr, mut frames) = cg_obs(false).await;
        let dispatcher = Dispatcher::new();
        let write = connect(addr, &dispatcher).await;
        let (tx, calls) = mpsc::unbounded_channel();
        let (wanted, _wanted_rx) = switch_to("sp-fast");
        let (abandoned, abandoned_rx) = mirror_of("sp-slow");
        drop(abandoned_rx);
        tx.send(wanted).unwrap();
        tx.send(abandoned).unwrap();
        let forwarder = tokio::spawn(run_calls(write, dispatcher, calls, HELD));
        let frame = next_frame(&mut frames).await;
        assert_eq!(frame["d"]["requestData"]["sceneName"], "sp-fast");
        drop(tx);
        forwarder.abort();
    }

    /// A call whose requester already gave up never reaches cg OBS — the
    /// first request a real WebSocket peer receives is the LIVE call queued
    /// after an abandoned one (a stale `SetCurrentProgramScene` must never
    /// switch cg OBS late).
    #[tokio::test]
    async fn an_abandoned_call_is_never_sent_and_a_live_one_is() {
        let (addr, mut frames) = cg_obs(false).await;
        let dispatcher = Dispatcher::new();
        let write = connect(addr, &dispatcher).await;
        let (tx, calls) = mpsc::unbounded_channel();
        let (stale, rx) = switch_to("stale");
        drop(rx);
        tx.send(stale).unwrap();
        let (live, _live_rx) = call("GetSceneList", None);
        tx.send(live).unwrap();
        drop(tx);
        run_calls(write, dispatcher, calls, HELD).await;
        let first = next_frame(&mut frames).await;
        assert_eq!(first["op"], 6);
        assert_eq!(first["d"]["requestType"], "GetSceneList");
    }
}
