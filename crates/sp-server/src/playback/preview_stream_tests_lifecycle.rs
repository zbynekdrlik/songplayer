//! #184: the preview encoder's stop / claim-release lifecycle against a viewer
//! that subscribes while the encoder stops (the frozen-preview race, ROOT
//! CAUSE comment 5989070744). The pure `StreamShared` steps the supervisor
//! takes after a child run: `settle_unwatched_run` (which ends the stopped
//! child's stream first), `end_stopped_stream`, `give_up`. No sleeps; the
//! only timeouts are the lock tests' safe-direction windows: they hold the
//! lifecycle lock as a gate, and correct code can never finish inside the
//! held window (a slow runner only makes it pass vacuously), plus a generous
//! bound on the step the released gate lets through.

use super::*;

use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tokio::sync::broadcast::error::TryRecvError;

use crate::playback::preview::fmp4_relay::RelayChunk;

/// The "must not happen yet" window of the lock tests (safe direction).
const HELD_WINDOW: Duration = Duration::from_millis(200);
/// Bound on the step the released gate lets through.
const RELEASED_BOUND: Duration = Duration::from_secs(20);

/// The box race: the running supervisor's TTL saw no viewer and tore the child
/// down. BEFORE the supervisor settled, a viewer subscribed exactly as
/// `api/preview.rs` does (subscribe → `ensure_running` → relay subscribe →
/// init). It found the claim held, so its `ensure_running` started nothing,
/// and it was sent the stopping child's init that was still cached. After the
/// supervisor's steps, that viewer must still have an encoder: the claim kept
/// for a new child. It must also not be left holding the stopped child's
/// init: its stream is closed, so it reconnects onto the next init.
#[test]
fn a_viewer_that_subscribes_while_the_encoder_stops_still_gets_an_encoder() {
    let tap = StreamTap::new("playlist-648".into(), 0);
    let shared = tap.shared().clone();
    // A running supervisor holds the claim; its child streamed.
    assert!(shared.try_claim_encoder(), "the running supervisor's claim");
    let relay = shared.relay();
    relay.ingest(RelayChunk::Init(b"INIT_OLD".to_vec()));
    relay.ingest(RelayChunk::Fragment(vec![1]));

    // The monitor saw no viewer past the TTL and tore the child down. Now the
    // viewer arrives, in the WS handler's order.
    let (_viewer, viewer_relay) = ViewerGuard::subscribe(&tap);
    assert!(
        !shared.try_claim_encoder(),
        "its ensure_running finds the claim held and starts nothing"
    );
    let mut frag_rx = viewer_relay.subscribe();
    assert_eq!(
        viewer_relay.init().as_deref(),
        Some(&b"INIT_OLD"[..]),
        "it is sent the stopping child's init (the box log: init sent, 23 ms later child stopped)"
    );

    // The supervisor's step after the teardown: the settle alone (it ends the
    // stopped child's stream itself, before it may release the claim).
    let end = shared.settle_unwatched_run();

    assert_eq!(
        end,
        UnwatchedEnd::Restart,
        "the supervisor must keep going for the viewer that subscribed as it stopped"
    );
    assert!(
        !shared.try_claim_encoder(),
        "the claim stays held: the supervisor starts a new child for the viewer \
         (and nobody may start a second one)"
    );
    match frag_rx.try_recv() {
        Err(TryRecvError::Closed) => {}
        other => panic!(
            "the viewer holding the stopped child's init must be closed so it \
             reconnects onto the next child's init, got {other:?}"
        ),
    }
    assert!(relay.init().is_none(), "the stopped child's init is gone");
}

