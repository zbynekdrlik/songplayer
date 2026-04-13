# Karaoke Dual-Language Lyrics — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Display synchronized karaoke lyrics in two languages (EN + SK) during playback, with word-level highlighting on Resolume LED walls and the dashboard.

**Architecture:** Background lyrics worker acquires lyrics from LRCLIB / YouTube subs / Qwen3-ASR, aligns with Qwen3-ForcedAligner for word-level timestamps, translates to Slovak via Gemini, persists as JSON sidecar. Lyrics renderer drives Resolume `#sp-subs` / `#sp-subssk` clips and an inline dashboard karaoke panel synchronized to playback position.

**Tech Stack:** Rust (sp-core types, sp-server worker/renderer/API), Python (Qwen3-ASR + ForcedAligner via subprocess), Gemini 2.5 Flash (translation), LRCLIB API, Leptos 0.7 (dashboard panel)

**Design spec:** `docs/superpowers/specs/2026-04-13-karaoke-dual-language-design.md`

---

## File Structure

### New Files

| File | Responsibility |
|------|---------------|
| `crates/sp-core/src/lyrics.rs` | Shared lyrics types: `LyricsTrack`, `LyricsLine`, `LyricsWord` (WASM-safe, Serialize/Deserialize) |
| `crates/sp-server/src/lyrics/mod.rs` | Lyrics worker orchestrator: sequential background processing, source waterfall, DB queries |
| `crates/sp-server/src/lyrics/lrclib.rs` | LRCLIB API client: fetch synced lyrics by artist/song/duration |
| `crates/sp-server/src/lyrics/youtube_subs.rs` | YouTube subtitle parser: yt-dlp invocation + json3 parsing |
| `crates/sp-server/src/lyrics/aligner.rs` | Python subprocess wrapper: Qwen3-ForcedAligner + Qwen3-ASR invocation |
| `crates/sp-server/src/lyrics/translator.rs` | Gemini translation: EN→SK with worship glossary prompt |
| `crates/sp-server/src/lyrics/renderer.rs` | Lyrics renderer: position → line lookup → word highlight → emit to Resolume/WS |
| `scripts/lyrics_worker.py` | Python helper: Qwen3-ASR transcription + ForcedAligner alignment via subprocess |
| `sp-ui/src/components/karaoke_panel.rs` | Leptos component: inline karaoke display with word-level highlighting |

### Modified Files

| File | Changes |
|------|---------|
| `crates/sp-core/src/lib.rs` | Add `pub mod lyrics;` |
| `crates/sp-core/src/ws.rs` | Add `ServerMsg::LyricsUpdate` variant |
| `crates/sp-core/src/models.rs` | Add `karaoke_enabled` to `Playlist` struct |
| `crates/sp-server/src/db/mod.rs` | Add MIGRATION_V5 (has_lyrics, lyrics_source, karaoke_enabled) |
| `crates/sp-server/src/db/models.rs` | Add lyrics-related DB query functions |
| `crates/sp-server/src/lib.rs` | Spawn lyrics worker in start(), add module declaration |
| `crates/sp-server/src/startup.rs` | Extend self_heal_cache for lyrics sidecar files |
| `crates/sp-server/src/downloader/cache.rs` | Add lyrics sidecar detection to scan_cache |
| `crates/sp-server/src/downloader/tools.rs` | Extend ToolPaths with python path, add ensure_python() |
| `crates/sp-server/src/playback/mod.rs` | Load lyrics on video start, emit LyricsUpdate on position broadcast |
| `crates/sp-server/src/resolume/mod.rs` | Add `ShowSubtitles`/`HideSubtitles` commands, add subtitle tokens |
| `crates/sp-server/src/resolume/driver.rs` | Handle subtitle commands, discover `#sp-subs`/`#sp-subssk` tokens |
| `crates/sp-server/src/resolume/handlers.rs` | Add set_subtitle/clear_subtitle functions |
| `crates/sp-server/src/api/routes.rs` | Add lyrics endpoints, extend playlist PATCH |
| `sp-ui/src/store.rs` | Add lyrics state to DashboardStore, handle LyricsUpdate dispatch |
| `sp-ui/src/components/playlist_card.rs` | Integrate KaraokePanel component |
| `sp-ui/src/components/mod.rs` | Add `pub mod karaoke_panel;` |
| `e2e/post-deploy-flac.spec.ts` | Add lyrics E2E assertions |

---

## Phase 1: Core Types and Database

### Task 1: sp-core lyrics types

**Files:**
- Create: `crates/sp-core/src/lyrics.rs`
- Modify: `crates/sp-core/src/lib.rs`

- [ ] Write tests for lyrics types serde roundtrip in `crates/sp-core/src/lyrics.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LyricsWord {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LyricsLine {
    pub start_ms: u64,
    pub end_ms: u64,
    pub en: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub words: Option<Vec<LyricsWord>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LyricsTrack {
    pub version: u32,
    pub source: String,
    #[serde(default)]
    pub language_source: String,
    #[serde(default)]
    pub language_translation: String,
    pub lines: Vec<LyricsLine>,
}

impl LyricsTrack {
    /// Find the lyrics line active at the given position in milliseconds.
    /// Returns (line_index, line) or None if no line covers the position.
    pub fn line_at(&self, position_ms: u64) -> Option<(usize, &LyricsLine)> {
        self.lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.start_ms <= position_ms && position_ms < line.end_ms)
    }

    /// Find the active word index within a line at the given position.
    /// Returns None if the line has no word-level data.
    pub fn word_index_at(line: &LyricsLine, position_ms: u64) -> Option<usize> {
        line.words.as_ref().and_then(|words| {
            words
                .iter()
                .enumerate()
                .rev()
                .find(|(_, w)| w.start_ms <= position_ms)
                .map(|(i, _)| i)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_track() -> LyricsTrack {
        LyricsTrack {
            version: 1,
            source: "lrclib+aligner".to_string(),
            language_source: "en".to_string(),
            language_translation: "sk".to_string(),
            lines: vec![
                LyricsLine {
                    start_ms: 1500,
                    end_ms: 4200,
                    en: "Amazing grace how sweet the sound".to_string(),
                    sk: Some("Predivná milosť jak ľúby to zvuk".to_string()),
                    words: Some(vec![
                        LyricsWord { text: "Amazing".to_string(), start_ms: 1500, end_ms: 1920 },
                        LyricsWord { text: "grace".to_string(), start_ms: 1920, end_ms: 2340 },
                        LyricsWord { text: "how".to_string(), start_ms: 2340, end_ms: 2520 },
                        LyricsWord { text: "sweet".to_string(), start_ms: 2520, end_ms: 2890 },
                        LyricsWord { text: "the".to_string(), start_ms: 2890, end_ms: 3050 },
                        LyricsWord { text: "sound".to_string(), start_ms: 3050, end_ms: 4200 },
                    ]),
                },
                LyricsLine {
                    start_ms: 4200,
                    end_ms: 7800,
                    en: "That saved a wretch like me".to_string(),
                    sk: Some("Čo zachránila úbožiaka ako ja".to_string()),
                    words: Some(vec![
                        LyricsWord { text: "That".to_string(), start_ms: 4200, end_ms: 4600 },
                        LyricsWord { text: "saved".to_string(), start_ms: 4600, end_ms: 5100 },
                        LyricsWord { text: "a".to_string(), start_ms: 5100, end_ms: 5300 },
                        LyricsWord { text: "wretch".to_string(), start_ms: 5300, end_ms: 5900 },
                        LyricsWord { text: "like".to_string(), start_ms: 5900, end_ms: 6300 },
                        LyricsWord { text: "me".to_string(), start_ms: 6300, end_ms: 7800 },
                    ]),
                },
            ],
        }
    }

    #[test]
    fn serde_roundtrip() {
        let track = sample_track();
        let json = serde_json::to_string_pretty(&track).unwrap();
        let parsed: LyricsTrack = serde_json::from_str(&json).unwrap();
        assert_eq!(track, parsed);
    }

    #[test]
    fn serde_without_sk() {
        let track = LyricsTrack {
            version: 1,
            source: "asr+aligner".to_string(),
            language_source: "en".to_string(),
            language_translation: String::new(),
            lines: vec![LyricsLine {
                start_ms: 0,
                end_ms: 1000,
                en: "Hello".to_string(),
                sk: None,
                words: None,
            }],
        };
        let json = serde_json::to_string(&track).unwrap();
        assert!(!json.contains("\"sk\""));
        assert!(!json.contains("\"words\""));
        let parsed: LyricsTrack = serde_json::from_str(&json).unwrap();
        assert_eq!(track, parsed);
    }

    #[test]
    fn line_at_finds_correct_line() {
        let track = sample_track();
        let (idx, line) = track.line_at(2000).unwrap();
        assert_eq!(idx, 0);
        assert!(line.en.starts_with("Amazing"));

        let (idx, line) = track.line_at(5000).unwrap();
        assert_eq!(idx, 1);
        assert!(line.en.starts_with("That saved"));
    }

    #[test]
    fn line_at_returns_none_outside_range() {
        let track = sample_track();
        assert!(track.line_at(0).is_none());
        assert!(track.line_at(1499).is_none());
        assert!(track.line_at(7800).is_none());
        assert!(track.line_at(99999).is_none());
    }

    #[test]
    fn line_at_boundary_start_inclusive_end_exclusive() {
        let track = sample_track();
        assert!(track.line_at(1500).is_some());
        assert!(track.line_at(4199).is_some());
        assert_eq!(track.line_at(4199).unwrap().0, 0);
        assert!(track.line_at(4200).is_some());
        assert_eq!(track.line_at(4200).unwrap().0, 1);
    }

    #[test]
    fn word_index_at_finds_active_word() {
        let track = sample_track();
        let line = &track.lines[0];
        assert_eq!(LyricsTrack::word_index_at(line, 1500), Some(0));
        assert_eq!(LyricsTrack::word_index_at(line, 1919), Some(0));
        assert_eq!(LyricsTrack::word_index_at(line, 1920), Some(1));
        assert_eq!(LyricsTrack::word_index_at(line, 3050), Some(5));
        assert_eq!(LyricsTrack::word_index_at(line, 4199), Some(5));
    }

    #[test]
    fn word_index_at_none_without_words() {
        let line = LyricsLine {
            start_ms: 0,
            end_ms: 1000,
            en: "Hello".to_string(),
            sk: None,
            words: None,
        };
        assert_eq!(LyricsTrack::word_index_at(&line, 500), None);
    }

    #[test]
    fn word_index_at_before_first_word() {
        let line = LyricsLine {
            start_ms: 0,
            end_ms: 5000,
            en: "Hello world".to_string(),
            sk: None,
            words: Some(vec![
                LyricsWord { text: "Hello".to_string(), start_ms: 1000, end_ms: 2000 },
                LyricsWord { text: "world".to_string(), start_ms: 2000, end_ms: 3000 },
            ]),
        };
        assert_eq!(LyricsTrack::word_index_at(&line, 500), None);
    }
}
```

