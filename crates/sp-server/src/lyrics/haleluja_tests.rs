//! #284: the Slovak lines say "haleluja": the rule, the track and the
//! one-shot pass over the stored tracks.
//! Wired via `#[cfg(test)] #[path = "haleluja_tests.rs"] mod tests;` in `haleluja.rs`.

use std::borrow::Cow;

use sp_core::lyrics::{LyricsLine, LyricsTrack};
use sqlx::SqlitePool;

use super::{Rewritten, fix_track, haleluja, rewrite_stored};

#[test]
fn every_aleluja_word_becomes_haleluja_in_its_own_case() {
    for (sk, want) in [
        ("Aleluja", "Haleluja"),
        ("aleluja", "haleluja"),
        ("ALELUJA", "HALELUJA"),
        ("Pozdvihujem aleluja.", "Pozdvihujem haleluja."),
        ("Aleluja. Aleluja. No tak.", "Haleluja. Haleluja. No tak."),
        ("spievam alelujou", "spievam halelujou"),
        ("Alleluja, amen", "Haleluja, amen"),
        ("(aleluja)", "(haleluja)"),
        ("aleluja-aleluja", "haleluja-haleluja"),
    ] {
        assert_eq!(haleluja(sk), want, "{sk:?}");
    }
}

#[test]
fn a_line_with_nothing_to_change_is_borrowed() {
    for sk in [
        "Haleluja",
        "haleluja, HALELUJA",
        "Galeluja",
        "kaleluja",
        "Hallelujah",
        "Svätý je Pán",
        "",
    ] {
        assert!(matches!(haleluja(sk), Cow::Borrowed(_)), "{sk:?}");
    }
}

fn line(en: &str, sk: Option<&str>) -> LyricsLine {
    LyricsLine {
        start_ms: 1_000,
        end_ms: 2_000,
        en: en.into(),
        sk: sk.map(str::to_string),
        words: None,
    }
}

fn track(lines: Vec<LyricsLine>) -> LyricsTrack {
    LyricsTrack {
        version: 2,
        source: "gemini-3-5-transcribe".into(),
        language_source: "en".into(),
        language_translation: "sk".into(),
        lines,
        register: None,
    }
}

#[test]
fn the_track_fix_counts_the_slovak_lines_it_changed() {
    let mut t = track(vec![
        line("Hallelujah", Some("Aleluja")),
        line("Holy", Some("Svätý")),
        line("Alleluia", None),
        line("Sing alleluia", Some("Spievaj aleluja, aleluja")),
    ]);
    assert_eq!(fix_track(&mut t), 2);
    let sk: Vec<Option<&str>> = t.lines.iter().map(|l| l.sk.as_deref()).collect();
    assert_eq!(
        sk,
        [
            Some("Haleluja"),
            Some("Svätý"),
            None,
            Some("Spievaj haleluja, haleluja")
        ]
    );
    let en: Vec<&str> = t.lines.iter().map(|l| l.en.as_str()).collect();
    assert_eq!(
        en,
        ["Hallelujah", "Holy", "Alleluia", "Sing alleluia"],
        "the EN lines stay"
    );
    assert_eq!(fix_track(&mut t), 0, "nothing left to change");
}

/// A video row: id, YouTube id, `has_lyrics`, `dub_requested`, `lyrics_source`.
type Row<'a> = (i64, &'a str, i64, i64, Option<&'a str>);

async fn pool_with(rows: &[Row<'_>]) -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    for (id, youtube_id, has_lyrics, dub, source) in rows {
        // Each row in a playlist of its own: two rows of one video (a dub on
        // another playlist's row) cannot share one (UNIQUE playlist + video).
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (?, 'p', 'u', 'SP-test', 1)",
        )
        .bind(*id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
             dub_requested, lyrics_source) VALUES (?, ?, ?, 1, ?, ?, ?)",
        )
        .bind(*id)
        .bind(*id)
        .bind(*youtube_id)
        .bind(*has_lyrics)
        .bind(*dub)
        .bind(*source)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool
}

/// Written pretty, unlike the pass's own writes, so a track the pass wrote
/// back with nothing changed shows in its bytes.
fn write_track(dir: &std::path::Path, youtube_id: &str, t: &LyricsTrack) -> Vec<u8> {
    let bytes = serde_json::to_vec_pretty(t).unwrap();
    std::fs::write(dir.join(format!("{youtube_id}_lyrics.json")), &bytes).unwrap();
    bytes
}

fn read_track(dir: &std::path::Path, youtube_id: &str) -> Vec<u8> {
    std::fs::read(dir.join(format!("{youtube_id}_lyrics.json"))).unwrap()
}

