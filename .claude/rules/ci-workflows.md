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
