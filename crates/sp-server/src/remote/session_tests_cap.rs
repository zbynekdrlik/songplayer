//! #221 (main-session decision 5882671183 item 2): the facade serves at most
//! 16 sessions at once. Companion needs one, the post-deploy E2E two. A
//! handshake over the cap is refused with HTTP 503 before it becomes a
//! session, and counted as `remote.refused_over_cap`. Over REAL sockets with
//! the rig of `session_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "session_tests_cap.rs"] mod tests_cap;`.

use std::net::SocketAddr;

use serde_json::Value;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::Error;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use super::tests::{Client, Rig, TIMEOUT, connect, hello_identify, request, rig, wait_for};

/// The decided cap (`remote::MAX_SESSIONS`, pinned against it in `mod_tests.rs`).
const CAP: usize = 16;

/// A handshake that must be refused: the HTTP status the facade answered.
async fn refused_status(addr: SocketAddr) -> u16 {
    let mut req = format!("ws://{addr}").into_client_request().unwrap();
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static("obswebsocket.json"),
    );
    let answer = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .expect("no handshake answer within the timeout");
    match answer {
        Err(Error::Http(resp)) => resp.status().as_u16(),
        Ok(_) => panic!("a handshake over the cap became a session"),
        Err(e) => panic!("the handshake failed without an HTTP answer: {e}"),
    }
}

/// `remote.refused_over_cap`, as `GET /api/v1/program` serializes it.
fn refused_over_cap(rig: &Rig) -> Value {
    serde_json::to_value(rig.remote()).unwrap()["refused_over_cap"].clone()
}

/// An identified JSON session.
async fn session(rig: &Rig) -> Client {
    let mut ws = connect(rig.addr).await;
    hello_identify(&mut ws, 0).await;
    ws
}

#[tokio::test]
async fn a_seventeenth_session_is_refused_with_503_until_one_ends() {
    let rig = rig().await;
    let mut sessions = Vec::new();
    for _ in 0..CAP {
        sessions.push(session(&rig).await);
    }
    assert_eq!(rig.remote().clients, CAP);

    assert_eq!(refused_status(rig.addr).await, 503);
    assert_eq!(refused_over_cap(&rig), 1);
    assert_eq!(
        rig.remote().clients,
        CAP,
        "the refused client never became a session"
    );
    // The sessions already open are served as before.
    let d = request(&mut sessions[CAP - 1], "GetStudioModeEnabled", None).await;
    assert_eq!(d["requestStatus"]["code"], 100);

    // One session ends: its slot is free again, and only its slot.
    drop(sessions.remove(0));
    wait_for("the ended session is gone", || {
        rig.remote().clients == CAP - 1
    })
    .await;
    sessions.push(session(&rig).await);
    assert_eq!(rig.remote().clients, CAP);
    assert_eq!(refused_status(rig.addr).await, 503);
    assert_eq!(refused_over_cap(&rig), 2);
}
