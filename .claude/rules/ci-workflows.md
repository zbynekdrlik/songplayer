---
paths:
  - ".github/workflows/**"
  - ".cargo/mutants.toml"
  - "scripts/rig_lease_gate.py"
  - "scripts/tests/test_rig_lease_gate.py"
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

- **`shell: powershell` (Windows PowerShell 5.1) reads the step script as cp1252, so
  keep every code line ASCII** (26.9.2026, E2E job 108408085281). An em dash inside
  `"..."` decodes to bytes ending in 0x94, a curly quote PowerShell closes the string
  on (`A positional parameter cannot be found`). An accented literal in a here-string
  (`'odpojené'`) reaches the written spec as mojibake and silently never matches.
  Comments are fine. `scripts/check_workflow_ps_ascii.py` (run by the eval-checks
  pytest via `scripts/tests/test_check_workflow_ps_ascii.py`) fails CI on a violation.
- **Windows PowerShell 5.1 array traps** (release 0.71.0 review). (1)
  `Invoke-RestMethod` emits a JSON array as ONE pipeline object, so
  `Invoke-RestMethod … | Where-Object` sees the whole array once: store it in
  a variable and `foreach` over it. (2) A single `[pscustomobject]` has no
  `.Count` (it reads `$null`, fixed only in PowerShell 6+), so
  `($rows | Where-Object …).Count -gt 0` is FALSE for exactly one match: wrap
  every filter whose count you test in `@(…)`.
- **Relaunching SongPlayer after a kill: end the scheduled-task INSTANCE first**
  (`Stop-ScheduledTask -TaskName SongPlayer`, then wait until the state is no longer
  `Running`). The task is `MultipleInstances=IgnoreNew`; after a bare
  `taskkill /F`, Task Scheduler can still report it Running, and `schtasks /run`
  then prints "currently running" and does nothing.

Run `actionlint` locally on any workflow you touch (`~/.local/bin/actionlint`,
config `.github/actionlint.yaml` declares the `resolume` runner label and PP's
`resolume-pp`, #229). Note: CI
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
- **A `file.rs:LINE:COL` pin goes stale SILENTLY** when lines above it move: it
  then excludes nothing (or the wrong operator), and nothing fails until that
  file is in some PR's diff. #221 lane 3 found four pins (`pacer.rs`,
  `lock_state.rs`) already wrong at base. After ANY edit above a pinned line, or
  when a pinned file is in the diff, re-check each pin: `sed -n LINEp` the file,
  confirm column COL is the operator the comment names inside the named
  function, and keep the comment's line numbers in step. A doc-only edit in a
  pinned file should stay line-neutral.
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
- **shards** = `ceil(mutants / 4)` clamped 4..64 (`/ 6` until 9.10.2026: with
  #233's resampler tests in the suite a mutant killed late in the test order
  costs ~4.5 min, build ~1.5-3.5 min + ~2.7 min of tests, two at a time, and
  shard 19 of run 37924943623 ran its six past 20 min; was 24 until 26.9.2026: 335 mutants → 14 per shard ≈ 18 min, one attempt cancelled at the 20-min bound; shards beyond the runner concurrency just queue, and the per-job bound counts from job start), fed to the matrix via
  `fromJSON(needs.mutation-plan.outputs.shards)`; the 20-min per-shard bound is
  unchanged (never raise it). Job names become `Mutation Testing (i/N)`.
- **sharding = round-robin** (`--sharding round-robin` on the shard's list AND
  run, pinned by `scripts/tests/test_ci_mutation_sharding.py`): mutant i on
  shard i % N. cargo-mutants' default `slice` hands each shard consecutive
  mutants, and `--in-diff` lists them file by file, so a push with 351 cheap
  sp-core mutants and 109 sp-server ones (~5 min each: build + the slow suite)
  put eight sp-server mutants in each of shards 44-57 — 50-56 were cancelled at
  the 20-min bound while the sp-core shards took 0.8 min (#242, CI run
  37956682432). The per-shard cost is set by the slow crate's share, not the
  count.
- The Gate needs `mutation-plan` too (a failed plan must not read as "skipped").
A mutant that turns a loop infinite costs a 300 s TIMEOUT and fails the step
(exit 3) — shape loops so no single comparison flip can spin (`rest.is_empty()`
on a shrinking slice instead of two `offset < len` checks).

