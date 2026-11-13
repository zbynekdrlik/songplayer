//! #321: the blind A/B of the EN→SK translation (the flip's evidence).
//!
//! The owner's ruling (#232, 13.11.2026): the translation's default moves to
//! OpenRouter only after a blind A/B on our songs shows it is no worse.
//! `POST /api/v1/ai/translate-ab {"youtube_ids", "candidates"?}` runs, on the
//! box and through SongPlayer's own client (the key never leaves it), the
//! production translator prompt (`translator::build_prompt` with the song's
//! gender) of each song through every candidate, and writes into
//! `<cache>/ab/translate-<unix ms>/`:
//!
//! - `blind.md`: per song, per line the EN and each candidate's SK line
//!   under a LETTER (A, B, C …), the letters shuffled afresh for every song
//!   from a seed of OS randomness (one unmasked letter unmasks one song);
//!   a failed call reads a neutral `(chyba)` (an error's text would name
//!   its provider);
//! - `key.json`: per song, letter → provider + model (+ the call's error),
//!   kept apart (open it only after the grading).
//!
//! Default candidates: the proxy's current Claude (`ai_model`),
//! `google/gemini-4-carbon` and `google/gemini-4-hydrogen` on OpenRouter.
//! Every song is read (its lyrics, its row, at least one line) BEFORE the
//! first paid call: a song that cannot run fails the run naming every such
//! id, nothing spent. A failed call is written into its cells as
//! `(chyba)` and the run goes on. Paid AI: the route refuses while it is
//! off (both sides are paid). An OpenRouter call writes at most
//! [`CALL_MAX_TOKENS`] (a song's translation needs far less).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sp_core::ai_tasks::AiProvider;
use sp_core::lyrics::LyricsTrack;
use sqlx::SqlitePool;

use crate::ai::client::AiClient;
use crate::lyrics::translator::{SpeakerGender, build_prompt, parse_translation_response};

/// At most this many songs per run (each costs one paid call per candidate).
pub const MAX_SONGS: usize = 40;

/// At most this many candidates per run (letters A..F).
pub const MAX_CANDIDATES: usize = 6;

/// One call's bound (s): the translator's own.
pub const CALL_TIMEOUT_S: u64 = 300;

/// The most one OpenRouter call of the A/B may write (bounds what a run of
/// caller-chosen models can cost; a song's SK lines are a few hundred
/// tokens).
pub const CALL_MAX_TOKENS: u32 = 8_000;

/// One model under test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    /// `claude_proxy` or `openrouter`.
    pub provider: String,
    pub model: String,
}

/// The route's body.
#[derive(Debug, Deserialize)]
pub struct AbRequest {
    #[serde(default)]
    pub youtube_ids: Vec<String>,
    #[serde(default)]
    pub candidates: Option<Vec<Candidate>>,
}

/// The run's state, for `GET /api/v1/ai/translate-ab`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AbStatus {
    /// `idle`, `running`, `done` or `failed`.
    pub state: String,
    pub dir: Option<String>,
    pub songs_done: usize,
    pub songs_total: usize,
    pub error: Option<String>,
}

/// One song's input: its lines and its gender.
#[derive(Debug, Clone)]
pub struct Song {
    pub youtube_id: String,
    pub title: String,
    pub track: LyricsTrack,
    pub gender: SpeakerGender,
}

/// A YouTube id: 11 of `A-Z a-z 0-9 _ -`.
pub fn valid_youtube_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The default candidates: the proxy's `proxy_model`, then Carbon and
/// Hydrogen on OpenRouter.
pub fn default_candidates(proxy_model: &str) -> Vec<Candidate> {
    vec![
        Candidate {
            provider: AiProvider::ClaudeProxy.as_str().into(),
            model: proxy_model.into(),
        },
        Candidate {
            provider: AiProvider::OpenRouter.as_str().into(),
            model: "google/gemini-4-carbon".into(),
        },
        Candidate {
            provider: AiProvider::OpenRouter.as_str().into(),
            model: "google/gemini-4-hydrogen".into(),
        },
    ]
}

