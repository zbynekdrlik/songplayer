# Probe-Sources Endpoint + Reprocess Unblock + Queue Counts Fix — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `POST /api/v1/lyrics/probe-sources`, fix `/lyrics/reprocess` no-op on `no_source@pv-current` rows, align `/lyrics/queue` count SQL with worker-pop SQL. Single PR.

**Architecture:** New `probe.rs` module reuses each provider's existing raw-fetch fn (no Claude cleanup, no alignment). Reprocess SQL extended with `CASE` to clear `lyrics_source` for `('failed','empty','no_source')` while leaving `asr_gap` untouched. Queue count SQL gains the same `lyrics_source NOT IN (...)` filter the worker pop uses.

**Tech Stack:** Rust 2024, sqlx 0.8 SQLite, axum 0.8, tokio, anyhow, reqwest (existing patterns).

**Design spec:** `docs/superpowers/specs/2026-05-13-probe-sources-reprocess-fix-design.md` (commit `6bc70c4`).

**LYRICS_PIPELINE_VERSION stays at 20** per `feedback_no_bump_until_proven.md`. NO bump under any framing.

---

## Phase A — Probe module + endpoint

### Task A.1: probe.rs module + types + impl + sibling tests

**Files:**
- Create: `crates/sp-server/src/lyrics/probe.rs`
- Create: `crates/sp-server/src/lyrics/probe_tests.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub mod probe;` and `#[cfg(test)] mod probe_tests;`)

- [ ] **Step 1: Write failing tests in `probe_tests.rs`**

```rust
//! Tests for the lyrics-source probe.
//!
//! Sibling file (not nested `#[cfg(test)] mod tests` inside probe.rs) per the
//! split-tests convention used elsewhere in the lyrics module — keeps probe.rs
//! under the file-size cap and lets each test file stay narrowly focused.

use super::probe::{probe_sources_impl, ProbeReport, ProbeResult};
use crate::db::models::VideoLyricsRow;

fn fixture_row(song: &str, artist: &str, spotify_track_id: Option<&str>) -> VideoLyricsRow {
    VideoLyricsRow {
        id: 1,
        youtube_id: "ytid_test".into(),
        song: song.into(),
        artist: artist.into(),
        duration_ms: Some(180_000),
        audio_file_path: None,
        youtube_url: "https://www.youtube.com/playlist?list=X".into(),
        lyrics_override_text: None,
        lyrics_time_offset_ms: 0,
        spotify_track_id: spotify_track_id.map(String::from),
        spotify_resolved_at: None,
    }
}

#[tokio::test]
async fn probe_reports_any_text_source_false_when_all_providers_miss() {
    // All six probes miss: empty song/artist short-circuits the song-title
    // providers; no spotify_track_id; no genius token; bogus ytdlp path so
    // yt_subs + description return None.
    let report = probe_sources_impl(
        None,            // ai_client: not used by probe (no Claude)
        std::path::Path::new("/nonexistent/ytdlp"),
        std::path::Path::new("/tmp"),
        &reqwest::Client::new(),
        &fixture_row("", "", None),
        "",              // genius_access_token empty
    )
    .await;

    assert!(!report.any_text_source);
    assert_eq!(report.recommendation, "skip_no_text_source");
    assert!(report.probes.iter().all(|p| !p.available));
    let names: Vec<&str> = report.probes.iter().map(|p| p.provider.as_str()).collect();
    assert!(names.contains(&"yt_subs"));
    assert!(names.contains(&"description"));
    assert!(names.contains(&"lyrics_ovh"));
    assert!(names.contains(&"genius"));
    assert!(names.contains(&"lrclib"));
    assert!(names.contains(&"spotify"));
}

#[tokio::test]
async fn probe_reports_spotify_skipped_when_no_track_id() {
    let report = probe_sources_impl(
        None,
        std::path::Path::new("/nonexistent/ytdlp"),
        std::path::Path::new("/tmp"),
        &reqwest::Client::new(),
        &fixture_row("Song", "Artist", None),
        "",
    )
    .await;
    let spotify = report
        .probes
        .iter()
        .find(|p| p.provider == "spotify")
        .expect("spotify probe must be present");
    assert!(!spotify.available);
    assert!(spotify.note.to_lowercase().contains("no spotify_track_id"));
}

