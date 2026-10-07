//! #229 `peer::queued`: a job is queued when this node's own worker would
//! take its row now, read with the worker's own predicate.

use super::*;
use crate::peer::kind::Job;
use crate::peer::rig::{TestNode, set};

/// Every row of these tests is downloaded (stems and lyrics are possible).
const DOWNLOADED: &str = "normalized = 1, audio_file_path = '/c/x_audio.flac'";

/// A row of `youtube_id` in `playlist`, then `UPDATE videos SET <set>`.
async fn row(node: &TestNode, playlist: i64, youtube_id: &str, set: &str) -> i64 {
    let id = node.add_video_to(playlist, youtube_id).await;
    sqlx::query(&format!("UPDATE videos SET {set} WHERE id = ?"))
        .bind(id)
        .execute(node.pool())
        .await
        .unwrap();
    id
}

/// Playlist 3, not active.
async fn inactive_playlist(node: &TestNode) {
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (3, 'p3', 'u3', 'SP-p3', 0)",
    )
    .execute(node.pool())
    .await
    .unwrap();
}

fn of(ids: &[&str], job: Job) -> Vec<(String, Job)> {
    ids.iter().map(|id| (id.to_string(), job)).collect()
}

#[tokio::test]
async fn a_download_is_queued_when_the_worker_would_take_it_now() {
    let node = TestNode::start("snv", None).await;
    inactive_playlist(&node).await;
    set(node.pool(), LYRICS_WORKER_ENABLED, "false").await;
    set(node.pool(), STEM_WORKER_ENABLED, "false").await;
    node.add_video("dl_new00001").await;
    let due = "next_attempt_at = '2000-01-01T00:00:00+00:00'";
    row(&node, 1, "dl_retrydue", due).await;
    let later = "next_attempt_at = '2999-01-01T00:00:00+00:00'";
    row(&node, 1, "dl_retryfut", later).await;
    row(&node, 3, "dl_inactive", "normalized = 0").await;
    row(&node, 1, "dl_done0001", DOWNLOADED).await;
    node.add_video("not an id").await;
    node.add_video("dl_twice001").await;
    node.add_video_to(2, "dl_twice001").await;
    assert_eq!(
        queued(node.pool()).await.unwrap(),
        of(
            &["dl_new00001", "dl_retrydue", "dl_twice001"],
            Job::Download
        )
    );
}

