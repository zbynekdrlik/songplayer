//! #136 reopen: a song's COMPLETE file set, named after its audio sidecar.
//! Sibling of `cache.rs`, wired via `#[path = "cache_tests_files.rs"]`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::*;

const ID: &str = "IYAOosrh7HY";

/// Every file of one song under one base name, in move order (derived files,
/// audio, video).
fn set_under(dir: &Path, base: &str) -> [PathBuf; 6] {
    [
        dir.join(format!("{base}_audio_vocals.flac")),
        dir.join(format!("{base}_audio_instrumental.flac")),
        dir.join(format!("{base}_dub.flac")),
        dir.join(format!("{base}_dub_transcripts.json")),
        dir.join(format!("{base}_audio.flac")),
        dir.join(format!("{base}_video.mp4")),
    ]
}

fn files_of(set: &[PathBuf; 6]) -> SongFiles {
    SongFiles {
        video: Some(set[5].clone()),
        audio: Some(set[4].clone()),
    }
}

fn old_base() -> String {
    format!("Old Song_Old Artist_{ID}_normalized_gf")
}

fn new_base() -> String {
    format!("Gods Not Dead_Enjoy Worship_{ID}_normalized")
}

fn write_all(set: &[PathBuf]) {
    for p in set {
        fs::write(p, p.to_string_lossy().as_bytes()).unwrap();
    }
}

fn set_mtime(path: &Path, secs_ago: u64) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(secs_ago))
        .unwrap();
}

#[test]
fn derived_files_are_the_stems_then_the_dub_named_after_the_audio() {
    let d = Path::new("cache");
    let audio = d.join(format!("{}_audio.flac", new_base()));
    let set = set_under(d, &new_base());
    assert_eq!(
        derived_files(&audio),
        [
            set[0].clone(),
            set[1].clone(),
            set[2].clone(),
            set[3].clone()
        ]
    );
}

#[test]
fn recorded_reads_an_empty_file_path_as_no_video() {
    assert_eq!(
        SongFiles::recorded("", Some("a_audio.flac")),
        SongFiles {
            video: None,
            audio: Some(PathBuf::from("a_audio.flac")),
        }
    );
    assert_eq!(
        SongFiles::recorded("v_video.mp4", None),
        SongFiles {
            video: Some(PathBuf::from("v_video.mp4")),
            audio: None,
        }
    );
}

#[test]
fn named_gives_each_present_member_its_new_name_and_keeps_absent_ones_absent() {
    let d = Path::new("cache");
    let old = SongFiles::recorded("old_video.mp4", Some("old_audio.flac"));
    let new = old.named(d, "Gods Not Dead", "Enjoy Worship", ID, false);
    let set = set_under(d, &new_base());
    assert_eq!(new, files_of(&set));
    let gf = old.named(d, "Gods Not Dead", "Enjoy Worship", ID, true);
    assert_eq!(
        gf.audio,
        Some(d.join(format!(
            "Gods Not Dead_Enjoy Worship_{ID}_normalized_gf_audio.flac"
        )))
    );

    let no_video = SongFiles::recorded("", Some("old_audio.flac"));
    let new = no_video.named(d, "Gods Not Dead", "Enjoy Worship", ID, false);
    assert_eq!(new.video, None);
    assert_eq!(new.audio, Some(set[4].clone()));
    let no_audio = SongFiles::recorded("old_video.mp4", None);
    let new = no_audio.named(d, "Gods Not Dead", "Enjoy Worship", ID, false);
    assert_eq!(new.video, Some(set[5].clone()));
    assert_eq!(new.audio, None);
}

fn text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[test]
fn columns_record_the_set_and_the_names_its_audio_derives() {
    let d = Path::new("cache");
    let set = set_under(d, &new_base());
    assert_eq!(
        files_of(&set).columns(),
        SongColumns {
            video: text(&set[5]),
            audio: Some(text(&set[4])),
            vocals: Some(text(&set[0])),
            instrumental: Some(text(&set[1])),
            dub: Some(text(&set[2])),
        }
    );
    assert_eq!(
        SongFiles {
            video: None,
            audio: None,
        }
        .columns(),
        SongColumns {
            video: String::new(),
            audio: None,
            vocals: None,
            instrumental: None,
            dub: None,
        },
        "no video is the empty file_path; no audio names no stems"
    );
}