#[tokio::test]
async fn probe_reports_genius_skipped_when_no_token() {
    let report = probe_sources_impl(
        None,
        std::path::Path::new("/nonexistent/ytdlp"),
        std::path::Path::new("/tmp"),
        &reqwest::Client::new(),
        &fixture_row("Song", "Artist", None),
        "", // empty token
    )
    .await;
    let genius = report
        .probes
        .iter()
        .find(|p| p.provider == "genius")
        .expect("genius probe must be present");
    assert!(!genius.available);
    assert!(genius.note.to_lowercase().contains("no genius token") || genius.note.to_lowercase().contains("skipped"));
}

#[tokio::test]
async fn probe_reports_yt_subs_and_description_skipped_when_ytdlp_unavailable() {
    let report = probe_sources_impl(
        None,
        std::path::Path::new("/nonexistent/ytdlp"),
        std::path::Path::new("/tmp"),
        &reqwest::Client::new(),
        &fixture_row("Song", "Artist", None),
        "",
    )
    .await;
    let yt_subs = report.probes.iter().find(|p| p.provider == "yt_subs").unwrap();
    let descr = report.probes.iter().find(|p| p.provider == "description").unwrap();
    assert!(!yt_subs.available);
    assert!(!descr.available);
}
```

- [ ] **Step 2: Run tests, verify FAIL**

These tests will not compile yet because `probe.rs` does not exist. Expected failure: `unresolved module probe`. That counts as RED for TDD; the test code is staged as the failing assertion.

- [ ] **Step 3: Implement `probe.rs`**

```rust
//! Per-provider text-source availability probe.
//!
//! Returns a per-provider `available` bool + line-count estimate for the
//! six text-source providers used by `gather_sources_impl`. Probes do NOT
//! invoke Claude cleanup or alignment — they reuse each provider's cheap
//! raw-fetch fn so the operator can pre-flight a song before burning
//! Demucs/Gemini cycles in the lyrics worker.
//!
//! Wired into `POST /api/v1/lyrics/probe-sources` (see `api/lyrics.rs`).
//! Design spec: `docs/superpowers/specs/2026-05-13-probe-sources-reprocess-fix-design.md`.

use serde::Serialize;
use std::path::Path;
use tracing::debug;

use crate::lyrics::{
    description_provider::fetch_raw_description, genius, lrclib, lyrics_ovh,
    spotify_proxy::SpotifyLyricsFetcher, youtube_subs,
};

#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    pub provider: String,
    pub available: bool,
    pub line_count: usize,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProbeReport {
    pub video_id: i64,
    pub youtube_id: String,
    pub song: String,
    pub artist: String,
    pub probes: Vec<ProbeResult>,
    pub any_text_source: bool,
    pub recommendation: String,
}

