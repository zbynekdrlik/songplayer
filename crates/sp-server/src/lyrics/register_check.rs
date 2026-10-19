//! #250: the register check — ONE Claude call per track, made while the track
//! is processed (right after its translation, before it is persisted), never
//! at display. Owner, 11.10.2026: the songs are Christian worship songs, and a
//! transcript that misheard "I will dwell in the house of the Lord" as "I
//! won't swell" (translated "Nenapuchnem") must not reach a church wall.
//!
//! The transcriber cannot be told the context: Gemini 3.5 Transcribe takes no
//! prompt, and its only text input (`custom_vocabulary`) is refused together
//! with the word timestamps the base tier needs (#250 design comment). So the
//! check runs after it, on the finished EN + SK lines:
//!
//! - a line the model can correct is FIXED in the track (EN and SK; its
//!   timing untouched, its old text kept in [`RegisterCheck::fixed`]);
//! - a line it cannot correct is FLAGGED for the operator and left as it is;
//! - more corrections than [`max_fixes`] is a rewrite, not a fix: nothing is
//!   applied, every listed line is flagged;
//! - a refusal is recorded as [`RegisterVerdict::Refused`] and changes
//!   nothing.
//!
//! The prompt is the translator's sibling: numbered lines in, numbered lines
//! out, nothing else (`translator.rs` "Prompt design" — the content-policy
//! classifier of Claude via CLIProxyAPI is why).

use anyhow::Result;
use sp_core::lyrics::{LyricsTrack, RegisterCheck, RegisterLine, RegisterVerdict};

/// The check's version. A served track checked under an older one (or never)
/// is checked again by the stale pass (`worker_register.rs`). Bump it only
/// when the prompt or the apply rules change what the check would do.
pub const REGISTER_CHECK_VERSION: u32 = 1;

/// The answer that says every line reads right.
const ALL_FINE: &str = "NONE";

/// What the model said about one line (0-based `index`).
#[derive(Debug, Clone, PartialEq)]
pub enum Finding {
    /// A corrected English line and its Slovak.
    Fix {
        index: usize,
        en: String,
        sk: String,
    },
    /// The line is wrong, but the model cannot tell what was meant.
    Unsure { index: usize },
}

impl Finding {
    fn index(&self) -> usize {
        match self {
            Self::Fix { index, .. } | Self::Unsure { index } => *index,
        }
    }
}

/// The prompt for `numbered` (`N: english || slovak` lines).
pub fn build_prompt(numbered: &str) -> String {
    format!(
        "Each numbered line below is English text from an automatic speech \
         recognition of contemporary Christian worship music, followed by \
         \" || \" and its Slovak rendering. Biblical and devotional phrasing is \
         expected (for example \"I will dwell in the house of the Lord\"). Find \
         ONLY (a) the lines where the recognition clearly misheard a well-known \
         biblical or devotional phrase, and (b) the lines whose English or Slovak \
         reads as vulgar, crude or indecent. Leave every other line alone, even \
         when it reads oddly. For each such line output one line \"N | corrected \
         English | corrected Slovak\"; when you cannot tell what was meant, output \
         \"N | ? | ?\". If every line reads correctly, output only {ALL_FINE}. \
         Output nothing else.\n\n{numbered}"
    )
}

