//! #229: the download job asks first — two real nodes, a counting provider.

use super::*;
use crate::db::models_peer::fetch_record;
use crate::downloader::VideoRow;
use crate::downloader::cache::{audio_filename, video_filename};
use crate::metadata::manual::{DownloadTitle, MANUAL_SOURCE};
use crate::peer::ask::Ask;
use crate::peer::config::NodeConfig;
use crate::peer::kind::{ArtifactKind, Job};
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

/// A peer's title counts as taken only when it is the title
/// `record_download` wrote: a correction made meanwhile (#136) is written
/// instead, and then nothing came from the peer.
#[test]
fn a_peer_title_counts_as_taken_only_when_it_was_written() {
    let taken = PeerTitle::of("snv", &meta(Some("gemini"), false, "Way Maker")).unwrap();
    assert_eq!(
        written_title(Some(taken.clone()), &taken.title),
        Some(taken.clone())
    );
    let correction = DownloadTitle {
        song: "Cesta".into(),
        artist: "Zbor".into(),
        source: MANUAL_SOURCE,
        gemini_failed: false,
    };
    assert_eq!(written_title(Some(taken), &correction), None);
    assert_eq!(written_title(None, &correction), None);
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
    let (title_from, version, sha) = fetch_record(pp.pool(), YT, "metadata")
        .await
        .unwrap()
        .expect("the title taken from the peer is recorded as its");
    assert_eq!((title_from.as_str(), version), ("snv", 1));
    assert_eq!(
        sha,
        snv_title_sha(&pp).await,
        "the catalog's sha of the title"
    );
    assert_eq!(
        std::fs::read_dir(pp.ex.parts_dir()).unwrap().count(),
        0,
        "no part left"
    );
    assert!(pp.ex.board.snapshot("pp").is_empty(), "nothing announced");
}

/// The sha256 SNV's catalog lists for the title of `YT`.
async fn snv_title_sha(pp: &TestNode) -> String {
    let cfg = NodeConfig::load(pp.pool()).await.unwrap();
    let catalog = pp
        .ex
        .client
        .catalog(cfg.peer("snv").unwrap())
        .await
        .unwrap();
    catalog
        .artifacts
        .iter()
        .find(|a| a.youtube_id == YT && a.kind == ArtifactKind::Metadata)
        .unwrap()
        .sha256
        .clone()
}

/// The final names of the pair PP's row takes (the peer's title).
fn final_pair(pp: &TestNode) -> (std::path::PathBuf, std::path::PathBuf) {
    (
        pp.cache()
            .join(video_filename("Way Maker", "Sinach", YT, false)),
        pp.cache()
            .join(audio_filename("Way Maker", "Sinach", YT, false)),
    )
}

/// The kinds of the parts left in PP's parts dir, sorted.
fn parts_of(pp: &TestNode) -> Vec<String> {
    let mut kinds: Vec<String> = std::fs::read_dir(pp.ex.parts_dir())
        .map(|d| {
            d.map(|e| {
                let name = e.unwrap().file_name().to_string_lossy().into_owned();
                let rest = name.strip_prefix(&format!("{YT}_")).unwrap().to_string();
                rest.rsplit_once('_').unwrap().0.to_string()
            })
            .collect::<Vec<String>>()
        })
        .unwrap_or_default();
    kinds.sort();
    kinds
}

/// The video goes first: when it cannot take its final name (another row's
/// video open in a player; here a directory) nothing is touched, both
/// verified parts stay, and the next ask places them.
#[tokio::test]
async fn a_video_that_cannot_take_its_name_touches_nothing() {
    let (_snv, pp, row) = snv_and_pp().await;
    let (video, audio) = final_pair(&pp);
    std::fs::create_dir_all(&video).unwrap();
    let (chain, _calls) = counting_chain();
    assert!(matches!(
        first(Some(&pp.ex), &chain, &row).await,
        PeerStep::Deferred
    ));
    assert!(!audio.exists(), "no audio under its final name");
    assert_eq!(row_now(&pp, row.id).await.normalized, 0);
    assert_eq!(parts_of(&pp), vec!["audio", "video"], "both parts stay");
    assert_eq!(fetch_record(pp.pool(), YT, "metadata").await.unwrap(), None);
    std::fs::remove_dir(&video).unwrap();
    assert!(matches!(
        first(Some(&pp.ex), &chain, &row).await,
        PeerStep::Done
    ));
    assert_eq!(std::fs::read(&video).unwrap(), bytes(2_000, 1));
    assert_eq!(std::fs::read(&audio).unwrap(), bytes(3_000, 2));
    assert!(parts_of(&pp).is_empty());
}

