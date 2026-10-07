//! #229 `peer::decide`.

use super::*;
use crate::peer::kind::{ArtifactKind, Job, MEDIA_VERSION, STEMS_VERSION};
use crate::peer::wire::{Artifact, Catalog, CatalogJob, JobState};
use ArtifactKind::{Audio, StemInstrumental, StemVocals, Video};
use std::time::Duration;

const YT: &str = "aaaaaaaaaaa";
const MIN: Duration = Duration::from_secs(60);

fn sha() -> String {
    "0123456789abcdef".repeat(4)
}

fn art(kind: ArtifactKind, version: u32) -> Artifact {
    Artifact {
        youtube_id: YT.into(),
        kind,
        version,
        size: 10,
        sha256: sha(),
        updated_at: None,
    }
}

/// A catalog holding `artifacts` and announcing a job of `state` for each of
/// `jobs` (one entry per kind, as a node lists them).
fn catalog_with(artifacts: Vec<Artifact>, jobs: &[ArtifactKind], state: JobState) -> Catalog {
    Catalog {
        node: "x".into(),
        artifacts,
        jobs: jobs
            .iter()
            .map(|k| CatalogJob {
                youtube_id: YT.into(),
                kind: *k,
                node: "x".into(),
                state,
                started_at: None,
            })
            .collect(),
    }
}

fn catalog(artifacts: Vec<Artifact>, running: &[ArtifactKind]) -> Catalog {
    catalog_with(artifacts, running, JobState::Running)
}

fn read<'a>(peer: &'a str, c: Option<&'a Catalog>) -> PeerRead<'a> {
    PeerRead { peer, catalog: c }
}

#[test]
fn no_peers_processes_here() {
    assert_eq!(
        decide(Job::Download, YT, &[], None),
        Decision::Local(LocalWhy::NoPeers)
    );
    assert_eq!(
        decide(Job::Download, YT, &[], Some(3 * MAX_PEER_WAIT)),
        Decision::Local(LocalWhy::NoPeers),
        "no peer: never a wait"
    );
}

#[test]
fn a_peer_with_every_needed_artifact_is_fetched_from() {
    let snv = catalog(
        vec![art(Audio, MEDIA_VERSION), art(Video, MEDIA_VERSION)],
        &[],
    );
    assert_eq!(
        decide(Job::Download, YT, &[read("snv", Some(&snv))], None),
        Decision::Fetch {
            peer: "snv".into(),
            artifacts: vec![art(Video, MEDIA_VERSION), art(Audio, MEDIA_VERSION)],
        },
        "every needed artifact, in the job's needs order"
    );
}

#[test]
fn a_peer_missing_one_needed_artifact_is_not_fetched_from() {
    let snv = catalog(vec![art(StemVocals, STEMS_VERSION)], &[]);
    assert_eq!(
        decide(Job::Stems, YT, &[read("snv", Some(&snv))], None),
        Decision::Local(LocalWhy::NobodyHasIt)
    );
    assert_eq!(holds(&snv, Job::Stems, YT), None);
    let only_instrumental = catalog(vec![art(StemInstrumental, STEMS_VERSION)], &[]);
    assert_eq!(holds(&only_instrumental, Job::Stems, YT), None);
    let both = catalog(
        vec![
            art(StemVocals, STEMS_VERSION),
            art(StemInstrumental, STEMS_VERSION),
        ],
        &[],
    );
    assert_eq!(
        holds(&both, Job::Stems, YT),
        Some(vec![
            art(StemVocals, STEMS_VERSION),
            art(StemInstrumental, STEMS_VERSION)
        ])
    );
    assert_eq!(
        holds(&both, Job::Stems, "bbbbbbbbbbb"),
        None,
        "another video"
    );
}

#[test]
fn a_format_this_node_does_not_take_is_not_fetched() {
    let newer = catalog(
        vec![art(Audio, MEDIA_VERSION + 1), art(Video, MEDIA_VERSION)],
        &[],
    );
    assert_eq!(
        decide(Job::Download, YT, &[read("snv", Some(&newer))], None),
        Decision::Local(LocalWhy::NobodyHasIt)
    );
}

#[test]
fn a_peer_that_has_it_wins_over_one_running_it() {
    let runner = catalog(vec![], &[Video]);
    let haver = catalog(
        vec![art(Audio, MEDIA_VERSION), art(Video, MEDIA_VERSION)],
        &[],
    );
    let d = decide(
        Job::Download,
        YT,
        &[read("a", Some(&runner)), read("b", Some(&haver))],
        None,
    );
    assert!(
        matches!(d, Decision::Fetch { ref peer, .. } if peer == "b"),
        "{d:?}"
    );
}

#[test]
fn a_peer_running_the_job_is_waited_for() {
    let snv = catalog(vec![], &[StemInstrumental]);
    assert_eq!(
        decide(Job::Stems, YT, &[read("snv", Some(&snv))], Some(MIN)),
        Decision::Wait {
            peer: "snv".into(),
            why: WaitWhy::PeerRunsIt
        }
    );
}