#[test]
fn rename_moves_every_file_of_the_song() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = (
        set_under(dir.path(), &old_base()),
        set_under(dir.path(), &new_base()),
    );
    write_all(&old);

    let in_effect = rename_song_files(ID, &files_of(&old), &files_of(&new));

    assert_eq!(in_effect, files_of(&new));
    for (from, to) in old.iter().zip(&new) {
        assert!(!from.exists(), "{} moved away", from.display());
        assert_eq!(fs::read_to_string(to).unwrap(), from.to_string_lossy());
    }
}

/// The audio move fails AFTER the four derived files moved: all four go back,
/// and the video (after the audio) is never touched.
#[test]
fn a_failed_move_moves_every_moved_file_back() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = (
        set_under(dir.path(), &old_base()),
        set_under(dir.path(), &new_base()),
    );
    write_all(&old);
    fs::create_dir(&new[4]).unwrap();
    fs::write(new[4].join("blocker"), b"x").unwrap();

    let in_effect = rename_song_files(ID, &files_of(&old), &files_of(&new));

    assert_eq!(in_effect, files_of(&old), "the old set stays in effect");
    for p in &old {
        assert_eq!(fs::read_to_string(p).unwrap(), p.to_string_lossy());
    }
    for (i, p) in new.iter().enumerate().filter(|(i, _)| *i != 4) {
        assert!(!p.exists(), "new member {i} {} must not exist", p.display());
    }
}

#[test]
fn missing_files_are_skipped_and_the_new_set_is_in_effect() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = (
        set_under(dir.path(), &old_base()),
        set_under(dir.path(), &new_base()),
    );
    // Only the audio and the video exist: no stems, no dub yet.
    write_all(&old[4..]);

    let in_effect = rename_song_files(ID, &files_of(&old), &files_of(&new));

    assert_eq!(in_effect, files_of(&new));
    assert!(new[4].exists() && new[5].exists());
    for p in &new[..4] {
        assert!(!p.exists(), "{} was never there", p.display());
    }
}

/// #136 review round 2: a move back that fails leaves that file at its NEW
/// name. The set returned must record it there, or the next start's self-heal
/// deletes it as unrecorded crash debris (the normalized audio, a re-download).
#[test]
fn a_file_whose_move_could_not_be_undone_is_recorded_where_it_is() {
    let d = Path::new("cache");
    let (old, new) = (
        files_of(&set_under(d, &old_base())),
        files_of(&set_under(d, &new_base())),
    );
    let (old_audio, new_audio) = (old.audio.clone().unwrap(), new.audio.clone().unwrap());
    let (old_video, new_video) = (old.video.clone().unwrap(), new.video.clone().unwrap());

    assert_eq!(
        in_effect_after_failure(&old, &new, &[(old_audio.clone(), new_audio.clone())]),
        SongFiles {
            video: Some(old_video.clone()),
            audio: Some(new_audio.clone()),
        },
        "the stuck audio is recorded at its new name, the video at its old one"
    );
    assert_eq!(
        in_effect_after_failure(&old, &new, &[(old_video.clone(), new_video.clone())]),
        SongFiles {
            video: Some(new_video),
            audio: Some(old_audio.clone()),
        }
    );
    assert_eq!(
        in_effect_after_failure(&old, &new, &[]),
        old,
        "everything moved back"
    );
    // A stuck DERIVED file never changes the recorded pair (a stuck stems pair
    // or dub is re-linked at the next start; a lone stuck stem resets the row).
    let stuck_stem = (
        derived_files(&old_audio)[0].clone(),
        derived_files(&new_audio)[0].clone(),
    );
    assert_eq!(in_effect_after_failure(&old, &new, &[stuck_stem]), old);
}

#[test]
fn move_as_unit_counts_the_moves_and_skips_a_file_already_named() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (a, b, c) = (d.join("a"), d.join("b"), d.join("c"));
    fs::write(&a, b"a").unwrap();
    fs::write(&c, b"c").unwrap();
    let moved = move_as_unit(
        ID,
        &[
            (a.clone(), b.clone()),
            (d.join("missing"), d.join("x")),
            (c.clone(), c.clone()),
        ],
    )
    .unwrap();
    assert_eq!(moved, 1, "only a → b moves");
    assert!(!a.exists() && b.exists() && c.exists());
    assert!(!d.join("x").exists());
}

