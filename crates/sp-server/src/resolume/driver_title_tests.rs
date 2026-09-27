//! #217 addendum 3 — the driver owns the wall title: each title command acts
//! only on a difference from what it last did to the `#sp-title` clips, and a
//! `Resync` supersedes the title commands queued before it. wiremock request
//! sequences, like `driver_relaunch_tests.rs`.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use wiremock::MockServer;

use super::super::not_ready_tests::{arena, composition_fetches, composition_sequence, drain};
use super::super::relaunch_tests::{
    composition, composition_always, mapped_driver, opacities_put, put_answers, texts_put,
};
use crate::resolume::driver::{ClipInfo, HostDriver};
use crate::resolume::title_state::{TitleState, WallTitle, take_queued};
use crate::resolume::{ResolumeCommand, SUBS_TOKEN, TITLE_TOKEN};

/// The `#sp-title` clip 100 with its text param 900.
fn title_clip() -> Vec<ClipInfo> {
    vec![ClipInfo {
        clip_id: 100,
        text_param_id: 900,
    }]
}

/// An Arena whose composition maps `#sp-title` on clip 100 / param 900, both
/// answering every PUT; the driver's startup refresh has mapped it.
async fn title_arena() -> (MockServer, HostDriver) {
    let server = arena().await;
    composition_always(&server, composition(&[(TITLE_TOKEN, 100, 900)])).await;
    put_answers(&server, "/api/v1/parameter/by-id/900", 204).await;
    put_answers(&server, "/api/v1/composition/clips/by-id/100", 204).await;
    let (driver, _rx) = mapped_driver(&server, Instant::now()).await;
    (server, driver)
}

/// Every PUT in order, as `text <param> = "<value>"` / `opacity <clip> =
/// <value>`: the request sequence the wall sees.
async fn put_sequence(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "PUT")
        .map(|r| {
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            let id = r.url.path().rsplit('/').next().unwrap().to_string();
            match body["value"].as_str() {
                Some(text) => format!("text {id} = {text:?}"),
                None => format!("opacity {id} = {}", body["video"]["opacity"]["value"]),
            }
        })
        .collect()
}

fn resync(title: Option<&str>) -> ResolumeCommand {
    ResolumeCommand::Resync {
        title: title.map(str::to_string),
    }
}

fn show_title() -> ResolumeCommand {
    ResolumeCommand::ShowTitle {
        song: "Song".into(),
        artist: "Artist".into(),
    }
}

/// The 20-step fade-in's opacities, 0.05 … 1.0.
fn fade_in() -> Vec<f64> {
    crate::resolume::handlers::fade_steps(20)
}

// -- the acceptance: act only on a difference ---------------------------------

/// The title the engine wants is already up: no request at all. Before, the
/// recovery re-pushed a ShowTitle, whose fade from 5 % was a visible blink.
#[tokio::test]
async fn a_resync_of_the_title_already_up_sends_nothing() {
    let (server, mut driver) = title_arena().await;
    driver.title = WallTitle::at(TitleState::Shown("Song - Artist".into()), title_clip());

    driver.handle_command(resync(Some("Song - Artist"))).await;

    assert!(put_sequence(&server).await.is_empty(), "no request");
    assert_eq!(
        driver.title.state(),
        &TitleState::Shown("Song - Artist".into())
    );
}

#[tokio::test]
async fn a_resync_of_a_hidden_title_fades_it_in_once() {
    let (server, mut driver) = title_arena().await;
    driver.title = WallTitle::at(TitleState::Hidden, title_clip());

    driver.handle_command(resync(Some("Song - Artist"))).await;

    assert_eq!(texts_put(&server, 900).await, ["Song - Artist"]);
    assert_eq!(
        opacities_put(&server, 100).await,
        fade_in(),
        "one fade-in, 5 % to 100 %"
    );
    assert_eq!(
        driver.title.state(),
        &TitleState::Shown("Song - Artist".into())
    );
    assert_eq!(
        composition_fetches(&server).await,
        1,
        "every request answered: no stale-map refresh"
    );
}

/// No title must be up (outside the song's title window): an instant hide,
/// never a fade from full opacity.
#[tokio::test]
async fn a_resync_naming_no_title_hides_a_shown_title_at_once() {
    let (server, mut driver) = title_arena().await;
    driver.title = WallTitle::at(TitleState::Shown("Song - Artist".into()), title_clip());

    driver.handle_command(resync(None)).await;

    assert_eq!(
        put_sequence(&server).await,
        ["opacity 100 = 0.0", "text 900 = \"\""],
        "opacity 0 at once, then the text cleared"
    );
    assert_eq!(driver.title.state(), &TitleState::Hidden);
}

/// Review of addendum 2 (comment 5859006863): a ShowTitle queued behind a slow
/// driver step, then the engine's re-sync saying no title must be up (the song
/// ended, or its title window closed). The queued show faded the title in,
/// and the hide then took it down: a flash. The Resync supersedes it, so the
/// title that was up goes down at once and the queued title never appears.
#[tokio::test]
async fn a_queued_show_then_a_resync_naming_no_title_ends_hidden_without_a_flash() {
    let (server, mut driver) = title_arena().await;
    driver.title = WallTitle::at(TitleState::Shown("Old - Title".into()), title_clip());
    let (tx, mut rx) = mpsc::channel(8);
    tx.send(show_title()).await.unwrap();
    tx.send(resync(None)).await.unwrap();

    let first = rx.recv().await.unwrap();
    for cmd in take_queued(first, &mut rx) {
        driver.handle_command(cmd).await;
    }

    assert_eq!(
        put_sequence(&server).await,
        ["opacity 100 = 0.0", "text 900 = \"\""],
        "no fade-in, no \"Song - Artist\" on the wall: only the instant hide"
    );
    assert_eq!(driver.title.state(), &TitleState::Hidden);
}

