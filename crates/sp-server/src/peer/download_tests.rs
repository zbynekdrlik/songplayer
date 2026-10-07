//! #229: the download job asks first — two real nodes, a counting provider.

use super::*;
use crate::db::models_peer::fetch_record;
use crate::downloader::VideoRow;
use crate::downloader::cache::{audio_filename, video_filename};
use crate::metadata::manual::MANUAL_SOURCE;
use crate::peer::kind::Job;
use crate::peer::rig::{SNV_KEY, TestNode, bytes, counting_chain};
use crate::peer::wire::PeerMetadata;
use sp_core::metadata::MetadataSource;
use std::sync::atomic::Ordering;

const YT: &str = "aaaaaaaaaaa";

/// SNV has the song hashed; PP asks SNV and has an undownloaded row of it.
async fn snv_and_pp() -> (TestNode, TestNode, VideoRow) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = snv.add_video(YT).await;
    snv.give_song(id, YT, "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let pp_id = pp.add_video(YT).await;
    let row = VideoRow {
        id: pp_id,
        youtube_id: YT.into(),
        title: "A YouTube title".into(),
    };
    (snv, pp, row)
}

#[derive(Debug, sqlx::FromRow)]
struct RowNow {
    normalized: i64,
    song: Option<String>,
    artist: Option<String>,
    metadata_source: Option<String>,
    gemini_failed: i64,
    file_path: Option<String>,
    audio_file_path: Option<String>,
    download_attempts: i64,
    next_attempt_at: Option<String>,
}

