//! #221 lane 3 (ROZHODNUTÉ 5877969167): SongPlayer broadcasts only
//! `SP-program` and `SP-program-MAX`, so a playlist's pipeline has NO NDI
//! sender of its own — it feeds the program bus and nothing else. A
//! structural guard over the pipeline side's sources (the decode loop, the
//! paced scopes, the paced output, the runtime and startup creation): none of
//! them may create, own, submit to or poll an NDI sender. The `SP-program`
//! sender (`program_output.rs`) is the only one, and it is not in this list.
//! Wired from `pipeline.rs` (`#[path]`), so `include_str!` resolves next to it.

/// The pipeline side's sources, by file name.
const PIPELINE_SIDE: [(&str, &str); 6] = [
    ("pipeline.rs", include_str!("pipeline.rs")),
    ("pipeline_paced.rs", include_str!("pipeline_paced.rs")),
    (
        "pipeline_paced_idle.rs",
        include_str!("pipeline_paced_idle.rs"),
    ),
    (
        "pipeline_paced_submit.rs",
        include_str!("pipeline_paced_submit.rs"),
    ),
    ("paced_output.rs", include_str!("paced_output.rs")),
    ("runtime_pipeline.rs", include_str!("runtime_pipeline.rs")),
];

/// What a playlist's own NDI output needed: a sender (and the submitter that
/// owns one), its creation, its sends and its receiver poll, and the NDI
/// backend handed to each pipeline thread.
const SENDER_TOKENS: [&str; 7] = [
    "NdiSender",
    "FrameSubmitter",
    "new_with_clocking",
    "send_video_async",
    "send_audio",
    "get_no_connections",
    "ndi_backend",
];

#[test]
fn a_playlist_pipeline_has_no_ndi_sender_of_its_own() {
    let mut found = Vec::new();
    for (file, src) in PIPELINE_SIDE {
        for token in SENDER_TOKENS {
            if src.contains(token) {
                found.push(format!("{file}: {token}"));
            }
        }
    }
    assert!(
        found.is_empty(),
        "the pipeline side still builds or feeds a per-playlist NDI sender: {found:#?}"
    );
}
