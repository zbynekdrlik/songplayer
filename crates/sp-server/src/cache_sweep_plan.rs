//! #261 (no infinite archive): what the daily cache sweep deletes, decided
//! from a listing of the cache and the rows' facts, with no I/O
//! ([`plan`]; the listing and the deletes are `cache_sweep`'s).
//!
//! An entry goes only when it is KNOWN and old enough:
//!
//! - [`Why::NoRow`]: named after a YouTube id no `videos` row has, recorded
//!   by no row, unchanged for [`ORPHAN_AFTER_MS`] (7 d), and only while the
//!   database has a row at all (an empty or replaced one would make every
//!   song a video no row has: [`RowFacts::is_empty`]). Named after a
//!   video means a song file (`{song}_{artist}_{id}_normalized[_gf]…`, song
//!   and artist carry no `_`, so the id is the 11 characters before
//!   `_normalized`: [`SONG_RE`]), an id-first file of a known kind
//!   ([`ID_FILE_RE`]) or a job's work dir (`{id}_isolation` / `_stemsep` /
//!   `_dub`, [`WORK_DIR_RE`]). Never a generic `{11 chars}_…` match: a song
//!   named `HolySpirit1_…` would read as a video id.
//! - [`Why::Stale`]: a name no code writes any more, whatever its video
//!   (unchanged for [`STALE_AFTER_MS`], 7 d): a retired pipeline's output
//!   ([`RETIRED_KINDS`]) or a cleanup cache older than the code's
//!   `_cleaned_v`[`CLEANED_VERSION`] (or unversioned). A newer version is
//!   kept (a rolled-back build must not eat a newer build's caches).
//! - [`Why::Replaced`]: a set-aside `<name>.replaced` / `<name>.N.replaced`
//!   (`cache::set_aside_name`) of a song file (`<name>` matches [`SONG_RE`])
//!   unchanged for [`REPLACED_AFTER_MS`] (30 d),
//!   only while `<name>` holds a file: it is then the stale copy the unit
//!   move meant to delete; with `<name>` empty it may be the only copy of a
//!   song file, so it stays (the self-heal WARNs it at every start).
//! - [`Why::DubPrev`]: a dub's `<dub>.flac.prev` (`dub_worker.py trim-tail`),
//!   [`DUB_PREV_AFTER_MS`] (14 d) after the LATER of its own and its dub's
//!   last write (the copy keeps the original dub's time; the cut dub is
//!   written at the trim), only while its dub is there.
//! - [`Why::Part`]: a `<cache>/peer/*.part` unchanged for [`PART_AFTER_MS`]
//!   (7 d); a fetch resumes from a younger one.
//!
//! "Unchanged for" reads the entry's last write (a work dir: the newest in
//! its tree); an entry whose time cannot be read never goes. A file a row
//! records (any of its five path columns, by file name, ignoring case like
//! NTFS) never goes; ids and song bases compare ignoring case too: a match
//! only KEEPS more. At most [`MAX_PER_PASS`] entries a pass, the oldest
//! first; the rest wait for the next day.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

/// A day.
pub const DAY_MS: i64 = 86_400_000;
/// A file of a video no row has goes this long after its last write (7 d).
pub const ORPHAN_AFTER_MS: i64 = 604_800_000;
/// A name no code writes any more goes this long after its last write (7 d).
pub const STALE_AFTER_MS: i64 = 604_800_000;
/// A peer fetch's part goes this long after its last write (7 d).
pub const PART_AFTER_MS: i64 = 604_800_000;
/// A set-aside `.replaced` goes this long after its last write (30 d).
pub const REPLACED_AFTER_MS: i64 = 2_592_000_000;
/// A dub's `.flac.prev` goes this long after the trim (14 d).
pub const DUB_PREV_AFTER_MS: i64 = 1_209_600_000;
/// The most entries one pass deletes.
pub const MAX_PER_PASS: usize = 500;
/// The cleanup cache version the code writes (`lyrics/gather.rs`,
/// `lyrics/title_search.rs`: `_cleaned_v3.json`; a test reads both, so a
/// bump there fails until this follows it).
pub const CLEANED_VERSION: u32 = 3;

/// The outputs of retired pipelines, after `{id}_` (git history: the v20
/// WhisperX route, the Gemini chunked era, the description merge, the
/// AssemblyAI path).
pub const RETIRED_KINDS: [&str; 9] = [
    "whisperx_track.json",
    "descmerge_audit.json",
    "asr_path_audit.json",
    "gemini_chunks.json",
    "chunks.json",
    "asr_output.json",
    "align_output.json",
    "align_out.json",
    "vocals_dereverbed.wav",
];

