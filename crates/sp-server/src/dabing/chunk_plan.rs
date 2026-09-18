//! Pure dub chunking + placement decisions (#183 D4).
//!
//! The Gemini Live Translate session has a length ceiling, so a long video is
//! split into chunks — but ONLY at speech PAUSES (silences), never mid-sentence,
//! so the translated speech never loses a word across a boundary. These are the
//! two canonical, unit-tested decisions:
//!
//! - [`plan_chunks`] — where to cut, from the ffmpeg-detected silences (the
//!   worker runs `silencedetect`, parses the intervals, and calls this to build
//!   the child's `--chunk-plan` input).
//! - [`placement_for`] — where each chunk's TRANSLATED output lands on the video
//!   timeline, and whether to speed it up (`atempo`, capped at [`MAX_TEMPO`]) so
//!   it does not overrun the next chunk. `out_len` is only known after the Live
//!   call returns, so the worker calls this per chunk to log + verify the drift;
//!   the Python child implements the same placement at runtime.

use serde::{Deserialize, Serialize};

/// A silence interval detected in the source audio, milliseconds from the start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Silence {
    pub start_ms: u64,
    pub end_ms: u64,
}

impl Silence {
    /// Length of the silence.
    pub fn len_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }

    /// Midpoint of the silence — the point a chunk boundary is placed at, so both
    /// the chunk that ends and the one that begins keep a little of the pause.
    pub fn mid_ms(&self) -> u64 {
        self.start_ms + self.len_ms() / 2
    }
}

/// The minimum silence length that qualifies as a chunk boundary (700 ms — a real
/// speech pause, not the micro-gaps between words).
pub const MIN_PAUSE_MS: u64 = 700;

/// The maximum chunk length (8 minutes) — headroom under the Live session limit.
pub const MAX_CHUNK_MS: u64 = 8 * 60 * 1000;

/// Chunk-plan tuning. Defaults are [`MIN_PAUSE_MS`] / [`MAX_CHUNK_MS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkPlanConfig {
    /// Minimum silence length to cut at.
    pub min_pause_ms: u64,
    /// Maximum chunk length.
    pub max_chunk_ms: u64,
}

impl Default for ChunkPlanConfig {
    fn default() -> Self {
        Self {
            min_pause_ms: MIN_PAUSE_MS,
            max_chunk_ms: MAX_CHUNK_MS,
        }
    }
}

/// One planned chunk `[start_ms, end_ms)` of the source timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub start_ms: u64,
    pub end_ms: u64,
}

impl Chunk {
    /// Chunk length.
    pub fn len_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }
}

/// Split `[0, total_ms)` into chunks no longer than `cfg.max_chunk_ms`, cutting
/// ONLY at silence midpoints whose silence is at least `cfg.min_pause_ms` long
/// (never mid-speech). Short audio (`<= max_chunk_ms`) is a single chunk.
///
/// Greedy: from each chunk start, pick the LATEST eligible silence midpoint that
/// still fits inside `max_chunk_ms`; if a stretch genuinely has no qualifying
/// pause within the ceiling (very rare for speech), it falls back to a hard cut
/// at the ceiling so the Live session limit is still honoured. Always makes
/// forward progress, so it terminates.
pub fn plan_chunks(silences: &[Silence], total_ms: u64, cfg: &ChunkPlanConfig) -> Vec<Chunk> {
    if total_ms == 0 {
        return Vec::new();
    }
    if total_ms <= cfg.max_chunk_ms {
        return vec![Chunk {
            start_ms: 0,
            end_ms: total_ms,
        }];
    }

    let mut chunks = Vec::new();
    let mut start = 0u64;
    while start < total_ms {
        // Final chunk: the remainder fits.
        if total_ms - start <= cfg.max_chunk_ms {
            chunks.push(Chunk {
                start_ms: start,
                end_ms: total_ms,
            });
            break;
        }
        let limit = start + cfg.max_chunk_ms;
        let cut = silences
            .iter()
            .filter(|s| s.len_ms() >= cfg.min_pause_ms)
            .map(|s| s.mid_ms())
            .filter(|&m| m > start && m <= limit)
            .max()
            // No qualifying pause within the ceiling: forced hard cut at the
            // ceiling so a session is never asked to exceed its limit.
            .unwrap_or(limit);
        // Guarantee forward progress and stay within the timeline.
        let end = cut.clamp(start + 1, total_ms);
        chunks.push(Chunk {
            start_ms: start,
            end_ms: end,
        });
        start = end;
    }
    chunks
}

/// The maximum speed-up applied to a chunk's translated output to fit before the
/// next chunk (ffmpeg `atempo`). 1.08 keeps the voice natural; we NEVER slow
/// down (tempo `>= 1.0`) and never exceed this.
pub const MAX_TEMPO: f32 = 1.08;