/// `track`'s lines as the prompt numbers them, 1-based.
pub fn numbered_lines(track: &LyricsTrack) -> String {
    track
        .lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let sk = line.sk.as_deref().unwrap_or("");
            format!("{}: {} || {}", i + 1, line.en, sk)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the model's answer over a track of `line_count` lines. `Some(vec![])`
/// for [`ALL_FINE`]; `None` when the answer holds neither it nor one finding
/// (a refusal, or an answer in another shape). A line number outside the
/// track, or one already given, is ignored.
pub fn parse_response(text: &str, line_count: usize) -> Option<Vec<Finding>> {
    let mut findings: Vec<Finding> = Vec::new();
    let mut all_fine = false;
    for raw in text.lines() {
        let line = raw.trim();
        // A trailing full stop is still the answer ("NONE.").
        if line
            .trim_end_matches(['.', '!'])
            .eq_ignore_ascii_case(ALL_FINE)
        {
            all_fine = true;
            continue;
        }
        let Some(finding) = parse_finding(line, line_count) else {
            continue;
        };
        if findings.iter().all(|f| f.index() != finding.index()) {
            findings.push(finding);
        }
    }
    (all_fine || !findings.is_empty()).then_some(findings)
}

/// One `N | english | slovak` line, or `None`.
fn parse_finding(line: &str, line_count: usize) -> Option<Finding> {
    let mut parts = line.split('|').map(str::trim);
    let number: usize = parts.next()?.parse().ok()?;
    let en = parts.next()?;
    let sk = parts.next()?;
    if parts.next().is_some() || number == 0 || number > line_count {
        return None;
    }
    let index = number - 1;
    if unknown(en) || unknown(sk) {
        return Some(Finding::Unsure { index });
    }
    Some(Finding::Fix {
        index,
        en: en.to_string(),
        sk: sk.to_string(),
    })
}

/// An empty or `?` field: the model could not say.
fn unknown(field: &str) -> bool {
    field.is_empty() || field == "?"
}

/// The most corrections one check applies to a track of `line_count` lines:
/// a quarter of them, at least 3. More is the model rewriting the song from
/// memory, not fixing a mishearing.
pub fn max_fixes(line_count: usize) -> usize {
    (line_count / 4).max(3)
}

/// May the check rewrite `track`'s lines? Not an operator's own text (the
/// `lyrics_override_text` sources) nor a ★ text (a scraped lyric the
/// transcript verified, `REFERENCE_SOURCE_SUFFIX`): their lines are only
/// flagged.
pub fn may_fix(track: &LyricsTrack) -> bool {
    !(track.source.contains("override")
        || track
            .source
            .ends_with(crate::lyrics::REFERENCE_SOURCE_SUFFIX))
}

/// Apply `findings` to `track` and return the check to store with it. A fix
/// that equals the line as it is changes nothing and is not listed. A track
/// the check may not rewrite ([`may_fix`]) has every finding flagged.
pub fn apply(track: &mut LyricsTrack, findings: Vec<Finding>) -> RegisterCheck {
    fn line_of(track: &LyricsTrack, index: usize) -> RegisterLine {
        let line = &track.lines[index];
        RegisterLine {
            index,
            en: line.en.clone(),
            sk: line.sk.clone(),
        }
    }
    let changes = |f: &Finding| match f {
        Finding::Fix { index, en, sk } => {
            let line = &track.lines[*index];
            line.en != *en || line.sk.as_deref() != Some(sk.as_str())
        }
        Finding::Unsure { .. } => true,
    };
    let findings: Vec<Finding> = findings.into_iter().filter(changes).collect();
    let fix_count = findings
        .iter()
        .filter(|f| matches!(f, Finding::Fix { .. }))
        .count();
    // A rewrite, or a text the check may not touch: every finding is flagged.
    let flag_all = fix_count > max_fixes(track.lines.len()) || !may_fix(track);
    let mut fixed = Vec::new();
    let mut flagged = Vec::new();
    for finding in findings {
        match finding {
            Finding::Fix { index, en, sk } if !flag_all => {
                fixed.push(line_of(track, index));
                let line = &mut track.lines[index];
                line.en = en;
                line.sk = Some(super::haleluja::haleluja(&sk).into_owned());
                // Word timings belong to the words that were there.
                line.words = None;
            }
            other => flagged.push(line_of(track, other.index())),
        }
    }
    fixed.sort_by_key(|l| l.index);
    flagged.sort_by_key(|l| l.index);
    let verdict = if !flagged.is_empty() {
        RegisterVerdict::Flagged
    } else if !fixed.is_empty() {
        RegisterVerdict::Fixed
    } else {
        RegisterVerdict::Ok
    };
    RegisterCheck {
        version: REGISTER_CHECK_VERSION,
        verdict,
        fixed,
        flagged,
    }
}

/// The check of a track the model refused: nothing changed.
pub fn refused() -> RegisterCheck {
    RegisterCheck {
        version: REGISTER_CHECK_VERSION,
        verdict: RegisterVerdict::Refused,
        fixed: Vec::new(),
        flagged: Vec::new(),
    }
}

/// What the model's answer means for the track: its findings, or a refusal,
/// or an error for an answer in another shape (checked again later).
pub fn read_answer(text: &str, line_count: usize) -> Result<Option<Vec<Finding>>> {
    if let Some(findings) = parse_response(text, line_count) {
        return Ok(Some(findings));
    }
    if crate::lyrics::translator::is_refusal_shaped(text) {
        return Ok(None);
    }
    Err(UnreadableAnswer(text.chars().take(300).collect()).into())
}

/// The model answered, in a shape the check cannot read (its first 300
/// characters). Told apart from a failed call ([`unreadable`]): only this
/// counts toward giving a song up — a proxy outage never does.
#[derive(Debug)]
pub struct UnreadableAnswer(pub String);

impl std::fmt::Display for UnreadableAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "register check: an answer in another shape: {}", self.0)
    }
}

impl std::error::Error for UnreadableAnswer {}

/// Is `error` (or one of its causes) an [`UnreadableAnswer`]?
pub fn unreadable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<UnreadableAnswer>())
}

/// Ask Claude and apply its answer to `track`. A refusal (in its answer, or
/// Claude's upstream content filter) is a [`RegisterVerdict::Refused`] check;
/// any other failure is an error and the track is left as it was.
#[cfg_attr(test, mutants::skip)] // HTTP glue; its parts are tested
pub async fn check_via_claude(
    ai_client: &crate::ai::client::AiClient,
    track: &mut LyricsTrack,
) -> Result<RegisterCheck> {
    if track.lines.is_empty() {
        return Ok(apply(track, Vec::new()));
    }
    let prompt = build_prompt(&numbered_lines(track));
    let answer = match ai_client.chat("", &prompt).await {
        Ok(answer) => answer,
        Err(e) if crate::ai::retry::content_filtered(&e) => return Ok(refused()),
        Err(e) => return Err(e.context("register check: the Claude call failed")),
    };
    match read_answer(&answer, track.lines.len())? {
        Some(findings) => Ok(apply(track, findings)),
        None => Ok(refused()),
    }
}

#[cfg(test)]
#[path = "register_check_tests.rs"]
mod tests;