/// A song file: the base `{song}_{artist}_{id}_normalized[_gf]` (1), the id
/// (2) and the rest (3): `_video.mp4`, `_audio.flac`, the stems, the dub and
/// its transcripts / events, a `.prev`, a `.replaced`, a job's `.tmp`.
pub static SONG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([^_]*_[^_]*_([A-Za-z0-9_-]{11})_normalized(?:_gf)?)([_.].*)$").unwrap()
});

/// An id-first file of a known kind (current, download temps, retired).
pub static ID_FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^([A-Za-z0-9_-]{11})_(?:lyrics\.json|vocals16k\.wav|g35t_words(?:_used)?\.json|g35t_audit\.json|alignment_audit\.json|title_search_audit\.json|(?:lrclib|genius)_cleaned(?:_v[0-9]+)?\.json|title_.+_cleaned(?:_v[0-9]+)?\.json|description\.txt|description_lyrics\.json|mtl_(?:text|out)\.json|(?:video|audio)(?:_upgrade|_repair)?_temp(?:\..+)?|whisperx_track\.json|descmerge_audit\.json|asr_path_audit\.json|gemini_chunks\.json|chunks\.json|asr_output\.json|align_output\.json|align_out\.json|vocals_dereverbed\.wav)$",
    )
    .unwrap()
});

/// A job's work dir: the lyrics isolation, the stem separation, the dub.
pub static WORK_DIR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Za-z0-9_-]{11})_(?:isolation|stemsep|dub)$").unwrap());

/// A cleanup cache and its version (none = the unversioned first one).
static CLEANED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9_-]{11}_(?:lrclib|genius|title_.+)_cleaned(?:_v([0-9]+))?\.json$")
        .unwrap()
});

/// Where an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Place {
    /// The cache dir itself.
    Cache,
    /// `<cache>/peer/`, a peer fetch's parts.
    Parts,
}

/// One entry of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub place: Place,
    /// Its name in its dir (one path component).
    pub name: String,
    pub is_dir: bool,
    /// Its bytes (a dir: its whole tree).
    pub bytes: u64,
    /// Its last write, UTC ms (a dir: the newest in its tree); `None` when it
    /// could not be read (never deleted).
    pub modified_ms: Option<i64>,
}

/// Why an entry goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Why {
    NoRow,
    Stale,
    Replaced,
    DubPrev,
    Part,
}

impl Why {
    /// Its name in the sweep's log line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoRow => "no_row",
            Self::Stale => "stale",
            Self::Replaced => "replaced",
            Self::DubPrev => "dub_prev",
            Self::Part => "part",
        }
    }
}

/// What the rows say, read once: every row's id, every file a row records
/// and every song base a recorded file names; all lowercased.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowFacts {
    ids: HashSet<String>,
    recorded: HashSet<String>,
    bases: HashSet<String>,
}

impl RowFacts {
    /// One row: its YouTube id and its path columns (`None` / `""` = none).
    pub fn add_row(&mut self, youtube_id: &str, paths: &[Option<&str>]) {
        self.ids.insert(youtube_id.to_lowercase());
        for path in paths.iter().flatten().filter(|p| !p.is_empty()) {
            let name = file_name_of(path).to_lowercase();
            if let Some(caps) = SONG_RE.captures(&name) {
                self.bases.insert(caps[1].to_string());
            }
            self.recorded.insert(name);
        }
    }

    /// Whether the database has no row at all: then no file is taken for a
    /// video no row has (a fresh or replaced database must not empty the
    /// cache).
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Whether a row has the video `id` or records a file of the song
    /// `base`.
    fn has(&self, id: &str, base: Option<&str>) -> bool {
        self.ids.contains(&id.to_lowercase())
            || base.is_some_and(|b| self.bases.contains(&b.to_lowercase()))
    }

    /// Whether a file of the video `id` (of the song `base`) may go as a
    /// video no row has: the database has rows, and none of them has the
    /// video or records a file of the song.
    pub fn may_take(&self, id: &str, base: Option<&str>) -> bool {
        !self.is_empty() && !self.has(id, base)
    }

    /// Whether a row records a file of this name.
    fn records(&self, name: &str) -> bool {
        self.recorded.contains(&name.to_lowercase())
    }
}

