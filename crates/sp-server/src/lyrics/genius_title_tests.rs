//! Tests for the Genius TITLE search (#144): `search_by_title_at` and
//! `title_hit_pages`. Sibling file of `genius.rs`.

use super::*;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn hit(hit_type: &str, url: &str, artist: Option<&str>) -> SearchHit {
    SearchHit {
        hit_type: hit_type.into(),
        result: HitResult {
            url: url.into(),
            primary_artist: Some(ArtistRef {
                name: artist.map(str::to_string),
            }),
        },
    }
}

fn resp(hits: Vec<SearchHit>) -> SearchResponse {
    SearchResponse {
        response: SearchResponseInner { hits },
    }
}

/// Song-type hits only, single-song pages only, the first three; the artist
/// is carried along (empty when Genius names none).
#[test]
fn title_hit_pages_takes_the_first_three_single_song_pages() {
    let r = resp(vec![
        hit(
            "song",
            "https://genius.com/a-lyrics",
            Some("Elevation Worship"),
        ),
        hit("album", "https://genius.com/an-album", Some("X")),
        hit(
            "song",
            "https://genius.com/Genius-june-2021-singles-release-calendar-annotated",
            Some("Genius"),
        ),
        hit("song", "https://genius.com/b-lyrics", None),
        hit("song", "https://genius.com/c-lyrics", Some("C")),
        hit("song", "https://genius.com/d-lyrics", Some("D")),
    ]);
    assert_eq!(
        title_hit_pages(&r),
        vec![
            (
                "https://genius.com/a-lyrics".to_string(),
                "Elevation Worship".to_string()
            ),
            ("https://genius.com/b-lyrics".to_string(), String::new()),
            ("https://genius.com/c-lyrics".to_string(), "C".to_string()),
        ]
    );
}

fn page(lines: &[&str]) -> String {
    format!(
        "<html><body><div data-lyrics-container=\"true\">{}</div></body></html>",
        lines.join("<br/>")
    )
}

/// The search asks by title alone, then fetches the song pages: a page that
/// 404s is skipped, a hit beyond the first three is never fetched.
#[tokio::test]
async fn the_title_search_fetches_the_song_pages() {
    let server = MockServer::start().await;
    let base = server.uri();
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "Jesus Be the Name"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "response": {
                "hits": [
                    {"type": "song", "result": {"url": format!("{base}/elevation-lyrics"),
                        "primary_artist": {"name": "Elevation Worship"}}},
                    {"type": "song", "result": {"url": format!("{base}/gone-lyrics"),
                        "primary_artist": {"name": "Gone"}}},
                    {"type": "song", "result": {"url": format!("{base}/jeff-jim-live-lyrics"),
                        "primary_artist": {"name": "Jeff Jim"}}},
                    {"type": "song", "result": {"url": format!("{base}/fourth-lyrics"),
                        "primary_artist": {"name": "Fourth"}}}
                ]
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/elevation-lyrics"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(page(&["You never fail", "You never will"])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/gone-lyrics"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/jeff-jim-live-lyrics"))
        .respond_with(ResponseTemplate::new(200).set_body_string(page(&["Jesus be the name"])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fourth-lyrics"))
        .respond_with(ResponseTemplate::new(200).set_body_string(page(&["never fetched"])))
        .expect(0)
        .mount(&server)
        .await;

    let hits = search_by_title_at(
        &Client::new(),
        &format!("{base}/search"),
        " tok ",
        " Jesus Be the Name ",
    )
    .await
    .unwrap();
    let got: Vec<(String, String, Vec<String>)> = hits
        .into_iter()
        .map(|h| {
            let lines = h.lyrics.lines.into_iter().map(|l| l.en).collect();
            (h.url, h.artist, lines)
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (
                format!("{base}/elevation-lyrics"),
                "Elevation Worship".to_string(),
                vec!["You never fail".to_string(), "You never will".to_string()]
            ),
            (
                format!("{base}/jeff-jim-live-lyrics"),
                "Jeff Jim".to_string(),
                vec!["Jesus be the name".to_string()]
            ),
        ]
    );
}

/// No token (Genius not configured) or no title: nothing is asked.
#[tokio::test]
async fn no_token_or_no_title_asks_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let url = format!("{}/search", server.uri());
    let client = Client::new();
    assert!(
        search_by_title_at(&client, &url, "  ", "Song")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        search_by_title_at(&client, &url, "tok", " ")
            .await
            .unwrap()
            .is_empty()
    );
}

/// A refused search (401: a revoked token) is no hit, not an error.
#[tokio::test]
async fn a_refused_search_finds_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    let url = format!("{}/search", server.uri());
    let hits = search_by_title_at(&Client::new(), &url, "tok", "Song")
        .await
        .unwrap();
    assert!(hits.is_empty());
}
