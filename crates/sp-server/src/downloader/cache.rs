//! Cache scanning and cleanup — manages normalized song files on disk.
//!
//! The pipeline stores each processed song as two sidecar files that share
//! a common base name:
//!
//! ```text
//! {safe_song}_{safe_artist}_{video_id}_normalized[_gf]_video.mp4
//! {safe_song}_{safe_artist}_{video_id}_normalized[_gf]_audio.flac
//! ```
//!
//! `scan_cache` walks the directory and returns three disjoint sets:
//!
//! * [`ScanResult::songs`] — complete video+audio pairs.
//! * [`ScanResult::legacy`] — pre-migration single `.mp4` files (these are
//!   deleted by the self-healing startup scan).
//! * [`ScanResult::orphans`] — unpaired half-sidecars from a crashed mid
//!   download (these are deleted by the self-healing startup scan).
//!
//! More files are named after the audio sidecar ([`derived_files`]): the
//! karaoke stems and the dub track with its transcripts. A consumer finds them
//! only under the name the song's CURRENT audio derives, so a song's files are
//! renamed as one set ([`rename_song_files`], #136) and the startup self-heal
//! re-links any left under an old name ([`derived_file_owners`]).

use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// A complete, processed song present in the cache.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedSong {
    pub video_id: String,
    pub song: String,
    pub artist: String,
    pub gemini_failed: bool,
    pub video_path: PathBuf,
    pub audio_path: PathBuf,
}

/// A single-file legacy `.mp4` from before the FLAC migration.
#[derive(Debug, Clone, PartialEq)]
pub struct LegacyFile {
    pub video_id: String,
    pub gemini_failed: bool,
    pub path: PathBuf,
}

/// An unpaired sidecar (video without audio, or audio without video).
#[derive(Debug, Clone, PartialEq)]
pub struct Orphan {
    pub video_id: String,
    pub path: PathBuf,
}

/// Result of walking the cache directory once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanResult {
    pub songs: Vec<CachedSong>,
    pub legacy: Vec<LegacyFile>,
    pub orphans: Vec<Orphan>,
    /// Lyrics sidecar files: `(youtube_id, path)`.
    pub lyrics_files: Vec<(String, PathBuf)>,
    /// `(youtube_id, path)` for every preprocess-vocals output found in
    /// the cache directory. Persisted across alignment runs (see #41) so
    /// reprocess reuses Demucs output via aligner.rs cache-hit logic.
    pub vocals_files: Vec<(String, PathBuf)>,
    /// Complete pairs for a video id that already has a NEWER complete pair
    /// (a re-download under different metadata left the old pair behind).
    /// `songs` keeps the newest pair per id; these are removed by
    /// [`remove_duplicates`] at startup.
    pub duplicates: Vec<CachedSong>,
}

static VIDEO_ID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_-]{11}$").unwrap());

static SPLIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+)_(.+)_([a-zA-Z0-9_-]{11})_normalized(_gf)?_(video|audio)\.(mp4|flac)$")
        .unwrap()
});

static LEGACY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+)_(.+)_([a-zA-Z0-9_-]{11})_normalized(_gf)?\.mp4$").unwrap());

static LYRICS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-zA-Z0-9_-]{11})_lyrics\.json$").unwrap());

static VOCALS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-zA-Z0-9_-]{11})_vocals16k\.wav$").unwrap());

/// A file named after an audio sidecar ([`derived_files`]): captures the base
/// name (`{song}_{artist}_{id}_normalized[_gf]`) and the YouTube id.
static DERIVED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(.+_([a-zA-Z0-9_-]{11})_normalized(?:_gf)?)_(?:audio_vocals\.flac|audio_instrumental\.flac|dub\.flac|dub_transcripts\.json)$",
    )
    .unwrap()
});

/// Build the output filename for the video sidecar.
pub fn video_filename(song: &str, artist: &str, video_id: &str, gemini_failed: bool) -> String {
    let safe_song = sanitize_filename(song);
    let safe_artist = sanitize_filename(artist);
    let gf = if gemini_failed { "_gf" } else { "" };
    format!("{safe_song}_{safe_artist}_{video_id}_normalized{gf}_video.mp4")
}