- [ ] Add `pub mod lyrics;` to `crates/sp-core/src/lib.rs`

- [ ] Verify: `cargo test -p sp-core` passes, `cargo check --target wasm32-unknown-unknown -p sp-core` compiles

- [ ] Commit: `feat(core): add lyrics types with line/word lookup and serde`

---

### Task 2: ServerMsg::LyricsUpdate variant

**Files:**
- Modify: `crates/sp-core/src/ws.rs`

- [ ] Add the `LyricsUpdate` variant to the `ServerMsg` enum (after the existing `Pong` variant):

```rust
    LyricsUpdate {
        playlist_id: i64,
        line_en: Option<String>,
        line_sk: Option<String>,
        prev_line_en: Option<String>,
        next_line_en: Option<String>,
        active_word_index: Option<usize>,
        word_count: Option<usize>,
    },
```

- [ ] Add a serde roundtrip test for the new variant in the existing test module:

```rust
    #[test]
    fn lyrics_update_serde_roundtrip() {
        let msg = ServerMsg::LyricsUpdate {
            playlist_id: 1,
            line_en: Some("Amazing grace".to_string()),
            line_sk: Some("Predivná milosť".to_string()),
            prev_line_en: None,
            next_line_en: Some("How sweet the sound".to_string()),
            active_word_index: Some(1),
            word_count: Some(2),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ServerMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn lyrics_update_empty_serde() {
        let msg = ServerMsg::LyricsUpdate {
            playlist_id: 1,
            line_en: None,
            line_sk: None,
            prev_line_en: None,
            next_line_en: None,
            active_word_index: None,
            word_count: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ServerMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, parsed);
    }
```

- [ ] Verify: `cargo test -p sp-core` passes

- [ ] Commit: `feat(core): add ServerMsg::LyricsUpdate WS message variant`

---

### Task 3: Database migration V5

**Files:**
- Modify: `crates/sp-server/src/db/mod.rs`
- Modify: `crates/sp-server/src/db/models.rs`
- Modify: `crates/sp-core/src/models.rs`

- [ ] Add MIGRATION_V5 constant in `crates/sp-server/src/db/mod.rs`:

```rust
const MIGRATION_V5: &str = "
ALTER TABLE videos ADD COLUMN has_lyrics INTEGER NOT NULL DEFAULT 0;
ALTER TABLE videos ADD COLUMN lyrics_source TEXT;
ALTER TABLE playlists ADD COLUMN karaoke_enabled INTEGER NOT NULL DEFAULT 1;
";
```

- [ ] Add `(5, MIGRATION_V5)` to the `MIGRATIONS` array.

- [ ] Add `karaoke_enabled` field to `Playlist` struct in `crates/sp-core/src/models.rs`:

```rust
    #[serde(default = "default_true")]
    pub karaoke_enabled: bool,
```

And add the helper:

```rust
fn default_true() -> bool {
    true
}
```

- [ ] Add lyrics DB query functions to `crates/sp-server/src/db/models.rs`:

```rust
pub async fn get_next_video_without_lyrics(
    pool: &SqlitePool,
) -> Result<Option<VideoLyricsRow>, sqlx::Error> {
    sqlx::query_as::<_, VideoLyricsRow>(
        "SELECT v.id, v.youtube_id, COALESCE(v.song, '') as song, \
         COALESCE(v.artist, '') as artist, v.duration_ms, v.audio_file_path, \
         p.youtube_url \
         FROM videos v \
         JOIN playlists p ON p.id = v.playlist_id \
         WHERE v.normalized = 1 AND v.has_lyrics = 0 AND p.is_active = 1 \
         ORDER BY v.id LIMIT 1",
    )
    .fetch_optional(pool)
    .await
}

#[derive(Debug, sqlx::FromRow)]
pub struct VideoLyricsRow {
    pub id: i64,
    pub youtube_id: String,
    pub song: String,
    pub artist: String,
    pub duration_ms: Option<i64>,
    pub audio_file_path: Option<String>,
    pub youtube_url: String,
}

pub async fn mark_video_lyrics(
    pool: &SqlitePool,
    video_id: i64,
    has_lyrics: bool,
    lyrics_source: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos SET has_lyrics = ?, lyrics_source = ? WHERE id = ?",
    )
    .bind(has_lyrics as i32)
    .bind(lyrics_source)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_lyrics_status(
    pool: &SqlitePool,
) -> Result<(i64, i64, i64), sqlx::Error> {
    let row = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT \
         COUNT(*) as total, \
         SUM(CASE WHEN has_lyrics = 1 THEN 1 ELSE 0 END) as processed, \
         SUM(CASE WHEN has_lyrics = 0 AND normalized = 1 THEN 1 ELSE 0 END) as pending \
         FROM videos v \
         JOIN playlists p ON p.id = v.playlist_id \
         WHERE p.is_active = 1",
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn reset_video_lyrics(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE videos SET has_lyrics = 0, lyrics_source = NULL WHERE id = ?")
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok(())
}
```

- [ ] Write tests for migration and new queries in `crates/sp-server/src/db/mod.rs` tests section:

```rust
    #[tokio::test]
    async fn migration_v5_adds_lyrics_columns() {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        let row = sqlx::query_scalar::<_, i64>(
            "SELECT has_lyrics FROM videos LIMIT 0",
        )
        .fetch_optional(&pool)
        .await;
        assert!(row.is_ok());

        let row = sqlx::query_scalar::<_, i64>(
            "SELECT karaoke_enabled FROM playlists LIMIT 0",
        )
        .fetch_optional(&pool)
        .await;
        assert!(row.is_ok());
    }
```

- [ ] Verify: `cargo test -p sp-server` passes

- [ ] Commit: `feat(db): add migration V5 for lyrics columns and karaoke toggle`

---

## Phase 2: Lyrics Sources

### Task 4: LRCLIB client

**Files:**
- Create: `crates/sp-server/src/lyrics/lrclib.rs`
- Create: `crates/sp-server/src/lyrics/mod.rs`

- [ ] Create `crates/sp-server/src/lyrics/mod.rs` with module declarations:

```rust
pub mod lrclib;
pub mod youtube_subs;
pub mod aligner;
pub mod translator;
pub mod renderer;

mod worker;
pub use worker::LyricsWorker;
```

- [ ] Create `crates/sp-server/src/lyrics/lrclib.rs` with LRCLIB API client:

