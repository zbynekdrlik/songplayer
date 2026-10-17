//! #261: the cache sweep's decision, entry by entry (no I/O).

use std::collections::HashMap;

use super::{
    CLEANED_VERSION, DAY_MS, DUB_PREV_AFTER_MS, Entry, MAX_PER_PASS, ORPHAN_AFTER_MS,
    PART_AFTER_MS, Place, Plan, Planned, REPLACED_AFTER_MS, RowFacts, STALE_AFTER_MS, Why,
    dub_prev_owner, file_name_of, is_stale_name, plan, replaced_owner, video_of, why,
};

const NOW: i64 = 1_800_000_000_000;

/// A song file of `id` (`Song_Artist_{id}_normalized{rest}`).
fn song(id: &str, rest: &str) -> String {
    format!("Song_Artist_{id}_normalized{rest}")
}

fn file(name: &str, age_ms: i64) -> Entry {
    Entry {
        place: Place::Cache,
        name: name.to_string(),
        is_dir: false,
        bytes: 10,
        modified_ms: Some(NOW - age_ms),
    }
}

fn dir(name: &str, age_ms: i64) -> Entry {
    Entry {
        is_dir: true,
        ..file(name, age_ms)
    }
}

fn part(name: &str, age_ms: i64) -> Entry {
    Entry {
        place: Place::Parts,
        ..file(name, age_ms)
    }
}

/// Rows of `rowvideo001` (recording its song's video and audio) and
/// `UPPER_case1`.
fn facts() -> RowFacts {
    let mut facts = RowFacts::default();
    let video = format!(
        r"C:\ProgramData\SongPlayer\cache\{}",
        song("rowvideo001", "_video.mp4")
    );
    let audio = format!("/c/{}", song("rowvideo001", "_audio.flac"));
    let paths = [
        Some(video.as_str()),
        Some(audio.as_str()),
        None,
        Some(""),
        None,
    ];
    facts.add_row("rowvideo001", &paths);
    facts.add_row("UPPER_case1", &[None, None, None, None, None]);
    facts
}

/// Why `entry` goes, alone in its listing (no siblings).
fn why_alone(entry: &Entry) -> Option<Why> {
    why(entry, &HashMap::new(), &facts(), NOW)
}

#[test]
fn the_bounds_are_the_designs() {
    assert_eq!(DAY_MS, 24 * 3_600_000);
    assert_eq!(ORPHAN_AFTER_MS, 7 * DAY_MS);
    assert_eq!(STALE_AFTER_MS, 7 * DAY_MS);
    assert_eq!(PART_AFTER_MS, 7 * DAY_MS);
    assert_eq!(REPLACED_AFTER_MS, 30 * DAY_MS);
    assert_eq!(DUB_PREV_AFTER_MS, 14 * DAY_MS);
    assert_eq!(MAX_PER_PASS, 500);
}

/// The cleanup cache version is the one the code writes: both writers name
/// `_cleaned_v{CLEANED_VERSION}.json` and no other version.
#[test]
fn the_cleaned_version_is_the_one_the_lyrics_code_writes() {
    let current = format!("_cleaned_v{CLEANED_VERSION}.json");
    for (file, src) in [
        ("lyrics/gather.rs", include_str!("lyrics/gather.rs")),
        (
            "lyrics/title_search.rs",
            include_str!("lyrics/title_search.rs"),
        ),
    ] {
        assert!(src.contains(&current), "{file} writes {current}");
        for other in (1..=9).filter(|v| *v != CLEANED_VERSION) {
            let older = format!("_cleaned_v{other}.json\"");
            assert!(!src.contains(&older), "{file} writes {older}");
        }
    }
}

/// A row's path as the box stores it (`\`) or as a test does (`/`).
#[test]
fn a_rows_file_name_is_its_last_component_either_way() {
    assert_eq!(file_name_of(r"C:\a\b\x_audio.flac"), "x_audio.flac");
    assert_eq!(file_name_of("/a/b/x_audio.flac"), "x_audio.flac");
    assert_eq!(file_name_of("x_audio.flac"), "x_audio.flac");
}

