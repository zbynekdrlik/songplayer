//! Rust production wrapper around the `lyrics-alignment-mtl` eval script
//! (`eval/lyrics/aligners/lyrics_alignment_mtl/run.py`), promoted for the
//! Lever-2 forced-alignment reference regime (#143, design on #130
//! 2026-09-12). Mirrors `aligner.rs::preprocess_vocals`'s subprocess
//! pattern: `PYTHONUTF8=1` (#137), BELOW_NORMAL priority + no console
//! window on Windows, kill-on-drop, a bounded timeout, and a captured
//! stderr tail on failure.
//!
//! `run.py`'s own `align_fixture()` already retries a clean
//! `torch.cuda.OutOfMemoryError` on CPU *inside the same process*. The
//! `--no-cuda` retry here is the OUTER safety net for a CUDA failure that
//! crashes the whole interpreter before that in-process catch ever runs
//! (a corrupted CUDA context, an OOM during model load) — detected from the
//! captured stderr tail.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{debug, warn};

const TIMEOUT_SECS: u64 = 15 * 60;
const STDERR_TAIL_BYTES: usize = 2048;

/// Where the three mtl-alignment artifacts live under the shared tools dir
/// — the same dir `LyricsWorker::ensure_script` writes `lyrics_worker.py` /
/// `measure_lyrics_quality.py` into.
#[derive(Debug, Clone)]
pub struct MtlConfig {
    pub python: PathBuf,
    pub run_py: PathBuf,
    pub repo_dir: PathBuf,
}

impl MtlConfig {
    pub fn from_tools_dir(tools_dir: &Path) -> Self {
        Self {
            python: tools_dir
                .join("mtl_aligner_venv")
                .join("Scripts")
                .join("python.exe"),
            run_py: tools_dir.join("lyrics_alignment_mtl_run.py"),
            repo_dir: tools_dir.join("LyricsAlignment-MTL"),
        }
    }

