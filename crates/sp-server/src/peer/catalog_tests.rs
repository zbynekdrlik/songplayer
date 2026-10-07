//! #229 `peer::catalog`: what this node lists, from its own rows + the hash
//! cache, and the jobs it runs or has queued.

use std::path::{Path, PathBuf};

use ArtifactKind::{Audio, Lyrics, Metadata, StemInstrumental, StemVocals, Video};

use super::*;
use crate::db::models_peer::{HashEntry, put_hash};
use crate::peer::hash::sha256_hex;
use crate::peer::kind::{ArtifactKind, Job, MEDIA_VERSION, STEMS_VERSION};
use crate::peer::rig::TestNode;
use crate::peer::wire::{Catalog, CatalogJob, JobState};

const YT: &str = "aaaaaaaaaaa";

/// Cache `path`'s real sha256, as hashed at `at` ms.
async fn hash(node: &TestNode, path: &Path, at: i64) {
    let bytes = std::fs::read(path).unwrap();
    let entry = HashEntry {
        path: path_key(path),
        size: bytes.len() as i64,
        mtime_ms: 1,
        sha256: sha256_hex(&bytes),
        hashed_at_ms: at,
    };
    put_hash(node.pool(), &entry).await.unwrap();
}

fn kinds(c: &Catalog, youtube_id: &str) -> Vec<ArtifactKind> {
    let mut k: Vec<ArtifactKind> = c
        .artifacts
        .iter()
        .filter(|a| a.youtube_id == youtube_id)
        .map(|a| a.kind)
        .collect();
    k.sort_by_key(|k| k.as_str());
    k
}

#[tokio::test]
async fn a_file_is_listed_once_hashed_with_its_size_sha_and_version() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (video, audio) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    let before = build(&node.ex, "snv", None).await.unwrap();
    assert_eq!(kinds(&before, YT), vec![Metadata], "nothing hashed yet");
    let counted = counts(&node.ex).await.unwrap();
    assert_eq!((counted.files, counted.listed), (2, 0));
    hash(&node, &video, 100).await;
    hash(&node, &audio, 100).await;
    let c = build(&node.ex, "snv", None).await.unwrap();
    assert_eq!(c.node, "snv");
    assert_eq!(kinds(&c, YT), vec![Audio, Metadata, Video]);
    let a = c.artifacts.iter().find(|a| a.kind == Audio).unwrap();
    assert_eq!(a.size, 3_000);
    assert_eq!(a.sha256, sha256_hex(&std::fs::read(&audio).unwrap()));
    assert_eq!(a.version, MEDIA_VERSION);
    assert_eq!(a.updated_at.as_deref(), Some("1970-01-01T00:00:00.100Z"));
    let v = c.artifacts.iter().find(|a| a.kind == Video).unwrap();
    assert_eq!((v.size, v.version), (2_000, MEDIA_VERSION));
    let m = c.artifacts.iter().find(|a| a.kind == Metadata).unwrap();
    assert_eq!(m.version, 1, "a provider's title (gemini, not failed)");
    let counted = counts(&node.ex).await.unwrap();
    assert_eq!((counted.files, counted.listed), (2, 2));
}

#[tokio::test]
async fn stems_are_listed_when_done_under_the_audios_name() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (_, audio) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    let before = artifact_files(node.pool(), node.cache(), None)
        .await
        .unwrap();
    assert!(
        before.iter().all(|f| f.kind == Video || f.kind == Audio),
        "no stems before they are done"
    );
    let (vocals, instrumental) = node.give_stems(id).await;
    assert_eq!(
        (vocals.clone(), instrumental.clone()),
        crate::stems::stem_paths(&audio)
    );
    let files = artifact_files(node.pool(), node.cache(), None)
        .await
        .unwrap();
    let stems: Vec<(ArtifactKind, u32, PathBuf)> = files
        .iter()
        .filter(|f| f.kind == StemVocals || f.kind == StemInstrumental)
        .map(|f| (f.kind, f.version, f.path.clone()))
        .collect();
    assert_eq!(
        stems,
        vec![
            (StemVocals, STEMS_VERSION, vocals),
            (StemInstrumental, STEMS_VERSION, instrumental)
        ]
    );
}