/// #136 review round 5: a move may replace a file already under the new name
/// (a job's fresh output over an older copy, a whole pair over half a pair).
/// When a LATER move of the unit fails, the rollback must give the replaced
/// file back, never leave the name empty with the replaced content gone.
#[test]
fn a_failed_move_gives_back_a_file_it_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (fresh, older) = (d.join("fresh_dub"), d.join("current_dub"));
    fs::write(&fresh, b"new dub").unwrap();
    fs::write(&older, b"old dub").unwrap();
    let (t_from, blocked) = (d.join("fresh_t"), d.join("current_t"));
    fs::write(&t_from, b"new t").unwrap();
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("x"), b"x").unwrap();

    let failed = move_as_unit(
        ID,
        &[
            (fresh.clone(), older.clone()),
            (t_from.clone(), blocked.clone()),
        ],
    )
    .unwrap_err();

    assert!(failed.stuck.is_empty());
    assert_eq!(
        fs::read(&older).unwrap(),
        b"old dub",
        "the replaced file is back"
    );
    assert_eq!(
        fs::read(&fresh).unwrap(),
        b"new dub",
        "the moved file is back"
    );
    assert_eq!(fs::read(&t_from).unwrap(), b"new t");
    assert_eq!(
        fs::read_dir(d).unwrap().count(),
        4,
        "no set-aside copy is left behind"
    );
}

/// A successful move over an existing file leaves only the moved file: the
/// replaced copy is not kept anywhere.
#[test]
fn a_move_over_an_existing_file_leaves_only_the_moved_file() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (fresh, older) = (d.join("fresh_dub"), d.join("current_dub"));
    fs::write(&fresh, b"new dub").unwrap();
    fs::write(&older, b"old dub").unwrap();

    assert_eq!(
        move_as_unit(ID, &[(fresh.clone(), older.clone())]).unwrap(),
        1
    );

    assert_eq!(fs::read(&older).unwrap(), b"new dub");
    assert!(!fresh.exists());
    assert_eq!(
        fs::read_dir(d).unwrap().count(),
        1,
        "nothing set aside remains"
    );
}

/// Real file operations except the ones named, which fail: the seam for
/// forcing a chosen step of a unit move to fail on both Linux and Windows.
#[derive(Default)]
struct Failing {
    /// The path whose stat fails.
    exists_of: Option<PathBuf>,
    /// Every set-aside name reads as taken.
    asides_taken: bool,
    /// The one `(from, to)` rename that fails.
    rename: Option<(PathBuf, PathBuf)>,
    /// The `to` whose identity check fails.
    identity_of: Option<PathBuf>,
    /// The path whose delete fails.
    remove: Option<PathBuf>,
}

fn injected(what: &str) -> std::io::Error {
    std::io::Error::other(format!("injected {what} failure"))
}

impl FileOps for Failing {
    fn exists(&self, path: &Path) -> std::io::Result<bool> {
        if self.exists_of.as_deref() == Some(path) {
            return Err(injected("stat"));
        }
        if self.asides_taken && path.extension().is_some_and(|e| e == "replaced") {
            return Ok(true);
        }
        RealFs.exists(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        if self
            .rename
            .as_ref()
            .is_some_and(|(f, t)| f == from && t == to)
        {
            return Err(injected("rename"));
        }
        std::fs::rename(from, to)
    }

    fn is_other_file(&self, from: &Path, to: &Path) -> std::io::Result<bool> {
        if self.identity_of.as_deref() == Some(to) {
            return Err(injected("identity"));
        }
        RealFs.is_other_file(from, to)
    }

    fn remove(&self, path: &Path) -> std::io::Result<()> {
        if self.remove.as_deref() == Some(path) {
            return Err(injected("remove"));
        }
        std::fs::remove_file(path)
    }
}

/// A [`Failing`] whose one `(from, to)` rename fails.
fn failing_on(from: PathBuf, to: PathBuf) -> Failing {
    Failing {
        rename: Some((from, to)),
        ..Failing::default()
    }
}

/// #136 review round 9: a source whose stat fails is not read as "absent" (the
/// song would be recorded under a name with no file, and the next start would
/// delete the old one as an orphan): the unit rolls back.
#[test]
fn a_source_whose_existence_is_unknown_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (a, a2, b, b2) = (d.join("a"), d.join("a2"), d.join("b"), d.join("b2"));
    fs::write(&a, b"a").unwrap();
    fs::write(&b, b"b").unwrap();