/// Where a chunk's translated output is placed, and how fast it plays.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Offset on the video timeline where the output starts (= the chunk start).
    pub at_ms: u64,
    /// `atempo` factor: `1.0` = unchanged, up to [`MAX_TEMPO`] when the output
    /// would otherwise overrun the next chunk.
    pub tempo: f32,
}

/// Decide where a chunk's translated output goes and whether to speed it up.
///
/// The output starts at the chunk's own start (`chunk_start`). If placing
/// `out_len` there would run past `next_start` (the following chunk's start),
/// the output is sped up just enough to fit — but never faster than
/// [`MAX_TEMPO`], and never slower than real time. The last chunk (`next_start`
/// `None`) is never sped up: nothing follows it. `chunk_len` is the source-chunk
/// length, unused by the decision but kept for the drift log at the call site.
pub fn placement_for(
    chunk_start: u64,
    _chunk_len: u64,
    out_len: u64,
    next_start: Option<u64>,
) -> Placement {
    let tempo = match next_start {
        Some(ns) if ns > chunk_start => {
            let avail = ns - chunk_start;
            if avail > 0 && out_len > avail {
                let needed = out_len as f32 / avail as f32;
                needed.clamp(1.0, MAX_TEMPO)
            } else {
                1.0
            }
        }
        _ => 1.0,
    };
    Placement {
        at_ms: chunk_start,
        tempo,
    }
}

/// Signed drift of a chunk's translated output vs its source length, in ms
/// (`out_len - chunk_len`): positive = the SK output is longer than the source.
/// Logged and bounds-checked (`<= 2 s`) by the worker after each chunk.
pub fn drift_ms(chunk_len: u64, out_len: u64) -> i64 {
    out_len as i64 - chunk_len as i64
}

/// Parse `ffmpeg -af silencedetect ... -f null -` stderr into the total media
/// duration + the detected silence intervals (ms). ffmpeg prints, on stderr:
///   `  Duration: HH:MM:SS.ff, start: ...`
///   `[silencedetect @ ..] silence_start: 12.345`
///   `[silencedetect @ ..] silence_end: 13.567 | silence_duration: 1.222`
/// A dangling `silence_start` with no matching `silence_end` (silence runs to EOF)
/// is closed at `total_ms` when known. Pure — unit-tested on captured output.
pub fn parse_silencedetect(stderr: &str) -> (Option<u64>, Vec<Silence>) {
    let mut total_ms: Option<u64> = None;
    let mut silences = Vec::new();
    let mut pending_start: Option<u64> = None;

    for line in stderr.lines() {
        let l = line.trim();
        if total_ms.is_none()
            && let Some(idx) = l.find("Duration:")
        {
            let rest = &l[idx + "Duration:".len()..];
            let ts = rest.trim_start().split(',').next().unwrap_or("").trim();
            total_ms = parse_hms_ms(ts);
        }
        if let Some(idx) = l.find("silence_start:") {
            let v = l[idx + "silence_start:".len()..].trim();
            if let Some(ms) = parse_secs_ms(v) {
                pending_start = Some(ms);
            }
        }
        if let Some(idx) = l.find("silence_end:") {
            // "13.567 | silence_duration: 1.222" — take the first token.
            let after = &l[idx + "silence_end:".len()..];
            let end_tok = after.split('|').next().unwrap_or("").trim();
            if let (Some(start_ms), Some(end_ms)) = (pending_start, parse_secs_ms(end_tok)) {
                silences.push(Silence {
                    start_ms,
                    end_ms: end_ms.max(start_ms),
                });
                pending_start = None;
            }
        }
    }
    // A silence that runs to the end of file has no `silence_end`.
    if let (Some(start_ms), Some(t)) = (pending_start, total_ms)
        && t > start_ms
    {
        silences.push(Silence {
            start_ms,
            end_ms: t,
        });
    }
    (total_ms, silences)
}

/// Parse `SS.fff` (seconds, fractional) into milliseconds. `None` on a bad token.
fn parse_secs_ms(s: &str) -> Option<u64> {
    let secs: f64 = s.trim().parse().ok()?;
    if secs < 0.0 {
        return None;
    }
    Some((secs * 1000.0).round() as u64)
}

/// Parse `HH:MM:SS.ff` into milliseconds. `None` on a malformed timestamp.
fn parse_hms_ms(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: u64 = parts[0].trim().parse().ok()?;
    let m: u64 = parts[1].trim().parse().ok()?;
    let sec: f64 = parts[2].trim().parse().ok()?;
    Some((h * 3600 + m * 60) * 1000 + (sec * 1000.0).round() as u64)
}

#[cfg(test)]
#[path = "chunk_plan_tests.rs"]
mod tests;