/// With an init cached, `end_stopped_stream` CLOSES the relay. A bare reset
/// would leave a viewer holding the stopped child's init while the next
/// child's restarted timeline arrives. A viewer that joins after the stop gets
/// the next child's stream.
#[test]
fn a_stopped_childs_init_is_closed_for_the_viewer_holding_it() {
    let tap = StreamTap::new("t".into(), 0);
    let shared = tap.shared().clone();
    let relay = shared.relay();
    relay.ingest(RelayChunk::Init(b"INIT_A".to_vec()));
    relay.ingest(RelayChunk::Fragment(vec![1]));
    let (_viewer, viewer_relay) = ViewerGuard::subscribe(&tap);
    let mut holding = viewer_relay.subscribe();

    shared.end_stopped_stream();

    assert!(
        matches!(holding.try_recv(), Err(TryRecvError::Closed)),
        "the viewer holding INIT_A is closed, never left on a reset relay"
    );
    assert!(
        relay.init().is_none(),
        "the stopped child's init is cleared"
    );
    assert_eq!(relay.produced_ms(), 0, "and its media-time counter");
    let mut next = relay.subscribe();
    relay.ingest(RelayChunk::Init(b"INIT_B".to_vec()));
    relay.ingest(RelayChunk::Fragment(vec![7]));
    assert_eq!(relay.init().as_deref(), Some(&b"INIT_B"[..]));
    assert_eq!(next.try_recv().unwrap().as_ref(), &[7u8][..]);
}

/// With no init cached, the stopped child produced nothing a viewer could hold
/// (a cold start whose viewer left, or a child that never opened its encoder).
/// A viewer still waiting for an init keeps its stream (no needless reconnect)
/// and receives the next child's.
#[test]
fn a_stopped_child_without_an_init_leaves_waiting_viewers_their_stream() {
    let tap = StreamTap::new("t".into(), 0);
    let shared = tap.shared().clone();
    let (_viewer, relay) = ViewerGuard::subscribe(&tap);
    let mut waiting = relay.subscribe();

    shared.end_stopped_stream();

    assert!(
        matches!(waiting.try_recv(), Err(TryRecvError::Empty)),
        "the waiting viewer's stream stays open"
    );
    relay.ingest(RelayChunk::Init(b"INIT_NEXT".to_vec()));
    relay.ingest(RelayChunk::Fragment(vec![3]));
    assert_eq!(relay.init().as_deref(), Some(&b"INIT_NEXT"[..]));
    assert_eq!(waiting.try_recv().unwrap().as_ref(), &[3u8][..]);
}

/// With nobody attached, the settle releases the claim, so the NEXT viewer's
/// `ensure_running` claims a fresh encoder. This is the race's other order:
/// the subscribe comes after the settle.
#[test]
fn with_no_viewer_the_settle_releases_the_claim_for_the_next_viewer() {
    let tap = StreamTap::new("t".into(), 0);
    let shared = tap.shared().clone();
    assert!(shared.try_claim_encoder(), "the running supervisor's claim");
    // A viewer that came and went before the settle does not count.
    drop(ViewerGuard::subscribe(&tap));

    assert_eq!(shared.settle_unwatched_run(), UnwatchedEnd::Released);

    let (_viewer, _relay) = ViewerGuard::subscribe(&tap);
    assert!(
        shared.try_claim_encoder(),
        "the next viewer's ensure_running claims a fresh encoder"
    );
}

/// `ViewerGuard::subscribe` counts the viewer under the lifecycle lock. A
/// settle in progress holds that lock, so the settle either finishes before
/// the viewer is counted or sees it. The test holds the lock the way a running
/// settle does, and the subscribe must wait for it.
#[test]
fn a_subscribe_waits_for_a_settle_in_progress() {
    let tap = StreamTap::new("t".into(), 0);
    let held = tap.shared().lifecycle_lock();
    let (done_tx, done_rx) = mpsc::channel();
    let viewer_tap = tap.clone();
    let viewer = thread::spawn(move || {
        let (guard, _relay) = ViewerGuard::subscribe(&viewer_tap);
        let _ = done_tx.send(());
        guard
    });

    assert!(
        done_rx.recv_timeout(HELD_WINDOW).is_err(),
        "the subscribe must wait while a settle holds the lifecycle lock"
    );
    assert!(
        !tap.shared().has_viewer(),
        "the viewer is not counted while the settle runs"
    );
    drop(held);
    done_rx
        .recv_timeout(RELEASED_BOUND)
        .expect("the subscribe completes once the settle is done");
    let _guard = viewer.join().expect("the viewer thread");
    assert!(tap.shared().has_viewer(), "and the viewer is counted");
}

