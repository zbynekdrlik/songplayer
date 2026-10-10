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

fn mtl_line(text: &str, timing: Option<(u64, u64)>) -> MtlLine {
    MtlLine {
        text: text.into(),
        start_ms: timing.map(|t| t.0),
        end_ms: timing.map(|t| t.1),
    }
}

/// #144: the ★ track keeps mtl's timed lines in order and says, once, how
/// many it left out; a fully timed track says nothing.
#[test]
fn the_star_track_says_how_many_untimed_lines_it_left_out() {
    use crate::lyrics::orchestrator::timed_lines;
    use crate::test_log::{Captured, capturing};

    let cap = Captured::default();
    let shipped = tracing::subscriber::with_default(capturing(&cap), || {
        timed_lines(
            vec![
                mtl_line("a", Some((100, 900))),
                mtl_line("24/7", None),
                mtl_line("b", Some((1_000, 1_800))),
            ],
            "yt_one",
        )
    });
    let got: Vec<(&str, u32, u32)> = shipped
        .iter()
        .map(|l| (l.text.as_str(), l.start_ms, l.end_ms))
        .collect();
    assert_eq!(got, [("a", 100, 900), ("b", 1_000, 1_800)]);
    let infos = cap.lines_with("could not time");
    assert_eq!(infos.len(), 1, "{infos:#?}");
    assert!(infos[0].contains("untimed=1"), "{infos:#?}");

    let quiet = Captured::default();
    let all = tracing::subscriber::with_default(capturing(&quiet), || {
        timed_lines(vec![mtl_line("a", Some((100, 900)))], "yt_all")
    });
    assert_eq!(all.len(), 1);
    assert!(quiet.lines_with("could not time").is_empty());
}

/// #144 F3: the pre-mtl Coverage verdict holds only while mtl returns every
/// line it was given with its text unchanged — a changed count or text is an
/// mtl error, never gated.
#[test]
fn mtl_must_return_the_lines_it_was_given() {
    use crate::lyrics::orchestrator::lines_changed;
    let given: Vec<String> = vec!["a b".into(), "c d".into()];
    let same = vec![mtl_line("a b", Some((1, 2))), mtl_line("c d", None)];
    assert_eq!(lines_changed(&given, &same), None);
    assert_eq!(
        lines_changed(&given, &same[..1]),
        Some("mtl returned 1 lines for the 2 it was given".into())
    );
    let edited = vec![mtl_line("a b", Some((1, 2))), mtl_line("c e", Some((3, 4)))];
    assert_eq!(
        lines_changed(&given, &edited),
        Some("mtl changed line 2 of the text".into())
    );
}
