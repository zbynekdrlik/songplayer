//! #217 addendum 2 — the driver's Arena-relaunch edge cases. Arena gives
//! every clip and text param a new id on each relaunch, so a push answered
//! 404 marks the clip map stale; a RecoveryEvent fires only on a real
//! recovery; a non-2xx `/composition` is a failed fetch; a not-ready episode
//! ticks every 2 s. Split from `driver_not_ready_tests.rs` for the 1000-line
//! cap.

use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::not_ready_tests::{arena, composition_fetches, composition_sequence, drain};
use super::*;
use crate::resolume::title_state::{TitleState, WallTitle};
use crate::resolume::{RecoveryEvent, ResolumeCommand, SUBS_TOKEN, TITLE_TOKEN};

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

/// A composition with one text clip per `(token, clip id, text param id)`.
pub(super) fn composition(clips: &[(&str, i64, i64)]) -> serde_json::Value {
    let clips: Vec<serde_json::Value> = clips
        .iter()
        .map(|(token, clip_id, param_id)| {
            serde_json::json!({
                "id": clip_id,
                "name": { "value": token },
                "video": { "sourceparams": { "Text": { "id": param_id, "valuetype": "ParamText" } } }
            })
        })
        .collect();
    serde_json::json!({ "layers": [{ "clips": clips }] })
}

/// `/composition` always answers `body`.
pub(super) async fn composition_always(server: &MockServer, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// A `PUT` on `route` answers `status`. An unmounted route answers 404 too
/// (wiremock's default), so every route a test pushes to is mounted.
pub(super) async fn put_answers(server: &MockServer, route: &str, status: u16) {
    Mock::given(method("PUT"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

/// The `value` of every text PUT to param `param_id`, in order.
pub(super) async fn texts_put(server: &MockServer, param_id: i64) -> Vec<String> {
    let route = format!("/api/v1/parameter/by-id/{param_id}");
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == route)
        .map(|r| {
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            body["value"].as_str().unwrap().to_string()
        })
        .collect()
}

/// How many opacity PUTs reached clip `clip_id`.
async fn opacity_puts(server: &MockServer, clip_id: i64) -> usize {
    let route = format!("/api/v1/composition/clips/by-id/{clip_id}");
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == route)
        .count()
}

/// The opacity of every PUT to clip `clip_id`, in order.
pub(super) async fn opacities_put(server: &MockServer, clip_id: i64) -> Vec<f64> {
    let route = format!("/api/v1/composition/clips/by-id/{clip_id}");
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == route)
        .map(|r| {
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            body["video"]["opacity"]["value"].as_f64().unwrap()
        })
        .collect()
}

fn subtitle(en: &str) -> ResolumeCommand {
    ResolumeCommand::ShowSubtitles {
        en: en.to_string(),
        next_en: String::new(),
        sk: None,
        next_sk: None,
        suppress_en: false,
    }
}

/// A driver whose startup refresh at `base` mapped the composition's first
/// answer (the ids from before the relaunch).
pub(super) async fn mapped_driver(
    server: &MockServer,
    base: Instant,
) -> (HostDriver, broadcast::Receiver<RecoveryEvent>) {
    let (tx, rx) = broadcast::channel(16);
    let mut driver =
        HostDriver::new("127.0.0.1".into(), server.address().port()).with_recovery_channel(tx);
    driver.on_tick_at(base).await;
    (driver, rx)
}

// -- item 1: a push answered 404 marks the clip map stale ---------------------

/// The box (2026-09-27): Arena was relaunched quicker than three failed
/// probes, so the breaker never opened, and every push went to the ids from
/// before the relaunch (`404 Not Found`) until the 300 s TTL. The 404 marks
/// the map stale: the refresh runs at once (NotReady path), and the push is
/// retried once on the id Arena gave the clip.
#[tokio::test]
async fn a_404_on_a_param_push_refreshes_the_stale_map_and_retries_the_push_once() {
    let server = arena().await;
    composition_sequence(
        &server,
        composition(&[(SUBS_TOKEN, 100, 900)]),
        composition(&[(SUBS_TOKEN, 200, 1900)]),
    )
    .await;
    put_answers(&server, "/api/v1/parameter/by-id/900", 404).await;
    put_answers(&server, "/api/v1/parameter/by-id/1900", 204).await;
    let (mut driver, mut rx) = mapped_driver(&server, Instant::now()).await;
    assert_eq!(composition_fetches(&server).await, 1, "the startup refresh");

    driver.handle_command(subtitle("Line one")).await;

    assert_eq!(
        texts_put(&server, 900).await,
        ["Line one"],
        "the push went to the id from before the relaunch, once"
    );
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "the 404 refreshed the stale map at once, not at the TTL"
    );
    assert_eq!(
        texts_put(&server, 1900).await,
        ["Line one"],
        "the push was retried once, on the clip's new text param id"
    );
    assert!(
        driver.not_ready_since.is_none(),
        "the refresh mapped SongPlayer's clips: the episode is over"
    );
    assert_eq!(
        drain(&mut rx),
        1,
        "the ready transition fires one RecoveryEvent (the title and line re-push)"
    );
}