/// The one-shot pass writes back a served song's track that says
/// "aleluja", leaves a track with nothing to change byte for byte, a dub's
/// subtitles (a dub row, or a Live-Translate track on any row of the
/// video), a row that serves no lyrics, and a missing file alone.
#[tokio::test]
async fn the_pass_rewrites_only_the_served_song_tracks_that_say_aleluja() {
    let pool = pool_with(&[
        (1, "aaaaaaaaaaa", 1, 0, Some("gemini-3-5-transcribe")),
        (2, "bbbbbbbbbbb", 1, 0, Some("gemini-3-5-transcribe")),
        (3, "ccccccccccc", 1, 1, Some("gemini-3-5-transcribe")),
        (4, "ddddddddddd", 1, 0, Some("gemini-3-5-transcribe")),
        (5, "ddddddddddd", 0, 0, Some("gemini-live-translate")),
        (6, "eeeeeeeeeee", 0, 0, None),
        (7, "fffffffffff", 1, 0, Some("gemini-3-5-transcribe")),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let aleluja = track(vec![
        line("Hallelujah", Some("Aleluja")),
        line("Holy", Some("Svätý")),
    ]);
    write_track(dir.path(), "aaaaaaaaaaa", &aleluja);
    let clean = write_track(
        dir.path(),
        "bbbbbbbbbbb",
        &track(vec![line("Hallelujah", Some("Haleluja"))]),
    );
    let dub = write_track(dir.path(), "ccccccccccc", &aleluja);
    let live = write_track(dir.path(), "ddddddddddd", &aleluja);
    let unserved = write_track(dir.path(), "eeeeeeeeeee", &aleluja);
    let done = rewrite_stored(&pool, dir.path()).await.unwrap();
    assert_eq!(
        done,
        Rewritten {
            tracks: 1,
            lines: 1,
            failed: 0
        }
    );
    let fixed: LyricsTrack =
        serde_json::from_slice(&read_track(dir.path(), "aaaaaaaaaaa")).unwrap();
    let mut want = aleluja.clone();
    want.lines[0].sk = Some("Haleluja".into());
    assert_eq!(fixed, want, "only the SK word changed");
    assert_eq!(read_track(dir.path(), "bbbbbbbbbbb"), clean);
    assert_eq!(read_track(dir.path(), "ccccccccccc"), dub, "a dub row's");
    assert_eq!(
        read_track(dir.path(), "ddddddddddd"),
        live,
        "a Live-Translate row's"
    );
    assert_eq!(
        read_track(dir.path(), "eeeeeeeeeee"),
        unserved,
        "no lyrics served"
    );
    let again = rewrite_stored(&pool, dir.path()).await.unwrap();
    assert_eq!(
        again,
        Rewritten::default(),
        "a second pass has nothing to do"
    );
}

/// A track that cannot be parsed or read (here a directory in its place)
/// is counted, left as it is, and the pass goes on to the next.
#[tokio::test]
async fn a_track_that_cannot_be_read_is_left_and_counted() {
    let pool = pool_with(&[
        (1, "aaaaaaaaaaa", 1, 0, Some("gemini-3-5-transcribe")),
        (2, "bbbbbbbbbbb", 1, 0, Some("gemini-3-5-transcribe")),
        (3, "ccccccccccc", 1, 0, Some("gemini-3-5-transcribe")),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("aaaaaaaaaaa_lyrics.json"), b"{not json").unwrap();
    std::fs::create_dir(dir.path().join("ccccccccccc_lyrics.json")).unwrap();
    write_track(
        dir.path(),
        "bbbbbbbbbbb",
        &track(vec![
            line("Hallelujah", Some("ALELUJA")),
            line("Hallelujah", Some("aleluja")),
        ]),
    );
    let done = rewrite_stored(&pool, dir.path()).await.unwrap();
    assert_eq!(
        done,
        Rewritten {
            tracks: 1,
            lines: 2,
            failed: 2
        }
    );
    assert_eq!(read_track(dir.path(), "aaaaaaaaaaa"), b"{not json");
}

/// Both writers of a Slovak line apply the rule: the translation (an empty
/// translation stays none) and the register check's fix.
#[test]
fn the_translation_and_the_register_fix_write_haleluja() {
    let mut t = track(vec![line("Hallelujah", None), line("Holy", None)]);
    crate::lyrics::worker::LyricsWorker::apply_translations(
        &mut t,
        vec!["Aleluja, aleluja".into(), String::new()],
    );
    assert_eq!(t.lines[0].sk.as_deref(), Some("Haleluja, haleluja"));
    assert_eq!(t.lines[1].sk, None);
    let mut t = track(vec![line("Hallelujah", Some("Sláva"))]);
    let check = crate::lyrics::register_check::apply(
        &mut t,
        vec![crate::lyrics::register_check::Finding::Fix {
            index: 0,
            en: "Hallelujah".into(),
            sk: "Aleluja".into(),
        }],
    );
    assert_eq!(t.lines[0].sk.as_deref(), Some("Haleluja"));
    assert_eq!(check.fixed.len(), 1, "fixed by the check");
}
