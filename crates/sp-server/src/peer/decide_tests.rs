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

/// The song this node took from `peer`: an audio of sha256 `sha` ([`art`]
/// lists [`sha`]).
fn took<'a>(peer: &'a str, sha: &'a str) -> Option<SongFrom<'a>> {
    Some(SongFrom { peer, sha256: sha })
}

#[test]
fn no_peers_processes_here() {
    assert_eq!(
        decide(Job::Download, YT, &[], None, None),
        Decision::Local(LocalWhy::NoPeers)
    );
    assert_eq!(
        decide(Job::Download, YT, &[], Some(3 * MAX_PEER_WAIT), None),
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
        decide(Job::Download, YT, &[read("snv", Some(&snv))], None, None),
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
        decide(Job::Stems, YT, &[read("snv", Some(&snv))], None, None),
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
        decide(Job::Download, YT, &[read("snv", Some(&newer))], None, None),
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
        decide(Job::Stems, YT, &[read("snv", Some(&snv))], Some(MIN), None),
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
        decide(Job::Download, YT, &[read("snv", Some(&snv))], None, None),
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
        decide(Job::Stems, YT, &[read("snv", Some(&snv))], None, None),
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
        decide(
            Job::Stems,
            YT,
            &[read("snv", Some(&other_video))],
            None,
            None
        ),
        Decision::Local(LocalWhy::NobodyHasIt),
        "a job of another video"
    );
}

/// #229 PP audit (comment 6054582866): SNV had the song PP fetched (its
/// audio listed) but no lyrics job announced, and PP processed the lyrics
/// itself. A lyrics job now waits while a listed peer has the song, within
/// the same 2 h bound; it runs here only when no listed peer has it at all.
/// A peer announcing the job is named first, a peer with the song before an
/// unreadable one; the stems and the download do not wait on the song.
#[test]
fn a_lyrics_job_waits_while_a_peer_has_the_song() {
    let snv = catalog(vec![art(Audio, MEDIA_VERSION)], &[]);
    let reads = [read("snv", Some(&snv))];
    let sha = sha();
    let from_snv = took("snv", &sha);
    let has_the_song = Decision::Wait {
        peer: "snv".into(),
        why: WaitWhy::PeerHasTheSong,
    };
    assert_eq!(
        decide(Job::Lyrics, YT, &reads, None, from_snv),
        has_the_song
    );
    let just_under = MAX_PEER_WAIT - Duration::from_secs(1);
    assert_eq!(
        decide(Job::Lyrics, YT, &reads, Some(just_under), from_snv),
        has_the_song
    );
    assert_eq!(
        decide(Job::Lyrics, YT, &reads, Some(MAX_PEER_WAIT), from_snv),
        Decision::Local(LocalWhy::WaitedLongEnough)
    );
    assert_eq!(
        decide(Job::Lyrics, "bbbbbbbbbbb", &reads, None, from_snv),
        Decision::Local(LocalWhy::NobodyHasIt),
        "the audio of another video"
    );
    assert_eq!(
        decide(Job::Stems, YT, &reads, None, from_snv),
        Decision::Local(LocalWhy::NobodyHasIt)
    );
    assert_eq!(
        decide(Job::Download, YT, &reads, None, from_snv),
        Decision::Local(LocalWhy::NobodyHasIt)
    );
    let runner = catalog(vec![], &[ArtifactKind::Lyrics]);
    assert_eq!(
        decide(
            Job::Lyrics,
            YT,
            &[read("snv", Some(&snv)), read("busy", Some(&runner))],
            None,
            from_snv
        ),
        Decision::Wait {
            peer: "busy".into(),
            why: WaitWhy::PeerRunsIt
        }
    );
    assert_eq!(
        decide(
            Job::Lyrics,
            YT,
            &[read("down", None), read("snv", Some(&snv))],
            None,
            from_snv
        ),
        has_the_song
    );
}