/// A ShowTitle answered 404: its refresh ends the episode with a
/// RecoveryEvent, whose engine re-push shows the title. Retrying the
/// ShowTitle too would run a second fade from 5 %, a visible blink.
#[tokio::test]
async fn a_404_on_a_title_push_is_re_pushed_by_the_recovery_event_not_retried() {
    let server = arena().await;
    composition_sequence(
        &server,
        composition(&[(TITLE_TOKEN, 100, 900)]),
        composition(&[(TITLE_TOKEN, 200, 1900)]),
    )
    .await;
    // The startup state is Unknown, so the ShowTitle is a Replace: its cut to
    // opacity 0 answers, and the text PUT on the dead param is the 404.
    put_answers(&server, "/api/v1/composition/clips/by-id/100", 204).await;
    put_answers(&server, "/api/v1/parameter/by-id/900", 404).await;
    put_answers(&server, "/api/v1/parameter/by-id/1900", 204).await;
    put_answers(&server, "/api/v1/composition/clips/by-id/200", 204).await;
    let (mut driver, mut rx) = mapped_driver(&server, Instant::now()).await;

    driver
        .handle_command(ResolumeCommand::ShowTitle {
            song: "Song".into(),
            artist: "Artist".into(),
        })
        .await;

    assert_eq!(
        composition_fetches(&server).await,
        2,
        "the 404 refreshed the stale map"
    );
    assert_eq!(
        drain(&mut rx),
        1,
        "one RecoveryEvent: its re-push shows the title on the new ids"
    );
    assert!(
        texts_put(&server, 1900).await.is_empty(),
        "no retried ShowTitle text"
    );
    assert_eq!(
        opacity_puts(&server, 200).await,
        0,
        "no retried fade: the re-push runs the only one"
    );
}

/// A 404 on a clip id (the opacity PUT) is a stale map too. A HideTitle is
/// retried on the new clip id AT ONCE (opacity 0, then the text cleared):
/// the relaunched clip holds whatever Arena's saved composition restored,
/// possibly at opacity 0, and `hide_title`'s fade starts at FULL opacity, a
/// 1 s flash of that stale text (review round 3). The new clip ids made the
/// driver's title state `Unknown`, which is why the retry hides at once
/// (#217 addendum 3). The RecoveryEvent's re-sync names no title after the
/// song's hide point
/// (`handle_resolume_recovery_does_not_re_show_a_title_the_song_end_hid`).
#[tokio::test]
async fn a_404_on_a_clip_opacity_push_marks_the_map_stale_too() {
    let server = arena().await;
    composition_sequence(
        &server,
        composition(&[(TITLE_TOKEN, 100, 900)]),
        composition(&[(TITLE_TOKEN, 200, 1900)]),
    )
    .await;
    put_answers(&server, "/api/v1/composition/clips/by-id/100", 404).await;
    put_answers(&server, "/api/v1/composition/clips/by-id/200", 204).await;
    put_answers(&server, "/api/v1/parameter/by-id/1900", 204).await;
    let (mut driver, mut rx) = mapped_driver(&server, Instant::now()).await;
    // The song's title is up, so its HideTitle fades out (#217 addendum 3:
    // only a title known to be up fades).
    driver.title = WallTitle::at(
        TitleState::Shown("Song - Artist".into()),
        vec![ClipInfo {
            clip_id: 100,
            text_param_id: 900,
        }],
    );

    driver.handle_command(ResolumeCommand::HideTitle).await;

    assert_eq!(
        opacity_puts(&server, 100).await,
        1,
        "the fade-out stopped at the dead clip id"
    );
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "the 404 refreshed the stale map"
    );
    assert_eq!(
        opacities_put(&server, 200).await,
        [0.0],
        "the hide was retried at once on the new clip id: opacity 0, no fade from full"
    );
    assert_eq!(
        texts_put(&server, 1900).await,
        [""],
        "the retried hide cleared the title text"
    );
    assert_eq!(
        drain(&mut rx),
        1,
        "the ready transition's one RecoveryEvent"
    );
}

