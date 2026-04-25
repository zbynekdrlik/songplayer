# CI Performance Optimization: Design

**Date:** 2026-04-25
**Target:** dev iteration time `47 min → ~9 min` per push, no quality loss on path-to-production.

## Goal

Reduce wall-clock CI time on every dev push from **47 min 46 s** (baseline: run [24931489578](https://github.com/zbynekdrlik/songplayer/actions/runs/24931489578)) to **~9 min**, by:

1. Removing one job that consumes 30 minutes and produces no actionable quality signal.
2. Adding `Swatinem/rust-cache@v2` to all Rust jobs so the workspace doesn't recompile from scratch on every push.

Deploy-to-`win-resolume` and the on-target Playwright E2E stay on every dev push — that's how the operator validates real-world behaviour during development. Nothing in this spec lowers test or E2E quality.

## Non-goals

- **Tiered CI by branch / event.** Deploy continues to run on every dev push.
- **Replacing the deleted job with a ground-truth lyrics test.** Quality is verified by ear during reprocess sessions; if a real ground-truth test is wanted later, it gets its own spec.
- **Changing Coverage threshold or Mutation Testing tier.** Both are correctly placed today and not on the critical path once cache is in.
- **Combining `build-windows` and `build-tauri`.** They serve different purposes (the former runs Windows-specific tests; the latter produces the installer). With cache, both finish well within the critical path's bottleneck.
- **Moving anything off the self-hosted `[windows, resolume]` runner.** Deploy and post-deploy E2E must touch the real machine.

## Baseline analysis

Latest successful dev run (`24931489578`, total **47:46**):

| Duration | Job | Runner | On critical path? |
|---:|---|---|:---:|
| 30:19 | Lyrics Quality Report (30-min post-deploy snapshot) | self-hosted resolume | yes |
| 9:48 | Build Tauri (Windows installer) | windows-latest | yes |
| 4:15 | Coverage (tarpaulin) | ubuntu-latest | no (parallel) |
| 4:09 | Build (Windows) | windows-latest | no (parallel) |
| 2:58 | E2E Tests (win-resolume) | self-hosted resolume | yes |
| 2:16 | Deploy to win-resolume | self-hosted resolume | yes |
| 1:41 | Build WASM (trunk) | ubuntu-latest | yes |
| 0:56 | Test (cargo test workspace) | ubuntu-latest | no (parallel) |
| 0:48 | Frontend E2E (mock-api) | ubuntu-latest | no (parallel) |
| ≤0:30 each | Lint, Test WASM, Security Audit, Test Integrity, File Size, Dev Version Check, Gate | ubuntu-latest | no |
| skipped | Mutation Testing (PR-to-main only — correct) | ubuntu-latest | n/a |
| skipped | Version Check (PR-to-main only — correct) | ubuntu-latest | n/a |

Critical path:
```
parallel ubuntu checks (≤56s)
  → Build WASM (101s)
    → Build Tauri (588s)
      → Gate (2s)
        → Deploy (136s)
          → E2E win-resolume (178s)
            → Lyrics Quality Report (1819s)   ← 63% of total wall time
TOTAL = 47:43
```

## Change 1: Delete the Lyrics Quality Report job

### What it does today

`ci.yml` lines **1485–1594**. After Deploy + E2E succeed:

1. `Start-Sleep -Seconds 1800` (30 min wait — no CPU work; gives the worker time to reprocess songs in the background).
2. Runs `measure_lyrics_quality.py --cache-dir … --out measure_after.json`, which produces three aggregate metrics:
   - `avg_confidence_mean` — average of provider-self-reported confidence
   - `duplicate_start_pct_mean` — share of word starts that collide on the same `start_ms`
   - `multi_provider_pct` — share of songs that ran an ensemble (multiple providers)
3. Compares to `baseline_before.json` (written by an earlier deploy step).
4. Soft-fails if `avg_confidence_mean` regresses by more than `0.02`. Soft-skips on any missing field, so most regressions go silently green.

### Why it goes

- **`avg_confidence_mean` is provider-self-reported.** A pipeline can hallucinate confidently. The metric reports pipeline self-trust, not lyric correctness, language correctness, or sync correctness — exactly the failure modes that have been hitting in practice (Saints / THE DEEP / WOMP WOMP timing, autosub contamination pre-v16, Gemini wordless lines pre-v15).
- **`duplicate_start_pct_mean` is already solved.** v8 → v10 drove this to ~0% across the catalog; we re-confirm a solved metric for 30 minutes per dev push.
- **No ground-truth comparison anywhere.** The job never compares against a known-correct reference.
- **Operator gut-check (2026-04-25):** "i dont have any feeling that lyrics are better".

### What we delete

- The `lyrics-quality-report:` job (lines 1485–1594).
- Any step in `deploy-resolume:` that writes `baseline_before.json` (it becomes dead code once the consumer is gone).

### What we keep

- `measure_lyrics_quality.py` stays installed on `win-resolume` as a manually-runnable tool for ad-hoc trend checks. Just no longer wired into CI.

### CLAUDE.md update

Add to `CLAUDE.md` (project-level) under a new top-level **CI architecture** section:

> **No sleep-based CI jobs.** Any job whose runtime is dominated by `sleep`/`Start-Sleep` is forbidden. If a soak window is needed for trend analysis, it goes into a scheduled workflow (cron), not the post-deploy critical path. Cron-scheduled jobs do not gate dev pushes.

## Change 2: Add `Swatinem/rust-cache@v2` to all Rust jobs

### Where

Add immediately **after** the toolchain-setup step and **before** any `cargo` invocation, in:

| Job | Runner | File location (today) |
|---|---|---|
| `lint` | ubuntu-latest | `ci.yml:64–77` |
| `test` | ubuntu-latest | `ci.yml:78–86` |
| `test-wasm` | ubuntu-latest | `ci.yml:88–98` |
| `security` | ubuntu-latest | `ci.yml:100–110` |
| `build-wasm` | ubuntu-latest | `ci.yml:112–131` |
| `build-windows` | windows-latest | `ci.yml:198–216` |
| `build-tauri` | windows-latest | `ci.yml:133–155` |
| `coverage` | ubuntu-latest | `ci.yml:410–421` |
| `mutation-testing` | ubuntu-latest (PR-to-main only) | `ci.yml:423–513` |

`security` is included for `cargo audit` registry caching. `lint` is included for the registry cache that `cargo clippy` warms.

### Configuration

Default Swatinem v2 config — keys on `Cargo.lock` hash + Rust toolchain + workflow file hash + job name. Per-platform caches are isolated automatically. Example placement:

```yaml
- uses: actions/checkout@v4
- uses: dtolnay/rust-toolchain@stable
  with:
    components: rustfmt,clippy
- uses: Swatinem/rust-cache@v2
- run: cargo fmt --all -- --check
- run: cargo clippy --workspace -- -D warnings
```

For `mutation-testing`, use a **separate cache key** because `cargo-mutants` modifies source between iterations and could in principle pollute a shared `target/`:

```yaml
- uses: Swatinem/rust-cache@v2
  with:
    shared-key: mutants
    save-if: ${{ github.event_name == 'pull_request' }}
```

For PR builds in general, write only on dev pushes (PR caches read but don't save) to stay under the 10 GB repo cache limit:

```yaml
- uses: Swatinem/rust-cache@v2
  with:
    save-if: ${{ github.ref == 'refs/heads/dev' }}
```

(This applies to all jobs except `mutation-testing`, which has its own `save-if` above.)

### Expected timings (from public benchmarks for similarly-sized Rust projects)

| Job | Today | After cache hit | Cold start (Cargo.lock change) |
|---|---:|---:|---:|
| Test | 0:56 | ~0:25 | 0:56 |
| Build WASM | 1:41 | ~0:35 | 1:41 |
| Build (Windows) | 4:09 | ~1:30 | 4:09 |
| Coverage | 4:15 | ~2:00 | 4:15 |
| Build Tauri | 9:48 | ~3:00 | 9:48 |

Build Tauri's cached time is dominated by linking — compilation is largely cached but the final link step is full-cost.

## Expected critical path after both changes

```
parallel ubuntu checks (≤25s)
  → Build WASM (~35s)
    → Build Tauri (~3:00)
      → Gate (2s)
        → Deploy (2:16)
          → E2E win-resolume (2:58)
TOTAL ≈ 8:50
```

Cold start (after a `Cargo.lock`-changing PR merges to main and caches reset on next dev push): ≈ 17 min — same as Change-1-only steady state.

## Risks and mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| First push after `Cargo.lock` change → cache miss → full rebuild | every dep bump (~weekly) | one ~17-min push | Acceptable. Cold cost = current Change-1-only state. |
| Stale cache from out-of-band toolchain change (e.g., `rust-toolchain.toml` edit, env var change Swatinem doesn't see) | rare | confusing build failures | Swatinem keys on workflow file hash, so any workflow edit busts the cache. If a non-workflow change requires a bust, add a no-op env var to the affected job. |
| 10 GB cache limit exceeded → eviction churn | low (typical Rust+Tauri repo: ~2–5 GB cached) | colder starts on some jobs | `save-if: refs/heads/dev` keeps PR caches read-only, avoiding double-storage |
| `cargo-mutants` cache pollution | possible | flaky mutation runs | Separate `shared-key: mutants` cache scope |
| Caches evicted after 7 idle days | low (active project) | one slow push after a vacation | Acceptable |
| Operator loses lyrics-quality trend visibility | certain | by-ear is subjective | The deleted metric was self-reported confidence — already not a real signal. `measure_lyrics_quality.py` stays as an ad-hoc tool. |

## Verification

After landing both changes on a dev push:

1. **Total runtime < 12 min** (target ~9). Read from the GitHub Actions UI.
2. **All jobs green.** Same job set as today minus `Lyrics Quality Report`.
3. **Lyrics Quality Report no longer in the job list** (and not just skipped — fully removed).
4. **Cache visible** in `Actions → Caches`. Expect 5–9 entries (one per cached job × per platform).

Then push a trivial follow-up (e.g., a whitespace-only commit):

5. **Build Tauri ≤ 4 min** on the second push (cache hit confirmation).
6. **Build (Windows) ≤ 2 min** (cache hit confirmation).

## Out of scope (recorded for future)

- A real ground-truth lyrics test (Playwright plays a hand-verified song on `win-resolume` and asserts highlighter line at known timestamps). Worthwhile but separate spec.
- Cron-scheduled nightly run of `measure_lyrics_quality.py` for trend tracking. Easy to add later if anyone wants the trend back.
- Reviewing whether `Coverage` still earns its 4-min slot at 40% threshold. Not on critical path with cache; defer.
