//! #144 (review of F3): a line mtl cannot time — upstream filters every word
//! of it away ("1 2 3 4", "24/7": digits and punctuation only) — comes back
//! from `run.py` untimed (`start_ms: null`, never dropped). It must never
//! reach the wall at the song's start: the ★ track leaves it out. Wired into
//! `mtl_aligner.rs` as a sibling `#[path]` test module (it parses `run.py`'s
//! real output shape with the private `parse_output_str`).

use super::*;
use crate::lyrics::g35t_client::AsrWord;
use crate::lyrics::orchestrator::{
    ReferenceStageBackend, ReferenceStageResult, run_reference_stage,
};
use std::path::Path;

/// mtl answers with `run.py`'s output, parsed as production parses it.
struct RunPyAnswers(std::sync::Mutex<Option<MtlOutput>>);

#[async_trait::async_trait]
impl ReferenceStageBackend for RunPyAnswers {
    async fn mtl_align(
        &self,
        _vocals_wav: &Path,
        _video_id: &str,
        _lines: &[String],
    ) -> anyhow::Result<MtlOutput> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .take()
            .expect("mtl_align called twice"))
    }
    async fn asr_transcribe(&self, _vocals_wav: &Path) -> anyhow::Result<Vec<AsrWord>> {
        unreachable!("the reference stage never transcribes (#144)")
    }
}

/// `text`'s words sung one every 300 ms from `start_ms`.
fn sung(text: &str, start_ms: u64) -> Vec<AsrWord> {
    text.split_whitespace()
        .zip(0u64..)
        .map(|(word, i)| AsrWord {
            text: word.into(),
            start_ms: start_ms + i * 300,
            end_ms: start_ms + i * 300 + 280,
        })
        .collect()
}

/// `run.py`'s output: four timed lines, on the singing, and "1 2 3 4"
/// between them, untimed (upstream keeps no digit).
const RUN_PY_OUT: &str = r#"{
    "lines": [
        {"text": "amazing grace", "start_ms": 1000, "end_ms": 1600, "text_sk": null, "words": null},
        {"text": "1 2 3 4", "start_ms": null, "end_ms": null, "text_sk": null, "words": null},
        {"text": "how sweet the sound", "start_ms": 4000, "end_ms": 5200, "text_sk": null, "words": null},
        {"text": "that saved a wretch", "start_ms": 6000, "end_ms": 7200, "text_sk": null, "words": null},
        {"text": "like me", "start_ms": 8000, "end_ms": 8600, "text_sk": null, "words": null}
    ],
    "metadata": {"runtime_sec": 1.0, "device": "cpu"}
}"#;

#[tokio::test]
async fn a_line_mtl_cannot_time_never_ships_at_the_songs_start() {
    let lines: Vec<String> = [
        "amazing grace",
        "1 2 3 4",
        "how sweet the sound",
        "that saved a wretch",
        "like me",
    ]
    .iter()
    .map(|l| l.to_string())
    .collect();
    let mut words = sung("amazing grace", 1_000);
    words.extend(sung("one two three four", 2_200));
    words.extend(sung("how sweet the sound", 4_000));
    words.extend(sung("that saved a wretch", 6_000));
    words.extend(sung("like me", 8_000));
    let backend = RunPyAnswers(std::sync::Mutex::new(Some(
        parse_output_str(RUN_PY_OUT).unwrap(),
    )));

    let result =
        run_reference_stage(&backend, Path::new("/x.wav"), "yt_untimed", &lines, &words).await;

    let ReferenceStageResult::Pass { lines: shipped, .. } = result else {
        panic!("four of five lines on time pass the gate");
    };
    let texts: Vec<&str> = shipped.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "amazing grace",
            "how sweet the sound",
            "that saved a wretch",
            "like me"
        ],
        "the untimed line is left out"
    );
    assert!(
        shipped
            .iter()
            .all(|l| l.start_ms > 0 && l.end_ms > l.start_ms),
        "{shipped:?}"
    );
}
