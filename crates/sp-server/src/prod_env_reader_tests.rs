//! #233: the bounded reader (a hung WMI read is unreadable, waited for
//! again, never started twice), one check's reads, and the real glue on the
//! Windows CI job.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use super::*;

static GATE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
static GATED_READS: AtomicUsize = AtomicUsize::new(0);

/// A read that waits until the gate opens (only the hung-read test uses
/// it): the number of the read.
fn gated_read() -> usize {
    let n = GATED_READS.fetch_add(1, Ordering::SeqCst) + 1;
    let (open, opened) = &GATE;
    let mut is_open = open.lock().unwrap_or_else(|e| e.into_inner());
    while !*is_open {
        is_open = opened.wait(is_open).unwrap_or_else(|e| e.into_inner());
    }
    n
}

/// Opens the gate when dropped, so a failing test never leaves the read
/// blocked (the runtime's drop waits for its blocking tasks).
struct OpenGate;

impl Drop for OpenGate {
    fn drop(&mut self) {
        let (open, opened) = &GATE;
        *open.lock().unwrap_or_else(|e| e.into_inner()) = true;
        opened.notify_all();
    }
}

#[tokio::test]
async fn a_hung_read_is_unreadable_and_waited_for_never_started_again() {
    let gate = OpenGate;
    let mut reader = Reader::new(gated_read);
    // The read cannot finish while the gate is shut: the bound passes.
    let first = reader.next(Duration::from_millis(50)).await;
    assert_eq!(
        first,
        Err("the WMI check did not finish within 50ms".to_string())
    );
    let second = reader.next(Duration::from_millis(50)).await;
    assert_eq!(second, first);
    // The third call finds the read still running and waits for it; the
    // gate opens only once that call has started waiting (`join!` polls
    // the call first, the opener sleeps first), so it takes that same
    // read's value. Had a call started another read, its number would be
    // 2 or more.
    let opener = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(gate);
    };
    let (third, ()) = tokio::join!(reader.next(Duration::from_secs(60)), opener);
    assert_eq!(third, Ok(1));
    assert_eq!(
        GATED_READS.load(Ordering::SeqCst),
        1,
        "one read, waited for three times"
    );
    // Done with it: the next call reads again.
    assert_eq!(reader.next(Duration::from_secs(60)).await, Ok(2));
    assert_eq!(GATED_READS.load(Ordering::SeqCst), 2);
}

static LATE_GATE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
static LATE_READS: AtomicUsize = AtomicUsize::new(0);

/// A read that waits until `LATE_GATE` opens (only the stale-read test uses
/// it): the number of the read.
fn late_read() -> usize {
    let n = LATE_READS.fetch_add(1, Ordering::SeqCst) + 1;
    let (open, opened) = &LATE_GATE;
    let mut is_open = open.lock().unwrap_or_else(|e| e.into_inner());
    while !*is_open {
        is_open = opened.wait(is_open).unwrap_or_else(|e| e.into_inner());
    }
    n
}

/// Opens `LATE_GATE` when dropped.
struct OpenLateGate;

impl Drop for OpenLateGate {
    fn drop(&mut self) {
        let (open, opened) = &LATE_GATE;
        *open.lock().unwrap_or_else(|e| e.into_inner()) = true;
        opened.notify_all();
    }
}

impl<T> Reader<T> {
    /// The read a call gave up on has finished since.
    fn late_read_done(&self) -> bool {
        self.pending.as_ref().is_some_and(|read| read.is_finished())
    }
}

/// Review finding 7: a read that finished after its call gave up is stale;
/// the next call reads afresh and never hands it over as this check's.
#[tokio::test]
async fn a_read_that_finished_late_is_dropped_and_read_afresh() {
    let gate = OpenLateGate;
    let mut reader = Reader::new(late_read);
    assert_eq!(
        reader.next(Duration::from_millis(50)).await,
        Err("the WMI check did not finish within 50ms".to_string())
    );
    // The first read finishes now, between two checks.
    drop(gate);
    for _ in 0..4_000 {
        if reader.late_read_done() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(reader.late_read_done(), "the first read never finished");
    // The next check reads again: read 2's value, not the stale read 1's.
    assert_eq!(reader.next(Duration::from_secs(60)).await, Ok(2));
    assert_eq!(LATE_READS.load(Ordering::SeqCst), 2);
}

fn panicking_read() -> usize {
    panic!("the WMI read panicked");
}

#[tokio::test]
async fn a_read_that_panics_is_unreadable() {
    let mut reader = Reader::new(panicking_read);
    let why = reader.next(Duration::from_secs(60)).await.unwrap_err();
    assert!(why.starts_with("the WMI check failed: "), "{why}");
}

fn fake_direct() -> DirectRead {
    DirectRead {
        edition_id: Ok(Some("IoTEnterprise".to_string())),
        product_name: Ok(None),
        build: Ok(Some("22631".to_string())),
        firewall_policy: [RegDword::Absent; 3],
        firewall_legacy_policy: RegDword::Absent,
        firewall_local: [RegDword::Value(0), RegDword::Value(1), RegDword::Value(0)],
        defender_service: ServiceRead::State(4),
    }
}

fn fake_wmi() -> WmiRead {
    WmiRead {
        defender: Ok(Some(DefenderRow {
            realtime: Some(true),
            mode: Some("Normal".to_string()),
            antivirus_enabled: Some(true),
        })),
        firewall: Ok(Vec::new()),
        antivirus: Ok(Vec::new()),
    }
}

#[tokio::test]
async fn a_check_reads_the_platform_or_says_not_windows() {
    assert_eq!(
        next_check(None, Duration::from_secs(60)).await,
        Check::NotWindows
    );
    let mut platform = Platform::new(fake_direct, fake_wmi);
    let check = next_check(Some(&mut platform), Duration::from_secs(60)).await;
    assert_eq!(
        check,
        Check::Read(facts_from(&fake_direct(), &Ok(fake_wmi())))
    );
    let Check::Read(facts) = check else {
        unreachable!()
    };
    assert_eq!(facts.edition_id, Some("IoTEnterprise".to_string()));
    assert_eq!(facts.build_number, Some(22631));
    assert_eq!(facts.defender_realtime, Some(true));
    assert_eq!(facts.firewall, [Some(false), Some(true), Some(false)]);
}

// ---- the Windows glue, on the Windows CI job ----

/// The registry and WMI reads run on a real Windows (the CI runner): every
/// fact is read (its values are the runner's, so only their presence is
/// asserted) and written to the job's log as evidence.
#[cfg(windows)]
#[test]
fn the_windows_glue_reads_every_fact() {
    use std::io::Write;
    let direct = super::win::read_direct();
    let wmi = super::win::read_wmi();
    let _ = writeln!(
        std::io::stderr(),
        "[#233 prod_env on the runner] {direct:?} {wmi:?}"
    );
    let facts = facts_from(&direct, &Ok(wmi));
    assert!(facts.read_errors.is_empty(), "{:?}", facts.read_errors);
    assert!(facts.edition_id.is_some(), "{facts:?}");
    assert!(facts.defender_realtime.is_some(), "{facts:?}");
    assert!(facts.firewall.iter().all(Option::is_some), "{facts:?}");
}
