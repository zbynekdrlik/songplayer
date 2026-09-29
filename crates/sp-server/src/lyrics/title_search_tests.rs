//! Tests for `title_search.rs` (#144). Exact scores derived with a scratch
//! model of the multiset Dice score.

use super::*;
use crate::ai::AiSettings;
use crate::ai::client::AiClient;
use crate::db::models::VideoLyricsRow;
use crate::lyrics::g35t_client::AsrWord;
use crate::lyrics::tier1::CandidateText as TierCandidate;
use sp_core::lyrics::{LyricsLine, LyricsTrack};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn lyrics(lines: &[&str]) -> LyricsTrack {
    LyricsTrack {
        version: 1,
        source: "t".into(),
        language_source: "en".into(),
        language_translation: String::new(),
        lines: lines
            .iter()
            .map(|l| LyricsLine {
                start_ms: 0,
                end_ms: 0,
                en: l.to_string(),
                sk: None,
                words: None,
            })
            .collect(),
    }
}

fn sung(text: &str) -> Vec<AsrWord> {
    text.split_whitespace()
        .enumerate()
        .map(|(i, w)| AsrWord {
            text: w.to_string(),
            start_ms: i as u64 * 400,
            end_ms: i as u64 * 400 + 300,
        })
        .collect()
}

const SUNG: &str = "You never fail You never will Jesus be the name above every other name";

fn cand(source: &str) -> TierCandidate {
    TierCandidate {
        source: source.into(),
        lines: vec!["a".into()],
        line_timings: None,
        has_timing: false,
    }
}

// ---- needs_title_search ----

#[test]
fn the_title_search_runs_only_when_no_lookup_found_the_song() {
    assert!(needs_title_search(&[]));
    assert!(needs_title_search(&[cand("description"), cand("yt_subs")]));
    assert!(!needs_title_search(&[cand("description"), cand("lrclib")]));
    assert!(!needs_title_search(&[cand("genius")]));
    assert!(!needs_title_search(&[cand("override")]));
    assert!(!needs_title_search(&[cand("tier1:spotify")]));
}

// ---- overlap_score ----

/// All 14 lyric words are sung, every sung word is in the lyric: 1.0.
#[test]
fn the_sung_lyric_scores_one() {
    let l = lyrics(&[
        "You never fail",
        "You never will",
        "Jesus be the name",
        "Above every other name",
    ]);
    assert_eq!(overlap_score(&l, &sung(SUNG)), 1.0);
}

/// 6 lyric words, 7 sung, 6 shared: 2·6 / 13.
#[test]
fn the_score_is_twice_the_shared_words_over_both_totals() {
    let l = lyrics(&["You never fail,", "You never will!"]);
    let s = sung("you never fail you never will jesus");
    assert_eq!(overlap_score(&l, &s), 12.0 / 13.0);
}

/// A word counts only as often as both sides have it: "oh" ×3 in the lyric,
/// ×1 sung → 1 shared, 2·1 / (3 + 1).
#[test]
fn a_repeated_word_counts_as_often_as_both_sides_have_it() {
    let l = lyrics(&["oh oh oh"]);
    assert_eq!(overlap_score(&l, &sung("oh")), 0.5);
}

/// The same title by another song shares only common words: 5 of 17 lyric
/// words (jesus, be, the, name, other) against the 14 sung → 10 / 31.
#[test]
fn a_same_title_different_song_scores_low() {
    let l = lyrics(&[
        "Jesus be the name of my joy",
        "Something totally different words here",
        "Another line of other words",
    ]);
    assert_eq!(overlap_score(&l, &sung(SUNG)), 10.0 / 31.0);
}