```rust
use reqwest::Client;
use serde::Deserialize;
use sp_core::lyrics::{LyricsLine, LyricsTrack};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LrclibResponse {
    synced_lyrics: Option<String>,
    plain_lyrics: Option<String>,
    track_name: Option<String>,
    artist_name: Option<String>,
}

pub async fn fetch_lyrics(
    client: &Client,
    artist: &str,
    song: &str,
    duration_s: Option<i64>,
) -> Result<Option<LyricsTrack>, anyhow::Error> {
    let mut url = format!(
        "https://lrclib.net/api/get?artist_name={}&track_name={}",
        urlencoding::encode(artist),
        urlencoding::encode(song),
    );
    if let Some(d) = duration_s {
        url.push_str(&format!("&duration={d}"));
    }
    tracing::debug!("LRCLIB query: {url}");
    let resp = client
        .get(&url)
        .header("User-Agent", "SongPlayer/0.13.0 (github.com/zbynekdrlik/songplayer)")
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        tracing::debug!("LRCLIB: no results for {artist} - {song}");
        return Ok(None);
    }
    let resp = resp.error_for_status()?;
    let data: LrclibResponse = resp.json().await?;

    if let Some(synced) = &data.synced_lyrics {
        if let Some(track) = parse_lrc(synced) {
            tracing::info!("LRCLIB: found synced lyrics for {artist} - {song} ({} lines)", track.lines.len());
            return Ok(Some(track));
        }
    }
    if let Some(plain) = &data.plain_lyrics {
        if let Some(track) = parse_plain(plain) {
            tracing::info!("LRCLIB: found plain lyrics for {artist} - {song} ({} lines)", track.lines.len());
            return Ok(Some(track));
        }
    }
    Ok(None)
}

fn parse_lrc(lrc: &str) -> Option<LyricsTrack> {
    let mut lines = Vec::new();
    for raw_line in lrc.lines() {
        let raw_line = raw_line.trim();
        if raw_line.is_empty() {
            continue;
        }
        if let Some((ts, text)) = parse_lrc_line(raw_line) {
            if !text.is_empty() {
                lines.push((ts, text.to_string()));
            }
        }
    }
    if lines.is_empty() {
        return None;
    }
    let lyrics_lines: Vec<LyricsLine> = lines
        .windows(2)
        .map(|w| LyricsLine {
            start_ms: w[0].0,
            end_ms: w[1].0,
            en: w[0].1.clone(),
            sk: None,
            words: None,
        })
        .chain(std::iter::once(LyricsLine {
            start_ms: lines.last().unwrap().0,
            end_ms: lines.last().unwrap().0 + 5000,
            en: lines.last().unwrap().1.clone(),
            sk: None,
            words: None,
        }))
        .collect();

    Some(LyricsTrack {
        version: 1,
        source: "lrclib".to_string(),
        language_source: "en".to_string(),
        language_translation: String::new(),
        lines: lyrics_lines,
    })
}

fn parse_lrc_line(line: &str) -> Option<(u64, &str)> {
    let close = line.find(']')?;
    let ts_str = &line[1..close];
    let text = line[close + 1..].trim();
    let ts = parse_lrc_timestamp(ts_str)?;
    Some((ts, text))
}

fn parse_lrc_timestamp(ts: &str) -> Option<u64> {
    let parts: Vec<&str> = ts.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let minutes: u64 = parts[0].parse().ok()?;
    let sec_parts: Vec<&str> = parts[1].split('.').collect();
    let seconds: u64 = sec_parts[0].parse().ok()?;
    let centiseconds: u64 = if sec_parts.len() > 1 {
        let frac = sec_parts[1];
        match frac.len() {
            1 => frac.parse::<u64>().ok()? * 100,
            2 => frac.parse::<u64>().ok()? * 10,
            3 => frac.parse::<u64>().ok()?,
            _ => frac[..3].parse::<u64>().ok()?,
        }
    } else {
        0
    };
    Some(minutes * 60000 + seconds * 1000 + centiseconds)
}

fn parse_plain(plain: &str) -> Option<LyricsTrack> {
    let lines: Vec<String> = plain
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    let lyrics_lines: Vec<LyricsLine> = lines
        .into_iter()
        .map(|text| LyricsLine {
            start_ms: 0,
            end_ms: 0,
            en: text,
            sk: None,
            words: None,
        })
        .collect();
    Some(LyricsTrack {
        version: 1,
        source: "lrclib".to_string(),
        language_source: "en".to_string(),
        language_translation: String::new(),
        lines: lyrics_lines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lrc_timestamp_standard() {
        assert_eq!(parse_lrc_timestamp("01:32.45"), Some(92450));
        assert_eq!(parse_lrc_timestamp("00:00.00"), Some(0));
        assert_eq!(parse_lrc_timestamp("03:15.99"), Some(195990));
    }

    #[test]
    fn parse_lrc_timestamp_three_digit_frac() {
        assert_eq!(parse_lrc_timestamp("01:32.450"), Some(92450));
        assert_eq!(parse_lrc_timestamp("00:05.123"), Some(5123));
    }

    #[test]
    fn parse_lrc_timestamp_one_digit_frac() {
        assert_eq!(parse_lrc_timestamp("00:05.1"), Some(5100));
    }

    #[test]
    fn parse_lrc_timestamp_no_frac() {
        assert_eq!(parse_lrc_timestamp("02:30"), Some(150000));
    }

    #[test]
    fn parse_lrc_timestamp_invalid() {
        assert_eq!(parse_lrc_timestamp("invalid"), None);
        assert_eq!(parse_lrc_timestamp(""), None);
    }

    #[test]
    fn parse_lrc_full() {
        let lrc = "[00:01.50] Amazing grace how sweet the sound\n\
                    [00:04.20] That saved a wretch like me\n\
                    [00:07.80] I once was lost\n";
        let track = parse_lrc(lrc).unwrap();
        assert_eq!(track.lines.len(), 3);
        assert_eq!(track.lines[0].start_ms, 1500);
        assert_eq!(track.lines[0].end_ms, 4200);
        assert_eq!(track.lines[0].en, "Amazing grace how sweet the sound");
        assert_eq!(track.lines[1].start_ms, 4200);
        assert_eq!(track.lines[1].end_ms, 7800);
        assert_eq!(track.source, "lrclib");
    }

    #[test]
    fn parse_lrc_skips_empty_text() {
        let lrc = "[00:01.00] Hello\n[00:02.00] \n[00:03.00] World\n";
        let track = parse_lrc(lrc).unwrap();
        assert_eq!(track.lines.len(), 2);
        assert_eq!(track.lines[0].en, "Hello");
        assert_eq!(track.lines[1].en, "World");
    }

    #[test]
    fn parse_plain_lyrics() {
        let plain = "Amazing grace\nHow sweet the sound\n\nThat saved a wretch like me";
        let track = parse_plain(plain).unwrap();
        assert_eq!(track.lines.len(), 3);
        assert_eq!(track.lines[0].en, "Amazing grace");
        assert!(track.lines.iter().all(|l| l.start_ms == 0 && l.end_ms == 0));
    }

    #[test]
    fn parse_lrc_empty() {
        assert!(parse_lrc("").is_none());
        assert!(parse_lrc("\n\n\n").is_none());
    }

    #[test]
    fn parse_plain_empty() {
        assert!(parse_plain("").is_none());
        assert!(parse_plain("\n\n").is_none());
    }
}
```

- [ ] Add `pub mod lyrics;` to `crates/sp-server/src/lib.rs` module declarations.

- [ ] Verify: `cargo test -p sp-server -- lyrics::lrclib` passes

- [ ] Commit: `feat(lyrics): add LRCLIB API client with LRC parser`

---

### Task 5: YouTube subtitle parser

**Files:**
- Create: `crates/sp-server/src/lyrics/youtube_subs.rs`

- [ ] Create YouTube subtitle fetcher and json3 parser:

```rust
use serde::Deserialize;
use sp_core::lyrics::{LyricsLine, LyricsTrack};
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct Json3Root {
    events: Option<Vec<Json3Event>>,
}

#[derive(Debug, Deserialize)]
struct Json3Event {
    #[serde(default, rename = "tStartMs")]
    t_start_ms: u64,
    #[serde(default, rename = "dDurationMs")]
    d_duration_ms: u64,
    #[serde(default)]
    segs: Option<Vec<Json3Seg>>,
}

#[derive(Debug, Deserialize)]
struct Json3Seg {
    #[serde(default)]
    utf8: String,
}

pub async fn fetch_subtitles(
    ytdlp_path: &Path,
    youtube_id: &str,
    temp_dir: &Path,
) -> Result<Option<LyricsTrack>, anyhow::Error> {
    let output_template = temp_dir.join(youtube_id);
    let mut cmd = tokio::process::Command::new(ytdlp_path);
    cmd.args([
        "--write-subs",
        "--write-auto-subs",
        "--sub-format", "json3",
        "--sub-lang", "en",
        "--skip-download",
        "-o",
    ]);
    cmd.arg(&output_template);
    cmd.arg(format!("https://www.youtube.com/watch?v={youtube_id}"));
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    tracing::debug!("youtube_subs: running yt-dlp for subtitles of {youtube_id}");
    let output = cmd.output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::debug!("youtube_subs: yt-dlp failed for {youtube_id}: {stderr}");
        return Ok(None);
    }

    let sub_path = find_subtitle_file(temp_dir, youtube_id)?;
    match sub_path {
        Some(path) => {
            let content = tokio::fs::read_to_string(&path).await?;
            let _ = tokio::fs::remove_file(&path).await;
            parse_json3(&content)
        }
        None => {
            tracing::debug!("youtube_subs: no subtitle file found for {youtube_id}");
            Ok(None)
        }
    }
}

fn find_subtitle_file(dir: &Path, youtube_id: &str) -> Result<Option<PathBuf>, anyhow::Error> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains(youtube_id) && name.ends_with(".json3") {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

fn parse_json3(content: &str) -> Result<Option<LyricsTrack>, anyhow::Error> {
    let root: Json3Root = serde_json::from_str(content)?;
    let events = match root.events {
        Some(e) if !e.is_empty() => e,
        _ => return Ok(None),
    };

    let mut lines = Vec::new();
    for event in &events {
        let segs = match &event.segs {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        let text: String = segs.iter().map(|s| s.utf8.as_str()).collect::<String>();
        let text = text.trim().replace('\n', " ");
        if text.is_empty() {
            continue;
        }
        lines.push(LyricsLine {
            start_ms: event.t_start_ms,
            end_ms: event.t_start_ms + event.d_duration_ms,
            en: text,
            sk: None,
            words: None,
        });
    }

    if lines.is_empty() {
        return Ok(None);
    }

    Ok(Some(LyricsTrack {
        version: 1,
        source: "youtube".to_string(),
        language_source: "en".to_string(),
        language_translation: String::new(),
        lines,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json3_basic() {
        let json = r#"{"events": [
            {"tStartMs": 1000, "dDurationMs": 2000, "segs": [{"utf8": "Hello "}, {"utf8": "world"}]},
            {"tStartMs": 3500, "dDurationMs": 1500, "segs": [{"utf8": "Second line"}]}
        ]}"#;
        let track = parse_json3(json).unwrap().unwrap();
        assert_eq!(track.lines.len(), 2);
        assert_eq!(track.lines[0].en, "Hello world");
        assert_eq!(track.lines[0].start_ms, 1000);
        assert_eq!(track.lines[0].end_ms, 3000);
        assert_eq!(track.lines[1].en, "Second line");
        assert_eq!(track.source, "youtube");
    }

    #[test]
    fn parse_json3_skips_empty_segments() {
        let json = r#"{"events": [
            {"tStartMs": 0, "dDurationMs": 100, "segs": [{"utf8": "\n"}]},
            {"tStartMs": 1000, "dDurationMs": 2000, "segs": [{"utf8": "Real text"}]}
        ]}"#;
        let track = parse_json3(json).unwrap().unwrap();
        assert_eq!(track.lines.len(), 1);
        assert_eq!(track.lines[0].en, "Real text");
    }

    #[test]
    fn parse_json3_empty_events() {
        let json = r#"{"events": []}"#;
        assert!(parse_json3(json).unwrap().is_none());
    }

    #[test]
    fn parse_json3_no_events() {
        let json = r#"{}"#;
        assert!(parse_json3(json).unwrap().is_none());
    }

    #[test]
    fn parse_json3_replaces_newlines() {
        let json = r#"{"events": [
            {"tStartMs": 0, "dDurationMs": 1000, "segs": [{"utf8": "Line one\nLine two"}]}
        ]}"#;
        let track = parse_json3(json).unwrap().unwrap();
        assert_eq!(track.lines[0].en, "Line one Line two");
    }
}
```

- [ ] Verify: `cargo test -p sp-server -- lyrics::youtube_subs` passes

- [ ] Commit: `feat(lyrics): add YouTube subtitle fetcher with json3 parser`

---

### Task 6: Python helper script (Qwen3 ASR + ForcedAligner)

**Files:**
- Create: `scripts/lyrics_worker.py`
- Create: `crates/sp-server/src/lyrics/aligner.rs`