/// Build the output filename for the audio sidecar.
pub fn audio_filename(song: &str, artist: &str, video_id: &str, gemini_failed: bool) -> String {
    let safe_song = sanitize_filename(song);
    let safe_artist = sanitize_filename(artist);
    let gf = if gemini_failed { "_gf" } else { "" };
    format!("{safe_song}_{safe_artist}_{video_id}_normalized{gf}_audio.flac")
}

/// Every file named after a song's audio sidecar, in the order a rename moves
/// them: the karaoke stems (#148, [`crate::stems::stem_paths`]), then the dub
/// track and its transcripts (#183, [`crate::stems::dub_path`] /
/// [`crate::stems::dub_transcripts_path`]). The stem mixer, the dub mixer, the
/// lyrics isolation and `StemsState` derive these names from the song's CURRENT
/// audio path; the recorded `*_file_path` columns (which the dub worker's input
/// choice reads) are kept in sync with every move.
pub fn derived_files(audio: &Path) -> [PathBuf; 4] {
    let (vocals, instrumental) = crate::stems::stem_paths(audio);
    [
        vocals,
        instrumental,
        crate::stems::dub_path(audio),
        crate::stems::dub_transcripts_path(audio),
    ]
}

/// One owner of the song files at a time (#136 review round 2). A rename (the
/// reprocess worker, [`rename_song_files`]) and a re-link (`song_relink`, at
/// startup and after every stem / dub job) each read a song's recorded files,
/// move them and record the result. Run at the same moment on the same song,
/// one could move files the other just recorded elsewhere. Both hold this lock
/// from the read to the record.
pub static SONG_FILES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A song's complete file set, named by the two sidecars the DB records: every
/// other file of the song is named after `audio` ([`derived_files`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SongFiles {
    pub video: Option<PathBuf>,
    pub audio: Option<PathBuf>,
}

/// A [`SongFiles`] as the `videos` row records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SongColumns {
    /// `file_path` (`""` when the song has no video, as the row stores it).
    pub video: String,
    pub audio: Option<String>,
    /// The stem / dub names the audio derives (`None` without an audio).
    pub vocals: Option<String>,
    pub instrumental: Option<String>,
    pub dub: Option<String>,
}

impl SongFiles {
    /// The set a row records: `file_path` (`""` = no video) and `audio_file_path`.
    pub fn recorded(file_path: &str, audio_file_path: Option<&str>) -> Self {
        Self {
            video: (!file_path.is_empty()).then(|| PathBuf::from(file_path)),
            audio: audio_file_path.map(PathBuf::from),
        }
    }

    /// The same set named after `song` / `artist` in `cache_dir` (the names the
    /// download worker gives a new song, [`video_filename`] / [`audio_filename`]).
    pub fn named(
        &self,
        cache_dir: &Path,
        song: &str,
        artist: &str,
        video_id: &str,
        gemini_failed: bool,
    ) -> Self {
        Self {
            video: self
                .video
                .as_ref()
                .map(|_| cache_dir.join(video_filename(song, artist, video_id, gemini_failed))),
            audio: self
                .audio
                .as_ref()
                .map(|_| cache_dir.join(audio_filename(song, artist, video_id, gemini_failed))),
        }
    }

    /// The row's path columns for this set.
    pub fn columns(&self) -> SongColumns {
        let derived = self.audio.as_deref().map(derived_files);
        let column = |i: usize| derived.as_ref().map(|d| path_column(&d[i]));
        SongColumns {
            video: self.video.as_deref().map(path_column).unwrap_or_default(),
            audio: self.audio.as_deref().map(path_column),
            vocals: column(0),
            instrumental: column(1),
            dub: column(2),
        }
    }
}

