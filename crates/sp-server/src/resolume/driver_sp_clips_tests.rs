//! #275: the driver's read of SongPlayer's clip in Arena — a full refresh
//! evaluates its own body, the tracked params are read by id every 30 s
//! (only with the breaker closed, an answered probe and no not-ready
//! episode, within a 2 s budget), a 404 is a stale map (the push path's
//! refresh; a refused id holds the poll until the next full refresh),
//! anything else that does not read is unknown (an answer that does not
//! decode a reader fault), and the outage forgets. Every tick on the
//! synthetic clock (`base + Duration`, `resolume-driver.md` "Testing the
//! driver").

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sp_core::wall_clip::{ClipState, ClipView};
use tokio::sync::broadcast;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::resolume::RecoveryEvent;
use crate::resolume::SUBS_TOKEN;
use crate::resolume::driver::HostDriver;
use crate::resolume::driver::not_ready_tests::{
    arena, composition_fetches, composition_sequence, drain,
};
use crate::resolume::driver::relaunch_tests::{composition, composition_always};
use crate::resolume::sp_clip_store::SpClipStore;
use crate::resolume::sp_clips::{self, Fact, ParamValue, Tracked};

/// SNV's composition of 15.10.2026 (`sp_clips_tests.rs`): two
/// `SP-program-MAX` clips connected, the NDI background connected, 29
/// tracked params (the masters included).
fn snv() -> Value {
    serde_json::from_str(include_str!("testdata/composition-snv-2026-10-15.json")).unwrap()
}

/// The two connected SongPlayer clips and the lowest tracked param (the
/// group "G# Kontent"'s bypassed).
const YT_A: i64 = 1_791_369_575_545;
const YT_B: i64 = 1_791_369_575_524;
const FIRST_PARAM: i64 = 1_681_395_963_139;
/// The first `#sp-title` clip's text param (the liveness flip's probe).
const TITLE_TEXT: i64 = 1_777_202_099_753;
/// Every tracked param of SNV's composition.
const TRACKED: usize = 29;

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

fn yt_music() -> ClipView {
    ClipView {
        layer: "L# Fill".to_string(),
        clip: "Yt music".to_string(),
        source: "SP-program-MAX".to_string(),
    }
}

fn snv_bg() -> ClipView {
    ClipView {
        layer: "L # BG".to_string(),
        clip: "SNV BG".to_string(),
        source: "NDI Source".to_string(),
    }
}

/// The body with the clips `ids` disconnected.
fn disconnected(mut body: Value, ids: &[i64]) -> Value {
    for layer in body["layers"].as_array_mut().unwrap() {
        for clip in layer["clips"].as_array_mut().unwrap() {
            let id = clip["id"].as_i64().unwrap();
            if ids.contains(&id) {
                clip["connected"]["value"] = json!("Disconnected");
            }
        }
    }
    body
}

/// Every id in the body moved by `by`: Arena re-ids every clip and param on
/// a relaunch.
fn shift_ids(value: &mut Value, by: i64) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match child.as_i64() {
                    Some(id) if key == "id" => *child = json!(id + by),
                    _ => shift_ids(child, by),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                shift_ids(item, by);
            }
        }
        _ => {}
    }
}

/// SNV after an Arena relaunch: every id new, no clip connected.
fn relaunched() -> Value {
    let mut body = snv();
    shift_ids(&mut body, 1_000_000);
    for layer in body["layers"].as_array_mut().unwrap() {
        for clip in layer["clips"].as_array_mut().unwrap() {
            clip["connected"]["value"] = json!("Disconnected");
        }
    }
    body
}

/// Each tracked param of `body` answers its value there, as Arena's
/// `parameter/by-id` does (the single param object).
async fn params_answer(server: &MockServer, body: &Value) {
    for (id, value) in sp_clips::track(body).values {
        let value = match value {
            ParamValue::Bool(b) => json!(b),
            ParamValue::Number(n) => json!(n),
            ParamValue::Text(t) => json!(t),
        };
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/parameter/by-id/{id}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": id, "value": value})),
            )
            .mount(server)
            .await;
    }
}