/// The video a name is named after: a song file by its exact shape (the id
/// is the 11 characters before `_normalized`, underscores and dashes in it
/// included), an id-first file of a known kind, a work dir; nothing else.
#[test]
fn a_name_is_named_after_a_video_only_by_a_known_shape() {
    let table = [
        (
            "Song_Artist_abcdefghijk_normalized_video.mp4",
            false,
            Some(("abcdefghijk", Some("Song_Artist_abcdefghijk_normalized"))),
        ),
        (
            "Song_Artist_ab_cd-fghij_normalized_gf_audio.flac",
            false,
            Some(("ab_cd-fghij", Some("Song_Artist_ab_cd-fghij_normalized_gf"))),
        ),
        (
            "Vámonos_Ártist_abcdefghijk_normalized_dub_events.jsonl",
            false,
            Some(("abcdefghijk", Some("Vámonos_Ártist_abcdefghijk_normalized"))),
        ),
        (
            "_Artist_abcdefghijk_normalized_audio_vocals.flac",
            false,
            Some(("abcdefghijk", Some("_Artist_abcdefghijk_normalized"))),
        ),
        (
            "S_A_abcdefghijk_normalized_dub.flac.prev",
            false,
            Some(("abcdefghijk", Some("S_A_abcdefghijk_normalized"))),
        ),
        (
            "S_A_abcdefghijk_normalized_audio.flac.2.replaced",
            false,
            Some(("abcdefghijk", Some("S_A_abcdefghijk_normalized"))),
        ),
        (
            "S_A_abcdefghijk_normalized.mp4",
            false,
            Some(("abcdefghijk", Some("S_A_abcdefghijk_normalized"))),
        ),
        (
            "abcdefghijk_lyrics.json",
            false,
            Some(("abcdefghijk", None)),
        ),
        (
            "ab_cdefghij_vocals16k.wav",
            false,
            Some(("ab_cdefghij", None)),
        ),
        (
            "abcdefghijk_g35t_words_used.json",
            false,
            Some(("abcdefghijk", None)),
        ),
        (
            "abcdefghijk_alignment_audit.json",
            false,
            Some(("abcdefghijk", None)),
        ),
        (
            "abcdefghijk_title_genius_x-y_cleaned_v3.json",
            false,
            Some(("abcdefghijk", None)),
        ),
        (
            "abcdefghijk_description.txt",
            false,
            Some(("abcdefghijk", None)),
        ),
        (
            "abcdefghijk_audio_temp.webm",
            false,
            Some(("abcdefghijk", None)),
        ),
        (
            "abcdefghijk_whisperx_track.json",
            false,
            Some(("abcdefghijk", None)),
        ),
        ("abcdefghijk_isolation", true, Some(("abcdefghijk", None))),
        ("abcdefghijk_stemsep", true, Some(("abcdefghijk", None))),
        ("abcdefghijk_dub", true, Some(("abcdefghijk", None))),
        // a song whose title is 11 id characters: its own id, never its title
        (
            "HolySpirit1_Artist_abcdefghijk_normalized_audio.flac",
            false,
            Some((
                "abcdefghijk",
                Some("HolySpirit1_Artist_abcdefghijk_normalized"),
            )),
        ),
        ("abcdefghijk_notes.txt", false, None),
        ("abcdefghijk_isolation", false, None),
        ("abcdefghijk_lyrics.json", true, None),
        ("A_B_C_abcdefghijk_normalized_audio.flac", false, None),
        ("tools", true, None),
        ("cli-proxy-api-config.yaml", false, None),
        ("abcdefghij_lyrics.json", false, None),
    ];
    for (name, is_dir, want) in table {
        assert_eq!(video_of(name, is_dir), want, "{name} (dir: {is_dir})");
    }
}