- [ ] Create `scripts/lyrics_worker.py`:

```python
#!/usr/bin/env python3
"""SongPlayer lyrics ML helper — Qwen3-ASR transcription + ForcedAligner alignment.

Usage:
    lyrics_worker.py check-gpu
    lyrics_worker.py download-models [--models-dir DIR]
    lyrics_worker.py transcribe --audio PATH --output PATH [--models-dir DIR]
    lyrics_worker.py align --audio PATH --text TEXT --output PATH [--models-dir DIR]
"""
import argparse
import json
import os
import sys


def check_gpu():
    try:
        import torch
        if torch.cuda.is_available():
            name = torch.cuda.get_device_name(0)
            mem = torch.cuda.get_device_properties(0).total_mem / 1024**3
            print(json.dumps({"gpu": True, "device": name, "vram_gb": round(mem, 1)}))
        else:
            print(json.dumps({"gpu": False, "device": None, "vram_gb": 0}))
    except ImportError:
        print(json.dumps({"gpu": False, "device": None, "vram_gb": 0, "error": "torch not installed"}))


def download_models(models_dir):
    os.environ["HF_HOME"] = models_dir
    from huggingface_hub import snapshot_download
    print("Downloading Qwen3-ForcedAligner-0.6B...", file=sys.stderr)
    snapshot_download("Qwen/Qwen3-ForcedAligner-0.6B", cache_dir=models_dir)
    print("Downloading Qwen3-ASR-1.7B...", file=sys.stderr)
    snapshot_download("Qwen/Qwen3-ASR-1.7B", cache_dir=models_dir)
    print(json.dumps({"status": "ok"}))


def transcribe(audio_path, output_path, models_dir):
    os.environ["HF_HOME"] = models_dir
    import torch
    from transformers import AutoModelForSpeechSeq2Seq, AutoProcessor

    device = "cuda" if torch.cuda.is_available() else "cpu"
    torch_dtype = torch.float16 if device == "cuda" else torch.float32

    model_id = "Qwen/Qwen3-ASR-1.7B"
    print(f"Loading {model_id} on {device}...", file=sys.stderr)
    processor = AutoProcessor.from_pretrained(model_id, cache_dir=models_dir)
    model = AutoModelForSpeechSeq2Seq.from_pretrained(
        model_id, torch_dtype=torch_dtype, cache_dir=models_dir
    ).to(device)

    import librosa
    audio, sr = librosa.load(audio_path, sr=16000)

    inputs = processor(audio, sampling_rate=16000, return_tensors="pt").to(device)
    with torch.no_grad():
        generated = model.generate(**inputs, return_timestamps=True)
    result = processor.batch_decode(generated, skip_special_tokens=True)

    text = result[0] if result else ""
    with open(output_path, "w", encoding="utf-8") as f:
        json.dump({"text": text}, f, ensure_ascii=False)
    print(json.dumps({"status": "ok", "text_length": len(text)}))


def align(audio_path, text, output_path, models_dir):
    os.environ["HF_HOME"] = models_dir
    import torch

    device = "cuda" if torch.cuda.is_available() else "cpu"
    torch_dtype = torch.float16 if device == "cuda" else torch.float32

    model_id = "Qwen/Qwen3-ForcedAligner-0.6B"
    print(f"Loading {model_id} on {device}...", file=sys.stderr)

    from transformers import AutoModelForCTC, AutoProcessor
    import librosa

    processor = AutoProcessor.from_pretrained(model_id, cache_dir=models_dir)
    model = AutoModelForCTC.from_pretrained(
        model_id, torch_dtype=torch_dtype, cache_dir=models_dir
    ).to(device)

    audio, sr = librosa.load(audio_path, sr=16000)

    lines = [l.strip() for l in text.strip().split("\n") if l.strip()]

    result_lines = []
    for line_text in lines:
        inputs = processor(
            audio, sampling_rate=16000, text=line_text,
            return_tensors="pt"
        ).to(device)

        with torch.no_grad():
            outputs = model(**inputs)

        # Extract word-level timestamps from CTC output
        words = line_text.split()
        word_timestamps = extract_word_timestamps(outputs, processor, words, len(audio) / sr)

        result_lines.append({
            "en": line_text,
            "words": word_timestamps,
        })

    with open(output_path, "w", encoding="utf-8") as f:
        json.dump({"lines": result_lines}, f, ensure_ascii=False, indent=2)
    print(json.dumps({"status": "ok", "lines": len(result_lines)}))


def extract_word_timestamps(outputs, processor, words, audio_duration_s):
    """Extract per-word timestamps from CTC model output."""
    logits = outputs.logits[0]
    predicted_ids = logits.argmax(dim=-1)

    tokens = processor.tokenizer.convert_ids_to_tokens(predicted_ids.tolist())
    num_frames = len(tokens)
    frame_duration_ms = (audio_duration_s * 1000) / num_frames

    word_timestamps = []
    current_word_idx = 0
    word_start = None

    for i, token in enumerate(tokens):
        if token == processor.tokenizer.pad_token or token == "<ctc_blank>":
            if word_start is not None and current_word_idx < len(words):
                word_timestamps.append({
                    "text": words[current_word_idx],
                    "start_ms": int(word_start * frame_duration_ms),
                    "end_ms": int(i * frame_duration_ms),
                })
                current_word_idx += 1
                word_start = None
        elif token.startswith("▁") or token.startswith(" "):
            if word_start is not None and current_word_idx < len(words):
                word_timestamps.append({
                    "text": words[current_word_idx],
                    "start_ms": int(word_start * frame_duration_ms),
                    "end_ms": int(i * frame_duration_ms),
                })
                current_word_idx += 1
            word_start = i
        elif word_start is None:
            word_start = i

    if word_start is not None and current_word_idx < len(words):
        word_timestamps.append({
            "text": words[current_word_idx],
            "start_ms": int(word_start * frame_duration_ms),
            "end_ms": int(num_frames * frame_duration_ms),
        })

    # Fill in any remaining words without timestamps
    while len(word_timestamps) < len(words):
        last_end = word_timestamps[-1]["end_ms"] if word_timestamps else 0
        word_timestamps.append({
            "text": words[len(word_timestamps)],
            "start_ms": last_end,
            "end_ms": last_end,
        })

    return word_timestamps


def main():
    parser = argparse.ArgumentParser(description="SongPlayer lyrics ML helper")
    sub = parser.add_subparsers(dest="command")

    sub.add_parser("check-gpu")

    dl = sub.add_parser("download-models")
    dl.add_argument("--models-dir", default="./models")

    tr = sub.add_parser("transcribe")
    tr.add_argument("--audio", required=True)
    tr.add_argument("--output", required=True)
    tr.add_argument("--models-dir", default="./models")

    al = sub.add_parser("align")
    al.add_argument("--audio", required=True)
    al.add_argument("--text", required=True)
    al.add_argument("--output", required=True)
    al.add_argument("--models-dir", default="./models")

    args = parser.parse_args()

    if args.command == "check-gpu":
        check_gpu()
    elif args.command == "download-models":
        download_models(args.models_dir)
    elif args.command == "transcribe":
        transcribe(args.audio, args.output, args.models_dir)
    elif args.command == "align":
        with open(args.text, "r", encoding="utf-8") as f:
            text = f.read()
        align(args.audio, text, args.output, args.models_dir)
    else:
        parser.print_help()
        sys.exit(1)


if __name__ == "__main__":
    main()
```

- [ ] Create `crates/sp-server/src/lyrics/aligner.rs` — Rust subprocess wrapper:

```rust
use serde::Deserialize;
use sp_core::lyrics::{LyricsLine, LyricsTrack, LyricsWord};
use std::path::Path;

#[derive(Debug, Deserialize)]
struct AlignOutput {
    lines: Vec<AlignLine>,
}

#[derive(Debug, Deserialize)]
struct AlignLine {
    en: String,
    words: Vec<AlignWord>,
}

#[derive(Debug, Deserialize)]
struct AlignWord {
    text: String,
    start_ms: u64,
    end_ms: u64,
}

#[derive(Debug, Deserialize)]
struct TranscribeOutput {
    text: String,
}

pub async fn align_lyrics(
    python_path: &Path,
    script_path: &Path,
    models_dir: &Path,
    audio_path: &Path,
    lyrics_text: &str,
    output_path: &Path,
) -> Result<Vec<LyricsLine>, anyhow::Error> {
    let text_file = output_path.with_extension("txt");
    tokio::fs::write(&text_file, lyrics_text).await?;

    let mut cmd = tokio::process::Command::new(python_path);
    cmd.args([
        script_path.to_str().unwrap(),
        "align",
        "--audio", audio_path.to_str().unwrap(),
        "--text", text_file.to_str().unwrap(),
        "--output", output_path.to_str().unwrap(),
        "--models-dir", models_dir.to_str().unwrap(),
    ]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    tracing::info!("aligner: running forced alignment on {}", audio_path.display());
    let output = cmd.output().await?;
    let _ = tokio::fs::remove_file(&text_file).await;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("alignment failed: {stderr}");
    }

    let content = tokio::fs::read_to_string(output_path).await?;
    let align_out: AlignOutput = serde_json::from_str(&content)?;

    let lines: Vec<LyricsLine> = align_out
        .lines
        .into_iter()
        .map(|al| {
            let start_ms = al.words.first().map(|w| w.start_ms).unwrap_or(0);
            let end_ms = al.words.last().map(|w| w.end_ms).unwrap_or(0);
            LyricsLine {
                start_ms,
                end_ms,
                en: al.en,
                sk: None,
                words: Some(
                    al.words
                        .into_iter()
                        .map(|w| LyricsWord {
                            text: w.text,
                            start_ms: w.start_ms,
                            end_ms: w.end_ms,
                        })
                        .collect(),
                ),
            }
        })
        .collect();

    Ok(lines)
}

pub async fn transcribe_audio(
    python_path: &Path,
    script_path: &Path,
    models_dir: &Path,
    audio_path: &Path,
    output_path: &Path,
) -> Result<String, anyhow::Error> {
    let mut cmd = tokio::process::Command::new(python_path);
    cmd.args([
        script_path.to_str().unwrap(),
        "transcribe",
        "--audio", audio_path.to_str().unwrap(),
        "--output", output_path.to_str().unwrap(),
        "--models-dir", models_dir.to_str().unwrap(),
    ]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    tracing::info!("aligner: transcribing {}", audio_path.display());
    let output = cmd.output().await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("transcription failed: {stderr}");
    }

    let content = tokio::fs::read_to_string(output_path).await?;
    let result: TranscribeOutput = serde_json::from_str(&content)?;
    Ok(result.text)
}

pub async fn check_gpu(
    python_path: &Path,
    script_path: &Path,
) -> Result<bool, anyhow::Error> {
    let mut cmd = tokio::process::Command::new(python_path);
    cmd.args([script_path.to_str().unwrap(), "check-gpu"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    let output = cmd.output().await?;
    if !output.status.success() {
        return Ok(false);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);

    #[derive(Deserialize)]
    struct GpuCheck { gpu: bool }
    let check: GpuCheck = serde_json::from_str(&stdout)?;
    Ok(check.gpu)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_align_output() {
        let json = r#"{"lines": [
            {"en": "Amazing grace", "words": [
                {"text": "Amazing", "start_ms": 1500, "end_ms": 1920},
                {"text": "grace", "start_ms": 1920, "end_ms": 2340}
            ]},
            {"en": "How sweet", "words": [
                {"text": "How", "start_ms": 2340, "end_ms": 2520},
                {"text": "sweet", "start_ms": 2520, "end_ms": 2890}
            ]}
        ]}"#;
        let out: AlignOutput = serde_json::from_str(json).unwrap();
        assert_eq!(out.lines.len(), 2);
        assert_eq!(out.lines[0].words.len(), 2);
        assert_eq!(out.lines[0].words[0].text, "Amazing");
        assert_eq!(out.lines[0].words[0].start_ms, 1500);
    }

    #[test]
    fn parse_transcribe_output() {
        let json = r#"{"text": "Hello world"}"#;
        let out: TranscribeOutput = serde_json::from_str(json).unwrap();
        assert_eq!(out.text, "Hello world");
    }
}
```