/// No shared word, or nothing sung: 0.0 (never a NaN). A punctuation-only
/// sung token is not a word.
#[test]
fn no_shared_word_scores_zero() {
    assert_eq!(
        overlap_score(&lyrics(&["God is not dead"]), &sung(SUNG)),
        0.0
    );
    assert_eq!(overlap_score(&lyrics(&["amen"]), &[]), 0.0);
    assert_eq!(overlap_score(&lyrics(&["amen"]), &sung("amen —")), 1.0);
}

// ---- rank_above_floor ----

#[test]
fn the_scores_above_the_floor_are_ranked_best_first() {
    assert_eq!(rank_above_floor(&[0.3, 0.7, 0.6]), vec![1, 2]);
}

#[test]
fn nothing_below_the_floor_is_ranked() {
    assert_eq!(rank_above_floor(&[0.49, 0.2]), Vec::<usize>::new());
    assert_eq!(rank_above_floor(&[]), Vec::<usize>::new());
}

#[test]
fn a_score_exactly_at_the_floor_is_ranked() {
    assert_eq!(rank_above_floor(&[MIN_TITLE_MATCH_SCORE]), vec![0]);
}

#[test]
fn a_tie_keeps_the_earlier_candidate_first() {
    assert_eq!(rank_above_floor(&[0.2, 0.6, 0.9, 0.6]), vec![2, 1, 3]);
}

/// The floor separates the measured distribution (#144): every song's own
/// lyric reaches it, no other song does.
#[test]
fn the_floor_separates_the_measured_scores() {
    for own in [0.664, 0.701, 0.951, 0.657] {
        assert_eq!(rank_above_floor(&[own]), vec![0], "own lyric {own}");
    }
    for other in [0.379, 0.366, 0.317, 0.166] {
        assert!(rank_above_floor(&[other]).is_empty(), "other song {other}");
    }
}

// ---- keeps_the_videos_text ----

/// A partial description of the video scores under the found lyric, or the
/// video has no text of its own: the lyric becomes the reference text.
#[test]
fn a_better_matching_title_lyric_becomes_the_reference() {
    assert!(!keeps_the_videos_text(Some(0.45), 0.9));
    assert!(!keeps_the_videos_text(None, 0.6));
}

/// The video's own complete captions match what is sung better than another
/// recording's lyric: they stay the reference text.
#[test]
fn the_videos_own_better_text_stays_the_reference() {
    assert!(keeps_the_videos_text(Some(0.95), 0.9));
}

/// A tie keeps the video's own text.
#[test]
fn a_tie_keeps_the_videos_own_text() {
    assert!(keeps_the_videos_text(Some(0.9), 0.9));
}

// ---- lines_overlap_score / remove_audit ----

#[test]
fn any_lines_are_scored_like_a_lyric() {
    let lines = ["You never fail,", "You never will!"];
    let s = sung("you never fail you never will jesus");
    assert_eq!(lines_overlap_score(lines.into_iter(), &s), 12.0 / 13.0);
}

#[tokio::test]
async fn a_stale_audit_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("yt1_title_search_audit.json");
    std::fs::write(&p, "{}").unwrap();
    remove_audit(dir.path(), "yt1").await;
    assert!(!p.exists());
    // Nothing to remove is fine too.
    remove_audit(dir.path(), "yt1").await;
}

// ---- cleanup_cache_name / audit_json ----

fn title_cand(provider: TitleProvider, id: &str, synced: bool) -> TitleCandidate {
    TitleCandidate {
        provider,
        id: id.into(),
        artist: "A".into(),
        record_duration_s: Some(484.5),
        lyrics: lyrics(&["x", "y"]),
        synced,
    }
}