    /// All three paths must exist for the reference stage to run at all.
    pub fn is_available(&self) -> bool {
        self.python.is_file() && self.run_py.is_file() && self.repo_dir.is_dir()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MtlLine {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MtlOutput {
    pub lines: Vec<MtlLine>,
    pub device: String,
    pub elapsed_s: f64,
}

// ---------------------------------------------------------------------------
// On-disk JSON shapes — must match run.py exactly.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct TextJsonLine<'a> {
    text: &'a str,
}

#[derive(Debug, Serialize)]
struct TextJsonFile<'a> {
    video_id: &'a str,
    lines: Vec<TextJsonLine<'a>>,
}

#[derive(Debug, Deserialize)]
struct OutLine {
    text: String,
    // `None` when every word in the line filtered to empty — never observed
    // across the 22-fixture eval corpus, but run.py emits it honestly
    // rather than assuming it away (see run.py::align_fixture).
    start_ms: Option<u64>,
    end_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OutMetadata {
    runtime_sec: f64,
    device: String,
}

#[derive(Debug, Deserialize)]
struct OutFile {
    lines: Vec<OutLine>,
    metadata: OutMetadata,
}

/// Build the `run.py` argv, in order. `no_cuda` appends `--no-cuda` — used
/// only on the CUDA-OOM retry.
fn build_args(
    cfg: &MtlConfig,
    wav: &Path,
    text_json: &Path,
    out_json: &Path,
    no_cuda: bool,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        cfg.run_py.clone().into_os_string(),
        "--wav".into(),
        wav.as_os_str().to_owned(),
        "--text-json".into(),
        text_json.as_os_str().to_owned(),
        "--out".into(),
        out_json.as_os_str().to_owned(),
        "--repo-dir".into(),
        cfg.repo_dir.as_os_str().to_owned(),
    ];
    if no_cuda {
        args.push("--no-cuda".into());
    }
    args
}

fn is_cuda_oom(text: &str) -> bool {
    text.contains("CUDA out of memory") || text.contains("OutOfMemoryError")
}

// Spawn wrapper: integration-tested against the real subprocess on the box,
// not unit-tested here — skip mutation so the added env plumbing does not leave
// a survivor cargo-mutants can never kill without a live GPU.
#[cfg_attr(test, mutants::skip)]
async fn run_once(
    cfg: &MtlConfig,
    wav: &Path,
    text_json: &Path,
    out_json: &Path,
    no_cuda: bool,
    gpu_mem_setting: Option<&str>,
) -> Result<()> {
    let mut cmd = Command::new(&cfg.python);
    cmd.args(build_args(cfg, wav, text_json, out_json, no_cuda));
    // Python on Windows defaults stdio to the console codepage; #137 hit
    // mangled non-ASCII lyric text without this.
    cmd.env("PYTHONUTF8", "1");
    // #154: pass the VRAM cap; run.py's gpu_polite() applies the WDDM
    // below-normal priority + the memory fraction when CUDA is used.
    for (k, v) in crate::lyrics::gpu_policy::env_for_child(gpu_mem_setting) {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW 0x08000000 | BELOW_NORMAL_PRIORITY_CLASS 0x00004000
        // — same rationale as aligner.rs::preprocess_vocals: this is a
        // shared event PC, leave CPU/GPU headroom for live playback and
        // OBS/Resolume on the same box.
        cmd.creation_flags(0x08000000 | 0x00004000);
    }
    cmd.kill_on_drop(true);

    debug!(
        "mtl_aligner: running {} {:?}",
        cfg.python.display(),
        build_args(cfg, wav, text_json, out_json, no_cuda)
    );

    let mut child = cmd
        .spawn()
        .context("failed to spawn lyrics-alignment-mtl run.py")?;
    let mut stderr_buf = Vec::new();
    if let Some(mut stderr) = child.stderr.take() {
        // Best-effort capture; a read failure just leaves an empty tail.
        let _ = stderr.read_to_end(&mut stderr_buf).await;
    }
    let status = match tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => bail!("lyrics-alignment-mtl wait failed: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            bail!("lyrics-alignment-mtl timed out after {TIMEOUT_SECS}s");
        }
    };
    let stderr_text = String::from_utf8_lossy(&stderr_buf);
    let tail_start = stderr_text.len().saturating_sub(STDERR_TAIL_BYTES);
    let tail = &stderr_text[tail_start..];
    if !status.success() {
        bail!("lyrics-alignment-mtl exited with status {status}; stderr tail: {tail}");
    }
    Ok(())
}

async fn write_text_json(path: &Path, video_id: &str, lines: &[String]) -> Result<()> {
    let file = TextJsonFile {
        video_id,
        lines: lines.iter().map(|t| TextJsonLine { text: t }).collect(),
    };
    let json = serde_json::to_vec(&file).context("failed to serialize mtl text-json")?;
    tokio::fs::write(path, json)
        .await
        .context("failed to write mtl text-json")?;
    Ok(())
}

fn parse_output_str(content: &str) -> Result<MtlOutput> {
    let parsed: OutFile = serde_json::from_str(content)
        .context("failed to parse lyrics-alignment-mtl output JSON")?;
    let lines = parsed
        .lines
        .into_iter()
        .map(|l| MtlLine {
            text: l.text,
            start_ms: l.start_ms.unwrap_or(0),
            end_ms: l.end_ms.unwrap_or(0),
        })
        .collect();
    Ok(MtlOutput {
        lines,
        device: parsed.metadata.device,
        elapsed_s: parsed.metadata.runtime_sec,
    })
}

async fn parse_output(path: &Path) -> Result<MtlOutput> {
    let content = tokio::fs::read_to_string(path)
        .await
        .context("failed to read lyrics-alignment-mtl output JSON")?;
    parse_output_str(&content)
}