/// Review round 4: the instant hide that retries a HideTitle ends through
/// `finish_push` like every push. When it 404s too, the note is cleared, so
/// the next push that succeeds is not read as a stale map (a spurious ~14 MB
/// refresh).
#[tokio::test]
async fn a_retried_hide_that_404s_leaves_no_stale_note_for_the_next_push() {
    let server = arena().await;
    composition_sequence(
        &server,
        composition(&[(TITLE_TOKEN, 100, 900), (SUBS_TOKEN, 101, 901)]),
        composition(&[(TITLE_TOKEN, 200, 1900), (SUBS_TOKEN, 201, 1901)]),
    )
    .await;
    put_answers(&server, "/api/v1/composition/clips/by-id/100", 404).await;
    put_answers(&server, "/api/v1/composition/clips/by-id/200", 404).await;
    put_answers(&server, "/api/v1/parameter/by-id/1901", 204).await;
    let (mut driver, _rx) = mapped_driver(&server, Instant::now()).await;

    driver.handle_command(ResolumeCommand::HideTitle).await;
    assert_eq!(
        opacities_put(&server, 200).await,
        [0.0],
        "the refresh ran and the hide was retried, and 404'd again"
    );
    assert_eq!(composition_fetches(&server).await, 2);

    driver.handle_command(subtitle("Line one")).await;

    assert_eq!(texts_put(&server, 1901).await, ["Line one"]);
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "a push that succeeded refreshes nothing"
    );
}

#[tokio::test]
async fn a_push_that_succeeds_refreshes_nothing() {
    let server = arena().await;
    composition_always(&server, composition(&[(SUBS_TOKEN, 100, 900)])).await;
    put_answers(&server, "/api/v1/parameter/by-id/900", 204).await;
    let (mut driver, mut rx) = mapped_driver(&server, Instant::now()).await;

    driver.handle_command(subtitle("Line one")).await;

    assert_eq!(texts_put(&server, 900).await, ["Line one"]);
    assert_eq!(
        composition_fetches(&server).await,
        1,
        "only the startup refresh"
    );
    assert!(driver.not_ready_since.is_none(), "the map is still ready");
    assert_eq!(drain(&mut rx), 0);
}

/// Only a 404 means a dead id. Any other failed push (Arena's REST choking)
/// keeps the map: a refresh would not help, and it costs ~14 MB.
#[tokio::test]
async fn a_push_answered_500_does_not_mark_the_map_stale() {
    let server = arena().await;
    composition_always(&server, composition(&[(SUBS_TOKEN, 100, 900)])).await;
    put_answers(&server, "/api/v1/parameter/by-id/900", 500).await;
    let (mut driver, mut rx) = mapped_driver(&server, Instant::now()).await;

    driver.handle_command(subtitle("Line one")).await;

    assert_eq!(texts_put(&server, 900).await, ["Line one"], "no retry");
    assert_eq!(
        composition_fetches(&server).await,
        1,
        "only the startup refresh"
    );
    assert!(driver.not_ready_since.is_none());
    assert_eq!(drain(&mut rx), 0);
}

