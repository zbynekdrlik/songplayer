//! #229 `peer::board`: a running job is announced while its guard lives,
//! one entry per kind it makes, and the announcement ends with the last
//! guard.

use std::sync::Arc;

use super::*;
use crate::peer::Exchange;
use crate::peer::kind::{ArtifactKind, Job};
use crate::peer::wire::{JobState, rfc3339_to_ms};

#[test]
fn a_job_is_listed_while_its_guard_lives() {
    let board = Arc::new(JobBoard::default());
    assert!(board.snapshot("pp").is_empty());
    let guard = board.announce("aaaaaaaaaaa", Job::Stems);
    let jobs = board.snapshot("pp");
    let kinds: Vec<ArtifactKind> = jobs.iter().map(|j| j.kind).collect();
    assert_eq!(
        kinds,
        vec![ArtifactKind::StemInstrumental, ArtifactKind::StemVocals]
    );
    assert!(
        jobs.iter().all(|j| j.youtube_id == "aaaaaaaaaaa"
            && j.node == "pp"
            && j.state == JobState::Running)
    );
    let started = jobs[0].started_at.as_deref().and_then(rfc3339_to_ms);
    assert!(started.is_some(), "a running job carries its start");
    assert_eq!(jobs[0].started_at, jobs[1].started_at, "one start per job");
    drop(guard);
    assert!(board.snapshot("pp").is_empty());
}

#[test]
fn a_second_guard_of_the_same_job_keeps_it_listed_until_both_end() {
    let board = Arc::new(JobBoard::default());
    let first = board.announce("aaaaaaaaaaa", Job::Download);
    let second = board.announce("aaaaaaaaaaa", Job::Download);
    drop(first);
    assert_eq!(board.snapshot("pp").len(), 3, "video, audio, metadata");
    drop(second);
    assert!(board.snapshot("pp").is_empty());
}

/// Ending one job leaves the others of the same video and of other videos.
#[test]
fn a_guard_ends_only_its_own_job() {
    let board = Arc::new(JobBoard::default());
    let lyrics = board.announce("aaaaaaaaaaa", Job::Lyrics);
    let stems = board.announce("aaaaaaaaaaa", Job::Stems);
    let other = board.announce("bbbbbbbbbbb", Job::Lyrics);
    drop(stems);
    let left: Vec<(String, ArtifactKind)> = board
        .snapshot("pp")
        .into_iter()
        .map(|j| (j.youtube_id, j.kind))
        .collect();
    assert_eq!(
        left,
        vec![
            ("aaaaaaaaaaa".to_string(), ArtifactKind::Lyrics),
            ("bbbbbbbbbbb".to_string(), ArtifactKind::Lyrics),
        ]
    );
    drop(lyrics);
    drop(other);
    assert!(board.snapshot("pp").is_empty());
}

#[test]
fn jobs_are_listed_in_youtube_id_order() {
    let board = Arc::new(JobBoard::default());
    let ids = ["ddddddddddd", "bbbbbbbbbbb", "aaaaaaaaaaa", "ccccccccccc"];
    let _guards: Vec<JobGuard> = ids
        .iter()
        .map(|id| board.announce(id, Job::Lyrics))
        .collect();
    let listed: Vec<String> = board
        .snapshot("snv")
        .into_iter()
        .map(|j| j.youtube_id)
        .collect();
    assert_eq!(
        listed,
        vec!["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc", "ddddddddddd"]
    );
}

/// `Exchange::announce` announces on the exchange's own board (the board
/// lane 3's catalog lists).
#[tokio::test]
async fn the_exchange_announces_on_its_own_board() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ex = Exchange::new(pool, dir.path().to_path_buf());
    let guard = ex.announce("aaaaaaaaaaa", Job::Lyrics);
    let jobs = ex.board.snapshot("snv");
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        (
            jobs[0].youtube_id.as_str(),
            jobs[0].kind,
            jobs[0].node.as_str()
        ),
        ("aaaaaaaaaaa", ArtifactKind::Lyrics, "snv")
    );
    drop(guard);
    assert!(ex.board.snapshot("snv").is_empty());
}