/// The plan's decisions: a peer's QUEUED job is waited for like a running one
/// (both sites sync the same playlists).
#[test]
fn a_peer_with_the_job_queued_is_waited_for() {
    let snv = catalog_with(
        vec![],
        &[Video, Audio, ArtifactKind::Metadata],
        JobState::Queued,
    );
    assert_eq!(
        decide(Job::Download, YT, &[read("snv", Some(&snv))], None),
        Decision::Wait {
            peer: "snv".into(),
            why: WaitWhy::PeerRunsIt
        }
    );
}

#[test]
fn a_job_making_other_kinds_is_not_waited_for() {
    let snv = catalog(vec![], &[ArtifactKind::Lyrics]);
    assert_eq!(
        decide(Job::Stems, YT, &[read("snv", Some(&snv))], None),
        Decision::Local(LocalWhy::NobodyHasIt)
    );
    let other_video = Catalog {
        jobs: vec![CatalogJob {
            youtube_id: "bbbbbbbbbbb".into(),
            kind: StemVocals,
            node: "x".into(),
            state: JobState::Running,
            started_at: None,
        }],
        ..catalog(vec![], &[])
    };
    assert_eq!(
        decide(Job::Stems, YT, &[read("snv", Some(&other_video))], None),
        Decision::Local(LocalWhy::NobodyHasIt),
        "a job of another video"
    );
}

#[test]
fn an_unreadable_peer_is_waited_for_after_the_readable_ones() {
    let busy = catalog(vec![], &[Video]);
    assert_eq!(
        decide(Job::Download, YT, &[read("down", None)], None),
        Decision::Wait {
            peer: "down".into(),
            why: WaitWhy::PeerUnreadable
        }
    );
    assert_eq!(
        decide(
            Job::Download,
            YT,
            &[read("down", None), read("busy", Some(&busy))],
            None
        ),
        Decision::Wait {
            peer: "busy".into(),
            why: WaitWhy::PeerRunsIt
        }
    );
    let idle = catalog(vec![], &[]);
    assert_eq!(
        decide(
            Job::Download,
            YT,
            &[read("idle", Some(&idle)), read("down", None)],
            None
        ),
        Decision::Wait {
            peer: "down".into(),
            why: WaitWhy::PeerUnreadable
        },
        "an unreadable peer is waited for even when a readable one has nothing"
    );
}

#[test]
fn the_wait_ends_at_two_hours_but_a_peers_copy_is_still_taken() {
    let snv = catalog(vec![], &[Video]);
    let reads = [read("snv", Some(&snv))];
    let just_under = MAX_PEER_WAIT - Duration::from_secs(1);
    assert!(matches!(
        decide(Job::Download, YT, &reads, Some(just_under)),
        Decision::Wait { .. }
    ));
    assert_eq!(
        decide(Job::Download, YT, &reads, Some(MAX_PEER_WAIT)),
        Decision::Local(LocalWhy::WaitedLongEnough)
    );
    assert_eq!(
        decide(Job::Download, YT, &reads, Some(3 * MAX_PEER_WAIT)),
        Decision::Local(LocalWhy::WaitedLongEnough),
        "past the bound too"
    );
    assert_eq!(
        decide(
            Job::Download,
            YT,
            &[read("down", None)],
            Some(MAX_PEER_WAIT)
        ),
        Decision::Local(LocalWhy::WaitedLongEnough),
        "an unreadable peer has the same bound"
    );
    let has = catalog(
        vec![art(Audio, MEDIA_VERSION), art(Video, MEDIA_VERSION)],
        &[],
    );
    let late = decide(
        Job::Download,
        YT,
        &[read("snv", Some(&has))],
        Some(3 * MAX_PEER_WAIT),
    );
    assert!(matches!(late, Decision::Fetch { .. }), "{late:?}");
    assert_eq!(MAX_PEER_WAIT, Duration::from_secs(7_200));
}

#[test]
fn a_job_gives_up_on_its_peers_at_two_hours_exactly() {
    assert!(!gives_up(Duration::ZERO));
    assert!(!gives_up(MAX_PEER_WAIT - Duration::from_millis(1)));
    assert!(gives_up(MAX_PEER_WAIT));
    assert!(gives_up(3 * MAX_PEER_WAIT));
}

#[test]
fn a_recheck_is_a_quarter_of_the_wait_2_to_20_min_never_past_the_bound() {
    let m = |n: u64| Duration::from_secs(n * 60);
    assert_eq!(recheck_after(Duration::ZERO), m(2));
    assert_eq!(recheck_after(m(8)), m(2), "the floor exactly");
    assert_eq!(recheck_after(m(40)), m(10));
    assert_eq!(recheck_after(m(60)), m(15));
    assert_eq!(recheck_after(m(80)), m(20), "the ceiling exactly");
    assert_eq!(recheck_after(m(100)), m(20));
    assert_eq!(recheck_after(m(110)), m(10), "only what is left of the 2 h");
    assert_eq!(
        recheck_after(MAX_PEER_WAIT - Duration::from_secs(30)),
        m(1),
        "at least a minute"
    );
    assert_eq!(recheck_after(m(180)), m(1));
}
