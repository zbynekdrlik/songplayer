//! Tests for PlaybackPipeline::spawn / ndi_name accessor.
//! Extracted from pipeline.rs to keep that file under the 1000-line cap.
//! Included via `#[path = "pipeline_spawn_tests.rs"]` so `super::*` resolves
//! to `pipeline`'s private items.

use super::*;

#[test]
fn spawn_stores_ndi_name_for_accessor() {
    // Construct a real PlaybackPipeline. On non-Windows the run_loop
    // stub just waits for commands and exits on Shutdown — no MF/NDI
    // required. This test kills all three mutants on spawn/ndi_name:
    // - Default::default() substitution → compile error or "" ndi_name
    // - "" substitution on ndi_name() → assertion fails
    // - "xyzzy" substitution on ndi_name() → assertion fails
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<(i64, PipelineEvent)>();
    let pp = PlaybackPipeline::spawn("SP-fixture-name".to_string(), None, event_tx, 42);
    assert_eq!(
        pp.ndi_name(),
        "SP-fixture-name",
        "spawn must store the ndi_name argument so ndi_health can label snapshots"
    );
}
