---
name: ci-discipline
description: >
  Songplayer CI quality rules. Load when writing or reviewing CI workflows,
  writing tests, or assessing whether a PR is ready to merge — covers quality
  gates, integration test requirements, canonical-source regression checks,
  cross-state testing, and the quality-over-CI-green principle.
user-invocable: false
triggers:
  - CI
  - test
  - quality gate
  - merge ready
  - regression
  - integration test
  - canonical source
  - mutation
---

# Songplayer CI Quality Discipline

## Quality gate IS the deliverable — not CI green

On PRs whose goal is quality improvement, never report "merge ready" when
measurement shows a regression — even if CI is green. The measurable quality
gate IS the actual deliverable for that PR. Example: if the lyrics quality
score regresses 0.631→0.524, the PR is NOT done regardless of green CI.

## CI must match restreamer / reaperiem quality

Minimum CI gates that MUST be present (match reference projects):
- Test integrity check (ban `#[ignore]`, empty tests, `assert!(true)`,
  `continue-on-error`)
- File size limits
- Mutation testing (diff-scoped, <20-min budget)
- Coverage thresholds
- Post-deploy E2E that exercises REAL user workflows — not just API smoke tests

Post-deploy E2E must verify: NDI sources created and discoverable, OBS sources
controllable, dashboard loads and is interactive, playlist sync actually
downloads videos.

## Canonical-source regression CI — required

CI must include a job that pins a fixed song set (~5-10 songs, one per
canonical source type: description, lrclib, genius, spotify, yt_subs) and
asserts each song's `lyrics_source` label stays the same after reprocessing.
Without this gate, silent provider regressions ship and only surface when the
user wall-verifies.

Also assert line-count is within a reasonable band (description-extracted ≈ 25
lines, NOT 95 from whisperx re-segmentation).

Add this BEFORE building any new lyrics provider to lock in the contract.

## Cross-state and transition testing

Unit happy-path coverage is not enough when behavior depends on state
transitions (pipeline version bumps, cache invalidation, priority reordering).

For every mechanism gated on "current version vs row version":
- Write a test that explicitly sets a row at `version = old` and asserts
  behavior under `version = new`
- Example required test: `null_bucket_retries_failed_songs_when_pipeline_version_bumps`
- Mutation testing catches logic flaws; it does NOT catch scenarios where the
  test never called the function with the right STATE

## Integration test gap — ship-path verification

The recurring failure mode: unit tests pass (prompt builder, parser, cache
helpers, subprocess wrapper all green), but the integrated pipeline drops data
silently in production.

For any feature whose claim is "improves production behavior":
- Ship a test that ACTUALLY runs the production path against real-like data —
  not mocks of each step
- At minimum: boot a worker, point at a fixture DB + cache with representative
  songs, run one full processing cycle, assert expected DB state
- If a live Claude call is too expensive for CI: run it manually on win-resolume
  against a small fixture set and document the run in the PR body before merging
- STOP shipping "CI green" as proof of correctness — quality gates must measure
  on REAL data

## CI must not use no-sleep jobs as critical path

No CI job whose runtime is dominated by `sleep` / `Start-Sleep` / `time.sleep`.
Soak windows go into a scheduled workflow (cron), not the post-deploy critical
path. Cron-scheduled jobs do not gate dev pushes.

## Eval → CI for runner checkout race

Eval scripts running on win-resolume that subprocess into backend Python files
MUST reference a stable copy under `C:\ProgramData\SongPlayer\eval-cache\`
(not the actions-runner checkout at `C:\actions-runner\_work\songplayer\`). The
checkout gets wiped + reset mid-run by any passing CI job. Copy backends to
`eval-cache/backends/` before starting the batch loop.

## E2E must not switch disruptive OBS scenes

The post-deploy Playwright suite shares win-resolume's OBS with the LED wall.
The baseline-scene picker must prefer `sp-slow` and fall back to non-disruptive
sp-* scenes. The suite must restore the original program scene at test end.
Never leave the wall on whatever scene the last test happened to switch to.