    let failed = move_as_unit_with(
        ID,
        &[(a.clone(), a2.clone()), (b.clone(), b2.clone())],
        &Failing {
            exists_of: Some(b.clone()),
            ..Failing::default()
        },
    )
    .unwrap_err();

    assert!(failed.error.to_string().contains("injected stat"));
    assert_eq!(fs::read(&a).unwrap(), b"a", "the earlier move is undone");
    assert_eq!(fs::read(&b).unwrap(), b"b");
    assert_eq!(fs::read_dir(d).unwrap().count(), 2);
}

/// A set-aside name whose stat fails, and a set-aside with every name taken,
/// both roll the unit back: never an unbounded search, never an overwrite.
#[test]
fn a_set_aside_without_a_known_free_name_rolls_back() {
    let stat_fails = |older: &Path| Failing {
        exists_of: Some(numbered_aside_name(older, 1)),
        ..Failing::default()
    };
    let all_taken = |_: &Path| Failing {
        asides_taken: true,
        ..Failing::default()
    };
    let cases: [&dyn Fn(&Path) -> Failing; 2] = [&stat_fails, &all_taken];
    for ops_for in cases {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let (a, a2, fresh, older) = (d.join("a"), d.join("a2"), d.join("fresh"), d.join("older"));
        fs::write(&a, b"a").unwrap();
        fs::write(&fresh, b"new").unwrap();
        fs::write(&older, b"old").unwrap();

        let failed = move_as_unit_with(
            ID,
            &[(a.clone(), a2.clone()), (fresh.clone(), older.clone())],
            &ops_for(&older),
        )
        .unwrap_err();

        assert!(failed.stuck.is_empty());
        assert_eq!(fs::read(&a).unwrap(), b"a");
        assert_eq!(fs::read(&fresh).unwrap(), b"new");
        assert_eq!(fs::read(&older).unwrap(), b"old");
        assert_eq!(fs::read_dir(d).unwrap().count(), 3);
    }
}

/// #136 review round 8: when the identity check of a step fails, the unit rolls
/// back rather than guess whether the new name is another file.
#[test]
fn a_move_whose_file_identity_is_unknown_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (a, a2, fresh, older) = (d.join("a"), d.join("a2"), d.join("fresh"), d.join("older"));
    fs::write(&a, b"a").unwrap();
    fs::write(&fresh, b"new").unwrap();
    fs::write(&older, b"old").unwrap();

    let failed = move_as_unit_with(
        ID,
        &[(a.clone(), a2.clone()), (fresh.clone(), older.clone())],
        &Failing {
            identity_of: Some(older.clone()),
            ..Failing::default()
        },
    )
    .unwrap_err();

    assert!(failed.stuck.is_empty());
    assert!(failed.error.to_string().contains("injected identity"));
    assert_eq!(fs::read(&a).unwrap(), b"a", "the earlier move is undone");
    assert_eq!(fs::read(&fresh).unwrap(), b"new");
    assert_eq!(fs::read(&older).unwrap(), b"old");
    assert_eq!(fs::read_dir(d).unwrap().count(), 3);
}

/// A replaced file whose delete fails after its unit moved stays set aside,
/// where the startup self-heal reports it; the move itself succeeded.
#[test]
fn a_replaced_file_that_cannot_be_deleted_stays_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (fresh, older) = (d.join("fresh"), d.join("older"));
    fs::write(&fresh, b"new").unwrap();
    fs::write(&older, b"old").unwrap();
    let aside = numbered_aside_name(&older, 1);

    let moved = move_as_unit_with(
        ID,
        &[(fresh.clone(), older.clone())],
        &Failing {
            remove: Some(aside.clone()),
            ..Failing::default()
        },
    )
    .unwrap();

    assert_eq!(moved, 1);
    assert_eq!(fs::read(&older).unwrap(), b"new");
    assert_eq!(fs::read(&aside).unwrap(), b"old");
    assert_eq!(set_aside_leftovers(d), vec![aside]);
}

