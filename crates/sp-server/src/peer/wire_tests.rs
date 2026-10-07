//! #229 `peer::wire`: the peer API's typed JSON — what a catalog keeps from
//! a newer peer, the jobs it announces (running or queued), the metadata
//! artifact's bytes and the time helpers.

use super::*;
use crate::peer::kind::ArtifactKind;

/// A sha256 as 64 lowercase hex digits (built, so no 40+ hex blob sits in
/// the source: the staging hook refuses one as a possible key).
fn sha() -> String {
    "0123456789abcdef".repeat(4)
}

fn artifact(id: &str, kind: ArtifactKind, sha: &str) -> Artifact {
    Artifact {
        youtube_id: id.into(),
        kind,
        version: 1,
        size: 10,
        sha256: sha.into(),
        updated_at: None,
    }
}

fn job(id: &str, kind: ArtifactKind, state: JobState) -> CatalogJob {
    CatalogJob {
        youtube_id: id.into(),
        kind,
        node: "snv".into(),
        state,
        started_at: None,
    }
}

#[test]
fn a_sha256_is_64_lowercase_hex_digits() {
    let sha = sha();
    assert!(is_sha256_hex(&sha));
    assert!(!is_sha256_hex(&sha[..63]));
    assert!(!is_sha256_hex(&format!("{sha}0")));
    assert!(!is_sha256_hex(&sha.to_uppercase()));
    assert!(!is_sha256_hex(&sha.replace('0', "g")));
    assert!(!is_sha256_hex(""));
}

/// Review Focus 3: a newer peer's catalog — an unknown kind, a bad id, a bad
/// sha, a job state this node does not know, a node name that does not hold,
/// fields this node does not know (at the top and inside entries) — keeps
/// every entry this node can use; a time that is not one reads as `None`.
#[test]
fn sanitized_keeps_only_what_this_node_can_use() {
    let sha = sha();
    let json = format!(
        r#"{{"node":"snv","artifacts":[
            {{"youtube_id":"aaaaaaaaaaa","kind":"audio","version":1,"size":10,"sha256":"{sha}","updated_at":"soon","codec":"flac"}},
            {{"youtube_id":"aaaaaaaaaaa","kind":"dub","version":1,"size":10,"sha256":"{sha}"}},
            {{"youtube_id":"../../x","kind":"video","version":1,"size":10,"sha256":"{sha}"}},
            {{"youtube_id":"bbbbbbbbbbb","kind":"video","version":1,"size":10,"sha256":"nothex"}}],
          "jobs":[
            {{"youtube_id":"ccccccccccc","kind":"lyrics","node":"snv","state":"running","started_at":"2026-10-06T16:00:00.123Z","progress":0.5}},
            {{"youtube_id":"ddddddddddd","kind":"audio","node":"snv","state":"queued","started_at":"yesterday"}},
            {{"youtube_id":"fffffffffff","kind":"lyrics","node":"SNV site","state":"running","started_at":"t"}},
            {{"youtube_id":"ccccccccccc","kind":"dub","node":"snv","state":"running","started_at":"t"}},
            {{"youtube_id":"eeeeeeeeeee","kind":"lyrics","node":"snv","state":"paused"}},
            {{"youtube_id":"bad","kind":"lyrics","node":"snv","state":"running","started_at":"t"}}],
          "later_field":true}}"#
    );
    let c: Catalog = serde_json::from_str::<Catalog>(&json).unwrap().sanitized();
    assert_eq!(c.node, "snv");
    assert_eq!(
        c.artifacts,
        vec![artifact("aaaaaaaaaaa", ArtifactKind::Audio, &sha)]
    );
    let running = CatalogJob {
        started_at: Some("2026-10-06T16:00:00.123Z".into()),
        ..job("ccccccccccc", ArtifactKind::Lyrics, JobState::Running)
    };
    let queued = job("ddddddddddd", ArtifactKind::Audio, JobState::Queued);
    assert_eq!(c.jobs, vec![running, queued]);
}

/// The catalog's own node name is checked like a configured one
/// (`peer::config::valid_name`): one that does not hold reads as empty.
#[test]
fn a_catalog_node_name_that_does_not_hold_reads_as_empty() {
    let too_long = "a".repeat(33);
    for bad in ["", "SNV", "snv\nINFO forged", too_long.as_str()] {
        let c = Catalog {
            node: bad.to_string(),
            ..Catalog::default()
        };
        assert_eq!(c.sanitized().node, "", "{bad:?}");
    }
    let c = Catalog {
        node: "pp-2".into(),
        ..Catalog::default()
    };
    assert_eq!(c.sanitized().node, "pp-2");
}