### Write new pure code so it has NO equivalent mutants (#182 lesson)
The no-compile box only learns about survivors ~15 min after the push, so shape
pure code up front. cargo-mutants' binary-operator table (its book,
`mutants.md`):

- `<` → `==`, `>` (and `<=`, below)
- `>` → `==`, `<` (and `>=`, below)
- `<=` → `>`
- `>=` → `<` ONLY
- `==` ↔ `!=`, `&&` ↔ `||`
- `&` → `|`, `^`

So a cap guard written `if len >= CAP { return }` has a single mutant, `<`,
and a monotonic counter leaves it no `==` equivalent (#213 `note_unsupported`).

**The table is not the whole list (#223 S1a).** The box's cargo-mutants 27.0
`--list` also turned `x > MAX` into `x >= MAX` and `stride < row` into
`stride <= row`, so every strict comparison needs an exact-boundary test
(`a_side_over_the_texture_limit_is_refused` takes exactly 16384 on BOTH
axes; one axis alone left the other's `>=` mutant unkilled). It also lists
`delete -` on a negative float literal inside a `const` table
(`sp-gpu` `BT709_LIMITED_TO_FULL`): pin every entry's value, not only a few.
List the diff's mutants (`rust-workspace.md`) instead of trusting the table.
- **Clamp with `.max()` / `.min()`, not `if a < b { a = b }`** — `<` → `<=` on
  such a clamp is a provably EQUIVALENT mutant (the assignment is a no-op when
  equal) and can never be killed; `.max()` leaves no comparison to mutate.
- **One helper per formula.** Two copies of `at + x / tempo` (start + end) let the
  copy whose result a later clamp masks survive `/` → `%`; one shared fn is
  covered by whichever call site a test pins.
- **Every `<` / `>` on a threshold needs an exact-boundary test** (gap == limit,
  fraction == line boundary), not just a far-inside / far-outside pair.
- **No `|` of disjoint flag bits ANYWHERE in non-test code** (#147 r9): `A | B` →
  `A ^ B` is equivalent when the bits don't overlap. cargo-mutants DOES mutate
  `const` initialisers too (dev run 35996650748: 4 MISSED on
  `const X: u32 = A | B;`). Write each flag word as a LITERAL (`0x2110`), pin
  its composition with a test (`assert_eq!(BASE, A | B | C)` — tests are never
  mutated), and have the fn only pick one (`if cap == 0 { BASE } else { … }`).
  The bit mirrors then read only in `cfg(windows)` code need
  `#[cfg_attr(not(windows), allow(dead_code))]` or the Linux clippy fails.
- **No redundant guard after an earlier arm** (#147 r9): `Some(0) => 0,
  Some(v) if v > 0 => …` makes `>` → `>=` equivalent (0 is caught first). Parse
  as `u64` so a negative simply fails to parse, and drop the guard.
- **A match guard that now GATES a side effect needs a test that observes it**
  (#147 r10): `Ok(mut g) if !g.busy => g.spare.take()…` makes the mutant
  `replace match guard !g.busy with true` observable ONLY through the stolen
  `spare`. The old busy test, which asserted only "nothing pending", let the
  mutant survive, because a later re-check hid it. Seed the side-effect state
  (a `spare`), trip the guard, and assert that the state is untouched.
- **Never re-check a condition an earlier one already implies (#147 backward
  slice).** In `decide_anchor_step`, an armed step is over 1 ms, and a read
  that is within ±1 ms of it and itself over 1 ms already has the same sign.
  An extra `p.direction == StepDirection::of(delta)` check would be dead
  logic. Keep that proof in the doc comment instead. For the same reason,
  classify the sign of a value that can never be 0 with `v.is_positive()`
  (no operator to mutate), not `v > 0`: there `>` → `>=` is an equivalent
  mutant.
- **No trivial `const fn new()` next to `#[derive(Default)]`**: its body can be
  swapped for `Default::default()` with no observable change. Seed a `static`
  with a struct literal in the same module and use `Default` in tests.
- **Test sizes must break `*` vs `+` (#212):** 2×2 makes `w * h == w + h`
  (and `2 * 2 == 2 + 2`), so a `*`→`+` mutant on a size/length formula is
  EQUIVALENT under that fixture. Use non-square, non-2 dimensions (2×4, 4×2,
  a 2×3 silence block) wherever a test pins a computed length.
- **Don't split one decision into `Arm if cond => …` + `Arm => return` (#212):**
  cargo-mutants rewrites a match guard to `true`/`false`. When the fallback arm's
  effect is unobservable (e.g. an offer the bus would reject anyway), the
  guard→`true` mutant survives. Keep ONE arm with `if !cond { return; }` inside:
  its only mutant is `delete !`, which the positive case kills.
- **A pure capacity (`frame_pool::take(len)`) is invisible to output tests** —
  a wrong `len` just reallocates. Pin it through `frame_pool::pool_len(CAP)`
  on a unique size class after every holder dropped (ndi_input's 50×34 test).
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

## `Build WASM (trunk)` red with "error downloading archive file: 504" = trunk's wasm-bindgen fetch, not our code (21.9.2026)

trunk downloads `wasm-bindgen` from the OLD `rustwasm/wasm-bindgen` release
URL, which now only 301-redirects to `wasm-bindgen/wasm-bindgen`; GitHub's
release edge answered that redirect with 504 on two consecutive runs
(35614160178 + its `--failed` re-run) while the same asset downloaded fine from
dev1. A second failure of the same shape is NOT a transient to re-run again —
`ci.yml` now pre-seeds the exact locked version (parsed from
`sp-ui/Cargo.lock`, never hard-coded) onto `$HOME/.cargo/bin` (trunk uses a
matching PATH binary before downloading) and into `~/.cache/trunk/wasm-bindgen-<v>/`
with a 6-attempt retrying `curl` from the new org. If the step itself fails,
check the new-org URL for that version from dev1 first (`curl -sIL …`), then
whether the lock's `wasm-bindgen` version changed.

## NEVER delete `songplayer.db-wal` / `-shm` in a deploy or restart step (#184 round A, 21.9.2026)

`db/mod.rs::pool_tuning()` runs SQLite in **WAL mode** since #184 round A. In WAL
mode every committed write lives in `songplayer.db-wal` until a checkpoint
(auto at ~1000 pages / graceful close); SQLite replays it on the next open. The
deploy/E2E restart step used to `Remove-Item songplayer.db-wal / -shm` after
`taskkill` — a rollback-journal-era leftover that was harmless before and, under
WAL, **silently reverted the database on every restart** (a dub that reached
`ready` at 16:06Z was back in `synth` after the 16:11Z restart; the E2E "a READY
dub is listed" then failed twice). A `taskkill /F` needs NO cleanup: the WAL is
on disk and recovered; the `-shm` is recreated. If a step ever needs a compact
DB, call `PRAGMA wal_checkpoint(TRUNCATE)` through the running app — never
delete the files.

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
The poll is bounded at ~45 min (90 × 30 s): the push Gate waits for Build Tauri
AND every mutation shard (20-min bound each, up to 64 shards). The old ~15 min bound
timed out on release PRs #211/#214 while the push Gate later went green. If it still
times out, re-run ONLY the failed PR Gate job once the push Gate is green
(`gh run rerun <pr-run> --failed`).

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

**Never cancel a run once its `Deploy to win-resolume` job has started**
(29.9.2026 03:15Z, run `36514244587`). The deploy stops SongPlayer BEFORE
it installs, and a cancel that lands mid-job leaves the new build installed
but SongPlayer STOPPED (it was down ~45 s until a manual
`Start-ScheduledTask -TaskName SongPlayer`). Since #221 L4a the deploy job
waits out another repo's rig lease ITSELF, before anything stops SongPlayer
(next section), so there is no reason to cancel a run for a lease any more.
If a run must still be cancelled, do it before the deploy job starts, or
while it is still in its "Wait for the rig lease" step (nothing is stopped
yet). Later re-run the cancelled jobs with `gh run rerun <id> --failed`
(cancelled jobs count), which reuses the finished build. If a deploy was cut
anyway, check `Get-Process SongPlayer` and start the task.

**The box jobs use `!cancelled()`, never `always()` (29.9.2026, run
`36553367664`).** `gate`, `deploy-resolume` and `e2e-resolume` need a
status-check function so a SKIPPED dependency (`version-check` on a push)
does not skip them. `always()` does that, but it is also TRUE on a cancelled
run: a run cancelled at 10:21Z with `Gate` queued still ran `Gate`, started
the deploy, waited in the lease step (a second cancel did not stop it
either) and deployed at 10:31Z. So "produkcia beží → zruš CI" never stopped
a deploy or an E2E. `!cancelled()` keeps the skipped-dependency behaviour
and is false once the run is cancelled; the Test Integrity Check pins it for
all three jobs and rejects `always()` there. A step-level `if: always()`
(artifact uploads) is harmless and stays. Never cancel a run for a held rig
lease: the deploy waits for it itself (above). The PP deploy
(`deploy-pp.yml`, #229, `peer-exchange.md` "PP deploy") takes the other
side of the same lesson: its "Start SongPlayer" step is `if: always()`, so
a cancel after the stop still brings SongPlayer back.

## The deploy waits for the rig lease (#221 L4a, `scripts/rig_lease_gate.py`)

Other repos (camera-box's full-path E2E, the A/V soak) hold a cross-repo rig
lease on dev1 while they drive the shared rig. "Deploy to win-resolume" runs
"Wait for the rig lease" right after checkout, before "Deploy SongPlayer"
stops SongPlayer:

- `GET /rig-lease.json` (camera-box `scripts/rig-lease-server.py`, :8890;
  `held`, `stale`, `holder.repo`, `expected_release_at`, `ttl_s`), trying
  `http://10.77.9.200:8890` (dev1's LAN address, 5-15 ms from win-resolume,
  29.9.2026) and then `http://dev1:8890` (the mDNS name `dev1.local`: it
  survives a LAN IP change, but one cold lookup from the box's Python failed
  with `getaddrinfo failed`). win-resolume has NO tailscale, so the
  tailscale address times out, and camera-box's documented `10.77.9.103` is
  stale (dev1's LAN IP moved). If dev1's LAN IP moves again, the name keeps
  the gate working; update the first URL.
- Held, not stale, by another repo (a held lease with no readable holder
  counts as another repo's): wait, one line per 30 s poll naming the holder,
  its run and `expected_release_at`; at most 60 min, then exit 3 and the job
  fails with its own message ("never deploy over another repo's lease").
  Free, stale, or held by `${{ github.repository }}`: deploy. A lease
  service that does not answer with a lease (dev1 down, something else on
  the port; a `holder` that is not an object or null counts as no lease):
  a `::warning::` annotation and the deploy goes on — an outage never
  blocks a deploy. Right after another repo's live lease was read, one or
  two unreachable reads are a blip (a cold mDNS lookup, a lease-server
  restart) and the lease still counts as held; the third in a row
  (`OUTAGE_READS`) is the outage (review round 1: one blip used to deploy
  into the lease just seen held). Any other non-zero exit is the gate's own
  failure and fails the job.
- Residual: the gate reads the lease, it does not HOLD it. A lease another
  repo takes in the ~1 min between the check and "Deploy SongPlayer"
  (artifact downloads) is not seen. The E2E job runs the same gate again
  twice: right after its checkout, before its first step that acts on the
  box (release 0.71.0 review: starting OBS, the restart, the title check's
  program cut, the Presenter "[CI PROBE]"), and right before
  "Feature-level Playwright (post-deploy spec)" (camera-box's request,
  6.10.2026). The suite switches cg OBS's program for the A/V
  gate's probe scene and presses scenes through the facade, and minutes
  pass between the deploy's wait and the take.
- Hardening (review rounds 3-4): a body over 64 KiB (`MAX_BODY_BYTES`; a
  lease is ~400 B; the bound is pinned exactly), JSON nested past the
  recursion limit, bad UTF-8, a non-HTTP listener, a body shorter than its
  Content-Length (`read(amt)` returns what came: it does not parse — no
  `IncompleteRead` there; a chunked body cut short IS one) are all "no
  lease" from that URL. `main` refuses a `--url` that is not http(s) with a
  host and a valid port (`is_http_url`; a broken one would otherwise read as
  an outage on every deploy). Everything the gate logs from the lease port
  (`describe_holder`, a not-a-lease answer, an exception text) goes through
  `one_line`: a line break would start a runner workflow command. The
  "Wait for the rig lease" STEP has `timeout-minutes: 70`, so a lease server
  that trickles bytes under the per-read timeout is cut in that step, never
  inside "Deploy SongPlayer" (which stops SongPlayer first); the job keeps
  `timeout-minutes: 90`.
- test-integrity's "deploy job uses always()" check greps
  `'^  deploy-resolume:'` (anchored): unanchored it matched its own line,
  which holds the job name and "always()", and never read the real header.
- It runs on the box's `C:\Program Files\Python312\python.exe` (the step
  fails if it is missing; the A/V gate needs the same Python). Stdlib only.
- The wait is coordination, not a soak (CLAUDE.md "No sleep-based CI
  jobs"): a free lease costs one GET. A newer push cancels a waiting deploy
  via the concurrency group — safe, nothing is stopped yet.
- Tests: `scripts/tests/test_rig_lease_gate.py` (Eval Checks pytest; the
  script is in both ruff lists): the decision, the bounded wait on a fake
  clock (120 × 30 s then exit 3; a last short pause ends exactly at the
  bound), the fetch over a real local HTTP server. Python runs locally on
  the Tier-0 box, so its RED/GREEN is really run before the push.

**The E2E job's "Verify Resolume title delivery" cuts SP-program** (release
0.71.0 review). Since #221 lane 2 only SP-program's playlist writes the
title and the lines (`may_write_wall`), and with "OBS manuál" (-1) on
program nobody does, so playing a playlist off program proves nothing. The
step picks the test playlist by the baseline discipline
(`e2e/obs-baseline-scene.ts`: an active `sp-slow` playlist with videos, else
any `sp-*` but `sp-fast` and `sp-warmup`, the sync tone), skipping any
playlist listed in `GET /api/v1/program` → `cut_refused` (#221: the cut
would answer 409; the step's own filter, not the helper's; and it fails
before any cut when SP-program's own source is listed there, since the cut
back would be refused), cuts SP-program to
it with `POST /api/v1/program/cut`, plays, reads Arena's composition, and in
a `finally` cuts back to the old source with NO pause (the test playlist
leaves program and pauses itself after the fade, so the program never
carries its frozen, silent picture). It pauses the test playlist only with
no source to give back, when the cut back is refused (then the step fails),
or when it was the source but not playing (played, then paused again). When
the test
playlist already IS the program's source and playing (its `/api/v1/ndi/health`
row reads `Playing`), it is read in place: no cut, no play, no pause, so the
live program is never paused (review round 3). An empty title is read again
once a second, at most 10 more times: a song shows no title in its last
3.5 s and first 1.5 s. Never let it pick `sp-warmup` or `sp-fast`: it is on
the live wall and FOH for ~20 s. The cut back re-kicks the old source: a
playlist starts its NEXT song, whether it was playing or paused (the same
as the post-deploy suite's `afterAll` restore); a box whose program never
had a source keeps the test playlist (no "nothing" to cut back to). The
step before it, "Verify playback updates OBS text source", plays and pauses
a playlist that is NOT the program's source, for the same reason.

**Two push runs for ONE commit: never cancel either by hand** (28.9.2026,
`36494106201` + `36494106433`). The concurrency group already cancels the
older one. `gh run cancel` on the queued survivor still lands, even when the
run keeps showing `queued` for a minute, and then NO run is left for the
commit. Leave a duplicate alone. If both end cancelled, re-run the survivor
in full (`gh run rerun <id>`, not `--failed`).

## Post-deploy E2E: OBS is in Studio Mode with a 2000ms Fade — never blind-sleep after a scene switch (#170)

**#221 L3 (read this first).** The E2E scene driver no longer talks to cg
OBS: `ObsDriver` connects to SongPlayer's obs-websocket facade
(`FACADE_WS_URL`, :4456). cg OBS :4455 serves only the A/V gate: its
recording and profile read, its probe scene (#221 lane 3) and the wait for
the probe's audio meter (#221 dev.18; see `obs-ndi-health.md`). The contract is in `remote-control.md` ("Program feedback")
and the driver's own doc:
- the transition is SP-program's (the Settings fade, e.g. 300 ms, or a Cut
  that ends at once), announced by SongPlayer's `SceneTransitionStarted` /
  `SceneTransitionEnded`, not cg OBS's 2 s fade; the driver raises its
  transition flag BEFORE the trigger;
- a switch to the scene already on program is ALWAYS sent (the facade's
  re-kick); the round-3 skip below is gone (review round 1 of the L3 lane);
- since L4b `active_scene` / `active_playlist_ids` are SongPlayer's own
  program (the resolver + the on-air set, SP-program's playlist alone since
  B4 step 6); the playback authority applies a switch a moment AFTER the
  driver returns: wait for the engine (`waitEngineActiveScene`), never read
  once. Since B4 step 6 a playlist press never switches cg OBS (no mirror).

The rest of this section is the #170 history of the cg OBS driver.

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

**Contract (round 3, superseded by #221 L3 above for (1)):** `ObsDriver.switchScene`
(1) skipped when `program == target` (removed: on the facade a same-scene
switch is the designed re-kick); (2) in Studio Mode drives the transition the studio way —
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

**Engine self-heal (history):** a dropped `CurrentProgramSceneChanged` in daily
studio-mode use was a dark wall for the operator while playback followed cg
OBS's program, so the OBS client polled `GetCurrentProgramScene` every ~2 s
(`obs/scene_poll.rs`). #221 L4b moved the playback authority to SongPlayer's own
program and L6 deleted that poll with the rest of cg OBS's scene detection
(`obs-ndi-health.md`): nothing depends on cg OBS's program events any more.

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

## A Deploy-job re-run only works while the run's artifacts exist (`dist` = 5 days)

`gh run rerun --job <Deploy>` of an older run is the sanctioned way to restart
SongPlayer on the box (same build, Deploy + post-deploy E2E) — but the `dist`
artifact is kept only `retention-days: 5` (1 until #229, which raised it for
the PP deploy: a job queued on an offline PP runner fails after 24 h and its
redo needs the run's artifacts), so a re-run of an older run fails at
"Download WASM frontend: Artifact not found for name: dist" BEFORE touching the
box (17.9.2026, #170 acceptance, then with 1 day). Past that window a
post-restart suite needs a fresh push (a version bump is enough).

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

**Per-test bound (`.config/nextest.toml`, 8.10.2026):** nextest (the mutation
gate's test tool) ends any test after 3 x 60 s and reports it FAILED, so a
mutant that makes a test wait forever counts as caught instead of a 300 s
TIMEOUT that fails the shard. It is no excuse for unbounded waits in tests:
shape them as below, and keep healthy tests far under the bound.

**A caught mutant's run ends at its first failure (`fail-fast = { max-fail =
1, terminate = "immediate" }`, #223 follow-up, 9.10.2026).** nextest's
default `wait` stops scheduling but waits for the tests already running: a
mutant killed only by a test late in the order (index 2798 of 3883) sat
behind `asio_out::tests_clock::a_driver_silent_for_5_min_runs_by_itself_once_it_ticks`
(95-160 s under the `mutants` profile, two mutants in parallel) until the
run passed 300 s: TIMEOUT, a red shard for a caught mutant (CI run
37922642659). Read the per-mutant log in the shard's `mutants-report-shard-N`
artifact (`gh run download <run> -n mutants-report-shard-N`): a `FAIL` of the
killing test followed by `SIGTERM` of another test is this case, not a hang.


`while deque.len() > CAP { deque.pop_front(); }` is correct code that a
`>`→`<` mutant turns into an infinite loop on an empty deque — cargo-mutants
reports TIMEOUT (300 s), which fails the shard exactly like a MISSED mutant
(#192 r3, `loop_stats.rs::SubmitHist::observe`, deleted by #221 lane 3). When one push can overshoot
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

## Restarting SongPlayer via the Deploy job: `gh run rerun --job <job-id>` (no run id)

`gh run rerun <run-id> --job <id>` is rejected ("specify only one of <run-id> or
--job") and prints the usage text — a `| tail -1` swallowed that and box test 7
sampled 30 min with the flag still OFF (21.9.2026). The working form is
`gh run rerun --job <job-id>` alone; it creates run ATTEMPT 2 whose Deploy job
has a NEW job id, so poll `gh api repos/<r>/actions/runs/<run>/jobs?filter=latest`
(or `jobs/<new-id>`) — polling the old id reports the old attempt's success.
On a MAIN run that is still main's tip, the completed re-run also
re-deploys PP (`deploy-pp.yml` fires on every completed attempt, #229,
`peer-exchange.md` "PP deploy"); an older main run's re-run does not reach
PP. Restart SNV from a dev run when PP must stay untouched.
Confirm the restart with `/api/v1/status` `uptime_s` before sampling anything
that depends on a startup-read setting (e.g. `sp_min_working_set_mb`;
the `genlock_pacing` setting this was written for is deleted, #221 lane 3).