/// Retired outputs and cleanup caches older than the code's version are
/// stale; the current and a newer version, and any other name, are not.
#[test]
fn a_stale_name_is_a_retired_output_or_an_older_cleanup_cache() {
    let stale = [
        "abcdefghijk_whisperx_track.json",
        "abcdefghijk_descmerge_audit.json",
        "abcdefghijk_asr_path_audit.json",
        "abcdefghijk_gemini_chunks.json",
        "abcdefghijk_chunks.json",
        "abcdefghijk_asr_output.json",
        "abcdefghijk_align_output.json",
        "abcdefghijk_align_out.json",
        "abcdefghijk_vocals_dereverbed.wav",
        "abcdefghijk_genius_cleaned.json",
        "abcdefghijk_lrclib_cleaned_v2.json",
        "abcdefghijk_genius_cleaned_v1.json",
        "ab_cdefghij_title_lrclib_35094059_cleaned_v2.json",
        "abcdefghijk_title_genius_x_cleaned.json",
    ];
    for name in stale {
        assert!(is_stale_name(name), "{name} is stale");
    }
    let current = [
        "abcdefghijk_lrclib_cleaned_v3.json",
        "abcdefghijk_genius_cleaned_v3.json",
        "abcdefghijk_title_lrclib_35094059_cleaned_v3.json",
        "abcdefghijk_genius_cleaned_v4.json",
        "abcdefghijk_genius_cleaned_v99999999999.json",
        "abcdefghijk_lyrics.json",
        "abcdefghijk_g35t_words.json",
        "Song_Artist_abcdefghijk_normalized_audio.flac",
        "abcdefghij_whisperx_track.json",
        "abcdefghijkl_chunks.json",
        "x_whisperx_track.json",
    ];
    for name in current {
        assert!(!is_stale_name(name), "{name} is not stale");
    }
}

#[test]
fn a_set_aside_name_belongs_to_the_name_it_was_set_aside_from() {
    let table = [
        ("x_audio.flac.replaced", Some("x_audio.flac")),
        ("x_audio.flac.2.replaced", Some("x_audio.flac")),
        ("x_audio.flac.100.replaced", Some("x_audio.flac")),
        ("x_audio.flac..replaced", Some("x_audio.flac.")),
        ("x_audio.flac.2a.replaced", Some("x_audio.flac.2a")),
        ("5.replaced", Some("5")),
        (".replaced", None),
        ("x_audio.flac", None),
        ("x_audio.flac.replaced.part", None),
    ];
    for (name, owner) in table {
        assert_eq!(replaced_owner(name), owner, "{name}");
    }
}

#[test]
fn a_dub_prev_belongs_to_its_songs_dub() {
    let dub = song("abcdefghijk", "_dub.flac");
    assert_eq!(dub_prev_owner(&format!("{dub}.prev")), Some(dub.as_str()));
    let video = song("abcdefghijk", "_video.mp4.prev");
    assert_eq!(dub_prev_owner(&video), None, "an upgrade's .prev");
    assert_eq!(dub_prev_owner(&dub), None);
    assert_eq!(dub_prev_owner("abcdefghijk_dub.flac.prev"), None);
}

/// A video no row has: every kind of its files and its work dirs go once
/// 7 days old, not a millisecond before.
#[test]
fn a_video_no_row_has_goes_after_seven_days() {
    let names = [
        file(&song("gone0000001", "_video.mp4"), ORPHAN_AFTER_MS),
        file(&song("gone0000001", "_audio.flac"), ORPHAN_AFTER_MS),
        file(&song("gone0000001", "_audio_vocals.flac"), ORPHAN_AFTER_MS),
        file(&song("gone0000001", "_dub_events.jsonl"), ORPHAN_AFTER_MS),
        file(&song("gone0000001", "_dub.flac.prev"), ORPHAN_AFTER_MS),
        file(
            &song("gone0000001", "_audio.flac.replaced"),
            ORPHAN_AFTER_MS,
        ),
        file("gone0000001_lyrics.json", ORPHAN_AFTER_MS),
        file("gone0000001_g35t_audit.json", ORPHAN_AFTER_MS),
        file("gone0000001_description_lyrics.json", ORPHAN_AFTER_MS),
        dir("gone0000001_isolation", ORPHAN_AFTER_MS),
        dir("gone0000001_dub", ORPHAN_AFTER_MS),
    ];
    for entry in &names {
        assert_eq!(why_alone(entry), Some(Why::NoRow), "{}", entry.name);
        let fresh = Entry {
            modified_ms: entry.modified_ms.map(|m| m + 1),
            ..entry.clone()
        };
        assert_eq!(
            why_alone(&fresh),
            None,
            "{} a millisecond younger",
            fresh.name
        );
    }
}