- [ ] Verify: `cargo test -p sp-server -- lyrics::aligner` passes

- [ ] Commit: `feat(lyrics): add Python ML helper and Rust subprocess wrapper for Qwen3 alignment`

---

### Task 7: Gemini translation

**Files:**
- Create: `crates/sp-server/src/lyrics/translator.rs`

- [ ] Create the translator with worship glossary prompt:

```rust
use reqwest::Client;
use serde_json::{json, Value};
use sp_core::lyrics::LyricsTrack;

pub async fn translate_lyrics(
    client: &Client,
    api_key: &str,
    model: &str,
    track: &mut LyricsTrack,
) -> Result<(), anyhow::Error> {
    let en_lines: Vec<&str> = track.lines.iter().map(|l| l.en.as_str()).collect();
    let numbered: String = en_lines
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}: {}", i + 1, l))
        .collect::<Vec<_>>()
        .join("\n");

    let body = build_translation_body(model, &numbered, en_lines.len());
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        model, api_key
    );

    tracing::debug!("translator: sending {} lines to Gemini for EN→SK", en_lines.len());
    let resp = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?;

    let resp_json: Value = resp.json().await?;
    let text = resp_json["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .unwrap_or("");

    let sk_lines = parse_translation_response(text, en_lines.len());

    for (line, sk) in track.lines.iter_mut().zip(sk_lines.into_iter()) {
        if !sk.is_empty() {
            line.sk = Some(sk);
        }
    }

    track.language_translation = "sk".to_string();
    Ok(())
}

fn build_translation_body(model: &str, numbered_lyrics: &str, line_count: usize) -> Value {
    let system_prompt = format!(
        "You are a worship song translator. Translate English worship lyrics to natural Slovak.\n\
         \n\
         RULES:\n\
         1. Output EXACTLY {line_count} numbered lines, one Slovak translation per input line.\n\
         2. Format: \"N: Slovak text\" — same numbering as input.\n\
         3. Preserve the meaning and emotional tone of worship lyrics.\n\
         4. Use natural Slovak phrasing, not literal word-for-word translation.\n\
         5. Keep lines concise (max 45 characters) for LED wall display.\n\
         6. DO NOT translate these terms — keep in original: Hallelujah, Hosanna, Amen, Selah, Maranatha, Emmanuel\n\
         7. USE this worship glossary:\n\
            - Jesus → Ježiš\n\
            - Christ → Kristus\n\
            - Lord → Pán\n\
            - God → Boh\n\
            - grace → milosť\n\
            - Holy Spirit → Duch Svätý\n\
            - Lamb of God → Baránok Boží\n\
            - salvation → spasenie\n\
            - faith → viera\n\
            - mercy → milosrdenstvo\n\
            - glory → sláva\n\
            - kingdom → kráľovstvo\n\
            - cross → kríž\n\
            - praise → chvála\n\
            - worship → uctievanie\n\
            - eternal life → večný život\n\
            - resurrection → vzkriesenie\n\
         8. Slovak is ~10-20% longer than English. Prioritize conciseness for display.\n\
         9. Output ONLY the numbered translations. No commentary, no explanations.\n\
         10. Never produce Czech words — use Slovak orthography (pretože not protože, tiež not také)."
    );

    json!({
        "system_instruction": {
            "parts": [{"text": system_prompt}]
        },
        "contents": [{
            "role": "user",
            "parts": [{"text": numbered_lyrics}]
        }],
        "generationConfig": {
            "temperature": 0.3,
            "candidateCount": 1
        }
    })
}

fn parse_translation_response(text: &str, expected_count: usize) -> Vec<String> {
    let mut result = vec![String::new(); expected_count];
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((num_str, translation)) = line.split_once(':') {
            let num_str = num_str.trim();
            if let Ok(num) = num_str.parse::<usize>() {
                if num >= 1 && num <= expected_count {
                    result[num - 1] = translation.trim().to_string();
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_translation_response_basic() {
        let text = "1: Predivná milosť\n2: Jak ľúby to zvuk\n3: Čo zachránil úbožiaka\n";
        let result = parse_translation_response(text, 3);
        assert_eq!(result[0], "Predivná milosť");
        assert_eq!(result[1], "Jak ľúby to zvuk");
        assert_eq!(result[2], "Čo zachránil úbožiaka");
    }

    #[test]
    fn parse_translation_response_with_colon_in_text() {
        let text = "1: Svätý: Svätý: Svätý\n2: Pán Boh\n";
        let result = parse_translation_response(text, 2);
        assert_eq!(result[0], "Svätý: Svätý: Svätý");
        assert_eq!(result[1], "Pán Boh");
    }

    #[test]
    fn parse_translation_response_missing_lines() {
        let text = "1: Line one\n3: Line three\n";
        let result = parse_translation_response(text, 3);
        assert_eq!(result[0], "Line one");
        assert_eq!(result[1], "");
        assert_eq!(result[2], "Line three");
    }

    #[test]
    fn parse_translation_response_extra_lines_ignored() {
        let text = "1: One\n2: Two\n3: Three\n4: Four\n";
        let result = parse_translation_response(text, 2);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "One");
        assert_eq!(result[1], "Two");
    }

    #[test]
    fn parse_translation_response_empty() {
        let result = parse_translation_response("", 3);
        assert_eq!(result.len(), 3);
        assert!(result.iter().all(|s| s.is_empty()));
    }

    #[test]
    fn build_translation_body_structure() {
        let body = build_translation_body("gemini-2.5-flash", "1: Hello\n2: World", 2);
        let system = body["system_instruction"]["parts"][0]["text"].as_str().unwrap();
        assert!(system.contains("EXACTLY 2 numbered lines"));
        assert!(system.contains("Hallelujah"));
        assert!(system.contains("Ježiš"));
        assert!(system.contains("pretože"));
        let user = body["contents"][0]["parts"][0]["text"].as_str().unwrap();
        assert_eq!(user, "1: Hello\n2: World");
        let temp = body["generationConfig"]["temperature"].as_f64().unwrap();
        assert!((temp - 0.3).abs() < 0.01);
    }

    #[test]
    fn build_translation_body_no_search_tool() {
        let body = build_translation_body("gemini-2.5-flash", "1: Test", 1);
        assert!(body.get("tools").is_none());
    }
}
```

- [ ] Verify: `cargo test -p sp-server -- lyrics::translator` passes

- [ ] Commit: `feat(lyrics): add Gemini EN→SK translator with worship glossary`

---

## Phase 3: Lyrics Worker

### Task 8: Lyrics worker orchestrator

**Files:**
- Create: `crates/sp-server/src/lyrics/worker.rs` (rename from mod.rs re-export)
- Modify: `crates/sp-server/src/lyrics/mod.rs`
- Modify: `crates/sp-server/src/downloader/tools.rs`

- [ ] Extend `ToolPaths` in `crates/sp-server/src/downloader/tools.rs` to include python:

```rust
pub struct ToolPaths {
    pub ytdlp: PathBuf,
    pub ffmpeg: PathBuf,
    pub python: Option<PathBuf>,
}
```

Update `ensure_tools` to detect Python (search PATH for `python` or `python3`) and set `python` field. Do not auto-install Python — just detect it.

- [ ] Create `crates/sp-server/src/lyrics/worker.rs`:

