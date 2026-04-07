//! WebSocket client with automatic reconnection.

use futures::StreamExt;
use gloo_net::websocket::Message;
use gloo_net::websocket::futures::WebSocket;
use leptos::prelude::Set;
use sp_core::ws::ServerMsg;

use crate::store::DashboardStore;

/// Spawn a long-lived WebSocket connection that auto-reconnects.
pub fn connect(store: DashboardStore) {
    leptos::task::spawn_local(async move {
        loop {
            match try_connect(&store).await {
                Ok(()) => {
                    // Clean close — reconnect after a short delay.
                }
                Err(_e) => {
                    // Connection failed or was lost.
                }
            }
            store.ws_connected.set(false);
            gloo_timers::future::TimeoutFuture::new(2_000).await;
        }
    });
}

/// Attempt a single WebSocket session. Returns when the connection closes.
async fn try_connect(store: &DashboardStore) -> Result<(), String> {
    let host = window_host();
    let protocol = if window_protocol() == "https:" {
        "wss"
    } else {
        "ws"
    };
    let ws_url = format!("{protocol}://{host}/api/v1/ws");

    let ws = WebSocket::open(&ws_url).map_err(|e| e.to_string())?;
    store.ws_connected.set(true);

    let (_write, mut read) = ws.split();

    while let Some(msg_result) = read.next().await {
        match msg_result {
            Ok(Message::Text(text)) => {
                if let Ok(server_msg) = serde_json::from_str::<ServerMsg>(&text) {
                    store.dispatch(server_msg);
                }
            }
            Ok(Message::Bytes(_)) => {
                // Binary messages not expected; ignore.
            }
            Err(_e) => {
                break;
            }
        }
    }

    Ok(())
}

fn window_host() -> String {
    web_sys::window()
        .expect("no window")
        .location()
        .host()
        .unwrap_or_else(|_| "localhost:8920".into())
}

fn window_protocol() -> String {
    web_sys::window()
        .expect("no window")
        .location()
        .protocol()
        .unwrap_or_else(|_| "http:".into())
}