#[tokio::test]
async fn lyrics_are_listed_with_the_rows_pipeline_version() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    node.give_song(id, YT, "Way Maker", "Sinach").await;
    node.give_lyrics(id, YT, "mtl+g35t").await;
    let files = artifact_files(node.pool(), node.cache(), None)
        .await
        .unwrap();
    let lyrics = files.iter().find(|f| f.kind == Lyrics).unwrap();
    assert_eq!(lyrics.version, crate::lyrics::LYRICS_PIPELINE_VERSION);
    assert_eq!(lyrics.path, node.cache().join(format!("{YT}_lyrics.json")));
}

/// Review Focus 5: a dubbed video's `{yt}_lyrics.json` is the Live-Translate
/// subtitle track, never lyrics — whichever row of the video asked for the dub.
#[tokio::test]
async fn a_dub_track_is_never_listed_as_lyrics() {
    let node = TestNode::start("snv", None).await;
    let dubbed = node.add_video("ddddddddddd").await;
    node.give_lyrics(dubbed, "ddddddddddd", "mtl+g35t").await;
    sqlx::query("UPDATE videos SET dub_requested = 1 WHERE id = ?")
        .bind(dubbed)
        .execute(node.pool())
        .await
        .unwrap();
    let track = node.add_video("ttttttttttt").await;
    let source = crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE;
    node.give_lyrics(track, "ttttttttttt", source).await;
    // Song lyrics in one playlist, a dub asked for in the other (no lyrics
    // row there yet): the one `{yt}_lyrics.json` may be the dub's next.
    let sung = node.add_video("sssssssssss").await;
    node.give_lyrics(sung, "sssssssssss", "mtl+g35t").await;
    let asked = node.add_video_to(2, "sssssssssss").await;
    sqlx::query("UPDATE videos SET dub_requested = 1 WHERE id = ?")
        .bind(asked)
        .execute(node.pool())
        .await
        .unwrap();
    let plain = node.add_video(YT).await;
    node.give_lyrics(plain, YT, "mtl+g35t").await;
    let files = artifact_files(node.pool(), node.cache(), None)
        .await
        .unwrap();
    let with_lyrics: Vec<&str> = files
        .iter()
        .filter(|f| f.kind == Lyrics)
        .map(|f| f.youtube_id.as_str())
        .collect();
    assert_eq!(with_lyrics, vec![YT]);
}

#[tokio::test]
async fn a_video_in_two_playlists_is_listed_once_and_a_bad_id_never() {
    let node = TestNode::start("snv", None).await;
    let first = node.add_video(YT).await;
    let (video, audio) = node.give_song(first, YT, "Way Maker", "Sinach").await;
    let second = node.add_video_to(2, YT).await;
    crate::db::models::mark_video_processed_pair(
        node.pool(),
        second,
        "Way Maker",
        "Sinach",
        "gemini",
        false,
        &video.to_string_lossy(),
        &audio.to_string_lossy(),
    )
    .await
    .unwrap();
    let odd = node.add_video("not an id").await;
    node.give_song(odd, "not an id", "Odd", "Odd").await;
    node.give_lyrics(odd, "not an id", "mtl+g35t").await;
    let files = artifact_files(node.pool(), node.cache(), None)
        .await
        .unwrap();
    assert_eq!(files.iter().filter(|f| f.kind == Video).count(), 1);
    assert!(files.iter().all(|f| f.youtube_id == YT), "{files:?}");
    let meta = metadata_for(node.pool(), None).await.unwrap();
    let ids: Vec<&str> = meta.iter().map(|m| m.youtube_id.as_str()).collect();
    assert_eq!(ids, vec![YT]);
    let only = artifact_files(node.pool(), node.cache(), Some("bbbbbbbbbbb"))
        .await
        .unwrap();
    assert!(only.is_empty(), "the id filter");
    let mine = artifact_files(node.pool(), node.cache(), Some(YT))
        .await
        .unwrap();
    assert_eq!(mine.len(), 2, "{mine:?}");
}

#[tokio::test]
async fn since_lists_only_files_hashed_after_it() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (video, audio) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    hash(&node, &video, 100).await;
    hash(&node, &audio, 200).await;
    let at = |since| {
        let ex = node.ex.clone();
        async move { kinds(&build(&ex, "snv", Some(since)).await.unwrap(), YT) }
    };
    assert_eq!(at(99).await, vec![Audio, Metadata, Video]);
    assert_eq!(
        at(100).await,
        vec![Audio, Metadata],
        "hashed exactly at `since` is not after it"
    );
    assert_eq!(at(199).await, vec![Audio, Metadata]);
    assert_eq!(
        at(200).await,
        vec![Metadata],
        "metadata has no time: always listed"
    );
}

