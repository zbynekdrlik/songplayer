//! #144: the live g35t probe — the clip it picks and cuts, and what it
//! reports for every answer the API can give. The transcription goes through
//! the worker's own `g35t_client::transcribe_at` against a wiremock server:
//! the same upload, poll, request body and key rotation as a song.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use sqlx::SqlitePool;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

/// The Gemini File-API name the mock upload gives the clip.
const FILE: &str = "files/probe-clip";

/// A long enough bound for every mock answer (the timeout test sets its own).
const LIMIT: Duration = Duration::from_secs(60);

fn keys(list: &[&str]) -> Vec<String> {
    list.iter().map(|k| k.to_string()).collect()
}

/// The clip the probe uploads. The mock never decodes it.
fn clip_wav(dir: &Path) -> PathBuf {
    let wav = dir.join("g35t_probe.wav");
    std::fs::write(&wav, b"RIFF\x24\0\0\0WAVEfmt not really audio").unwrap();
    wav
}

fn clip_info() -> ClipInfo {
    ClipInfo {
        youtube_id: "fffffffffff".to_string(),
        source: ClipSource::Vocals,
        start_ms: 12_345,
        duration_ms: CLIP_MS,
    }
}

/// The upload answers `key` with `status` + `body`.
async fn upload_answers(server: &MockServer, key: &str, status: u16, body: String) {
    Mock::given(method("POST"))
        .and(path("/upload/v1beta/files"))
        .and(header("x-goog-api-key", key))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(server)
        .await;
}