/// Review round 1: the song is the one this node took from a peer. A node
/// whose audio is its own download (or a copy, no `peer_fetches` record)
/// waits on no peer's song — that peer's lyrics would not fit it, and in
/// phase 2 two nodes listing each other would wait on each other's audio.
/// Nor does it wait on another peer that lists the audio, or on its source
/// once that no longer lists it.
#[test]
fn a_lyrics_job_waits_only_on_the_peer_it_took_the_song_from() {
    let sha = sha();
    let with_audio = catalog(vec![art(Audio, MEDIA_VERSION)], &[]);
    let without = catalog(vec![], &[]);
    let local = Decision::Local(LocalWhy::NobodyHasIt);
    let reads = [read("snv", Some(&with_audio))];
    assert_eq!(
        decide(Job::Lyrics, YT, &reads, None, None),
        local,
        "its own audio"
    );
    assert_eq!(
        decide(Job::Lyrics, YT, &reads, None, took("pp2", &sha)),
        local,
        "taken from a peer that is not listed"
    );
    let two = [
        read("pp2", Some(&with_audio)),
        read("snv", Some(&with_audio)),
    ];
    assert_eq!(
        decide(Job::Lyrics, YT, &two, None, took("snv", &sha)),
        Decision::Wait {
            peer: "snv".into(),
            why: WaitWhy::PeerHasTheSong
        },
        "the source, not the first peer listing the audio"
    );
    let gone = [read("snv", Some(&without)), read("pp2", Some(&with_audio))];
    assert_eq!(
        decide(Job::Lyrics, YT, &gone, None, took("snv", &sha)),
        local,
        "the source lists the song no more"
    );
    assert_eq!(song_holder(&two, YT, took("snv", &sha)), Some("snv"));
    assert_eq!(song_holder(&two, YT, None), None);
    assert_eq!(
        song_holder(
            &[read("down", None), read("snv", Some(&with_audio))],
            YT,
            took("snv", &sha)
        ),
        Some("snv")
    );
    assert_eq!(song_holder(&reads, "bbbbbbbbbbb", took("snv", &sha)), None);
}

/// Review round 2: the source must still list the very audio this node took
/// from it (the sha256 of its `peer_fetches` record). A source that
/// downloaded the song again lists another audio, whose lyrics this node
/// would refuse (`same_audio`), so the lyrics do not wait for them.
#[test]
fn a_lyrics_job_waits_only_for_the_audio_it_took() {
    let snv = catalog(vec![art(Audio, MEDIA_VERSION)], &[]);
    let reads = [read("snv", Some(&snv))];
    let other = "fedcba9876543210".repeat(4);
    assert_eq!(
        decide(Job::Lyrics, YT, &reads, None, took("snv", &other)),
        Decision::Local(LocalWhy::NobodyHasIt),
        "the source lists another audio now"
    );
    assert_eq!(song_holder(&reads, YT, took("snv", &other)), None);
    let sha = sha();
    assert_eq!(song_holder(&reads, YT, took("snv", &sha)), Some("snv"));
}