#[tokio::test]
async fn metadata_carries_its_version_and_the_sha_of_its_bytes() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    node.give_song(id, YT, "Way Maker", "Sinach").await;
    sqlx::query("UPDATE videos SET metadata_source = 'manual' WHERE id = ?")
        .bind(id)
        .execute(node.pool())
        .await
        .unwrap();
    let meta = metadata_for(node.pool(), Some(YT)).await.unwrap();
    assert_eq!(meta.len(), 1);
    assert_eq!(meta[0].song, "Way Maker");
    assert_eq!(meta[0].artist, "Sinach");
    assert_eq!(meta[0].version(), crate::peer::kind::METADATA_MANUAL);
    let c = build(&node.ex, "snv", None).await.unwrap();
    let a = c.artifacts.iter().find(|a| a.kind == Metadata).unwrap();
    assert_eq!(a.sha256, sha256_hex(&meta[0].to_bytes()));
    assert_eq!(a.size, meta[0].to_bytes().len() as u64);
    assert_eq!(a.version, 2);
    assert_eq!(a.updated_at, None);
}

#[tokio::test]
async fn the_running_jobs_are_listed_under_the_node_name() {
    let node = TestNode::start("snv", None).await;
    let _job = node.ex.announce(YT, Job::Lyrics);
    let c = build(&node.ex, "snv", None).await.unwrap();
    assert_eq!(c.jobs.len(), 1);
    let j = &c.jobs[0];
    assert_eq!(
        (j.youtube_id.as_str(), j.kind, j.node.as_str(), j.state),
        (YT, Lyrics, "snv", JobState::Running)
    );
    assert!(j.started_at.is_some());
}

/// The plan's decisions (ROZHODNUTÉ 6022851957 point 2): the queued jobs
/// too, and a job both queued and running once, as running.
#[tokio::test]
async fn queued_jobs_are_listed_and_a_running_one_only_once() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    node.give_song(id, YT, "Way Maker", "Sinach").await;
    node.add_video("bbbbbbbbbbb").await;
    let _job = node.ex.announce("bbbbbbbbbbb", Job::Download);
    node.add_video("ccccccccccc").await;
    let c = build(&node.ex, "snv", None).await.unwrap();
    let got: Vec<(&str, ArtifactKind, JobState, bool)> = c
        .jobs
        .iter()
        .map(|j| {
            assert_eq!(j.node, "snv");
            (
                j.youtube_id.as_str(),
                j.kind,
                j.state,
                j.started_at.is_some(),
            )
        })
        .collect();
    let (q, r) = (JobState::Queued, JobState::Running);
    assert_eq!(
        got,
        vec![
            (YT, Lyrics, q, false),
            (YT, StemInstrumental, q, false),
            (YT, StemVocals, q, false),
            ("bbbbbbbbbbb", Audio, r, true),
            ("bbbbbbbbbbb", Metadata, r, true),
            ("bbbbbbbbbbb", Video, r, true),
            ("ccccccccccc", Audio, q, false),
            ("ccccccccccc", Metadata, q, false),
            ("ccccccccccc", Video, q, false),
        ]
    );
    let counted = counts(&node.ex).await.unwrap();
    assert_eq!(counted.queued, 6, "the running download is not counted");
}

fn running(youtube_id: &str, kind: ArtifactKind) -> CatalogJob {
    CatalogJob {
        youtube_id: youtube_id.into(),
        kind,
        node: "snv".into(),
        state: JobState::Running,
        started_at: Some("2026-10-07T06:00:00.000Z".into()),
    }
}

#[test]
fn listed_jobs_adds_each_queued_kind_once_and_sorts() {
    let queued = vec![
        ("ccccccccccc".to_string(), Job::Stems),
        (YT.to_string(), Job::Lyrics),
        (YT.to_string(), Job::Lyrics),
        ("bbbbbbbbbbb".to_string(), Job::Lyrics),
    ];
    let jobs = listed_jobs(vec![running("bbbbbbbbbbb", Lyrics)], &queued, "snv");
    let queued_entry = |youtube_id: &str, kind| CatalogJob {
        youtube_id: youtube_id.into(),
        kind,
        node: "snv".into(),
        state: JobState::Queued,
        started_at: None,
    };
    assert_eq!(
        jobs,
        vec![
            queued_entry(YT, Lyrics),
            running("bbbbbbbbbbb", Lyrics),
            queued_entry("ccccccccccc", StemInstrumental),
            queued_entry("ccccccccccc", StemVocals),
        ]
    );
}
