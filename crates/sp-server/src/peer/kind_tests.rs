//! #229 `peer::kind`: the artifact kinds and their wire names, the jobs and
//! the kinds they need and make, and the version of each kind a node takes.

use super::*;
use crate::lyrics::LYRICS_PIPELINE_VERSION;

const ALL: [ArtifactKind; 6] = [
    ArtifactKind::Video,
    ArtifactKind::Audio,
    ArtifactKind::StemVocals,
    ArtifactKind::StemInstrumental,
    ArtifactKind::Lyrics,
    ArtifactKind::Metadata,
];

#[test]
fn every_kind_has_its_wire_name_both_ways() {
    let names = [
        "video",
        "audio",
        "stem_vocals",
        "stem_instrumental",
        "lyrics",
        "metadata",
    ];
    for (kind, name) in ALL.iter().zip(names) {
        assert_eq!(kind.as_str(), name);
        assert_eq!(ArtifactKind::parse(name), Some(*kind));
        assert_eq!(serde_json::to_string(kind).unwrap(), format!("\"{name}\""));
        assert_eq!(
            serde_json::from_str::<ArtifactKind>(&format!("\"{name}\"")).unwrap(),
            *kind
        );
    }
    assert_eq!(ArtifactKind::Unknown.as_str(), "unknown");
    assert_eq!(ArtifactKind::parse("unknown"), None);
    assert_eq!(ArtifactKind::parse("dub"), None);
    assert_eq!(ArtifactKind::parse("Video"), None, "a URL segment is exact");
}

/// Review Focus 3: a newer peer's kind (`dub`) reads as Unknown, never an error.
#[test]
fn a_kind_this_node_does_not_know_reads_as_unknown() {
    assert_eq!(
        serde_json::from_str::<ArtifactKind>("\"dub\"").unwrap(),
        ArtifactKind::Unknown
    );
}

#[test]
fn each_job_needs_and_makes_its_kinds() {
    use ArtifactKind::*;
    assert_eq!(Job::Download.needs(), &[Video, Audio]);
    assert_eq!(Job::Download.makes(), &[Video, Audio, Metadata]);
    assert_eq!(Job::Lyrics.needs(), &[Lyrics]);
    assert_eq!(Job::Lyrics.makes(), &[Lyrics]);
    assert_eq!(Job::Stems.needs(), &[StemVocals, StemInstrumental]);
    assert_eq!(Job::Stems.makes(), &[StemVocals, StemInstrumental]);
    assert_eq!(Job::Download.as_str(), "download");
    assert_eq!(Job::Lyrics.as_str(), "lyrics");
    assert_eq!(Job::Stems.as_str(), "stems");
}

#[test]
fn a_node_takes_the_current_format_of_each_kind() {
    use ArtifactKind::*;
    assert_eq!((MEDIA_VERSION, STEMS_VERSION), (1, 1));
    for k in [Video, Audio] {
        assert!(acceptable(k, MEDIA_VERSION));
        assert!(!acceptable(k, MEDIA_VERSION + 1));
        assert!(!acceptable(k, MEDIA_VERSION - 1));
    }
    for k in [StemVocals, StemInstrumental] {
        assert!(acceptable(k, STEMS_VERSION));
        assert!(!acceptable(k, STEMS_VERSION + 1));
        assert!(!acceptable(k, STEMS_VERSION - 1));
    }
    assert!(acceptable(Lyrics, LYRICS_PIPELINE_VERSION));
    assert!(
        !acceptable(Lyrics, LYRICS_PIPELINE_VERSION - 1),
        "an older pipeline"
    );
    assert!(
        !acceptable(Lyrics, LYRICS_PIPELINE_VERSION + 1),
        "a newer pipeline (a dev peer)"
    );
    assert!(!acceptable(Metadata, METADATA_PARSER));
    assert!(acceptable(Metadata, METADATA_PROVIDER));
    assert!(acceptable(Metadata, METADATA_MANUAL));
    assert!(!acceptable(Unknown, 1));
}

/// The version names who named the title: an operator, a provider (the
/// chain's `gemini` label, the Claude provider uses it too), or a parser.
#[test]
fn metadata_version_ranks_parser_provider_operator() {
    assert_eq!(
        (METADATA_PARSER, METADATA_PROVIDER, METADATA_MANUAL),
        (0, 1, 2)
    );
    assert_eq!(metadata_version(Some("manual"), false), METADATA_MANUAL);
    assert_eq!(metadata_version(Some("manual"), true), METADATA_MANUAL);
    assert_eq!(metadata_version(Some("gemini"), false), METADATA_PROVIDER);
    assert_eq!(metadata_version(Some("gemini"), true), METADATA_PARSER);
    assert_eq!(metadata_version(Some("regex"), true), METADATA_PARSER);
    assert_eq!(metadata_version(None, false), METADATA_PARSER);
    assert_eq!(metadata_version(None, true), METADATA_PARSER);
    assert_eq!(
        metadata_version(Some("claude"), false),
        METADATA_PARSER,
        "a label this node does not know is not taken as a provider's"
    );
}

/// With no provider configured, the title parser writes `regex` with
/// `gemini_failed = 0` (`metadata::fallback_from_title`): still a parser's
/// guess, never advertised as a provider's title.
#[test]
fn a_parser_title_with_no_provider_configured_is_a_parser_title() {
    assert_eq!(metadata_version(Some("regex"), false), METADATA_PARSER);
    assert!(!acceptable(
        ArtifactKind::Metadata,
        metadata_version(Some("regex"), false)
    ));
}