/// Probe all six text-source providers without invoking Claude cleanup.
///
/// `ai_client` is accepted for signature symmetry with `gather_sources_impl`
/// but is unused (no Claude call from the probe path).
#[cfg_attr(test, mutants::skip)]
pub async fn probe_sources_impl(
    _ai_client: Option<&crate::ai::client::AiClient>,
    ytdlp_path: &Path,
    cache_dir: &Path,
    client: &reqwest::Client,
    row: &crate::db::models::VideoLyricsRow,
    genius_access_token: &str,
) -> ProbeReport {
    let youtube_id = row.youtube_id.clone();
    let mut probes: Vec<ProbeResult> = Vec::with_capacity(6);

    // 1. yt_subs (manual only — autosub is banned)
    let yt_tmp = std::env::temp_dir().join("sp_yt_subs_probe");
    let _ = tokio::fs::create_dir_all(&yt_tmp).await;
    let yt_subs_result = match youtube_subs::fetch_subtitles(ytdlp_path, &youtube_id, &yt_tmp).await {
        Ok(Some(track)) => ProbeResult {
            provider: "yt_subs".into(),
            available: true,
            line_count: track.lines.len(),
            note: format!("manual captions hit ({} lines)", track.lines.len()),
        },
        Ok(None) => ProbeResult {
            provider: "yt_subs".into(),
            available: false,
            line_count: 0,
            note: "no manual captions".into(),
        },
        Err(e) => ProbeResult {
            provider: "yt_subs".into(),
            available: false,
            line_count: 0,
            note: format!("error: {e}"),
        },
    };
    probes.push(yt_subs_result);

    // 2. description (raw fetch only, no Claude)
    let descr_result = match fetch_raw_description(ytdlp_path, &youtube_id, cache_dir).await {
        Ok(Some(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                ProbeResult {
                    provider: "description".into(),
                    available: false,
                    line_count: 0,
                    note: "description present but empty".into(),
                }
            } else {
                let lc = trimmed.lines().filter(|l| !l.trim().is_empty()).count();
                ProbeResult {
                    provider: "description".into(),
                    available: true,
                    line_count: lc,
                    note: format!("description has {lc} non-empty lines"),
                }
            }
        }
        Ok(None) => ProbeResult {
            provider: "description".into(),
            available: false,
            line_count: 0,
            note: "yt-dlp failed".into(),
        },
        Err(e) => ProbeResult {
            provider: "description".into(),
            available: false,
            line_count: 0,
            note: format!("error: {e}"),
        },
    };
    probes.push(descr_result);

    // 3. lyrics.ovh
    let lyrics_ovh_result = if row.song.is_empty() || row.artist.is_empty() {
        ProbeResult {
            provider: "lyrics_ovh".into(),
            available: false,
            line_count: 0,
            note: "skipped (song or artist empty)".into(),
        }
    } else {
        match lyrics_ovh::fetch_lyrics(client, &row.artist, &row.song).await {
            Ok(Some(lines)) => ProbeResult {
                provider: "lyrics_ovh".into(),
                available: true,
                line_count: lines.len(),
                note: format!("hit ({} lines)", lines.len()),
            },
            Ok(None) => ProbeResult {
                provider: "lyrics_ovh".into(),
                available: false,
                line_count: 0,
                note: "no lyrics found".into(),
            },
            Err(e) => ProbeResult {
                provider: "lyrics_ovh".into(),
                available: false,
                line_count: 0,
                note: format!("error: {e}"),
            },
        }
    };
    probes.push(lyrics_ovh_result);

    // 4. genius
    let genius_result = if genius_access_token.is_empty() {
        ProbeResult {
            provider: "genius".into(),
            available: false,
            line_count: 0,
            note: "no genius token configured".into(),
        }
    } else if row.song.is_empty() || row.artist.is_empty() {
        ProbeResult {
            provider: "genius".into(),
            available: false,
            line_count: 0,
            note: "skipped (song or artist empty)".into(),
        }
    } else {
        match genius::fetch_lyrics(client, genius_access_token, &row.artist, &row.song).await {
            Ok(Some(track)) => ProbeResult {
                provider: "genius".into(),
                available: true,
                line_count: track.lines.len(),
                note: format!("hit ({} lines)", track.lines.len()),
            },
            Ok(None) => ProbeResult {
                provider: "genius".into(),
                available: false,
                line_count: 0,
                note: "not found".into(),
            },
            Err(e) => ProbeResult {
                provider: "genius".into(),
                available: false,
                line_count: 0,
                note: format!("error: {e}"),
            },
        }
    };
    probes.push(genius_result);

    // 5. lrclib
    let lrclib_result = if row.song.is_empty() || row.artist.is_empty() {
        ProbeResult {
            provider: "lrclib".into(),
            available: false,
            line_count: 0,
            note: "skipped (song or artist empty)".into(),
        }
    } else {
        let dur = row.duration_ms.map(|ms| (ms / 1000) as u32).unwrap_or(0);
        match lrclib::fetch_lyrics(client, &row.artist, &row.song, dur).await {
            Ok(Some(track)) => {
                let has_real_timing = track.lines.iter().any(|l| l.end_ms > 0);
                ProbeResult {
                    provider: "lrclib".into(),
                    available: true,
                    line_count: track.lines.len(),
                    note: format!(
                        "hit ({} lines, {})",
                        track.lines.len(),
                        if has_real_timing { "synced timing" } else { "plain text only" }
                    ),
                }
            }
            Ok(None) => ProbeResult {
                provider: "lrclib".into(),
                available: false,
                line_count: 0,
                note: "not found".into(),
            },
            Err(e) => ProbeResult {
                provider: "lrclib".into(),
                available: false,
                line_count: 0,
                note: format!("error: {e}"),
            },
        }
    };
    probes.push(lrclib_result);

    // 6. spotify (only if row has spotify_track_id)
    let spotify_result = if let Some(tid) = row.spotify_track_id.as_deref() {
        let fetcher = SpotifyLyricsFetcher::new();
        match fetcher.fetch(tid).await {
            Ok(Some(track)) => ProbeResult {
                provider: "spotify".into(),
                available: true,
                line_count: track.lines.len(),
                note: format!("hit ({} lines)", track.lines.len()),
            },
            Ok(None) => ProbeResult {
                provider: "spotify".into(),
                available: false,
                line_count: 0,
                note: "no synced lyrics".into(),
            },
            Err(e) => ProbeResult {
                provider: "spotify".into(),
                available: false,
                line_count: 0,
                note: format!("error: {e}"),
            },
        }
    } else {
        ProbeResult {
            provider: "spotify".into(),
            available: false,
            line_count: 0,
            note: "no spotify_track_id on row".into(),
        }
    };
    probes.push(spotify_result);

    let any_text_source = probes.iter().any(|p| p.available);
    let recommendation = if any_text_source {
        "proceed".to_string()
    } else {
        "skip_no_text_source".to_string()
    };

    debug!(
        video_id = row.id,
        youtube_id = %youtube_id,
        any_text_source,
        "probe: complete"
    );

    ProbeReport {
        video_id: row.id,
        youtube_id,
        song: row.song.clone(),
        artist: row.artist.clone(),
        probes,
        any_text_source,
        recommendation,
    }
}
```

- [ ] **Step 4: Wire into `crates/sp-server/src/lyrics/mod.rs`**

Add near other `pub mod` declarations:

```rust
pub mod probe;