/// An id Arena keeps refusing while its composition still lists it: not a
/// relaunch, the refresh maps the same clips. It lost nothing, so no
/// RecoveryEvent (its re-push would only 404 again, a loop with no timer in
/// it) and no retry. Every push would cost a ~14 MB fetch, so such a 404 is
/// not refreshed again for the 60 s retry window.
#[tokio::test]
async fn an_id_the_composition_still_lists_is_refreshed_at_most_once_per_retry_window() {
    let server = arena().await;
    composition_always(&server, composition(&[(SUBS_TOKEN, 100, 900)])).await;
    put_answers(&server, "/api/v1/parameter/by-id/900", 404).await;
    let base = Instant::now();
    let (mut driver, mut rx) = mapped_driver(&server, base).await;
    let line = subtitle("Line one");

    driver.run_push(&line, base + secs(30)).await;
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "the first 404 refreshes the map"
    );
    assert_eq!(
        drain(&mut rx),
        0,
        "the map came back unchanged: nothing to re-push"
    );
    assert_eq!(
        texts_put(&server, 900).await.len(),
        1,
        "no retry: the same id would 404 again"
    );
    assert!(driver.not_ready_since.is_none(), "the map is ready");

    for s in [31, 89] {
        driver.run_push(&line, base + secs(s)).await;
        assert_eq!(
            composition_fetches(&server).await,
            2,
            "{} s after that refresh a 404 on the same id refreshes nothing",
            s - 30
        );
    }

    driver.run_push(&line, base + secs(90)).await;
    assert_eq!(
        composition_fetches(&server).await,
        3,
        "60 s after it a 404 refreshes once more"
    );
    assert_eq!(drain(&mut rx), 0, "never a RecoveryEvent");
}

/// Arena relaunched twice within a minute. The first relaunch's 404 mapped
/// new ids, so the second relaunch's 404 is a stale map again and refreshes
/// at once; only an id the composition still lists holds the refresh back.
#[tokio::test]
async fn a_second_relaunch_within_a_minute_is_refreshed_at_once() {
    let server = arena().await;
    let relaunch_1 = composition(&[(SUBS_TOKEN, 200, 1900)]);
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(composition(&[(SUBS_TOKEN, 100, 900)])),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(relaunch_1))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    composition_always(&server, composition(&[(SUBS_TOKEN, 300, 2900)])).await;
    put_answers(&server, "/api/v1/parameter/by-id/900", 404).await;
    // 1900 works until the second relaunch, then 404s.
    Mock::given(method("PUT"))
        .and(path("/api/v1/parameter/by-id/1900"))
        .respond_with(ResponseTemplate::new(204))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    put_answers(&server, "/api/v1/parameter/by-id/1900", 404).await;
    put_answers(&server, "/api/v1/parameter/by-id/2900", 204).await;
    let base = Instant::now();
    let (mut driver, mut rx) = mapped_driver(&server, base).await;

    driver.run_push(&subtitle("Line one"), base + secs(5)).await;
    assert_eq!(texts_put(&server, 1900).await, ["Line one"]);
    assert_eq!(drain(&mut rx), 1, "relaunch 1: one RecoveryEvent");

    driver
        .run_push(&subtitle("Line two"), base + secs(38))
        .await;
    assert_eq!(
        composition_fetches(&server).await,
        3,
        "33 s later the second relaunch's 404 refreshes at once"
    );
    assert_eq!(
        texts_put(&server, 2900).await,
        ["Line two"],
        "and the push is retried on the newest id"
    );
    assert_eq!(drain(&mut rx), 1, "relaunch 2: one RecoveryEvent");
}

/// A 404 whose refresh fails (Arena's REST chokes on the composition): the
/// episode stays open for the ticks to fetch, and the push is not retried
/// against the old, dead ids.
#[tokio::test]
async fn a_404_whose_refresh_fails_is_not_retried() {
    let server = arena().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(composition(&[(SUBS_TOKEN, 100, 900)])),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    put_answers(&server, "/api/v1/parameter/by-id/900", 404).await;
    let base = Instant::now();
    let (mut driver, mut rx) = mapped_driver(&server, base).await;

    driver
        .run_push(&subtitle("Line one"), base + secs(30))
        .await;

    assert_eq!(composition_fetches(&server).await, 2, "the refresh ran");
    assert_eq!(texts_put(&server, 900).await.len(), 1, "no retry");
    assert_eq!(
        driver.not_ready_since,
        Some(base + secs(30)),
        "the episode stays open for the ticks"
    );
    assert_eq!(drain(&mut rx), 0);
}