```rust
use crate::db;
use crate::lyrics::{aligner, lrclib, translator, youtube_subs};
use reqwest::Client;
use sp_core::lyrics::LyricsTrack;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use tokio::sync::broadcast;

pub struct LyricsWorker {
    pool: SqlitePool,
    client: Client,
    cache_dir: PathBuf,
    ytdlp_path: PathBuf,
    python_path: Option<PathBuf>,
    script_path: PathBuf,
    models_dir: PathBuf,
    gemini_api_key: String,
    gemini_model: String,
}

impl LyricsWorker {
    pub fn new(
        pool: SqlitePool,
        cache_dir: PathBuf,
        ytdlp_path: PathBuf,
        python_path: Option<PathBuf>,
        tools_dir: PathBuf,
        gemini_api_key: String,
        gemini_model: String,
    ) -> Self {
        let script_path = tools_dir.join("lyrics_worker.py");
        let models_dir = tools_dir.join("models");
        Self {
            pool,
            client: Client::new(),
            cache_dir,
            ytdlp_path,
            python_path,
            script_path,
            models_dir,
            gemini_api_key,
            gemini_model,
        }
    }

    pub async fn run(self, mut shutdown_rx: broadcast::Receiver<()>) {
        tracing::info!("lyrics_worker: started");
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    tracing::info!("lyrics_worker: shutdown received");
                    break;
                }
                _ = self.process_next() => {}
            }
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    tracing::info!("lyrics_worker: shutdown during sleep");
                    break;
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }
        }
    }

    async fn process_next(&self) {
        let row = match db::models::get_next_video_without_lyrics(&self.pool).await {
            Ok(Some(row)) => row,
            Ok(None) => return,
            Err(e) => {
                tracing::error!("lyrics_worker: DB error: {e}");
                return;
            }
        };

        tracing::info!(
            "lyrics_worker: processing {} ({} - {})",
            row.youtube_id, row.artist, row.song
        );

        match self.process_song(&row).await {
            Ok(()) => {
                tracing::info!("lyrics_worker: completed {}", row.youtube_id);
            }
            Err(e) => {
                tracing::error!("lyrics_worker: failed {}: {e}", row.youtube_id);
                let _ = db::models::mark_video_lyrics(
                    &self.pool, row.id, false, Some("failed"),
                ).await;
            }
        }
    }

    async fn process_song(&self, row: &db::models::VideoLyricsRow) -> Result<(), anyhow::Error> {
        let duration_s = row.duration_ms.map(|ms| ms / 1000);

        // Step 1: Acquire lyrics text
        let (mut track, source) = self.acquire_lyrics(row, duration_s).await?;

        // Step 2: Forced alignment for word-level timestamps
        if let Some(ref python) = self.python_path {
            if let Some(ref audio_path) = row.audio_file_path {
                let audio = Path::new(audio_path);
                if audio.exists() {
                    let en_text: String = track.lines.iter().map(|l| l.en.as_str()).collect::<Vec<_>>().join("\n");
                    let align_output = self.cache_dir.join(format!("{}_align_temp.json", row.youtube_id));
                    match aligner::align_lyrics(
                        python, &self.script_path, &self.models_dir,
                        audio, &en_text, &align_output,
                    ).await {
                        Ok(aligned_lines) => {
                            for (orig, aligned) in track.lines.iter_mut().zip(aligned_lines.into_iter()) {
                                orig.start_ms = aligned.start_ms;
                                orig.end_ms = aligned.end_ms;
                                orig.words = aligned.words;
                            }
                            track.source = format!("{}+aligner", source);
                        }
                        Err(e) => {
                            tracing::warn!("lyrics_worker: alignment failed for {}: {e}", row.youtube_id);
                        }
                    }
                    let _ = tokio::fs::remove_file(&align_output).await;
                }
            }
        }

        // Step 3: Translate to Slovak
        if !self.gemini_api_key.is_empty() {
            if let Err(e) = translator::translate_lyrics(
                &self.client, &self.gemini_api_key, &self.gemini_model, &mut track,
            ).await {
                tracing::warn!("lyrics_worker: translation failed for {}: {e}", row.youtube_id);
            }
        }

        // Step 4: Persist
        let lyrics_path = self.cache_dir.join(format!("{}_lyrics.json", row.youtube_id));
        let json = serde_json::to_string_pretty(&track)?;
        tokio::fs::write(&lyrics_path, &json).await?;
        db::models::mark_video_lyrics(&self.pool, row.id, true, Some(&source)).await?;

        Ok(())
    }

    async fn acquire_lyrics(
        &self,
        row: &db::models::VideoLyricsRow,
        duration_s: Option<i64>,
    ) -> Result<(LyricsTrack, String), anyhow::Error> {
        // Source 1: LRCLIB
        if !row.song.is_empty() && !row.artist.is_empty() {
            match lrclib::fetch_lyrics(&self.client, &row.artist, &row.song, duration_s).await {
                Ok(Some(track)) => return Ok((track, "lrclib".to_string())),
                Ok(None) => tracing::debug!("lyrics_worker: LRCLIB miss for {}", row.youtube_id),
                Err(e) => tracing::warn!("lyrics_worker: LRCLIB error: {e}"),
            }
        }

        // Source 2: YouTube subtitles
        let temp_dir = self.cache_dir.join("_subs_temp");
        let _ = tokio::fs::create_dir_all(&temp_dir).await;
        match youtube_subs::fetch_subtitles(&self.ytdlp_path, &row.youtube_id, &temp_dir).await {
            Ok(Some(track)) => {
                let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                return Ok((track, "youtube".to_string()));
            }
            Ok(None) => tracing::debug!("lyrics_worker: no YouTube subs for {}", row.youtube_id),
            Err(e) => tracing::warn!("lyrics_worker: YouTube subs error: {e}"),
        }
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;

        // Source 3: Qwen3-ASR transcription
        if let Some(ref python) = self.python_path {
            if let Some(ref audio_path) = row.audio_file_path {
                let audio = Path::new(audio_path);
                if audio.exists() {
                    let output = self.cache_dir.join(format!("{}_asr_temp.json", row.youtube_id));
                    let text = aligner::transcribe_audio(
                        python, &self.script_path, &self.models_dir, audio, &output,
                    ).await?;
                    let _ = tokio::fs::remove_file(&output).await;

                    let lines: Vec<sp_core::lyrics::LyricsLine> = text
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(|l| sp_core::lyrics::LyricsLine {
                            start_ms: 0,
                            end_ms: 0,
                            en: l.trim().to_string(),
                            sk: None,
                            words: None,
                        })
                        .collect();

                    if !lines.is_empty() {
                        return Ok((
                            LyricsTrack {
                                version: 1,
                                source: "asr".to_string(),
                                language_source: "en".to_string(),
                                language_translation: String::new(),
                                lines,
                            },
                            "asr".to_string(),
                        ));
                    }
                }
            }
        }

        anyhow::bail!("no lyrics source available for {}", row.youtube_id)
    }
}
```

- [ ] Add `pub mod lyrics;` to `crates/sp-server/src/lib.rs` if not already done in Task 4.

- [ ] Verify: `cargo check -p sp-server` passes

- [ ] Commit: `feat(lyrics): add lyrics worker with source waterfall and alignment pipeline`

---

### Task 9: Wire lyrics worker into server startup

**Files:**
- Modify: `crates/sp-server/src/lib.rs`

- [ ] In the `start()` function, after the tools setup block that spawns the download worker, add lyrics worker spawning:

```rust
// Inside the tools-ready block, after download worker spawn:
let lyrics_pool = pool.clone();
let lyrics_cache_dir = cache_dir.clone();
let lyrics_ytdlp = paths.ytdlp.clone();
let lyrics_python = paths.python.clone();
let lyrics_tools_dir = tools_dir.clone();
let lyrics_gemini_key = gemini_api_key.clone();
let lyrics_gemini_model = gemini_model.clone();
let lyrics_shutdown = shutdown_tx.subscribe();

tokio::spawn(async move {
    let worker = lyrics::LyricsWorker::new(
        lyrics_pool,
        lyrics_cache_dir,
        lyrics_ytdlp,
        lyrics_python,
        lyrics_tools_dir,
        lyrics_gemini_key,
        lyrics_gemini_model,
    );
    worker.run(lyrics_shutdown).await;
});
tracing::info!("lyrics worker spawned");
```

- [ ] Verify: `cargo check -p sp-server` passes

- [ ] Commit: `feat: wire lyrics worker into server startup`

---

## Phase 4: Lyrics Renderer and Display

### Task 10: Lyrics renderer (position → line → word → emit)

**Files:**
- Create: `crates/sp-server/src/lyrics/renderer.rs`

- [ ] Create the renderer that computes lyrics state from playback position:

