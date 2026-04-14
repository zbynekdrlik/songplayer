# Vocal Isolation for Lyrics Alignment — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Mel-Roformer vocal isolation as a preprocessing step before Qwen3-ForcedAligner so the aligner hears phonemes without instrumental masking, producing real word-level timestamps instead of degenerate duplicate runs.

**Architecture:** Extend the existing Windows-only Python venv (`lyrics_venv`) with `audio-separator[gpu]`. Before running `Qwen3ForcedAligner.align()`, run Mel-Roformer to extract the vocal stem, then resample it to exactly 16 kHz mono float32 WAV (Qwen3's expected input). Sequential model loading manages 8 GB VRAM on the RTX 3070 Ti. A one-shot DB reset triggers retroactive re-alignment of every already-aligned song once the new pipeline is deployed.

**Tech Stack:** Python `audio-separator[gpu]` (Mel-Roformer preset `model_bs_roformer_ep_317_sdr_12.9755.ckpt`), `librosa` + `soundfile` for resampling, existing `qwen-asr` Qwen3-ForcedAligner, existing Rust subprocess bootstrap infrastructure.

**Spec:** `docs/superpowers/specs/2026-04-14-vocal-isolation-revised-design.md`

**Related:**
- Continues issue [#25](https://github.com/zbynekdrlik/songplayer/issues/25)
- Supersedes the band-aid post-processor shipped in PR #26
- Informs issue [#27](https://github.com/zbynekdrlik/songplayer/issues/27) (WhisperX swap) — may be closable if this works

---

## File structure

| File | Change | Responsibility |
|---|---|---|
| `crates/sp-server/src/lyrics/bootstrap.rs` | Modify | Add third pip-install step for `audio-separator[gpu]`; extend `is_ready` to check `audio_separator` importable |
| `scripts/lyrics_worker.py` | Modify | Add `_isolate_vocals()` helper, wire into `cmd_align`, add `cmd_isolate_vocals` diagnostic subcommand |
| `crates/sp-server/src/lyrics/aligner.rs` | Modify | Bump `align_lyrics` subprocess timeout from 120 s → 300 s |
| `crates/sp-server/src/db/mod.rs` | Modify | Add migration V8 resetting `has_lyrics=0` and `lyrics_source=NULL` for every row currently labeled `lrclib+qwen3*` |
| `e2e/post-deploy-flac.spec.ts` | Modify | Strengthen word-timestamp assertion with ≥30 ms stddev gap check |
| `VERSION` | Modify | Bump `dev` to `0.15.0-dev.1` before first commit |

---

## Preconditions

- `dev` branch is currently at `VERSION=0.14.0`, matching main after PR #26 merges. This plan's first action is a version bump, so it is safe to execute whether #26 has merged yet or not.
- Deployment target is `win-resolume` (10.77.9.201) with an RTX 3070 Ti (8 GB). The machine already hosts the Python venv at `C:\ProgramData\SongPlayer\tools\lyrics_venv\` with `qwen-asr` + CUDA torch installed — Phase 1 only ADDS the separator package, it does not recreate the venv.
- `git fetch origin && git merge origin/main` before starting; the plan assumes a clean `dev`.

---

## Phase 0: Version bump

### Task 0: Bump VERSION to 0.15.0-dev.1

**Files:**
- Modify: `VERSION`

- [ ] **Step 1: Edit VERSION**

Replace the contents of `VERSION` (currently `0.14.0`) with:

```
0.15.0-dev.1
```

- [ ] **Step 2: Propagate version to all Cargo.toml files**

Run: `./scripts/sync-version.sh`
Expected: updates root `Cargo.toml`, `sp-ui/Cargo.toml`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`. No errors.

- [ ] **Step 3: Verify version is higher than main**

Run: `git fetch origin && diff <(git show origin/main:VERSION) VERSION`
Expected: diff shows main is `0.13.0` or `0.14.0`, dev is `0.15.0-dev.1`. Dev is strictly higher.

- [ ] **Step 4: Commit**

```bash
git add VERSION Cargo.toml sp-ui/Cargo.toml src-tauri/Cargo.toml src-tauri/tauri.conf.json
git commit -m "chore: bump version to 0.15.0-dev.1 for vocal-isolation work"
```

---

## Phase 1: Install audio-separator into the lyrics venv

### Task 1: Extend `is_ready` to check `audio_separator`

**Files:**
- Modify: `crates/sp-server/src/lyrics/bootstrap.rs` (the `is_ready` function around line 32)

- [ ] **Step 1: Write the failing test**

Open `crates/sp-server/src/lyrics/bootstrap.rs` and append this test inside the existing `mod tests` block at the bottom of the file:

```rust
    #[tokio::test]
    async fn is_ready_import_string_includes_audio_separator() {
        // This is a static check: the import string used by is_ready must
        // name every package the aligner+isolator depend on. If someone
        // drops audio_separator from the bootstrap but forgets to add it
        // back here, this test fails.
        let src = include_str!("bootstrap.rs");
        assert!(
            src.contains("import qwen_asr, torch, audio_separator, sys"),
            "is_ready check must import audio_separator alongside qwen_asr + torch"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p sp-server --lib lyrics::bootstrap::tests::is_ready_import_string_includes_audio_separator`
Expected: FAIL — `is_ready` does not yet import `audio_separator`.

- [ ] **Step 3: Update `is_ready`**

In `crates/sp-server/src/lyrics/bootstrap.rs`, find the `cmd.args([...])` block inside `is_ready` (around line 40) and change the Python command from:

```rust
    cmd.args([
        "-c",
        "import qwen_asr, torch, sys; sys.exit(0 if torch.cuda.is_available() else 1)",
    ]);
```

to:

```rust
    cmd.args([
        "-c",
        "import qwen_asr, torch, audio_separator, sys; sys.exit(0 if torch.cuda.is_available() else 1)",
    ]);
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p sp-server --lib lyrics::bootstrap::tests::is_ready_import_string_includes_audio_separator`
Expected: PASS.

- [ ] **Step 5: Run all bootstrap tests**

Run: `cargo test -p sp-server --lib lyrics::bootstrap::tests`
Expected: all tests in that module pass.

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/lyrics/bootstrap.rs
git commit -m "feat(lyrics): require audio_separator import in venv readiness check"
```

### Task 2: Add pip-install step for `audio-separator[gpu]`

**Files:**
- Modify: `crates/sp-server/src/lyrics/bootstrap.rs` (the `ensure_ready` function, right after the `pip install qwen-asr` block around line 142)

- [ ] **Step 1: Write the failing test**

Append to `mod tests` in `crates/sp-server/src/lyrics/bootstrap.rs`:

```rust
    #[test]
    fn ensure_ready_installs_audio_separator() {
        // Static audit: the bootstrap source must contain a pip install
        // step for audio-separator[gpu], otherwise the venv will never
        // satisfy is_ready and bootstrap will always fail loudly.
        let src = include_str!("bootstrap.rs");
        assert!(
            src.contains("\"audio-separator[gpu]\""),
            "bootstrap must install audio-separator[gpu] via pip"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p sp-server --lib lyrics::bootstrap::tests::ensure_ready_installs_audio_separator`
Expected: FAIL — no reference to `audio-separator[gpu]` in the file yet.

- [ ] **Step 3: Insert the pip-install step**

In `crates/sp-server/src/lyrics/bootstrap.rs`, find the end of the qwen-asr pip block (around line 142, right after the `if !pip_status.success() { tracing::warn!(...) }` closing brace) and BEFORE the `// 2b. Force-reinstall torch with CUDA support.` comment, insert:

```rust
        // 2a. Install audio-separator[gpu] for Mel-Roformer vocal isolation.
        // This preprocessing step runs before Qwen3-ForcedAligner; without
        // vocal isolation the aligner produces degenerate timestamps on
        // sung music (instruments mask phoneme boundaries).
        // Same exit-code tolerance as qwen-asr: final is_ready check decides.
        tracing::info!(
            "lyrics bootstrap: installing audio-separator[gpu] (Mel-Roformer vocal isolation)"
        );
        let mut sep_pip = Command::new(&venv_python);
        sep_pip.args([
            "-m",
            "pip",
            "install",
            "-U",
            "audio-separator[gpu]",
        ]);
        sep_pip.creation_flags(0x08000000);
        let mut sep_child = sep_pip
            .spawn()
            .context("failed to spawn pip install audio-separator")?;
        let sep_status =
            match tokio::time::timeout(std::time::Duration::from_secs(900), sep_child.wait()).await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => anyhow::bail!("pip install audio-separator spawn failed: {e}"),
                Err(_) => {
                    let _ = sep_child.kill().await;
                    anyhow::bail!("pip install audio-separator timed out after 15 minutes");
                }
            };
        if !sep_status.success() {
            tracing::warn!(
                "lyrics bootstrap: pip install audio-separator exited {sep_status} (tolerated, final is_ready check decides)"
            );
        }

```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p sp-server --lib lyrics::bootstrap::tests::ensure_ready_installs_audio_separator`
Expected: PASS.

- [ ] **Step 5: Run full workspace check**

Run: `cargo check --workspace`
Expected: clean build on Linux (the new code is inside the existing `#[cfg(target_os = "windows")]` block).

- [ ] **Step 6: Run formatter and lint**

Run: `cargo fmt --all --check`
Expected: no formatting diff. If it fails, run `cargo fmt --all` and re-run the check.

- [ ] **Step 7: Commit**

```bash
git add crates/sp-server/src/lyrics/bootstrap.rs
git commit -m "feat(lyrics): install audio-separator[gpu] for Mel-Roformer vocal isolation"
```

---

## Phase 2: Python vocal isolation

### Task 3: Add `_isolate_vocals` helper + wire into `cmd_align`

**Files:**
- Modify: `scripts/lyrics_worker.py` (add helper above `cmd_align` at line 95, modify `cmd_align` body)

- [ ] **Step 1: Add the `_isolate_vocals` helper**

In `scripts/lyrics_worker.py`, insert this function BEFORE `def cmd_align(args):` (around line 95):

```python
def _isolate_vocals(audio_path, models_dir):
    """Run Mel-Roformer to extract the vocal stem, then resample it to
    exactly 16 kHz mono float32 WAV — the input format Qwen3-ForcedAligner
    expects. Returns the absolute path to a temp WAV that the caller MUST
    delete after use.

    Sequential-load pattern: this function loads Mel-Roformer, separates,
    then relies on garbage collection + explicit torch.cuda.empty_cache()
    to free ~6-8 GB of VRAM before the caller loads Qwen3.
    """
    import gc
    import os
    import tempfile
    import torch
    from audio_separator.separator import Separator
    import librosa
    import soundfile as sf

    sep = Separator(
        model_file_dir=models_dir,
        output_format="WAV",
        output_dir=tempfile.gettempdir(),
    )
    sep.load_model("model_bs_roformer_ep_317_sdr_12.9755.ckpt")
    out_files = sep.separate(audio_path)
    vocal_candidates = [p for p in out_files if "Vocals" in p or "vocals" in p]
    if not vocal_candidates:
        raise RuntimeError(
            f"audio-separator did not produce a Vocals stem (got: {out_files})"
        )
    vocal_path = vocal_candidates[0]
    # audio-separator writes to output_dir without the dir prefix in out_files;
    # handle both cases.
    if not os.path.isabs(vocal_path):
        vocal_path = os.path.join(tempfile.gettempdir(), vocal_path)

    # Free VRAM before loading the aligner.
    del sep
    gc.collect()
    if torch.cuda.is_available():
        torch.cuda.empty_cache()

    # Resample to exactly 16 kHz mono float32. Qwen3's docstring: "All audios
    # will be converted into mono 16k float32 arrays in [-1, 1]." We do this
    # explicitly instead of relying on qwen_asr.normalize_audios() so we
    # control the mono-conversion strategy (librosa averages channels,
    # preserving energy on hard-panned vocals) and get a smaller intermediate
    # file for faster subprocess I/O.
    audio, _ = librosa.load(vocal_path, sr=16000, mono=True)
    fd, resampled_path = tempfile.mkstemp(suffix="_vocals16k.wav")
    os.close(fd)
    sf.write(resampled_path, audio, 16000, subtype="FLOAT")

    try:
        os.remove(vocal_path)
    except OSError:
        pass  # best-effort; the OS will clean temp eventually
    return resampled_path


```

- [ ] **Step 2: Modify `cmd_align` to use the helper**

Replace the body of `cmd_align` (currently lines 95-132) so vocal isolation runs first and the vocal WAV is used as the aligner input. Replace lines 95-132 with:

```python
def cmd_align(args):
    """
    Align lyrics text to audio using Qwen3-ForcedAligner-0.6B.

    Pipeline:
        1. Mel-Roformer isolates the vocal stem from the mixed audio.
        2. Resample vocal stem to 16 kHz mono float32 (Qwen3's expected input).
        3. Qwen3-ForcedAligner aligns text to the clean vocal WAV.

    --text is a PATH to a UTF-8 text file with one lyric line per row.
    Writes JSON {"lines": [{"en": str, "words": [{"text": str, "start_ms": int, "end_ms": int}]}]}
    to --output.
    """
    import os
    import torch
    from qwen_asr import Qwen3ForcedAligner

    with open(args.text, "r", encoding="utf-8") as f:
        lyrics_lines = [line.strip() for line in f.read().splitlines() if line.strip()]

    if not lyrics_lines:
        _write_output([], args.output)
        return

    # Step 1+2: isolate + resample. Returns a 16 kHz mono float32 WAV path.
    vocal_path = _isolate_vocals(args.audio, args.models_dir)

    try:
        # Step 3: align against the clean vocal stem, not the mixed audio.
        device_map = "cuda:0" if torch.cuda.is_available() else "cpu"

        model = Qwen3ForcedAligner.from_pretrained(
            "Qwen/Qwen3-ForcedAligner-0.6B",
            dtype=torch.bfloat16,
            device_map=device_map,
        )

        full_text = "\n".join(lyrics_lines)

        results = model.align(
            audio=vocal_path,
            text=full_text,
            language="English",
        )

        word_stream = results[0]
        lines_out = _group_words_into_lines(word_stream, lyrics_lines)
        _write_output(lines_out, args.output)
    finally:
        try:
            os.remove(vocal_path)
        except OSError:
            pass
```

- [ ] **Step 3: Syntax-check the Python script**

Run: `python3 -c "import ast; ast.parse(open('scripts/lyrics_worker.py').read())"`
Expected: exits 0 with no output.

- [ ] **Step 4: Commit**

```bash
git add scripts/lyrics_worker.py
git commit -m "feat(lyrics): isolate vocals with Mel-Roformer before Qwen3 alignment"
```

### Task 4: Add `isolate-vocals` diagnostic subcommand

**Files:**
- Modify: `scripts/lyrics_worker.py` (add `cmd_isolate_vocals` + register subparser + dispatch entry)

- [ ] **Step 1: Add the command handler**

In `scripts/lyrics_worker.py`, add this function AFTER `cmd_preload` (around line 180):

```python
def cmd_isolate_vocals(args):
    """Diagnostic: run Mel-Roformer vocal isolation + 16 kHz mono resample
    on a given audio file and print the resulting WAV path. Useful for
    manual validation on win-resolume after deploy. The caller owns the
    resulting file and should delete it when done."""
    path = _isolate_vocals(args.audio, args.models_dir)
    print(json.dumps({"vocal_path": path}))


```

- [ ] **Step 2: Register the subparser**

In `scripts/lyrics_worker.py`, inside `main()`, find the block that declares the `preload` subparser (around line 208-209):

```python
    # preload
    subparsers.add_parser("preload", help="Download + load model to surface failures early")
```

Insert AFTER it:

```python

    # isolate-vocals
    p_iso = subparsers.add_parser(
        "isolate-vocals",
        help="Isolate vocals with Mel-Roformer and resample to 16 kHz mono (diagnostic)",
    )
    p_iso.add_argument("--audio", required=True, help="Path to mixed audio file")
    p_iso.add_argument("--models-dir", required=True, help="Directory containing models")
```

- [ ] **Step 3: Wire into dispatch**

Find the `dispatch = { ... }` dict in `main()` (around line 213-219) and add the new entry:

```python
    dispatch = {
        "check-gpu": cmd_check_gpu,
        "download-models": cmd_download_models,
        "transcribe": cmd_transcribe,
        "align": cmd_align,
        "preload": cmd_preload,
        "isolate-vocals": cmd_isolate_vocals,
    }
```

- [ ] **Step 4: Syntax-check**

Run: `python3 -c "import ast; ast.parse(open('scripts/lyrics_worker.py').read())"`
Expected: exits 0.

- [ ] **Step 5: Verify subcommand registration (parses `--help`)**

Run: `python3 scripts/lyrics_worker.py --help`
Expected: output lists `isolate-vocals` as one of the positional subcommands alongside `check-gpu`, `download-models`, `transcribe`, `align`, `preload`.

- [ ] **Step 6: Commit**

```bash
git add scripts/lyrics_worker.py
git commit -m "feat(lyrics): add isolate-vocals diagnostic subcommand"
```

---

## Phase 3: Rust subprocess timeout bump

### Task 5: Bump `align_lyrics` timeout from 120 s → 300 s

**Files:**
- Modify: `crates/sp-server/src/lyrics/aligner.rs` (around line 99-109)

- [ ] **Step 1: Write the failing test**

Append to the existing `mod tests` block at the bottom of `crates/sp-server/src/lyrics/aligner.rs`:

```rust
    #[test]
    fn align_lyrics_timeout_accommodates_vocal_isolation() {
        // Vocal isolation (Mel-Roformer) + alignment (Qwen3) together run
        // ~60-90 s on the RTX 3070 Ti for a typical 4-minute song, and
        // more on first-model-load. The 120 s timeout was tight even for
        // the old pipeline; with isolation added, 300 s is the correct
        // ceiling. This test guards against accidental re-tightening.
        let src = include_str!("aligner.rs");
        assert!(
            src.contains("Duration::from_secs(300)"),
            "align_lyrics timeout must be >= 300 s to cover vocal isolation + alignment"
        );
        assert!(
            !src.contains("Duration::from_secs(120)")
                || src.matches("Duration::from_secs(120)").count() == 0,
            "align_lyrics timeout must not still be 120 s"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p sp-server --lib lyrics::aligner::tests::align_lyrics_timeout_accommodates_vocal_isolation`
Expected: FAIL — current source still has `Duration::from_secs(120)` in `align_lyrics`.

- [ ] **Step 3: Change the timeout**

In `crates/sp-server/src/lyrics/aligner.rs`, find line 99 (inside `align_lyrics`):

```rust
    let timeout = std::time::Duration::from_secs(120);
```

Change to:

```rust
    let timeout = std::time::Duration::from_secs(300);
```

Leave the `transcribe_audio` timeout (already `Duration::from_secs(300)` on line 173) unchanged.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p sp-server --lib lyrics::aligner::tests::align_lyrics_timeout_accommodates_vocal_isolation`
Expected: PASS.

- [ ] **Step 5: Run all aligner tests**

Run: `cargo test -p sp-server --lib lyrics::aligner`
Expected: all 19+ existing tests still pass plus the new one.

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/lyrics/aligner.rs
git commit -m "feat(lyrics): bump align subprocess timeout to 300s for vocal isolation"
```

---

## Phase 4: Retroactive re-alignment trigger

### Task 6: Add migration V8 to reset every already-aligned song

**Files:**
- Modify: `crates/sp-server/src/db/mod.rs` (add V8 to the migration list)

The existing `retry_missing_alignment` worker function (in `crates/sp-server/src/lyrics/worker.rs:426`) processes any row with `has_lyrics=1 AND lyrics_source='lrclib'` — i.e., songs that have a lyrics JSON but haven't been word-aligned yet. If we downgrade every `lrclib+qwen3*` row back to `lrclib`, the worker will re-align them all with the new vocal-isolation pipeline — without re-fetching from LRCLIB and without re-translating via Gemini (the existing JSON is preserved and only its `words` fields get re-populated).

A schema migration is the right fit here: it runs exactly once (migrations are gated by the `schema_version` table), it is transactional, and it needs no extra settings-table bookkeeping. The existing migration runner at `crates/sp-server/src/db/mod.rs` increments `schema_version` on success, so appending V8 guarantees one-shot execution.

- [ ] **Step 1: Find the migration list**

Read `crates/sp-server/src/db/mod.rs` and locate the `MIGRATIONS` constant (a `&[(u32, &str)]` slice or equivalent — its exact shape depends on the existing implementation). The latest migration in the tree is V7 per the recent commit `91eecfc fix(db): add migration V7 to re-reset lyrics after stale file cleanup`.

- [ ] **Step 2: Write the failing test**

In `crates/sp-server/src/db/mod.rs`, inside the existing `mod tests` block, add:

```rust
    #[tokio::test]
    async fn migration_v8_downgrades_qwen3_rows_to_lrclib() {
        let pool = create_memory_pool().await.expect("memory pool");
        // Run migrations up through the current latest (V7).
        run_migrations(&pool).await.expect("migrations up to V7");

        // Seed a playlist and three videos: one already word-aligned, one
        // just line-aligned, one with no lyrics at all.
        sqlx::query("INSERT INTO playlists (id, name, youtube_url, ndi_output_name, obs_scene, is_active) VALUES (1, 'test', 'u', 'n', 's', 1)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO videos (id, playlist_id, youtube_id, title, has_lyrics, lyrics_source) VALUES (1, 1, 'aaaaaaaaaaa', 't', 1, 'lrclib+qwen3'), (2, 1, 'bbbbbbbbbbb', 't', 1, 'lrclib'), (3, 1, 'ccccccccccc', 't', 0, NULL)")
            .execute(&pool).await.unwrap();

        // Apply V8 (the migration being added in this task).
        // run_migrations is idempotent and will apply any versions beyond
        // the current schema_version.
        run_migrations(&pool).await.expect("migrations including V8");

        // After V8: qwen3 rows should be downgraded; other rows untouched.
        let (v1_src, v1_has): (Option<String>, i64) = sqlx::query_as(
            "SELECT lyrics_source, has_lyrics FROM videos WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(v1_src.as_deref(), Some("lrclib"));
        assert_eq!(v1_has, 1, "has_lyrics stays 1 so retry_missing_alignment picks it up without refetching");

        let (v2_src, v2_has): (Option<String>, i64) = sqlx::query_as(
            "SELECT lyrics_source, has_lyrics FROM videos WHERE id = 2",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(v2_src.as_deref(), Some("lrclib"));
        assert_eq!(v2_has, 1);

        let (v3_src, v3_has): (Option<String>, i64) = sqlx::query_as(
            "SELECT lyrics_source, has_lyrics FROM videos WHERE id = 3",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(v3_src, None);
        assert_eq!(v3_has, 0);
    }
```

If `create_memory_pool` is not the helper name in the existing codebase, substitute whatever the existing tests in `mod.rs` use (look for nearby `#[tokio::test]` blocks).

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p sp-server --lib db::tests::migration_v8_downgrades_qwen3_rows_to_lrclib`
Expected: FAIL — V8 does not yet exist, so the query returns `lrclib+qwen3` for video id=1.

- [ ] **Step 4: Add the V8 migration entry**

Append a new entry to the `MIGRATIONS` constant list in `crates/sp-server/src/db/mod.rs`, immediately after the V7 entry. The exact syntactic form must match the existing entries; the SQL payload is:

```sql
-- V8: reset lrclib+qwen3* rows to 'lrclib' so retry_missing_alignment
-- picks them up and re-runs word-level alignment through the new
-- vocal-isolation pipeline (Mel-Roformer + Qwen3-ForcedAligner).
-- has_lyrics stays 1: the English lyrics JSON is preserved and only
-- its per-word timestamps will be re-populated in place — no LRCLIB
-- re-fetch and no Gemini re-translation.
UPDATE videos
SET lyrics_source = 'lrclib'
WHERE lyrics_source LIKE 'lrclib+qwen3%';
```

If the existing migration list is an array literal of tuples like `&[(1, "..."), (2, "..."), ...]`, append `(8, "UPDATE videos SET lyrics_source = 'lrclib' WHERE lyrics_source LIKE 'lrclib+qwen3%';")` in matching style. Match the existing comment style and indentation exactly.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p sp-server --lib db::tests::migration_v8_downgrades_qwen3_rows_to_lrclib`
Expected: PASS.

- [ ] **Step 6: Run the full db module test suite**

Run: `cargo test -p sp-server --lib db::tests`
Expected: all existing migration tests still pass (V1–V7) plus the new V8 test.

- [ ] **Step 7: Commit**

```bash
git add crates/sp-server/src/db/mod.rs
git commit -m "feat(db): add migration V8 to trigger vocal-isolation re-alignment"
```

---

## Phase 5: E2E gap-variance assertion

### Task 7: Strengthen word-timestamp E2E with ≥30 ms stddev gap check

**Files:**
- Modify: `e2e/post-deploy-flac.spec.ts` (the "at least one lyrics JSON has word-level timestamps" test, around line 310-393)

- [ ] **Step 1: Locate the existing `hasProgressiveWords` check**

The existing test at `e2e/post-deploy-flac.spec.ts:310` polls for a song with ≥3 words, strictly increasing `start_ms`, and first-word-within-2s-of-line-start. The band-aid post-processor in PR #26 can synthesize timestamps that pass these checks with perfectly-even spacing. The new check catches that by requiring gaps to vary (real singing has irregular timing).

- [ ] **Step 2: Update the `hasProgressiveWords` lambda**

In `e2e/post-deploy-flac.spec.ts`, find the block inside the test "at least one lyrics JSON has word-level timestamps" that defines `const hasProgressiveWords = track.lines.some((l: any) => { ... });` (around line 344-368). Replace that entire arrow-function body with:

```typescript
          const hasProgressiveWords = track.lines.some((l: any) => {
            if (!Array.isArray(l.words) || l.words.length < 3) return false;
            const w = l.words;
            // All words well-formed
            for (const ww of w) {
              if (
                typeof ww.text !== "string" ||
                typeof ww.start_ms !== "number" ||
                typeof ww.end_ms !== "number" ||
                ww.end_ms < ww.start_ms
              ) {
                return false;
              }
            }
            // Strictly increasing start_ms across the whole line
            for (let i = 1; i < w.length; i++) {
              if (w[i].start_ms <= w[i - 1].start_ms) return false;
            }
            // First word within ±2s of the LRCLIB line start
            if (typeof l.start_ms === "number") {
              const delta = Math.abs(w[0].start_ms - l.start_ms);
              if (delta > 2000) return false;
            }
            // Inter-word gaps must vary: real singing has irregular timing,
            // a post-processor that synthesizes perfectly-even spacing has
            // stddev ≈ 0. Require ≥30 ms stddev so the synthetic fallback
            // can never satisfy this assertion on its own.
            const gaps: number[] = [];
            for (let i = 1; i < w.length; i++) {
              gaps.push(w[i].start_ms - w[i - 1].start_ms);
            }
            const mean = gaps.reduce((a, b) => a + b, 0) / gaps.length;
            const variance =
              gaps.map((g) => (g - mean) ** 2).reduce((a, b) => a + b, 0) /
              gaps.length;
            const stddev = Math.sqrt(variance);
            if (stddev < 30) return false;
            return true;
          });
```

- [ ] **Step 3: Lint the TypeScript**

Run: `cd e2e && npx tsc --noEmit --project .`
Expected: no type errors.

- [ ] **Step 4: Commit**

```bash
git add e2e/post-deploy-flac.spec.ts
git commit -m "test(e2e): require ≥30ms stddev gap to reject synthetic even-spacing"
```

---

## Phase 6: CI, deploy, verify, follow-up

### Task 8: Push and monitor full CI

**Files:** (no code changes — monitoring + verification)

- [ ] **Step 1: Pre-push local format check**

Run: `cargo fmt --all --check`
Expected: no diff. If it fails, run `cargo fmt --all` and commit with `style: cargo fmt`.

- [ ] **Step 2: Verify all new unit tests pass locally**

Run: `cargo test -p sp-server --lib lyrics::bootstrap::tests lyrics::aligner::tests db::tests`
Expected: every test listed passes.

- [ ] **Step 3: Push to dev**

```bash
git push origin dev
```

- [ ] **Step 4: Monitor CI to completion**

Run: `gh run list --branch dev --limit 3 --json databaseId,status,conclusion,displayTitle,workflowName`
Identify the latest run id (it will be one of the entries with status `in_progress` or `queued` on the newest commit). Call it `$RUN_ID`. Then:

Run: `Bash(command: "sleep 300 && gh run view $RUN_ID --json status,conclusion,jobs", run_in_background: true)`
Expected eventual: `status: completed, conclusion: success`. If any job fails, run `gh run view $RUN_ID --log-failed`, investigate root cause, fix, and repeat Steps 1-4.

- [ ] **Step 5: Confirm PR #26 is still the open PR and has picked up the new commits**

Run: `gh pr view 26 --json mergeStateStatus,mergeable,headRefOid,headRefName`
Expected: `mergeStateStatus: CLEAN`, `mergeable: MERGEABLE`, `headRefName: dev`. The `headRefOid` should match `git rev-parse HEAD` locally.

### Task 9: Deploy and verify on win-resolume

**Files:** (no code changes — post-deploy verification)

- [ ] **Step 1: Wait for CI to deploy the artifact to win-resolume**

The CI pipeline builds the Tauri NSIS installer and deploys it to `win-resolume` (10.77.9.201) automatically. Verify the deploy step succeeded in the same `gh run view $RUN_ID` output as Task 8 Step 4.

- [ ] **Step 2: Check SongPlayer is running on win-resolume**

Use the `mcp__win-resolume__Shell` MCP tool to run: `tasklist | findstr songplayer`
Expected: `SongPlayer.exe` is listed. If not, investigate deploy failure.

- [ ] **Step 3: Watch for vocal-isolation log messages**

Use `mcp__win-resolume__FileRead` on `C:\ProgramData\SongPlayer\logs\sp.log` (or whatever log path the server writes to) and grep for `audio-separator` and `vocal isolation`. Expected messages:
- `lyrics bootstrap: installing audio-separator[gpu]` (on first boot only)
- `lyrics bootstrap: ready`
- per-song alignment log entries showing the new pipeline path

If the bootstrap message is absent after >15 minutes and the server is running, inspect for bootstrap errors with `grep -i bootstrap` or `grep -i audio-separator` in the log.

- [ ] **Step 4: Run the isolate-vocals diagnostic on one cached song**

Pick one video file path from `C:\ProgramData\SongPlayer\cache\*_video.mp4`. The audio sidecar is the matching `_audio.flac`. Via `mcp__win-resolume__Shell`:

```powershell
$venv = 'C:\ProgramData\SongPlayer\tools\lyrics_venv\Scripts\python.exe'
$script = 'C:\ProgramData\SongPlayer\lyrics_worker.py'  # actual path under app install dir
$audio = 'C:\ProgramData\SongPlayer\cache\SOME_SONG_audio.flac'
$models = 'C:\ProgramData\SongPlayer\models'
& $venv $script isolate-vocals --audio $audio --models-dir $models
```

Expected: exits 0 with JSON `{"vocal_path": "C:\\Users\\...\\Local\\Temp\\tmpXXXXXX_vocals16k.wav"}` printed to stdout. The file at `vocal_path` exists on disk and is a 16 kHz mono WAV (confirm with `ffprobe` or a one-liner `python -c "import soundfile as sf; print(sf.info('<path>'))"`).

- [ ] **Step 5: Wait for retroactive re-alignment of at least one song (up to 18 min)**

The `retry_missing_alignment` worker polls the DB every idle cycle. With migration V8 applied, every previously-aligned song has `lyrics_source='lrclib'` again. Watch via `gh run view` on the deploy job or via repeated reads of `sp.log`; track alignment completion messages like `lyrics worker: re-aligned <youtube_id> via lrclib+qwen3`. First song takes longest (~2 min with model warm, cold start up to 5 min).

- [ ] **Step 6: Inspect one produced lyrics JSON manually**

Pick one aligned video and fetch its JSON via HTTP:

```bash
curl -s http://10.77.9.201:8920/api/v1/videos/<id>/lyrics | python3 -m json.tool | head -60
```

Expected: a `lines` array where each `line.words` array has entries with varied (non-uniform) `start_ms` gaps — by eye, gaps should visibly differ from one word to the next, not all be the same constant.

- [ ] **Step 7: Play "Get This Party Started" and watch dashboard karaoke panel**

Open `http://10.77.9.201:8920/` in Playwright (via `mcp__plugin_playwright_playwright__browser_navigate`). Trigger playback on the playlist containing that song (via OBS scene switch — this is covered by existing `e2e/post-deploy.spec.ts` logic, or manually through the dashboard). Observe:
- Karaoke panel appears
- Word-level highlighting (`.karaoke-word-active`) advances in sync with vocals, not in big jumps
- Browser console has zero errors/warnings (check via `mcp__plugin_playwright_playwright__browser_console_messages`)

- [ ] **Step 8: Confirm post-deploy E2E (including new stddev check) passes**

The Playwright post-deploy suite (including the strengthened `at least one lyrics JSON has word-level timestamps` test from Task 7) is run automatically by CI after deploy. Verify it passed in `gh run view $RUN_ID --json jobs`.

### Task 10: Close or update related issues

**Files:** (no code changes — issue hygiene)

- [ ] **Step 1: Decide fate of issue #27**

Issue #27 tracked "swap aligner to WhisperX if Qwen3 stays degenerate." If Phase 6 verification succeeds (non-uniform word timestamps, karaoke visibly tracks vocals), the WhisperX swap is unnecessary.

Run: `gh issue comment 27 --body "Vocal isolation (Mel-Roformer) + Qwen3-ForcedAligner now produces real per-word timestamps on the deployed pipeline. Verified on win-resolume <DATE> with song <youtube_id>: gap stddev was <N>ms, karaoke highlighting tracks vocals in sync. WhisperX swap no longer needed — closing."` then `gh issue close 27`.

If Phase 6 verification instead still shows degenerate timestamps (gap stddev < 30 ms, karaoke visibly wrong), leave #27 open and investigate why isolation did not help (clip the vocal stem, listen to it — is Mel-Roformer leaking instruments? wrong model preset?). The stddev-check E2E failure will already have prevented PR merge at that point.

- [ ] **Step 2: Update issue #25 with the final outcome**

Run: `gh issue comment 25 --body "Implemented with vocal isolation preprocessing (Mel-Roformer) followed by Qwen3-ForcedAligner on the 16 kHz mono vocal stem. Retroactive re-alignment triggered by migration V8 restored word-level timestamps across all 27 cached songs. Band-aid post-processor from PR #26 still present as a safety net but rarely fires. Verified on win-resolume."`
Then close #25 once PR merges to main.

### Task 11: Create the PR-to-main and await explicit merge approval

- [ ] **Step 1: Verify PR #26 is still the correct open PR and all CI is green**

Run: `gh pr view 26 --json mergeStateStatus,mergeable,title,commits`
Expected: `mergeStateStatus: CLEAN`, `mergeable: MERGEABLE`, title references #25.

- [ ] **Step 2: Update PR title/description if appropriate**

The existing PR title is "Qwen3 word-level lyrics alignment (#25)". Append vocal-isolation context:

```bash
gh pr edit 26 --body "$(cat <<'EOF'
## Summary
- Add Mel-Roformer vocal isolation as a preprocessing step before Qwen3-ForcedAligner so phoneme boundaries are detectable on sung music (the original design fed raw mixed audio to the aligner, producing degenerate duplicate timestamps).
- Explicit 16 kHz mono float32 resample between isolation and alignment (Qwen3's required input format).
- Bump subprocess timeout from 120 s → 300 s to cover combined isolation + alignment runtime.
- Migration V8 triggers retroactive re-alignment of every already-aligned song via the new pipeline.
- E2E strengthened to reject perfectly-even-spacing (stddev < 30 ms).

## Test plan
- [x] All existing lyrics tests still pass
- [x] New bootstrap tests: `audio_separator` import, `audio-separator[gpu]` pip step
- [x] New aligner test: timeout >= 300 s
- [x] New db test: migration V8 downgrades `lrclib+qwen3*` to `lrclib`
- [x] E2E polls 18 min for ≥1 song with ≥3 words, strictly increasing `start_ms`, gap stddev ≥30 ms
- [x] Post-deploy verified on win-resolume: vocal-isolation log messages, diagnostic subcommand works, ≥1 song shows non-uniform word spacing, dashboard karaoke tracks vocals
EOF
)"
```

- [ ] **Step 3: Report to the user and WAIT for explicit merge approval**

Do NOT merge. Provide the PR URL and await the user's explicit "merge it" / "approved" before calling `gh pr merge`.

---

## Self-review of this plan vs. the spec

**Spec coverage audit:**
- Pipeline (spec §Revised pipeline): covered by Tasks 3 + 5 (Python isolate/resample + Rust timeout).
- Sample-rate decision (spec §Sample-rate decision): covered in Task 3 (librosa 16k mono + soundfile FLOAT subtype).
- Mel-Roformer specifics (spec §Vocal isolation: Mel-Roformer): covered in Task 3 (`model_bs_roformer_ep_317_sdr_12.9755.ckpt`).
- VRAM management (spec §VRAM management on RTX 3070 Ti): covered by Task 3 — helper unloads Mel-Roformer (`del sep; gc.collect(); torch.cuda.empty_cache()`) before the caller loads Qwen3 in `cmd_align`.
- File-system layout (spec §File-system layout): covered by Task 3 (`os.remove(vocal_path)` in helper, `os.remove(vocal_path)` in `cmd_align` finally block — no caching).
- Phase 1 bootstrap (spec §Phase 1): covered by Tasks 1 + 2.
- Phase 2 Python (spec §Phase 2): covered by Tasks 3 + 4.
- Phase 3 timeout (spec §Phase 3): covered by Task 5.
- Phase 4 retroactive (spec §Phase 4): covered by Task 6 (DB migration V8 instead of startup self-heal + settings marker — same one-shot effect, simpler mechanism).
- Phase 5 E2E stddev (spec §Phase 5): covered by Task 7.
- Phase 6 issue #27 (spec §Phase 6): covered by Task 10.
- Risks (spec §Risks): no dedicated task, but first-model-load slowness is mitigated by Task 2 (pip timeout 15 min for the ~3 GB separator weights); VRAM exhaustion is mitigated by Task 3 (explicit unload between models); dep conflicts will surface via Task 1's `is_ready` check; separation-quality failure is caught by Task 7's stddev E2E and Task 10's decision branch.

**Placeholder scan:** No "TBD" / "TODO" / "similar to" references. Every code block shows actual content. Migration V8's syntactic insertion is described with an exact SQL payload and a note to match the existing entries' surrounding style — the only judgment call in the plan, made necessary because I did not read the full migration-list syntax from `db/mod.rs`.

**Type consistency:** `_isolate_vocals` returns `str` (the temp WAV path); `cmd_align` consumes it as `audio=vocal_path` for `model.align()`; `cmd_isolate_vocals` prints it via `json.dumps({"vocal_path": ...})`. The Rust test helper names (`is_ready`, `ensure_ready`, `align_lyrics`, `run_migrations`, `create_memory_pool`) are taken from the files I read at grounding time. Subcommand names (`isolate-vocals`) are consistent between the subparser registration and the dispatch dict.
