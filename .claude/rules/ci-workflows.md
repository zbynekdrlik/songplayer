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

**Contract:** any E2E that switches OBS scenes must wait for the switch to
actually apply, never a fixed sleep. `ObsDriver.switchScene` polls
`GetCurrentProgramScene` until it equals the target (`e2e/obs-scene-wait.ts`
`waitForProgramScene`, unit-tested in the mock suite `obs-scene-wait.spec.ts`),
then a short settle, and **throws loudly** if the program scene never applies
(a stuck transition / missing scene surfaces here, not as a mysterious
downstream failure). This is transition-duration-agnostic — a 0 ms cut or a
2 s fade both work. Do NOT "fix" scene-switch flake by bumping test timeouts
(`no-timeout-band-aids.md`) or by mutating the shared live-wall OBS config
(transition duration / studio mode).

## A Deploy-job re-run only works while the run's artifacts exist (`dist` = 1 day)

`gh run rerun --job <Deploy>` of an older run is the sanctioned way to restart
SongPlayer on the box (same build, Deploy + post-deploy E2E) — but the `dist`
artifact has `retention-days: 1`, so a re-run of a run older than a day fails at
"Download WASM frontend: Artifact not found for name: dist" BEFORE touching the
box (17.9.2026, #170 acceptance). Past that window a post-restart suite needs a
fresh push (a version bump is enough).
