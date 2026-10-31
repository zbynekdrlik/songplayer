//! #284: the Slovak lines say "haleluja", never "aleluja" (the owner,
//! 18.10.2026: "namiesto aleluja pouzivat haleluja v prekladoch").
//!
//! [`haleluja`] is the one rule, applied where a Slovak line is written:
//! the translation (`worker_translation.rs::apply_translations`) and the
//! #250 register check's fix (`register_check.rs`). [`rewrite_stored`]
//! applies it once per start to the tracks already stored: the lyrics
//! worker runs it before its loop, so it never races a lyrics write (every
//! lyrics writer runs in that worker's tick). The EN lines, the timing and
//! a dub's subtitles are left alone; no `LYRICS_PIPELINE_VERSION` bump.

use std::borrow::Cow;
use std::path::Path;
use std::sync::LazyLock;

use regex::{Captures, Regex};
use sp_core::lyrics::LyricsTrack;
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE;

/// A word that starts with `aleluj` or `alleluj`, any case. At a word
/// boundary, so `haleluja` and a word that only contains the letters stay.
static ALELUJ: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?i:al{1,2}eluj)").expect("a valid pattern"));

/// `sk` with every `aleluj…` / `alleluj…` word written `haleluj…`, its case
/// kept (`Aleluja` → `Haleluja`, `ALELUJA` → `HALELUJA`, `alelujou` →
/// `halelujou`); borrowed when nothing changes.
pub fn haleluja(sk: &str) -> Cow<'_, str> {
    ALELUJ.replace_all(sk, |found: &Captures| haleluj_like(&found[0]))
}

/// `haleluj` in the case of `found` (the word's matched start): all capitals,
/// a capital first, or lower case.
fn haleluj_like(found: &str) -> &'static str {
    if found.chars().all(char::is_uppercase) {
        "HALELUJ"
    } else if found.chars().next().is_some_and(char::is_uppercase) {
        "Haleluj"
    } else {
        "haleluj"
    }
}

/// [`haleluja`] over every Slovak line of `track`; how many lines changed.
pub fn fix_track(track: &mut LyricsTrack) -> usize {
    let mut changed = 0;
    for line in &mut track.lines {
        let Some(Cow::Owned(fixed)) = line.sk.as_deref().map(haleluja) else {
            continue;
        };
        line.sk = Some(fixed);
        changed += 1;
    }
    changed
}

/// What one [`rewrite_stored`] pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rewritten {
    /// Tracks written back, and their lines that changed.
    pub tracks: usize,
    pub lines: usize,
    /// Tracks that could not be read, parsed or written (each WARNed).
    pub failed: usize,
}

/// The videos whose served `{yt}_lyrics.json` is a song's own track: a row
/// with `has_lyrics = 1` and no row of the video asking a dub or serving a
/// Live-Translate track (that file is the dub's subtitles).
const SERVED_SONGS: &str = "SELECT DISTINCT v.youtube_id FROM videos v WHERE v.has_lyrics = 1 \
     AND NOT EXISTS (SELECT 1 FROM videos d WHERE d.youtube_id = v.youtube_id \
     AND (d.dub_requested = 1 OR d.lyrics_source = ?)) ORDER BY v.youtube_id";

/// #284: write every stored song track whose Slovak lines say `aleluja`
/// back with [`haleluja`] applied; a track with nothing to change is not
/// touched. A file that cannot be read, parsed or written is WARNed and
/// left as it is; one INFO per pass names what changed.
pub async fn rewrite_stored(pool: &SqlitePool, cache_dir: &Path) -> Result<Rewritten, sqlx::Error> {
    let ids: Vec<String> = sqlx::query_scalar(SERVED_SONGS)
        .bind(SOURCE_LIVE_TRANSLATE)
        .fetch_all(pool)
        .await?;
    let mut done = Rewritten::default();
    for youtube_id in ids {
        let path = cache_dir.join(format!("{youtube_id}_lyrics.json"));
        match rewrite_one(&path).await {
            Ok(0) => {}
            Ok(lines) => {
                done.tracks += 1;
                done.lines += lines;
            }
            Err(e) => {
                warn!(%youtube_id, error = %e, "lyrics: the haleluja pass left a track as it was");
                done.failed += 1;
            }
        }
    }
    info!(
        tracks = done.tracks,
        lines = done.lines,
        failed = done.failed,
        "lyrics: the haleluja pass over the stored Slovak lines is done (#284)"
    );
    Ok(done)
}

/// One track: the lines [`fix_track`] changed, written back only when any
/// did (0: nothing written). A missing file is nothing to change.
async fn rewrite_one(path: &Path) -> anyhow::Result<usize> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut track: LyricsTrack = serde_json::from_slice(&bytes)?;
    let lines = fix_track(&mut track);
    if lines > 0 {
        tokio::fs::write(path, serde_json::to_vec(&track)?).await?;
    }
    Ok(lines)
}

#[cfg(test)]
#[path = "haleluja_tests.rs"]
mod tests;