/// The upload accepts `key`; the file is ACTIVE at once; the uploaded file
/// is deleted exactly once afterwards.
async fn upload_accepts(server: &MockServer, key: &str) {
    let file = json!({
        "name": FILE,
        "uri": format!("{}/v1beta/{FILE}", server.uri()),
        "mimeType": "audio/wav",
        "state": "ACTIVE",
    });
    upload_answers(server, key, 200, json!({ "file": file }).to_string()).await;
    Mock::given(method("GET"))
        .and(path(format!("/v1beta/{FILE}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(file.to_string()))
        .mount(server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/v1beta/{FILE}")))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .expect(1)
        .mount(server)
        .await;
}

/// The interactions call answers `status` + `body`.
async fn interactions_answer(server: &MockServer, status: u16, body: String) {
    Mock::given(method("POST"))
        .and(path("/v1beta/interactions"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(server)
        .await;
}

/// A completed interaction carrying `words` as `word_info` annotations.
fn completed_with(words: &[&str]) -> String {
    let annotations: Vec<Value> = words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            json!({
                "type": "word_info",
                "text": w,
                "start_offset": format!("{}s", i + 1),
                "end_offset": format!("{}.5s", i + 1),
            })
        })
        .collect();
    json!({
        "id": "interaction-1",
        "status": "completed",
        "steps": [{"content": [{"annotations": annotations}]}],
    })
    .to_string()
}

/// Google's refusal of an invalid key (a 400 naming the API key: next key).
const INVALID_KEY: &str = r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT","details":[{"reason":"API_KEY_INVALID"}]}}"#;

// ---- the answer for every API outcome -------------------------------------

/// A refused first key moves to the next; the answer reports the key that
/// answered, the words, the model and the hint — and the request really
/// carried that model and hint (the worker's own body).
#[tokio::test]
async fn a_probe_reports_the_words_the_answering_key_the_model_and_the_hint() {
    let server = MockServer::start().await;
    upload_answers(&server, "k-dead", 400, INVALID_KEY.to_string()).await;
    upload_accepts(&server, "k-live").await;
    interactions_answer(&server, 200, completed_with(&["Holy", "is", "the", "Lord"])).await;
    let dir = tempfile::tempdir().unwrap();

    let report = probe_clip(
        &reqwest::Client::new(),
        &server.uri(),
        &keys(&["k-dead", "k-live"]),
        &clip_wav(dir.path()),
        Some(clip_info()),
        LIMIT,
    )
    .await;

    assert_eq!(report.error, None);
    assert!(report.ok);
    assert_eq!(report.model, "gemini-3.5-transcribe");
    assert_eq!(report.language_codes, ["en-US", "es-419"]);
    assert_eq!(report.key_index, Some(1), "the second key answered");
    assert_eq!(report.word_count, 4);
    assert_eq!(report.sample, "Holy is the Lord");
    assert_eq!(report.clip, Some(clip_info()));

    let requests = server.received_requests().await.unwrap();
    let interactions: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/v1beta/interactions")
        .collect();
    assert_eq!(interactions.len(), 1);
    assert_eq!(
        interactions[0].headers.get("x-goog-api-key").unwrap(),
        "k-live"
    );
    let sent: Value = interactions[0].body_json().unwrap();
    assert_eq!(sent["model"], "gemini-3.5-transcribe");
    assert_eq!(
        sent["generation_config"]["transcription_config"]["language_codes"],
        json!(["en-US", "es-419"])
    );
}

/// Every key refused: the probe fails with the API's own message, naming
/// the last key by its place in the list — never by its value, even when
/// the API echoes it.
#[tokio::test]
async fn a_refused_key_fails_the_probe_with_the_api_message() {
    let server = MockServer::start().await;
    let first = r#"{"error":{"code":403,"message":"Method doesn't allow unregistered callers.","status":"PERMISSION_DENIED"}}"#;
    let second = r#"{"error":{"code":403,"message":"Requests from the API key k-two-secret are blocked.","status":"PERMISSION_DENIED"}}"#;
    upload_answers(&server, "k-one", 403, first.to_string()).await;
    upload_answers(&server, "k-two-secret", 403, second.to_string()).await;
    let dir = tempfile::tempdir().unwrap();

    let report = probe_clip(
        &reqwest::Client::new(),
        &server.uri(),
        &keys(&["k-one", "k-two-secret"]),
        &clip_wav(dir.path()),
        Some(clip_info()),
        LIMIT,
    )
    .await;

    assert!(!report.ok);
    assert_eq!(
        report.error.as_deref(),
        Some(
            "g35t_client: all 2 keys refused; key 2 of 2: g35t_client upload: key refused \
             status=403 body={\"error\":{\"code\":403,\"message\":\"Requests from the API key \
             <key> are blocked.\",\"status\":\"PERMISSION_DENIED\"}}"
        )
    );
    assert_eq!(report.key_index, Some(1));
    assert_eq!(report.word_count, 0);
    assert_eq!(report.sample, "");
    assert_eq!(report.language_codes, ["en-US", "es-419"]);
    let uploads = server.received_requests().await.unwrap();
    assert_eq!(uploads.len(), 2, "one upload per key, nothing else");
}

/// A 400 on the request body (e.g. a refused `language_codes` field) fails
/// the probe at once with the API's message on one line: it would fail the
/// same on every key, so the spare key is never tried, and the uploaded clip
/// is still deleted.
#[tokio::test]
async fn a_refused_request_body_fails_the_probe_with_the_api_message() {
    let server = MockServer::start().await;
    upload_accepts(&server, "k-live").await;
    Mock::given(method("POST"))
        .and(path("/upload/v1beta/files"))
        .and(header("x-goog-api-key", "k-spare"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    // Google answers errors pretty-printed.
    let refused = "{\n  \"error\": {\n    \"code\": 400,\n    \"message\": \"Invalid JSON \
                   payload received. Unknown name \\\"language_codes\\\": Cannot find \
                   field.\",\n    \"status\": \"INVALID_ARGUMENT\"\n  }\n}\n";
    interactions_answer(&server, 400, refused.to_string()).await;
    let dir = tempfile::tempdir().unwrap();

    let report = probe_clip(
        &reqwest::Client::new(),
        &server.uri(),
        &keys(&["k-live", "k-spare"]),
        &clip_wav(dir.path()),
        None,
        LIMIT,
    )
    .await;

    assert!(!report.ok);
    assert_eq!(
        report.error.as_deref(),
        Some(
            "key 1 of 2: g35t_client interactions: unexpected status=400 body={ \"error\": { \
             \"code\": 400, \"message\": \"Invalid JSON payload received. Unknown name \
             \\\"language_codes\\\": Cannot find field.\", \"status\": \"INVALID_ARGUMENT\" } }"
        )
    );
    assert_eq!(report.key_index, Some(0));
    assert_eq!(report.word_count, 0);
    assert_eq!(report.clip, None);
}

/// A completed answer with no words is a failed probe: the gate must not
/// pass a model that hears nothing.
#[tokio::test]
async fn an_answer_with_no_words_fails_the_probe() {
    let server = MockServer::start().await;
    upload_accepts(&server, "k-live").await;
    interactions_answer(&server, 200, completed_with(&[])).await;
    let dir = tempfile::tempdir().unwrap();

    let report = probe_clip(
        &reqwest::Client::new(),
        &server.uri(),
        &keys(&["k-live"]),
        &clip_wav(dir.path()),
        Some(clip_info()),
        LIMIT,
    )
    .await;

    assert!(!report.ok);
    assert_eq!(
        report.error.as_deref(),
        Some("Gemini answered the clip with no words")
    );
    assert_eq!(report.key_index, Some(0));
    assert_eq!(report.word_count, 0);
    assert_eq!(report.sample, "");
}

/// A call that does not answer within the bound fails the probe naming the
/// bound (the e2e spec waits longer, so a hung call fails WITH this error).
#[tokio::test]
async fn a_probe_with_no_answer_in_time_fails_naming_the_bound() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload/v1beta/files"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();

    let report = probe_clip(
        &reqwest::Client::new(),
        &server.uri(),
        &keys(&["k-live"]),
        &clip_wav(dir.path()),
        None,
        Duration::from_millis(100),
    )
    .await;

    assert!(!report.ok);
    assert_eq!(report.error.as_deref(), Some("no answer within 0.1 s"));
    assert_eq!(report.key_index, None);
}

// ---- the clip ----------------------------------------------------------------

#[test]
fn seconds_keep_milliseconds() {
    assert_eq!(seconds(61_234), "61.234");
    assert_eq!(seconds(20_000), "20.000");
    assert_eq!(seconds(5), "0.005");
    assert_eq!(seconds(0), "0.000");
}

/// 20 s of the input from the window start, audio only, 16 kHz mono PCM.
#[test]
fn clip_args_cut_the_window_into_a_16khz_mono_wav() {
    let args = clip_args(
        Path::new("/cache/S_A_fffffffffff_normalized_audio_vocals.flac"),
        61_234,
        Path::new("/tmp/g35t_probe.wav"),
    );
    let want: Vec<OsString> = [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-ss",
        "61.234",
        "-t",
        "20.000",
        "-i",
        "/cache/S_A_fffffffffff_normalized_audio_vocals.flac",
        "-vn",
        "-ac",
        "1",
        "-ar",
        "16000",
        "-c:a",
        "pcm_s16le",
        "-y",
        "/tmp/g35t_probe.wav",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    assert_eq!(args, want);
}

#[test]
fn the_clip_info_names_the_song_the_file_and_the_window() {
    let clip = ProbeClip {
        youtube_id: "fffffffffff".to_string(),
        input: PathBuf::from("/cache/x_audio_vocals.flac"),
        source: ClipSource::Vocals,
        start_ms: 12_345,
    };
    assert_eq!(clip.info(), clip_info());
    assert_eq!(
        serde_json::to_value(clip_info()).unwrap(),
        json!({"youtube_id": "fffffffffff", "source": "vocals", "start_ms": 12345,
               "duration_ms": 20000})
    );
    assert_eq!(serde_json::to_value(ClipSource::Mix).unwrap(), json!("mix"));
}

/// A catalogue row the pick reads. `audio` is the recorded path (it may not
/// exist on disk).
async fn insert_video(
    pool: &SqlitePool,
    id: i64,
    youtube_id: &str,
    normalized: i64,
    has_lyrics: i64,
    audio: Option<&Path>,
) {
    sqlx::query("INSERT OR IGNORE INTO playlists (id, name, youtube_url) VALUES (1, 'P', 'url')")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, audio_file_path)
         VALUES (?, 1, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(youtube_id)
    .bind(normalized)
    .bind(has_lyrics)
    .bind(audio.map(|p| p.to_string_lossy().into_owned()))
    .execute(pool)
    .await
    .unwrap();
}

/// The song's audio sidecar in `cache`, created on disk.
fn audio_on_disk(cache: &Path, youtube_id: &str) -> PathBuf {
    let audio = cache.join(format!("S_A_{youtube_id}_normalized_audio.flac"));
    std::fs::write(&audio, b"flac").unwrap();
    audio
}

/// The song's served lyrics with lines starting at `starts` (ms).
fn lyrics_on_disk(cache: &Path, youtube_id: &str, starts: &[u64]) {
    let lines: Vec<Value> = starts
        .iter()
        .map(|s| json!({"start_ms": s, "end_ms": s + 2_000, "en": "line"}))
        .collect();
    let track = json!({"version": 22, "source": "gemini-3-5-transcribe", "lines": lines});
    std::fs::write(
        cache.join(format!("{youtube_id}_lyrics.json")),
        track.to_string(),
    )
    .unwrap();
}

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

/// The lowest-id row with served lyrics and its audio on disk wins; its
/// vocal stem is the input when on disk; the window starts at its EARLIEST
/// line. Rows missing a piece are passed over.
#[tokio::test]
async fn the_probe_picks_the_lowest_served_song_with_its_audio_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path();
    let pool = pool().await;
    // 1: audio recorded, not on disk.
    insert_video(
        &pool,
        1,
        "aaaaaaaaaaa",
        1,
        1,
        Some(cache.join("gone_audio.flac").as_path()),
    )
    .await;
    lyrics_on_disk(cache, "aaaaaaaaaaa", &[1_000]);
    // 2: no served lyrics. 3: not normalized.
    for (id, yt, normalized, has_lyrics) in [(2, "bbbbbbbbbbb", 1, 0), (3, "ccccccccccc", 0, 1)] {
        let audio = audio_on_disk(cache, yt);
        insert_video(&pool, id, yt, normalized, has_lyrics, Some(audio.as_path())).await;
        lyrics_on_disk(cache, yt, &[1_000]);
    }
    // 4: no lyrics file. 5: a lyrics file with no line.
    let audio = audio_on_disk(cache, "ddddddddddd");
    insert_video(&pool, 4, "ddddddddddd", 1, 1, Some(audio.as_path())).await;
    let audio = audio_on_disk(cache, "eeeeeeeeeee");
    insert_video(&pool, 5, "eeeeeeeeeee", 1, 1, Some(audio.as_path())).await;
    lyrics_on_disk(cache, "eeeeeeeeeee", &[]);
    // 6: the pick — its stem on disk, its lines out of order.
    let audio = audio_on_disk(cache, "fffffffffff");
    let vocals = crate::stems::stem_paths(&audio).0;
    std::fs::write(&vocals, b"flac").unwrap();
    insert_video(&pool, 6, "fffffffffff", 1, 1, Some(audio.as_path())).await;
    lyrics_on_disk(cache, "fffffffffff", &[41_500, 12_345, 20_000]);
    // 7: also good, but a higher id.
    let audio = audio_on_disk(cache, "ggggggggggg");
    insert_video(&pool, 7, "ggggggggggg", 1, 1, Some(audio.as_path())).await;
    lyrics_on_disk(cache, "ggggggggggg", &[3_000]);

    let clip = pick_clip(&pool, cache).await.unwrap();

    assert_eq!(
        clip,
        ProbeClip {
            youtube_id: "fffffffffff".to_string(),
            input: vocals,
            source: ClipSource::Vocals,
            start_ms: 12_345,
        }
    );
}

#[tokio::test]
async fn without_a_vocal_stem_the_clip_is_cut_from_the_mix() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path();
    let pool = pool().await;
    let audio = audio_on_disk(cache, "fffffffffff");
    insert_video(&pool, 1, "fffffffffff", 1, 1, Some(audio.as_path())).await;
    lyrics_on_disk(cache, "fffffffffff", &[7_250]);

    let clip = pick_clip(&pool, cache).await.unwrap();

    assert_eq!(
        clip,
        ProbeClip {
            youtube_id: "fffffffffff".to_string(),
            input: audio,
            source: ClipSource::Mix,
            start_ms: 7_250,
        }
    );
}

const NO_SONG: &str = "no song to probe: no row with served lyrics has its audio on disk";

#[tokio::test]
async fn an_empty_catalogue_has_no_song_to_probe() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        pick_clip(&pool().await, dir.path()).await,
        Err(NO_SONG.to_string())
    );
}

// ---- run_probe: each missing piece stops it before anything is sent ---------

#[tokio::test]
async fn with_no_song_the_probe_stops_naming_it() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    let report = run_probe(
        &pool().await,
        dir.path(),
        Some(PathBuf::from("ffmpeg")),
        &keys(&["k-live"]),
        &reqwest::Client::new(),
        &server.uri(),
    )
    .await;

    assert_eq!(report, G35tProbeReport::refused(NO_SONG.into(), None));
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// A pool with one song the probe can pick, and that clip's info.
async fn one_song(cache: &Path) -> (SqlitePool, ClipInfo) {
    let pool = pool().await;
    let audio = audio_on_disk(cache, "fffffffffff");
    insert_video(&pool, 1, "fffffffffff", 1, 1, Some(audio.as_path())).await;
    lyrics_on_disk(cache, "fffffffffff", &[9_000]);
    let info = ClipInfo {
        youtube_id: "fffffffffff".to_string(),
        source: ClipSource::Mix,
        start_ms: 9_000,
        duration_ms: CLIP_MS,
    };
    (pool, info)
}

#[tokio::test]
async fn without_ffmpeg_the_probe_stops_naming_the_clip() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (pool, info) = one_song(dir.path()).await;

    let report = run_probe(
        &pool,
        dir.path(),
        None,
        &keys(&["k-live"]),
        &reqwest::Client::new(),
        &server.uri(),
    )
    .await;

    let error = "ffmpeg is not ready yet (the tools are still starting)";
    assert_eq!(report, G35tProbeReport::refused(error.into(), Some(info)));
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// An ffmpeg that cannot run stops the probe with its reason; nothing is
/// uploaded.
#[tokio::test]
async fn a_clip_that_cannot_be_cut_stops_the_probe() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (pool, info) = one_song(dir.path()).await;

    let report = run_probe(
        &pool,
        dir.path(),
        Some(dir.path().join("no-such-ffmpeg")),
        &keys(&["k-live"]),
        &reqwest::Client::new(),
        &server.uri(),
    )
    .await;

    assert!(!report.ok);
    assert_eq!(report.clip, Some(info));
    let error = report.error.unwrap();
    assert!(error.starts_with("ffmpeg did not start: "), "{error}");
    assert!(server.received_requests().await.unwrap().is_empty());
}
