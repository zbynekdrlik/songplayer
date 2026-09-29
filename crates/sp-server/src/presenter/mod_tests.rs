//! #222 `presenter/mod.rs`: the push decision (`payload_for`, pure) and the
//! push itself (`maybe_push_line`) against a wiremock Presenter. Both
//! languages always go out; a line is pushed again when only its Slovak
//! changed.
//! Wired via `#[cfg(test)] #[path = "mod_tests.rs"] mod tests;`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::lyrics::renderer::PresenterLines;

fn lines(current_en: &str, next_en: &str, current_sk: &str, next_sk: &str) -> PresenterLines {
    PresenterLines {
        current_en: current_en.to_string(),
        next_en: next_en.to_string(),
        current_sk: current_sk.to_string(),
        next_sk: next_sk.to_string(),
    }
}

fn pushed(en: &str, sk: &str) -> PushedLine {
    (en.to_string(), sk.to_string())
}

#[test]
fn a_line_is_pushed_in_both_languages() {
    let l = lines(
        "Amazing grace",
        "How sweet the sound",
        "Úžasná milosť",
        "Ako sladko znie",
    );
    let payload = payload_for(None, &l, "Amazing Grace", "Chris Tomlin").expect("a new line");
    assert_eq!(
        payload,
        PresenterPayload {
            current_text: "Amazing grace".into(),
            next_text: "How sweet the sound".into(),
            current_song: "Amazing Grace - Chris Tomlin".into(),
            next_song: String::new(),
            current_translation: "Úžasná milosť".into(),
            next_translation: "Ako sladko znie".into(),
        }
    );
    // No artist: the song line is the song alone.
    let payload = payload_for(None, &l, "Amazing Grace", "").expect("a new line");
    assert_eq!(payload.current_song, "Amazing Grace");
}

#[test]
fn a_line_without_a_translation_sends_empty_translations() {
    let l = lines("Amazing grace", "", "", "");
    let payload = payload_for(None, &l, "Song", "").expect("a new line");
    assert_eq!(
        (
            payload.current_translation.as_str(),
            payload.next_translation.as_str()
        ),
        ("", "")
    );
    let json = serde_json::to_value(&payload).unwrap();
    assert_eq!(json["currentTranslation"], "");
    assert_eq!(json["nextTranslation"], "");
}

#[test]
fn every_lyric_field_is_wrapped_for_the_stage_display() {
    // 66 and 65 characters: over the wrap width (one wall line, 52 chars),
    // as a single long source line can be.
    let en = "When I survey the wondrous cross on which the Prince of glory died";
    let sk = "Keď hľadím na ten podivuhodný kríž, na ktorom zomrel Knieža slávy";
    let payload = payload_for(None, &lines(en, en, sk, sk), "Song", "").expect("a new line");
    for text in [
        &payload.current_text,
        &payload.next_text,
        &payload.current_translation,
        &payload.next_translation,
    ] {
        assert!(text.contains('\n'), "not wrapped: {text:?}");
    }
    assert_eq!(payload.current_translation, payload::wrap_for_presenter(sk));
    assert_eq!(payload.next_translation, payload::wrap_for_presenter(sk));
}

#[test]
fn a_line_already_pushed_in_both_languages_is_not_pushed_again() {
    let l = lines("Amazing grace", "How sweet", "Úžasná milosť", "Ako sladko");
    let seen = pushed("Amazing grace", "Úžasná milosť");
    assert_eq!(payload_for(Some(&seen), &l, "Song", ""), None);
}

#[test]
fn a_push_is_made_when_only_the_translation_changed() {
    // The Slovak line arrived later under the same English line.
    let seen = pushed("Amazing grace", "");
    let l = lines("Amazing grace", "How sweet", "Úžasná milosť", "Ako sladko");
    let payload = payload_for(Some(&seen), &l, "Song", "").expect("the SK changed");
    assert_eq!(payload.current_translation, "Úžasná milosť");
    // And when only the English changed.
    let seen = pushed("Amazing", "Úžasná milosť");
    assert!(payload_for(Some(&seen), &l, "Song", "").is_some());
}

/// The bodies Presenter received so far.
async fn received(mock: &MockServer) -> Vec<Value> {
    mock.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).expect("a JSON body"))
        .collect()
}

/// Wait (bounded) until Presenter received `n` pushes.
async fn wait_for_pushes(mock: &MockServer, n: usize) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let bodies = received(mock).await;
        if bodies.len() >= n {
            return bodies;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Presenter got {} of {n} pushes: {bodies:?}",
            bodies.len()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn maybe_push_line_pushes_a_translation_that_changed_under_the_same_line() {
    let mock = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/stage"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&mock)
        .await;
    let client = Arc::new(PresenterClient::new(format!("{}/api/stage", mock.uri())));
    let push = |seen, l| maybe_push_line(Some(&client), seen, l, "Song", "Artist");

    // The English line first, with no translation yet.
    let seen = push(None, lines("Amazing grace", "How sweet", "", ""));
    assert_eq!(seen, Some(pushed("Amazing grace", "")));
    let bodies = wait_for_pushes(&mock, 1).await;
    assert_eq!(bodies[0]["currentText"], "Amazing grace");
    assert_eq!(bodies[0]["currentTranslation"], "");
    // The same line again: no push, the key is kept.
    let seen = push(seen, lines("Amazing grace", "How sweet", "", ""));
    assert_eq!(seen, Some(pushed("Amazing grace", "")));
    // The Slovak arrives for the same English line: pushed.
    let seen = push(
        seen,
        lines("Amazing grace", "How sweet", "Úžasná milosť", "Ako sladko"),
    );
    assert_eq!(seen, Some(pushed("Amazing grace", "Úžasná milosť")));
    let bodies = wait_for_pushes(&mock, 2).await;
    assert_eq!(
        bodies[1],
        json!({
            "currentText": "Amazing grace",
            "nextText": "How sweet",
            "currentSong": "Song - Artist",
            "nextSong": "",
            "currentTranslation": "Úžasná milosť",
            "nextTranslation": "Ako sladko",
        })
    );
    // Exactly two: the repeated line was never pushed (the pure dedup is
    // pinned above; this "not yet" window only errs in the safe direction).
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(received(&mock).await.len(), 2);
}

#[tokio::test]
async fn without_a_client_nothing_is_pushed_and_the_key_is_kept() {
    let seen = Some(pushed("a", "b"));
    let l = lines("c", "", "d", "");
    assert_eq!(maybe_push_line(None, seen.clone(), l, "Song", ""), seen);
}
