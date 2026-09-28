//! #217 addendum 3 — the RecoveryEvent → engine forwarder survives a lagged
//! receiver and stops when the channel closes or on shutdown.

use std::time::Duration;

use tokio::sync::{broadcast, mpsc};

use super::forward_recovery_events;
use crate::EngineCommand;
use crate::resolume::RecoveryEvent;

fn event(host: &str) -> RecoveryEvent {
    RecoveryEvent {
        host: host.to_string(),
    }
}

/// The next forwarded command's host (bounded: a dead forwarder fails the
/// test instead of hanging it).
async fn next_host(engine_rx: &mut mpsc::Receiver<EngineCommand>) -> String {
    match tokio::time::timeout(Duration::from_secs(5), engine_rx.recv()).await {
        Ok(Some(EngineCommand::ResolumeRecovered { host })) => host,
        other => panic!("expected a forwarded ResolumeRecovered, got {other:?}"),
    }
}

/// Review of addendum 2 (comment 5859006863): the forwarder's
/// `Ok(event) = recv()` select branch was disabled by the first `Lagged`, and
/// the select then waited on shutdown only: no RecoveryEvent reached the
/// engine again. Three events into a capacity-1 channel lag the receiver by
/// two: ONE event is forwarded for the two it missed, then the one the channel
/// kept, and a later event still arrives.
#[tokio::test]
async fn the_forwarder_survives_a_lag_and_stops_when_the_channel_closes() {
    let (events_tx, events_rx) = broadcast::channel(1);
    let (engine_tx, mut engine_rx) = mpsc::channel(16);
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);
    for host in ["a", "b", "c"] {
        events_tx.send(event(host)).unwrap();
    }

    let forwarder = tokio::spawn(forward_recovery_events(events_rx, engine_tx, shutdown_rx));

    assert_eq!(
        next_host(&mut engine_rx).await,
        "(lagged)",
        "one re-push for the two missed events"
    );
    assert_eq!(
        next_host(&mut engine_rx).await,
        "c",
        "the event the channel kept"
    );
    events_tx.send(event("d")).unwrap();
    assert_eq!(
        next_host(&mut engine_rx).await,
        "d",
        "a later event is still forwarded"
    );

    drop(events_tx);
    tokio::time::timeout(Duration::from_secs(5), forwarder)
        .await
        .expect("the forwarder stops when the channel closes")
        .unwrap();
    assert!(engine_rx.try_recv().is_err(), "nothing else was forwarded");
    drop(shutdown_tx); // alive until here: the close, not shutdown, ended it
}

#[tokio::test]
async fn the_forwarder_stops_on_shutdown() {
    let (_events_tx, events_rx) = broadcast::channel::<RecoveryEvent>(4);
    let (engine_tx, _engine_rx) = mpsc::channel(16);
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);
    let forwarder = tokio::spawn(forward_recovery_events(events_rx, engine_tx, shutdown_rx));

    shutdown_tx.send(()).unwrap();

    tokio::time::timeout(Duration::from_secs(5), forwarder)
        .await
        .expect("the forwarder stops on shutdown")
        .unwrap();
}
