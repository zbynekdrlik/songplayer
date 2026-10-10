//! Tests for the worker's Genius fetch, `fetch_lyrics_at` (#232): what keeps
//! Genius from answering is an error naming why, so the probe and the live
//! gate show it; "Genius has no song of this artist" stays `Ok(None)`.
//! Sibling file of `genius.rs`.

use super::*;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn page(lines: &[&str]) -> String {
    format!(
        "<html><body><div data-lyrics-container=\"true\">{}</div></body></html>",
        lines.join("<br/>")
    )
}

/// A search answer whose one song hit is `artist`'s page at `{base}/song-lyrics`.
async fn search_finds(server: &MockServer, artist: &str) {
    let base = server.uri();
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "Hillsong What a Beautiful Name"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "response": {"hits": [
                {"type": "song", "result": {"url": format!("{base}/song-lyrics"),
                    "primary_artist": {"name": artist}}}
            ]}
        })))
        .expect(1)
        .mount(server)
        .await;
}

async fn fetch(server: &MockServer) -> Result<Option<LyricsTrack>> {
    fetch_lyrics_at(
        &Client::new(),
        &format!("{}/search", server.uri()),
        "tok",
        "Hillsong",
        "What a Beautiful Name",
    )
    .await
}

#[tokio::test]
async fn a_matched_song_page_returns_its_lyric() {
    let server = MockServer::start().await;
    search_finds(&server, "Hillsong Worship").await;
    Mock::given(method("GET"))
        .and(path("/song-lyrics"))
        .respond_with(ResponseTemplate::new(200).set_body_string(page(&[
            "You were the Word at the beginning",
            "One with God the Lord Most High",
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let track = fetch(&server).await.unwrap().unwrap();
    let lines: Vec<&str> = track.lines.iter().map(|l| l.en.as_str()).collect();
    assert_eq!(
        lines,
        [
            "You were the Word at the beginning",
            "One with God the Lord Most High"
        ]
    );
}

/// The search answered (the token works) with no song of this artist: no
/// hit, not an error, and no page is fetched.
#[tokio::test]
async fn no_song_of_the_artist_is_no_hit_not_an_error() {
    let server = MockServer::start().await;
    search_finds(&server, "Elevation Worship").await;
    Mock::given(method("GET"))
        .and(path("/song-lyrics"))
        .respond_with(ResponseTemplate::new(200).set_body_string(page(&["never"])))
        .expect(0)
        .mount(&server)
        .await;
    assert!(fetch(&server).await.unwrap().is_none());
}

/// A revoked token (401): an error naming the status, never "no hit".
#[tokio::test]
async fn a_refused_search_is_an_error_naming_its_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    let err = fetch(&server).await.unwrap_err().to_string();
    assert!(err.contains("Genius search answered 401"), "{err}");
}

#[tokio::test]
async fn an_unreadable_search_answer_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>blocked</html>"))
        .expect(1)
        .mount(&server)
        .await;
    let err = fetch(&server).await.unwrap_err().to_string();
    assert!(err.contains("Genius search answer unreadable"), "{err}");
}

/// The matched song's page refused (403: Genius blocks the box).
#[tokio::test]
async fn a_refused_song_page_is_an_error_naming_its_status() {
    let server = MockServer::start().await;
    search_finds(&server, "Hillsong Worship").await;
    Mock::given(method("GET"))
        .and(path("/song-lyrics"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&server)
        .await;
    let err = fetch(&server).await.unwrap_err().to_string();
    assert!(err.contains("Genius song page answered 403"), "{err}");
    assert!(err.contains("/song-lyrics"), "{err}");
}

/// A matched page with no lyric container (a changed page layout).
#[tokio::test]
async fn a_matched_page_with_no_lyric_is_an_error() {
    let server = MockServer::start().await;
    search_finds(&server, "Hillsong Worship").await;
    Mock::given(method("GET"))
        .and(path("/song-lyrics"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("<html><body>new layout</body></html>"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let err = fetch(&server).await.unwrap_err().to_string();
    assert!(err.contains("Genius song page holds no lyric"), "{err}");
}

/// No token, or no artist / song: nothing is asked.
#[tokio::test]
async fn no_token_or_no_names_asks_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let url = format!("{}/search", server.uri());
    let client = Client::new();
    for (token, artist, song) in [(" ", "A", "S"), ("tok", " ", "S"), ("tok", "A", "")] {
        assert!(
            fetch_lyrics_at(&client, &url, token, artist, song)
                .await
                .unwrap()
                .is_none()
        );
    }
}
