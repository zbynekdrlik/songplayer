//! Tests for PlaybackPipeline::spawn / the output-name accessor.
//! Extracted from pipeline.rs to keep that file small.
//! Included via `#[path = "pipeline_spawn_tests.rs"]` so `super::*` resolves
//! to `pipeline`'s private items.

use super::*;

#[test]
fn spawn_stores_the_output_name_for_the_accessor() {
    // Construct a real PlaybackPipeline. On non-Windows the run_loop
    // stub just waits for commands and exits on Shutdown — no MF required.
    // This test kills the mutants on spawn/output_name:
    // - Default::default() substitution → compile error or "" output_name
    // - "" substitution on output_name() → assertion fails
    // - "xyzzy" substitution on output_name() → assertion fails
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<(i64, PipelineEvent)>();
    let pp = PlaybackPipeline::spawn(
        "SP-fixture-name".to_string(),
        event_tx,
        42,
        crate::playback::preview::preview_stream::DecodeTaps {
            preview: crate::playback::preview::PreviewTap::new(Default::default(), "test".into()),
            stream: crate::playback::preview::preview_stream::StreamTap::new("test".into(), 0),
        },
    );
    assert_eq!(
        pp.output_name(),
        "SP-fixture-name",
        "spawn must store the output name so ndi_health can label snapshots"
    );
}