/// A path as the `videos` row stores it.
fn path_column(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Rename a song's COMPLETE file set from `old` to `new` as one unit (#136):
/// the files named after the audio ([`derived_files`]) first, then the audio,
/// then the video. Returns the set now in effect: `new` when every move
/// succeeded; after a failed move, where each of the two recorded sidecars
/// really is ([`in_effect_after_failure`]). A failed move first moves back
/// every file already moved, so a song is never split across two names. Only a
/// move back that ALSO fails leaves it split (logged at ERROR). A stuck AUDIO is
/// returned at its new name (the video moves last, so it is never stuck), and
/// the startup self-heal keeps a half a row records. A stuck stems pair or dub
/// is re-linked to the recorded audio's name at the next start; a lone stuck
/// stem resets the row to pending (a pair is never mixed from two names). A file
/// that does not exist, or that already has its new name, is skipped.
pub fn rename_song_files(youtube_id: &str, old: &SongFiles, new: &SongFiles) -> SongFiles {
    let mut moves = Vec::new();
    if let (Some(from), Some(to)) = (&old.audio, &new.audio) {
        moves.extend(derived_files(from).into_iter().zip(derived_files(to)));
        moves.push((from.clone(), to.clone()));
    }
    if let (Some(from), Some(to)) = (&old.video, &new.video) {
        moves.push((from.clone(), to.clone()));
    }
    match move_as_unit(youtube_id, &moves) {
        Ok(_) => new.clone(),
        Err(failed) => in_effect_after_failure(old, new, &failed.stuck),
    }
}

/// Where a song's two recorded sidecars are after a unit move from `old` to
/// `new` failed: each back at its old name, except one whose move could not
/// be undone (`stuck`), which is still at its new name. The row must record
/// where each file IS, or the startup self-heal would take the stuck half for
/// unrecorded crash debris and delete it (#136 review round 2).
fn in_effect_after_failure(
    old: &SongFiles,
    new: &SongFiles,
    stuck: &[(PathBuf, PathBuf)],
) -> SongFiles {
    let at = |old: &Option<PathBuf>, new: &Option<PathBuf>| match (old, new) {
        (Some(from), Some(to)) if stuck.iter().any(|(f, t)| f == from && t == to) => {
            Some(to.clone())
        }
        _ => old.clone(),
    };
    SongFiles {
        video: at(&old.video, &new.video),
        audio: at(&old.audio, &new.audio),
    }
}

/// A unit move that failed ([`move_as_unit`]): the error, and every move that
/// could not be undone, whose file is still at its `to` name.
#[derive(Debug)]
pub struct MoveFailed {
    pub error: std::io::Error,
    pub stuck: Vec<(PathBuf, PathBuf)>,
}

/// Move every `(from, to)` whose `from` exists and differs from `to`, in
/// order, as one unit. Returns how many files moved. On the first failure,
/// moves the files already moved back in reverse order and returns that
/// error, with every move it could not undo. Logs every move at INFO, a
/// failure at WARN, and a move back that fails at ERROR.
///
/// A FILE already under a `to` name is replaced: the song's own file wins over
/// a stale copy (a job's fresh output over an older one, a whole stems pair over
/// half a pair). It is first set aside ([`set_aside_name`]) and only deleted once
/// the whole unit has moved, so a rollback gives it back (#136 review round 5).
/// A directory under a `to` name is never set aside; the move fails on it.
pub fn move_as_unit(youtube_id: &str, moves: &[(PathBuf, PathBuf)]) -> Result<usize, MoveFailed> {
    move_as_unit_with(youtube_id, moves, &RealFs)
}

/// The filesystem operations a unit move makes: `std::fs` in production
/// ([`RealFs`]); a test fails a chosen one (a set-aside, a move, a give-back,
/// a move back, the identity check, the delete of a replaced file).
trait FileOps {
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()>;
    fn is_other_file(&self, from: &Path, to: &Path) -> std::io::Result<bool>;
    fn remove(&self, path: &Path) -> std::io::Result<()>;
}

/// The real filesystem.
struct RealFs;

impl FileOps for RealFs {
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::rename(from, to)
    }

    fn is_other_file(&self, from: &Path, to: &Path) -> std::io::Result<bool> {
        is_other_file(from, to)
    }

    fn remove(&self, path: &Path) -> std::io::Result<()> {
        std::fs::remove_file(path)
    }
}