/// The last component of a path as a row stores it (`\` or `/`: a box's
/// Windows paths read on any system).
pub fn file_name_of(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// The video an entry is named after: `(id, song base)`, the base only for
/// a song file.
pub fn video_of(name: &str, is_dir: bool) -> Option<(&str, Option<&str>)> {
    if is_dir {
        return WORK_DIR_RE
            .captures(name)
            .and_then(|caps| caps.get(1))
            .map(|id| (id.as_str(), None));
    }
    if let Some(caps) = SONG_RE.captures(name) {
        let (base, id) = (caps.get(1)?, caps.get(2)?);
        return Some((id.as_str(), Some(base.as_str())));
    }
    ID_FILE_RE
        .captures(name)
        .and_then(|caps| caps.get(1))
        .map(|id| (id.as_str(), None))
}

/// Whether a file's name is one no code writes any more (module doc).
pub fn is_stale_name(name: &str) -> bool {
    let retired = name
        .get(12..)
        .is_some_and(|kind| RETIRED_KINDS.contains(&kind))
        && ID_FILE_RE.is_match(name);
    retired
        || CLEANED_RE.captures(name).is_some_and(|caps| {
            caps.get(1)
                .map_or(Some(0), |v| v.as_str().parse::<u32>().ok())
                .is_some_and(|version| version < CLEANED_VERSION)
        })
}

/// The name a set-aside file was set aside from: `<name>.replaced` and
/// `<name>.N.replaced` → `<name>`.
pub fn replaced_owner(name: &str) -> Option<&str> {
    let owner = name.strip_suffix(".replaced")?;
    Some(match owner.rsplit_once('.') {
        Some((rest, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => rest,
        _ => owner,
    })
    .filter(|owner| !owner.is_empty())
}

/// The dub a song's `<dub>.flac.prev` belongs to (`<base>_dub.flac`).
pub fn dub_prev_owner(name: &str) -> Option<&str> {
    let dub = name.strip_suffix(".prev")?;
    let caps = SONG_RE.captures(dub)?;
    (caps.get(3)?.as_str() == "_dub.flac").then_some(dub)
}

/// Whether `modified_ms` is at least `after_ms` before `now_ms`.
fn older(modified_ms: Option<i64>, now_ms: i64, after_ms: i64) -> bool {
    modified_ms.is_some_and(|m| now_ms.saturating_sub(m) >= after_ms)
}

/// Why `entry` goes now, if it does (module doc). `files` = the cache dir's
/// files by name, with their last write.
pub fn why(
    entry: &Entry,
    files: &HashMap<&str, Option<i64>>,
    facts: &RowFacts,
    now_ms: i64,
) -> Option<Why> {
    let name = entry.name.as_str();
    if entry.place == Place::Parts {
        let part = !entry.is_dir && name.ends_with(".part");
        return (part && older(entry.modified_ms, now_ms, PART_AFTER_MS)).then_some(Why::Part);
    }
    if facts.records(name) {
        return None;
    }
    if let Some((id, base)) = video_of(name, entry.is_dir)
        && facts.may_take(id, base)
        && older(entry.modified_ms, now_ms, ORPHAN_AFTER_MS)
    {
        return Some(Why::NoRow);
    }
    if entry.is_dir {
        return None;
    }
    if is_stale_name(name) && older(entry.modified_ms, now_ms, STALE_AFTER_MS) {
        return Some(Why::Stale);
    }
    if replaced_owner(name)
        .is_some_and(|owner| SONG_RE.is_match(owner) && files.contains_key(owner))
        && older(entry.modified_ms, now_ms, REPLACED_AFTER_MS)
    {
        return Some(Why::Replaced);
    }
    let dub = dub_prev_owner(name).and_then(|dub| files.get(dub).copied());
    if let Some(dub_ms) = dub {
        let trimmed = entry.modified_ms.zip(dub_ms).map(|(a, b)| a.max(b));
        if older(trimmed, now_ms, DUB_PREV_AFTER_MS) {
            return Some(Why::DubPrev);
        }
    }
    None
}

/// One entry to delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub place: Place,
    pub name: String,
    pub is_dir: bool,
    pub bytes: u64,
    pub why: Why,
}

/// A pass's deletes, the oldest first, and how many more were due.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub delete: Vec<Planned>,
    pub left: usize,
}

/// What a pass deletes (module doc): every entry [`why`] names, the oldest
/// first, at most [`MAX_PER_PASS`].
pub fn plan(entries: &[Entry], facts: &RowFacts, now_ms: i64) -> Plan {
    let files: HashMap<&str, Option<i64>> = entries
        .iter()
        .filter(|e| e.place == Place::Cache && !e.is_dir)
        .map(|e| (e.name.as_str(), e.modified_ms))
        .collect();
    let mut due: Vec<(Option<i64>, Planned)> = entries
        .iter()
        .filter_map(|e| {
            why(e, &files, facts, now_ms).map(|reason| {
                let planned = Planned {
                    place: e.place,
                    name: e.name.clone(),
                    is_dir: e.is_dir,
                    bytes: e.bytes,
                    why: reason,
                };
                (e.modified_ms, planned)
            })
        })
        .collect();
    due.sort_by(|(a, x), (b, y)| {
        a.cmp(b)
            .then_with(|| (x.place, &x.name).cmp(&(y.place, &y.name)))
    });
    let left = due.len().saturating_sub(MAX_PER_PASS);
    due.truncate(MAX_PER_PASS);
    Plan {
        delete: due.into_iter().map(|(_, planned)| planned).collect(),
        left,
    }
}

#[cfg(test)]
#[path = "cache_sweep_plan_tests.rs"]
mod tests;
