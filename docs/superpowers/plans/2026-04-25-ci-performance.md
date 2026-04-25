# CI Performance Optimization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut dev-push CI from 47 min → ~17 min by deleting the 30-min Lyrics Quality Report and its now-orphan baseline producer in `deploy-resolume`.

**Architecture:** Workflow YAML edits only. No code changes. No tests added (verification is the next CI run itself).

**Tech Stack:** GitHub Actions YAML.

---

## Scope correction vs spec

The spec at `docs/superpowers/specs/2026-04-25-ci-performance-design.md` proposes two changes. **Investigation of run 24931489578 shows Change 2 (cache config) buys nothing:**

- `Swatinem/rust-cache@v2` is already in every Rust job in `ci.yml`.
- The Build Tauri job's cache **hit fully** (`Cache restored successfully ... full match: true` at 13:03:54).
- The 8m31s wall time is genuinely `cargo tauri build` release-compiling sp-server's dep graph (sqlx, axum, tokio, reqwest, etc.) on a 4-vCPU windows-latest runner.
- `src-tauri` is excluded from the workspace, so workspace-root `[profile.release]` (lto=true, codegen-units=1) does NOT apply — Tauri builds already use cargo's default codegen-units=16.

**This plan therefore implements Change 1 only.** Cache-config refinements (`save-if`) and Tauri compile-time work are deferred — they buy nothing measurable in the current state and would add risk.

Expected outcome: **47:46 → ~17 min** on every dev push. Certain.

---

## Files touched

| File | Change |
|---|---|
| `VERSION` | Bump `0.23.0-dev.1` → `0.23.0-dev.2` |
| Workspace `Cargo.toml` files via `./scripts/sync-version.sh` | Version sync |
| `.github/workflows/ci.yml` | Delete lines 589–626 (baseline producer + dead recorder); delete lines 1485–1594 (lyrics-quality-report job) |
| `CLAUDE.md` | Add new top-level "## CI architecture" section |

---

## Task 1: Version bump