async fn row_now(node: &TestNode, id: i64) -> RowNow {
    sqlx::query_as(
        "SELECT normalized, song, artist, metadata_source, gemini_failed, file_path, \
                audio_file_path, download_attempts, next_attempt_at \
         FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(node.pool())
    .await
    .unwrap()
}

fn meta(source: Option<&str>, gemini_failed: bool, song: &str) -> PeerMetadata {
    PeerMetadata {
        youtube_id: YT.into(),
        song: song.into(),
        artist: "Sinach".into(),
        metadata_source: source.map(str::to_string),
        gemini_failed,
    }
}

#[test]
fn a_peers_provider_or_operator_title_is_taken_a_parser_guess_is_not() {
    assert_eq!(MetadataSource::Gemini.as_str(), "gemini");
    assert_eq!(MetadataSource::Regex.as_str(), "regex");
    let t = adopted_title(&meta(Some("gemini"), false, "Way Maker")).unwrap();
    assert_eq!(
        (
            t.song.as_str(),
            t.artist.as_str(),
            t.source,
            t.gemini_failed
        ),
        ("Way Maker", "Sinach", "gemini", false)
    );
    let manual = adopted_title(&meta(Some("manual"), false, "Cesta")).unwrap();
    assert_eq!(
        (manual.song.as_str(), manual.source),
        ("Cesta", MANUAL_SOURCE)
    );
    assert_eq!(
        adopted_title(&meta(Some("manual"), true, "Cesta"))
            .unwrap()
            .source,
        MANUAL_SOURCE,
        "an operator's title whatever the flag"
    );
    assert_eq!(
        adopted_title(&meta(Some("regex"), false, "Odd")),
        None,
        "a parser's title (no provider configured there), metadata version 0"
    );
    assert_eq!(
        adopted_title(&meta(Some("gemini"), true, "Guess")),
        None,
        "a parser guess"
    );
    assert_eq!(
        adopted_title(&meta(Some("gemini"), false, "  ")),
        None,
        "no song"
    );
    assert_eq!(
        adopted_title(&meta(Some("someday-a-new-source"), false, "X")),
        None,
        "unknown source"
    );
    assert_eq!(adopted_title(&meta(None, false, "X")), None);
}

/// The spec's "a peer has the artifact, so the other node fetches and
/// processes nothing".
#[tokio::test]
async fn a_peers_pair_is_taken_and_nothing_runs_here() {
    let (_snv, pp, row) = snv_and_pp().await;
    let (chain, calls) = counting_chain();
    let step = first(Some(&pp.ex), &chain, &row).await;
    assert!(matches!(step, PeerStep::Done));
    let now = row_now(&pp, row.id).await;
    assert_eq!(now.normalized, 1);
    assert_eq!(
        (now.song.as_deref(), now.artist.as_deref()),
        (Some("Way Maker"), Some("Sinach"))
    );
    assert_eq!(
        (now.metadata_source.as_deref(), now.gemini_failed),
        (Some("gemini"), 0)
    );
    assert_eq!(now.download_attempts, 0);
    let audio = pp
        .cache()
        .join(audio_filename("Way Maker", "Sinach", YT, false));
    let video = pp
        .cache()
        .join(video_filename("Way Maker", "Sinach", YT, false));
    assert_eq!(
        now.audio_file_path.as_deref(),
        Some(audio.to_string_lossy().as_ref())
    );
    assert_eq!(
        now.file_path.as_deref(),
        Some(video.to_string_lossy().as_ref())
    );
    assert_eq!(std::fs::read(&audio).unwrap(), bytes(3_000, 2));
    assert_eq!(std::fs::read(&video).unwrap(), bytes(2_000, 1));
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no provider was asked");
    let (node, _, _) = fetch_record(pp.pool(), YT, "audio").await.unwrap().unwrap();
    assert_eq!(node, "snv");
    assert!(
        fetch_record(pp.pool(), YT, "video")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        std::fs::read_dir(pp.ex.parts_dir()).unwrap().count(),
        0,
        "no part left"
    );
    assert!(pp.ex.board.snapshot("pp").is_empty(), "nothing announced");
}

#[tokio::test]
async fn an_operator_correction_here_names_the_pair() {
    let (_snv, pp, row) = snv_and_pp().await;
    let other = pp.add_video_to(2, YT).await;
    sqlx::query(
        "UPDATE videos SET song = 'Cesta', artist = 'Zbor', metadata_source = 'manual' WHERE id = ?",
    )
    .bind(other)
    .execute(pp.pool())
    .await
    .unwrap();
    let (chain, calls) = counting_chain();
    assert!(matches!(
        first(Some(&pp.ex), &chain, &row).await,
        PeerStep::Done
    ));
    let now = row_now(&pp, row.id).await;
    assert_eq!(
        (now.song.as_deref(), now.metadata_source.as_deref()),
        (Some("Cesta"), Some("manual"))
    );
    assert!(
        pp.cache()
            .join(audio_filename("Cesta", "Zbor", YT, false))
            .exists()
    );
    assert!(
        !pp.cache()
            .join(audio_filename("Way Maker", "Sinach", YT, false))
            .exists()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_peers_operator_title_is_taken_as_an_operators() {
    let (snv, pp, row) = snv_and_pp().await;
    sqlx::query(
        "UPDATE videos SET song = 'Opraveny', metadata_source = 'manual' WHERE youtube_id = ?",
    )
    .bind(YT)
    .execute(snv.pool())
    .await
    .unwrap();
    let (chain, calls) = counting_chain();
    assert!(matches!(
        first(Some(&pp.ex), &chain, &row).await,
        PeerStep::Done
    ));
    let now = row_now(&pp, row.id).await;
    assert_eq!(
        (now.song.as_deref(), now.metadata_source.as_deref()),
        (Some("Opraveny"), Some("manual"))
    );
    assert!(
        pp.cache()
            .join(audio_filename("Opraveny", "Sinach", YT, false))
            .exists()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_peers_parser_title_asks_this_nodes_providers() {
    let (snv, pp, row) = snv_and_pp().await;
    sqlx::query(
        "UPDATE videos SET gemini_failed = 1, metadata_source = 'regex' WHERE youtube_id = ?",
    )
    .bind(YT)
    .execute(snv.pool())
    .await
    .unwrap();
    let (chain, calls) = counting_chain();
    assert!(matches!(
        first(Some(&pp.ex), &chain, &row).await,
        PeerStep::Done
    ));
    let now = row_now(&pp, row.id).await;
    assert_eq!(
        (now.song.as_deref(), now.artist.as_deref()),
        (Some("Chain Song"), Some("Chain Artist"))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        pp.cache()
            .join(audio_filename("Chain Song", "Chain Artist", YT, false))
            .exists()
    );
}

/// A fetch that fails asks no provider: a peer's copy is retried on every
/// recheck (a Fetch beats the 2 h bound), and a paid provider call on each
/// would add up. The title is chosen only once the pair is here.
#[tokio::test]
async fn a_failed_fetch_asks_no_provider() {
    let (snv, pp, row) = snv_and_pp().await;
    sqlx::query(
        "UPDATE videos SET gemini_failed = 1, metadata_source = 'regex' WHERE youtube_id = ?",
    )
    .bind(YT)
    .execute(snv.pool())
    .await
    .unwrap();
    let wrong = crate::peer::hash::sha256_hex(b"not the audio");
    sqlx::query("UPDATE peer_hashes SET sha256 = ? WHERE path LIKE '%_audio.flac'")
        .bind(&wrong)
        .execute(snv.pool())
        .await
        .unwrap();
    let (chain, calls) = counting_chain();
    assert!(matches!(
        first(Some(&pp.ex), &chain, &row).await,
        PeerStep::Deferred
    ));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no provider for a failed fetch"
    );
}

/// The spec's "sha256 mismatch → discard and retry".
#[tokio::test]
async fn a_sha_mismatch_defers_then_the_retry_takes_the_pair() {
    let (snv, pp, row) = snv_and_pp().await;
    let wrong = crate::peer::hash::sha256_hex(b"not the audio");
    sqlx::query("UPDATE peer_hashes SET sha256 = ? WHERE path LIKE '%_audio.flac'")
        .bind(&wrong)
        .execute(snv.pool())
        .await
        .unwrap();
    let (chain, _) = counting_chain();
    assert!(matches!(
        first(Some(&pp.ex), &chain, &row).await,
        PeerStep::Deferred
    ));
    let now = row_now(&pp, row.id).await;
    assert_eq!((now.normalized, now.download_attempts), (0, 0));
    assert!(now.next_attempt_at.is_some(), "re-picked later");
    let waits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM peer_waits")
        .fetch_one(pp.pool())
        .await
        .unwrap();
    assert_eq!(waits, 1, "the failed fetch counts as waiting");
    sqlx::query("DELETE FROM peer_hashes")
        .execute(snv.pool())
        .await
        .unwrap();
    snv.hash_now().await;
    assert!(matches!(
        first(Some(&pp.ex), &chain, &row).await,
        PeerStep::Done
    ));
    assert_eq!(row_now(&pp, row.id).await.normalized, 1);
    let waits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM peer_waits")
        .fetch_one(pp.pool())
        .await
        .unwrap();
    assert_eq!(waits, 0, "the wait ends with the peer's copy");
}

/// The spec's "a peer is running the job, so the other waits".
#[tokio::test]
async fn a_peer_downloading_it_is_waited_for_without_an_attempt() {
    let (snv, pp, _) = snv_and_pp().await;
    let _running = snv.ex.announce("bbbbbbbbbbb", Job::Download);
    let id = pp.add_video("bbbbbbbbbbb").await;
    let row = VideoRow {
        id,
        youtube_id: "bbbbbbbbbbb".into(),
        title: "t".into(),
    };
    let (chain, calls) = counting_chain();
    assert!(matches!(
        first(Some(&pp.ex), &chain, &row).await,
        PeerStep::Deferred
    ));
    let now = row_now(&pp, id).await;
    assert_eq!((now.normalized, now.download_attempts), (0, 0));
    let next =
        chrono::DateTime::parse_from_rfc3339(now.next_attempt_at.as_deref().unwrap()).unwrap();
    let ahead = (next.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
    assert!((100..=125).contains(&ahead), "{ahead}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// The spec's "nobody has it, so the node processes and announces".
#[tokio::test]
async fn nobody_has_it_so_it_runs_here_announced() {
    let (_snv, pp, _) = snv_and_pp().await;
    let id = pp.add_video("ccccccccccc").await;
    let row = VideoRow {
        id,
        youtube_id: "ccccccccccc".into(),
        title: "t".into(),
    };
    let (chain, _) = counting_chain();
    let PeerStep::Local(Some(guard)) = first(Some(&pp.ex), &chain, &row).await else {
        panic!("expected Local")
    };
    let running = pp.ex.board.snapshot("pp");
    assert_eq!(running.len(), 3, "video, audio and metadata announced");
    assert!(running.iter().all(|j| j.youtube_id == "ccccccccccc"));
    let catalog = crate::peer::catalog::build(&pp.ex, "pp", None)
        .await
        .unwrap();
    let listed = catalog
        .jobs
        .iter()
        .filter(|j| {
            j.youtube_id == "ccccccccccc" && j.state == crate::peer::wire::JobState::Running
        })
        .count();
    assert_eq!(listed, 3, "this node's catalog lists the job as running");
    drop(guard);
    assert!(pp.ex.board.snapshot("pp").is_empty());
    let now = row_now(&pp, id).await;
    assert_eq!(
        (now.normalized, now.next_attempt_at),
        (0, None),
        "nothing written"
    );
}

#[tokio::test]
async fn with_no_exchange_the_worker_runs_as_before() {
    let pp = TestNode::start("pp", None).await;
    let id = pp.add_video(YT).await;
    let row = VideoRow {
        id,
        youtube_id: YT.into(),
        title: "t".into(),
    };
    let (chain, _) = counting_chain();
    assert!(matches!(
        first(None, &chain, &row).await,
        PeerStep::Local(None)
    ));
}
