---
paths:
  - ".github/workflows/**"
  - ".cargo/mutants.toml"
---

# CI workflows — self-hosted runner shell traps, mutation gate, event de-dup

## win-resolume self-hosted runner: bash + PowerShell shell traps

The `win-resolume` runner has **no `bash` on PATH** (`shell: bash` → "bash:
command not found") and GitHub's `bash` shell doesn't auto-find Git Bash there.
Two traps, both cost real debugging time (#149):

- **A custom `shell:` string is split on the FIRST SPACE, NOT quote-aware.** So
  `shell: '"C:\Program Files\Git\bin\bash.exe" …'` breaks — the runner takes
  `"C:\Program` as the executable and PATH-resolves garbage
  (`InvalidPath 'C:\Program Files\Python312\Scripts\"C:'`). Use the **8.3 short
  path with no spaces**:
  ```yaml
  shell: 'C:\PROGRA~1\Git\bin\bash.exe --noprofile --norc -eo pipefail {0}'
  ```
  Verified live: `Test-Path 'C:\PROGRA~1\Git\bin\bash.exe'` → True and
  awk/sed/grep/tr/date resolve under /usr/bin. (Depends on NTFS 8.3 name
  generation, which is on by default on the runner's C:.)
- **PowerShell reads `"$var:"` inside a double-quoted string as a
  drive/scope-qualified variable** (`InvalidVariableReferenceWithDrive`), so
  `"minute $m: …"` fails to PARSE the whole script. Delimit it: `"minute ${m}: …"`.

Run `actionlint` locally on any workflow you touch (`~/.local/bin/actionlint`,
config `.github/actionlint.yaml` declares the `resolume` runner label). Note: CI
has **no** actionlint gate, and a pre-existing SC2034 (`i` unused) in the
frontend-e2e mock-API wait loop makes actionlint exit 1 — that is not your diff.

## Mutation gate (`.cargo/mutants.toml` + ci.yml `mutation-testing`)

- PR gate is diff-scoped + HARD-bounded to `timeout-minutes: 20` (mutation-testing.md).
  Prebuilt binaries via `taiki-e/install-action` (`cargo-mutants@27.1.0,cargo-nextest@0.9.140`),
  `cargo mutants --in-diff pr.diff --baseline=skip --test-tool=nextest --jobs 2`.
- Profile + test tool + ALL exclusions live in `.cargo/mutants.toml` (`profile="mutants"`,
  `test_tool="nextest"`, `exclude_re=[…]`), shared with the on-demand full sweep;
  `Cargo.toml` `[profile.mutants]` (inherits test, `debug="none"`) is what lets the
  gate drop the old swap-file hacks. Exclusions are STRUCTURAL (cfg(windows)/shell-out/
  HTTP glue) or documented provably-equivalent/TIMEOUT pins — keep every rationale
  comment; a dropped exclusion resurrects a survivor and reds a future PR.
- cargo-mutants exit codes: **0**=all caught, **2**=survivors, **3**=timeouts (all
  "expected" for the full sweep), **1**=usage, **4**=baseline/build fail (real tooling
  errors). The PR gate treats a MISSED mutant as a HARD fail (never continue-on-error).
- Full-tree catch-up is `mutation-full.yml` (`workflow_dispatch` only, `/mutation-sweep`),
  survivors → ONE `test-quality` issue per run, fails only on a tooling error.

### The mutation range + shard count are PLANNED per push (`mutation-plan` job, 19.9.2026)
A single 1000-line push produced 131 mutants; 4 shards × 20 min tested ~33, the
shards were cancelled at the bound, the Gate went red AND the next push would
only have looked at its own `before..HEAD` — leaving the big range unverified
forever. So `ci.yml` now has a `mutation-plan` job (dev pushes only):
- **base** = the NEWEST ancestor (last 60 commits) whose `Mutation Testing*`
  check-runs all concluded `success` (skipped ones ignored); fallback =
  `github.event.before`. A cancelled / failed / timed-out mutation run is thereby
  re-covered by the next push automatically — never re-run an over-budget shard.
- **shards** = `ceil(mutants / 6)` clamped 4..24, fed to the matrix via
  `fromJSON(needs.mutation-plan.outputs.shards)`; the 20-min per-shard bound is
  unchanged (never raise it). Job names become `Mutation Testing (i/N)`.
- The Gate needs `mutation-plan` too (a failed plan must not read as "skipped").
A mutant that turns a loop infinite costs a 300 s TIMEOUT and fails the step
(exit 3) — shape loops so no single comparison flip can spin (`rest.is_empty()`
on a shrinking slice instead of two `offset < len` checks).

### Write new pure code so it has NO equivalent mutants (#182 lesson)
The no-compile box only learns about survivors ~15 min after the push, so shape
pure code up front:
- **Clamp with `.max()` / `.min()`, not `if a < b { a = b }`** — `<` → `<=` on
  such a clamp is a provably EQUIVALENT mutant (the assignment is a no-op when
  equal) and can never be killed; `.max()` leaves no comparison to mutate.
- **One helper per formula.** Two copies of `at + x / tempo` (start + end) let the
  copy whose result a later clamp masks survive `/` → `%`; one shared fn is
  covered by whichever call site a test pins.
- **Every `<` / `>` on a threshold needs an exact-boundary test** (gap == limit,
  fraction == line boundary), not just a far-inside / far-outside pair.
- A fn that only shells out (child process / ffmpeg) and is reachable only from
  an already-excluded orchestrator gets its own STRUCTURAL `exclude_re` line with
  a rationale naming the pure fns that carry its decisions.

## A queued job on an OFFLINE self-hosted runner blocks the branch's concurrency group
With win-resolume offline, `Deploy to win-resolume` stays `queued`, the run never
completes, and the next dev push sits `pending` with ZERO jobs —
`cancel-in-progress` and `gh run cancel` do NOT clear a run in that state
(19.9.2026). It looks like a GitHub runner backlog; it is not. Clear it with
`gh api -X POST repos/<owner>/<repo>/actions/runs/<old-run>/force-cancel`; the
pending run starts within seconds.

## push + pull_request de-dup (#124)

Shared build/test jobs run **once, on the `push` event** (`if: github.event_name ==
'push'`); only the PR-specific gates (`version-check`, `mutation-testing`,
`red-green-order`) run on `pull_request`. The required checks `Gate` / `Deploy to
win-resolume` / `E2E Tests (win-resolume)` are produced on push and satisfy the
dev→main PR by **commit-SHA match** (GitHub matches required checks by SHA, not
event) — so no branch-protection change is needed. The `gate` job runs on both
events with the invariant "no needed job may FAIL; `skipped` is acceptable" (a job
skipped on one event ran on the sibling event for the same SHA), and on the PR event
it polls the push-run `Gate` check (needs `checks: read`) to confirm it was green —
closing the hole where a shared failure reds the push Gate but is skipped-ok on the
PR Gate. `version-check` must stay `pull_request`-only (a dev push legitimately has a
`-dev` VERSION).

## RED-GREEN gate: retroactive `[no-test: <sha> <reason>]` (release PR #160)
`scripts/check-red-green-order.sh` runs on the PR event over the whole
`main..dev` range, so a `fix(#N):` commit that landed on dev without a
`[no-test:]` marker (a merge-integration compile fix, a clippy allow) fails the
release PR weeks later. History rewrite is banned — declare the LOGGED bypass
from a LATER commit instead: an empty `chore(red-green): …` commit whose body
carries one `[no-test: <sha7> <reason>]` per covered commit; the script prints
`bypass: … (declared by <sha7>)`. Only the leading `fix(#N):` form is gated;
scope-only subjects (`fix(stems): … (#14)`) are not. Run
`bash scripts/check-red-green-order.sh origin/main..HEAD` before opening a
release PR — it is bash-only, allowed under Tier-0.

## `gh run rerun <old-run>` CANCELS the newer in-flight run on the same branch
`ci.yml` has a per-branch concurrency group with `cancel-in-progress`; a re-run
of an OLDER run is a NEW run in that group, so GitHub cancels whatever is
currently in flight (2026-09-15: the mutant-fix run 34948303347 died because I
re-ran 34947398815 for its cancelled shard 1/4). Sequence instead: let the
in-flight run finish (or cancel it deliberately), THEN re-run the old one, THEN
`gh run rerun <new-run>`. Also: cancelling a run whose mutation shards had not
finished leaves that push range without a mutation verdict — re-run its failed
jobs before trusting the diff, and expect the old commit's already-known
survivors to fail again there (read only the shard you need).

## Post-deploy E2E: OBS is in Studio Mode with a 2000ms Fade — never blind-sleep after a scene switch (#170)

The `E2E Tests (win-resolume)` job restarts SongPlayer (`taskkill` + `schtasks
/run /tn SongPlayer` + `Wait-SongPlayerUp`), then the Playwright post-deploy
suite drives OBS scene switches via `e2e/obs-driver.ts`. **OBS on win-resolume
runs Studio Mode with a `Fade` transition of `2000ms`** (verify:
`obs-get-studio-mode`, `obs-get-current-transition`). In studio mode
`SetCurrentProgramScene` only *starts* the fade; obs-websocket updates
`GetCurrentProgramScene` and emits `CurrentProgramSceneChanged` — the event
SongPlayer's OBS client reacts to (in <1 ms) — **only when the fade completes,
~2 s later** (both come from OBS's `OBS_FRONTEND_EVENT_SCENE_CHANGED`, which
fires at transition end).

So a blind `sleep(300)` after `SetCurrentProgramScene` **races the 2 s fade**:
`/api/v1/status.active_playlist_ids` still shows the old scene's playlist for
~2 s, which fails a single-read assertion (test 15's baseline "ytfast NOT
active" read) and, with back-to-back scene tests, re-triggers `SelectAndPlay`
from position 0 so the position never advances (test 17's 0→0). This was the
"post-restart window flake" in #170 — NOT an engine/scene-detection bug (the
box log showed SongPlayer receiving and reacting to every switch, each ~2 s
after the OBS switch).

### Round 3 (#170): a name-only wait is NOT enough — same-scene switches DROP the next event

The round-2 fix polled `GetCurrentProgramScene == target` only. Two studio-mode
behaviours defeat that (reproduced live 3×, 17.9 02:06–02:09 UTC):

1. **A SAME-scene `SetCurrentProgramScene` still runs a real 2 s transition**,
   leaving `preview == program == that scene`. Test 17 opened with
   `switchScene(baseline)` while OBS was already on `sp-slow` → an
   `sp-slow→sp-slow` fade.
2. **From that `preview==program` state OBS DROPS the next
   `SetCurrentProgramScene`'s `CurrentProgramSceneChanged`** — `GetCurrentProgramScene`
   reports the target but no event fires, so SongPlayer (event stream alive)
   never learns of the switch; ytfast stays paused and the wall sits on a paused
   source. A real transition to another scene first (so preview becomes the
   *previous* program) restores normal behaviour.

**Contract (round 3):** `ObsDriver.switchScene`
(1) **skips when `program == target`** (`shouldSkipSceneSwitch` — never issue a
same-scene switch); (2) in Studio Mode drives the transition the studio way —
`SetCurrentPreviewScene(target)` + `TriggerStudioModeTransition` (which DOES emit
the program-scene-changed event), else `SetCurrentProgramScene` (read
`GetStudioModeEnabled` once); (3) waits until `GetCurrentProgramScene == target`
**AND the transition has ENDED** (tracked via `SceneTransition{Started,Ended}`),
then settles — `sceneSwitchSettled` / `waitForSceneSwitchApplied` in
`e2e/obs-scene-wait.ts`, unit-tested in the mock suite `obs-scene-wait.spec.ts`.
Throws loudly if the switch never settles. Transition-duration-agnostic (0 ms cut
or 2 s fade). Do NOT "fix" scene-switch flake by bumping test timeouts
(`no-timeout-band-aids.md`) or by mutating the shared live-wall OBS config.

### Round 4 (19.9.2026): wait for the PREVIEW to apply before `TriggerStudioModeTransition`
Three red post-deploy runs in one day, all `OBS scene switch to "sp-fast" did not
settle … (last program "sp-alex", transitionActive=false)`, with OBS left at
`preview = sp-fast, program = sp-alex`. Cause: `SetCurrentPreviewScene` returns
before OBS's UI thread applies it; a `TriggerStudioModeTransition` sent right
behind it can still see the OLD preview, and when that equals the program scene
OBS fades the scene to ITSELF (`SceneTransitionEnded` fires, the program never
changes). `ObsDriver.switchScene` therefore polls `GetCurrentPreviewScene ==
target` (`waitForPreviewApplied`, 3 s bound, throws "stale preview") BEFORE the
trigger. Same rule for any future studio-mode automation (Companion-style
control in the app): set preview → confirm preview → trigger.

**Engine self-heal (the production bug the harness exposed):** a dropped
`CurrentProgramSceneChanged` in daily studio-mode use is a dark wall for the
operator, not just an E2E flake. `crates/sp-server/src/obs/` now polls
`GetCurrentProgramScene` every ~2 s (`scene_poll::reconcile_program_scene`) and,
on a mismatch with the last event-derived scene
(`scene_poll::scene_poll_detects_change`), feeds the same `scene::apply_scene_change`
path the event does (INFO log `obs: program scene changed without an event —
reconciled by poll`).

**afterAll read-back + afterEach restore:** the post-deploy suite restores the
scene it started on and asserts `/api/v1/status.active_scene` (the engine's view)
followed, retrying once and failing loudly — so a dropped switch never leaves the
wall on the E2E baseline silently. `test.afterEach` restores the start scene after
every test (pass OR fail), so a failed assertion that aborts a test's own trailing
cleanup still returns the wall to the operator's scene.

**Card honesty:** a `PlaybackStateChanged`-only store entry (video_id 0, empty
song, zero duration) renders `np-idle` "Nothing playing", never a bogus
`np-info` "0:00 / 0:00" (`sp-ui` `NowPlayingInfo::has_now_playing_content`) — so
the position-advance check cannot be satisfied by an empty entry.

## A Deploy-job re-run only works while the run's artifacts exist (`dist` = 1 day)

`gh run rerun --job <Deploy>` of an older run is the sanctioned way to restart
SongPlayer on the box (same build, Deploy + post-deploy E2E) — but the `dist`
artifact has `retention-days: 1`, so a re-run of a run older than a day fails at
"Download WASM frontend: Artifact not found for name: dist" BEFORE touching the
box (17.9.2026, #170 acceptance). Past that window a post-restart suite needs a
fresh push (a version bump is enough).

## RED commit subjects: `test(#N): …` only — `test[red](#N)` is NOT parsed

`scripts/check-red-green-order.sh` (the RED-GREEN Commit Order CI job and the
pre-push gate) accepts a RED commit only when its subject matches `test(#N)` /
`test(<scope>): … (#N)`. The `test[red](#N): …` form passes unnoticed only while
an OLDER `test(#N)` commit for the same ticket sits in the same range (that is
how it slipped through the whole 0.60.0 cycle) and fails the moment the range
holds just your pair. Write `test(#N): RED — …`. If a wrongly-formed RED commit
is already pushed (history rewrite is banned), a LATER commit carrying
`[no-test: <fix-sha> RED test is <test-sha> — <reason>]` declares it.

## Mutation-timeout trap: bounded windows pop with `if`, never `while`

`while deque.len() > CAP { deque.pop_front(); }` is correct code that a
`>`→`<` mutant turns into an infinite loop on an empty deque — cargo-mutants
reports TIMEOUT (300 s), which fails the shard exactly like a MISSED mutant
(#192 r3, `loop_stats.rs::SubmitHist::observe`). When one push can overshoot
by at most one, write `if len > CAP { pop_front(); }`; for bulk trims use
`truncate`/`drain(..n)` with a `saturating_sub` count. Any loop whose exit
depends on a comparison a mutant can flip needs a structural bound.

## Post-deploy restart-skip compares the DEPLOY job's observed version (#198 item 2)

The `e2e-resolume` "Restart SongPlayer" step's `#196 item 6` skip must compare
`$status.version` against the version the DEPLOY job observed the running process
report, NOT `(Get-Content VERSION -Raw)` from the E2E job's own checkout — on a
re-run of an older run / a moved ref the checkout VERSION drifts and flips the
skip silently. The `deploy-resolume` Health-checks step (id `healthcheck`, which
already verified `$resp.version -eq VERSION`) publishes it as a job output
`deployed_version`; the E2E step reads
`${{ needs.deploy-resolume.outputs.deployed_version }}`.

**PowerShell -> `$env:GITHUB_OUTPUT`: use `Add-Content`, NEVER `Out-File -Encoding
utf8`.** Under WinPS 5.1 (`shell: powershell` on the runner) `-Encoding utf8`
writes a UTF-8 BOM, which prefixes the key (`﻿key=value`) and can blank the
output on a strict reader. `Add-Content -Path $env:GITHUB_OUTPUT -Value
"key=$($val)"` writes the ASCII line with no BOM. (The bash steps' `>> $GITHUB_OUTPUT`
have no such issue.)