/// [`move_as_unit`] on the given [`FileOps`].
fn move_as_unit_with<O: FileOps>(
    youtube_id: &str,
    moves: &[(PathBuf, PathBuf)],
    ops: &O,
) -> Result<usize, MoveFailed> {
    let mut done: Vec<Moved> = Vec::new();
    for (from, to) in moves {
        if from == to || !from.exists() {
            continue;
        }
        let other_file = match ops.is_other_file(from, to) {
            Ok(other) => other,
            Err(error) => {
                tracing::warn!(
                    youtube_id,
                    from = %from.display(),
                    to = %to.display(),
                    "cache: cannot tell whether a song file's new name is another file: {error}"
                );
                return Err(undo(youtube_id, done, error, ops));
            }
        };
        let set_aside = if other_file {
            let aside = set_aside_name(to);
            if let Err(error) = ops.rename(to, &aside) {
                tracing::warn!(
                    youtube_id,
                    to = %to.display(),
                    "cache: could not set aside the file under a song file's new name: {error}"
                );
                return Err(undo(youtube_id, done, error, ops));
            }
            tracing::info!(
                youtube_id,
                to = %to.display(),
                "cache: setting aside the older file under a song file's new name"
            );
            Some(aside)
        } else {
            None
        };
        let step = Moved {
            from: from.clone(),
            to: to.clone(),
            set_aside,
        };
        if let Err(error) = ops.rename(from, to) {
            tracing::warn!(
                youtube_id,
                from = %from.display(),
                to = %to.display(),
                "cache: moving a song file failed, moving the {} already moved back: {error}",
                done.len()
            );
            give_back(youtube_id, &step, ops);
            return Err(undo(youtube_id, done, error, ops));
        }
        tracing::info!(
            youtube_id,
            from = %from.display(),
            to = %to.display(),
            "cache: moved a song file"
        );
        done.push(step);
    }
    // The unit moved: the set-aside files are the stale copies it replaced.
    for aside in done.iter().filter_map(|step| step.set_aside.as_ref()) {
        if let Err(e) = ops.remove(aside) {
            tracing::warn!(
                youtube_id,
                aside = %aside.display(),
                "cache: could not delete a replaced song file: {e}"
            );
        }
    }
    Ok(done.len())
}

/// Whether `to` names an existing FILE other than `from`, one a move must set
/// aside before it can take the name. On a case-insensitive filesystem (NTFS,
/// the box) a `to` that differs from `from` only in letter case names the SAME
/// file: both canonicalize to it, so it is not set aside and the rename just
/// changes the case (#136 review round 6). When either cannot be
/// canonicalized, the answer is unknown: an error, and the unit rolls back
/// rather than guess (a wrong guess set the song's own file aside).
fn is_other_file(from: &Path, to: &Path) -> std::io::Result<bool> {
    if !to.is_file() {
        return Ok(false);
    }
    Ok(std::fs::canonicalize(from)? != std::fs::canonicalize(to)?)
}

/// Every set-aside `.replaced` file left in `cache_dir` ([`set_aside_name`]).
/// One is left by a unit move that crashed, could not give a replaced file
/// back, had a file stuck under its new name, or could not delete the replaced
/// copy after the unit moved. They are only reported (the startup self-heal
/// WARNs each), never deleted: one may be the only copy of a file.
pub fn set_aside_leftovers(cache_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return Vec::new();
    };
    let mut leftovers: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "replaced") && path.is_file())
        .collect();
    leftovers.sort();
    leftovers
}

/// One move [`move_as_unit`] made, and where it set aside the file it replaced.
struct Moved {
    from: PathBuf,
    to: PathBuf,
    set_aside: Option<PathBuf>,
}

/// The name a replaced file waits under until its unit has moved: the first
/// free one of `<name>.replaced`, `<name>.2.replaced`, `<name>.3.replaced`, …
/// (no cache scan matches them). A leftover from an earlier move is never
/// overwritten: it may be the only copy of a file.
fn set_aside_name(path: &Path) -> PathBuf {
    let mut n: u32 = 1;
    loop {
        let candidate = numbered_aside_name(path, n);
        // Free only when NOTHING is there: a name whose stat fails for another
        // reason may still hold a leftover, so it is skipped too.
        if std::fs::symlink_metadata(&candidate)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return candidate;
        }
        n += 1;
    }
}

/// The `n`-th set-aside name of `path`: `<name>.replaced` for 1, else
/// `<name>.<n>.replaced`.
fn numbered_aside_name(path: &Path, n: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    if n > 1 {
        name.push(format!(".{n}"));
    }
    name.push(".replaced");
    PathBuf::from(name)
}

