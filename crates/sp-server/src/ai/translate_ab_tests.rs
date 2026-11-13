//! #321: the blind translation A/B — its checks, its lettering, and one
//! whole run against a mock proxy and a mock OpenRouter.

use sp_core::lyrics::{LyricsLine, LyricsTrack};
use sqlx::SqlitePool;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    AbRequest, Candidate, RunGuard, Song, begin, blind_sheet, checked, default_candidates,
    key_json, letter, random_seed, run, shuffled, song_orders, status, valid_youtube_id,
};
use crate::ai::AiSettings;
use crate::ai::client::AiClient;
use crate::ai::openrouter::OpenRouterClient;
use crate::ai::retry::RetryPolicy;
use crate::db::models::set_setting;
use crate::lyrics::translator::SpeakerGender;

const YT: &str = "abcdefghijk";

fn cand(provider: &str, model: &str) -> Candidate {
    Candidate {
        provider: provider.into(),
        model: model.into(),
    }
}

fn track(lines: &[&str]) -> LyricsTrack {
    LyricsTrack {
        version: 1,
        source: "test".into(),
        language_source: "en".into(),
        language_translation: String::new(),
        lines: lines
            .iter()
            .enumerate()
            .map(|(i, en)| LyricsLine {
                start_ms: i as u64 * 1000,
                end_ms: i as u64 * 1000 + 900,
                en: (*en).to_string(),
                sk: None,
                words: None,
            })
            .collect(),
        register: None,
    }
}

#[test]
fn a_youtube_id_is_eleven_url_safe_characters() {
    assert!(valid_youtube_id("abc-DEF_123"));
    assert!(!valid_youtube_id("abc-DEF_12"));
    assert!(!valid_youtube_id("abc-DEF_1234"));
    assert!(!valid_youtube_id("abc/DEF_123"));
}

#[test]
fn the_default_candidates_are_claude_carbon_and_hydrogen() {
    assert_eq!(
        default_candidates("claude-opus-5-5"),
        vec![
            cand("claude_proxy", "claude-opus-5-5"),
            cand("openrouter", "google/gemini-4-carbon"),
            cand("openrouter", "google/gemini-4-hydrogen"),
        ]
    );
}

#[test]
fn the_request_is_checked() {
    let req = |ids: &[&str], c: Option<Vec<Candidate>>| AbRequest {
        youtube_ids: ids.iter().map(|s| s.to_string()).collect(),
        candidates: c,
    };
    let (ids, c) = checked(req(&[YT, " abcdefghijk ", "zzzzzzzzzzz"], None), "m").unwrap();
    assert_eq!(ids, vec![YT.to_string(), "zzzzzzzzzzz".to_string()]);
    assert_eq!(c.len(), 3);
    assert!(checked(req(&[], None), "m").is_err());
    assert!(checked(req(&["bad"], None), "m").is_err());
    let too_many: Vec<String> = (0..41).map(|i| format!("{i:011}")).collect();
    let refs: Vec<&str> = too_many.iter().map(String::as_str).collect();
    assert!(checked(req(&refs, None), "m").is_err());
    let forty: Vec<&str> = refs[..40].to_vec();
    assert_eq!(checked(req(&forty, None), "m").unwrap().0.len(), 40);
    assert!(checked(req(&[YT], Some(vec![cand("openrouter", "x")])), "m").is_err());
    let six = vec![cand("openrouter", "x"); 6];
    assert!(checked(req(&[YT], Some(six.clone())), "m").is_ok());
    let mut seven = six;
    seven.push(cand("openrouter", "x"));
    assert!(checked(req(&[YT], Some(seven)), "m").is_err());
    let unknown = vec![cand("openrouter", "x"), cand("gemini", "y")];
    assert!(checked(req(&[YT], Some(unknown)), "m").is_err());
    let blank_provider = vec![cand("openrouter", "x"), cand(" ", "y")];
    assert!(checked(req(&[YT], Some(blank_provider)), "m").is_err());
    let blank_model = vec![cand("openrouter", "x"), cand("openrouter", " ")];
    assert!(checked(req(&[YT], Some(blank_model)), "m").is_err());
    // A proxy candidate names the proxy's own model, never another one
    // (the key would lie: the proxy always sends `ai_model`).
    let proxy_other = vec![cand("openrouter", "x"), cand("claude_proxy", "other")];
    assert!(checked(req(&[YT], Some(proxy_other)), "m").is_err());
    let proxy_own = vec![cand("openrouter", "x"), cand("claude_proxy", " m ")];
    assert!(checked(req(&[YT], Some(proxy_own)), "m").is_ok());
}