/// #136 review round 6: a move whose OWN rename fails right after it set aside
/// the file under its new name gives that file back (the round-5 loss left the
/// name empty and the older file stranded as `.replaced`).
#[test]
fn a_move_failing_after_its_set_aside_gives_the_file_back() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (a, a2, fresh, older) = (d.join("a"), d.join("a2"), d.join("fresh"), d.join("older"));
    fs::write(&a, b"a").unwrap();
    fs::write(&fresh, b"new").unwrap();
    fs::write(&older, b"old").unwrap();

    let failed = move_as_unit_with(
        ID,
        &[(a.clone(), a2.clone()), (fresh.clone(), older.clone())],
        &failing_on(fresh.clone(), older.clone()),
    )
    .unwrap_err();

    assert!(failed.stuck.is_empty());
    assert_eq!(fs::read(&a).unwrap(), b"a", "the earlier move is undone");
    assert_eq!(fs::read(&fresh).unwrap(), b"new");
    assert_eq!(
        fs::read(&older).unwrap(),
        b"old",
        "the set-aside file is back"
    );
    assert_eq!(
        fs::read_dir(d).unwrap().count(),
        3,
        "nothing left set aside"
    );
}

/// A set-aside that fails rolls the unit back and touches neither file.
#[test]
fn a_set_aside_that_fails_rolls_the_unit_back() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (a, a2, fresh, older) = (d.join("a"), d.join("a2"), d.join("fresh"), d.join("older"));
    fs::write(&a, b"a").unwrap();
    fs::write(&fresh, b"new").unwrap();
    fs::write(&older, b"old").unwrap();

    let failed = move_as_unit_with(
        ID,
        &[(a.clone(), a2.clone()), (fresh.clone(), older.clone())],
        &failing_on(older.clone(), numbered_aside_name(&older, 1)),
    )
    .unwrap_err();

    assert!(failed.stuck.is_empty());
    assert_eq!(fs::read(&a).unwrap(), b"a");
    assert_eq!(fs::read(&fresh).unwrap(), b"new");
    assert_eq!(fs::read(&older).unwrap(), b"old");
    assert_eq!(fs::read_dir(d).unwrap().count(), 3);
}

/// A move back that fails leaves that file stuck under its new name (reported
/// in `stuck`), and the file it replaced stays set aside: both are kept, never
/// deleted.
#[test]
fn a_move_back_that_fails_is_stuck_and_keeps_the_replaced_file_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (fresh, older, t, blocked) = (
        d.join("fresh"),
        d.join("older"),
        d.join("t"),
        d.join("blocked"),
    );
    fs::write(&fresh, b"new").unwrap();
    fs::write(&older, b"old").unwrap();
    fs::write(&t, b"t").unwrap();
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("x"), b"x").unwrap();

    let failed = move_as_unit_with(
        ID,
        &[(fresh.clone(), older.clone()), (t.clone(), blocked.clone())],
        &failing_on(older.clone(), fresh.clone()),
    )
    .unwrap_err();

    assert_eq!(failed.stuck, vec![(fresh.clone(), older.clone())]);
    assert_eq!(
        fs::read(&older).unwrap(),
        b"new",
        "stuck under its new name"
    );
    assert_eq!(fs::read(numbered_aside_name(&older, 1)).unwrap(), b"old");
    assert!(!fresh.exists());
}

/// #136 review round 7: a give-back that fails after the move back leaves the
/// song's file under its old name, the target name empty, and the replaced
/// file set aside, where the startup self-heal reports it.
#[test]
fn a_failed_give_back_leaves_the_replaced_file_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (fresh, older, t, blocked) = (
        d.join("fresh"),
        d.join("older"),
        d.join("t"),
        d.join("blocked"),
    );
    fs::write(&fresh, b"new").unwrap();
    fs::write(&older, b"old").unwrap();
    fs::write(&t, b"t").unwrap();
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("x"), b"x").unwrap();
    let aside = numbered_aside_name(&older, 1);

    let failed = move_as_unit_with(
        ID,
        &[(fresh.clone(), older.clone()), (t.clone(), blocked.clone())],
        &failing_on(aside.clone(), older.clone()),
    )
    .unwrap_err();

    assert!(failed.stuck.is_empty(), "the song's file moved back");
    assert_eq!(fs::read(&fresh).unwrap(), b"new");
    assert!(!older.exists(), "the target name is left empty");
    assert_eq!(fs::read(&aside).unwrap(), b"old");
    assert_eq!(set_aside_leftovers(d), vec![aside]);
}

