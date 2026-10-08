//! #229 item C: the lyrics worker calls no paid AI while this node's switch
//! is off — not for a song (held by the exchange, `peer::lyrics`), not for
//! a translation (`translation_allowed`). Claude is a wiremock that counts.

use std::sync::Arc;

use sp_core::lyrics::{LyricsLine, LyricsTrack};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::ai::AiSettings;
use crate::ai::client::AiClient;
use crate::lyrics::translator::SpeakerGender;
use crate::peer::rig::{TestNode, set};
use tokio::sync::broadcast;

const YT: &str = "trn_paid_01";

/// A track with one English line and no SK line.
fn untranslated() -> LyricsTrack {
    LyricsTrack {
        version: 1,
        source: "mtl+g35t".into(),
        language_source: "en".into(),
        language_translation: String::new(),
        lines: vec![LyricsLine {
            start_ms: 0,
            end_ms: 1000,
            en: "Way maker".into(),
            sk: None,
            words: None,
        }],
    }
}

/// A Claude that answers every translation with one SK line.
async fn claude() -> MockServer {
    let server = MockServer::start().await;
    let body = serde_json::json!({"choices": [{"message": {"content": "1: Cestu robíš"}}]});
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    server
}

/// PP alone (no peer, the exchange wired) with its Claude on `server`, one
/// active song with lyrics but no SK line (its JSON in the cache), and one
/// song still to be processed.
async fn pp_with_claude(server: &MockServer) -> (TestNode, LyricsWorker, i64) {
    let pp = TestNode::start("pp", None).await;
    let done = pp.add_video(YT).await;
    // Done at the current pipeline version: the lyrics queue never takes it.
    sqlx::query(
        "UPDATE videos SET normalized = 1, has_lyrics = 1, lyrics_source = 'mtl+g35t', \
         lyrics_pipeline_version = ? WHERE id = ?",
    )
    .bind(i64::from(crate::lyrics::LYRICS_PIPELINE_VERSION))
    .bind(done)
    .execute(pp.pool())
    .await
    .unwrap();
    let json = serde_json::to_vec(&untranslated()).unwrap();
    std::fs::write(pp.cache().join(format!("{YT}_lyrics.json")), json).unwrap();
    let todo = pp.add_video("trn_paid_02").await;
    pp.give_song(todo, "trn_paid_02", "Way Maker", "Sinach")
        .await;
    let (events, _) = broadcast::channel(16);
    let mut worker =
        LyricsWorker::new_for_test(pp.pool().clone(), pp.cache().to_path_buf(), events)
            .with_peer(pp.ex.clone());
    worker.ai_client = Some(Arc::new(AiClient::new(AiSettings {
        api_url: format!("{}/v1", server.uri()),
        api_key: None,
        model: "claude-test".into(),
        system_prompt_extra: None,
    })));
    (pp, worker, todo)
}

/// The SK line the cached track of [`YT`] holds now.
fn cached_sk(pp: &TestNode) -> Option<String> {
    let json = std::fs::read(pp.cache().join(format!("{YT}_lyrics.json"))).unwrap();
    let track: LyricsTrack = serde_json::from_slice(&json).unwrap();
    track.lines[0].sk.clone()
}

/// Off: the song is held and nothing is translated — Claude hears nothing.
#[tokio::test]
async fn no_paid_ai_call_while_the_switch_is_off() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let server = claude().await;
    let (pp, worker, todo) = pp_with_claude(&server).await;
    set(pp.pool(), "paid_ai_enabled", "false").await;
    worker.process_next().await;
    worker.retry_missing_translations().await;
    worker.retranslate_next_stale().await;
    let mut track = untranslated();
    worker
        .translate_track(&mut track, YT, SpeakerGender::Male)
        .await;
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
    assert_eq!(track.lines[0].sk, None);
    assert_eq!(cached_sk(&pp), None);
    let (has, attempts, next): (i64, i64, Option<String>) = sqlx::query_as(
        "SELECT has_lyrics, lyrics_attempts, lyrics_next_attempt_at FROM videos WHERE id = ?",
    )
    .bind(todo)
    .fetch_one(pp.pool())
    .await
    .unwrap();
    assert_eq!((has, attempts), (0, 0), "held: nothing ran, no attempt");
    assert!(next.is_some(), "picked again later");
}

/// On: the same pass translates the song through Claude, once.
#[tokio::test]
async fn the_translation_pass_calls_claude_while_the_switch_is_on() {
    let server = claude().await;
    let (pp, worker, _todo) = pp_with_claude(&server).await;
    worker.retry_missing_translations().await;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(cached_sk(&pp).as_deref(), Some("Cestu robíš"));
}

/// The log lines a scoped subscriber wrote (this test's thread only).
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The captured lines `paid_ai::hold` wrote (INFO or DEBUG).
fn holds(cap: &Captured) -> Vec<String> {
    String::from_utf8(cap.0.lock().unwrap().clone())
        .unwrap()
        .lines()
        .filter(|l| l.contains("paid AI is off"))
        .map(str::to_string)
        .collect()
}

/// Review round 12: a translation pass holds only a song it would
/// translate — with nothing to translate nothing is held, so the status
/// names no translation that is not waiting.
#[tokio::test]
async fn a_translation_pass_holds_only_a_song_it_would_translate() {
    let cap = Captured::default();
    let writer = cap.clone();
    let _log = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish(),
    );
    let server = claude().await;
    let empty = TestNode::start("pp", None).await;
    set(empty.pool(), "paid_ai_enabled", "false").await;
    let (events, _) = broadcast::channel(16);
    let mut idle =
        LyricsWorker::new_for_test(empty.pool().clone(), empty.cache().to_path_buf(), events);
    idle.ai_client = Some(Arc::new(AiClient::new(AiSettings {
        api_url: format!("{}/v1", server.uri()),
        api_key: None,
        model: "claude-test".into(),
        system_prompt_extra: None,
    })));
    idle.retry_missing_translations().await;
    idle.retranslate_next_stale().await;
    assert_eq!(holds(&cap), Vec::<String>::new(), "nothing to translate");
    let (pp, worker, _todo) = pp_with_claude(&server).await;
    set(pp.pool(), "paid_ai_enabled", "false").await;
    worker.retry_missing_translations().await;
    let held = holds(&cap);
    assert!(
        held.iter()
            .any(|l| l.contains("job=\"translation\"") && l.contains(YT)),
        "{held:?}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}