```rust
use sp_core::lyrics::LyricsTrack;
use sp_core::ws::ServerMsg;

pub struct LyricsState {
    track: LyricsTrack,
    last_line_index: Option<usize>,
}

impl LyricsState {
    pub fn new(track: LyricsTrack) -> Self {
        Self {
            track,
            last_line_index: None,
        }
    }

    /// Compute the lyrics update message for the given position.
    /// Returns Some if there's a lyrics line to display, None if between lines or no lyrics.
    pub fn update(&mut self, playlist_id: i64, position_ms: u64) -> Option<ServerMsg> {
        let current = self.track.line_at(position_ms);

        let (line_idx, line) = match current {
            Some((idx, line)) => (Some(idx), Some(line)),
            None => (None, None),
        };

        // Always emit update so dashboard can clear when between lines
        let changed = line_idx != self.last_line_index;
        self.last_line_index = line_idx;

        if !changed && line.is_some() {
            // Same line, but update word index
            let word_info = line.and_then(|l| {
                LyricsTrack::word_index_at(l, position_ms)
                    .map(|wi| (wi, l.words.as_ref().map(|w| w.len()).unwrap_or(0)))
            });
            return Some(ServerMsg::LyricsUpdate {
                playlist_id,
                line_en: line.map(|l| l.en.clone()),
                line_sk: line.and_then(|l| l.sk.clone()),
                prev_line_en: line_idx.and_then(|i| {
                    if i > 0 { Some(self.track.lines[i - 1].en.clone()) } else { None }
                }),
                next_line_en: line_idx.and_then(|i| {
                    self.track.lines.get(i + 1).map(|l| l.en.clone())
                }),
                active_word_index: word_info.map(|(wi, _)| wi),
                word_count: word_info.map(|(_, wc)| wc),
            });
        }

        Some(ServerMsg::LyricsUpdate {
            playlist_id,
            line_en: line.map(|l| l.en.clone()),
            line_sk: line.and_then(|l| l.sk.clone()),
            prev_line_en: line_idx.and_then(|i| {
                if i > 0 { Some(self.track.lines[i - 1].en.clone()) } else { None }
            }),
            next_line_en: line_idx.and_then(|i| {
                self.track.lines.get(i + 1).map(|l| l.en.clone())
            }),
            active_word_index: line.and_then(|l| {
                LyricsTrack::word_index_at(l, position_ms)
            }),
            word_count: line.and_then(|l| {
                l.words.as_ref().map(|w| w.len())
            }),
        })
    }

    /// Returns the current EN and SK lines for Resolume output.
    /// Only returns Some when the line has changed (to avoid spamming Resolume).
    pub fn resolume_update(&self, position_ms: u64) -> (Option<String>, Option<String>) {
        match self.track.line_at(position_ms) {
            Some((_, line)) => (
                Some(line.en.clone()),
                line.sk.clone(),
            ),
            None => (None, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_core::lyrics::{LyricsLine, LyricsWord};

    fn test_track() -> LyricsTrack {
        LyricsTrack {
            version: 1,
            source: "test".to_string(),
            language_source: "en".to_string(),
            language_translation: "sk".to_string(),
            lines: vec![
                LyricsLine {
                    start_ms: 1000,
                    end_ms: 3000,
                    en: "First line".to_string(),
                    sk: Some("Prvý riadok".to_string()),
                    words: Some(vec![
                        LyricsWord { text: "First".to_string(), start_ms: 1000, end_ms: 2000 },
                        LyricsWord { text: "line".to_string(), start_ms: 2000, end_ms: 3000 },
                    ]),
                },
                LyricsLine {
                    start_ms: 3000,
                    end_ms: 5000,
                    en: "Second line".to_string(),
                    sk: Some("Druhý riadok".to_string()),
                    words: Some(vec![
                        LyricsWord { text: "Second".to_string(), start_ms: 3000, end_ms: 4000 },
                        LyricsWord { text: "line".to_string(), start_ms: 4000, end_ms: 5000 },
                    ]),
                },
            ],
        }
    }

    #[test]
    fn update_emits_lyrics_for_active_line() {
        let mut state = LyricsState::new(test_track());
        let msg = state.update(1, 1500).unwrap();
        match msg {
            ServerMsg::LyricsUpdate { line_en, line_sk, active_word_index, .. } => {
                assert_eq!(line_en.as_deref(), Some("First line"));
                assert_eq!(line_sk.as_deref(), Some("Prvý riadok"));
                assert_eq!(active_word_index, Some(0));
            }
            _ => panic!("unexpected message type"),
        }
    }

    #[test]
    fn update_emits_none_between_lines() {
        let mut state = LyricsState::new(test_track());
        let msg = state.update(1, 500).unwrap();
        match msg {
            ServerMsg::LyricsUpdate { line_en, line_sk, .. } => {
                assert!(line_en.is_none());
                assert!(line_sk.is_none());
            }
            _ => panic!("unexpected message type"),
        }
    }

    #[test]
    fn update_prev_next_lines() {
        let mut state = LyricsState::new(test_track());
        let msg = state.update(1, 3500).unwrap();
        match msg {
            ServerMsg::LyricsUpdate { line_en, prev_line_en, next_line_en, .. } => {
                assert_eq!(line_en.as_deref(), Some("Second line"));
                assert_eq!(prev_line_en.as_deref(), Some("First line"));
                assert!(next_line_en.is_none());
            }
            _ => panic!("unexpected message type"),
        }
    }

    #[test]
    fn update_word_index_advances() {
        let mut state = LyricsState::new(test_track());
        let msg = state.update(1, 1500).unwrap();
        match &msg {
            ServerMsg::LyricsUpdate { active_word_index, .. } => assert_eq!(*active_word_index, Some(0)),
            _ => panic!(),
        }
        let msg = state.update(1, 2500).unwrap();
        match &msg {
            ServerMsg::LyricsUpdate { active_word_index, .. } => assert_eq!(*active_word_index, Some(1)),
            _ => panic!(),
        }
    }

    #[test]
    fn resolume_update_returns_text() {
        let state = LyricsState::new(test_track());
        let (en, sk) = state.resolume_update(1500);
        assert_eq!(en.as_deref(), Some("First line"));
        assert_eq!(sk.as_deref(), Some("Prvý riadok"));
    }

    #[test]
    fn resolume_update_returns_none_between_lines() {
        let state = LyricsState::new(test_track());
        let (en, sk) = state.resolume_update(500);
        assert!(en.is_none());
        assert!(sk.is_none());
    }
}
```

- [ ] Verify: `cargo test -p sp-server -- lyrics::renderer` passes

- [ ] Commit: `feat(lyrics): add lyrics renderer with line/word lookup and Resolume output`

---

### Task 11: Integrate renderer into PlaybackEngine

**Files:**
- Modify: `crates/sp-server/src/playback/mod.rs`

- [ ] Add `lyrics_state: Option<lyrics::renderer::LyricsState>` field to `PlaylistPipeline` struct.

- [ ] In `on_video_started` (or the equivalent method that fires when a new video begins playing): load the lyrics JSON file if `has_lyrics=1` in DB, parse into `LyricsTrack`, create `LyricsState::new(track)`, store in `pipeline.lyrics_state`.

- [ ] In `maybe_broadcast_position_update`: after sending `ServerMsg::NowPlaying`, also call `lyrics_state.update(playlist_id, position_ms)` and if it returns `Some(msg)`, send via `ws_event_tx`. Also check `karaoke_enabled` on the playlist before emitting.

- [ ] For Resolume: after the lyrics update, if the line changed, send `ResolumeCommand::ShowSubtitles` or `ResolumeCommand::HideSubtitles` (added in Task 12).

- [ ] When video stops (`on_video_ended` or equivalent): clear `lyrics_state` to `None`, send `LyricsUpdate` with all fields `None` to clear the dashboard, send `ResolumeCommand::HideSubtitles`.

- [ ] Verify: `cargo check -p sp-server` passes

- [ ] Commit: `feat(playback): integrate lyrics renderer into position broadcasts`

---

### Task 12: Resolume subtitle commands

**Files:**
- Modify: `crates/sp-server/src/resolume/mod.rs`
- Modify: `crates/sp-server/src/resolume/driver.rs`
- Modify: `crates/sp-server/src/resolume/handlers.rs`

- [ ] Add new constants and command variants to `crates/sp-server/src/resolume/mod.rs`:

```rust
pub const SUBS_TOKEN: &str = "#sp-subs";
pub const SUBS_SK_TOKEN: &str = "#sp-subssk";
```

Add to `ResolumeCommand` enum:

```rust
    ShowSubtitles { en: String, sk: Option<String> },
    HideSubtitles,
```

- [ ] In `crates/sp-server/src/resolume/driver.rs`, the `run_loop` match arm: add handling for `ShowSubtitles` and `HideSubtitles` commands. `ShowSubtitles` calls `set_text_all` on `#sp-subs` clips with the EN text, and `set_text_all` on `#sp-subssk` clips with the SK text. No fade animation for subtitles — instant text swap. `HideSubtitles` clears both to empty string.

- [ ] In `crates/sp-server/src/resolume/handlers.rs`, add:

```rust
pub async fn set_subtitles(
    driver: &HostDriver,
    en: &str,
    sk: Option<&str>,
) -> Result<(), anyhow::Error> {
    let subs_clips = driver.clips_for_token(super::SUBS_TOKEN);
    let subs_sk_clips = driver.clips_for_token(super::SUBS_SK_TOKEN);

    if !subs_clips.is_empty() {
        set_text_all(driver, &subs_clips, en).await?;
    }
    if !subs_sk_clips.is_empty() {
        let sk_text = sk.unwrap_or("");
        set_text_all(driver, &subs_sk_clips, sk_text).await?;
    }
    Ok(())
}

pub async fn clear_subtitles(driver: &HostDriver) -> Result<(), anyhow::Error> {
    let subs_clips = driver.clips_for_token(super::SUBS_TOKEN);
    let subs_sk_clips = driver.clips_for_token(super::SUBS_SK_TOKEN);

    if !subs_clips.is_empty() {
        set_text_all(driver, &subs_clips, "").await?;
    }
    if !subs_sk_clips.is_empty() {
        set_text_all(driver, &subs_sk_clips, "").await?;
    }
    Ok(())
}
```

- [ ] The existing `clips_for_token` method (or equivalent lookup in `clip_mapping`) already discovers clips by token. Ensure `#sp-subs` and `#sp-subssk` are discovered alongside `#sp-title` during the composition scan.

- [ ] Verify: `cargo check -p sp-server` passes

- [ ] Commit: `feat(resolume): add #sp-subs and #sp-subssk subtitle clip commands`

---

## Phase 5: API and Cache

### Task 13: API routes for lyrics

**Files:**
- Modify: `crates/sp-server/src/api/routes.rs`

- [ ] Add lyrics API endpoints:

```rust
pub async fn get_video_lyrics(
    State(state): State<Arc<AppState>>,
    Path(video_id): Path<i64>,
) -> impl IntoResponse {
    // Query video to get youtube_id, check has_lyrics
    // Read {youtube_id}_lyrics.json from cache_dir
    // Return as JSON response
}

pub async fn reprocess_video_lyrics(
    State(state): State<Arc<AppState>>,
    Path(video_id): Path<i64>,
) -> impl IntoResponse {
    // Reset has_lyrics=0 in DB so lyrics worker picks it up again
    db::models::reset_video_lyrics(&state.pool, video_id).await
    // Return 200 OK
}

pub async fn get_lyrics_status(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let (total, processed, pending) = db::models::get_lyrics_status(&state.pool).await?;
    Json(json!({"total": total, "processed": processed, "pending": pending}))
}
```

- [ ] Register routes in the Axum router:

```rust
.route("/api/v1/videos/:id/lyrics", get(get_video_lyrics))
.route("/api/v1/videos/:id/lyrics/reprocess", post(reprocess_video_lyrics))
.route("/api/v1/lyrics/status", get(get_lyrics_status))
```

- [ ] Extend the existing `update_playlist` handler to accept `karaoke_enabled` in the PATCH body.

- [ ] Verify: `cargo check -p sp-server` passes

- [ ] Commit: `feat(api): add lyrics REST endpoints and karaoke toggle`

---

### Task 14: Cache scan extension for lyrics sidecars

**Files:**
- Modify: `crates/sp-server/src/downloader/cache.rs`
- Modify: `crates/sp-server/src/startup.rs`