/// `GET /api/v1/parameter/by-id/{id}` answers `status` (mounted before
/// [`params_answer`], so it wins).
async fn param_answers(server: &MockServer, id: i64, status: u16) {
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/parameter/by-id/{id}")))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

/// How many `parameter/by-id` reads reached Arena.
async fn param_reads(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().starts_with("/api/v1/parameter/by-id/"))
        .count()
}

/// A driver on `server` publishing into its own store (never the process's).
fn driver_on(
    server: &MockServer,
) -> (
    HostDriver,
    broadcast::Receiver<RecoveryEvent>,
    &'static SpClipStore,
) {
    let store: &'static SpClipStore = Box::leak(Box::new(SpClipStore::default()));
    let (tx, rx) = broadcast::channel(16);
    let driver = HostDriver::new("127.0.0.1".into(), server.address().port())
        .with_recovery_channel(tx)
        .with_sp_clip_store(store);
    (driver, rx, store)
}

fn running(clips: Vec<ClipView>, ndi: Vec<ClipView>) -> Fact {
    Fact {
        state: ClipState::Running,
        clips,
        ndi_clips: ndi,
        why: None,
        reader_fault: false,
    }
}

fn none(ndi: Vec<ClipView>) -> Fact {
    Fact {
        state: ClipState::NotRunning,
        clips: Vec::new(),
        ndi_clips: ndi,
        why: None,
        reader_fault: false,
    }
}

#[tokio::test]
async fn a_full_refresh_reads_the_clip_from_its_own_body() {
    let server = arena().await;
    composition_always(&server, snv()).await;
    let (mut driver, _rx, store) = driver_on(&server);
    let base = Instant::now();

    driver.on_tick_at(base).await;

    assert_eq!(
        driver.sp_clips.fact,
        running(vec![yt_music(), yt_music()], vec![snv_bg()])
    );
    assert_eq!(
        param_reads(&server).await,
        0,
        "the body carried every value"
    );
    let published = store.current(42);
    assert_eq!(published.state, ClipState::Running);
    assert_eq!(published.clips, vec![yt_music(), yt_music()]);
    assert_eq!(published.ndi_clips, vec![snv_bg()]);
    assert!(published.since_ms.is_some());
    assert_eq!(published.now_ms, Some(42));
}

/// The operator's Stream Deck scene ends (both clips disconnected) between
/// two refreshes: the poll sees it within 30 s, reading 29 params of ~140 B
/// each, never the 14 MB composition.
#[tokio::test]
async fn the_tracked_params_are_read_every_30_s_between_refreshes() {
    let server = arena().await;
    composition_always(&server, snv()).await;
    params_answer(&server, &disconnected(snv(), &[YT_A, YT_B])).await;
    let (mut driver, _rx, store) = driver_on(&server);
    let base = Instant::now();
    driver.on_tick_at(base).await;

    for at in [secs(10), secs(20), Duration::from_millis(29_999)] {
        driver.on_tick_at(base + at).await;
    }
    assert_eq!(param_reads(&server).await, 0, "the refresh was the read");
    assert_eq!(driver.sp_clips.fact.state, ClipState::Running);

    driver.on_tick_at(base + secs(30)).await;
    assert_eq!(
        param_reads(&server).await,
        TRACKED,
        "each tracked param once"
    );
    assert_eq!(driver.sp_clips.fact, none(vec![snv_bg()]));
    assert_eq!(store.current(0).state, ClipState::NotRunning);

    driver.on_tick_at(base + secs(40)).await;
    assert_eq!(param_reads(&server).await, TRACKED);
    driver.on_tick_at(base + secs(60)).await;
    assert_eq!(param_reads(&server).await, 2 * TRACKED);
    assert_eq!(
        composition_fetches(&server).await,
        1,
        "no refresh: the TTL is 300 s"
    );
}

