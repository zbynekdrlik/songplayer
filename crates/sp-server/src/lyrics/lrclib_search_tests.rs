//! Tests for `lrclib_search.rs` (#144): the LRCLIB title search.

use super::*;
use wiremock::matchers::{header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn record(
    id: i64,
    duration: Option<f32>,
    synced: Option<&str>,
    plain: Option<&str>,
) -> SearchRecord {
    SearchRecord {
        id,
        track_name: "Jesus Be the Name".into(),
        artist_name: format!("Artist {id}"),
        duration,
        instrumental: false,
        synced_lyrics: synced.map(str::to_string),
        plain_lyrics: plain.map(str::to_string),
    }
}

fn ids(hits: &[LrclibTitleHit]) -> Vec<i64> {
    hits.iter().map(|h| h.id).collect()
}

const SYNCED: &str = "[00:10.00] You never fail\n[00:12.00] You never will";

/// The song is 484 s: 539 s is outside ±15 s; 480 s and exactly 499 s
/// (Δ 15) are inside; 499.5 s is outside; a record with no duration is kept.
#[test]
fn only_records_within_15_s_of_the_song_are_kept() {
    let records = vec![
        record(1, Some(539.0), Some(SYNCED), None),
        record(2, Some(480.0), Some(SYNCED), None),
        record(3, Some(499.0), Some(SYNCED), None),
        record(4, Some(499.5), Some(SYNCED), None),
        record(5, None, Some(SYNCED), None),
        record(6, Some(469.0), Some(SYNCED), None),
    ];
    assert_eq!(
        ids(&hits_from_records(records, Some(484))),
        vec![2, 3, 5, 6]
    );
}

/// A song with no known duration keeps every record: the transcript score
/// decides.
#[test]
fn a_song_of_unknown_length_keeps_every_record() {
    let records = vec![
        record(1, Some(539.0), Some(SYNCED), None),
        record(2, Some(180.0), Some(SYNCED), None),
    ];
    assert_eq!(ids(&hits_from_records(records, None)), vec![1, 2]);
}

#[test]
fn an_instrumental_record_is_never_a_candidate() {
    let mut instrumental = record(1, Some(484.0), Some(SYNCED), None);
    instrumental.instrumental = true;
    let records = vec![instrumental, record(2, Some(484.0), Some(SYNCED), None)];
    assert_eq!(ids(&hits_from_records(records, Some(484))), vec![2]);
}

/// Synced lyrics win (real line starts); plain lyrics are the fallback; a
/// record with neither is dropped.
#[test]
fn synced_lyrics_are_preferred_and_plain_is_the_fallback() {
    let records = vec![
        record(1, None, Some(SYNCED), Some("plain one\nplain two")),
        record(2, None, None, Some("Plain A\n\nPlain B")),
        record(3, None, Some("[00:01.00] "), Some("Only plain")),
        record(4, None, None, None),
    ];
    let hits = hits_from_records(records, Some(484));
    let got: Vec<(i64, bool, Vec<&str>, Vec<u64>)> = hits
        .iter()
        .map(|h| {
            (
                h.id,
                h.synced,
                h.lyrics.lines.iter().map(|l| l.en.as_str()).collect(),
                h.lyrics.lines.iter().map(|l| l.start_ms).collect(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (
                1,
                true,
                vec!["You never fail", "You never will"],
                vec![10_000, 12_000]
            ),
            (2, false, vec!["Plain A", "Plain B"], vec![0, 0]),
            (3, false, vec!["Only plain"], vec![0]),
        ]
    );
    assert_eq!(hits[0].artist, "Artist 1");
    assert_eq!(hits[0].track, "Jesus Be the Name");
    assert_eq!(hits[0].duration_s, None);
}

#[tokio::test]
async fn the_search_asks_lrclib_by_title_and_parses_the_records() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/search"))
        .and(query_param("track_name", "Jesus Be the Name"))
        .and(header_exists("user-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "id": 35094059,
                "trackName": "Jesus Be The Name",
                "artistName": "Elevation Worship",
                "albumName": "ignored",
                "duration": 539.0,
                "instrumental": false,
                "syncedLyrics": SYNCED,
                "plainLyrics": "You never fail"
            },
            {
                "id": 7,
                "trackName": "Jesus Be The Name",
                "artistName": "Cover Band",
                "duration": 486.0,
                "instrumental": false,
                "syncedLyrics": null,
                "plainLyrics": "You never fail\nYou never will"
            }
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let hits = search_by_title_at(
        &Client::new(),
        &format!("{}/api/search", server.uri()),
        "  Jesus Be the Name ",
        Some(484),
    )
    .await
    .unwrap();
    // Elevation's 539 s records are outside ±15 s of the 484 s song.
    assert_eq!(ids(&hits), vec![7]);
    assert_eq!(hits[0].artist, "Cover Band");
    assert_eq!(hits[0].duration_s, Some(486.0));
    assert!(!hits[0].synced);
    assert_eq!(hits[0].lyrics.lines.len(), 2);
}

#[tokio::test]
async fn an_empty_title_searches_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(0)
        .mount(&server)
        .await;
    let hits = search_by_title_at(&Client::new(), &server.uri(), "   ", Some(200))
        .await
        .unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn a_server_error_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let got = search_by_title_at(&Client::new(), &server.uri(), "Song", Some(200)).await;
    assert!(got.is_err());
}
