//! #255 item C: the live guard and the HTTP accept filter over REAL sockets
//! on 127.0.0.1. A refused set holding `127.0.0.1/8` (an address and its
//! prefix, as an interface gives it) stands in for the Dante network.
//! Every wait is bounded.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::super::Cidr;
use super::{DanteGuard, DanteRefusals, GuardedListener, Which};

const TIMEOUT: Duration = Duration::from_secs(10);

fn guard(set: &[&str]) -> Arc<DanteGuard> {
    Arc::new(DanteGuard::new(
        set.iter().map(|s| Cidr::parse(s).unwrap()).collect(),
    ))
}

/// An axum server on 127.0.0.1 behind the guard, answering `GET /` "ok".
async fn http_server(guard: Arc<DanteGuard>) -> SocketAddr {
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let listener = GuardedListener::new(tcp, guard, Which::Http);
    let router = Router::new().route("/", get(|| async { "ok" }));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}

/// Everything the server sends back to a plain `GET /` before it closes
/// (a reset reads as the bytes read until then).
async fn get_root(addr: SocketAddr) -> Vec<u8> {
    tokio::time::timeout(TIMEOUT, async {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let _ = s
            .write_all(b"GET / HTTP/1.1\r\nHost: sp\r\nConnection: close\r\n\r\n")
            .await;
        let mut got = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
            }
        }
        got
    })
    .await
    .expect("the server neither answered nor closed in time")
}

#[tokio::test]
async fn a_connection_on_a_refused_subnet_is_closed_unanswered_and_counted() {
    let g = guard(&["10.77.7.0/24", "127.0.0.1/8"]);
    let addr = http_server(Arc::clone(&g)).await;
    assert_eq!(get_root(addr).await, b"");
    assert_eq!(get_root(addr).await, b"");
    assert_eq!(g.refusals(), DanteRefusals { http: 2, remote: 0 });
}

#[tokio::test]
async fn a_connection_outside_the_set_is_served() {
    let g = guard(&["10.77.7.0/24"]);
    let addr = http_server(Arc::clone(&g)).await;
    let answer = String::from_utf8(get_root(addr).await).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.ends_with("ok"), "{answer}");
    assert_eq!(g.refusals(), DanteRefusals::default());
}

#[tokio::test]
async fn the_set_changes_live_between_connections() {
    let g = guard(&[]);
    let addr = http_server(Arc::clone(&g)).await;
    assert!(!get_root(addr).await.is_empty());
    assert!(g.set_subnets(vec![Cidr::parse("127.0.0.1/8").unwrap()]));
    assert_eq!(get_root(addr).await, b"");
    assert!(g.set_subnets(vec![]));
    assert!(!get_root(addr).await.is_empty());
    assert_eq!(g.refusals().http, 1);
}

#[test]
fn set_subnets_reports_a_change_only() {
    let g = guard(&["10.77.7.0/24"]);
    assert!(!g.set_subnets(vec![Cidr::parse("10.77.7.0/24").unwrap()]));
    assert!(g.set_subnets(vec![Cidr::parse("10.77.8.0/24").unwrap()]));
    assert_eq!(g.subnets(), vec![Cidr::parse("10.77.8.0/24").unwrap()]);
}

#[test]
fn each_listener_counts_its_own_refusals_and_an_unreadable_address_is_served() {
    let g = guard(&["10.77.7.107/24"]);
    let dante: SocketAddr = "10.77.7.107:8920".parse().unwrap();
    let lan: SocketAddr = "10.77.9.201:4456".parse().unwrap();
    let peer: SocketAddr = "10.77.7.5:50000".parse().unwrap();
    assert!(g.refuse(Which::Remote, Ok(dante), peer));
    assert!(g.refuse(Which::Remote, Ok(dante), peer));
    assert!(g.refuse(Which::Http, Ok(dante), peer));
    assert!(!g.refuse(Which::Http, Ok(lan), peer));
    let unreadable = Err(std::io::Error::other("gone"));
    assert!(!g.refuse(Which::Http, unreadable, peer));
    assert_eq!(g.refusals(), DanteRefusals { http: 1, remote: 2 });
    assert_eq!(
        serde_json::to_value(g.refusals()).unwrap(),
        serde_json::json!({"http": 1, "remote": 2})
    );
}

#[test]
fn the_listeners_are_named_in_the_log() {
    assert_eq!(Which::Http.label(), "http");
    assert_eq!(Which::Remote.label(), "remote");
}

#[tokio::test]
async fn the_guarded_listener_reports_its_own_address() {
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let listener = GuardedListener::new(tcp, guard(&[]), Which::Http);
    assert_eq!(axum::serve::Listener::local_addr(&listener).unwrap(), addr);
}

#[test]
fn the_global_guard_is_one_instance() {
    assert!(Arc::ptr_eq(super::global(), super::global()));
}