/// Arena relaunched between two polls (no push, no failed probe): the first
/// tracked param answers 404. The push path's stale-map refresh runs on this
/// tick; the relaunched composition's new map fires the one RecoveryEvent
/// (none of the poll's own) and its body says no clip runs.
#[tokio::test]
async fn a_poll_404_refreshes_the_stale_map_and_reads_the_relaunched_composition() {
    let server = arena().await;
    composition_sequence(&server, snv(), relaunched()).await;
    // No param is mounted: the old ids answer 404 (wiremock's default).
    let (mut driver, mut rx, store) = driver_on(&server);
    let base = Instant::now();
    driver.on_tick_at(base).await;
    assert_eq!(driver.sp_clips.fact.state, ClipState::Running);

    driver.on_tick_at(base + secs(30)).await;

    assert_eq!(param_reads(&server).await, 1, "the poll stopped at the 404");
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "refreshed on this tick"
    );
    assert_eq!(driver.sp_clips.fact, none(Vec::new()));
    assert_eq!(store.current(0).state, ClipState::NotRunning);
    assert_eq!(driver.sp_clips.tracked.params()[0], FIRST_PARAM + 1_000_000);
    assert!(driver.not_ready_since.is_none(), "the new map is ready");
    assert_eq!(
        drain(&mut rx),
        1,
        "the ready transition's one RecoveryEvent"
    );
    assert_eq!(driver.consecutive_failures, 0, "a 404 counts nothing");
}

/// A 404 on a param Arena still lists (a refused id, never the steady
/// state): the refresh maps the same clips, so its body is read; the poll
/// then holds until the next full refresh (the TTL's), so a refused id
/// costs one extra 14 MB fetch per TTL, never one a minute.
#[tokio::test]
async fn a_404_on_a_param_arena_still_lists_holds_the_poll_until_the_next_refresh() {
    let server = arena().await;
    composition_always(&server, snv()).await;
    param_answers(&server, FIRST_PARAM, 404).await;
    params_answer(&server, &snv()).await;
    let (mut driver, mut rx, _store) = driver_on(&server);
    let base = Instant::now();
    driver.on_tick_at(base).await;

    driver.on_tick_at(base + secs(30)).await;
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "the stale map refreshed"
    );
    assert_eq!(driver.refused_ids_at, Some(base + secs(30)));
    assert_eq!(
        driver.sp_clips.fact,
        running(vec![yt_music(), yt_music()], vec![snv_bg()]),
        "the refresh's own body"
    );
    assert!(driver.sp_clips.held, "the 404'd param is still tracked");
    assert_eq!(drain(&mut rx), 0, "the same map: no event");

    for at in [60, 90, 300] {
        driver.on_tick_at(base + secs(at)).await;
    }
    assert_eq!(param_reads(&server).await, 1, "held: nothing polled");
    assert_eq!(composition_fetches(&server).await, 2, "held: no refresh");
    assert_eq!(driver.sp_clips.fact.state, ClipState::Running);

    // The TTL's refresh (300 s after the last one) reads a body and ends
    // the hold; the next due poll meets the refused id once more.
    driver.on_tick_at(base + secs(330)).await;
    assert_eq!(composition_fetches(&server).await, 3, "the TTL refresh");
    assert!(!driver.sp_clips.held);
    assert_eq!(param_reads(&server).await, 1, "the refresh was the read");
    driver.on_tick_at(base + secs(360)).await;
    assert_eq!(param_reads(&server).await, 2);
    assert_eq!(composition_fetches(&server).await, 4);
    assert!(driver.sp_clips.held);
    assert_eq!(driver.sp_clips.fact.state, ClipState::Running);
    assert!(driver.not_ready_since.is_none());
}