#[test]
fn the_letters_are_a_permutation_fixed_by_the_seed() {
    for seed in [0_u64, 1, 7, 1_763_000_000_000] {
        let order = shuffled(3, seed);
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2], "seed {seed}");
        assert_eq!(shuffled(3, seed), order, "same seed, same order");
    }
    // Different seeds give different orders (not always the identity).
    let orders: std::collections::HashSet<Vec<usize>> =
        (0..50_u64).map(|s| shuffled(3, s * 7919 + 13)).collect();
    assert!(orders.len() > 3, "{orders:?}");
    assert_eq!(shuffled(1, 5), vec![0]);
    assert_eq!((letter(0), letter(2), letter(5)), ('A', 'C', 'F'));
}

#[test]
fn every_song_has_its_own_order_and_the_seed_is_random() {
    let orders = song_orders(42, 20, 3);
    assert_eq!(orders.len(), 20);
    for order in &orders {
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2]);
    }
    let distinct: std::collections::HashSet<&Vec<usize>> = orders.iter().collect();
    assert!(distinct.len() > 1, "{orders:?}");
    assert_eq!(song_orders(42, 20, 3), orders, "same seed, same orders");
    assert_ne!(song_orders(43, 20, 3), orders);
    assert_ne!(random_seed(), random_seed());
}

#[test]
fn the_sheet_names_no_model_nor_provider_and_the_key_maps_each_letter() {
    let song = |yt: &str| Song {
        youtube_id: yt.into(),
        title: "Song — Artist".into(),
        track: track(&["Line one", "Line two"]),
        gender: SpeakerGender::Male,
    };
    let candidates = vec![
        cand("claude_proxy", "claude-x"),
        cand("openrouter", "google/g"),
    ];
    let results = || {
        vec![
            Ok(vec!["Claude jeden".to_string(), "Claude dva".to_string()]),
            Err("OpenRouter refused (HTTP 500)".to_string()),
        ]
    };
    let songs = vec![(song(YT), results()), (song("zzzzzzzzzzz"), results())];
    let orders = vec![vec![1, 0], vec![0, 1]];
    let sheet = blind_sheet("2026-11-13T00:00:00.000Z", &songs, &orders);
    assert!(
        !sheet.contains("claude-x") && !sheet.contains("google/g"),
        "{sheet}"
    );
    // A failed call's text would name its provider: the cell is neutral.
    assert!(
        !sheet.contains("OpenRouter") && !sheet.contains("HTTP"),
        "{sheet}"
    );
    assert!(sheet.contains("## Song — Artist (abcdefghijk)"), "{sheet}");
    assert!(
        sheet.contains("1. EN: Line one\n   A: (chyba)\n   B: Claude jeden\n"),
        "{sheet}"
    );
    assert!(sheet.contains("2. EN: Line two\n   A: (chyba)\n   B: Claude dva\n"));
    // The second song has its own order.
    assert!(
        sheet.contains(
            "## Song — Artist (zzzzzzzzzzz)\n\n1. EN: Line one\n   A: Claude jeden\n   B: (chyba)\n"
        ),
        "{sheet}"
    );
    assert!(sheet.contains("kandidáti: A, B."), "{sheet}");
    assert_eq!(
        key_json(&candidates, &songs, &orders),
        serde_json::json!({
            "abcdefghijk": {
                "A": {"provider": "openrouter", "model": "google/g", "error": "OpenRouter refused (HTTP 500)"},
                "B": {"provider": "claude_proxy", "model": "claude-x"}
            },
            "zzzzzzzzzzz": {
                "A": {"provider": "claude_proxy", "model": "claude-x"},
                "B": {"provider": "openrouter", "model": "google/g", "error": "OpenRouter refused (HTTP 500)"}
            }
        })
    );
}

#[test]
fn a_run_dropped_while_running_frees_the_slot() {
    // The slot is process-wide: this is the only test that takes it.
    assert!(begin(3));
    assert!(!begin(3), "a running run holds the slot");
    drop(RunGuard);
    let s = status();
    assert_eq!(s.state, "failed");
    assert!(s.error.unwrap().contains("aborted"));
    assert!(begin(1), "the slot is free again");
    super::update(|s| s.state = "done".into());
    drop(RunGuard);
    assert_eq!(status().state, "done", "a finished run is left as it is");
}

async fn pool_with_song(gender: Option<&str>) -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&pool)
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, song, artist, normalized, has_lyrics) \
         VALUES (1, ?, 't', 'Way Maker', 'Sinach', 1, 1) RETURNING id",
    )
    .bind(YT)
    .fetch_one(&pool)
    .await
    .unwrap();
    crate::db::models::set_translation_gender(&pool, id, gender)
        .await
        .unwrap();
    pool
}