#[cfg(test)]
mod probe_tests;
```

- [ ] **Step 5: `cargo fmt --all --check`. Commit.**

```bash
cargo fmt --all --check
git add crates/sp-server/src/lyrics/probe.rs crates/sp-server/src/lyrics/probe_tests.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "$(cat <<'EOF'
feat(lyrics): probe.rs module + 4 sibling tests

Probes all six text-source providers (yt_subs, description, lyrics.ovh,
genius, lrclib, spotify_proxy) without invoking Claude cleanup or
alignment. Each provider's existing raw-fetch fn is reused so probe is
cheap.

Returns per-provider ProbeResult + any_text_source bool + recommendation
("proceed" or "skip_no_text_source").

ai_client parameter accepted for signature symmetry with
gather_sources_impl but unused — probe path never calls Claude.

Spec: docs/superpowers/specs/2026-05-13-probe-sources-reprocess-fix-design.md
EOF
)"
```

---

### Task A.2: HTTP handler + types + route wire

**Files:**
- Modify: `crates/sp-server/src/api/lyrics.rs`
- Modify: `crates/sp-server/src/api/mod.rs`

- [ ] **Step 1: Add failing handler integration test in api/lyrics.rs test mod**

Look for the existing tests at the bottom of `api/lyrics.rs` (`#[cfg(test)] mod tests`). Add this test after the existing quarantine tests:

```rust
#[tokio::test]
async fn probe_sources_returns_404_for_missing_video_id() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let (state, _temp) = test_state_with_cache_dir().await;
    let app = crate::api::routes::build_router(state);

    let req = Request::builder()
        .uri("/api/v1/lyrics/probe-sources")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"video_id": 99999}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn probe_sources_returns_report_with_six_probes_for_known_video() {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let (state, _temp) = test_state_with_cache_dir().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'p', 'u', 'n', 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, title, song, artist, normalized) \
         VALUES (5, 1, 'ytidX', 't', 'song', 'artist', 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let app = crate::api::routes::build_router(state);
    let req = Request::builder()
        .uri("/api/v1/lyrics/probe-sources")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"video_id": 5}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(json["video_id"], 5);
    assert_eq!(json["youtube_id"], "ytidX");
    let probes = json["probes"].as_array().expect("probes array");
    assert_eq!(probes.len(), 6);
    let provider_names: Vec<&str> = probes
        .iter()
        .map(|p| p["provider"].as_str().unwrap())
        .collect();
    for expected in ["yt_subs", "description", "lyrics_ovh", "genius", "lrclib", "spotify"] {
        assert!(provider_names.contains(&expected), "missing {expected}");
    }
}
```

- [ ] **Step 2: Run tests, confirm FAIL (route not registered, handler missing)**

- [ ] **Step 3: Implement handler + types in `api/lyrics.rs`**

Add at the bottom of the file (before `#[cfg(test)] mod tests` block):

