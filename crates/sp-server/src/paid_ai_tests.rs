//! #229 item C: the one paid-AI gate.

use std::sync::{Arc, Mutex};

use super::*;
use crate::db::models::set_setting;

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

/// The switch is read live at every call: unset = on (SNV unchanged),
/// `false` = off, `true` / blank = on, anything else = off.
#[tokio::test]
async fn the_switch_is_read_live() {
    let pool = pool().await;
    assert!(enabled(&pool).await, "unset = on");
    set_setting(&pool, SETTING_PAID_AI_ENABLED, "false")
        .await
        .unwrap();
    assert!(!enabled(&pool).await);
    set_setting(&pool, SETTING_PAID_AI_ENABLED, "true")
        .await
        .unwrap();
    assert!(enabled(&pool).await);
    set_setting(&pool, SETTING_PAID_AI_ENABLED, " ")
        .await
        .unwrap();
    assert!(enabled(&pool).await, "blank = unset");
    set_setting(&pool, SETTING_PAID_AI_ENABLED, "off")
        .await
        .unwrap();
    assert!(!enabled(&pool).await, "a value the API refuses = off");
}

/// A switch that cannot be read is off: no paid call on a guess.
#[tokio::test]
async fn an_unreadable_switch_is_off() {
    let pool = pool().await;
    pool.close().await;
    assert!(!enabled(&pool).await);
    assert_eq!(gemini_keys(&pool).await, None);
}

/// Every Gemini key read goes through the gate: no key while off.
#[tokio::test]
async fn no_gemini_key_while_paid_ai_is_off() {
    let pool = pool().await;
    assert_eq!(gemini_keys(&pool).await, Some(Vec::new()), "on, no key set");
    set_setting(&pool, "gemini_api_key", " k1 , k2")
        .await
        .unwrap();
    assert_eq!(
        gemini_keys(&pool).await,
        Some(vec!["k1".to_string(), "k2".to_string()])
    );
    set_setting(&pool, SETTING_PAID_AI_ENABLED, "false")
        .await
        .unwrap();
    assert_eq!(gemini_keys(&pool).await, None);
}

/// A PATCH of the switch takes true / false / "" only, stored lowercase;
/// every other setting passes as sent.
#[test]
fn a_patch_of_the_switch_takes_true_false_or_blank() {
    assert_eq!(
        checked(SETTING_PAID_AI_ENABLED, " FALSE "),
        Ok("false".into())
    );
    assert_eq!(checked(SETTING_PAID_AI_ENABLED, "True"), Ok("true".into()));
    assert_eq!(checked(SETTING_PAID_AI_ENABLED, " "), Ok(String::new()));
    assert_eq!(
        checked(SETTING_PAID_AI_ENABLED, "off"),
        Err("paid_ai_enabled must be true or false".into())
    );
    assert_eq!(checked("gemini_model", " X "), Ok(" X ".into()));
}

/// The log lines a scoped subscriber wrote (this test's thread only).
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Held work logs ONE INFO per kind and song, then DEBUG — never a WARN.
/// The keys are this test's own: the holds are process-wide.
#[test]
fn a_hold_logs_one_info_per_kind_and_song() {
    let cap = Captured::default();
    let writer = cap.clone();
    let sub = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::with_default(sub, || {
        hold(Held::Lyrics, "hold-test-1");
        hold(Held::Lyrics, "hold-test-1");
        hold(Held::Dub, "hold-test-1");
        hold(Held::Lyrics, "hold-test-2");
    });
    let text = String::from_utf8(cap.0.lock().unwrap().clone()).unwrap();
    let count = |level: &str| text.lines().filter(|l| l.contains(level)).count();
    assert_eq!(
        (count(" INFO "), count("DEBUG"), count(" WARN ")),
        (3, 1, 0),
        "{text}"
    );
    assert!(text.contains("job=\"dub\""), "{text}");
}

/// Held work is picked again after 30 minutes; a probe says why it sent
/// nothing.
#[test]
fn held_work_waits_30_minutes() {
    assert_eq!(HELD_RECHECK, Duration::from_secs(30 * 60));
    assert_eq!(
        OFF_REASON,
        "paid AI is off on this node (paid_ai_enabled = false): nothing was sent"
    );
}