/// Run `lyrics-alignment-mtl` on `vocals_wav` against `lines` (the chosen
/// text candidate's reference lines — any source, timed or not) and return
/// the mtl line timings. Writes scratch files under `work_dir`:
/// `{video_id}_mtl_text.json` (input) and `{video_id}_mtl_out.json`
/// (output) — left on disk for debugging, same convention as the vocals
/// WAV cache in `aligner.rs`.
// Orchestration wrapper around the spawn: integration-tested only (the gate /
// control flow needs a live subprocess). Skip mutation so the #154 arg
// threading does not introduce an unkillable survivor.
#[cfg_attr(test, mutants::skip)]
pub async fn align(
    cfg: &MtlConfig,
    vocals_wav: &Path,
    video_id: &str,
    lines: &[String],
    work_dir: &Path,
    gpu_mem_setting: Option<&str>,
) -> Result<MtlOutput> {
    tokio::fs::create_dir_all(work_dir)
        .await
        .context("failed to create mtl work_dir")?;
    let text_json = work_dir.join(format!("{video_id}_mtl_text.json"));
    let out_json = work_dir.join(format!("{video_id}_mtl_out.json"));
    write_text_json(&text_json, video_id, lines).await?;

    match run_once(
        cfg,
        vocals_wav,
        &text_json,
        &out_json,
        false,
        gpu_mem_setting,
    )
    .await
    {
        Ok(()) => {}
        Err(e) if is_cuda_oom(&e.to_string()) => {
            warn!(
                video_id,
                error = %e,
                "mtl_aligner: CUDA OOM — retrying with --no-cuda"
            );
            run_once(
                cfg,
                vocals_wav,
                &text_json,
                &out_json,
                true,
                gpu_mem_setting,
            )
            .await?;
        }
        Err(e) => return Err(e),
    }
    parse_output(&out_json).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MtlConfig {
        MtlConfig::from_tools_dir(Path::new("/tools"))
    }

    #[test]
    fn from_tools_dir_builds_expected_paths() {
        let c = cfg();
        assert_eq!(
            c.python,
            Path::new("/tools/mtl_aligner_venv/Scripts/python.exe")
        );
        assert_eq!(c.run_py, Path::new("/tools/lyrics_alignment_mtl_run.py"));
        assert_eq!(c.repo_dir, Path::new("/tools/LyricsAlignment-MTL"));
    }

    #[test]
    fn is_available_false_when_nothing_exists() {
        assert!(!cfg().is_available());
    }

    #[test]
    fn is_available_true_when_all_three_paths_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let venv_scripts = tmp.path().join("mtl_aligner_venv").join("Scripts");
        std::fs::create_dir_all(&venv_scripts).unwrap();
        std::fs::write(venv_scripts.join("python.exe"), b"").unwrap();
        std::fs::write(tmp.path().join("lyrics_alignment_mtl_run.py"), b"").unwrap();
        std::fs::create_dir_all(tmp.path().join("LyricsAlignment-MTL")).unwrap();
        assert!(MtlConfig::from_tools_dir(tmp.path()).is_available());
    }

    #[test]
    fn build_args_has_all_required_flags_without_no_cuda_by_default() {
        let c = cfg();
        let args = build_args(
            &c,
            Path::new("/x/v.wav"),
            Path::new("/x/t.json"),
            Path::new("/x/o.json"),
            false,
        );
        let joined: Vec<String> = args
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        // Compare as paths, not strings: `Path::join` yields backslashes on
        // the Windows runner (CI run 34695364979).
        assert_eq!(Path::new(&joined[0]), c.run_py.as_path());
        assert!(joined.contains(&"--wav".to_string()));
        assert!(joined.contains(&"/x/v.wav".to_string()));
        assert!(joined.contains(&"--text-json".to_string()));
        assert!(joined.contains(&"/x/t.json".to_string()));
        assert!(joined.contains(&"--out".to_string()));
        assert!(joined.contains(&"/x/o.json".to_string()));
        assert!(joined.contains(&"--repo-dir".to_string()));
        assert!(joined.iter().any(|s| Path::new(s) == c.repo_dir.as_path()));
        assert!(
            !joined.contains(&"--no-cuda".to_string()),
            "--no-cuda must be absent on the first (non-retry) attempt"
        );
    }

    #[test]
    fn build_args_includes_no_cuda_only_on_retry() {
        let c = cfg();
        let args = build_args(
            &c,
            Path::new("/x/v.wav"),
            Path::new("/x/t.json"),
            Path::new("/x/o.json"),
            true,
        );
        let joined: Vec<String> = args
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(joined.last().unwrap(), "--no-cuda");
    }

    #[test]
    fn is_cuda_oom_detects_both_markers() {
        assert!(is_cuda_oom(
            "RuntimeError: CUDA out of memory. Tried to allocate"
        ));
        assert!(is_cuda_oom("torch.cuda.OutOfMemoryError: ..."));
        assert!(!is_cuda_oom("RuntimeError: something else entirely"));
    }

    #[test]
    fn text_json_serializes_video_id_and_line_text_only() {
        let file = TextJsonFile {
            video_id: "abc123",
            lines: vec![
                TextJsonLine {
                    text: "hello world",
                },
                TextJsonLine { text: "line two" },
            ],
        };
        let v: serde_json::Value = serde_json::to_value(&file).unwrap();
        assert_eq!(v["video_id"], "abc123");
        assert_eq!(v["lines"][0]["text"], "hello world");
        assert_eq!(v["lines"][1]["text"], "line two");
        assert_eq!(v["lines"].as_array().unwrap().len(), 2);
    }

    /// Fixture verbatim-shaped from run.py::main's `payload` dict.
    #[test]
    fn parse_output_str_matches_run_py_shape() {
        let fixture = r#"{
            "backend_id": "lyrics-alignment-mtl",
            "backend_revision": 1,
            "wav_path": "/x/v.wav",
            "duration_ms": 123456,
            "lines": [
                {"text": "amazing grace", "start_ms": 1000, "end_ms": 2500, "text_sk": null, "words": [{"text": "amazing", "start_ms": 1000, "end_ms": 1800}, {"text": "grace", "start_ms": 1800, "end_ms": 2500}]},
                {"text": "how sweet the sound", "start_ms": 2600, "end_ms": 4000, "text_sk": null, "words": []}
            ],
            "raw_confidence": null,
            "metadata": {
                "aligner": "lyrics-alignment-mtl",
                "checkpoint": "checkpoint_MTL + checkpoint_BDR",
                "granularity": "word",
                "runtime_sec": 106.4,
                "preprocess_sec": 3.2,
                "method": "MTL_BDR",
                "device": "cuda",
                "cuda_oom_retried": false
            }
        }"#;
        let out = parse_output_str(fixture).unwrap();
        assert_eq!(out.lines.len(), 2);
        assert_eq!(out.lines[0].text, "amazing grace");
        assert_eq!(out.lines[0].start_ms, 1000);
        assert_eq!(out.lines[0].end_ms, 2500);
        assert_eq!(out.device, "cuda");
        assert_eq!(out.elapsed_s, 106.4);
    }

    #[test]
    fn parse_output_str_defaults_null_timing_to_zero() {
        let fixture = r#"{
            "lines": [{"text": "untimeable", "start_ms": null, "end_ms": null, "text_sk": null, "words": null}],
            "metadata": {"runtime_sec": 1.0, "device": "cpu", "cuda_oom_retried": true, "aligner": "x", "checkpoint": "x", "granularity": "word", "preprocess_sec": 0.1, "method": "MTL_BDR"}
        }"#;
        let out = parse_output_str(fixture).unwrap();
        assert_eq!(out.lines[0].start_ms, 0);
        assert_eq!(out.lines[0].end_ms, 0);
        assert_eq!(out.device, "cpu");
    }

    #[test]
    fn parse_output_str_rejects_garbage() {
        assert!(parse_output_str("not json").is_err());
    }
}

#[cfg(test)]
#[path = "mtl_encoding_guard_tests.rs"]
mod mtl_encoding_guard_tests;