#[test]
fn an_unreadable_peer_is_waited_for_after_the_readable_ones() {
    let busy = catalog(vec![], &[Video]);
    assert_eq!(
        decide(Job::Download, YT, &[read("down", None)], None, None),
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
            None,
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
            None,
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
        decide(Job::Download, YT, &reads, Some(just_under), None),
        Decision::Wait { .. }
    ));
    assert_eq!(
        decide(Job::Download, YT, &reads, Some(MAX_PEER_WAIT), None),
        Decision::Local(LocalWhy::WaitedLongEnough)
    );
    assert_eq!(
        decide(Job::Download, YT, &reads, Some(3 * MAX_PEER_WAIT), None),
        Decision::Local(LocalWhy::WaitedLongEnough),
        "past the bound too"
    );
    assert_eq!(
        decide(
            Job::Download,
            YT,
            &[read("down", None)],
            Some(MAX_PEER_WAIT),
            None
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
        None,
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

/// A failed fetch gives up at the bound, never for this node's own pause:
/// the pause is about its bandwidth, and heavy work instead would defeat it.
#[test]
fn a_failed_fetch_gives_up_at_two_hours_but_never_for_a_pause_here() {
    let m = |n: u64| Duration::from_secs(n * 60);
    assert_eq!(after_failure(Duration::ZERO, false), Some(m(2)));
    assert_eq!(after_failure(m(40), false), Some(m(10)));
    assert_eq!(
        after_failure(MAX_PEER_WAIT - Duration::from_secs(30), false),
        Some(m(1))
    );
    assert_eq!(after_failure(MAX_PEER_WAIT, false), None);
    assert_eq!(after_failure(3 * MAX_PEER_WAIT, false), None);
    assert_eq!(after_failure(Duration::ZERO, true), Some(PAUSED_RECHECK));
    assert_eq!(after_failure(3 * MAX_PEER_WAIT, true), Some(PAUSED_RECHECK));
    assert_eq!(PAUSED_RECHECK, m(5));
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

/// A distinct sha256 per `n` (built, never a hex literal: the staging hook).
fn sha_n(n: char) -> String {
    n.to_string().repeat(64)
}

fn with_sha(youtube_id: &str, kind: ArtifactKind, sha256: String) -> Artifact {
    Artifact {
        youtube_id: youtube_id.into(),
        sha256,
        ..art(kind, MEDIA_VERSION)
    }
}

/// The audio of the video asked: not another video's audio listed before
/// it, not the video's other kinds listed before it.
#[test]
fn the_listed_audio_is_the_videos_own_audio() {
    let c = catalog(
        vec![
            with_sha("bbbbbbbbbbb", Audio, sha_n('a')),
            with_sha(YT, StemVocals, sha_n('b')),
            with_sha(YT, Audio, sha_n('c')),
        ],
        &[],
    );
    assert_eq!(listed_audio(&c, YT), Some(&c.artifacts[2]));
    assert_eq!(listed_audio(&c, "ccccccccccc"), None);
    let no_audio = catalog(vec![with_sha(YT, Video, sha_n('d'))], &[]);
    assert_eq!(listed_audio(&no_audio, YT), None);
}

/// One row per case: what this node recorded (the fetch: node, sha), its
/// row audio's size, what it hashed, the answer. SNV lists sha 'a', 10 bytes.
type AudioCase = (
    &'static str,
    Option<(&'static str, char)>,
    u64,
    Option<char>,
    bool,
);

#[test]
fn this_nodes_audio_is_the_peers_only_by_its_fetch_or_its_own_hash() {
    let snv_audio = with_sha(YT, Audio, sha_n('a'));
    assert_eq!(snv_audio.size, 10);
    let fetched_a = Some(("snv", 'a'));
    let table: [AudioCase; 8] = [
        (
            "fetched from snv at its sha, the same size",
            fetched_a,
            10,
            None,
            true,
        ),
        (
            "fetched, but this row's audio has another size",
            fetched_a,
            11,
            None,
            false,
        ),
        (
            "hashed here at the sha snv lists",
            None,
            10,
            Some('a'),
            true,
        ),
        ("both", fetched_a, 10, Some('a'), true),
        (
            "fetched from another peer",
            Some(("pp2", 'a')),
            10,
            None,
            false,
        ),
        (
            "fetched at an older sha",
            Some(("snv", 'b')),
            10,
            Some('b'),
            false,
        ),
        ("its own encode", None, 10, Some('b'), false),
        ("nothing known here", None, 10, None, false),
    ];
    for (case, fetched, size, hashed, want) in table {
        let fetched = fetched.map(|(node, s)| (node, sha_n(s)));
        let hashed = hashed.map(sha_n);
        let own = OwnAudio {
            fetched: fetched.as_ref().map(|(node, s)| (*node, s.as_str())),
            size,
            hashed: hashed.as_deref(),
        };
        assert_eq!(same_audio("snv", &snv_audio, own), want, "{case}");
    }
}