/// The status names the switch and, while off, the kinds it holds.
#[tokio::test]
async fn the_status_names_what_paid_ai_off_holds() {
    let pool = pool().await;
    hold(Held::Metadata, "status-test-1");
    hold(Held::Translation, "");
    let on = status(&pool).await;
    assert_eq!(
        on,
        PaidAiStatus {
            enabled: true,
            held: Vec::new()
        }
    );
    set_setting(&pool, SETTING_PAID_AI_ENABLED, "false")
        .await
        .unwrap();
    let off = status(&pool).await;
    assert!(!off.enabled);
    for kind in ["metadata", "translation"] {
        assert!(off.held.iter().any(|k| k == kind), "{kind}: {off:?}");
    }
    assert_eq!(Held::Lyrics.as_str(), "lyrics");
    assert_eq!(Held::Dub.as_str(), "dub");
}

/// `GET /api/v1/status` names the node and the switch, through the router.
#[tokio::test]
async fn get_status_names_the_node_and_the_paid_ai_switch() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::api::routes::StatusResponse;
    use crate::api::routes::tests::{app, test_state};

    async fn read(state: crate::AppState) -> StatusResponse {
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    let state = test_state().await;
    let fresh = read(state.clone()).await;
    assert_eq!(
        (fresh.node_name, fresh.paid_ai_enabled, fresh.paid_ai_held),
        (None, true, Vec::new()),
        "a node that never set either"
    );
    set_setting(&state.pool, "node_name", " pp ").await.unwrap();
    set_setting(&state.pool, SETTING_PAID_AI_ENABLED, "false")
        .await
        .unwrap();
    hold(Held::Dub, "route-test-1");
    let pp = read(state).await;
    assert_eq!(pp.node_name.as_deref(), Some("pp"));
    assert!(!pp.paid_ai_enabled);
    assert!(
        pp.paid_ai_held.iter().any(|k| k == "dub"),
        "{:?}",
        pp.paid_ai_held
    );
}

/// A PATCH that would store a mangled switch is refused whole (400), and
/// nothing is written; `false` is stored lowercase.
#[tokio::test]
async fn a_patch_of_a_mangled_switch_is_refused() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::api::routes::tests::{app, test_state};

    let state = test_state().await;
    let patch = |body: &'static str| {
        Request::builder()
            .method("PATCH")
            .uri("/api/v1/settings")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    let resp = app(state.clone())
        .oneshot(patch(r#"{"paid_ai_enabled":"nope","gemini_model":"m2"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let pool = state.pool.clone();
    let stored = crate::db::models::get_setting(&pool, SETTING_PAID_AI_ENABLED).await;
    assert_eq!(stored.unwrap(), None);
    let model = crate::db::models::get_setting(&pool, "gemini_model").await;
    assert_eq!(model.unwrap(), None, "nothing written");
    let resp = app(state.clone())
        .oneshot(patch(r#"{"paid_ai_enabled":" FALSE "}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let stored = crate::db::models::get_setting(&pool, SETTING_PAID_AI_ENABLED).await;
    assert_eq!(stored.unwrap().as_deref(), Some("false"));
    assert!(!enabled(&pool).await);
}

/// A hold shows on the status for 40 min after its last hold, then not:
/// held work holds again within that, finished work does not.
#[test]
fn a_hold_shows_for_40_minutes_after_its_last_hold() {
    let t = Instant::now();
    assert!(shown(t, t));
    assert!(shown(t, t + HELD_SHOWN));
    assert!(!shown(t, t + HELD_SHOWN + Duration::from_nanos(1)));
    assert_eq!(HELD_SHOWN, Duration::from_secs(40 * 60));
    hold(Held::Translation, "shown-test-1");
    assert!(
        held_kinds(Instant::now())
            .iter()
            .any(|k| k == "translation")
    );
    let tomorrow = Instant::now() + Duration::from_secs(86_400);
    assert_eq!(held_kinds(tomorrow), Vec::<String>::new());
}