/// A whole run: the production prompt (with the song's gender) through the
/// proxy and through OpenRouter, the sheet blind, the key apart.
#[tokio::test]
async fn a_run_writes_the_blind_sheet_and_its_key() {
    let pool = pool_with_song(Some("f")).await;
    set_setting(
        &pool,
        "openrouter_api_key",
        "example-openrouter-key-for-tests",
    )
    .await
    .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let json = serde_json::to_string(&track(&["I was lost", "Now found"])).unwrap();
    std::fs::write(cache.path().join(format!("{YT}_lyrics.json")), json).unwrap();

    let proxy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "1: Bola som stratená\n2: Teraz nájdená"}}]
        })))
        .expect(1)
        .mount(&proxy)
        .await;
    let openrouter = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/chat/completions"))
        .and(body_partial_json(
            serde_json::json!({"model": "google/gemini-4-carbon", "max_tokens": 8000}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "1: Stratená\n2: Nájdená"}}]
        })))
        .expect(1)
        .mount(&openrouter)
        .await;
    let client = AiClient::new(AiSettings {
        api_url: format!("{}/v1", proxy.uri()),
        api_key: None,
        model: "claude-test".into(),
        system_prompt_extra: None,
    })
    .with_retry_policy(RetryPolicy::NO_WAIT)
    .with_routing_to(
        pool.clone(),
        OpenRouterClient::new(format!("{}/api/v1", openrouter.uri()))
            .with_retry_policy(RetryPolicy::NO_WAIT),
    );
    let candidates = vec![
        cand("claude_proxy", "claude-test"),
        cand("openrouter", "google/gemini-4-carbon"),
    ];
    let done = std::sync::Mutex::new(Vec::new());
    let progress = |n: usize| done.lock().unwrap().push(n);
    let dir = run(
        &client,
        &pool,
        cache.path(),
        &[YT.to_string()],
        &candidates,
        42,
        &progress,
    )
    .await
    .unwrap();
    assert_eq!(*done.lock().unwrap(), vec![1]);
    assert!(dir.starts_with(cache.path().join("ab")));

    // Both prompts are the translator's, with the feminine directive.
    for server in [&proxy, &openrouter] {
        let req = &server.received_requests().await.unwrap()[0];
        let body = String::from_utf8(req.body.clone()).unwrap();
        assert!(body.contains("use feminine forms"), "{body}");
        assert!(body.contains("1: I was lost\\n2: Now found"), "{body}");
    }

    let sheet = std::fs::read_to_string(dir.join("blind.md")).unwrap();
    let key: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("key.json")).unwrap()).unwrap();
    assert!(
        sheet.contains("## Way Maker — Sinach (abcdefghijk)"),
        "{sheet}"
    );
    assert!(
        !sheet.contains("claude-test") && !sheet.contains("gemini"),
        "{sheet}"
    );
    let order = &song_orders(42, 1, 2)[0];
    let claude_letter = letter(order.iter().position(|&c| c == 0).unwrap());
    let carbon_letter = letter(order.iter().position(|&c| c == 1).unwrap());
    assert!(
        sheet.contains(&format!("   {claude_letter}: Bola som stratená\n")),
        "{sheet}"
    );
    assert!(
        sheet.contains(&format!("   {carbon_letter}: Stratená\n")),
        "{sheet}"
    );
    assert_eq!(key[YT][claude_letter.to_string()]["model"], "claude-test");
    assert_eq!(
        key[YT][carbon_letter.to_string()]["model"],
        "google/gemini-4-carbon"
    );
}

/// Every song is read before the first paid call: a run with a song that
/// cannot run sends nothing and names EVERY such id (no lyrics file, no row,
/// no line), and writes no sheet.
#[tokio::test]
async fn a_song_that_cannot_run_fails_the_run_before_any_call() {
    let pool = pool_with_song(None).await;
    let cache = tempfile::tempdir().unwrap();
    // A second id with lyrics but no row, a third whose lyrics have no line.
    let empty = serde_json::to_string(&track(&[])).unwrap();
    std::fs::write(cache.path().join("zzzzzzzzzzz_lyrics.json"), &empty).unwrap();
    std::fs::write(cache.path().join(format!("{YT}_lyrics.json")), &empty).unwrap();
    let proxy = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&proxy)
        .await;
    let client = AiClient::new(AiSettings {
        api_url: format!("{}/v1", proxy.uri()),
        api_key: None,
        model: "m".into(),
        system_prompt_extra: None,
    });
    let ids = [
        "yyyyyyyyyyy".to_string(),
        "zzzzzzzzzzz".to_string(),
        YT.to_string(),
    ];
    let err = run(
        &client,
        &pool,
        cache.path(),
        &ids,
        &default_candidates("m"),
        1,
        &|_| {},
    )
    .await
    .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("no call was sent"), "{text}");
    assert!(text.contains("yyyyyyyyyyy: no lyrics file"), "{text}");
    assert!(text.contains("zzzzzzzzzzz: no video row"), "{text}");
    assert!(
        text.contains("abcdefghijk: the lyrics have no line"),
        "{text}"
    );
    assert!(!cache.path().join("ab").exists());
}