#[tokio::test]
async fn lyrics_are_queued_as_the_lyrics_worker_picks_them() {
    let node = TestNode::start("snv", None).await;
    inactive_playlist(&node).await;
    set(node.pool(), STEM_WORKER_ENABLED, "false").await;
    let v = crate::lyrics::LYRICS_PIPELINE_VERSION;
    let old = v - 1;
    let fullmix = crate::lyrics::g35t_transcript::SOURCE_G35T_FULLMIX;
    let rows = [
        (
            1,
            "lyr_manual1",
            format!(
                ", has_lyrics = 1, lyrics_source = 'mtl', lyrics_pipeline_version = {v}, lyrics_manual_priority = 1"
            ),
        ),
        (1, "lyr_null001", String::new()),
        (
            1,
            "lyr_stale01",
            format!(", has_lyrics = 1, lyrics_source = 'mtl', lyrics_pipeline_version = {old}"),
        ),
        (
            1,
            "lyr_current",
            format!(", has_lyrics = 1, lyrics_source = 'mtl', lyrics_pipeline_version = {v}"),
        ),
        (
            1,
            "lyr_parked1",
            format!(", lyrics_source = 'no_source', lyrics_pipeline_version = {v}"),
        ),
        (
            1,
            "lyr_parkold",
            format!(", lyrics_source = 'no_source', lyrics_pipeline_version = {old}"),
        ),
        (
            1,
            "lyr_mparked",
            format!(
                ", lyrics_manual_priority = 1, lyrics_source = 'failed', lyrics_pipeline_version = {v}"
            ),
        ),
        (1, "lyr_dub0001", ", dub_requested = 1".to_string()),
        (
            1,
            "lyr_backoff",
            ", lyrics_next_attempt_at = '2999-01-01T00:00:00.000Z'".to_string(),
        ),
        (3, "lyr_inactv1", String::new()),
        (
            1,
            "lyr_fullmix",
            format!(", has_lyrics = 1, lyrics_source = '{fullmix}', lyrics_pipeline_version = {v}"),
        ),
    ];
    for (playlist, youtube_id, extra) in &rows {
        row(
            &node,
            *playlist,
            youtube_id,
            &format!("{DOWNLOADED}{extra}"),
        )
        .await;
    }
    // Review Focus 5: the catalog never serves lyrics for a video one of
    // whose rows asked for the dub or holds the Live-Translate track, so its
    // plain row is not announced as queued lyrics either.
    let live = crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE;
    let siblings = [
        (1, "lyr_dubsib1", String::new()),
        (2, "lyr_dubsib1", ", dub_requested = 1".to_string()),
        (1, "lyr_ltsib01", String::new()),
        (
            2,
            "lyr_ltsib01",
            format!(", has_lyrics = 1, lyrics_source = '{live}', lyrics_pipeline_version = {v}"),
        ),
    ];
    for (playlist, youtube_id, extra) in &siblings {
        let set = format!("{DOWNLOADED}{extra}");
        row(&node, *playlist, youtube_id, &set).await;
    }
    assert_eq!(
        queued(node.pool()).await.unwrap(),
        of(
            &["lyr_manual1", "lyr_null001", "lyr_parkold", "lyr_stale01"],
            Job::Lyrics
        )
    );
    set(node.pool(), LYRICS_WORKER_ENABLED, " Off ").await;
    let none: Vec<(String, Job)> = Vec::new();
    assert_eq!(queued(node.pool()).await.unwrap(), none, "worker off");
}

#[tokio::test]
async fn stems_are_queued_as_the_stem_worker_picks_them() {
    let node = TestNode::start("snv", None).await;
    set(node.pool(), LYRICS_WORKER_ENABLED, "0").await;
    let rows = [
        ("stm_new0001", String::new()),
        (
            "stm_faildue",
            ", stem_status = 'failed', stem_next_attempt_at = '2000-01-01T00:00:00.000Z'"
                .to_string(),
        ),
        (
            "stm_failfut",
            ", stem_status = 'failed', stem_next_attempt_at = '2999-01-01T00:00:00.000Z'"
                .to_string(),
        ),
        ("stm_done001", ", stem_status = 'done'".to_string()),
        ("stm_unsupp1", ", stem_status = 'unsupported'".to_string()),
        ("stm_noaudio", ", audio_file_path = NULL".to_string()),
    ];
    for (youtube_id, extra) in &rows {
        row(&node, 2, youtube_id, &format!("{DOWNLOADED}{extra}")).await;
    }
    assert_eq!(
        queued(node.pool()).await.unwrap(),
        of(&["stm_faildue", "stm_new0001"], Job::Stems)
    );
    set(node.pool(), STEM_WORKER_ENABLED, "no").await;
    let none: Vec<(String, Job)> = Vec::new();
    assert_eq!(queued(node.pool()).await.unwrap(), none, "worker off");
}

/// Downloads first, then lyrics, then stems; a worker on by default.
#[tokio::test]
async fn every_queue_is_read_with_the_workers_on_by_default() {
    let node = TestNode::start("snv", None).await;
    node.add_video("ccccccccccc").await;
    row(&node, 1, "bbbbbbbbbbb", DOWNLOADED).await;
    let mut want = of(&["ccccccccccc"], Job::Download);
    want.extend(of(&["bbbbbbbbbbb"], Job::Lyrics));
    want.extend(of(&["bbbbbbbbbbb"], Job::Stems));
    assert_eq!(queued(node.pool()).await.unwrap(), want);
}