/// A final name may hold another row's file of the same video (rows share
/// files by name, #136): a failed video rename leaves that row's audio as
/// it was.
#[tokio::test]
async fn a_failed_video_rename_leaves_another_rows_audio_as_it_was() {
    let (_snv, pp, row) = snv_and_pp().await;
    let (video, audio) = final_pair(&pp);
    std::fs::write(&audio, b"another row's audio").unwrap();
    std::fs::create_dir_all(&video).unwrap();
    let (chain, _calls) = counting_chain();
    assert!(matches!(
        first(Some(&pp.ex), &chain, &row).await,
        PeerStep::Deferred
    ));
    assert_eq!(std::fs::read(&audio).unwrap(), b"another row's audio");
}

/// When the audio cannot take its final name, a video this attempt placed
/// goes back into its part: no unrecorded video under a final name, and
/// both parts stay for the next ask.
#[tokio::test]
async fn an_audio_that_cannot_take_its_name_puts_the_video_back() {
    let (_snv, pp, row) = snv_and_pp().await;
    let (video, audio) = final_pair(&pp);
    std::fs::create_dir_all(&audio).unwrap();
    let (chain, _calls) = counting_chain();
    assert!(matches!(
        first(Some(&pp.ex), &chain, &row).await,
        PeerStep::Deferred
    ));
    assert!(!video.exists(), "no unrecorded video under its final name");
    assert_eq!(parts_of(&pp), vec!["audio", "video"], "both parts stay");
}