/// The operator deleted a SongPlayer clip: its param answers 404 and the
/// refresh maps the same text clips, but the deleted clip is no longer
/// tracked, so nothing holds and the next poll reads the 28 params left.
#[tokio::test]
async fn a_deleted_clip_s_404_refreshes_and_the_poll_goes_on() {
    let server = arena().await;
    let mut without = snv();
    for layer in without["layers"].as_array_mut().unwrap() {
        layer["clips"]
            .as_array_mut()
            .unwrap()
            .retain(|clip| clip["id"] != YT_A);
    }
    composition_sequence(&server, snv(), without).await;
    param_answers(&server, YT_A + 1, 404).await;
    params_answer(&server, &snv()).await;
    let (mut driver, _rx, _store) = driver_on(&server);
    let base = Instant::now();
    driver.on_tick_at(base).await;

    driver.on_tick_at(base + secs(30)).await;
    // In id order YT_A's connected is the 21st tracked param.
    assert_eq!(param_reads(&server).await, 21);
    assert_eq!(composition_fetches(&server).await, 2);
    assert_eq!(
        driver.sp_clips.fact,
        running(vec![yt_music()], vec![snv_bg()])
    );
    assert!(!driver.sp_clips.held, "the 404'd param is gone");

    driver.on_tick_at(base + secs(60)).await;
    assert_eq!(param_reads(&server).await, 21 + 28);
    assert_eq!(
        driver.sp_clips.fact,
        running(vec![yt_music()], vec![snv_bg()])
    );
}

/// A poll past its budget is unknown: a slow Arena never holds the driver's
/// step (and the title and lyric pushes behind it) longer than the budget.
#[tokio::test]
async fn a_poll_past_its_budget_is_unknown() {
    let server = arena().await;
    composition_always(&server, snv()).await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/parameter/by-id/{FIRST_PARAM}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": FIRST_PARAM, "value": false}))
                .set_delay(secs(30)),
        )
        .mount(&server)
        .await;
    params_answer(&server, &snv()).await;
    let (mut driver, _rx, _store) = driver_on(&server);
    driver.sp_clips.budget = Some(Duration::from_millis(200));
    let base = Instant::now();
    driver.on_tick_at(base).await;

    driver.on_tick_at(base + secs(30)).await;

    // At most the delayed first read reached Arena (under a ptrace stall the
    // budget may run out before it even arrives: never a wall-time pin).
    assert!(
        param_reads(&server).await <= 1,
        "ended while the first one waited"
    );
    assert_eq!(
        driver.sp_clips.fact,
        Fact::unknown("the poll of 29 params took over 200ms")
    );
    assert_eq!(driver.consecutive_failures, 0);
    assert_eq!(composition_fetches(&server).await, 1);
}

/// An answer that does not decode as a param is SongPlayer's own reader
/// failing: a reader fault, which the post-deploy gate fails on.
#[tokio::test]
async fn an_answer_that_is_no_param_is_a_reader_fault() {
    let server = arena().await;
    composition_always(&server, snv()).await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/parameter/by-id/{FIRST_PARAM}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": FIRST_PARAM})))
        .mount(&server)
        .await;
    params_answer(&server, &snv()).await;
    let (mut driver, _rx, store) = driver_on(&server);
    let base = Instant::now();
    driver.on_tick_at(base).await;

    driver.on_tick_at(base + secs(30)).await;

    let fact = &driver.sp_clips.fact;
    assert_eq!(fact.state, ClipState::Unknown);
    assert!(fact.reader_fault, "{fact:?}");
    assert!(
        fact.why
            .as_deref()
            .is_some_and(|why| why.starts_with("param 1681395963139: ")),
        "{fact:?}"
    );
    let published = store.current(0);
    assert!(published.reader_fault);
    assert_eq!(published.read_error, fact.why);
}

/// A param that does not read (a 500: Arena's REST choking) makes the fact
/// unknown; it never counts toward the breaker and refreshes nothing.
#[tokio::test]
async fn a_param_that_does_not_read_is_unknown_and_counts_nothing() {
    let server = arena().await;
    composition_always(&server, snv()).await;
    param_answers(&server, FIRST_PARAM, 500).await;
    params_answer(&server, &snv()).await;
    let (mut driver, _rx, store) = driver_on(&server);
    let base = Instant::now();
    driver.on_tick_at(base).await;

    driver.on_tick_at(base + secs(30)).await;

    assert_eq!(
        param_reads(&server).await,
        1,
        "the poll stopped at the failure"
    );
    assert_eq!(driver.sp_clips.fact.state, ClipState::Unknown);
    assert!(
        driver
            .sp_clips
            .fact
            .why
            .as_deref()
            .is_some_and(|why| why.starts_with("param 1681395963139: ")),
        "{:?}",
        driver.sp_clips.fact
    );
    assert_eq!(store.current(0).state, ClipState::Unknown);
    assert!(
        !driver.sp_clips.fact.reader_fault,
        "Arena choking is no reader fault"
    );
    assert_eq!(driver.consecutive_failures, 0);
    assert!(!driver.circuit_breaker_open);
    assert!(driver.not_ready_since.is_none());
    assert_eq!(composition_fetches(&server).await, 1);
}

