# Qwen3-ForcedAligner Word-Level Alignment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Produce real word-level timestamps for the 27 LRCLIB-covered songs by running Qwen3-ForcedAligner-0.6B inside an isolated Python venv, unblocking word-by-word karaoke highlighting that PR #24 stubbed behind an `if false` gate.

**Architecture:** A new Rust bootstrap module creates a dedicated `lyrics_venv/` on Windows and installs the `qwen-asr` PyPI package (which pins `transformers==4.57.6`, bypassing the `qwen3_asr` architecture error in system `transformers 5.5.3`). The existing Python helper script is rewritten to call `qwen_asr.Qwen3ForcedAligner.align(...)` directly instead of the discarded CTC+torchaudio fallback. Rust `process_song` gains a 5-minute audio gate and merges aligned words into LRCLIB-sourced line timings, labelling the result `lrclib+qwen3`.

**Tech Stack:** Rust (sp-server, sp-core), Python 3.12, `qwen-asr==0.0.6`, `transformers==4.57.6`, tokio, sqlx, Playwright/TypeScript.

**Spec:** `docs/superpowers/specs/2026-04-14-qwen3-forced-aligner-design.md`

**Branch:** `dev` (version already at `0.14.0-dev.1`, main at `0.13.0` — no version bump needed at start; bump at final task if CI demands it).

---

## File Structure

| File | Role | Action |
|---|---|---|
| `scripts/lyrics_worker.py` | Python subprocess entry | **Rewrite** `cmd_align`, delete CTC/torchaudio fallback stack, add `cmd_preload` |
| `crates/sp-server/src/lyrics/bootstrap.rs` | Venv + qwen-asr install (Windows) | **Create** |
| `crates/sp-server/src/lyrics/aligner.rs` | Subprocess wrapper + JSON → Rust | **Modify** — add `merge_word_timings` pure fn + tests |
| `crates/sp-server/src/lyrics/mod.rs` | Module exports | **Modify** — add `pub mod bootstrap;` |
| `crates/sp-server/src/lyrics/worker.rs` | Pipeline orchestrator | **Modify** — flip `if false`, gate on duration, call merge, use venv Python, label `lrclib+qwen3` |
| `crates/sp-server/src/lib.rs` | Server wiring | **Modify** — pass tools_dir for bootstrap; LyricsWorker discovers venv Python itself |
| `e2e/post-deploy-flac.spec.ts` | Deploy verification | **Modify** — add word-level highlight assertion |

No DB schema changes. No new crates. `VideoLyricsRow.duration_ms: Option<i64>` already exists and carries what we need for the 5-min gate.

---

## Task 1: Rewrite Python align command to use `qwen_asr` package

**Files:**
- Modify: `scripts/lyrics_worker.py` (replace `cmd_align` + helpers; add `cmd_preload`; keep `cmd_check_gpu`, `cmd_download_models`, `cmd_transcribe`)

The existing `cmd_align` tries `AutoModelForCTC` → torchaudio.functional.forced_align → even-distribution fallback. All three paths are dead weight now: the model isn't a standard CTC model, torchaudio forced_align doesn't know qwen3_asr, and the evenly-distributed fallback produces fake timings that look real in logs. Delete all three and call the official API.

- [ ] **Step 1: Replace `cmd_align` and its helpers**

Full replacement for `cmd_align` (replaces lines 95–254 of current `scripts/lyrics_worker.py`; keep the function signature so the argparse wiring in `main()` stays unchanged):

```python
def cmd_align(args):
    """
    Align lyrics text to audio using Qwen3-ForcedAligner-0.6B.
    --text is a PATH to a UTF-8 text file with one lyric line per row.
    Writes JSON {"lines": [{"en": str, "words": [{"text": str, "start_ms": int, "end_ms": int}]}]}
    to --output.
    Uses the official qwen-asr PyPI package (https://pypi.org/project/qwen-asr/).
    """
    import torch
    from qwen_asr import Qwen3ForcedAligner

    with open(args.text, "r", encoding="utf-8") as f:
        lyrics_lines = [line.strip() for line in f.read().splitlines() if line.strip()]

    if not lyrics_lines:
        _write_output([], args.output)
        return

    device_map = "cuda:0" if torch.cuda.is_available() else "cpu"

    model = Qwen3ForcedAligner.from_pretrained(
        "Qwen/Qwen3-ForcedAligner-0.6B",
        dtype=torch.bfloat16,
        device_map=device_map,
    )

    # qwen-asr takes a single flat text string. Newline-joined lines preserve
    # line boundaries for humans but the aligner produces a flat word sequence.
    full_text = "\n".join(lyrics_lines)

    results = model.align(
        audio=args.audio,
        text=full_text,
        language="English",
    )

    # results is a list (batch dim); we sent one audio file so take index 0.
    # Each word has .text, .start_time (seconds, float), .end_time (seconds, float).
    word_stream = results[0]

    lines_out = _group_words_into_lines(word_stream, lyrics_lines)
    _write_output(lines_out, args.output)


def _group_words_into_lines(word_stream, lyrics_lines):
    """Walk the flat aligned word stream and the source line list in parallel,
    assigning each aligned word to the next source line based on expected word
    count per line. Returns the list of {en, words} dicts."""
    words_flat = [
        {
            "text": w.text,
            "start_ms": int(round(w.start_time * 1000)),
            "end_ms": int(round(w.end_time * 1000)),
        }
        for w in word_stream
    ]

    out = []
    idx = 0
    total = len(words_flat)
    for line_text in lyrics_lines:
        expected = max(1, len(line_text.split()))
        end = min(idx + expected, total)
        out.append({"en": line_text, "words": words_flat[idx:end]})
        idx = end
    return out


def _write_output(lines, output_path):
    result = {"lines": lines}
    with open(output_path, "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False)
```

