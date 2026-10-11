//! #250: the lyrics worker's register check — the stale pass over a served
//! track, through a wiremock Claude that counts.

use std::sync::Arc;

use sp_core::lyrics::{LyricsLine, LyricsTrack, RegisterCheck, RegisterVerdict};
use tokio::sync::broadcast;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{CheckOutcome, MAX_FAILED_CHECKS, PEER_SCAN_EVERY, checked_now, gives_up, scan_due};
use crate::ai::AiSettings;
use crate::ai::client::AiClient;
use crate::lyrics::register_check::REGISTER_CHECK_VERSION;
use crate::lyrics::worker::LyricsWorker;
use crate::peer::rig::{TestNode, set};

const YT: &str = "reg_check_1";

/// The served track: line 2 misheard.
fn served() -> LyricsTrack {
    let line = |en: &str, sk: &str| LyricsLine {
        start_ms: 0,
        end_ms: 1000,
        en: en.into(),
        sk: Some(sk.into()),
        words: None,
    };
    LyricsTrack {
        version: 22,
        source: "gemini-3-5-transcribe".into(),
        language_source: "en".into(),
        language_translation: "sk".into(),
        lines: vec![
            line("Holy is the Lord", "Svätý je Pán"),
            line("I won't swell in the house", "Nenapuchnem v dome"),
        ],
        register: None,
    }
}

/// A Claude that answers every request with `answer`.
async fn claude(answer: &str) -> MockServer {
    let server = MockServer::start().await;
    let body = serde_json::json!({"choices": [{"message": {"content": answer}}]});
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    server
}

/// A node with one served song (`track` as its JSON, `None` = no file) and
/// a worker whose Claude is `server`.
async fn node(server: &MockServer, track: Option<&LyricsTrack>) -> (TestNode, LyricsWorker, i64) {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    sqlx::query("UPDATE videos SET normalized = 1, has_lyrics = 1 WHERE id = ?")
        .bind(id)
        .execute(node.pool())
        .await
        .unwrap();
    if let Some(track) = track {
        let json = serde_json::to_vec(track).unwrap();
        std::fs::write(node.cache().join(format!("{YT}_lyrics.json")), json).unwrap();
    }
    let (events, _) = broadcast::channel(16);
    let mut worker =
        LyricsWorker::new_for_test(node.pool().clone(), node.cache().to_path_buf(), events);
    worker.ai_client = Some(Arc::new(AiClient::new(AiSettings {
        api_url: format!("{}/v1", server.uri()),
        api_key: None,
        model: "claude-test".into(),
        system_prompt_extra: None,
    })));
    (node, worker, id)
}

fn cached(node: &TestNode) -> LyricsTrack {
    let json = std::fs::read(node.cache().join(format!("{YT}_lyrics.json"))).unwrap();
    serde_json::from_slice(&json).unwrap()
}