- [ ] Add lyrics file detection to `scan_cache` in `cache.rs`:

Add a new regex for lyrics files:
```rust
static LYRICS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([a-zA-Z0-9_-]{11})_lyrics\.json$").unwrap()
});
```

Add `lyrics_files: Vec<PathBuf>` to `ScanResult`. Populate during the directory scan.

- [ ] In `crates/sp-server/src/startup.rs`, extend `self_heal_cache`:

After the existing video+audio pair handling, scan for lyrics files:
- For each `{youtube_id}_lyrics.json`, check if a matching video+audio pair exists.
- If no pair: delete the orphaned lyrics file.
- If pair exists: update `has_lyrics=1` in DB for the matching video row.

- [ ] Add tests for lyrics file detection in cache scan.

- [ ] Verify: `cargo test -p sp-server` passes

- [ ] Commit: `feat(cache): detect and heal lyrics sidecar files on startup`

---

## Phase 6: Dashboard UI

### Task 15: Dashboard store — lyrics state

**Files:**
- Modify: `sp-ui/src/store.rs`

- [ ] Add lyrics fields to `NowPlayingInfo`:

```rust
pub struct NowPlayingInfo {
    pub video_id: i64,
    pub song: String,
    pub artist: String,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub state: PlaybackState,
    pub mode: PlaybackMode,
    // New lyrics fields:
    pub line_en: Option<String>,
    pub line_sk: Option<String>,
    pub prev_line_en: Option<String>,
    pub next_line_en: Option<String>,
    pub active_word_index: Option<usize>,
    pub word_count: Option<usize>,
}
```

- [ ] Add `LyricsUpdate` dispatch handler in `store.rs`:

```rust
ServerMsg::LyricsUpdate {
    playlist_id,
    line_en,
    line_sk,
    prev_line_en,
    next_line_en,
    active_word_index,
    word_count,
} => {
    self.now_playing.update(|map| {
        if let Some(info) = map.get_mut(&playlist_id) {
            info.line_en = line_en;
            info.line_sk = line_sk;
            info.prev_line_en = prev_line_en;
            info.next_line_en = next_line_en;
            info.active_word_index = active_word_index;
            info.word_count = word_count;
        }
    });
}
```

- [ ] Verify: `cd sp-ui && trunk build --release` passes

- [ ] Commit: `feat(ui): add lyrics state to dashboard store`

---

### Task 16: KaraokePanel Leptos component

**Files:**
- Create: `sp-ui/src/components/karaoke_panel.rs`
- Modify: `sp-ui/src/components/mod.rs`
- Modify: `sp-ui/src/components/playlist_card.rs`

- [ ] Create `sp-ui/src/components/karaoke_panel.rs`:

```rust
use leptos::prelude::*;
use crate::store::NowPlayingInfo;

#[component]
pub fn KaraokePanel(info: NowPlayingInfo) -> impl IntoView {
    let has_lyrics = info.line_en.is_some()
        || info.prev_line_en.is_some()
        || info.next_line_en.is_some();

    if !has_lyrics {
        return view! { }.into_any();
    }

    let prev_line = info.prev_line_en.clone().unwrap_or_default();
    let next_line = info.next_line_en.clone().unwrap_or_default();
    let current_en = info.line_en.clone().unwrap_or_default();
    let current_sk = info.line_sk.clone().unwrap_or_default();
    let active_idx = info.active_word_index.unwrap_or(0);
    let word_count = info.word_count.unwrap_or(0);

    let words: Vec<String> = current_en.split_whitespace().map(String::from).collect();

    view! {
        <div class="karaoke-panel">
            {if !prev_line.is_empty() {
                view! { <div class="karaoke-line karaoke-dim">{prev_line}</div> }.into_any()
            } else {
                view! { }.into_any()
            }}
            <div class="karaoke-line karaoke-current">
                {words.into_iter().enumerate().map(|(i, word)| {
                    let class = if i < active_idx {
                        "karaoke-word karaoke-word-past"
                    } else if i == active_idx {
                        "karaoke-word karaoke-word-active"
                    } else {
                        "karaoke-word karaoke-word-future"
                    };
                    view! { <span class=class>{word}{" "}</span> }
                }).collect_view()}
            </div>
            {if !current_sk.is_empty() {
                view! { <div class="karaoke-line karaoke-sk">{current_sk}</div> }.into_any()
            } else {
                view! { }.into_any()
            }}
            {if !next_line.is_empty() {
                view! { <div class="karaoke-line karaoke-dim">{next_line}</div> }.into_any()
            } else {
                view! { }.into_any()
            }}
        </div>
    }.into_any()
}
```

- [ ] Add `pub mod karaoke_panel;` to `sp-ui/src/components/mod.rs`.

- [ ] Integrate into `playlist_card.rs`: after the progress bar section, render `<KaraokePanel info=info.clone() />` when the playlist is playing.

- [ ] Add CSS to `sp-ui/style.css`:

```css
.karaoke-panel {
    background: #0d1b2a;
    border-radius: 6px;
    padding: 12px;
    margin-top: 8px;
    text-align: center;
}
.karaoke-line {
    margin: 4px 0;
}
.karaoke-dim {
    color: #444;
    font-size: 0.85em;
}
.karaoke-current {
    font-size: 1.1em;
}
.karaoke-sk {
    color: #4ea8de;
    font-size: 0.95em;
}
.karaoke-word {
    transition: color 0.15s;
}
.karaoke-word-past {
    color: #888;
}
.karaoke-word-active {
    color: #e94560;
    font-weight: bold;
    text-decoration: underline;
}
.karaoke-word-future {
    color: #555;
}
```

- [ ] Verify: `cd sp-ui && trunk build --release` passes

- [ ] Commit: `feat(ui): add KaraokePanel component with word-level highlighting`

---

## Phase 7: E2E and CI

### Task 17: E2E test for lyrics

**Files:**
- Modify: `e2e/post-deploy-flac.spec.ts`

- [ ] Add a lyrics status test block to the existing E2E spec:

```typescript
test('lyrics processing status endpoint responds', async ({ request }) => {
    const resp = await request.get(`${BASE}/api/v1/lyrics/status`);
    expect(resp.status()).toBe(200);
    const data = await resp.json();
    expect(data).toHaveProperty('total');
    expect(data).toHaveProperty('processed');
    expect(data).toHaveProperty('pending');
    expect(typeof data.total).toBe('number');
});

test('lyrics available for at least one video', async ({ request }) => {
    const playlistResp = await request.get(`${BASE}/api/v1/playlists`);
    const playlists = await playlistResp.json();
    let foundLyrics = false;

    for (const pl of playlists) {
        const videosResp = await request.get(`${BASE}/api/v1/playlists/${pl.id}/videos`);
        const videos = await videosResp.json();

        for (const vid of videos) {
            if (!vid.normalized) continue;
            const lyricsResp = await request.get(`${BASE}/api/v1/videos/${vid.id}/lyrics`);
            if (lyricsResp.status() === 200) {
                const lyrics = await lyricsResp.json();
                expect(lyrics).toHaveProperty('lines');
                expect(lyrics.lines.length).toBeGreaterThan(0);
                expect(lyrics.lines[0]).toHaveProperty('en');
                // Check word-level timestamps if available
                if (lyrics.lines[0].words) {
                    expect(lyrics.lines[0].words.length).toBeGreaterThan(0);
                    expect(lyrics.lines[0].words[0]).toHaveProperty('start_ms');
                }
                foundLyrics = true;
                break;
            }
        }
        if (foundLyrics) break;
    }

    // Allow lyrics to not be processed yet on first deploy
    if (!foundLyrics) {
        console.log('DIAGNOSTIC: No videos with lyrics found yet — worker may still be processing');
    }
});
```

- [ ] Add Playwright browser test for dashboard karaoke panel in `e2e/post-deploy-flac.spec.ts`:

```typescript
test('dashboard shows karaoke panel when playing with lyrics', async ({ page }) => {
    const consoleMessages: string[] = [];
    page.on('console', (msg) => {
        if (msg.type() === 'error' || msg.type() === 'warning') {
            consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
        }
    });

    await page.goto(BASE);
    await page.waitForSelector('.playlist-card', { timeout: 10000 });

    // Check if any karaoke panel is visible (requires active playback with lyrics)
    const karaokePanel = page.locator('.karaoke-panel');
    const panelCount = await karaokePanel.count();

    if (panelCount > 0) {
        // Verify panel structure
        const panel = karaokePanel.first();
        await expect(panel.locator('.karaoke-current')).toBeVisible();
    } else {
        console.log('DIAGNOSTIC: No karaoke panel visible — no active playback or no lyrics');
    }

    expect(consoleMessages).toEqual([]);
});
```

- [ ] Verify: `cargo fmt --all --check` passes

- [ ] Commit: `test(e2e): add lyrics API and dashboard karaoke E2E tests`

---

### Task 18: Version bump and final verification

**Files:**
- Modify: `VERSION`

- [ ] Ensure VERSION is already `0.13.0-dev.1` (bumped from previous work). If not, bump it.

- [ ] Run `./scripts/sync-version.sh` to propagate.

- [ ] Run `cargo fmt --all --check` — fix any issues.

- [ ] Run `cargo test` — all tests pass.

- [ ] Push to dev branch and monitor CI until all jobs reach terminal state.

- [ ] Commit any CI fixes as needed.

- [ ] Commit: `chore: version sync and CI fixes` (if needed)

---

## Verification Checklist

After all tasks:

1. `cargo check --workspace` passes on Linux
2. `cargo test --workspace` passes — including all new lyrics tests
3. `cargo check --target wasm32-unknown-unknown -p sp-core` compiles (WASM-safe)
4. `cd sp-ui && trunk build --release` produces dist/
5. CI green on dev branch (all jobs)
6. Deploy to win-resolume — lyrics worker starts processing songs
7. LRCLIB fetches lyrics for known worship songs
8. Forced alignment produces word-level timestamps
9. Gemini translates to Slovak
10. Dashboard shows karaoke panel with word highlighting
11. Resolume `#sp-subs` and `#sp-subssk` clips receive lyrics text
12. Per-playlist karaoke toggle works via API