#[test]
fn the_cleanup_cache_is_keyed_by_the_candidate() {
    let lrclib = title_cand(TitleProvider::Lrclib, "35094059", false);
    assert_eq!(
        cleanup_cache_name("yt1", &lrclib),
        "yt1_title_lrclib_35094059_cleaned_v3.json"
    );
    let genius = title_cand(
        TitleProvider::Genius,
        "https://genius.com/Elevation-worship-jesus-be-the-name-lyrics/",
        false,
    );
    assert_eq!(
        cleanup_cache_name("yt1", &genius),
        "yt1_title_genius_Elevation-worship-jesus-be-the-name-lyrics_cleaned_v3.json"
    );
    let odd = title_cand(TitleProvider::Genius, "https://x/a?b=c&d_e", false);
    assert_eq!(
        cleanup_cache_name("yt1", &odd),
        "yt1_title_genius_abcde_cleaned_v3.json"
    );
}

#[test]
fn the_audit_lists_every_candidate_its_score_and_the_choice() {
    let cands = vec![
        title_cand(TitleProvider::Lrclib, "1", true),
        title_cand(TitleProvider::Genius, "https://g/x", false),
    ];
    let a = audit_json(
        "Jesus Be the Name",
        Some(484),
        &cands,
        &[0.25, 0.75],
        Some(1),
    );
    assert_eq!(
        a,
        serde_json::json!({
            "title": "Jesus Be the Name",
            "duration_s": 484,
            "min_score": 0.5,
            "candidates": [
                {"provider": "lrclib", "id": "1", "artist": "A", "record_duration_s": 484.5,
                 "lines": 2, "synced": true, "score": 0.25},
                {"provider": "genius", "id": "https://g/x", "artist": "A", "record_duration_s": 484.5,
                 "lines": 2, "synced": false, "score": 0.75}
            ],
            "chosen": 1
        })
    );
}

// ---- TitleSearch::find (wiremock end to end) ----

fn row(song: &str) -> VideoLyricsRow {
    VideoLyricsRow {
        id: 158,
        youtube_id: "yt158".into(),
        song: song.into(),
        artist: "CityHill Worship".into(),
        duration_ms: Some(484_000),
        audio_file_path: None,
        youtube_url: String::new(),
        lyrics_override_text: None,
        lyrics_time_offset_ms: 0,
        spotify_track_id: None,
        spotify_resolved_at: None,
    }
}

fn endpoints(server: &MockServer) -> TitleSearchEndpoints {
    TitleSearchEndpoints {
        lrclib_search: format!("{}/api/search", server.uri()),
        genius_search: format!("{}/search", server.uri()),
    }
}

const RIGHT_SONG: &str = "[00:10.00] You never fail\n[00:13.00] You never will\n[00:16.00] Jesus be the name\n[00:19.00] Above every other name";
const OTHER_SONG: &str = "[00:05.00] Jesus be the name of my joy\n[00:09.00] Something totally different words here\n[00:13.00] Another line of other words";

