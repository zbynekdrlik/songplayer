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

#[test]
fn a_failed_move_returns_its_error() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (a, b, blocked) = (d.join("a"), d.join("b"), d.join("blocked"));
    fs::write(&a, b"a").unwrap();
    fs::write(&b, b"b").unwrap();
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("x"), b"x").unwrap();

    let err = move_as_unit(
        ID,
        &[(a.clone(), d.join("a2")), (b.clone(), blocked.clone())],
    );

    assert!(err.is_err());
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