// -- plain ShowTitle / HideTitle go through the same state ---------------------

/// The show timer's ShowTitle after a re-sync already showed that title: no
/// second fade.
#[tokio::test]
async fn a_show_title_of_the_title_already_up_runs_no_second_fade() {
    let (server, mut driver) = title_arena().await;

    driver.handle_command(show_title()).await;
    driver.handle_command(show_title()).await;

    assert_eq!(texts_put(&server, 900).await, ["Song - Artist"]);
    assert_eq!(opacities_put(&server, 100).await, fade_in(), "one fade");
}

/// A HideTitle fades only a title known to be up. At startup the driver does
/// not know what the clip shows (a SongPlayer restart, or Arena's restored
/// composition), so it hides at once; once hidden, a second hide is nothing.
#[tokio::test]
async fn a_hide_title_hides_at_once_unless_the_title_is_known_to_be_up() {
    let (server, mut driver) = title_arena().await;
    assert_eq!(driver.title.state(), &TitleState::Unknown, "startup");

    driver.handle_command(ResolumeCommand::HideTitle).await;
    driver.handle_command(ResolumeCommand::HideTitle).await;

    assert_eq!(
        put_sequence(&server).await,
        ["opacity 100 = 0.0", "text 900 = \"\""],
        "one instant hide, then nothing"
    );
    assert_eq!(driver.title.state(), &TitleState::Hidden);
}

// -- the state holds only for the clips it was reached on ----------------------

/// Arena relaunched: the title clip has a new id and shows whatever Arena
/// restored. The pre-relaunch `Shown` no longer holds, so the re-sync fades
/// the title in on the new clip.
#[tokio::test]
async fn new_title_clip_ids_let_the_resync_show_the_title_again() {
    let server = arena().await;
    composition_sequence(
        &server,
        composition(&[(TITLE_TOKEN, 100, 900)]),
        composition(&[(TITLE_TOKEN, 200, 1900)]),
    )
    .await;
    for route in [
        "/api/v1/parameter/by-id/900",
        "/api/v1/composition/clips/by-id/100",
        "/api/v1/parameter/by-id/1900",
        "/api/v1/composition/clips/by-id/200",
    ] {
        put_answers(&server, route, 204).await;
    }
    let (mut driver, _rx) = mapped_driver(&server, Instant::now()).await;
    driver.handle_command(show_title()).await;
    assert_eq!(
        driver.title.state(),
        &TitleState::Shown("Song - Artist".into())
    );

    driver.refresh_mapping(Instant::now()).await.unwrap();
    assert_eq!(
        driver.title.state(),
        &TitleState::Unknown,
        "new ids: the relaunched clip shows what Arena restored"
    );
    driver.handle_command(resync(Some("Song - Artist"))).await;

    assert_eq!(texts_put(&server, 1900).await, ["Song - Artist"]);
    assert_eq!(opacities_put(&server, 200).await, fade_in());
}

/// An outage without a relaunch: the breaker evicts the map, and the
/// breaker-closed refresh maps the SAME ids. The clip is as the driver left
/// it, so the RecoveryEvent's re-sync of that title sends nothing (before,
/// its ShowTitle re-ran the fade: a blink after every long REST hang).
#[tokio::test]
async fn an_outage_that_maps_the_same_title_clip_keeps_the_title_up() {
    let (server, mut driver) = title_arena().await;
    let (tx, mut events) = tokio::sync::broadcast::channel(16);
    driver = driver.with_recovery_channel(tx);
    driver.handle_command(show_title()).await;
    let before = put_sequence(&server).await.len();
    let base = Instant::now();

    for _ in 0..3 {
        driver.apply_outcome(false);
    }
    assert!(
        driver.clip_mapping.is_empty(),
        "the breaker evicted the map"
    );
    driver.on_tick_at(base + Duration::from_secs(61)).await;
    assert_eq!(drain(&mut events), 1, "the breaker close's RecoveryEvent");
    assert_eq!(composition_fetches(&server).await, 2, "the same ids mapped");

    driver.handle_command(resync(Some("Song - Artist"))).await;

    assert_eq!(
        put_sequence(&server).await.len(),
        before,
        "the title is still up: no request"
    );
}

/// With no `#sp-title` clip mapped nothing is shown, so nothing is recorded:
/// the re-sync after the clip appears still shows the title.
#[tokio::test]
async fn a_title_command_without_a_title_clip_keeps_the_state() {
    let server = arena().await;
    composition_sequence(
        &server,
        composition(&[(SUBS_TOKEN, 101, 901)]),
        composition(&[(SUBS_TOKEN, 101, 901), (TITLE_TOKEN, 100, 900)]),
    )
    .await;
    put_answers(&server, "/api/v1/parameter/by-id/900", 204).await;
    put_answers(&server, "/api/v1/composition/clips/by-id/100", 204).await;
    let (mut driver, _rx) = mapped_driver(&server, Instant::now()).await;

    driver.handle_command(resync(Some("Song - Artist"))).await;
    assert!(put_sequence(&server).await.is_empty(), "no title clip");
    assert_eq!(driver.title.state(), &TitleState::Unknown);

    driver.refresh_mapping(Instant::now()).await.unwrap();
    driver.handle_command(resync(Some("Song - Artist"))).await;
    assert_eq!(texts_put(&server, 900).await, ["Song - Artist"]);
    assert_eq!(opacities_put(&server, 100).await, fade_in());
}