/// Give a step's set-aside file its name back (its own move never happened or
/// was just undone).
fn give_back<O: FileOps>(youtube_id: &str, step: &Moved, ops: &O) {
    if let Some(aside) = &step.set_aside
        && let Err(e) = ops.rename(aside, &step.to)
    {
        tracing::error!(
            youtube_id,
            aside = %aside.display(),
            to = %step.to.display(),
            "cache: giving back a replaced song file failed, it stays set aside: {e}"
        );
    }
}

/// Undo `done` in reverse order after `error`: every file goes back to its old
/// name and every replaced file gets its name back, unless that give-back fails
/// (ERROR: the replaced file stays set aside, its name empty). A file that cannot
/// move back is `stuck`, and the file it replaced then stays set aside (both
/// logged). A set-aside file left behind is WARNed at every start.
fn undo<O: FileOps>(
    youtube_id: &str,
    done: Vec<Moved>,
    error: std::io::Error,
    ops: &O,
) -> MoveFailed {
    let mut stuck = Vec::new();
    for step in done.into_iter().rev() {
        match ops.rename(&step.to, &step.from) {
            Ok(()) => give_back(youtube_id, &step, ops),
            Err(back) => {
                tracing::error!(
                    youtube_id,
                    from = %step.to.display(),
                    to = %step.from.display(),
                    set_aside = %step
                        .set_aside
                        .as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    "cache: moving a song file back failed, the file stays under its new \
                     name (and the file it replaced stays set aside): {back}"
                );
                stuck.push((step.from, step.to));
            }
        }
    }
    MoveFailed { error, stuck }
}

/// Every name in `cache_dir` that still holds a file named after an audio
/// sidecar ([`derived_files`]), given as the audio path that name implies (the
/// audio itself may be gone) and grouped by YouTube id, in path order. A song
/// whose audio was renamed without its stems (#136) shows up here under its
/// OLD name. Which name a re-link takes is decided per unit by that unit's own
/// files (`song_relink`), never by this order.
pub fn derived_file_owners(cache_dir: &Path) -> HashMap<String, Vec<PathBuf>> {
    let entries = match std::fs::read_dir(cache_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("cannot read cache dir {}: {e}", cache_dir.display());
            return HashMap::new();
        }
    };
    let mut owners: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(caps) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| DERIVED_RE.captures(n))
        else {
            continue;
        };
        let audio = cache_dir.join(format!("{}_audio.flac", &caps[1]));
        owners.entry(caps[2].to_string()).or_default().push(audio);
    }
    for names in owners.values_mut() {
        names.sort();
        names.dedup();
    }
    owners
}

