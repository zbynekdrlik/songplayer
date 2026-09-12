//! Tests for the dantesync clock-health evaluation (#146, contract §D/§7).
//! Wired via `#[cfg(test)] #[path = "clock_health_tests.rs"] mod clock_health_tests;`.

use super::*;

fn status(is_locked: Option<bool>, mode: Option<&str>) -> DantesyncStatus {
    DantesyncStatus {
        is_locked,
        mode: mode.map(|m| m.to_string()),
        offset_ns: None,
        ntp_failed: None,
        ntp_age_s: None,
    }
}

#[test]
fn evaluate_locked_nano_is_ok() {
    let h = evaluate(Some(&status(Some(true), Some("NANO"))));
    assert!(h.clock_ok, "locked + NANO must be clock_ok");
    assert!(h.reason.is_none(), "an ok clock carries no reason");
    assert!(h.is_locked);
    assert_eq!(h.mode, "NANO");
}

#[test]
fn evaluate_locked_lock_is_ok() {
    let h = evaluate(Some(&status(Some(true), Some("LOCK"))));
    assert!(h.clock_ok, "locked + LOCK must be clock_ok");
}

#[test]
fn evaluate_locked_free_is_not_ok() {
    let h = evaluate(Some(&status(Some(true), Some("FREE"))));
    assert!(!h.clock_ok, "FREE is not a lock mode");
    assert!(h.reason.is_some());
}

#[test]
fn evaluate_unlocked_nano_is_not_ok() {
    let h = evaluate(Some(&status(Some(false), Some("NANO"))));
    assert!(!h.clock_ok, "not locked is never ok, even in NANO mode");
    assert!(h.reason.is_some());
}

#[test]
fn evaluate_none_is_not_ok_with_no_dantesync_reason() {
    let h = evaluate(None);
    assert!(!h.clock_ok);
    assert_eq!(h.reason.as_deref(), Some("no dantesync"));
}

#[test]
fn default_clock_health_is_no_dantesync() {
    let h = ClockHealth::default();
    assert!(!h.clock_ok);
    assert_eq!(h.reason.as_deref(), Some("no dantesync"));
}

/// Parse the exact live payload sample recorded in the contract digest §D
/// (RESOLUME-SNV, dantesync 1.8.53). Unknown fields must be ignored.
#[test]
fn parses_real_dantesync_sample_payload() {
    let raw = r#"{"offset_ns":164707,"drift_ppm":-7.68,"gm_source_ip":"10.77.9.184","settled":true,"is_locked":true,"mode":"NANO","ntp_offset_us":1249,"ntp_age_s":37,"ntp_failed":false,"accumulated_phase_us":-14020,"phase_slew_enabled":false}"#;
    let parsed: DantesyncStatus = serde_json::from_str(raw).expect("real payload must parse");
    assert_eq!(parsed.is_locked, Some(true));
    assert_eq!(parsed.mode.as_deref(), Some("NANO"));
    assert_eq!(parsed.offset_ns, Some(164_707));
    assert_eq!(parsed.ntp_age_s, Some(37));
    assert_eq!(parsed.ntp_failed, Some(false));

    let h = evaluate(Some(&parsed));
    assert!(h.clock_ok, "live locked+NANO sample must evaluate ok");
    assert_eq!(h.offset_ns, Some(164_707));
    assert_eq!(h.ntp_failed, Some(false));
    assert_eq!(h.ntp_age_s, Some(37));
}

#[test]
fn serialises_with_clock_ok_key() {
    let h = evaluate(Some(&status(Some(true), Some("NANO"))));
    let json = serde_json::to_string(&h).unwrap();
    assert!(
        json.contains("\"clock_ok\":true"),
        "serialised ClockHealth must carry clock_ok: {json}"
    );
    assert!(json.contains("\"mode\":\"NANO\""));
}