/// A set-aside never overwrites a `.replaced` left by an earlier move (it may
/// be the only copy of a file): it takes the next free name, and after the
/// unit has moved only its OWN set-aside file is deleted.
#[test]
fn a_set_aside_never_overwrites_a_leftover() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (fresh, older) = (d.join("fresh"), d.join("older"));
    fs::write(&fresh, b"new").unwrap();
    fs::write(&older, b"old").unwrap();
    let leftover = numbered_aside_name(&older, 1);
    fs::write(&leftover, b"leftover").unwrap();

    assert_eq!(
        set_aside_name(&older, &RealFs).unwrap(),
        numbered_aside_name(&older, 2),
        "the next free name"
    );
    assert_eq!(
        move_as_unit(ID, &[(fresh.clone(), older.clone())]).unwrap(),
        1
    );

    assert_eq!(fs::read(&older).unwrap(), b"new");
    assert_eq!(
        fs::read(&leftover).unwrap(),
        b"leftover",
        "the leftover is kept"
    );
    assert!(
        !numbered_aside_name(&older, 2).exists(),
        "its own set-aside is deleted"
    );
    assert_eq!(fs::read_dir(d).unwrap().count(), 2);
}

/// #136 review round 6: a rename that changes only the letter case. On a
/// case-insensitive filesystem (NTFS, the box) the new name already "exists"
/// as the very same file; the move must not set the song's own file aside
/// (it then failed, and the next start deleted the audio as an orphan).
#[test]
fn a_rename_that_only_changes_letter_case_moves_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let from = d.join(format!("Song_A_{ID}_normalized_audio.flac"));
    let to = d.join(format!("song_a_{ID}_normalized_audio.flac"));
    fs::write(&from, b"a").unwrap();

    assert_eq!(move_as_unit(ID, &[(from, to.clone())]).unwrap(), 1);

    let names: Vec<String> = fs::read_dir(d)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec![format!("song_a_{ID}_normalized_audio.flac")],
        "exactly the new spelling, nothing set aside"
    );
    assert_eq!(fs::read(&to).unwrap(), b"a");
}

/// A `.replaced` file (a unit move that crashed or could not give a file back)
/// is never taken for a song file by the cache scans.
#[test]
fn the_cache_scans_ignore_a_set_aside_file() {
    let dir = tempfile::tempdir().unwrap();
    let set = set_under(dir.path(), &new_base());
    for p in &set {
        fs::write(numbered_aside_name(p, 1), b"x").unwrap();
    }

    let scan = scan_cache(dir.path());
    assert!(scan.songs.is_empty() && scan.orphans.is_empty() && scan.legacy.is_empty());
    assert!(derived_file_owners(dir.path()).is_empty());
}

/// The startup self-heal reports every `.replaced` file left over, and only
/// those (it never deletes them).
#[test]
fn set_aside_leftovers_lists_only_the_replaced_files() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let set = set_under(d, &new_base());
    write_all(&set);
    let (a, b) = (
        numbered_aside_name(&set[0], 1),
        numbered_aside_name(&set[4], 1),
    );
    fs::write(&a, b"x").unwrap();
    fs::write(&b, b"x").unwrap();
    fs::create_dir(d.join("dir.replaced")).unwrap();

    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(set_aside_leftovers(d), expected);
    assert!(set_aside_leftovers(&d.join("missing")).is_empty());
}