/// Walk the cache directory and categorise every matching file.
pub fn scan_cache(cache_dir: &Path) -> ScanResult {
    let entries = match std::fs::read_dir(cache_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("cannot read cache dir {}: {e}", cache_dir.display());
            return ScanResult::default();
        }
    };

    // Temporary buckets per BASE name (`{song}_{artist}_{id}_normalized[_gf]`)
    // for pairing: a video pairs only with the audio of the same base, so two
    // downloads of one id under different metadata never cross-pair.
    let mut halves: HashMap<String, Halves> = HashMap::new();
    let mut legacy: Vec<LegacyFile> = Vec::new();
    let mut lyrics_files: Vec<(String, PathBuf)> = Vec::new();
    let mut vocals_files: Vec<(String, PathBuf)> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if let Some(caps) = SPLIT_RE.captures(filename) {
            let song = caps[1].to_string();
            let artist = caps[2].to_string();
            let vid = caps[3].to_string();
            let gf = caps.get(4).is_some();
            let kind = &caps[5];
            let base = filename
                .strip_suffix(if kind == "video" {
                    "_video.mp4"
                } else {
                    "_audio.flac"
                })
                .unwrap_or(filename)
                .to_string();
            let h = halves.entry(base).or_insert_with(|| Halves {
                video_id: vid,
                song,
                artist,
                gemini_failed: gf,
                video: None,
                audio: None,
            });
            if kind == "video" {
                h.video = Some(path.clone());
            } else {
                h.audio = Some(path.clone());
            }
            continue;
        }

        if let Some(caps) = LEGACY_RE.captures(filename) {
            legacy.push(LegacyFile {
                video_id: caps[3].to_string(),
                gemini_failed: caps.get(4).is_some(),
                path,
            });
            continue;
        }

        if let Some(caps) = LYRICS_RE.captures(filename) {
            lyrics_files.push((caps[1].to_string(), path));
            continue;
        }

        if let Some(caps) = VOCALS_RE.captures(filename) {
            vocals_files.push((caps[1].to_string(), path));
            continue;
        }
    }

    // Pair video + audio halves of the same base; a lone half is an orphan.
    let mut complete: HashMap<String, Vec<CachedSong>> = HashMap::new();
    let mut orphans: Vec<Orphan> = Vec::new();
    for h in halves.into_values() {
        match (h.video, h.audio) {
            (Some(video_path), Some(audio_path)) => {
                complete
                    .entry(h.video_id.clone())
                    .or_default()
                    .push(CachedSong {
                        video_id: h.video_id,
                        song: h.song,
                        artist: h.artist,
                        gemini_failed: h.gemini_failed,
                        video_path,
                        audio_path,
                    });
            }
            (Some(path), None) | (None, Some(path)) => orphans.push(Orphan {
                video_id: h.video_id,
                path,
            }),
            (None, None) => {}
        }
    }

    // One keeper per id: the newest pair (by the video file's mtime); every
    // older complete pair of that id is a superseded duplicate.
    let mut songs: Vec<CachedSong> = Vec::new();
    let mut duplicates: Vec<CachedSong> = Vec::new();
    for (_, mut pairs) in complete {
        pairs.sort_by_key(|p| std::cmp::Reverse(modified(&p.video_path)));
        let mut pairs = pairs.into_iter();
        if let Some(keeper) = pairs.next() {
            songs.push(keeper);
        }
        duplicates.extend(pairs);
    }

    ScanResult {
        songs,
        legacy,
        orphans,
        lyrics_files,
        vocals_files,
        duplicates,
    }
}

/// The two halves of one base name seen by [`scan_cache`].
struct Halves {
    video_id: String,
    song: String,
    artist: String,
    gemini_failed: bool,
    video: Option<PathBuf>,
    audio: Option<PathBuf>,
}

/// A file's modification time (the epoch when unreadable, so it loses).
fn modified(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// Delete every superseded duplicate pair: its video, its audio and its stems
/// (the stem worker separates the kept download again). Its dub track and
/// transcripts stay: a dub is an operator-requested synthesis nothing re-runs
/// on its own, and the self-heal re-link adopts it under the kept song's name
/// (same YouTube id, same audio; #136).
pub fn remove_duplicates(duplicates: &[CachedSong]) {
    for dup in duplicates {
        let (vocals, instrumental) = crate::stems::stem_paths(&dup.audio_path);
        tracing::info!(
            video_id = %dup.video_id,
            video = %dup.video_path.display(),
            "removing superseded duplicate cache pair (a newer pair of this id is kept)"
        );
        for path in [&dup.video_path, &dup.audio_path, &vocals, &instrumental] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("failed to remove duplicate {}: {e}", path.display()),
            }
        }
    }
}

/// Delete every legacy single-file `.mp4` listed in `legacy`.
pub fn cleanup_legacy(legacy: &[LegacyFile]) {
    for item in legacy {
        tracing::info!(
            "deleting legacy AAC file for {}: {}",
            item.video_id,
            item.path.display()
        );
        if let Err(e) = std::fs::remove_file(&item.path) {
            tracing::warn!("failed to remove legacy file {}: {e}", item.path.display());
        }
    }
}

/// Sanitize a string for use inside a filename.
pub fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-')
        .collect();
    let collapsed: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated = if collapsed.len() > 50 {
        let mut end = 50;
        while end > 0 && !collapsed.is_char_boundary(end) {
            end -= 1;
        }
        &collapsed[..end]
    } else {
        &collapsed
    };
    truncated.trim().to_string()
}

/// Check if a string looks like a valid YouTube video ID.
pub fn is_valid_video_id(s: &str) -> bool {
    VIDEO_ID_RE.is_match(s)
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "cache_tests_files.rs"]
mod tests_files;
