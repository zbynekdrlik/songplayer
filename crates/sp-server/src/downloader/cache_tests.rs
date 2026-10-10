//! Tests for the cache scan, the sidecar names and `remove_duplicates`.
//! Sibling of `cache.rs`, wired via `#[path = "cache_tests.rs"]` so
//! `cache.rs` stays under the 1000-line cap (moved out verbatim for #136).

use super::*;
use std::collections::HashSet;
use std::fs;

#[test]
fn sanitize_removes_special_chars() {
    assert_eq!(sanitize_filename("Hello World!"), "Hello World");
    assert_eq!(sanitize_filename("AC/DC"), "ACDC");
    assert_eq!(sanitize_filename("test@#$%^&*()file"), "testfile");
}

#[test]
fn sanitize_collapses_whitespace() {
    assert_eq!(sanitize_filename("  hello   world  "), "hello world");
}

#[test]
fn sanitize_limits_length() {
    let long = "a".repeat(100);
    let result = sanitize_filename(&long);
    assert!(result.len() <= 50);
}

#[test]
fn sanitize_preserves_hyphens() {
    assert_eq!(sanitize_filename("hip-hop"), "hip-hop");
}

#[test]
fn video_filename_without_gf() {
    let name = video_filename("Amazing Grace", "Chris Tomlin", "dQw4w9WgXcQ", false);
    assert_eq!(
        name,
        "Amazing Grace_Chris Tomlin_dQw4w9WgXcQ_normalized_video.mp4"
    );
}

#[test]
fn video_filename_with_gf() {
    let name = video_filename("Song", "Artist", "dQw4w9WgXcQ", true);
    assert_eq!(name, "Song_Artist_dQw4w9WgXcQ_normalized_gf_video.mp4");
}

#[test]
fn audio_filename_without_gf() {
    let name = audio_filename("Amazing Grace", "Chris Tomlin", "dQw4w9WgXcQ", false);
    assert_eq!(
        name,
        "Amazing Grace_Chris Tomlin_dQw4w9WgXcQ_normalized_audio.flac"
    );
}

#[test]
fn audio_filename_with_gf() {
    let name = audio_filename("Song", "Artist", "dQw4w9WgXcQ", true);
    assert_eq!(name, "Song_Artist_dQw4w9WgXcQ_normalized_gf_audio.flac");
}

#[test]
fn scan_cache_pairs_video_and_audio() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();

    fs::write(
        base.join("Amazing Grace_Chris Tomlin_dQw4w9WgXcQ_normalized_video.mp4"),
        "fake video",
    )
    .unwrap();
    fs::write(
        base.join("Amazing Grace_Chris Tomlin_dQw4w9WgXcQ_normalized_audio.flac"),
        "fake audio",
    )
    .unwrap();

    let result = scan_cache(base);
    assert_eq!(result.songs.len(), 1);
    assert!(result.legacy.is_empty());
    assert!(result.orphans.is_empty());

    let song = &result.songs[0];
    assert_eq!(song.video_id, "dQw4w9WgXcQ");
    assert!(!song.gemini_failed);
    assert_eq!(song.song, "Amazing Grace");
    assert_eq!(song.artist, "Chris Tomlin");
}

#[test]
fn scan_cache_flags_legacy_single_mp4() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path()
            .join("Old Song_Old Artist_xxxxxxxxxxx_normalized.mp4"),
        "legacy",
    )
    .unwrap();

    let result = scan_cache(dir.path());
    assert!(result.songs.is_empty());
    assert_eq!(result.legacy.len(), 1);
    assert_eq!(result.legacy[0].video_id, "xxxxxxxxxxx");
}

#[test]
fn scan_cache_flags_legacy_gf_single_mp4() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Old_Song_xxxxxxxxxxx_normalized_gf.mp4"),
        "legacy gf",
    )
    .unwrap();

    let result = scan_cache(dir.path());
    assert_eq!(result.legacy.len(), 1);
    assert!(result.legacy[0].gemini_failed);
}

#[test]
fn scan_cache_orphan_video_without_audio() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("S_A_aaaaaaaaaaa_normalized_video.mp4"), "v").unwrap();

    let result = scan_cache(dir.path());
    assert!(result.songs.is_empty());
    assert_eq!(result.orphans.len(), 1);
}

