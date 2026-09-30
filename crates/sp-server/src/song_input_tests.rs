//! #136 tests for `song_input`: a stem / dub job runs on the input its song's
//! row records AFTER the job holds the heavy slot, not on the paths it read
//! when it was picked.

/// Byte offsets of `needles` in `src`, each searched from `async fn
/// process_next(` on (CRLF-normalised for the Windows checkout).
fn offsets_in_process_next(src: &str, needles: &[&str]) -> Vec<usize> {
    let src = src.replace("\r\n", "\n");
    let start = src
        .find("async fn process_next(")
        .expect("the worker must have process_next");
    needles
        .iter()
        .map(|needle| {
            let at = src[start..]
                .find(needle)
                .unwrap_or_else(|| panic!("process_next must contain {needle:?}"));
            start + at
        })
        .collect()
}

/// Structural: the stem worker takes the heavy slot, THEN re-reads the song's
/// input and derives the stem paths from it, THEN separates. A job picked
/// before a rename (the metadata repair) waited for the slot with a path that
/// no longer exists.
#[test]
fn the_stem_worker_reads_its_input_after_it_holds_the_heavy_slot() {
    let at = offsets_in_process_next(
        include_str!("stems/worker.rs"),
        &[
            "acquire_slot_for_spawn(",
            "song_input::job_input(",
            ".stem_job(job)",
            "stem_paths(",
            "separate_stems(",
        ],
    );
    assert!(
        at.windows(2).all(|w| w[0] < w[1]),
        "slot → re-read → the job on it → stem paths → separation, got offsets {at:?}"
    );
}

/// Structural: the dub worker takes the heavy slot, THEN re-reads the song's
/// input (the audio, and the vocals stem it may feed the session), THEN
/// synthesizes on it.
#[test]
fn the_dub_worker_reads_its_input_after_it_holds_the_heavy_slot() {
    let at = offsets_in_process_next(
        include_str!("dabing/worker.rs"),
        &[
            "acquire_slot_for_spawn(",
            "song_input::job_input(",
            ".dub_job(job)",
            "self.synthesize(",
        ],
    );
    assert!(
        at.windows(2).all(|w| w[0] < w[1]),
        "slot → re-read → the job on it → synthesis, got offsets {at:?}"
    );
}