#[test]
fn a_failed_move_returns_its_error() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (a, b, blocked) = (d.join("a"), d.join("b"), d.join("blocked"));
    fs::write(&a, b"a").unwrap();
    fs::write(&b, b"b").unwrap();
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("x"), b"x").unwrap();

    let failed = move_as_unit(
        ID,
        &[(a.clone(), d.join("a2")), (b.clone(), blocked.clone())],
    )
    .unwrap_err();

    assert!(failed.stuck.is_empty(), "every moved file went back");
    assert_ne!(
        failed.error.kind(),
        std::io::ErrorKind::NotFound,
        "the failure is the blocked target, not a missing source: {}",
        failed.error
    );
    assert_eq!(fs::read(&a).unwrap(), b"a", "a moved back");
    assert!(!d.join("a2").exists());
    assert_eq!(fs::read(&b).unwrap(), b"b");
}

/// The names are listed in path order, never by age: WHICH name's files a
/// re-link takes is decided per unit by that unit's own files (the stems pair,
/// the dub), so a fresh dub can never make an older stems pair win (#136
/// review round 1).
#[test]
fn derived_file_owners_groups_the_names_by_id_in_path_order() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let second = format!("B_Second_{ID}_normalized_gf");
    let first = format!("A_First_{ID}_normalized");
    let other = "S_A_dQw4w9WgXcQ_normalized";
    // "B_Second" holds only a FRESH dub; "A_First" an OLDER stems pair.
    let second_dub = d.join(format!("{second}_dub.flac"));
    fs::write(&second_dub, b"d").unwrap();
    set_mtime(&second_dub, 5);
    for suffix in ["_audio_vocals.flac", "_audio_instrumental.flac"] {
        let p = d.join(format!("{first}{suffix}"));
        fs::write(&p, b"s").unwrap();
        set_mtime(&p, 5_000);
    }
    fs::write(d.join(format!("{other}_dub_transcripts.json")), b"t").unwrap();
    // Not derived files: the sidecars themselves, the id-keyed lyrics files,
    // and a directory that looks like a stem.
    fs::write(d.join(format!("{first}_audio.flac")), b"a").unwrap();
    fs::write(d.join(format!("{first}_video.mp4")), b"v").unwrap();
    fs::write(d.join(format!("{ID}_lyrics.json")), b"{}").unwrap();
    fs::write(d.join(format!("{ID}_vocals16k.wav")), b"w").unwrap();
    fs::create_dir(d.join(format!("Dir_A_{ID}_normalized_audio_vocals.flac"))).unwrap();

    let owners = derived_file_owners(d);

    assert_eq!(owners.len(), 2, "{owners:?}");
    assert_eq!(
        owners[ID],
        vec![
            d.join(format!("{first}_audio.flac")),
            d.join(format!("{second}_audio.flac")),
        ],
        "path order: A_First before B_Second, whatever their files' age"
    );
    assert_eq!(
        owners["dQw4w9WgXcQ"],
        vec![d.join(format!("{other}_audio.flac"))]
    );
}

#[test]
fn derived_file_owners_of_an_unreadable_dir_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    assert!(derived_file_owners(&dir.path().join("missing")).is_empty());
}

/// A superseded download loses its video, audio and stems (the stem worker
/// separates the kept download again), but NOT its dub: a dub is an
/// operator-requested synthesis nothing re-runs on its own, and the self-heal
/// re-link adopts it under the kept song's name (same YouTube id, same audio).
/// The round-0 test pinned the deletion; the review showed it lost the dub for
/// good while `dub_status` stayed `ready`.
#[test]
fn remove_duplicates_keeps_the_dub_for_the_relink_to_adopt() {
    let dir = tempfile::tempdir().unwrap();
    let dup = set_under(dir.path(), &old_base());
    let keep = set_under(dir.path(), &new_base());
    write_all(&dup);
    write_all(&keep);

    remove_duplicates(&[CachedSong {
        video_id: ID.into(),
        song: "Old Song".into(),
        artist: "Old Artist".into(),
        gemini_failed: true,
        video_path: dup[5].clone(),
        audio_path: dup[4].clone(),
    }]);

    for p in [&dup[0], &dup[1], &dup[4], &dup[5]] {
        assert!(!p.exists(), "{} removed", p.display());
    }
    for p in [&dup[2], &dup[3]] {
        assert!(p.exists(), "{} kept for the re-link", p.display());
    }
    for p in &keep {
        assert!(p.exists(), "{} kept", p.display());
    }
}