/// A video a row has keeps every file, whatever its age; ids and song bases
/// compare ignoring case (NTFS); a file a row records is kept whatever its
/// name says; an entry whose time is unknown, or an unknown name, is kept.
#[test]
fn what_a_row_has_or_records_is_kept() {
    let old = 400 * DAY_MS;
    let kept = [
        file(&song("rowvideo001", "_dub_events.jsonl"), old),
        file("rowvideo001_lyrics.json", old),
        file("ROWVIDEO001_g35t_words.json", old),
        file("upper_CASE1_lyrics.json", old),
        dir("rowvideo001_stemsep", old),
        // recorded by name, in any case
        file(&song("rowvideo001", "_VIDEO.mp4"), old),
        file("gone0000001_notes.txt", old),
        file("tools", old),
        dir("peer", old),
        dir(".cli-proxy-api", old),
    ];
    for entry in &kept {
        assert_eq!(why_alone(entry), None, "{}", entry.name);
    }
    let unknown_age = Entry {
        modified_ms: None,
        ..file("gone0000001_lyrics.json", old)
    };
    assert_eq!(why_alone(&unknown_age), None);
}

/// A row recording a file of ANOTHER video's name keeps it, and a base
/// recorded by any column (the dub's) keeps the whole song.
#[test]
fn a_recorded_file_or_base_keeps_a_file_of_a_video_no_row_has() {
    let mut facts = RowFacts::default();
    let odd = song("gone0000001", "_audio.flac");
    let odd_path = format!("/c/{odd}");
    let dub = format!("/c/{}", song("gone0000002", "_dub.flac"));
    let paths = [
        Some(odd_path.as_str()),
        None,
        None,
        None,
        Some(dub.as_str()),
    ];
    facts.add_row("other000001", &paths);
    let at = |name: &str| why(&file(name, 400 * DAY_MS), &HashMap::new(), &facts, NOW);
    assert_eq!(at(&odd), None, "recorded by name");
    assert_eq!(
        at(&song("gone0000002", "_dub_events.jsonl")),
        None,
        "a recorded base"
    );
    assert_eq!(at(&song("gone0000003", "_audio.flac")), Some(Why::NoRow));
}

/// Stale names go 7 days after their last write, whatever their video.
#[test]
fn a_stale_name_goes_after_seven_days_even_for_a_row_s_video() {
    let stale = file("rowvideo001_whisperx_track.json", STALE_AFTER_MS);
    assert_eq!(why_alone(&stale), Some(Why::Stale));
    let young = file("rowvideo001_whisperx_track.json", STALE_AFTER_MS - 1);
    assert_eq!(why_alone(&young), None);
    let old_cache = file("rowvideo001_genius_cleaned_v2.json", STALE_AFTER_MS);
    assert_eq!(why_alone(&old_cache), Some(Why::Stale));
    let current = file("rowvideo001_genius_cleaned_v3.json", 400 * DAY_MS);
    assert_eq!(why_alone(&current), None);
    let stale_dir = dir("rowvideo001_whisperx_track.json", 400 * DAY_MS);
    assert_eq!(why_alone(&stale_dir), None, "a dir is never a stale file");
}