```rust
#[derive(Debug, Deserialize)]
pub struct ProbeRequest {
    pub video_id: i64,
}

// HTTP handler: dispatches to lyrics::probe::probe_sources_impl. Behavior
// (per-provider availability) is covered by probe_tests.rs unit tests;
// this handler is thin Axum glue + the 404 / 200 + JSON-shape integration
// tests in this file's test module.
#[cfg_attr(test, mutants::skip)]
pub async fn post_probe_sources(
    State(state): State<AppState>,
    Json(req): Json<ProbeRequest>,
) -> impl IntoResponse {
    // Load the video row.
    let row_opt: Option<crate::db::models::VideoLyricsRow> = sqlx::query_as::<_, crate::db::models::VideoLyricsRow>(
        "SELECT v.id, v.youtube_id, COALESCE(v.song, '') AS song, \
                COALESCE(v.artist, '') AS artist, v.duration_ms, v.audio_file_path, \
                p.youtube_url, v.lyrics_override_text, v.lyrics_time_offset_ms, \
                v.spotify_track_id, v.spotify_resolved_at \
         FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE v.id = ?",
    )
    .bind(req.video_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let Some(row) = row_opt else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // Pull ytdlp path + genius token (best-effort; probe degrades gracefully).
    let ytdlp_path = state
        .tool_paths
        .read()
        .await
        .as_ref()
        .map(|tp| tp.ytdlp.clone())
        .unwrap_or_else(|| std::path::PathBuf::from("/nonexistent/ytdlp"));
    let genius_token =
        crate::db::models::get_setting(&state.pool, "genius_access_token")
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
    let client = reqwest::Client::new();

    let report = crate::lyrics::probe::probe_sources_impl(
        Some(&state.ai_client),
        &ytdlp_path,
        &state.cache_dir,
        &client,
        &row,
        &genius_token,
    )
    .await;

    Json(report).into_response()
}
```

- [ ] **Step 4: Wire route in `api/mod.rs`**

Find the existing lyrics routes block (around the quarantine route). Add after it:

```rust
            "/api/v1/lyrics/probe-sources",
            axum::routing::post(crate::api::lyrics::post_probe_sources),
        )
        .route(
```

(Match the existing `.route(` chain pattern exactly — copy the quarantine block as template.)

- [ ] **Step 5: `cargo fmt --all --check`. Commit.**

```bash
cargo fmt --all --check
git add crates/sp-server/src/api/lyrics.rs crates/sp-server/src/api/mod.rs
git commit -m "$(cat <<'EOF'
feat(api): POST /api/v1/lyrics/probe-sources

Thin Axum handler dispatching to lyrics::probe::probe_sources_impl.
Loads video row, pulls ytdlp path + genius token from AppState/settings,
runs the probe, returns ProbeReport JSON. 404 on unknown video_id.

Behavior: see lyrics/probe.rs sibling tests. Handler glue covered by
404-missing-id and 200-with-6-probes integration tests.

Spec: docs/superpowers/specs/2026-05-13-probe-sources-reprocess-fix-design.md
EOF
)"
```

---

## Phase B — Reprocess SQL fix

### Task B.1: Clear `lyrics_source` for auto-failure states on reprocess

**Files:**
- Modify: `crates/sp-server/src/api/lyrics.rs` (handler SQL + test mod)

- [ ] **Step 1: Add failing test in api/lyrics.rs test mod**

```rust
#[tokio::test]
async fn reprocess_clears_lyrics_source_for_no_source_failed_empty_states() {
    let (state, _temp) = test_state_with_cache_dir().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'p', 'u', 'n', 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized, lyrics_source) \
         VALUES (10, 1, 'y10', 's', 'a', 1, 'no_source'), \
                (11, 1, 'y11', 's', 'a', 1, 'failed'), \
                (12, 1, 'y12', 's', 'a', 1, 'empty'), \
                (13, 1, 'y13', 's', 'a', 1, 'asr_gap'), \
                (14, 1, 'y14', 's', 'a', 1, 'yt_subs')",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let app = crate::api::routes::build_router(state.clone());
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    let req = Request::builder()
        .uri("/api/v1/lyrics/reprocess")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"video_ids":[10,11,12,13,14]}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    // After reprocess: 10/11/12 should have NULL lyrics_source; 13 (asr_gap)
    // and 14 (yt_subs) should be untouched. ALL FIVE should have manual_priority=1.
    let rows: Vec<(i64, Option<String>, i64)> = sqlx::query_as(
        "SELECT id, lyrics_source, lyrics_manual_priority FROM videos ORDER BY id"
    )
    .fetch_all(&state.pool)
    .await
    .unwrap();
    let expected = vec![
        (10i64, None, 1i64),
        (11, None, 1),
        (12, None, 1),
        (13, Some("asr_gap".into()), 1), // untouched
        (14, Some("yt_subs".into()), 1), // untouched
    ];
    assert_eq!(rows, expected);
}
```