/// A peer's time is kept only when it is an RFC 3339 time, and then in this
/// node's canonical form: never the peer's own text (whitespace, a fraction
/// of any length, another offset).
#[test]
fn a_peer_time_is_kept_only_when_it_is_one() {
    let long_fraction = format!("2026-10-06T16:00:00.123{}Z", "4".repeat(997));
    for time in [
        "2026-10-06T16:00:00.123Z",
        "2026-10-06T16:00:00.123Z\r\n",
        long_fraction.as_str(),
        "2026-10-06T18:00:00.123+02:00",
        "2026-10-06t16:00:00.123z",
    ] {
        assert_eq!(
            checked_time(Some(time)).as_deref(),
            Some("2026-10-06T16:00:00.123Z"),
            "{time:?}"
        );
    }
    assert_eq!(checked_time(Some("t")), None);
    assert_eq!(
        checked_time(Some("2026-10-06T16:00:00.123Z\nINFO forged")),
        None
    );
    assert_eq!(checked_time(None), None);
}

#[test]
fn a_catalog_without_lists_reads_as_empty() {
    let c: Catalog = serde_json::from_str(r#"{"node":"snv"}"#).unwrap();
    assert!(c.artifacts.is_empty() && c.jobs.is_empty());
}

/// The job entry on the wire: its state in snake_case, and a queued job's
/// missing start as `null`.
#[test]
fn a_job_names_its_state_on_the_wire() {
    let running = CatalogJob {
        started_at: Some("2026-10-06T16:00:00.123Z".into()),
        ..job("aaaaaaaaaaa", ArtifactKind::Lyrics, JobState::Running)
    };
    assert_eq!(
        serde_json::to_string(&running).unwrap(),
        r#"{"youtube_id":"aaaaaaaaaaa","kind":"lyrics","node":"snv","state":"running","started_at":"2026-10-06T16:00:00.123Z"}"#
    );
    let queued = job("aaaaaaaaaaa", ArtifactKind::Audio, JobState::Queued);
    let text = serde_json::to_string(&queued).unwrap();
    assert_eq!(
        text,
        r#"{"youtube_id":"aaaaaaaaaaa","kind":"audio","node":"snv","state":"queued","started_at":null}"#
    );
    assert_eq!(serde_json::from_str::<CatalogJob>(&text).unwrap(), queued);
}

/// A peer that has the job running OR queued announces it (ROZHODNUTÉ
/// 6022851957: a node waits for a peer's queued job too).
#[test]
fn announces_matches_the_video_and_any_of_the_kinds_running_or_queued() {
    use ArtifactKind::*;
    let c = Catalog {
        node: "snv".into(),
        artifacts: vec![],
        jobs: vec![
            job("aaaaaaaaaaa", StemVocals, JobState::Running),
            job("bbbbbbbbbbb", Lyrics, JobState::Queued),
        ],
    };
    assert!(c.announces("aaaaaaaaaaa", &[StemVocals, StemInstrumental]));
    assert!(!c.announces("aaaaaaaaaaa", &[Lyrics]));
    assert!(!c.announces("ccccccccccc", &[StemVocals]));
    assert!(c.announces("bbbbbbbbbbb", &[Lyrics]), "a queued job");
    assert!(!c.announces("bbbbbbbbbbb", &[StemVocals]));
    assert!(!c.announces("aaaaaaaaaaa", &[]));
}

#[test]
fn metadata_bytes_are_canonical_and_carry_the_version() {
    let m = PeerMetadata {
        youtube_id: "aaaaaaaaaaa".into(),
        song: "Way Maker".into(),
        artist: "Sinach".into(),
        metadata_source: Some("gemini".into()),
        gemini_failed: false,
    };
    assert_eq!(m.version(), 1);
    assert_eq!(
        String::from_utf8(m.to_bytes()).unwrap(),
        r#"{"youtube_id":"aaaaaaaaaaa","song":"Way Maker","artist":"Sinach","metadata_source":"gemini","gemini_failed":false}"#
    );
    let back: PeerMetadata = serde_json::from_slice(&m.to_bytes()).unwrap();
    assert_eq!(back, m);
    let gf = PeerMetadata {
        gemini_failed: true,
        ..m.clone()
    };
    assert_eq!(gf.version(), 0);
    let manual = PeerMetadata {
        metadata_source: Some("manual".into()),
        ..m
    };
    assert_eq!(manual.version(), 2);
}

#[test]
fn times_go_both_ways_at_millisecond_precision() {
    let ms = 1_791_302_400_123;
    let text = ms_to_rfc3339(ms);
    assert_eq!(text, "2026-10-06T16:00:00.123Z");
    assert_eq!(rfc3339_to_ms(&text), Some(ms));
    assert_eq!(rfc3339_to_ms("2026-10-06T18:00:00.123+02:00"), Some(ms));
    assert_eq!(rfc3339_to_ms(" 2026-10-06T16:00:00.123Z "), Some(ms));
    assert_eq!(rfc3339_to_ms("yesterday"), None);
    assert_eq!(ms_to_rfc3339(i64::MAX), "", "out of range reads as empty");
    assert!(now_ms() > ms - 86_400_000 * 365);
}