/// The settle takes the same lock, so it cannot read "no viewer" while a
/// subscribe is mid-count. The test holds the lock like a subscribe in
/// progress: the settle waits, does not release the claim, and then sees the
/// viewer that subscribe counted.
#[test]
fn a_settle_waits_for_a_subscribe_in_progress() {
    let tap = StreamTap::new("t".into(), 0);
    let shared = tap.shared().clone();
    assert!(shared.try_claim_encoder(), "the running supervisor's claim");
    let held = shared.lifecycle_lock();
    let (done_tx, done_rx) = mpsc::channel();
    let settling = shared.clone();
    let settle = thread::spawn(move || {
        let end = settling.settle_unwatched_run();
        let _ = done_tx.send(());
        end
    });

    assert!(
        done_rx.recv_timeout(HELD_WINDOW).is_err(),
        "the settle must wait while a subscribe holds the lifecycle lock"
    );
    assert!(
        !shared.try_claim_encoder(),
        "the claim is not released while the subscribe is mid-count"
    );
    // The subscribe the held lock stands for counts its viewer, then lets go
    // (`ViewerGuard::subscribe` itself would wait on the lock this test holds).
    shared.viewers.fetch_add(1, Ordering::AcqRel);
    drop(held);
    done_rx
        .recv_timeout(RELEASED_BOUND)
        .expect("the settle completes once the subscribe is done");
    assert_eq!(
        settle.join().expect("the settle thread"),
        UnwatchedEnd::Restart,
        "the settle sees the viewer counted under the lock and keeps the claim"
    );
    assert!(!shared.try_claim_encoder(), "the claim is still held");
}

/// A settle that releases the claim also ends the stopped child's stream: its
/// cached init is closed (a straggling receiver sees `Closed`), so the
/// supervisor that claims next starts on a clean relay.
#[test]
fn a_released_settle_also_ends_the_stopped_childs_stream() {
    let tap = StreamTap::new("t".into(), 0);
    let shared = tap.shared().clone();
    assert!(shared.try_claim_encoder(), "the running supervisor's claim");
    let relay = shared.relay();
    relay.ingest(RelayChunk::Init(b"INIT_OLD".to_vec()));
    let mut straggler = relay.subscribe();

    assert_eq!(shared.settle_unwatched_run(), UnwatchedEnd::Released);

    assert!(
        matches!(straggler.try_recv(), Err(TryRecvError::Closed)),
        "the stopped child's stream is closed"
    );
    assert!(relay.init().is_none(), "its init is gone");
    assert!(shared.try_claim_encoder(), "and the claim is free");
}

/// Giving the stream up (the restart budget is spent, or the supervisor
/// panicked) closes EVERY viewer's stream, even one still waiting for an init,
/// so each WS socket closes, and frees the claim for the next viewer.
#[test]
fn give_up_closes_every_viewers_stream_and_frees_the_claim() {
    let tap = StreamTap::new("t".into(), 0);
    let shared = tap.shared().clone();
    assert!(shared.try_claim_encoder(), "the running supervisor's claim");
    let (_viewer, relay) = ViewerGuard::subscribe(&tap);
    let mut waiting = relay.subscribe();

    shared.give_up();

    assert!(
        matches!(waiting.try_recv(), Err(TryRecvError::Closed)),
        "the viewer's stream is closed"
    );
    assert!(
        shared.try_claim_encoder(),
        "the claim is free for the next viewer"
    );
}