/// A `.replaced` goes 30 days after its last write only while the name it
/// was set aside from holds a file (never an only copy).
#[test]
fn a_replaced_file_goes_only_beside_its_name() {
    let owner = song("rowvideo001", "_audio.flac");
    let aside = file(&format!("{owner}.2.replaced"), REPLACED_AFTER_MS);
    let mut files = HashMap::new();
    let facts = facts();
    assert_eq!(why(&aside, &files, &facts, NOW), None, "the name is empty");
    files.insert(owner.as_str(), Some(NOW));
    assert_eq!(why(&aside, &files, &facts, NOW), Some(Why::Replaced));
    let young = file(&format!("{owner}.2.replaced"), REPLACED_AFTER_MS - 1);
    assert_eq!(why(&young, &files, &facts, NOW), None);
}

/// A `.replaced` of a name that is no song file is never the sweep's, even
/// beside its name (a set-aside only ever comes from a song's unit move).
#[test]
fn a_set_aside_of_a_name_that_is_no_song_stays() {
    let aside = file("notes.txt.replaced", 400 * DAY_MS);
    let mut files = HashMap::new();
    files.insert("notes.txt", Some(NOW - 400 * DAY_MS));
    assert_eq!(why(&aside, &files, &facts(), NOW), None);
}

/// A database with no row at all takes no file for a video no row has (a
/// fresh or replaced database must not empty the cache); the rules that
/// need no row still hold.
#[test]
fn a_database_with_no_row_takes_no_file_of_a_video() {
    let empty = RowFacts::default();
    assert!(empty.is_empty());
    assert!(!facts().is_empty());
    let old = 400 * DAY_MS;
    let at = |entry: &Entry| why(entry, &HashMap::new(), &empty, NOW);
    assert_eq!(at(&file(&song("gone0000001", "_audio.flac"), old)), None);
    assert_eq!(at(&file("gone0000001_lyrics.json", old)), None);
    assert_eq!(at(&dir("gone0000001_isolation", old)), None);
    assert_eq!(at(&file("gone0000001_chunks.json", old)), Some(Why::Stale));
    assert_eq!(at(&part("a.part", old)), Some(Why::Part));
}

/// A dub's `.prev` goes 14 days after the later of its own and its dub's
/// last write, only while its dub is there.
#[test]
fn a_dub_prev_goes_fourteen_days_after_the_trim() {
    let dub = song("rowvideo001", "_dub.flac");
    let prev = file(&format!("{dub}.prev"), 300 * DAY_MS);
    let facts = facts();
    let with_dub = |dub_age: Option<i64>| {
        let mut files = HashMap::new();
        files.insert(dub.as_str(), dub_age.map(|age| NOW - age));
        why(&prev, &files, &facts, NOW)
    };
    assert_eq!(why(&prev, &HashMap::new(), &facts, NOW), None, "no dub");
    assert_eq!(with_dub(Some(DUB_PREV_AFTER_MS)), Some(Why::DubPrev));
    assert_eq!(
        with_dub(Some(DUB_PREV_AFTER_MS - 1)),
        None,
        "trimmed 14 d - 1 ms ago"
    );
    assert_eq!(with_dub(None), None, "the dub's time unknown");
    let young_prev = file(&format!("{dub}.prev"), DUB_PREV_AFTER_MS - 1);
    let mut files = HashMap::new();
    files.insert(dub.as_str(), Some(NOW - 300 * DAY_MS));
    assert_eq!(
        why(&young_prev, &files, &facts, NOW),
        None,
        "its own write is later"
    );
}

/// A peer part goes 7 days after its last write; nothing else in `peer/`.
#[test]
fn a_peer_part_goes_after_seven_days() {
    assert_eq!(
        why_alone(&part(
            "rowvideo001_audio_0123456789abcdef.part",
            PART_AFTER_MS
        )),
        Some(Why::Part)
    );
    assert_eq!(
        why_alone(&part(
            "rowvideo001_audio_0123456789abcdef.part",
            PART_AFTER_MS - 1
        )),
        None
    );
    assert_eq!(
        why_alone(&part("gone0000001_lyrics.json", 400 * DAY_MS)),
        None
    );
    let part_dir = Entry {
        is_dir: true,
        ..part("x.part", 400 * DAY_MS)
    };
    assert_eq!(why_alone(&part_dir), None);
}

