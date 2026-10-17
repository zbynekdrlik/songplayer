//! #261: the exchange's records of a video no row has any more go; every
//! record a row still needs stays.

use std::collections::{BTreeSet, HashSet};

use sqlx::SqlitePool;

use super::{Dropped, names_a_video, pass};

/// A path names a video when any window of an id's length is one of the
/// ids: at the start, inside, at the end; ids of two lengths.
#[test]
fn a_path_names_a_video_by_any_of_its_ids() {
    let ids = ["aaaaaaaaaaa", "bbbbbbbbbbb", "xyz"];
    let known: HashSet<&[u8]> = ids.iter().map(|id| id.as_bytes()).collect();
    let lengths: BTreeSet<usize> = ids.iter().map(|id| id.len()).collect();
    let names = |path: &str| names_a_video(path, &known, &lengths);
    assert!(names("/c/aaaaaaaaaaa_lyrics.json"));
    assert!(names("/c/Song_Artist_bbbbbbbbbbb_normalized_audio.flac"));
    assert!(names("/c/Song_Artist_qqqqqqqqqqq_normalized_audio.xyz"));
    assert!(!names("/c/Song_Artist_zzzzzzzzzzz_normalized_audio.flac"));
    assert!(!names("/c/aaaaaaaaaa_lyrics.json"), "one letter short");
    assert!(!names(""));
}

/// Rows of `aaaaaaaaaaa` and `bbbbbbbbbbb` (and one with an empty id, which
/// must not keep every hash entry); records of those, of `gone0000001` (no
/// row any more) in every table, and hash entries of each.
async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let statements = [
        "INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')",
        "INSERT INTO videos (id, playlist_id, youtube_id) VALUES \
         (11, 1, 'aaaaaaaaaaa'), (12, 1, 'bbbbbbbbbbb'), (13, 1, '')",
        "INSERT INTO peer_waits (youtube_id, job, since_ms) VALUES \
         ('aaaaaaaaaaa', 'lyrics', 1), ('gone0000001', 'stems', 1)",
        "INSERT INTO peer_fetches (youtube_id, kind, node, version, sha256, fetched_at_ms) VALUES \
         ('bbbbbbbbbbb', 'audio', 'snv', 1, 'ab', 1), ('gone0000001', 'audio', 'snv', 1, 'ab', 1), \
         ('gone0000001', 'video', 'snv', 1, 'ab', 1)",
        "INSERT INTO peer_standins (youtube_id, job, peer, made_at_ms, next_check_ms) VALUES \
         ('aaaaaaaaaaa', 'lyrics', 'snv', 1, 1), ('gone0000001', 'lyrics', 'snv', 1, 1)",
        "INSERT INTO peer_hashes (path, size, mtime_ms, sha256, hashed_at_ms) VALUES \
         ('/c/Song_Artist_aaaaaaaaaaa_normalized_audio.flac', 1, 1, 'ab', 1), \
         ('/c/bbbbbbbbbbb_lyrics.json', 1, 1, 'ab', 1), \
         ('/c/Old_Title_gone0000001_normalized_video.mp4', 1, 1, 'ab', 1), \
         ('/c/Song_Artist_zzzzzzzzzzz_normalized_audio_vocals.flac', 1, 1, 'ab', 1)",
    ];
    for sql in statements {
        sqlx::query(sql).execute(&pool).await.unwrap();
    }
    pool
}

/// `(youtube_id, other key)` of every record of `table`, sorted.
async fn records(pool: &SqlitePool, table: &str, key: &str) -> Vec<(String, String)> {
    sqlx::query_as(&format!(
        "SELECT youtube_id, {key} FROM {table} ORDER BY youtube_id, {key}"
    ))
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_pass_drops_only_the_records_of_videos_no_row_has() {
    let pool = pool().await;
    assert_eq!(
        pass(&pool).await.unwrap(),
        Dropped {
            records: 4,
            hashes: 2,
        }
    );
    let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
    assert_eq!(
        records(&pool, "peer_waits", "job").await,
        [pair("aaaaaaaaaaa", "lyrics")]
    );
    assert_eq!(
        records(&pool, "peer_fetches", "kind").await,
        [pair("bbbbbbbbbbb", "audio")]
    );
    assert_eq!(
        records(&pool, "peer_standins", "job").await,
        [pair("aaaaaaaaaaa", "lyrics")]
    );
    let paths: Vec<String> = sqlx::query_scalar("SELECT path FROM peer_hashes ORDER BY path")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        paths,
        [
            "/c/Song_Artist_aaaaaaaaaaa_normalized_audio.flac",
            "/c/bbbbbbbbbbb_lyrics.json",
        ]
    );
    assert_eq!(
        pass(&pool).await.unwrap(),
        Dropped::default(),
        "a second pass finds nothing"
    );
}