/// ... and never takes away a video that was there before (another row's):
/// it now holds the peer's verified copy of the same video (the rename
/// replaced it, which cannot be undone), and stays.
#[tokio::test]
async fn an_audio_that_cannot_take_its_name_keeps_a_video_that_was_there() {
    let (_snv, pp, row) = snv_and_pp().await;
    let (video, audio) = final_pair(&pp);
    std::fs::write(&video, b"another row's video").unwrap();
    std::fs::create_dir_all(&audio).unwrap();
    let (chain, _calls) = counting_chain();
    assert!(matches!(
        first(Some(&pp.ex), &chain, &row).await,
        PeerStep::Deferred
    ));
    assert_eq!(
        std::fs::read(&video).unwrap(),
        bytes(2_000, 1),
        "a video that was there stays, as the peer's copy"
    );
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
    assert_eq!(
        fetch_record(pp.pool(), YT, "metadata").await.unwrap(),
        None,
        "this node's own title: nothing taken from the peer"
    );
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

/// #229 item A: a peer's parser title is not taken, and this node's
/// providers (paid AI) are not asked either: SNV holds the song and names
/// it too (its own repair). The pair is named by this node's title parser,
/// marked for the repair, which takes SNV's title once SNV has one.
#[tokio::test]
async fn a_peers_parser_title_names_the_pair_by_the_parser_for_the_repair() {
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
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no provider was asked");
    let now = row_now(&pp, row.id).await;
    assert_eq!(
        (now.metadata_source.as_deref(), now.gemini_failed),
        (Some("regex"), 1),
        "the title parser's, in the repair queue"
    );
    assert_eq!(
        fetch_record(pp.pool(), YT, "metadata").await.unwrap(),
        None,
        "no peer's title was taken"
    );
}

/// A fetch that fails asks no provider: a peer's copy is retried on every
/// recheck for up to 2 h, and a paid provider call on each would add up.
/// The title is chosen only once the pair is here.
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

/// A peer's pair that keeps failing for 2 h is downloaded here: the spec's
/// "bounded at ~2 h, then process locally" holds for a failing fetch too.
#[tokio::test]
async fn a_pair_failing_past_the_bound_is_downloaded_here() {
    let (snv, pp, row) = snv_and_pp().await;
    let wrong = crate::peer::hash::sha256_hex(b"not the audio");
    sqlx::query("UPDATE peer_hashes SET sha256 = ? WHERE path LIKE '%_audio.flac'")
        .bind(&wrong)
        .execute(snv.pool())
        .await
        .unwrap();
    let past = crate::peer::wire::now_ms()
        - i64::try_from(crate::peer::decide::MAX_PEER_WAIT.as_millis()).unwrap()
        - 1_000;
    crate::db::models_peer::start_wait(pp.pool(), YT, "download", past)
        .await
        .unwrap();
    let (chain, _) = counting_chain();
    let PeerStep::Local(Some(_guard)) = first(Some(&pp.ex), &chain, &row).await else {
        panic!("expected the download to run here")
    };
    let waits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM peer_waits")
        .fetch_one(pp.pool())
        .await
        .unwrap();
    assert_eq!(waits, 0, "the wait ends: the job runs here");
    assert_eq!(
        row_now(&pp, row.id).await.next_attempt_at,
        None,
        "not deferred"
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

/// A download that runs here writes this node's own audio under the names
/// a fetched pair had: the pair's peer origin goes first, also on the
/// no-peers path, which never goes through `run_here` (else SNV's stems
/// would be taken for this node's own encode once the peer is listed again).
#[tokio::test]
async fn a_download_here_with_no_peers_forgets_the_pairs_peer_origin() {
    let pp = TestNode::start("pp", None).await;
    let id = pp.add_video(YT).await;
    pp.audio_from(YT, "snv").await;
    for kind in ["video", "metadata", "lyrics"] {
        crate::db::models_peer::record_fetch(pp.pool(), YT, kind, "snv", 1, "s", 10)
            .await
            .unwrap();
    }
    let row = VideoRow {
        id,
        youtube_id: YT.into(),
        title: "t".into(),
    };
    let (chain, _) = counting_chain();
    let PeerStep::Local(Some(_guard)) = first(Some(&pp.ex), &chain, &row).await else {
        panic!("expected Local")
    };
    for kind in ["video", "audio", "metadata"] {
        assert_eq!(
            fetch_record(pp.pool(), YT, kind).await.unwrap(),
            None,
            "{kind}"
        );
    }
    assert!(
        fetch_record(pp.pool(), YT, "lyrics")
            .await
            .unwrap()
            .is_some(),
        "a kind the download does not make stays"
    );
}

/// The audio's origin is recorded BEFORE `record_download` makes the row
/// playable (a stems or lyrics ask in between must find it): with every
/// UPDATE of the row refused, `record_download` fails, and the record of the
/// pair now under this node's names is already there.
#[tokio::test]
async fn an_adopted_pair_records_its_origin_before_the_row_plays() {
    let (_snv, pp, row) = snv_and_pp().await;
    let Ask::Fetch(plan) = pp.ex.ask(Job::Download, YT).await else {
        panic!("expected Fetch")
    };
    sqlx::query(
        "CREATE TRIGGER refuse_row_updates BEFORE UPDATE ON videos \
         BEGIN SELECT RAISE(ABORT, 'refused by the test'); END",
    )
    .execute(pp.pool())
    .await
    .unwrap();
    let (chain, _) = counting_chain();
    assert!(adopt(&pp.ex, &chain, &row, &plan).await.is_err());
    assert_eq!(row_now(&pp, row.id).await.normalized, 0, "never recorded");
    let (node, version, sha) = fetch_record(pp.pool(), YT, "audio")
        .await
        .unwrap()
        .expect("the audio's origin, recorded first");
    assert_eq!((node.as_str(), version), ("snv", 1));
    assert_eq!(sha, crate::peer::rig::song_audio_sha());
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
