//! #255 item C: the facade's accept loop asks the Dante guard — over REAL
//! sockets on 127.0.0.1, a refused set holding 127.0.0.1/8 standing in for
//! the Dante network. The rig is `session_tests`'s.

use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::tests::{TIMEOUT, connect, next_json};
use crate::dante_guard::{Cidr, DanteGuard, DanteRefusals};
use crate::playback::program_bus::ProgramBus;
use crate::remote::{Facade, IDENTIFY_TIMEOUT, UPSTREAM_TIMEOUT, Upstream, serve_guarded};

async fn facade_on(guard: Arc<DanteGuard>) -> std::net::SocketAddr {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let (events, _) = broadcast::channel(8);
    let upstream = Upstream::new(None, events).with_timeout(UPSTREAM_TIMEOUT);
    let facade = Facade::for_test(
        pool,
        Arc::new(ProgramBus::new()),
        upstream,
        None,
        IDENTIFY_TIMEOUT,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_guarded(listener, facade, guard));
    addr
}

fn guard(set: &[&str]) -> Arc<DanteGuard> {
    Arc::new(DanteGuard::new(
        set.iter().map(|s| Cidr::parse(s).unwrap()).collect(),
    ))
}

#[tokio::test]
async fn a_client_on_a_refused_subnet_gets_no_handshake_and_is_counted() {
    let g = guard(&["127.0.0.1/8"]);
    let addr = facade_on(Arc::clone(&g)).await;
    let req = format!("ws://{addr}").into_client_request().unwrap();
    let refused = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .expect("the refused connect neither failed nor succeeded in time");
    assert!(refused.is_err(), "the handshake must not be answered");
    assert_eq!(g.refusals(), DanteRefusals { http: 0, remote: 1 });
}

#[tokio::test]
async fn a_client_outside_the_set_gets_the_hello() {
    let g = guard(&["10.77.7.0/24"]);
    let addr = facade_on(Arc::clone(&g)).await;
    let mut ws = connect(addr).await;
    let hello = next_json(&mut ws).await;
    assert_eq!(hello["op"], 0, "{hello}");
    assert_eq!(g.refusals(), DanteRefusals::default());
}