#[test]
fn scan_cache_orphan_audio_without_video() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("S_A_bbbbbbbbbbb_normalized_audio.flac"),
        "a",
    )
    .unwrap();

    let result = scan_cache(dir.path());
    assert!(result.songs.is_empty());
    assert_eq!(result.orphans.len(), 1);
}

#[test]
fn scan_cache_ignores_unrelated_files() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("README.txt"), "ignore me").unwrap();
    fs::write(dir.path().join("xxxxxxxxxxx_temp.mp4"), "temp").unwrap();

    let result = scan_cache(dir.path());
    assert!(result.songs.is_empty());
    assert!(result.legacy.is_empty());
    assert!(result.orphans.is_empty());
}

#[test]
fn is_valid_video_id_accepts_valid() {
    assert!(is_valid_video_id("dQw4w9WgXcQ"));
    assert!(is_valid_video_id("xxxxxxxxxxx"));
    assert!(is_valid_video_id("abc-def_123"));
}

#[test]
fn is_valid_video_id_rejects_invalid() {
    assert!(!is_valid_video_id("short"));
    assert!(!is_valid_video_id("toolongstring123"));
    assert!(!is_valid_video_id("hello world"));
    assert!(!is_valid_video_id("abc!def@123"));
}

#[test]
fn scan_cache_detects_lyrics_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("dQw4w9WgXcQ_lyrics.json"),
        r#"{"lines":[]}"#,
    )
    .unwrap();

    let result = scan_cache(dir.path());
    assert_eq!(result.lyrics_files.len(), 1);
    assert_eq!(result.lyrics_files[0].0, "dQw4w9WgXcQ");
    assert!(result.songs.is_empty());
    assert!(result.legacy.is_empty());
    assert!(result.orphans.is_empty());
}

#[test]
fn scan_cache_ignores_non_matching_json() {
    let dir = tempfile::tempdir().unwrap();
    // Wrong suffix
    fs::write(dir.path().join("dQw4w9WgXcQ_meta.json"), "{}").unwrap();
    // Too long video id
    fs::write(dir.path().join("dQw4w9WgXcQXXX_lyrics.json"), "{}").unwrap();

    let result = scan_cache(dir.path());
    assert!(result.lyrics_files.is_empty());
}

#[test]
fn scan_cache_picks_up_vocals_files() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("dQw4w9WgXcQ_vocals16k.wav"), "fake vocals").unwrap();
    fs::write(dir.path().join("aBcDeFgHiJk_vocals16k.wav"), "fake").unwrap();
    let result = scan_cache(dir.path());
    assert_eq!(result.vocals_files.len(), 2);
    let ids: HashSet<&str> = result
        .vocals_files
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    assert!(ids.contains("dQw4w9WgXcQ"));
    assert!(ids.contains("aBcDeFgHiJk"));
}

fn touch_at(path: &Path, secs_ago: u64) {
    fs::write(path, b"x").unwrap();
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

/// Box, 27.9.2026: `TwzfEwsTfag` had two complete pairs in the cache — an
/// April pair under the artist "Indiana Bible College" and an August `_gf`
/// pair under "Worthy" (with stems). The scan kept whichever video/audio
/// half it met last per id, so it could even pair one base's video with
/// the other base's audio, and the stale pair stayed forever (the A/V
/// gate refused the ambiguity).
#[test]
fn two_complete_pairs_for_one_id_keep_the_newest_and_list_the_other() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let old_v =
        d.join("Never Lost Champion_Indiana Bible College_TwzfEwsTfag_normalized_video.mp4");
    let old_a =
        d.join("Never Lost Champion_Indiana Bible College_TwzfEwsTfag_normalized_audio.flac");
    let new_v = d.join("Never Lost Champion_Worthy_TwzfEwsTfag_normalized_gf_video.mp4");
    let new_a = d.join("Never Lost Champion_Worthy_TwzfEwsTfag_normalized_gf_audio.flac");
    touch_at(&old_v, 400_000);
    touch_at(&old_a, 400_000);
    touch_at(&new_v, 1_000);
    touch_at(&new_a, 1_000);

    let r = scan_cache(d);
    assert_eq!(r.songs.len(), 1, "one keeper per id");
    assert_eq!(r.songs[0].video_path, new_v, "the newest pair is kept");
    assert_eq!(r.songs[0].audio_path, new_a, "never a cross-paired half");
    assert_eq!(r.songs[0].artist, "Worthy");
    assert!(r.songs[0].gemini_failed);
    assert_eq!(r.duplicates.len(), 1);
    assert_eq!(r.duplicates[0].video_path, old_v);
    assert_eq!(r.duplicates[0].audio_path, old_a);
    assert_eq!(r.duplicates[0].video_id, "TwzfEwsTfag");
    assert!(r.orphans.is_empty(), "both pairs are complete: no orphan");
}

