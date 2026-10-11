//! #250: the lyrics worker's register check — the stale pass over a served
//! track, through a wiremock Claude that counts.

use std::sync::Arc;

use sp_core::lyrics::{LyricsLine, LyricsTrack, RegisterCheck, RegisterVerdict};
use tokio::sync::broadcast;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{CheckOutcome, checked_now};
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