/// The request checked: the ids (1..=[`MAX_SONGS`], each valid, each once,
/// in order) and the candidates (2..=[`MAX_CANDIDATES`], each a known
/// provider and a model). An error is the 400's text.
pub fn checked(req: AbRequest, proxy_model: &str) -> Result<(Vec<String>, Vec<Candidate>), String> {
    let mut ids: Vec<String> = Vec::new();
    for id in req.youtube_ids {
        let id = id.trim().to_string();
        if !valid_youtube_id(&id) {
            return Err(format!("not a YouTube id: {id:?}"));
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() || ids.len() > MAX_SONGS {
        return Err(format!("youtube_ids: 1 to {MAX_SONGS} ids"));
    }
    let candidates = req
        .candidates
        .unwrap_or_else(|| default_candidates(proxy_model));
    if candidates.len() < 2 || candidates.len() > MAX_CANDIDATES {
        return Err(format!("candidates: 2 to {MAX_CANDIDATES}"));
    }
    for c in &candidates {
        if c.provider.trim().is_empty() || AiProvider::parse(Some(&c.provider)).is_none() {
            return Err(format!(
                "candidate provider {:?}: claude_proxy or openrouter",
                c.provider
            ));
        }
        if c.model.trim().is_empty() {
            return Err("a candidate has no model".into());
        }
        // The proxy always sends its own `ai_model`: another model named
        // for it would be written into `key.json` under a false name.
        if AiProvider::read(Some(&c.provider)) == AiProvider::ClaudeProxy
            && c.model.trim() != proxy_model
        {
            return Err(format!(
                "a claude_proxy candidate sends the proxy's model {proxy_model:?}, not {:?}",
                c.model
            ));
        }
    }
    Ok((ids, candidates))
}

/// A permutation of `0..n` from `seed` (Fisher–Yates over xorshift64):
/// `order[letter] = candidate index`.
pub fn shuffled(n: usize, seed: u64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    // xorshift needs a non-zero state; every seed bit counts (a `| 1`
    // would make seeds that differ only in bit 0 give one order).
    let mut x = seed ^ 0x9E37_79B9_7F4A_7C15;
    if x == 0 {
        x = 1;
    }
    for i in (1..n).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let j = (x % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    order
}

/// Each song's letter order: [`shuffled`] with the run's seed mixed with
/// the song's place, so every song has its own order.
pub fn song_orders(seed: u64, songs: usize, candidates: usize) -> Vec<Vec<usize>> {
    (0..songs)
        .map(|i| {
            let mixed = seed ^ (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            shuffled(candidates, mixed)
        })
        .collect()
}

/// A run's seed from the OS's randomness (std's `RandomState` keys), never
/// the clock the run's dir is named after.
pub fn random_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

/// The letter of place `i` (A, B, …).
pub fn letter(i: usize) -> char {
    (b'A' + i as u8) as char
}

/// One song's translations: one per candidate (in candidate order), each
/// the SK lines or the call's error.
pub type SongResult = Vec<Result<Vec<String>, String>>;

/// The blind sheet (pure): per song its title, then per line the EN and the
/// SK of each letter (`orders[song][letter] = candidate`). Names no
/// provider and no model; a failed call is a neutral `(chyba)`.
pub fn blind_sheet(started: &str, songs: &[(Song, SongResult)], orders: &[Vec<usize>]) -> String {
    let mut out = String::new();
    out.push_str("# Slepé porovnanie prekladu EN → SK (#321)\n\n");
    let n = orders.first().map_or(0, Vec::len);
    out.push_str(&format!(
        "Spustené {started}; skladieb: {}; kandidáti: {}. Písmená sú pri každej skladbe zamiešané inak; ktorý model je ktoré písmeno, je v `key.json` — otvoriť až po hodnotení.\n",
        songs.len(),
        (0..n).map(letter).map(String::from).collect::<Vec<_>>().join(", ")
    ));
    for ((song, results), order) in songs.iter().zip(orders) {
        out.push_str(&format!("\n## {} ({})\n\n", song.title, song.youtube_id));
        for (i, line) in song.track.lines.iter().enumerate() {
            out.push_str(&format!("{}. EN: {}\n", i + 1, line.en));
            for (place, &cand) in order.iter().enumerate() {
                let text = match &results[cand] {
                    Ok(lines) => lines.get(i).cloned().unwrap_or_default(),
                    Err(_) => "(chyba)".to_string(),
                };
                out.push_str(&format!("   {}: {}\n", letter(place), text));
            }
        }
    }
    out
}

/// One letter's entry in the key.
#[derive(Serialize)]
struct KeyEntry<'a> {
    provider: &'a str,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

/// The key (pure): per song id, letter → candidate (+ its call's error).
pub fn key_json(
    candidates: &[Candidate],
    songs: &[(Song, SongResult)],
    orders: &[Vec<usize>],
) -> serde_json::Value {
    let mut by_song = serde_json::Map::new();
    for ((song, results), order) in songs.iter().zip(orders) {
        let mut letters = serde_json::Map::new();
        for (place, &cand) in order.iter().enumerate() {
            let entry = KeyEntry {
                provider: &candidates[cand].provider,
                model: &candidates[cand].model,
                error: results[cand].as_ref().err().map(String::as_str),
            };
            letters.insert(
                letter(place).to_string(),
                serde_json::to_value(entry).unwrap_or_default(),
            );
        }
        by_song.insert(song.youtube_id.clone(), serde_json::Value::Object(letters));
    }
    serde_json::Value::Object(by_song)
}

/// One song read from the box: its `{yt}_lyrics.json` (at least one line),
/// its title and its translation gender, read from the LOWEST row of the
/// YouTube id (the worker reads each row's own; rows of one video normally
/// agree).
pub async fn load_song(pool: &SqlitePool, cache_dir: &Path, youtube_id: &str) -> Result<Song> {
    let path = cache_dir.join(format!("{youtube_id}_lyrics.json"));
    let text = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("{youtube_id}: no lyrics file"))?;
    let track: LyricsTrack =
        serde_json::from_str(&text).with_context(|| format!("{youtube_id}: lyrics unreadable"))?;
    let row: Option<(i64, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, song, artist FROM videos WHERE youtube_id = ? ORDER BY id LIMIT 1",
    )
    .bind(youtube_id)
    .fetch_optional(pool)
    .await?;
    let (id, song, artist) = row.with_context(|| format!("{youtube_id}: no video row"))?;
    if track.lines.is_empty() {
        anyhow::bail!("{youtube_id}: the lyrics have no line");
    }
    let gender = match crate::db::models::get_translation_gender(pool, id).await? {
        Some(g) if g == "f" => SpeakerGender::Female,
        _ => SpeakerGender::Male,
    };
    let title = format!(
        "{} — {}",
        song.unwrap_or_default(),
        artist.unwrap_or_default()
    );
    Ok(Song {
        youtube_id: youtube_id.to_string(),
        title,
        track,
        gender,
    })
}

/// One candidate's translation of `song` with the production prompt.
pub async fn translate(
    client: &AiClient,
    c: &Candidate,
    song: &Song,
) -> Result<Vec<String>, String> {
    let numbered: String = song
        .track
        .lines
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}: {}", i + 1, l.en))
        .collect::<Vec<_>>()
        .join("\n");
    let n = song.track.lines.len();
    let prompt = build_prompt(n, &numbered, song.gender);
    let answer = match AiProvider::read(Some(&c.provider)) {
        AiProvider::ClaudeProxy => client.chat_with_timeout("", &prompt, CALL_TIMEOUT_S).await,
        AiProvider::OpenRouter => client
            .openrouter_chat(&c.model, "", &prompt, CALL_TIMEOUT_S, CALL_MAX_TOKENS)
            .await
            .map(|c| c.text),
    };
    answer
        .map(|text| parse_translation_response(&text, n))
        .map_err(|e| format!("{e:#}"))
}

/// Every song read before anything is sent: the songs, or an error naming
/// every id that cannot run (nothing spent).
pub async fn load_songs(pool: &SqlitePool, cache_dir: &Path, ids: &[String]) -> Result<Vec<Song>> {
    let mut songs = Vec::with_capacity(ids.len());
    let mut refused = Vec::new();
    for id in ids {
        match load_song(pool, cache_dir, id).await {
            Ok(song) => songs.push(song),
            Err(e) => refused.push(format!("{e:#}")),
        }
    }
    if !refused.is_empty() {
        anyhow::bail!(
            "no call was sent; songs that cannot run: {}",
            refused.join("; ")
        );
    }
    Ok(songs)
}

/// The whole run: every song read first ([`load_songs`]), then every song
/// through every candidate (concurrently per song), then the two files.
/// `progress(done)` after each song. Returns the run's dir.
pub async fn run(
    client: &AiClient,
    pool: &SqlitePool,
    cache_dir: &Path,
    ids: &[String],
    candidates: &[Candidate],
    seed: u64,
    progress: &(dyn Fn(usize) + Sync),
) -> Result<PathBuf> {
    let started_ms = crate::peer::wire::now_ms();
    let loaded = load_songs(pool, cache_dir, ids).await?;
    let dir = cache_dir.join("ab").join(format!("translate-{started_ms}"));
    tokio::fs::create_dir_all(&dir).await?;
    let mut songs = Vec::with_capacity(loaded.len());
    for (i, song) in loaded.into_iter().enumerate() {
        let results =
            futures::future::join_all(candidates.iter().map(|c| translate(client, c, &song))).await;
        songs.push((song, results));
        progress(i + 1);
    }
    let orders = song_orders(seed, songs.len(), candidates.len());
    let started = crate::peer::wire::ms_to_rfc3339(started_ms);
    tokio::fs::write(dir.join("blind.md"), blind_sheet(&started, &songs, &orders)).await?;
    let key = serde_json::to_string_pretty(&key_json(candidates, &songs, &orders))?;
    tokio::fs::write(dir.join("key.json"), key).await?;
    Ok(dir)
}

/// The process' one run state (the route's).
static STATUS: Mutex<Option<AbStatus>> = Mutex::new(None);

/// The last run's state (`idle` before one).
pub fn status() -> AbStatus {
    STATUS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .unwrap_or_else(|| AbStatus {
            state: "idle".into(),
            ..AbStatus::default()
        })
}

/// Take the run slot: `false` while a run holds it.
pub fn begin(total: usize) -> bool {
    let mut s = STATUS.lock().unwrap_or_else(|p| p.into_inner());
    if s.as_ref().is_some_and(|s| s.state == "running") {
        return false;
    }
    *s = Some(AbStatus {
        state: "running".into(),
        songs_total: total,
        ..AbStatus::default()
    });
    true
}

/// Held by the background run: when it is dropped with the slot still
/// `running` (the task panicked or was dropped), the slot reads `failed`,
/// so a later run is not refused until a restart.
pub struct RunGuard;

impl Drop for RunGuard {
    fn drop(&mut self) {
        update(|s| {
            if s.state == "running" {
                s.state = "failed".into();
                s.error = Some("the run ended without finishing (aborted)".into());
            }
        });
    }
}

/// Record progress / the end into the run slot.
pub fn update(f: impl FnOnce(&mut AbStatus)) {
    let mut s = STATUS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(s) = s.as_mut() {
        f(s);
    }
}

#[cfg(test)]
#[path = "translate_ab_tests.rs"]
mod tests;