**Files:**
- Modify: `VERSION`
- Run: `./scripts/sync-version.sh` (auto-updates root `Cargo.toml`, `sp-ui/Cargo.toml`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`)

- [ ] **Step 1: Bump VERSION**

Replace the file contents with exactly:
```
0.23.0-dev.2
```

- [ ] **Step 2: Run sync-version.sh**

```bash
./scripts/sync-version.sh
```

Expected: script exits 0, prints summary of files updated.

- [ ] **Step 3: Verify all files match**

```bash
grep -nE '0\.23\.0-dev\.[12]' VERSION Cargo.toml sp-ui/Cargo.toml src-tauri/Cargo.toml src-tauri/tauri.conf.json
```

Expected: every grep result shows `0.23.0-dev.2`. No remaining `0.23.0-dev.1`.

- [ ] **Step 4: Local fmt check**

```bash
cargo fmt --all --check
```

Expected: exit 0 (no formatting changes since we only edited TOML/JSON/text).

- [ ] **Step 5: Commit**

```bash
git add VERSION Cargo.toml sp-ui/Cargo.toml src-tauri/Cargo.toml src-tauri/tauri.conf.json
git commit -m "chore: bump version to 0.23.0-dev.2"
```

---

## Task 2: Delete `deploy-resolume` baseline-snapshot steps

**Files:**
- Modify: `.github/workflows/ci.yml` lines 589–626

Two adjacent steps in the `deploy-resolume` job become dead code once the consumer is gone:

1. `Snapshot baseline quality BEFORE binary replacement` (lines 589–614) — runs `measure_lyrics_quality.py` and writes `baseline_before.json`.
2. `Record baseline snapshot path` (lines 616–626) — diagnostic logger that just prints presence of the file.

- [ ] **Step 1: Read the current block to confirm the line range matches**

```bash
sed -n '587,628p' .github/workflows/ci.yml
```

Expected output begins with the previous step's tail (the `=== WASM frontend ===` Get-ChildItem block), then the two steps to delete, then the next step `Install WebView2 runtime` at line 628.

If the line numbers have drifted (e.g., earlier step edits), use `grep -n "Snapshot baseline quality"` and `grep -n "Install WebView2 runtime"` to relocate the exact range, then delete inclusively from the line containing `- name: Snapshot baseline quality BEFORE binary replacement` through the line immediately before `- name: Install WebView2 runtime`.

- [ ] **Step 2: Delete the two steps**

Use the Edit tool to remove the block. The exact text to remove is:

```yaml
      - name: Snapshot baseline quality BEFORE binary replacement
        shell: powershell
        run: |
          $cacheDir = "C:\ProgramData\SongPlayer\cache"
          $toolsDir = "C:\ProgramData\SongPlayer\cache\tools"
          $outFile  = "C:\ProgramData\SongPlayer\baseline_before.json"
          $pyScript = "$toolsDir\measure_lyrics_quality.py"
          if (-not (Test-Path $pyScript)) {
            Write-Host "INFO: measure_lyrics_quality.py not yet deployed -- skipping baseline snapshot"
            exit 0
          }
          if (-not (Test-Path $cacheDir)) {
            Write-Host "INFO: cache dir missing -- skipping baseline snapshot"
            exit 0
          }
          $py = Get-Command python -ErrorAction SilentlyContinue
          if (-not $py) {
            Write-Host "INFO: python not on PATH -- skipping baseline snapshot"
            exit 0
          }
          python $pyScript --cache-dir $cacheDir --out $outFile
          if ($LASTEXITCODE -ne 0) {
            Write-Error "FAIL: baseline snapshot exited $LASTEXITCODE"
            exit 1
          }
          Write-Host "Baseline snapshot written to $outFile"

      - name: Record baseline snapshot path
        shell: powershell
        run: |
          # Baseline JSON written to a known path on the self-hosted runner.
          # lyrics-quality-report reads it directly without artifact round-trip.
          $f = "C:\ProgramData\SongPlayer\baseline_before.json"
          if (Test-Path $f) {
            Write-Host "Baseline snapshot present: $f"
          } else {
            Write-Host "INFO: no baseline snapshot (first deploy or no lyrics yet)"
          }

```

Replace with empty string. The previous step (which ends `Get-ChildItem artifacts/dist/ | Select-Object Name`) and the next step (`- name: Install WebView2 runtime`) should now be separated by exactly one blank line.

- [ ] **Step 3: Verify no other references to baseline_before.json remain in deploy-resolume**

```bash
sed -n '550,800p' .github/workflows/ci.yml | grep -nE "baseline_before|measure_lyrics_quality"
```

Expected: no matches inside the deploy-resolume job range. (Matches in the lyrics-quality-report job below are expected — that's deleted in Task 3.)

- [ ] **Step 4: YAML syntax sanity check**

```bash
python3 -c "import yaml, sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('YAML OK')"
```

Expected: `YAML OK`.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: remove dead baseline_before.json producer from deploy-resolume

The two steps writing and logging baseline_before.json existed only to
feed the 30-min Lyrics Quality Report, which is being removed in the
next commit. measure_lyrics_quality.py stays installed on win-resolume
as a manually-runnable tool."
```

---

## Task 3: Delete the `lyrics-quality-report` job

**Files:**
- Modify: `.github/workflows/ci.yml` (delete the job spanning ~110 lines starting at the `lyrics-quality-report:` key)

After Task 2, line numbers will have shifted by 38 lines (lines 589–626 = 38 lines deleted). Use grep to locate the new range, not hardcoded line numbers.

- [ ] **Step 1: Locate the job's start and end**

```bash
grep -nE "^  lyrics-quality-report:|^  [a-z][a-z0-9_-]*:" .github/workflows/ci.yml | grep -A1 "lyrics-quality-report:"
```

Expected output: two lines — the `lyrics-quality-report:` start and the next top-level job key (or end of file). The job to delete is everything from `  lyrics-quality-report:` up to but NOT including the next job key (or end of file if it's last).

If `lyrics-quality-report` is the last job in the file (it currently is — followed by no other top-level key in the workflow), delete from `  lyrics-quality-report:` through the end of the file.

- [ ] **Step 2: Read the block to confirm**

```bash
sed -n '/^  lyrics-quality-report:/,$p' .github/workflows/ci.yml | head -120
```

Expected: starts with `  lyrics-quality-report:`, includes `name: Lyrics Quality Report (30-min post-deploy snapshot)`, includes `Start-Sleep -Seconds 1800`, ends with the artifact-upload step. Roughly 110 lines.

- [ ] **Step 3: Delete the job**

Use the Edit tool with `replace_all: false`. Replace the entire job block (from `  lyrics-quality-report:` through the last line of the file) with empty content. Make sure the file still ends with a single trailing newline.

- [ ] **Step 4: Verify the job is gone**

```bash
grep -c "lyrics-quality-report" .github/workflows/ci.yml
grep -c "Start-Sleep -Seconds 1800" .github/workflows/ci.yml
grep -c "measure_lyrics_quality" .github/workflows/ci.yml
```

Expected: all three print `0`.

- [ ] **Step 5: YAML syntax sanity check**

```bash
python3 -c "import yaml, sys; data=yaml.safe_load(open('.github/workflows/ci.yml')); jobs=list(data['jobs'].keys()); print('jobs:', jobs); assert 'lyrics-quality-report' not in jobs"
```

Expected: prints the job list (16 entries — was 17), then no AssertionError.

- [ ] **Step 6: Verify the e2e-resolume job no longer has dependents**

```bash
grep -nE "needs:.*e2e-resolume" .github/workflows/ci.yml
```

Expected: no matches. (`e2e-resolume` is now a leaf job, which is fine.)

- [ ] **Step 7: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: remove lyrics-quality-report job (30-min sleep + self-reported metric)

The job consumed 30:19 (63%) of every dev push. It compared one metric
(avg_confidence_mean — provider-self-reported) against a baseline and
soft-failed only on >0.02 regression, with soft-skip on missing fields.
No ground-truth comparison meant it could not catch the failure modes
that have been hitting in practice (timing drift, autosub contamination,
Gemini wordless lines, language errors).

measure_lyrics_quality.py remains on win-resolume as a manually-runnable
tool for ad-hoc trend checks. Spec:
docs/superpowers/specs/2026-04-25-ci-performance-design.md"
```

---

## Task 4: CLAUDE.md — add CI architecture section

**Files:**
- Modify: `/home/newlevel/devel/songplayer/CLAUDE.md` (append a new top-level section near the end, before any trailing whitespace)

- [ ] **Step 1: Locate the insertion point**

```bash
grep -nE "^## " CLAUDE.md | tail -5
```

Identify the last `## ` heading. The new section goes after the end of that section's content, as a new top-level `## CI architecture`.

- [ ] **Step 2: Append the section**

Use the Edit tool to append (or use Write if appending at exact EOF). The section content is:

```markdown
## CI architecture

The CI pipeline (`.github/workflows/ci.yml`) gates every push to `dev`
and `main`, plus every PR to `main`. Steady-state runtime on cache hit
is ~17 minutes for a dev push (after the 2026-04-25 lyrics-quality
removal).

**Critical path (a dev push):**

```
parallel ubuntu checks (≤56s)
  → Build WASM (~1:40)
    → Build Tauri (~9:50)             ← release compile of sp-server
      → Gate (2s)
        → Deploy win-resolume (~2:15)
          → E2E win-resolume (~3:00)
TOTAL ≈ 17 min
```

`Build Tauri` dominates because `src-tauri` is excluded from the
workspace and uses cargo's default release profile (codegen-units=16,
no LTO) — already maximally parallel for the Windows runner. Cache
(`Swatinem/rust-cache@v2`) is in place and hits reliably; the time is
genuine release optimization of sp-server's dependency graph.

**Hard rules:**

- **No sleep-based CI jobs.** Any job whose runtime is dominated by
  `sleep` / `Start-Sleep` / `time.sleep` is forbidden. If a soak window
  is needed for trend analysis, it goes into a scheduled workflow
  (cron), not the post-deploy critical path. Cron-scheduled jobs do
  not gate dev pushes.

- **Self-reported metrics are not quality gates.** A pipeline reporting
  its own `confidence` is not a quality signal. Real quality gates
  compare against ground truth (hand-verified fixtures, known-correct
  reference data, or human-perceptible behaviour exercised end-to-end
  via Playwright on the deployed target).

- **`measure_lyrics_quality.py`** stays installed on win-resolume at
  `C:\ProgramData\SongPlayer\cache\tools\measure_lyrics_quality.py` as
  an ad-hoc trend tool. It is no longer wired into CI.
```

- [ ] **Step 3: Verify section is present**

```bash
grep -nE "^## CI architecture" CLAUDE.md
grep -c "No sleep-based CI jobs" CLAUDE.md
```

Expected: section heading found; "No sleep-based CI jobs" count is 1.

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: add CI architecture section with no-sleep-jobs rule"
```

---

## Controller-only verification (after all 4 task commits land)

These steps are executed by the controller, not by an implementer subagent.

- [ ] **CV1: Push to dev**

```bash
git push origin dev
```

- [ ] **CV2: Monitor CI to terminal state**

```bash
RUN_ID=$(gh run list --branch dev --limit 1 --json databaseId --jq '.[0].databaseId')
echo "Watching run $RUN_ID"
# Background poll, returns when terminal:
sleep 600 && gh run view $RUN_ID --json status,conclusion,jobs
```

Expected outcome:
- `status: completed`, `conclusion: success`
- Job count: **16** (was 17 — `lyrics-quality-report` is gone, not skipped)
- Total runtime: **< 20 min** (target ~17, allow margin)

- [ ] **CV3: Confirm the deletion is real, not skipped**

```bash
gh run view $RUN_ID --json jobs --jq '.jobs[] | select(.name | test("Lyrics Quality"))'
```

Expected: empty output (no job matches).

- [ ] **CV4: Confirm Deploy + E2E still ran on win-resolume**

```bash
gh run view $RUN_ID --json jobs --jq '.jobs[] | select(.name | test("Deploy to win-resolume|E2E Tests \\(win-resolume\\)")) | {name, conclusion}'
```

Expected: both `success`.

- [ ] **CV5: Idle-machine sanity check**

After CI finishes, ping win-resolume and confirm the production wall is in a normal state:

```
mcp__win-resolume__Ping
```

Expected: responsive, no stuck CI artifacts in `C:\Users\…\actions-runner\_work\`.

---

## Plan self-review

**Spec coverage:**
- Spec Change 1 (delete lyrics-quality-report) → Tasks 2 + 3 ✓
- Spec Change 2 (cache config) → explicitly omitted with rationale at top of plan ✓
- Spec CLAUDE.md update → Task 4 ✓

**Placeholder scan:** No TBDs, no "implement later", no "add error handling" placeholders. Each step has the literal command or file content it needs.

**Type / interface consistency:** N/A — pure deletions + one doc append; no type signatures.

**Risk that the spec/plan disagree:** plan opens with an explicit "Scope correction vs spec" block so the implementer can't be confused. The committed spec stays as-is (it documents the original investigation; the plan reflects what we actually do).

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-04-25-ci-performance.md`. Per airuleset, executing via **superpowers:subagent-driven-development** — fresh subagent per task with two-stage review.

Tasks 1–4 are sequential (each commit in order). Tasks 1, 2, 3, 4 dispatch with **haiku** (mechanical edits, no judgment). Verification CV1–CV5 are controller-only.