async fn stamped(node: &TestNode, id: i64) -> (i64, Option<String>) {
    sqlx::query_as(
        "SELECT lyrics_register_version, lyrics_register_verdict FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(node.pool())
    .await
    .unwrap()
}

async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

#[tokio::test]
async fn the_stale_pass_fixes_a_served_track_in_place_and_stamps_it() {
    let server = claude("2 | I will dwell in the house | Budem prebývať v dome").await;
    let (node, worker, id) = node(&server, Some(&served())).await;
    worker.recheck_next_stale_register().await;
    assert_eq!(requests(&server).await, 1);
    let track = cached(&node);
    assert_eq!(track.lines[1].en, "I will dwell in the house");
    assert_eq!(track.lines[1].sk.as_deref(), Some("Budem prebývať v dome"));
    assert_eq!(track.lines[0].en, "Holy is the Lord");
    let check = track.register.expect("the check is kept with the track");
    assert_eq!(check.verdict, RegisterVerdict::Fixed);
    assert_eq!(check.fixed[0].en, "I won't swell in the house");
    assert_eq!(
        stamped(&node, id).await,
        (i64::from(REGISTER_CHECK_VERSION), Some("fixed".into()))
    );
    // Checked once: the next pass asks nothing.
    worker.recheck_next_stale_register().await;
    assert_eq!(requests(&server).await, 1);
}

#[tokio::test]
async fn a_track_that_already_carries_the_check_is_stamped_without_a_call() {
    let server = claude("NONE").await;
    let mut track = served();
    track.register = Some(RegisterCheck {
        version: REGISTER_CHECK_VERSION,
        verdict: RegisterVerdict::Flagged,
        fixed: vec![],
        flagged: vec![],
    });
    let (node, worker, id) = node(&server, Some(&track)).await;
    worker.recheck_next_stale_register().await;
    assert_eq!(
        requests(&server).await,
        0,
        "a peer's copy is checked already"
    );
    assert_eq!(
        stamped(&node, id).await,
        (i64::from(REGISTER_CHECK_VERSION), Some("flagged".into()))
    );
}

#[tokio::test]
async fn an_unreadable_track_is_stamped_forward_with_no_verdict() {
    let server = claude("NONE").await;
    let (node, worker, id) = node(&server, None).await;
    worker.recheck_next_stale_register().await;
    assert_eq!(requests(&server).await, 0);
    assert_eq!(
        stamped(&node, id).await,
        (i64::from(REGISTER_CHECK_VERSION), None)
    );
}

#[tokio::test]
async fn no_check_while_paid_ai_is_off() {
    let server = claude("NONE").await;
    let (node, worker, id) = node(&server, Some(&served())).await;
    set(node.pool(), "paid_ai_enabled", "false").await;
    worker.recheck_next_stale_register().await;
    assert_eq!(requests(&server).await, 0);
    assert_eq!(stamped(&node, id).await, (0, None), "checked once it is on");
    assert_eq!(cached(&node), served());
}

#[tokio::test]
async fn an_answer_in_another_shape_is_asked_again_after_a_backoff() {
    let server = claude("Here is my review of the text.").await;
    let (node, worker, id) = node(&server, Some(&served())).await;
    worker.recheck_next_stale_register().await;
    assert_eq!(requests(&server).await, 1);
    assert_eq!(stamped(&node, id).await, (0, None));
    assert_eq!(cached(&node), served(), "the track is kept as it was");
    // Within the backoff nothing is asked.
    worker.recheck_next_stale_register().await;
    assert_eq!(requests(&server).await, 1);
}

#[tokio::test]
async fn a_refusal_is_recorded_and_keeps_the_track() {
    let server = claude("I can't help with reviewing this text.").await;
    let (node, worker, id) = node(&server, Some(&served())).await;
    worker.recheck_next_stale_register().await;
    let track = cached(&node);
    assert_eq!(track.lines, served().lines);
    assert_eq!(track.register.unwrap().verdict, RegisterVerdict::Refused);
    assert_eq!(
        stamped(&node, id).await,
        (i64::from(REGISTER_CHECK_VERSION), Some("refused".into()))
    );
}

#[tokio::test]
async fn a_fresh_track_is_checked_before_it_is_persisted() {
    let server = claude("NONE").await;
    let (_node, worker, _id) = node(&server, None).await;
    let mut track = served();
    assert_eq!(
        worker.check_register(&mut track, YT).await,
        CheckOutcome::Checked
    );
    assert_eq!(track.register.unwrap().verdict, RegisterVerdict::Ok);
    // With no AI client nothing is asked.
    let mut bare = worker;
    bare.ai_client = None;
    let mut track = served();
    assert_eq!(
        bare.check_register(&mut track, YT).await,
        CheckOutcome::NotAsked
    );
    assert_eq!(track.register, None);
    assert_eq!(requests(&server).await, 1);
}

#[test]
fn a_track_is_checked_now_only_under_the_current_version() {
    let mut track = served();
    assert!(!checked_now(&track));
    let check = |version| RegisterCheck {
        version,
        verdict: RegisterVerdict::Ok,
        fixed: vec![],
        flagged: vec![],
    };
    track.register = Some(check(REGISTER_CHECK_VERSION - 1));
    assert!(!checked_now(&track));
    track.register = Some(check(REGISTER_CHECK_VERSION));
    assert!(checked_now(&track));
    track.register = Some(check(REGISTER_CHECK_VERSION + 1));
    assert!(checked_now(&track));
}

/// Another served song on `node` with `track` as its JSON.
async fn add_served(node: &TestNode, yt: &str, track: &LyricsTrack) -> i64 {
    let id = node.add_video(yt).await;
    sqlx::query("UPDATE videos SET normalized = 1, has_lyrics = 1 WHERE id = ?")
        .bind(id)
        .execute(node.pool())
        .await
        .unwrap();
    let json = serde_json::to_vec(track).unwrap();
    std::fs::write(node.cache().join(format!("{yt}_lyrics.json")), json).unwrap();
    id
}

fn peer_checked() -> LyricsTrack {
    let mut track = served();
    track.register = Some(RegisterCheck {
        version: REGISTER_CHECK_VERSION,
        verdict: RegisterVerdict::Fixed,
        fixed: vec![],
        flagged: vec![],
    });
    track
}

#[tokio::test]
async fn with_paid_ai_off_only_a_peers_check_is_stamped_and_nothing_is_asked() {
    let server = claude("NONE").await;
    // The lowest row has no check: it must not hold up the rows behind it.
    let (node, worker, unchecked) = node(&server, Some(&served())).await;
    let from_peer = add_served(&node, "reg_check_2", &peer_checked()).await;
    set(node.pool(), "paid_ai_enabled", "false").await;
    worker.recheck_next_stale_register().await;
    assert_eq!(requests(&server).await, 0);
    assert_eq!(stamped(&node, unchecked).await, (0, None));
    assert_eq!(
        stamped(&node, from_peer).await,
        (i64::from(REGISTER_CHECK_VERSION), Some("fixed".into()))
    );
    // Within the scan interval the next tick reads nothing: a check that
    // arrives meanwhile is stamped at the next scan.
    let late = add_served(&node, "reg_check_3", &peer_checked()).await;
    worker.recheck_next_stale_register().await;
    assert_eq!(stamped(&node, late).await, (0, None));
    worker.retry_backoff.lock().await.register_scan_at = None;
    worker.recheck_next_stale_register().await;
    assert_eq!(
        stamped(&node, late).await,
        (i64::from(REGISTER_CHECK_VERSION), Some("fixed".into()))
    );
    assert_eq!(requests(&server).await, 0);
}

#[tokio::test]
async fn a_node_with_no_ai_client_stamps_its_peers_checks() {
    let server = claude("NONE").await;
    let (node, mut worker, id) = node(&server, Some(&peer_checked())).await;
    worker.ai_client = None;
    worker.recheck_next_stale_register().await;
    assert_eq!(
        stamped(&node, id).await,
        (i64::from(REGISTER_CHECK_VERSION), Some("fixed".into()))
    );
}

#[tokio::test]
async fn after_a_refusal_the_next_song_waits_out_the_backoff() {
    let server = claude("I can't help with reviewing this text.").await;
    let (node, worker, _id) = node(&server, Some(&served())).await;
    let next = add_served(&node, "reg_check_2", &served()).await;
    worker.recheck_next_stale_register().await;
    assert_eq!(requests(&server).await, 1);
    worker.recheck_next_stale_register().await;
    assert_eq!(
        requests(&server).await,
        1,
        "a refusal backs off like a failure"
    );
    assert_eq!(stamped(&node, next).await, (0, None));
}

#[tokio::test]
async fn a_song_whose_answers_cannot_be_read_is_left_unchecked_after_three_tries() {
    let server = claude("Here is my review of the text.").await;
    let (node, worker, id) = node(&server, Some(&served())).await;
    for attempt in 1..=MAX_FAILED_CHECKS {
        assert_eq!(stamped(&node, id).await, (0, None), "before try {attempt}");
        worker.retry_backoff.lock().await.silent_until = None;
        worker.recheck_next_stale_register().await;
    }
    assert_eq!(requests(&server).await, MAX_FAILED_CHECKS as usize);
    assert_eq!(
        stamped(&node, id).await,
        (i64::from(REGISTER_CHECK_VERSION), None),
        "stamped forward with no verdict: the pass moves on"
    );
    assert!(
        worker
            .retry_backoff
            .lock()
            .await
            .register_failures
            .is_empty()
    );
    assert_eq!(cached(&node), served());
}

#[test]
fn a_song_is_given_up_from_its_third_failed_check() {
    assert_eq!(MAX_FAILED_CHECKS, 3);
    assert!(!gives_up(2));
    assert!(gives_up(3));
    assert!(gives_up(4));
}

#[test]
fn a_peer_check_scan_is_due_first_and_then_every_ten_minutes() {
    let t0 = std::time::Instant::now();
    assert_eq!(PEER_SCAN_EVERY, std::time::Duration::from_secs(600));
    assert!(scan_due(None, t0));
    assert!(!scan_due(Some(t0), t0));
    assert!(!scan_due(
        Some(t0),
        t0 + PEER_SCAN_EVERY - std::time::Duration::from_millis(1)
    ));
    assert!(scan_due(Some(t0), t0 + PEER_SCAN_EVERY));
    assert!(scan_due(
        Some(t0),
        t0 + PEER_SCAN_EVERY + std::time::Duration::from_secs(1)
    ));
    // A clock read before the last scan is no scan due.
    assert!(!scan_due(Some(t0 + PEER_SCAN_EVERY), t0));
}

/// The fresh path (`process_song` needs the whole pipeline): the check runs
/// after the translation and before the persist, the stamp after it, and
/// both translation passes run the stale pass.
#[test]
fn the_worker_checks_a_fresh_track_between_its_translation_and_its_persist() {
    let src = include_str!("worker.rs");
    let at = |needle: &str| {
        src.find(needle)
            .unwrap_or_else(|| panic!("worker.rs no longer has `{needle}`"))
    };
    let translate = at("self.translate_track(&mut track, &youtube_id, gender).await;");
    let check = at("self.check_register(&mut track, &youtube_id).await;");
    let persist = at("persist_lyrics_track(");
    let stamp = at("self.stamp_register_of(video_id, &track).await;");
    assert!(translate < check, "checked after its translation");
    assert!(check < persist, "checked before it is persisted");
    assert!(persist < stamp, "stamped once persisted");
    assert_eq!(
        src.matches("self.recheck_next_stale_register().await;")
            .count(),
        2,
        "the wall-wait tick and the empty-queue tick both run the stale pass"
    );
}