/// A 404 while the last `/composition` attempt failed: the episode starts,
/// but the 60 s retry window holds the refresh (the #157 case). A later 404
/// that refreshes into a composition Arena is still loading keeps the
/// episode's start, so its fast window never extends.
#[tokio::test]
async fn a_404_after_a_failed_fetch_waits_out_the_retry_window_and_keeps_the_episode_start() {
    let server = arena().await;
    composition_sequence(
        &server,
        composition(&[(SUBS_TOKEN, 100, 900)]),
        serde_json::json!({"layers": []}),
    )
    .await;
    put_answers(&server, "/api/v1/parameter/by-id/900", 404).await;
    let base = Instant::now();
    let (mut driver, _rx) = mapped_driver(&server, base).await;
    // The TTL refresh at 300 s failed (Arena choking on the composition).
    driver.last_full_attempt_at = Some(base + secs(300));
    driver.last_full_attempt_failed = true;

    driver
        .run_push(&subtitle("Line one"), base + secs(310))
        .await;
    assert_eq!(
        driver.not_ready_since,
        Some(base + secs(310)),
        "the 404 starts a not-ready episode"
    );
    assert_eq!(
        composition_fetches(&server).await,
        1,
        "the failed last attempt keeps the 60 s retry window"
    );

    driver
        .run_push(&subtitle("Line two"), base + secs(370))
        .await;
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "60 s after the failed attempt the next 404 refreshes"
    );
    assert_eq!(
        driver.not_ready_since,
        Some(base + secs(310)),
        "Arena is still loading: the open episode keeps its start"
    );
}

// -- item 2: a RecoveryEvent only on a real recovery ---------------------------