/// The poll waits while the breaker is open or a not-ready episode is open
/// (its refreshes read their bodies), and runs once both are over.
#[tokio::test]
async fn no_poll_with_the_breaker_open_or_a_not_ready_episode() {
    let server = arena().await;
    composition_always(&server, snv()).await;
    params_answer(&server, &snv()).await;
    let (mut driver, _rx, _store) = driver_on(&server);
    let base = Instant::now();
    driver.on_tick_at(base).await;
    let due = base + secs(60);

    driver.not_ready_since = Some(base + secs(1));
    driver.poll_sp_clips(due).await;
    assert_eq!(param_reads(&server).await, 0, "a not-ready episode");

    driver.not_ready_since = None;
    driver.circuit_breaker_open = true;
    driver.poll_sp_clips(due).await;
    assert_eq!(param_reads(&server).await, 0, "the breaker is open");

    driver.circuit_breaker_open = false;
    driver.poll_sp_clips(due).await;
    assert_eq!(param_reads(&server).await, TRACKED);
}

/// A tick whose liveness probe failed reads nothing; the next ok tick does
/// (after the flip's one-param probe of the title clip).
#[tokio::test]
async fn a_failed_probe_skips_the_poll() {
    let server = MockServer::start().await;
    for status in [200_u16, 500] {
        Mock::given(method("GET"))
            .and(path("/api/v1/product"))
            .respond_with(ResponseTemplate::new(status))
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/api/v1/product"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    composition_always(&server, snv()).await;
    param_answers(&server, TITLE_TEXT, 200).await;
    params_answer(&server, &snv()).await;
    let (mut driver, _rx, _store) = driver_on(&server);
    let base = Instant::now();
    driver.on_tick_at(base).await;

    driver.on_tick_at(base + secs(30)).await;
    assert_eq!(param_reads(&server).await, 0, "the probe failed");
    assert_eq!(driver.consecutive_failures, 1);

    driver.on_tick_at(base + secs(31)).await;
    assert_eq!(
        param_reads(&server).await,
        1 + TRACKED,
        "the flip's title probe, then the poll"
    );
    assert_eq!(composition_fetches(&server).await, 1);
}

/// Opening the breaker evicts the clip map and forgets the tracked clips:
/// Arena does not answer, so nothing is known until a refresh tracks them.
#[tokio::test]
async fn the_outage_forgets_the_clips() {
    let server = arena().await;
    composition_always(&server, snv()).await;
    let (mut driver, _rx, store) = driver_on(&server);
    let base = Instant::now();
    driver.on_tick_at(base).await;
    assert_eq!(store.current(0).state, ClipState::Running);

    for _ in 0..3 {
        driver.apply_outcome(false);
    }

    assert!(driver.circuit_breaker_open);
    assert_eq!(
        driver.sp_clips.fact,
        Fact::unknown("Arena does not answer (the circuit breaker is open)")
    );
    assert_eq!(driver.sp_clips.tracked, Tracked::NotLoaded);
    assert_eq!(store.current(0).state, ClipState::Unknown);
}

/// A composition with no SongPlayer clip: none runs, and nothing is polled.
#[tokio::test]
async fn a_composition_with_no_songplayer_clip_is_none_and_never_polled() {
    let server = arena().await;
    composition_always(&server, composition(&[(SUBS_TOKEN, 100, 900)])).await;
    let (mut driver, _rx, store) = driver_on(&server);
    let base = Instant::now();

    for at in [0, 30, 60, 90] {
        driver.on_tick_at(base + secs(at)).await;
    }

    assert_eq!(driver.sp_clips.fact, none(Vec::new()));
    assert_eq!(store.current(0).state, ClipState::NotRunning);
    assert_eq!(param_reads(&server).await, 0);
}