#[test]
fn a_half_of_another_base_is_an_orphan_not_a_pair_partner() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let v = d.join("Song_ArtistA_dQw4w9WgXcQ_normalized_video.mp4");
    let a = d.join("Song_ArtistB_dQw4w9WgXcQ_normalized_audio.flac");
    touch_at(&v, 10);
    touch_at(&a, 10);
    let r = scan_cache(d);
    assert!(r.songs.is_empty(), "different bases never pair");
    assert_eq!(r.orphans.len(), 2);
    assert!(r.duplicates.is_empty());
}

#[test]
fn remove_duplicates_deletes_the_pair_and_its_stems_only() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let dup_v = d.join("S_Old_TwzfEwsTfag_normalized_video.mp4");
    let dup_a = d.join("S_Old_TwzfEwsTfag_normalized_audio.flac");
    let dup_voc = d.join("S_Old_TwzfEwsTfag_normalized_audio_vocals.flac");
    let dup_ins = d.join("S_Old_TwzfEwsTfag_normalized_audio_instrumental.flac");
    let keep_v = d.join("S_New_TwzfEwsTfag_normalized_gf_video.mp4");
    let keep_a = d.join("S_New_TwzfEwsTfag_normalized_gf_audio.flac");
    let keep_voc = d.join("S_New_TwzfEwsTfag_normalized_gf_audio_vocals.flac");
    for p in [
        &dup_v, &dup_a, &dup_voc, &dup_ins, &keep_v, &keep_a, &keep_voc,
    ] {
        fs::write(p, b"x").unwrap();
    }
    remove_duplicates(&[CachedSong {
        video_id: "TwzfEwsTfag".into(),
        song: "S".into(),
        artist: "Old".into(),
        gemini_failed: false,
        video_path: dup_v.clone(),
        audio_path: dup_a.clone(),
    }]);
    for p in [&dup_v, &dup_a, &dup_voc, &dup_ins] {
        assert!(!p.exists(), "{} removed", p.display());
    }
    for p in [&keep_v, &keep_a, &keep_voc] {
        assert!(p.exists(), "{} kept", p.display());
    }
}

/// #223 S9b (D8): a crashed download's temps are their own class, and
/// nothing else is one (a song's sidecars, a name with no 11-char id).
#[test]
fn scan_cache_finds_a_crashed_downloads_temps() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "q_T_-Lh8AFI_video_temp.mp4",
        "q_T_-Lh8AFI_audio_temp.webm",
        "abcdefghijk_video_upgrade_temp.mp4",
        "Song_Artist_q_T_-Lh8AFI_normalized_video.mp4",
        "Song_Artist_q_T_-Lh8AFI_normalized_audio.flac",
        "short_video_temp.mp4",
        "q_T_-Lh8AFI_lyrics.json",
    ] {
        fs::write(dir.path().join(name), "x").unwrap();
    }

    let result = scan_cache(dir.path());
    let mut temps: Vec<String> = result
        .temps
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    temps.sort();
    assert_eq!(
        temps,
        [
            "abcdefghijk_video_upgrade_temp.mp4",
            "q_T_-Lh8AFI_audio_temp.webm",
            "q_T_-Lh8AFI_video_temp.mp4",
        ]
    );
    assert_eq!(result.songs.len(), 1, "the song's pair stays a song");
    assert_eq!(result.lyrics_files.len(), 1);
}
