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
    let seen: PushedLine = l.clone();
    assert_eq!(payload_for(Some(&seen), &l, "Song", ""), None);
}

#[test]
fn a_push_is_made_when_only_the_translation_changed() {
    // The Slovak line arrived later under the same English line.
    let seen = lines("Amazing grace", "How sweet", "", "Ako sladko");
    let l = lines("Amazing grace", "How sweet", "Úžasná milosť", "Ako sladko");
    let payload = payload_for(Some(&seen), &l, "Song", "").expect("the SK changed");
    assert_eq!(payload.current_translation, "Úžasná milosť");
    // And when only the English changed.
    let seen = lines("Amazing", "How sweet", "Úžasná milosť", "Ako sladko");
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
    assert_eq!(seen, Some(lines("Amazing grace", "How sweet", "", "")));
    let bodies = wait_for_pushes(&mock, 1).await;
    assert_eq!(bodies[0]["currentText"], "Amazing grace");
    assert_eq!(bodies[0]["currentTranslation"], "");
    // The same line again: no push, the key is kept.
    let seen = push(seen, lines("Amazing grace", "How sweet", "", ""));
    assert_eq!(seen, Some(lines("Amazing grace", "How sweet", "", "")));
    // The Slovak arrives for the same English line: pushed.
    let seen = push(
        seen,
        lines("Amazing grace", "How sweet", "Úžasná milosť", "Ako sladko"),
    );
    assert_eq!(
        seen,
        Some(lines(
            "Amazing grace",
            "How sweet",
            "Úžasná milosť",
            "Ako sladko"
        ))
    );
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
    let seen = Some(lines("a", "", "b", ""));
    let l = lines("c", "", "d", "");
    assert_eq!(maybe_push_line(None, seen.clone(), l, "Song", ""), seen);
}

// ── #222 release review: a sentence sung several times in a row ────────────

const WAG_EN: &str = "What a God, what a God";
const WAG_SK: &str = "Aký Boh, aký Boh";
const NEXT_EN: &str = "You're nothing like I thought";
const NEXT_SK: &str = "Nie si taký, ako som si myslel";

/// "What A God" sings "What a God, what a God." four times in a row: four
/// identical plan lines. Only the 4th has a different next line, the one the
/// singers sing after the repeats.
fn four_repeats_then_the_next_line() -> [PresenterLines; 5] {
    [
        lines(WAG_EN, WAG_EN, WAG_SK, WAG_SK),
        lines(WAG_EN, WAG_EN, WAG_SK, WAG_SK),
        lines(WAG_EN, WAG_EN, WAG_SK, WAG_SK),
        lines(WAG_EN, NEXT_EN, WAG_SK, NEXT_SK),
        lines(NEXT_EN, "", NEXT_SK, ""),
    ]
}

/// The dedup key is the whole payload: the 4th repeat goes out again because
/// its next line changed, so the stage display never shows a stale "next".
#[test]
fn a_repeated_line_is_pushed_again_when_its_next_line_changes() {
    let mut seen: Option<PushedLine> = None;
    let mut pushes = Vec::new();
    for (i, l) in four_repeats_then_the_next_line().iter().enumerate() {
        if let Some(p) = payload_for(seen.as_ref(), l, "What A God", "") {
            pushes.push((i + 1, p.current_text, p.next_text, p.next_translation));
            seen = Some(l.clone());
        }
    }
    let expected: Vec<(usize, String, String, String)> = vec![
        (
            1,
            WAG_EN.to_string(),
            WAG_EN.to_string(),
            WAG_SK.to_string(),
        ),
        (
            4,
            WAG_EN.to_string(),
            NEXT_EN.to_string(),
            NEXT_SK.to_string(),
        ),
        (5, NEXT_EN.to_string(), String::new(), String::new()),
    ];
    assert_eq!(
        pushes, expected,
        "(line, currentText, nextText, nextTranslation) of every push"
    );
}

/// The same through the real push: the Presenter receives the 4th repeat with
/// the new `nextText`, and nothing for the 2nd and 3rd (identical payloads).
#[tokio::test]
async fn the_stage_display_gets_the_next_line_after_the_repeats() {
    let mock = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/stage"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&mock)
        .await;
    let client = Arc::new(PresenterClient::new(format!("{}/api/stage", mock.uri())));

    // A push is a spawned task (spawn order is not arrival order), so wait
    // for each expected push before the next line: lines 1, 4 and 5 push.
    let mut seen = None;
    for (i, l) in four_repeats_then_the_next_line().into_iter().enumerate() {
        seen = maybe_push_line(Some(&client), seen, l, "What A God", "");
        let pushed_so_far = match i {
            0..=2 => 1,
            3 => 2,
            _ => 3,
        };
        wait_for_pushes(&mock, pushed_so_far).await;
    }
    let bodies = received(&mock).await;
    let shown: Vec<(Value, Value)> = bodies
        .iter()
        .map(|b| (b["currentText"].clone(), b["nextText"].clone()))
        .collect();
    assert_eq!(
        shown,
        vec![
            (json!(WAG_EN), json!(WAG_EN)),
            (json!(WAG_EN), json!(NEXT_EN)),
            (json!(NEXT_EN), json!("")),
        ]
    );
    // Exactly three: the 2nd and 3rd repeats were never pushed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(received(&mock).await.len(), 3);
}