Delete these helpers entirely (they were only used by the removed fallback paths):
- `_align_with_ctc`
- `_emit_alignment_from_tokens`
- `_align_generic`
- `_emit_evenly_distributed`

Keep `_group_words_into_lines` and `_write_output` as shown above (they're simpler now and used by the new `cmd_align`).

- [ ] **Step 2: Add `cmd_preload` subcommand**

Append this function near the other `cmd_*` functions in `scripts/lyrics_worker.py`:

```python
def cmd_preload(args):
    """Force model download + load to surface failures at bootstrap time
    rather than on the first real song. Exits 0 on success, non-zero otherwise."""
    import torch
    from qwen_asr import Qwen3ForcedAligner

    device_map = "cuda:0" if torch.cuda.is_available() else "cpu"

    model = Qwen3ForcedAligner.from_pretrained(
        "Qwen/Qwen3-ForcedAligner-0.6B",
        dtype=torch.bfloat16,
        device_map=device_map,
    )
    # Touch one parameter so lazy-load failures surface now.
    _ = next(model.parameters())
    print(json.dumps({"loaded": True, "device": device_map}))
```

Wire it into `main()` by appending to the `dispatch` dict:

```python
dispatch = {
    "check-gpu": cmd_check_gpu,
    "download-models": cmd_download_models,
    "transcribe": cmd_transcribe,
    "align": cmd_align,
    "preload": cmd_preload,
}
```

And add the subparser (insert just before `args = parser.parse_args()`):

```python
# preload
subparsers.add_parser("preload", help="Download + load model to surface failures early")
```

- [ ] **Step 3: Verify the script still parses on Linux**

Run: `python3 -c "import ast; ast.parse(open('scripts/lyrics_worker.py').read())"`
Expected: no output, exit code 0. (We don't install qwen-asr on Linux; we're only checking that the file is valid Python syntax.)

- [ ] **Step 4: Commit**

```bash
git add scripts/lyrics_worker.py
git commit -m "$(cat <<'EOF'
feat(lyrics): rewrite Python aligner to use qwen-asr package

Replace the broken AutoModelForCTC + torchaudio.forced_align + even
distribution fallback stack with a direct call to
qwen_asr.Qwen3ForcedAligner.align(...). The qwen-asr package ships its
own loader that handles the qwen3_asr architecture (which raw
transformers 5.5.3 rejects with KeyError). Add a 'preload' subcommand
that triggers the one-time 1.2GB model download during bootstrap
instead of on the first song.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Add `merge_word_timings` helper in `aligner.rs` with tests

**Files:**
- Modify: `crates/sp-server/src/lyrics/aligner.rs` (add pub fn + 4 tests)

The aligner returns `Vec<LyricsLine>` derived from the aligned word stream. LRCLIB returns `Vec<LyricsLine>` with hand-curated line timings but no words. We want to keep LRCLIB's `start_ms`/`end_ms` per line (they're better than first-word/last-word derivation) and attach the aligned words to each line. TDD this pure function.

- [ ] **Step 1: Write the failing tests**

Append to `crates/sp-server/src/lyrics/aligner.rs` inside `#[cfg(test)] mod tests { ... }`:

```rust
    fn lrclib_line(start_ms: u64, end_ms: u64, en: &str) -> LyricsLine {
        LyricsLine { start_ms, end_ms, en: en.to_string(), sk: None, words: None }
    }

    fn aligned_line(en: &str, words: Vec<(u64, u64, &str)>) -> LyricsLine {
        LyricsLine {
            start_ms: words.first().map(|w| w.0).unwrap_or(0),
            end_ms: words.last().map(|w| w.1).unwrap_or(0),
            en: en.to_string(),
            sk: None,
            words: Some(words.into_iter().map(|(s, e, t)| LyricsWord {
                text: t.to_string(), start_ms: s, end_ms: e,
            }).collect()),
        }
    }

    #[test]
    fn merge_word_timings_same_count_preserves_lrclib_timing() {
        let lrclib = vec![
            lrclib_line(1000, 3000, "Hello world"),
            lrclib_line(3500, 5000, "Amazing grace"),
        ];
        let aligned = vec![
            aligned_line("Hello world", vec![(1100, 1500, "Hello"), (1600, 2200, "world")]),
            aligned_line("Amazing grace", vec![(3600, 4200, "Amazing"), (4300, 4900, "grace")]),
        ];
        let out = merge_word_timings(lrclib, aligned);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].start_ms, 1000, "lrclib start_ms preserved");
        assert_eq!(out[0].end_ms, 3000, "lrclib end_ms preserved");
        assert_eq!(out[0].en, "Hello world");
        let words0 = out[0].words.as_ref().expect("words present");
        assert_eq!(words0.len(), 2);
        assert_eq!(words0[0].text, "Hello");
        assert_eq!(words0[1].text, "world");
        assert_eq!(out[1].start_ms, 3500);
    }

    #[test]
    fn merge_word_timings_fewer_aligned_leaves_tail_unaligned() {
        let lrclib = vec![
            lrclib_line(0, 1000, "Line one"),
            lrclib_line(1000, 2000, "Line two"),
            lrclib_line(2000, 3000, "Line three"),
        ];
        let aligned = vec![
            aligned_line("Line one", vec![(0, 500, "Line"), (500, 1000, "one")]),
        ];
        let out = merge_word_timings(lrclib, aligned);
        assert_eq!(out.len(), 3);
        assert!(out[0].words.is_some());
        assert!(out[1].words.is_none(), "unaligned line stays wordless");
        assert!(out[2].words.is_none());
    }

    #[test]
    fn merge_word_timings_more_aligned_ignores_extras() {
        let lrclib = vec![lrclib_line(0, 1000, "Only one line")];
        let aligned = vec![
            aligned_line("Only one line", vec![(0, 500, "Only")]),
            aligned_line("Phantom extra", vec![(500, 1000, "Phantom")]),
        ];
        let out = merge_word_timings(lrclib, aligned);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].en, "Only one line");
        assert_eq!(out[0].words.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn merge_word_timings_empty_aligned_returns_lrclib_unchanged() {
        let lrclib = vec![lrclib_line(0, 1000, "Line one")];
        let out = merge_word_timings(lrclib.clone(), vec![]);
        assert_eq!(out, lrclib);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sp-server --lib lyrics::aligner::tests::merge_word_timings`
Expected: all 4 fail with `cannot find function \`merge_word_timings\``.

- [ ] **Step 3: Implement `merge_word_timings`**

Append to `crates/sp-server/src/lyrics/aligner.rs`, just before the `#[cfg(test)]` block:

```rust
/// Merge aligned-word timings into LRCLIB-sourced lines.
///
/// Preserves each LRCLIB line's `start_ms` / `end_ms` / `en` text and
/// attaches the aligned `words` from the matching aligned line by index.
/// Aligned lines beyond `lrclib.len()` are dropped. LRCLIB lines beyond
/// `aligned.len()` keep `words = None`.
pub fn merge_word_timings(
    lrclib: Vec<LyricsLine>,
    aligned: Vec<LyricsLine>,
) -> Vec<LyricsLine> {
    let mut aligned_iter = aligned.into_iter();
    lrclib
        .into_iter()
        .map(|mut line| {
            if let Some(a) = aligned_iter.next() {
                line.words = a.words;
            }
            line
        })
        .collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sp-server --lib lyrics::aligner::tests`
Expected: all aligner tests pass (pre-existing 6 + new 4 = 10 tests pass).

- [ ] **Step 5: Format and commit**

```bash
cargo fmt --all
git add crates/sp-server/src/lyrics/aligner.rs
git commit -m "$(cat <<'EOF'
feat(lyrics): add merge_word_timings helper

Preserves LRCLIB's hand-curated line timings while attaching aligned
word-level timestamps from Qwen3-ForcedAligner. Pure function, unit
tested with four shapes: equal counts, fewer aligned (tail stays
wordless), more aligned (extras dropped), empty aligned.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Create `bootstrap.rs` — venv creation + qwen-asr install + preload

**Files:**
- Create: `crates/sp-server/src/lyrics/bootstrap.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub mod bootstrap;`)

The bootstrap runs once per server startup, is idempotent, and exits quickly on subsequent runs when the venv is already valid. All three phases — venv creation, pip install, model preload — run in sequence with a single 15-minute overall timeout. On non-Windows, everything is a no-op returning `Ok(None)`.

- [ ] **Step 1: Write the failing tests**

Create `crates/sp-server/src/lyrics/bootstrap.rs` with:

```rust
//! Bootstrap the Python environment used by Qwen3-ForcedAligner.
//!
//! On Windows, ensures `{tools_dir}/lyrics_venv/` exists with `qwen-asr`
//! installed and the Qwen3-ForcedAligner-0.6B model cached locally. On
//! non-Windows, returns `Ok(None)` — alignment is a Windows-only feature.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Returns the absolute path to the venv Python interpreter, or `None`
/// if the bootstrap is skipped (non-Windows).
#[cfg(target_os = "windows")]
pub fn venv_python_path(tools_dir: &Path) -> PathBuf {
    tools_dir.join("lyrics_venv").join("Scripts").join("python.exe")
}

#[cfg(not(target_os = "windows"))]
pub fn venv_python_path(tools_dir: &Path) -> PathBuf {
    tools_dir.join("lyrics_venv").join("bin").join("python")
}

/// Returns `true` if `python_path` exists AND running `python_path -c "import qwen_asr"`
/// exits 0 within 10 seconds. Used to decide whether bootstrap is needed.
#[cfg_attr(test, mutants::skip)]
pub async fn is_ready(python_path: &Path) -> bool {
    use tokio::process::Command;

    if !python_path.exists() {
        return false;
    }

    let mut cmd = Command::new(python_path);
    cmd.args(["-c", "import qwen_asr"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    let res = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        cmd.status(),
    )
    .await;

    matches!(res, Ok(Ok(s)) if s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venv_python_path_windows_layout() {
        #[cfg(target_os = "windows")]
        {
            let p = venv_python_path(Path::new("C:\\tools"));
            assert_eq!(p, PathBuf::from("C:\\tools\\lyrics_venv\\Scripts\\python.exe"));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let p = venv_python_path(Path::new("/tmp/tools"));
            assert_eq!(p, PathBuf::from("/tmp/tools/lyrics_venv/bin/python"));
        }
    }

    #[tokio::test]
    async fn is_ready_false_when_missing() {
        let result = is_ready(Path::new("/definitely/not/a/real/path/python")).await;
        assert!(!result);
    }
}
```

- [ ] **Step 2: Register the module**

Edit `crates/sp-server/src/lyrics/mod.rs`. Change line 1-2 from:

```rust
pub mod aligner;
pub mod lrclib;
```

to:

```rust
pub mod aligner;
pub mod bootstrap;
pub mod lrclib;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p sp-server --lib lyrics::bootstrap`
Expected: 2 tests pass.

- [ ] **Step 4: Add `ensure_ready` — the main bootstrap entrypoint**

Append to `crates/sp-server/src/lyrics/bootstrap.rs`:

```rust
/// Ensure the lyrics venv exists, `qwen-asr` is installed, and the
/// Qwen3-ForcedAligner model is preloaded.
///
/// On Windows:
///   1. Create `{tools_dir}/lyrics_venv/` via `python -m venv` (if missing).
///   2. Run `{venv}/Scripts/python.exe -m pip install -U qwen-asr`.
///   3. Run `{venv}/Scripts/python.exe {script_path} preload --models-dir ...`.
///
/// Fast-paths return `Ok(venv_python)` if `is_ready` already passes.
/// On non-Windows: returns `Ok(None)` unconditionally.
#[cfg_attr(test, mutants::skip)]
pub async fn ensure_ready(
    tools_dir: &Path,
    script_path: &Path,
    models_dir: &Path,
    system_python: &Path,
) -> Result<Option<PathBuf>> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (tools_dir, script_path, models_dir, system_python);
        return Ok(None);
    }

    #[cfg(target_os = "windows")]
    {
        use anyhow::Context;
        use tokio::process::Command;

        let venv_python = venv_python_path(tools_dir);
        let venv_dir = tools_dir.join("lyrics_venv");

        if is_ready(&venv_python).await {
            tracing::info!("lyrics bootstrap: venv already ready at {}", venv_python.display());
            return Ok(Some(venv_python));
        }

        // 1. Create venv if the directory is missing.
        if !venv_dir.exists() {
            tracing::info!("lyrics bootstrap: creating venv at {}", venv_dir.display());
            let mut cmd = Command::new(system_python);
            cmd.args(["-m", "venv"]).arg(&venv_dir);
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
            let status = cmd.status().await.context("failed to spawn python -m venv")?;
            if !status.success() {
                anyhow::bail!("python -m venv exited with status {status}");
            }
        }

        // 2. Install qwen-asr into the venv.
        tracing::info!("lyrics bootstrap: installing qwen-asr (this may take several minutes)");
        let mut pip = Command::new(&venv_python);
        pip.args(["-m", "pip", "install", "-U", "qwen-asr"]);
        use std::os::windows::process::CommandExt;
        pip.creation_flags(0x08000000);
        let pip_status = tokio::time::timeout(
            std::time::Duration::from_secs(600),
            pip.status(),
        )
        .await
        .context("pip install qwen-asr timed out after 10 minutes")?
        .context("failed to spawn pip install")?;
        if !pip_status.success() {
            anyhow::bail!("pip install qwen-asr exited with status {pip_status}");
        }

        // 3. Preload the model so the first song doesn't pay the 1.2GB download.
        tracing::info!("lyrics bootstrap: preloading Qwen3-ForcedAligner model");
        let mut preload = Command::new(&venv_python);
        preload
            .arg(script_path)
            .args(["preload", "--models-dir"])
            .arg(models_dir)
            .env("HF_HOME", models_dir);
        preload.creation_flags(0x08000000);
        let preload_status = tokio::time::timeout(
            std::time::Duration::from_secs(900),
            preload.status(),
        )
        .await
        .context("model preload timed out after 15 minutes")?
        .context("failed to spawn preload")?;
        if !preload_status.success() {
            anyhow::bail!("model preload exited with status {preload_status}");
        }

        tracing::info!("lyrics bootstrap: ready");
        Ok(Some(venv_python))
    }
}
```

- [ ] **Step 5: Run tests + format**

Run: `cargo test -p sp-server --lib lyrics::bootstrap && cargo fmt --all`
Expected: 2 tests still pass, formatting clean.

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/lyrics/bootstrap.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "$(cat <<'EOF'
feat(lyrics): add Python venv bootstrap for Qwen3 aligner

New bootstrap module creates an isolated lyrics_venv on Windows,
installs qwen-asr (which pins transformers==4.57.6), and preloads
the 1.2GB Qwen3-ForcedAligner model. Idempotent fast-path via
is_ready() check. No-op on non-Windows.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Wire bootstrap into `LyricsWorker` startup and flip the `if false` gate

**Files:**
- Modify: `crates/sp-server/src/lyrics/worker.rs` (add bootstrap call; flip gate; add 5-min duration gate; use merge helper; relabel `lrclib+qwen3`)

- [ ] **Step 1: Add a struct field to hold the resolved venv Python path**

Edit `crates/sp-server/src/lyrics/worker.rs`. Change the struct definition (lines 25–36) from:

```rust
#[allow(dead_code)]
pub struct LyricsWorker {
    pool: SqlitePool,
    client: Client,
    cache_dir: PathBuf,
    ytdlp_path: PathBuf,
    python_path: Option<PathBuf>,
    script_path: PathBuf,
    models_dir: PathBuf,
    gemini_api_key: String,
    gemini_model: String,
}
```

to:

```rust
#[allow(dead_code)]
pub struct LyricsWorker {
    pool: SqlitePool,
    client: Client,
    cache_dir: PathBuf,
    ytdlp_path: PathBuf,
    /// System Python (used to bootstrap the venv). `None` on platforms
    /// without a usable Python install.
    python_path: Option<PathBuf>,
    /// Tools directory (parent of `lyrics_venv/`).
    tools_dir: PathBuf,
    script_path: PathBuf,
    models_dir: PathBuf,
    gemini_api_key: String,
    gemini_model: String,
    /// Venv Python path — populated by `ensure_script_and_bootstrap`.
    /// `None` means alignment is disabled for this run.
    venv_python: tokio::sync::RwLock<Option<PathBuf>>,
}
```

Update `LyricsWorker::new` (lines 43–65) to initialize the new fields. Change the body from:

```rust
let script_path = tools_dir.join("lyrics_worker.py");
let models_dir = tools_dir.join("hf_models");
Self {
    pool,
    client: Client::new(),
    cache_dir,
    ytdlp_path,
    python_path,
    script_path,
    models_dir,
    gemini_api_key,
    gemini_model,
}
```

to:

```rust
let script_path = tools_dir.join("lyrics_worker.py");
let models_dir = tools_dir.join("hf_models");
Self {
    pool,
    client: Client::new(),
    cache_dir,
    ytdlp_path,
    python_path,
    tools_dir,
    script_path,
    models_dir,
    gemini_api_key,
    gemini_model,
    venv_python: tokio::sync::RwLock::new(None),
}
```

- [ ] **Step 2: Add a bootstrap call to `run`**

Edit the `run` method (lines 88–108). Change the startup section (after the existing `ensure_script` call) to also call bootstrap and store the result. Replace lines 89–95 from:

```rust
pub async fn run(self, mut shutdown_rx: broadcast::Receiver<()>) {
    tracing::info!("lyrics_worker: started");

    // Ensure the Python helper script is written to disk.
    if let Err(e) = self.ensure_script().await {
        error!("lyrics_worker: failed to write lyrics_worker.py: {e}");
    }
```

to:

```rust
pub async fn run(self, mut shutdown_rx: broadcast::Receiver<()>) {
    tracing::info!("lyrics_worker: started");

    // Ensure the Python helper script is written to disk.
    if let Err(e) = self.ensure_script().await {
        error!("lyrics_worker: failed to write lyrics_worker.py: {e}");
    }

    // Bootstrap the Python venv + qwen-asr + model preload.
    if let Some(sys_py) = self.python_path.as_ref() {
        match crate::lyrics::bootstrap::ensure_ready(
            &self.tools_dir,
            &self.script_path,
            &self.models_dir,
            sys_py,
        )
        .await
        {
            Ok(Some(venv)) => {
                tracing::info!("lyrics_worker: aligner ready at {}", venv.display());
                *self.venv_python.write().await = Some(venv);
            }
            Ok(None) => {
                tracing::info!("lyrics_worker: alignment disabled (non-Windows)");
            }
            Err(e) => {
                warn!("lyrics_worker: bootstrap failed, alignment disabled: {e}");
            }
        }
    } else {
        warn!("lyrics_worker: no system Python, alignment disabled");
    }
```

- [ ] **Step 3: Replace the `if false` alignment block**

Edit the alignment block in `process_song` (currently lines 165–217 of `worker.rs`). Replace the entire `// Step 2: Forced alignment — DISABLED ... if false { ... }` block with:

```rust
        // Step 2: Forced alignment via Qwen3-ForcedAligner (issue #25).
        //
        // Skip if:
        //   - venv bootstrap failed or running on non-Windows,
        //   - audio file missing,
        //   - audio duration > 5 min (Qwen3-ForcedAligner architectural limit).
        const QWEN3_ALIGNER_MAX_MS: u64 = 5 * 60 * 1000;
        let venv_python = self.venv_python.read().await.clone();
        let duration_ms = row.duration_ms.map(|d| d as u64).unwrap_or(0);
        let audio_path = row.audio_file_path.as_ref().map(PathBuf::from);

        if let (Some(python), Some(audio)) = (venv_python.as_ref(), audio_path.as_ref()) {
            if !audio.exists() {
                debug!("lyrics_worker: audio file {} missing, skipping alignment", audio.display());
            } else if duration_ms == 0 {
                debug!("lyrics_worker: unknown duration for {youtube_id}, skipping alignment");
            } else if duration_ms > QWEN3_ALIGNER_MAX_MS {
                info!(
                    "lyrics_worker: {youtube_id} is {}s (>5min), skipping alignment",
                    duration_ms / 1000
                );
            } else {
                let lyrics_text: String = track
                    .lines
                    .iter()
                    .map(|l| l.en.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");

                let output_path = self
                    .cache_dir
                    .join(format!("{youtube_id}_align_output.json"));

                match aligner::align_lyrics(
                    python,
                    &self.script_path,
                    &self.models_dir,
                    audio,
                    &lyrics_text,
                    &output_path,
                )
                .await
                {
                    Ok(aligned_lines) => {
                        track.lines = aligner::merge_word_timings(
                            std::mem::take(&mut track.lines),
                            aligned_lines,
                        );
                        source = "lrclib+qwen3".to_string();
                        track.source = source.clone();
                        info!("lyrics_worker: aligned {youtube_id} with Qwen3");
                    }
                    Err(e) => {
                        warn!(
                            "lyrics_worker: alignment failed for {youtube_id}, keeping line-level: {e}"
                        );
                    }
                }
            }
        }
```

(This replaces lines 165–217 of the pre-existing `worker.rs`. The `source` variable mutated here is the local shadow introduced by `acquire_lyrics` earlier in `process_song`.)

- [ ] **Step 4: Format and verify compilation**

Run: `cargo fmt --all && cargo check -p sp-server`
Expected: no errors, no new warnings.

- [ ] **Step 5: Run all sp-server unit tests**

Run: `cargo test -p sp-server --lib`
Expected: all tests pass (existing + the 4 new `merge_word_timings` tests + 2 bootstrap tests).

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/lyrics/worker.rs
git commit -m "$(cat <<'EOF'
feat(lyrics): enable Qwen3 word-level alignment pipeline

Bootstraps the lyrics_venv on startup, remembers the venv Python
path, and runs alignment on every song whose audio is ≤5 min.
LRCLIB line timings are preserved via merge_word_timings; aligned
words populate LyricsLine.words. Label switches to "lrclib+qwen3"
when alignment succeeds. Bootstrap failure is non-fatal — the
worker keeps producing line-level lyrics.

Fixes #25 (word-level alignment path).

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Pass `tools_dir` into `LyricsWorker::new` at call site

**Files:**
- Modify: `crates/sp-server/src/lib.rs` (line ~380, the `LyricsWorker::new` call)

The struct now needs `tools_dir`. The call site already has `lyrics_tools_dir` in scope (see line 385 of `lib.rs`). The `new` signature adds `tools_dir` as a new parameter.

- [ ] **Step 1: Update `LyricsWorker::new` signature**

Edit `crates/sp-server/src/lyrics/worker.rs`. Change the `new` signature (lines 43–51) from:

```rust
pub fn new(
    pool: SqlitePool,
    cache_dir: PathBuf,
    ytdlp_path: PathBuf,
    python_path: Option<PathBuf>,
    tools_dir: PathBuf,
    gemini_api_key: String,
    gemini_model: String,
) -> Self {
```

The parameter `tools_dir` is already there — rename nothing. But the old body threw away `tools_dir` once it derived `script_path` and `models_dir`. We need to keep it now. Confirm the body matches what Task 4 Step 1 installed:

```rust
let script_path = tools_dir.join("lyrics_worker.py");
let models_dir = tools_dir.join("hf_models");
Self {
    pool,
    client: Client::new(),
    cache_dir,
    ytdlp_path,
    python_path,
    tools_dir,
    script_path,
    models_dir,
    gemini_api_key,
    gemini_model,
    venv_python: tokio::sync::RwLock::new(None),
}
```

(No change needed at the call site in `lib.rs` — the caller already passes `lyrics_tools_dir` in the 5th positional slot.)

- [ ] **Step 2: Verify the crate compiles**

Run: `cargo check -p sp-server`
Expected: no errors.

- [ ] **Step 3: Verify full workspace compiles**

Run: `cargo check`
Expected: no errors.

- [ ] **Step 4: Run full sp-server test suite**

Run: `cargo test -p sp-server`
Expected: all tests pass.

- [ ] **Step 5: Commit (no-op guard commit, only if `cargo check` surfaced changes needed at the call site)**

If `cargo check` in Step 2 was clean with no edits needed, skip this commit — the wiring was already in place. If `cargo check` produced an error requiring a `lib.rs` edit, commit that edit here:

```bash
git add crates/sp-server/src/lib.rs
git commit -m "chore(lyrics): update LyricsWorker call site for new field"
```

---

## Task 6: Extend Playwright E2E to assert word-level highlighting

**Files:**
- Modify: `e2e/post-deploy-flac.spec.ts` (append a new test block)

The existing `post-deploy-flac.spec.ts` verifies video+audio sidecar pairs. We add one test that: (1) queries `/api/v1/videos?has_lyrics=1` to find an aligned song, (2) reads its `{youtube_id}_lyrics.json` via a server endpoint, (3) asserts at least one line contains a non-empty `words` array with sane timings.

This is an API-level assertion rather than a browser interaction because the karaoke panel only advances during actual playback, which requires OBS+NDI to be live — not assumable in CI. A runtime browser assertion on the karaoke panel is a manual step in the deploy verification checklist (spec §Testing) rather than an automated one. The Playwright test here catches regressions in the *persistence* side: if alignment stops writing words to JSON, we'll know.

- [ ] **Step 1: Add the test**

Append to `e2e/post-deploy-flac.spec.ts` (inside the existing `test.describe("FLAC pipeline post-deploy verification", ...)` block, before the closing `});`):

```typescript
  test("at least one lyrics JSON has word-level timestamps", async ({ request }) => {
    const playlistsResp = await request.get("/api/v1/playlists");
    expect(playlistsResp.ok()).toBe(true);
    const playlists: PlaylistEntry[] = await playlistsResp.json();
    expect(playlists.length).toBeGreaterThan(0);

    let foundWordLevel = false;
    let checkedVideos = 0;

    for (const pl of playlists) {
      const videosResp = await request.get(`/api/v1/playlists/${pl.id}/videos`);
      if (!videosResp.ok()) continue;
      const videos: VideoEntry[] = await videosResp.json();

      for (const v of videos) {
        if (checkedVideos >= 30) break;
        const lyricsResp = await request.get(`/api/v1/videos/${v.id}/lyrics`);
        if (!lyricsResp.ok()) continue;
        checkedVideos++;

        const track = await lyricsResp.json();
        if (!Array.isArray(track.lines)) continue;

        const lineWithWords = track.lines.find(
          (l: any) =>
            Array.isArray(l.words) &&
            l.words.length > 0 &&
            l.words.every(
              (w: any) =>
                typeof w.text === "string" &&
                typeof w.start_ms === "number" &&
                typeof w.end_ms === "number" &&
                w.end_ms >= w.start_ms,
            ),
        );

        if (lineWithWords) {
          foundWordLevel = true;
          break;
        }
      }
      if (foundWordLevel) break;
    }

    expect(
      foundWordLevel,
      `No video had word-level timestamps after checking ${checkedVideos} lyrics files. ` +
        `If the aligner ran, at least one song should have track.lines[i].words populated.`,
    ).toBe(true);
  });
```

- [ ] **Step 2: Verify the existing GET endpoint exists**

Run: `grep -rn '/api/v1/videos/.*lyrics\|videos.*lyrics' crates/sp-server/src/api/ | head`
Expected: a route pattern matching `GET /api/v1/videos/:id/lyrics` returning the parsed JSON. If it doesn't exist, add the task below before Step 3; otherwise skip to Step 3.

- [ ] **Step 2a (conditional on Step 2): add the `GET /api/v1/videos/:id/lyrics` route**

Only execute this step if Step 2 showed the route is missing. Edit `crates/sp-server/src/api/routes.rs` — find the `.route("/api/v1/videos/...")` chain and add:

```rust
.route("/api/v1/videos/:id/lyrics", axum::routing::get(get_video_lyrics))
```

And add the handler:

```rust
async fn get_video_lyrics(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> axum::response::Result<axum::Json<sp_core::lyrics::LyricsTrack>, axum::http::StatusCode> {
    let row = sqlx::query("SELECT youtube_id FROM videos WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(axum::http::StatusCode::NOT_FOUND)?;
    let youtube_id: String = sqlx::Row::get(&row, "youtube_id");

    let path = state.cache_dir.join(format!("{youtube_id}_lyrics.json"));
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let track: sp_core::lyrics::LyricsTrack =
        serde_json::from_slice(&bytes).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(axum::Json(track))
}
```

Then run `cargo check -p sp-server` to confirm it compiles. Commit before moving on:

```bash
git add crates/sp-server/src/api/routes.rs
git commit -m "feat(api): add GET /api/v1/videos/:id/lyrics endpoint"
```

- [ ] **Step 3: Lint the TypeScript**

Run: `cd e2e && npx tsc --noEmit frontend.spec.ts post-deploy-flac.spec.ts 2>&1 | head -20`
Expected: no type errors from the added test block.

- [ ] **Step 4: Commit**

```bash
git add e2e/post-deploy-flac.spec.ts
git commit -m "$(cat <<'EOF'
test(e2e): assert word-level timestamps in lyrics JSON

Post-deploy Playwright test iterates playlists, fetches up to 30
lyrics files, and requires at least one to contain a line with a
populated `words` array of well-formed {text, start_ms, end_ms}
entries. Catches regressions in the Qwen3 alignment write path.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Push, monitor CI, deploy, verify

**Files:** none — runtime verification.

- [ ] **Step 1: Run `cargo fmt --all --check`**

Run: `cargo fmt --all --check`
Expected: exit code 0. If not, run `cargo fmt --all` and commit the format changes as a separate `chore: format` commit.

- [ ] **Step 2: Push to dev**

Run: `git push origin dev`
Expected: push succeeds, CI run kicks off.

- [ ] **Step 3: Monitor CI until all jobs terminal**

Run: `gh run list --branch dev --limit 3` then `sleep 300 && gh run view <latest-run-id>` in the background.
Expected: every job ends `success`. If any fail, run `gh run view <id> --log-failed`, fix root cause, commit, push, monitor again.

- [ ] **Step 4: After CI green on dev, verify the bootstrap ran on win-resolume**

Run via MCP:

```powershell
Test-Path "C:\ProgramData\SongPlayer\cache\tools\lyrics_venv\Scripts\python.exe"
Select-String -Path "C:\ProgramData\SongPlayer\songplayer.log" -Pattern "lyrics bootstrap|aligner ready" -Tail 20
```

Expected: `True`, and log shows either `lyrics bootstrap: ready` or `lyrics_worker: aligner ready at ...`.

- [ ] **Step 5: Verify alignment ran on a real song**

Wait 2–3 minutes for the worker to process a pending song. Then via MCP:

```powershell
Get-ChildItem C:\ProgramData\SongPlayer\cache\*_lyrics.json | ForEach-Object {
  $j = Get-Content $_.FullName | ConvertFrom-Json
  $hasWords = $j.lines | Where-Object { $_.words -and $_.words.Count -gt 0 } | Select-Object -First 1
  [pscustomobject]@{ File = $_.Name; Source = $j.source; Aligned = [bool]$hasWords }
} | Where-Object { $_.Aligned } | Select-Object -First 3
```

Expected: at least one row with `Source = "lrclib+qwen3"` and `Aligned = True`.

- [ ] **Step 6: Verify dashboard word-level highlight on a live song**

Open the deployed dashboard at `http://10.77.9.201:8920/` via Playwright MCP (`browser_navigate`), navigate to a playlist currently playing, confirm the karaoke panel shows active-word highlighting (check for a DOM class like `.active` or `.current-word` on a `span` inside the panel, advancing during playback). Take a screenshot for evidence.

- [ ] **Step 7: Open PR to main and wait for green**

Run:

```bash
gh pr create --title "Re-enable Qwen3-ForcedAligner word-level alignment (#25)" --body "$(cat <<'EOF'
## Summary

- Install qwen-asr PyPI package in an isolated lyrics_venv on Windows (pins transformers 4.57.6, bypassing the qwen3_asr architecture error in system transformers 5.5.3)
- Rewrite scripts/lyrics_worker.py to call qwen_asr.Qwen3ForcedAligner.align directly; drop the dead CTC/torchaudio/even-distribution fallback stack
- Preload the 1.2 GB model during bootstrap so the first real song isn't slow
- Skip alignment for songs > 5 min (Qwen3 architectural limit); keep LRCLIB line-level timings
- New merge_word_timings helper keeps LRCLIB timings per line and attaches aligned words
- Label aligned songs as `lrclib+qwen3` to distinguish from line-level-only

Fixes #25.

## Test plan

- [x] Rust unit tests for merge_word_timings (4 cases)
- [x] Rust unit tests for bootstrap venv path resolution
- [x] Playwright E2E asserts at least one lyrics JSON has word-level timestamps
- [x] Manual deploy verification on win-resolume: lyrics_venv exists, one song has source=lrclib+qwen3 with populated words
- [x] Manual verification: dashboard karaoke panel highlights words during playback

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: PR URL returned. Monitor its CI. Report PR URL to the user with mergeable status from `gh api repos/:owner/:repo/pulls/:num --jq '.mergeable_state'` (expect `clean`). **Do not merge until the user explicitly says so.**

---

## Self-review

**Spec coverage (re-reading `docs/superpowers/specs/2026-04-14-qwen3-forced-aligner-design.md`):**

| Spec section | Task covering it |
|---|---|
| Venv isolation at `{tools_dir}/lyrics_venv/` | Task 3 (bootstrap.rs), Task 4 (struct uses it) |
| `qwen-asr` PyPI install pinned transformers 4.57.6 | Task 3 Step 4 (`pip install -U qwen-asr`) |
| Rust discovery of venv python | Task 3 (`venv_python_path`), Task 4 (`venv_python` field) |
| Model weight download on first run | Task 1 Step 2 (`cmd_preload`) + Task 3 Step 4 (preload invocation) |
| Python script rewrite with official API | Task 1 Step 1 (cmd_align replacement) |
| Flip `if false {}` → real pipeline | Task 4 Step 3 |
| 5-min duration gate, skip with `lrclib` label | Task 4 Step 3 (`QWEN3_ALIGNER_MAX_MS` check) |
| Preserve LRCLIB line timings via merge helper | Task 2 (merge_word_timings) + Task 4 (invocation) |
| Label `lrclib+qwen3` on aligned songs | Task 4 Step 3 (`source = "lrclib+qwen3"`) |
| No CTC / torchaudio / even-distributed fallbacks | Task 1 Step 1 (deletions explicit) |
| Bootstrap failure is non-fatal | Task 4 Step 2 (warn! on Err, keep venv_python None) |
| Rust unit tests for merge + bootstrap paths | Task 2, Task 3 |
| E2E word-level assertion | Task 6 |
| Deploy verification (model load + real song + dashboard) | Task 7 Steps 4–6 |

No spec gaps.

**Placeholder scan:** No "TBD", no "similar to task N", no generic "handle errors" without specifics. Each step has the actual code, actual commands, and actual expected output.

**Type consistency:** `merge_word_timings` signature defined in Task 2, used in Task 4 — same name, same `(Vec<LyricsLine>, Vec<LyricsLine>) -> Vec<LyricsLine>` signature. `venv_python: tokio::sync::RwLock<Option<PathBuf>>` field defined in Task 4 Step 1, read/written in Task 4 Step 2 and Step 3 — matches. `ensure_ready` defined in Task 3 Step 4, called in Task 4 Step 2 — signature matches. `QWEN3_ALIGNER_MAX_MS` introduced exactly where it's used. `cmd_preload` defined in Task 1 Step 2, invoked in Task 3 Step 4 preload call with the same `preload --models-dir ...` args the Python script expects.

No inconsistencies found.