- [ ] **Step 2: Run, confirm FAIL** (current SQL only sets `manual_priority=1`, doesn't clear `lyrics_source`)

- [ ] **Step 3: Modify `post_reprocess` SQL — both video_ids and playlist_id variants**

Replace the existing `UPDATE videos SET lyrics_manual_priority = 1 WHERE id IN (...)` (and `WHERE playlist_id = ?` variant) with:

```rust
// video_ids variant
let sql = format!(
    "UPDATE videos SET lyrics_manual_priority = 1, \
            lyrics_source = CASE \
                WHEN lyrics_source IN ('failed', 'empty', 'no_source') THEN NULL \
                ELSE lyrics_source \
            END \
     WHERE id IN ({})",
    placeholders.join(",")
);
```

```rust
// playlist_id variant
sqlx::query(
    "UPDATE videos SET lyrics_manual_priority = 1, \
            lyrics_source = CASE \
                WHEN lyrics_source IN ('failed', 'empty', 'no_source') THEN NULL \
                ELSE lyrics_source \
            END \
     WHERE playlist_id = ?",
)
```

Note: `asr_gap` is intentionally OMITTED from the CASE — those rows are operator-parked and must not retry without a separate dequarantine flow (per the quarantine spec + Iron Rule #1).

- [ ] **Step 4: Run, confirm GREEN**

- [ ] **Step 5: `cargo fmt --all --check`. Commit.**

```bash
cargo fmt --all --check
git add crates/sp-server/src/api/lyrics.rs
git commit -m "$(cat <<'EOF'
fix(lyrics): reprocess clears lyrics_source for ('failed','empty','no_source')

Previously /api/v1/lyrics/reprocess only set manual_priority=1, leaving
lyrics_source intact. With lyrics_source='no_source' at pipeline_version=
current, the worker's fetch_bucket_manual SQL excluded the row via the
NOT IN ('failed','empty','no_source','asr_gap') filter — manual_priority
was set but the row never got popped.

Fix: CASE-clear lyrics_source to NULL for the three auto-failure states
on reprocess. Leaves 'asr_gap' (operator quarantine sentinel) and
already-successful sources untouched.

Spec: docs/superpowers/specs/2026-05-13-probe-sources-reprocess-fix-design.md
EOF
)"
```

---

## Phase C — Queue counts SQL alignment

### Task C.1: Align bucket0 + bucket1 count SQL with worker pop SQL

**Files:**
- Modify: `crates/sp-server/src/api/lyrics.rs` (`fetch_queue_counts` + handler thread + test mod)

- [ ] **Step 1: Add failing test in api/lyrics.rs test mod**

```rust
#[tokio::test]
async fn queue_counts_exclude_failed_states_at_current_version() {
    use crate::api::lyrics::fetch_queue_counts;
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'p', 'u', 'n', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // bucket0 candidates (manual_priority=1):
    //   - id=20 no_source at current pv → SHOULD BE EXCLUDED (worker can't pop)
    //   - id=21 asr_gap at current pv   → SHOULD BE EXCLUDED
    //   - id=22 no_source at OLDER pv   → INCLUDED (version-bump exception)
    //   - id=23 lyrics_source=NULL      → INCLUDED
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized, has_lyrics, lyrics_manual_priority, lyrics_source, lyrics_pipeline_version) \
         VALUES (20, 1, 'y20', 's', 'a', 1, 0, 1, 'no_source', 7), \
                (21, 1, 'y21', 's', 'a', 1, 0, 1, 'asr_gap',  7), \
                (22, 1, 'y22', 's', 'a', 1, 0, 1, 'no_source', 5), \
                (23, 1, 'y23', 's', 'a', 1, 0, 1, NULL,        7)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (b0, _b1, _b2) = fetch_queue_counts(&pool, 7).await.unwrap();
    assert_eq!(b0, 2, "bucket0 should include only id=22 (older pv) + id=23 (NULL source)");
}

#[tokio::test]
async fn queue_bucket1_excludes_failed_states_at_current_version() {
    use crate::api::lyrics::fetch_queue_counts;
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'p', 'u', 'n', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // bucket1 candidates (manual_priority=0, has_lyrics=0):
    //   - id=30 no_source at current pv → EXCLUDED
    //   - id=31 no_source at older pv   → INCLUDED
    //   - id=32 lyrics_source=NULL      → INCLUDED
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized, has_lyrics, lyrics_manual_priority, lyrics_source, lyrics_pipeline_version) \
         VALUES (30, 1, 'y30', 's', 'a', 1, 0, 0, 'no_source', 7), \
                (31, 1, 'y31', 's', 'a', 1, 0, 0, 'no_source', 5), \
                (32, 1, 'y32', 's', 'a', 1, 0, 0, NULL,        7)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (_b0, b1, _b2) = fetch_queue_counts(&pool, 7).await.unwrap();
    assert_eq!(b1, 2, "bucket1 should include only id=31 (older pv) + id=32 (NULL source)");
}
```

- [ ] **Step 2: Run, confirm FAIL** (current `fetch_queue_counts` has no source-state filter on bucket0/bucket1)

- [ ] **Step 3: Modify `fetch_queue_counts`**

Replace the existing bucket0 SQL with:

```rust
let b0: i64 = sqlx::query_scalar(
    "SELECT COUNT(*) FROM videos v JOIN playlists p ON p.id = v.playlist_id \
     WHERE v.lyrics_manual_priority = 1 \
           AND (v.lyrics_source IS NULL \
                OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source', 'asr_gap') \
                OR v.lyrics_pipeline_version < ?) \
           AND p.is_active = 1 AND v.normalized = 1",
)
.bind(current_version as i64)
.fetch_one(pool)
.await?;
```

Replace bucket1 SQL with:

```rust
let b1: i64 = sqlx::query_scalar(
    "SELECT COUNT(*) FROM videos v JOIN playlists p ON p.id = v.playlist_id \
     WHERE (v.has_lyrics IS NULL OR v.has_lyrics = 0) \
           AND (v.lyrics_source IS NULL \
                OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source', 'asr_gap') \
                OR v.lyrics_pipeline_version < ?) \
           AND v.lyrics_manual_priority = 0 \
           AND p.is_active = 1 AND v.normalized = 1",
)
.bind(current_version as i64)
.fetch_one(pool)
.await?;
```

Leave bucket2 SQL unchanged (already current-version-aware).

- [ ] **Step 4: Run, confirm GREEN**

- [ ] **Step 5: `cargo fmt --all --check`. Commit.**

```bash
cargo fmt --all --check
git add crates/sp-server/src/api/lyrics.rs
git commit -m "$(cat <<'EOF'
fix(lyrics): queue counts align with worker-pop source-state filter

Previously /api/v1/lyrics/queue bucket0 + bucket1 counts had NO
lyrics_source filter, while the worker's fetch_bucket_{manual,null}
SQL excludes rows where lyrics_source IN ('failed','empty','no_source',
'asr_gap') AND pipeline_version >= current. Dashboard reported 60 songs
'queued' that worker could never consume.

Fix: bucket0 + bucket1 count SQL now matches the worker pop filter so
the dashboard reports rows the worker can actually pick up. Bucket2
unchanged (already version-aware).

Spec: docs/superpowers/specs/2026-05-13-probe-sources-reprocess-fix-design.md
EOF
)"
```

---

## Phase D — Skill update + memory + push

### Task D.1: Update lyrics-verify skill + memory + push

**Files:**
- Modify: `~/.claude/skills/lyrics-verify/SKILL.md`
- Create: `~/.claude/projects/-home-newlevel-devel-songplayer/memory/feedback_preflight_text_source.md`
- Modify: `~/.claude/projects/-home-newlevel-devel-songplayer/memory/MEMORY.md`

- [ ] **Step 1: Skill — add Phase 0.5 between Phase 1 and Phase 2**

Open `~/.claude/skills/lyrics-verify/SKILL.md`. After the `## Phase 1 — Pick ONE song` section and before `## Phase 2 — Reprocess this one song`, insert:

```
---

## Phase 1.5 — Pre-flight text-source probe (MANDATORY)

Before reprocessing, probe whether the song has ANY text source. A song with zero text sources will fall back to whisperx-only ASR alignment, which is weak STT and a waste of the loop. User has explicitly banned spending iterations on these songs.

```bash
curl -s -X POST http://10.77.9.201:8920/api/v1/lyrics/probe-sources \
  -H 'content-type: application/json' \
  -d '{"video_id":<video_id>}' | jq '.'
```

Response includes:
- `probes[]` — per-provider availability (yt_subs / description / lyrics_ovh / genius / lrclib / spotify)
- `any_text_source` — bool
- `recommendation` — `"proceed"` or `"skip_no_text_source"`

**Decision:**

- `any_text_source == true` → proceed to Phase 2 (reprocess)
- `any_text_source == false` → SKIP silently. Log one line: `Song <id>: <song> — <artist> — NO TEXT SOURCE → skipped`. Pick next song. Do NOT reprocess; do NOT invoke quarantine (quarantine is for ASR failures, not for missing-text-source — these songs are deferred until a new text-source provider lands or a future ASR upgrade).
```

- [ ] **Step 2: Skill — add Iron Rule #21**

```
21. **ALWAYS probe text-source availability before reprocess.** Phase 1.5 `/api/v1/lyrics/probe-sources` is mandatory. If `any_text_source == false` → SKIP silently. Don't burn Demucs / Gemini cycles on whisperx-only songs. (`feedback_preflight_text_source.md`)
```

- [ ] **Step 3: Memory — create `feedback_preflight_text_source.md`**

```markdown
---
name: Pre-flight text-source probe before reprocess
description: Always probe /api/v1/lyrics/probe-sources before reprocessing a song; skip silently when any_text_source=false (no Demucs/Gemini cycles on whisperx-only songs)
type: feedback
---

Before reprocessing any song in `/lyrics-verify` Phase 2, ALWAYS run the pre-flight probe via `POST /api/v1/lyrics/probe-sources` and check `any_text_source`.

- `true` → proceed to reprocess
- `false` → skip silently. Log one line, move to next song. Don't reprocess; don't quarantine.

**Why:** WhisperX-only alignment (pure ASR with no text reference) produces weak output on sung music. User has explicitly classified these songs as not-worth-the-cycle: "I don't like in this phase work on songs which has no lyrics at all available and it will be fully based on whisperx weak STT." Reprocessing such songs burns Demucs (~30s) + Gemini chunks (~1-2min) and ends with no_source anyway. Pre-flight check saves the cycle.

**How to apply:** Phase 1.5 of `/lyrics-verify` SKILL.md (Iron Rule #21). Probe runs all six text-source providers (yt_subs / description / lyrics_ovh / genius / lrclib / spotify) cheap-fetch only (no Claude cleanup, no alignment). Returns per-provider availability + `any_text_source` aggregate + `recommendation`.

Established 2026-05-13 after user said "i need to somehow clarify before process if that song has any avaiable source of lyrics as i dont like in this phase work on songs which has no lyrics at all avaiable and it will be fully based on whisperx week stt".
```

- [ ] **Step 4: MEMORY.md — add index entry**

Append to `MEMORY.md`:

```
- [Pre-flight text-source probe](feedback_preflight_text_source.md) — Always run /api/v1/lyrics/probe-sources before reprocess; skip silently when any_text_source=false (no whisperx-only songs)
```

- [ ] **Step 5: Push the songplayer dev branch**

```bash
cd /home/newlevel/devel/songplayer
git push origin dev
```

- [ ] **Step 6: Monitor CI per `ci-monitoring.md`**

```bash
gh run list --branch dev --limit 1 --json databaseId,headSha,status,conclusion
# Then background sleep + gh run view per the rule
```

Wait for ALL jobs green (Test, Lint, Security, Build WASM, Build Tauri, Deploy to win-resolume, E2E). If ANY fail → investigate via `gh run view --log-failed`, fix root cause, push fix, monitor again.

- [ ] **Step 7: Post-deploy probe verification**

After Deploy succeeds, probe a known-good song to verify endpoint live:

```bash
curl -s -X POST http://10.77.9.201:8920/api/v1/lyrics/probe-sources \
  -H 'content-type: application/json' \
  -d '{"video_id":81}' | jq '.'
```

Confirm response shape matches spec. Then reprocess id=81 (or test on a song with manual_priority=1 from earlier) and confirm worker picks it up within 10s tick.

---

## Verification

After all tasks:

1. `cargo fmt --all --check` passes
2. CI green on dev (all jobs)
3. `POST /api/v1/lyrics/probe-sources {"video_id":81}` returns 200 with 6-probe JSON
4. `POST /api/v1/lyrics/reprocess {"video_ids":[<no_source row>]}` clears `lyrics_source` to NULL and worker picks it up within 10s
5. `GET /api/v1/lyrics/queue` counts match worker-pop eligibility
6. `/lyrics-verify` skill has Phase 1.5 + Iron Rule #21
7. New memory `feedback_preflight_text_source.md` exists, indexed in MEMORY.md

Open PR from `dev` to `main`. Wait for explicit user merge instruction per `pr-merge-policy.md`.