/// The plan names each entry with its reason, the oldest first, at most
/// 500; how many more were due is `left`.
#[test]
fn a_pass_takes_the_oldest_five_hundred() {
    let mut entries: Vec<Entry> = (0_i64..501)
        .map(|i| file(&format!("gone{i:07}_lyrics.json"), ORPHAN_AFTER_MS + i))
        .collect();
    entries.push(file("rowvideo001_lyrics.json", 400 * DAY_MS));
    let Plan { delete, left } = plan(&entries, &facts(), NOW);
    assert_eq!(left, 1);
    assert_eq!(delete.len(), 500);
    assert_eq!(
        delete[0].name, "gone0000500_lyrics.json",
        "the oldest first"
    );
    assert!(
        !delete.iter().any(|p| p.name == "gone0000000_lyrics.json"),
        "the youngest waits"
    );
}

/// Each reason in one plan, with the entry's place, kind and bytes; the
/// siblings come from the listing itself.
#[test]
fn a_plan_names_each_reason() {
    let dub = song("rowvideo001", "_dub.flac");
    let owner = song("rowvideo001", "_audio.flac");
    let mut gone = dir("gone0000001_isolation", 9 * DAY_MS);
    gone.bytes = 77;
    let entries = vec![
        gone,
        file("rowvideo001_chunks.json", 8 * DAY_MS),
        file(&owner, DAY_MS),
        file(&format!("{owner}.replaced"), 40 * DAY_MS),
        file(&dub, 20 * DAY_MS),
        file(&format!("{dub}.prev"), 50 * DAY_MS),
        part("a.part", 10 * DAY_MS),
        file("rowvideo001_lyrics.json", 400 * DAY_MS),
    ];
    fn planned(place: Place, name: &str, is_dir: bool, bytes: u64, reason: Why) -> Planned {
        Planned {
            place,
            name: name.to_string(),
            is_dir,
            bytes,
            why: reason,
        }
    }
    assert_eq!(
        plan(&entries, &facts(), NOW),
        Plan {
            delete: vec![
                planned(
                    Place::Cache,
                    &format!("{dub}.prev"),
                    false,
                    10,
                    Why::DubPrev
                ),
                planned(
                    Place::Cache,
                    &format!("{owner}.replaced"),
                    false,
                    10,
                    Why::Replaced
                ),
                planned(Place::Parts, "a.part", false, 10, Why::Part),
                planned(Place::Cache, "gone0000001_isolation", true, 77, Why::NoRow),
                planned(
                    Place::Cache,
                    "rowvideo001_chunks.json",
                    false,
                    10,
                    Why::Stale
                ),
            ],
            left: 0,
        }
    );
}

/// A part does not stand in for a cache file of its name, and a dir does
/// not hold a `.replaced`'s name.
#[test]
fn only_the_cache_dirs_files_are_siblings() {
    let owner = song("rowvideo001", "_audio.flac");
    let entries = vec![
        part(&owner, DAY_MS),
        file(&format!("{owner}.replaced"), 40 * DAY_MS),
    ];
    assert_eq!(plan(&entries, &facts(), NOW).delete, vec![]);
    let entries = vec![
        dir(&owner, DAY_MS),
        file(&format!("{owner}.replaced"), 40 * DAY_MS),
    ];
    assert_eq!(plan(&entries, &facts(), NOW).delete, vec![]);
}

#[test]
fn every_reason_has_its_log_name() {
    let names = [
        (Why::NoRow, "no_row"),
        (Why::Stale, "stale"),
        (Why::Replaced, "replaced"),
        (Why::DubPrev, "dub_prev"),
        (Why::Part, "part"),
    ];
    for (why, name) in names {
        assert_eq!(why.as_str(), name);
    }
}