/// A single failed `/composition` (the TTL refresh) and then an ok probe:
/// nothing was evicted, so nothing is re-pushed. Before, the ok probe fired a
/// RecoveryEvent, which re-pushed every host and restarted the title fade.
#[tokio::test]
async fn a_single_failed_composition_then_an_ok_probe_fires_no_recovery_event() {
    let server = arena().await;
    let ready = composition(&[(SUBS_TOKEN, 100, 900)]);
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ready.clone()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    composition_always(&server, ready).await;
    // No relaunch: Arena still has the param the flip's staleness probe asks
    // for (#217 addendum 3; an unmounted route would answer 404).
    Mock::given(method("GET"))
        .and(path("/api/v1/parameter/by-id/900"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let base = Instant::now();
    let (mut driver, mut rx) = mapped_driver(&server, base).await;

    driver.on_tick_at(base + secs(300)).await;
    assert_eq!(
        composition_fetches(&server).await,
        2,
        "the TTL refresh ran, and failed"
    );
    assert_eq!(
        driver.consecutive_failures, 1,
        "one failure: the breaker stays closed"
    );

    driver.on_tick_at(base + secs(310)).await;
    assert_eq!(driver.consecutive_failures, 0, "the probe answered");
    assert_eq!(
        drain(&mut rx),
        0,
        "a bare failing→ok flip is no recovery: no RecoveryEvent"
    );

    driver.apply_outcome(false);
    driver.apply_outcome(true);
    assert_eq!(
        drain(&mut rx),
        0,
        "nor is a failed probe followed by an ok one"
    );
}

// -- item 3: a non-2xx /composition is a failed fetch --------------------------

#[tokio::test]
async fn a_non_2xx_composition_with_a_json_body_is_a_failed_fetch() {
    let server = arena().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/composition"))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(composition(&[(SUBS_TOKEN, 100, 900)])),
        )
        .mount(&server)
        .await;
    let mut driver = HostDriver::new("127.0.0.1".into(), server.address().port());

    let result = driver.refresh_mapping(Instant::now()).await;

    assert!(
        result.is_err(),
        "a 500 is a failed fetch, whatever its body"
    );
    assert!(
        driver.last_full_attempt_failed,
        "so `decide` keeps the 60 s retry window (#157)"
    );
    assert_eq!(
        driver.consecutive_failures, 1,
        "and the breaker bookkeeping counts it"
    );
    assert!(
        driver.clip_mapping.is_empty(),
        "the error body is never parsed as a clip map"
    );
}

// -- item 6: a not-ready episode ticks every 2 s -------------------------------

#[test]
fn the_tick_is_2_s_only_inside_a_not_ready_episode_s_fast_window() {
    let base = Instant::now();
    let liveness = secs(10);
    let mut driver = HostDriver::new("127.0.0.1".into(), 1);
    assert_eq!(
        driver.tick_period(base, liveness),
        liveness,
        "a ready map ticks at the liveness cadence"
    );
    driver.not_ready_since = Some(base);
    assert_eq!(
        driver.tick_period(base, liveness),
        secs(2),
        "a not-ready episode ticks every 2 s"
    );
    assert_eq!(
        driver.tick_period(base + Duration::from_millis(119_999), liveness),
        secs(2),
        "until its fast window closes"
    );
    assert_eq!(
        driver.tick_period(base + secs(120), liveness),
        liveness,
        "at 120 s the window is over (`<`, not `<=`)"
    );
    driver.last_full_attempt_failed = true;
    assert_eq!(
        driver.tick_period(base, liveness),
        liveness,
        "after a failed fetch `decide` waits out the retry window: no 2 s ticks"
    );
}

/// `run` re-arms the tick after every command with `tick_due_after_command`:
/// a push whose 404 put the episode on its fast path brings the next tick
/// to 2 s from now; otherwise, or when the tick is due sooner, it stays.
#[tokio::test]
async fn a_command_that_opens_a_not_ready_episode_moves_the_next_tick_up_to_2_s() {
    let base = Instant::now();
    let tnow = tokio::time::Instant::now();
    let later = tnow + secs(10);
    let mut driver = HostDriver::new("127.0.0.1".into(), 1);
    assert_eq!(
        driver.tick_due_after_command(later, tnow, base),
        later,
        "a ready map keeps the scheduled tick"
    );
    driver.not_ready_since = Some(base);
    assert_eq!(
        driver.tick_due_after_command(later, tnow, base),
        tnow + secs(2),
        "an episode on its fast path ticks 2 s from now"
    );
    let sooner = tnow + secs(1);
    assert_eq!(
        driver.tick_due_after_command(sooner, tnow, base),
        sooner,
        "a command never delays the tick"
    );
    driver.last_full_attempt_failed = true;
    assert_eq!(
        driver.tick_due_after_command(later, tnow, base),
        later,
        "after a failed fetch the retry window runs at the liveness cadence"
    );
}

/// A composition that never gets SongPlayer's clips, polled the way `run`
/// polls it (the tick period from `tick_period`, the jitter fixed at 10 s):
/// every 2 s tick fetches for the 120 s fast window (0..118 s), then the 60 s
/// retry window spaces the fetches (180, 240, 300 s). 63 fetches in 5 min.
#[tokio::test]
async fn a_not_ready_episode_refetches_every_2_s_then_falls_back_after_120_s() {
    let server = arena().await;
    composition_always(&server, composition(&[("#timer", 100, 900)])).await;
    let mut driver = HostDriver::new("127.0.0.1".into(), server.address().port());
    let base = Instant::now();
    let end = base + secs(300);

    let mut now = base;
    let mut fetched_at = Vec::new();
    let mut seen = 0;
    // Bounded (the real run is 79 ticks), so a tick period that never
    // advances the clock fails the test instead of hanging it.
    for _ in 0..200 {
        driver.on_tick_at(now).await;
        let fetches = composition_fetches(&server).await;
        if fetches > seen {
            fetched_at.push(now.duration_since(base).as_secs());
            seen = fetches;
        }
        if now >= end {
            break;
        }
        now += driver.tick_period(now, secs(10));
    }

    let mut expected: Vec<u64> = (0..120).step_by(2).collect();
    expected.extend([180, 240, 300]);
    assert_eq!(
        fetched_at, expected,
        "every 2 s for the 120 s fast window, then once per 60 s retry window"
    );
}