async fn mount_lrclib(server: &MockServer, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/api/search"))
        .and(query_param("track_name", "Jesus Be the Name"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(server)
        .await;
}

/// LRCLIB holds the song (484 s, synced) and a different song of the same
/// title; a 539 s record of the song is outside ±15 s. The sung transcript
/// picks the song (1.0 against 10/31), and its synced lyric becomes a timed
/// `lrclib` candidate as is. No Genius token: Genius is not asked.
#[tokio::test]
async fn the_sung_words_choose_the_song_over_a_same_title_different_song() {
    let server = MockServer::start().await;
    mount_lrclib(
        &server,
        serde_json::json!([
            {"id": 2, "trackName": "Jesus Be the Name", "artistName": "Other Band",
             "duration": 480.0, "instrumental": false, "syncedLyrics": OTHER_SONG},
            {"id": 1, "trackName": "Jesus Be The Name", "artistName": "Elevation Worship",
             "duration": 484.0, "instrumental": false, "syncedLyrics": RIGHT_SONG},
            {"id": 3, "trackName": "Jesus Be The Name", "artistName": "Elevation Worship",
             "duration": 539.0, "instrumental": false, "syncedLyrics": RIGHT_SONG}
        ]),
    )
    .await;
    Mock::given(path("/search"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let search = TitleSearch {
        client: &client,
        ai: None,
        genius_token: "",
        cache_dir: dir.path(),
    };

    let found = search
        .find(
            &endpoints(&server),
            &row("Jesus Be the Name"),
            &sung(SUNG),
            &[],
        )
        .await
        .expect("the song's lyric reaches the floor");
    assert_eq!(found.source, "lrclib");
    assert_eq!(
        found.lines,
        vec![
            "You never fail",
            "You never will",
            "Jesus be the name",
            "Above every other name"
        ]
    );
    assert!(found.has_timing);
    assert_eq!(
        found.line_timings,
        Some(vec![
            (10_000, 13_000),
            (13_000, 16_000),
            (16_000, 19_000),
            (19_000, 24_000)
        ])
    );

    let audit: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("yt158_title_search_audit.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(audit["chosen"], 1);
    assert_eq!(audit["duration_s"], 484);
    assert_eq!(audit["candidates"][0]["id"], "2");
    assert_eq!(audit["candidates"][0]["score"], 10.0 / 31.0);
    assert_eq!(audit["candidates"][1]["id"], "1");
    assert_eq!(audit["candidates"][1]["score"], 1.0);
    assert_eq!(audit["candidates"].as_array().unwrap().len(), 2);
    assert_eq!(
        audit["reference"],
        serde_json::json!({"source": "lrclib", "from": "title"})
    );
    assert_eq!(audit["videos_text"], serde_json::Value::Null);
}

/// Only Genius holds the song (the 158 case: LRCLIB's records are all 539 s).
/// Its page is a plain scraped lyric, so Claude cleans it into a text-only
/// `genius` candidate, cached under the candidate's own name.
#[tokio::test]
async fn a_genius_page_is_cleaned_into_a_text_candidate() {
    let server = MockServer::start().await;
    let base = server.uri();
    mount_lrclib(&server, serde_json::json!([])).await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "Jesus Be the Name"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "response": {"hits": [
                {"type": "song", "result": {"url": format!("{base}/elevation-lyrics"),
                    "primary_artist": {"name": "Elevation Worship"}}}
            ]}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/elevation-lyrics"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<div data-lyrics-container=\"true\">[Chorus]<br/>You never fail<br/>You never will<br/>Jesus be the name</div>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant",
                "content": "{\"lines\": [\"You never fail\", \"You never will\", \"Jesus be the name\"]}"}}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let ai = AiClient::new(AiSettings {
        api_url: format!("{base}/v1"),
        api_key: Some("test".into()),
        model: "claude-test".into(),
        system_prompt_extra: None,
    });
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let search = TitleSearch {
        client: &client,
        ai: Some(&ai),
        genius_token: "tok",
        cache_dir: dir.path(),
    };

    let found = search
        .find(
            &endpoints(&server),
            &row("Jesus Be the Name"),
            &sung(SUNG),
            &[],
        )
        .await
        .expect("the Genius page reaches the floor (20/24)");
    assert_eq!(found.source, "genius");
    assert_eq!(
        found.lines,
        vec!["You never fail", "You never will", "Jesus be the name"]
    );
    assert!(!found.has_timing);
    assert!(
        dir.path()
            .join("yt158_title_genius_elevation-lyrics_cleaned_v3.json")
            .exists()
    );
}

/// Only a same-title different song is found: nothing reaches the floor,
/// the song keeps what it had; the audit records the miss.
#[tokio::test]
async fn a_different_song_of_the_same_title_is_never_chosen() {
    let server = MockServer::start().await;
    mount_lrclib(
        &server,
        serde_json::json!([
            {"id": 2, "trackName": "Jesus Be the Name", "artistName": "Other Band",
             "duration": 480.0, "instrumental": false, "syncedLyrics": OTHER_SONG}
        ]),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let search = TitleSearch {
        client: &client,
        ai: None,
        genius_token: "",
        cache_dir: dir.path(),
    };
    let found = search
        .find(
            &endpoints(&server),
            &row("Jesus Be the Name"),
            &sung(SUNG),
            &[],
        )
        .await;
    assert!(found.is_none());
    let audit: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("yt158_title_search_audit.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(audit["chosen"], serde_json::Value::Null);
    assert_eq!(audit["candidates"][0]["score"], 10.0 / 31.0);
}

/// A plain lyric needs the Claude cleanup; without a Claude client the
/// chosen plain lyric is not used.
#[tokio::test]
async fn a_plain_lyric_without_a_claude_client_is_not_used() {
    let server = MockServer::start().await;
    mount_lrclib(
        &server,
        serde_json::json!([
            {"id": 9, "trackName": "Jesus Be the Name", "artistName": "Elevation Worship",
             "duration": 490.0, "instrumental": false,
             "plainLyrics": "You never fail\nYou never will\nJesus be the name\nAbove every other name"}
        ]),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let search = TitleSearch {
        client: &client,
        ai: None,
        genius_token: "",
        cache_dir: dir.path(),
    };
    let found = search
        .find(
            &endpoints(&server),
            &row("Jesus Be the Name"),
            &sung(SUNG),
            &[],
        )
        .await;
    assert!(found.is_none());
}

/// No title: nothing is searched, and a stale audit of an earlier run goes.
#[tokio::test]
async fn a_row_without_a_title_searches_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let stale = dir.path().join("yt158_title_search_audit.json");
    std::fs::write(&stale, "{}").unwrap();
    let client = reqwest::Client::new();
    let search = TitleSearch {
        client: &client,
        ai: None,
        genius_token: "tok",
        cache_dir: dir.path(),
    };
    assert!(
        search
            .find(&endpoints(&server), &row("  "), &sung(SUNG), &[])
            .await
            .is_none()
    );
    assert!(!stale.exists(), "the stale audit must go");
}

fn gathered(source: &str, has_timing: bool, lines: &[&str]) -> TierCandidate {
    TierCandidate {
        source: source.into(),
        lines: lines.iter().map(|l| l.to_string()).collect(),
        line_timings: None,
        has_timing,
    }
}

/// The best lyric is a plain one the cleanup cannot use (no Claude client):
/// the next-best usable lyric above the floor is taken instead.
#[tokio::test]
async fn a_lyric_the_cleanup_cannot_use_passes_to_the_next_best() {
    let server = MockServer::start().await;
    mount_lrclib(
        &server,
        serde_json::json!([
            {"id": 9, "trackName": "Jesus Be the Name", "artistName": "Plain Records",
             "duration": 490.0, "instrumental": false,
             "plainLyrics": "You never fail\nYou never will\nJesus be the name\nAbove every other name"},
            {"id": 1, "trackName": "Jesus Be The Name", "artistName": "Elevation Worship",
             "duration": 484.0, "instrumental": false, "syncedLyrics": RIGHT_SONG}
        ]),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let search = TitleSearch {
        client: &client,
        ai: None,
        genius_token: "",
        cache_dir: dir.path(),
    };
    let found = search
        .find(
            &endpoints(&server),
            &row("Jesus Be the Name"),
            &sung(SUNG),
            &[],
        )
        .await
        .expect("the synced next-best lyric is usable");
    assert_eq!(found.source, "lrclib");
    assert!(found.has_timing);
    let audit: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("yt158_title_search_audit.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        audit["chosen"], 1,
        "the found lyric is the second candidate"
    );
}

/// A partial description of the video (6 of the 14 sung words, 12/20)
/// loses to the full lyric found by title (1.0): the lyric is the reference.
#[tokio::test]
async fn the_found_lyric_replaces_a_partial_description() {
    let server = MockServer::start().await;
    mount_lrclib(
        &server,
        serde_json::json!([
            {"id": 1, "trackName": "Jesus Be The Name", "artistName": "Elevation Worship",
             "duration": 484.0, "instrumental": false, "syncedLyrics": RIGHT_SONG}
        ]),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let search = TitleSearch {
        client: &client,
        ai: None,
        genius_token: "",
        cache_dir: dir.path(),
    };
    let description = gathered("description", false, &["You never fail", "You never will"]);
    let found = search
        .find(
            &endpoints(&server),
            &row("Jesus Be the Name"),
            &sung(SUNG),
            &[description],
        )
        .await
        .expect("a lyric was found");
    assert_eq!(found.source, "lrclib");
    assert_eq!(found.lines.len(), 4);
    let audit: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("yt158_title_search_audit.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        audit["videos_text"],
        serde_json::json!({"source": "description", "score": 12.0 / 20.0})
    );
    assert_eq!(
        audit["reference"],
        serde_json::json!({"source": "lrclib", "from": "title"})
    );
}

/// The video's own complete captions (1.0) match what is sung better than
/// another recording's shorter lyric found by title (20/24): the captions
/// stay the reference text.
#[tokio::test]
async fn the_videos_own_complete_captions_stay_the_reference() {
    let server = MockServer::start().await;
    mount_lrclib(
        &server,
        serde_json::json!([
            {"id": 5, "trackName": "Jesus Be the Name", "artistName": "Other Recording",
             "duration": 470.0, "instrumental": false,
             "syncedLyrics": "[00:01.00] You never fail\n[00:03.00] You never will\n[00:05.00] Jesus be the name"}
        ]),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let search = TitleSearch {
        client: &client,
        ai: None,
        genius_token: "",
        cache_dir: dir.path(),
    };
    let captions = gathered(
        "yt_subs",
        true,
        &[
            "You never fail",
            "You never will",
            "Jesus be the name",
            "Above every other name",
        ],
    );
    let found = search
        .find(
            &endpoints(&server),
            &row("Jesus Be the Name"),
            &sung(SUNG),
            &[captions],
        )
        .await
        .expect("a lyric was found");
    assert_eq!(found.source, "yt_subs");
    assert_eq!(found.lines.len(), 4);
    let audit: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("yt158_title_search_audit.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(audit["candidates"][0]["score"], 20.0 / 24.0);
    assert_eq!(
        audit["reference"],
        serde_json::json!({"source": "yt_subs", "from": "video"})
    );
}

/// The found lyric competes with the text the video would use WITHOUT it —
/// its priority pick (timed captions over a description) — never with a
/// hand-picked best: here the partial captions (12/20) lose to the lyric
/// (20/24), and the full description (1.0) is not considered, exactly as it
/// is not picked when no lyric is found.
#[tokio::test]
async fn the_found_lyric_competes_with_the_videos_priority_pick() {
    let server = MockServer::start().await;
    mount_lrclib(
        &server,
        serde_json::json!([
            {"id": 5, "trackName": "Jesus Be the Name", "artistName": "Other Recording",
             "duration": 470.0, "instrumental": false,
             "syncedLyrics": "[00:01.00] You never fail\n[00:03.00] You never will\n[00:05.00] Jesus be the name"}
        ]),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let search = TitleSearch {
        client: &client,
        ai: None,
        genius_token: "",
        cache_dir: dir.path(),
    };
    let description = gathered(
        "description",
        false,
        &[
            "You never fail",
            "You never will",
            "Jesus be the name",
            "Above every other name",
        ],
    );
    let captions = gathered("yt_subs", true, &["You never fail", "You never will"]);
    let found = search
        .find(
            &endpoints(&server),
            &row("Jesus Be the Name"),
            &sung(SUNG),
            &[description, captions],
        )
        .await
        .expect("a lyric was found");
    assert_eq!(found.source, "lrclib");
    let audit: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("yt158_title_search_audit.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        audit["videos_text"],
        serde_json::json!({"source": "yt_subs", "score": 12.0 / 20.0})
    );
}
