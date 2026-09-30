---
paths:
  - "crates/**/*.rs"
  - "Cargo.toml"
---

# Rust workspace — the 1000-line cap and the cargo-fmt reorder trap

## Hard 1000-line-per-`.rs` CI cap
`ci.yml` has a step "Check no .rs file exceeds 1000 lines" that fails the build
for ANY `.rs` over 1000 lines (`crates` / `src-tauri` / `sp-ui`). Several files
sit near it deliberately (`worker.rs`, `obs/mod.rs`). Before adding to a large
file, `wc -l` it; if your change would push it over, DON'T inline — follow the
established split patterns:
- **Impl split:** put new methods in a sibling module with an `impl <Type>`
  block (e.g. `lyrics/worker_outcome.rs`, `worker_reference.rs`,
  `lyrics/idle_gate.rs`, `obs/output_state.rs`) and `pub(crate)` the fields the
  seam needs. worker.rs stays under the cap this way.
- **Test split:** move a `#[cfg(test)] mod` out to `<file>_tests.rs` and include
  it via `#[path = "<file>_tests.rs"] #[cfg(test)] mod tests;`.

## A wider `let` binding in a file AT the 1000-line cap can push it over via rustfmt (#198 item 8)

Changing a call site in a file already at exactly 1000 lines is only safe if it
adds NO line — and a wider `let` pattern can force rustfmt to re-wrap the block
and ADD one, which the no-compile box only learns at the CI cap check. `pipeline.rs`
(1000/1000): widening `let (ndi_audio, audio_us) = …timed(|| …)` to
`let ((ndi_audio, ring_depth), audio_us) = …` pushed the `let` line past ~100
cols, and rustfmt re-indented the whole closure body (a diff, and a risk of an
extra line). Fix: keep the binding SHORT — bind the tuple once
(`let (audio_out, audio_us) = …`) and use `audio_out.0` / `audio_out.1` at the use
sites, instead of destructuring in the `let`. Verify with
`awk 'NR==<line>{print length($0)}'` + `cargo fmt --all --check` (both Tier-0-allowed)
and re-`wc -l` before committing.

## rustfmt breaks a `foo(long::path::bar(arg()))` nested call into 3 lines even under ~94 cols (#207)

Adding a ONE-line statement to a file at the 1000-line cap can still trip the cap:
rustfmt's `fn_call_width` heuristic (60 = 60 % of max_width) breaks a call whose
single argument is ITSELF a call and whose args exceed ~60 cols — even when the
whole line is well under 100. `lib.rs` (999→cap): a hand-written
`tokio::spawn(crate::lyrics::host_commit::run_host_commit_logger(shutdown_tx.subscribe()));`
(94 cols) was reflowed by `cargo fmt` into a 3-line form (`tokio::spawn(\n  arg,\n));`),
silently taking the file to 1002 — the no-compile box only sees it via `wc -l` /
the CI cap check, NOT `cargo fmt --all --check` alone (which passes on EITHER form,
it just prefers the 3-line one). So near the cap: after writing any nested-call
statement, run `cargo fmt --all` FIRST, then `wc -l`, and budget for the form
rustfmt actually produces (a trailing `// comment` on the closing `));` line keeps
the doc without a separate comment line). A local receiver binding (`let x = …;`
then `spawn(fn(x))`) does NOT help — the outer arg is still a call over budget.

## A method chain wider than 60 cols is split into one line per call, even with `.await` (#209)

rustfmt's `chain_width` (60) splits `engine.start_program_output(program_bus, shutdown_tx.subscribe()).await;`
(73 cols of chain) into 3 lines (`engine` / `.start_program_output(..)` / `.await;`). A trailing
`// comment` that pushes the line past 100 forces the same split. In `lib.rs` at 1000/1000 the fix was a
shorter chain, not a `let` receiver: a short method name + a borrowed arg the callee subscribes itself
(`engine.start_program(program_bus, &shutdown_tx).await;` = 53 cols, one line).

## Line-neutral "handle sub-case, else fall through" in a file AT the cap: a match-guard arm (#207)

To add a new branch to an existing `match` in a file at 1000/1000 with the fewest
lines, put a GUARD arm BEFORE the catch-all and leave the original arm byte-identical:

```rust
Err(e) if super::helper::note_if_x(&e, id, &mut acc) => {}  // +1 line, empty body = fall-through/continue
Err(e) => { /* unchanged original abort/return arm */ }
```

The guard runs a side-effecting helper (classify + count + rate-limited WARN)
returning `bool`; `true` → empty arm → the loop continues, `false` → the next arm
runs. `&e` in the guard is `&DecoderError` (not `&&`), and a `&mut` borrow of an
OUTER local (not the scrutinee) in a guard is legal. Costs +1 line; offset it by
reclaiming one comment/blank line so the file stays ≤1000. Keep the guard line
≤100 cols (a `super::` path is shorter than `crate::playback::…`).

## `cargo fmt --all` reorders `crates/sp-server/src/db/models.rs` — REVERT it
The box's local rustfmt is OLDER than CI's `dtolnay/rust-toolchain@stable`, and
the two disagree on `reorder_modules` for `models.rs`'s no-blank-line `#[path]
mod` block (`b910ccf` deliberately dropped the blanks to save lines, in a
non-alphabetical order CI's rustfmt accepts). So running `cargo fmt --all`
locally rewrites those `mod` lines into a different order — a FALSE positive that
CI does NOT want. After any `cargo fmt --all`, `git checkout --
crates/sp-server/src/db/models.rs` if you didn't intend to touch it, and confirm
`cargo fmt --all --check` then flags ONLY models.rs (ignore that one). Never let
the models.rs reorder ride in an unrelated diff — CI's newer rustfmt would fail
`--check` on it. (TIER-0: `cargo fmt` is the only local cargo command allowed;
CI compiles everything else.)

## sp-ui / src-tauri are OUTSIDE the workspace
Root `cargo fmt --all` / `clippy` do NOT touch `sp-ui/` or `src-tauri/`, and CI
has no fmt/clippy step for them (only `trunk build`). Never blanket-`cargo fmt`
sp-ui (it rewrites long-drifted files) — see `.claude/rules/sp-ui-frontend.md`.

## RED commit on the TIER-0 (no local compile) box
You can't run tests locally, so a RED test must FAIL against a version of the
code you can only reason about, without leaving warnings CI's clippy
(`--all-targets -D warnings`) would reject. The clean pattern (used for #161's
`idle_gate_abort`): make the RED commit ship the REAL logic but with ONE WRONG
CONSTANT (e.g. `const ABORT_CONSECUTIVE_BUSY: u32 = u32::MAX;` → GREEN sets `2`;
#161 shipped `u32::MAX` there, but do not copy that value, see the extreme-constant
note below).
The logic still reads/writes every field, so nothing is dead_code, and the
"must-abort" tests fail cleanly; a stub function body that ignores its fields
would instead trip `dead_code`/`unused`. For timing tests (interval + sleep),
use `#[tokio::test(start_paused = true)]` (tokio `test-util` is a dev-dep) so the
1 s poll and a 30 s mock future advance deterministically with no real wait.

Pick the WRONG constant so it is NOT an arithmetic identity: shipping a RED
`produced_ms() = count * 1` (GREEN `* 500`) trips `clippy::identity_op` under
`-D warnings` (the Lint job the no-compile box can't see), reddening the RED
commit itself. Use a wrong NON-identity value (a named const `FRAGMENT_MS = 250`
→ GREEN `500`, or a different literal) so the exact-value test still fails but
the RED tree is clippy-clean.

Likewise avoid a RED constant at an unsigned type's minimum (`0`) or maximum
(`u32::MAX`) when that makes one side of the comparison impossible. The
examples below take `x` as unsigned; for a signed type the minimum is
`iN::MIN`, not `0`.
`clippy::absurd_extreme_comparisons` flags three shapes, and as a
correctness-group lint it is deny-by-default:

- always false: `x < 0` and `x > MAX`;
- always true: `x >= 0` and `x <= MAX`;
- really `==`: `x <= 0` and `x >= MAX`.

`x > 0` or `x < MAX` does not trigger it. The #161 example above, `u32::MAX`
compared with `>=`, is the "really `==`" shape, so prefer a non-extreme wrong
value such as `1_000`. #217 used `LONG_GAP_MS = 1`, then set it to `8_000` in
GREEN.

**A wrong-constant RED for a whole NEW module proves only the tests that
depend on that constant (#217 review round 3).** Every other new test passes at
the RED commit, so nobody ever saw it fail. In the RED commit message, list
only the tests that really fail under the wrong constant (derive them, do not
guess), and never claim the RED reproduces the old behaviour unless it does.
The tests that encode the new contract against the OLD code belong in their own
earlier `test(#N)` commit that uses only the existing API (#217 `e66514a`).

**Deriving exact expected values on the no-compile box (#217).** Do not
hand-compute dozens of pins. Write a scratch Python model that mirrors the pure
Rust function step by step, and derive every exact value from it: fixture
counts, boundary show/hide times, what each mutant would do. Keep the model in
the scratchpad, not the repo. When the Rust changes, update the model in the
same step. Each fresh-context review pass should re-derive the pins with its own
model; two independent models agreeing is the only local evidence available.

**For a state machine, also FUZZ the model against invariants (#215 rounds
4–6).** Hand-picked scenarios kept missing the program bus's cut edge cases;
a randomized run (random cuts, source skews, pre-rolls, stalls) checked for
"no hard cut, no mix out of a source that is off program, no stale reorder
entry" found one each round. All three came from ONE condition written twice
(which window holds the source on air vs which window a later cut freezes),
so when two decisions depend on the same state, derive both from one
predicate (`Window::holds_on_air`).

## Linux clippy `-D warnings` traps a no-compile box can't catch locally (#162)
The ubuntu job runs `clippy --workspace --all-targets -D warnings`, so these
compile CLEAN on Windows but FAIL on Linux — reason them out before pushing:

- **A `pub(crate)` fn called ONLY inside a `#[cfg(windows)]` block is dead_code
  on Linux** (the cfg block is stripped, so the fn has no non-test caller in the
  Linux lib target — tests don't count for the lib target). It fails `-D
  warnings`. Fix: `#[cfg_attr(not(windows), allow(dead_code))]` on the fn (see
  `HeavyStepPlan::creation_flags` in `lyrics/heavy_plan.rs`). Same idea for any
  item live only on one platform.
- **Deleting a fn's ONLY non-test consumer orphans a TYPE import to test-only →
  `unused_imports` in the lib target (#201 r2).** When you delete/replace the one
  non-test caller of a helper (e.g. `websocket.rs` dropped `transport_from_label`
  because the replay now reads `s.transport`), a type that was named only through
  that helper (`TransportState`) is suddenly referenced ONLY inside
  `#[cfg(test)] mod tests`. The top-level `use` is then unused in the LIB target
  (tests don't count), so `clippy --all-targets -D warnings` fails — the
  no-compile box can't see it. Fix: move that name into a test-only import
  (`use …::TransportState;` INSIDE `mod tests`, not the top-level `use`). Grep the
  non-test region for every name in a `use` you touched when deleting a consumer.
- **An RAII guard field held ONLY for its Drop is `dead_code` "never read" — a
  `_` prefix does NOT suppress it (that only silences `unused_variables` for
  LOCALS, never `dead_code` for a FIELD).** A guard that holds a permit / handle
  purely so its Drop fires (`_permit: OwnedSemaphorePermit`) needs an explicit
  `#[allow(dead_code)]` ON THE FIELD (see `HeavySlotGuard._permit` in
  `lyrics/heavy_slot.rs`). A field that IS read in the Drop body (e.g.
  `ChildJobGuard.handle`) is fine without it.
- **A guard held across `.await` in a SPAWNED (Send) worker future must itself be
  `Send`.** A raw Windows `HANDLE` (`*mut c_void`) is `!Send`, so storing it in a
  guard held across `child.wait().await` breaks `tokio::spawn`. Store the handle
  as `isize` (Send) and cast back (`h as HANDLE`) only inside the Drop's `unsafe`
  block (see `heavy_slot.rs::ChildJobGuard`). windows-sys (not `windows`) is the
  lighter FFI for such cfg(windows) OS calls — primitive types (`u32`/`i32`/
  `*mut c_void`), no Result/Param wrappers, so `== 0` / `.is_null()` checks and
  `std::mem::zeroed()` POD structs (set `dwLength` yourself) are what compile.
- **`clippy::duplicated_attributes`** (warn-by-default → error under `-D
  warnings`): adding `#[allow(clippy::too_many_arguments)]` to a fn that ALREADY
  had one makes the attribute appear twice → build fails. GREP for a pre-existing
  allow before adding (the #162 `separate_stems` incident).
- **`clippy::too_many_arguments` fires at 8+ args, not 7.** A 7-arg fn needs NO
  allow; adding one is a dead annotation (harmless, but don't add it "to be
  safe"). Count real params (free fns have no `&self`).
- **`clippy::manual_slice_fill`** (rust 1.98, `-D warnings`): a `for x in &mut
  slice { *x = <const> }` loop must be `slice.fill(<const>)`. The no-compile box
  can't see it; it failed #186's Lint on `for e in &mut self.eos { *e = false }`.
- **`clippy::collapsible_if` in edition 2024 wants a let-chain** (#147 r9):
  `if cond { if let Some(x) = f() { … } }` with no `else` fails `-D warnings`.
  Write `if cond && let Some(x) = f() { … }` (the tree already uses let-chains,
  e.g. `lyrics/genius.rs`).
- **`clippy::manual_div_ceil`** (warn-by-default → `-D warnings`): a hand-rolled
  ceil-division `(a + b - 1) / b` (or the `(a * p + 99) / 100` form) must be
  `a.div_ceil(b)`. `u64::div_ceil` is **const-fn since 1.73**, so it works inside
  a `const fn` too — don't avoid it there. The no-compile box can't see it; it
  cost #192 round 3 a whole review round (three ceil-divs in `audio_emitter.rs`
  `block_ms`/`ring_capacity_blocks` + `loop_stats.rs` `percentile_ceil`). The tree
  already uses `.div_ceil()` (`chunking.rs`, `burn_overlay.rs`) — grep before
  hand-rolling a ceil.
- **`clippy::manual_contains`** (`perf`, warn-by-default → `-D warnings`, #212
  follow-up review round 4). `slice.iter().any(|&x| x == y)` must be
  `slice.contains(&y)` whenever the element type and `y`'s type are equal
  once lifetimes are erased; a `Vec<&'static str>` searched for a `&str` is
  one case. The lint does NOT fire when the types differ, e.g. a `Vec<String>`
  searched with `|c| c == "literal"`, which is why the tree's many
  `calls().iter().any(|c| c == …)` pass. Give the needle the element's exact
  type (`&'static str`) and call `.contains(&needle)`.
- **`clippy::never_loop` is DENY-by-default** (a correctness lint, so it is an
  error even without `-D warnings`; #213 review round 1). A `loop { match … }`
  in a test helper where EVERY arm returns or panics never iterates twice, so
  it fails the Lint job. For example, "read the next frame; a Close returns,
  anything else panics". Write it straight-line with no `loop`. Keep the
  `loop` only when some arm continues (e.g. `Some(Ok(_)) => {}` skipping
  pings).
- **`clippy::result_large_err` and a tungstenite handshake callback** (#213).
  A named `fn` returning `Result<Response, ErrorResponse>` (http
  `Response<Option<String>>`, over 128 B) trips the lint — and so does an
  inline CLOSURE (clippy 1.98 checks closures too; CI Lint run 36283126391).
  The shape is fixed by tungstenite's `Callback`, so put
  `#[allow(clippy::result_large_err)]` on the `let callback = …` statement
  with that reason. Build the refusal in a helper that returns `ErrorResponse`
  by value. Own helpers returning `tungstenite::Error` (136 B) must box it:
  `Result<(), Box<tungstenite::Error>>` + `.map_err(Box::new)`.
- **`clippy::nonminimal_bool` rewrites `!opt.is_some_and(|x| …)`** (#217
  addendum 3 review round 2). It is warn-by-default (complexity), so under
  `-D warnings` it fails the Lint job. Clippy's `METHODS_WITH_NEGATION`
  table maps a negated `is_some_and` to `is_none_or` from MSRV 1.82, and the
  workspace is 1.85. Write `opt.is_none_or(|x| x.id != id)`: negate the
  closure body, never the call. `!opt.is_some()` / `!opt.is_none()` are in
  the same table.

## Two compile errors a no-compile review round cannot see (#218/#219 integration)

Six fresh-context review rounds passed both of these, and the first CI run
failed on them (`36438006665`):

- **`json!(5_000_000_000)` is an `i32` literal → `overflowing_literals`
  (deny-by-default).** `serde_json::json!` gives an unsuffixed integer
  literal no type hint, so it defaults to `i32`. Suffix any literal above
  `i32::MAX`: `json!(5_000_000_000_u64)`.
- **A guard passed to a generic `impl Fn(&T)` does NOT deref-coerce.**
  `done(&self.state.read().await)` with `done: impl Fn(&ObsState) -> bool`
  is E0308 (`expected ObsState, found RwLockReadGuard`). The Fn call's
  argument is a generic tuple, so no coercion site exists. Write
  `done(&*guard)`. A plain `fn f(s: &ObsState)` would coerce, which is why
  it reads as fine. The same holds for `&PathBuf` → `&Path` through a
  generic `R: Fn(&Path, &Path)` seam: write `.as_path()`. A small private
  trait seam (`downloader::cache::FileOps`, #136) avoids the trap
  entirely, because trait method calls DO coerce their arguments.
- **Splitting a fn: a `String` param that becomes `&str` leaves
  `f(&req_id)` behind → `clippy::needless_borrow`** (#221, caught in
  review before CI). When a moved body now receives `req_id: &str`, write
  `self.cancel(req_id)`, not `self.cancel(&req_id)`.
- **`&Box<dyn Trait>` passed where `&dyn Trait` is expected is E0277**
  (#136 review round 1). `probe_one(p, …)` with `p: &Box<dyn
  MetadataProvider>` does not deref-coerce: rustc commits to the unsize
  coercion and then needs `Box<dyn MetadataProvider>: MetadataProvider`.
  Write `&**p` (the repo's shape: `playback/wallclock.rs` `&*source`); a
  METHOD call on `p` auto-derefs fine.
- **Moving a fn out of a file can orphan its `///` lines** (#136 round 2):
  the doc comment left behind documents whatever item follows (there a
  `#[cfg(test)] mod tests` after a blank line) →
  `clippy::empty_line_after_doc_comments` under `-D warnings`. Delete the
  doc with the fn; re-read the cut site.
- **A new field on a struct that a TEST builds by literal is E0063 for the
  whole test target** (#224 part 2 review round 2: `SharedEmitterInner`
  gained `fleet`, `audio_emitter_tests.rs` still built one without it).
  Before adding a field, grep the crate for `TypeName {` in every file,
  tests included, and add it (or `..Default::default()`) at each site.

## Spawn order is not execution order — never order work by spawning it (#221)

A multi-thread tokio worker runs the task it spawned LAST first (its LIFO
slot) and other workers may steal the rest, so two tasks spawned back to
back can start in either order. The OBS client once spawned a task per
facade call, so a playlist press's mirror could reach cg OBS after a later
press. Anything whose ORDER matters goes through ONE task that takes the
items in order (`obs::remote_call::run_calls`: write each frame, then take
the next; a scene switch's answer is awaited first because cg OBS runs its
messages on a thread pool; only a getter's answer wait is spawned). A test
of such an order must queue all items before the consumer runs, so a
reordering spot fails deterministically (`the_calls_reach_cg_obs_in_queue_order`;
its RED sent the drained batch newest first). Give such a consumer its
timeouts as PARAMETERS (`run_calls(…, answer_timeout)`, like
`Upstream::with_timeout`): a test passing a very long one makes "this must
not be awaited" fail deterministically instead of racing the production
value.

## A unit test that hardcodes a PLATFORM-specific value fails on the Windows job (#189)
The `Build (Windows)` CI job runs `cargo test --workspace` on `windows-latest`,
so EVERY `#[test]` in `crates/` runs on BOTH Linux (the `Test` job) and Windows.
A test that bakes in a Unix-only literal passes on Linux and FAILS on Windows —
and the no-compile box can't see it. The one that bit #189: a `path_with_tools`
test asserting the joined PATH string `"/opt/tools:/usr/bin:/bin"` — on Windows
`std::env::{split_paths,join_paths}` use `;`, not `:`, so the string differs
(`1482 passed; 1 failed` on the Windows job only). Fix: assert the INVARIANT, not
the platform string — round-trip the result through `std::env::split_paths` and
check `parts[0] == tools_dir`, which holds on both separators. Same rule for any
`MAIN_SEPARATOR` / line-ending / drive-letter / temp-path assumption in a test.

**An engine test must not count the test pipeline's replies (release 0.68.0
blockers).** On Linux the stub pipeline (`pipeline_stub.rs`) answers every
`PipelineCommand::Play` with a `PipelineEvent::Error`; on Windows the real
pipeline has no NDI backend in CI, sends ONE Error at spawn and then only
waits for Shutdown. So "no Play was sent" read from `event_rx` passes or fails
by platform. Read it from engine state every Play resets instead: every Play
calls `begin_play`, which clears the song's title clock (`tests_hold.rs`).

**Never key a "stale event" check on a tokio task id.** tokio documents that
an id may be reused once its task has ended, which is exactly the state of a
stale event's task. Carry your own id from a process-wide `AtomicU64`
(`scene_off.rs::NEXT_RE_CHECK`, #215 review round 3).

**A "nothing was sent while X" test must use a NEW value.** The dispatch
paths dedup on the last value sent (`last_*_signature`), so the same line is
held back with or without the gate under test, and the test passes vacuously
(#215 review round 1: a held Position on the same line proved nothing).

## Doc-comment lists: blank `//!`/`///` line before the paragraph that follows

CI clippy runs with `-D warnings`, and `clippy::doc_lazy_continuation` (stable
since 1.80) rejects a paragraph line that directly follows a list item without
a blank doc line or indentation — the Tier-0 box cannot see it, so it fails
the Lint job (#195, three sites). After the last `- item` / `3. item`, insert a
bare `//!` (or `///`) line before continuing prose.

A sign convention written as a doc line that STARTS with `+ = …` or `* …` (or
`- = …`) is ALSO a markdown list item, so the next prose line trips the same
lint (#148 v5 review). Start such a line with a word (`Positive = …`), or keep
the `+` mid-line.

It happens by accident when prose WRAPS so a line begins with `+ `/`- `
(#219: `//! … over a real pool` / `//! + \`ProgramBus\`, and …` failed
review round 3). Before pushing, scan every changed file's doc lines: a doc
line matching `^\s*(//!|///) ?([-+*]|\d+[.)]) ` followed by a non-blank doc
line indented less than 3 spaces is the lint (a ~30-line Python scan in the
scratchpad, list files from `git diff --name-only <base>..HEAD -- '*.rs'`;
check it flags a known-bad sample first).

## A long-lived `JoinSet` must be JOINED, not only spawned and aborted (#219)

A tokio `JoinSet` keeps a FINISHED task (its cell + output) until
`join_next` / `try_join_next` takes it. A set that only ever sees `spawn` +
`abort_all` — the OBS connection loop's helpers, one per ~2 s poll tick —
grows for the whole life of the connection (~43 000 cells a day). Route every
spawn through a helper that first drains `try_join_next()` (and WARNs a
`JoinError` that is not a cancellation): `obs/mod.rs::spawn_helper`. Test it
on the current-thread `#[tokio::test]`: spawn finished tasks, `yield_now` a
few times, spawn one more, assert `len() == 1` (the RED is the helper that
only spawns → `len() == 4`).

## Format BEFORE every commit, RED commits included

The Lint job runs `cargo fmt --all -- --check` on the pushed HEAD, so a RED
test commit formatted only at the GREEN step still leaves the tree dirty when
the GREEN `git add` is selective — the later `cargo fmt --all` then formats
the RED files as an unstaged change nobody commits (0.62.0 cut, `e151ca1`).
Run `cargo fmt --all && git checkout -- crates/sp-server/src/db/models.rs`
before EACH commit in a RED→GREEN chain, and commit with `git add -u crates/`
(not a hand-picked file list) after formatting.

## Staying under the 1000-line cap when adding a big trait method (#203)

Adding a method to a trait (e.g. `PacedSink::submit_shared`, an 8-arg method) to
a file already near the cap pushes it over — and a trait DEFAULT method's body
CANNOT move to a sibling `impl` block (a default lives in the trait def). The fix
that worked: keep only a THIN delegating default in the trait
(`fn m(..) { sibling::default_m(self, ..) }`) whose body is a free fn in a new
sibling module, AND RELOCATE an unrelated pure item to the sibling to reclaim the
lines the new signature costs (the #203 lane moved `SleepDecision` +
`plan_sleep_100ns` from `pacer.rs` into a new `pacer_sink.rs` and re-exported them
`pub use pacer_sink::{SleepDecision, plan_sleep_100ns};` so `pacer::…` paths + the
test submodules' `super::*` stay valid). The sibling free fn that takes `self`
needs `<S: TheTrait + ?Sized>(sink: &mut S, …)` — `Self` is `?Sized` inside a
trait default, so a non-`?Sized` bound fails to compile. Verify `wc -l` after
`cargo fmt` — an 8-arg signature reflows to ~10 lines.

## `use super::*` in a `#[path]` test submodule: import what the test uses explicitly

A glob DOES bring the parent's private `use` imports into a child module (Rust
name resolution since RFC 1560 — e.g. `sp-ndi/src/sender.rs`'s tests use its
private `use std::sync::Arc` through `super::*`). What breaks is an AMBIGUOUS
name: two globs (or a glob + a glob-imported glob) that both provide it, which
errors only where the name is used. So, as the repo convention (#147 review):
give a test file an explicit `use` for every type it names that the parent
only imports (`SharedFrame`, `AudioFrame`, `Arc`, …). An explicit import
shadows a glob-imported name and never conflicts, so this is safe either way.

## RED on the no-compile box for a REFACTOR (not just a wrong constant) — #203

The "ship real logic with ONE WRONG CONSTANT" pattern extends to structural
refactors: ship the FULL structural change in the RED commit but make ONE spot
behave wrong so the new characterization test fails, then fix only that spot in
GREEN. Deterministic REDs that worked: `pop_block` extends its reuse scratch
WITHOUT `clear()` (scratch grows → byte-identity test fails); `service_standby`
submits `SharedFrame::new(video.to_vec())` (a fresh copy) instead of
`video.clone()` (a `ptr_eq`-across-slots test fails); `submit_nv12` sends a
throwaway copy while holding the original allocation (a holdover-identity test
fails). Prefer a deterministic wrong (grow / different-length) over "allocate
fresh" — a freed-then-reallocated buffer can land at the SAME address and make a
pointer-equality RED pass by luck.

**A LOCK-SCOPE refactor uses the same pattern (#147 r11, `sp-ndi/handle_table.rs`).**

- **RED:** ship the whole new structure, wired in, but keep the OLD scope in one
  spot (`with` holds the map's WRITE lock across the op, which is exactly the old
  global mutex).
- **GREEN:** only narrows that scope.
- **Tests:** prove concurrency with channels plus bounded `recv_timeout`, never
  sleeps. Hold an op inside handle A until signalled, then assert that handle B's
  op (and an insert/remove) completes within the bound.
- **Watch the lock kind:** a READ lock held across the op would still let B's
  read-side op through. Only the insert/remove test catches that shape, so write
  both.
- **No hangs:** drop the `release` sender on every failure path, so the held
  thread's `recv().unwrap()` panics instead of hanging the test.

**A RED for a new state machine or a restructure (#217 addendum 3, three
rounds).** Ship the whole new structure in RED (new types, fields, modules,
command variants). Keep the OLD behaviour in a few NAMED spots and list them
in the RED message:

- a `plan` that runs every command as sent;
- a supersede that returns the batch unchanged;
- a predicate that still reads the old input;
- an empty `rearm` with `_`-prefixed params (warning-free, still called);
- a helper without its new first step.

Keep every new item USED in RED, or the lib target warns. Two things that
were enough:

- a variant only GREEN's `plan` returns can be constructed by an old path
  (the old retry's instant hide as `run_title_action(HideNow)`);
- a new fn only GREEN's predicate calls can be read by a debug log of the
  window inputs, which GREEN keeps.

Write GREEN first and tar the changed files to the scratchpad. Build RED with
anchor-asserted scripts (rustfmt re-wraps lines, so re-read the formatted text
before anchoring). Commit RED, then restore ONLY the files whose spots differ
from the tar.

**A RED whose new tests call a CHANGED signature (#215 review rounds 3–5).**
The new tests must compile at the RED commit, but the fix changes an API
(an extra `&mut events` param, a renamed fn, an extra argument). Do it like
this:

- **Write GREEN first.** Save a copy of the GREEN file to the scratchpad.
- **Build the RED file from `git show HEAD:<file>`.** Apply ONLY the
  signature change with scripted, anchor-asserted edits: the param stays
  unused as `_events` / `_dropped`, or the renamed fn returns what the old
  one returned. Keep the old logic byte-identical otherwise. An unused
  `_`-param is warning-free.
- **Commit** the tests with that RED file as `test(#N) … [red]`.
- **Copy the saved GREEN back** and commit `fix(#N) … [green]`.
- **List only the tests that really fail on the old logic in the RED
  message.** Walk each one by hand.

**When the fix makes a parameter DEAD (it removes, not adds, an input) —
RED → GREEN → refactor (#224).** `ProgramOutput::submit(job, audio_now)`
lost its reason to take the emit instant. The RED tests keep the OLD
signature (they compile against the old code and fail on it); GREEN keeps
the parameter as `_audio_now_100ns` and edits no test; a separate
`refactor(#N)` commit then drops it from the signature, the loop and every
test call site (mechanical, no behaviour change) — and deletes whatever that
orphans (an unused test `const MS`, a `T0` import: `-D warnings`).

**Changing what a shared counter means (#224 review rounds 1–2).** Round 1
made a follow restart `WallClock::frames_since_resample`; a test on a
DIFFERENT harness (`WallClock::settable`, whose realtime read paired with the
real `Instant::now()`, so the new probe followed phantom steps) counted ticks
through it and went red — only the next review caught it. Before changing a
counter's or a stat's semantics, grep EVERY reader of it across all test
files and harnesses (`frames_since_resample`, `anchor_stats`, `samples()` /
`reads()` counters), not just the files of the change.

**`cargo fmt` can take minutes on a loaded box.** Two sessions formatting at
once left rustfmt in `D` state for > 2 min; the default 120 s Bash timeout
then silently moved the call to the background. Run it as
`timeout 540 cargo fmt --all` with the tool timeout near 600 s, and revert
`db/models.rs` in a separate command (the worktree guard refuses the chain).

**A RED for a wire-protocol feature runs against the OLD code (#221 L2).**
Tests that speak the wire (the facade's obs-websocket JSON over a real
socket) compile against the old implementation, so the RED is the real old
behaviour, not a wrong spot. Keep them compiling on both sides:

- add only the test SEAMS in RED (a `#[cfg(test)]` constructor such as
  `Facade::for_test` instead of a struct literal whose fields GREEN changes;
  a configurable timeout such as `Upstream::with_timeout`) and the new
  telemetry STRUCTURE (fields the old path fills with `None`);
- read new telemetry through its serialized JSON
  (`serde_json::to_value(status)["field"]`), which compiles whether or not
  the field exists yet;
- a fake peer matched with `let … else { continue }` on the one variant it
  serves keeps compiling when GREEN deletes the other variants;
- tests of functions that only GREEN adds (new pure helpers) go in the GREEN
  commit, as new tests; no RED test is edited there.

**`cargo mutants --in-diff <range> --list` compiles nothing (#215).** It
lists the diff's mutants (`file:line` + replacement) so a review can name
the test that kills each one BEFORE CI's mutation gate runs.

- The Tier-0 hook blocks it as a cargo subcommand. Because it only lists,
  the logged `# airuleset:build-ok list-only` bypass is honest here, and
  only here.
- **In a worktree lane, and after merging origin/dev into it** (#221 L4b):
  the Bash guard refuses `cd <wt> && cargo mutants … > file`. Run two plain
  commands instead: `git -C <wt> diff origin/dev..HEAD --output=<scratch>/range.diff`,
  then `cargo mutants --in-diff <scratch>/range.diff --list --dir <wt>`
  (no `cd`, no redirect). Diff from the MERGED `origin/dev`, not from the
  lane's original base: `<base>..HEAD` then also lists dev's own commits.
- **Give a review dispatch the merged base SHA, not `origin/dev`** (#136):
  the `.git` is shared with the main checkout, so another session's fetch
  can move `origin/dev` mid-review, and `git diff origin/dev..HEAD` (a TREE
  diff) then shows the other lane's work reversed. Name the SHA the lane
  merged (`git diff 0e988e5c..HEAD`).
- **The worktree Bash guard refuses `cd <wt> && python3 - <<EOF` edits**
  (and loops over computed paths): write the edit script to the scratchpad
  and run `python3 <scratch>/edit.py` with absolute paths inside it; assert
  each anchor's count before replacing. It also refuses a `cat >> file
  <<'EOF'` append, a `$VAR`-computed script path, and any command whose text
  contains `github.com` (a curl User-Agent tripped it, #144): same remedy.
- **A NEW file is missing from `git diff <base>` until git tracks it**
  (#221 L2b): listing uncommitted work with `git diff 5ad0178f > range.diff`
  showed no mutant at all for the new `remote/codec.rs`. `git add -N
  <new files>` first (or list the committed range), then re-list.
- cargo-mutants 27 turns `|=` only into `&=`, not `^=`.
- It turns a match guard into `true` / `false`, and `==` into `!=`.
- `a && b && c` parses as `(a && b) && c`, so its two `&&`→`||` mutants
  are `(a || b) && c` and `(a && b) || c` — never `a || (b && c)`. Model
  those two when you name the killing test (#215 round 4).
- It generates NO mutant for a plain assignment (`self.flag = false;`) or
  for an `if` condition that is a bare variable (`if breaker_just_closed {`).
  Deleting such a line survives the gate unseen, so give it its own
  behaviour test that fails without it. #217 pinned the Ok-arm clear of
  `last_full_attempt_failed` with
  `an_answered_fetch_after_a_failed_one_restores_the_fast_path`.
- The same holds for a CALL STATEMENT whose result is discarded
  (`self.finish_push("hide_title_now", result);`): no mutant, so pin its
  effect with a behaviour test (#217 addendum 2,
  `a_retried_hide_that_404s_leaves_no_stale_note_for_the_next_push`).
- **A branch whose ONLY effect is a log line survives the gate** (#224
  part 2 review round 3: `if … && !slew.owe(..) { warn!(..) }` — the
  delete-`!` mutant only moves the WARN). Give such a branch an observable
  effect a test reads (a counter: `WallVbanClock::taken_at_once`). Likewise
  never compute a log-only value inline (`jump_us = jump / 10`): its `/`→`%`
  / `*` mutants are invisible; log through a tested helper (`to_us(jump)`).
- **A timing pin at ONE phase can be phase-lucky** (#224 part 2 review
  round 3: VBAN's ±100 ppm bound held with the step on block 100 and broke
  on block 101 at 50 ppm). When a result depends on where an event lands
  on a grid (packet spacing 41 666/41 667/41 668, the 100-tick resample),
  sweep the event over ≥ 3 consecutive phases in the test, and fuzz the
  scratch model over all of them before pinning.
- A match GUARD that is always true where it sits (`ShowTitle { .. } if
  self.recovery_sent_this_step` when the step has always fired an event by
  then) makes the guard→`true` mutant EQUIVALENT: it survives the gate. Drop
  the redundant guard (a plain arm; its "delete match arm" mutant is
  killable next to a `_` arm), or keep it as `a && b` where the `||` mutant
  is observable (#217 addendum 2 review round 3).
- A mutation that cannot compile (`&&`→`||` inside a let-chain) is
  "unviable": it costs a build but cannot fail the gate.
- **A binary op inside a `const` initializer IS mutated** (#221 review
  round 5). `Duration::from_secs(2 * DEFAULT_RESPONSE_TIMEOUT.as_secs())`
  listed `*`→`+` and `*`→`/`; with the 2 s default the `+` mutant is
  EQUIVALENT (2 + 2 = 2 × 2) and would survive the gate. Write such a
  constant as a literal (`Duration::from_secs(4)`) and pin the relation in
  a test (`MIRROR_EXTRA_WAIT == DEFAULT_RESPONSE_TIMEOUT * 2`; a runtime
  `Duration * u32` is fine there, it is not `const`).
- `(at - plane) % ds` where `plane` is a multiple of `ds` (a plane or row
  edge): the `-`→`+` mutant gives the SAME remainder, so it is equivalent
  and survives. Subtract ONCE into a local (`let offset = …; (offset / ds,
  offset % ds)`), so the mutant also moves the quotient, which a test sees
  (#215 addendum 3, `nv12_mix.rs`).
- **A Python mutation harness for a pure kernel (#215 addendum 3).** Mirror
  the Rust function in the scratch model with one switch per listed mutant
  (`cargo mutants --in-diff … --list`) and a mirror of the Rust tests. Also
  emulate Rust's panics: usize underflow, slice / index bounds, division by
  zero, and `clamp(lo > hi)`. Python wraps negative indices silently, so
  those panics must be raised by hand. A Python survivor is then a superset
  of the Rust ones; zero survivors is the local evidence before the CI gate.
  A mutant that only a mid-row / off-edge input can reveal (`a - c0` with
  `c0` always 0 on row-aligned runs) needs a test that cuts the input
  arbitrarily.
- **A new early return in front of pinned comparisons can silently orphan
  their killers** (#217 addendum 3, review rounds 4-5). Round 4 added
  `TitleClock::shows` as a guard ahead of `arm_title_timers`'s `>`
  comparisons, and adapted a boundary test whose clock the guard now
  rejected. The test still passed, but it no longer reached the
  comparisons, so their `>` → `>=` mutants survived. CI's gate diffs from
  the last GREEN mutation verdict, not from the last review round. So after
  any fix that adds a guard or adapts an existing test, re-list the FULL
  branch range (`cargo mutants --in-diff <base>..HEAD --list`) and re-map
  every mutant to a killer, not just the round's own diff.
- **A HANG fails the gate exactly like a survivor** (review round 1, same
  ticket). cargo-mutants kills a stalled test run at `--timeout` and reports
  TIMEOUT, which turns the shard red. The #215 harness first counted its
  own iteration guard as a "kill" and missed one: `%`→`+` made a hand-advanced
  `while !out.is_empty()` cursor step by 0 bytes forever. So:
  - give every loop of the model an iteration guard, and count a trip as a
    gate FAILURE, never a kill;
  - in the Rust, make progress structural instead of a cursor you advance
    by a computed length: split at the edge, then `chunks_mut(n)` /
    `enumerate`. A mutated length then panics or moves the output (killed),
    it never stalls.
  - the same for a retry/poll loop that ends only on a time comparison
    (#221 review round 1, `bootstrap_probe::decide`): `elapsed + delay >
    budget` → `==` never matches, and `delay * 2` → `/ 2` shrinks the pause
    to 0 — both spin forever on a paused clock. Bound it with `for n in
    1..=MAX` and break on the cap or the budget before pausing; the cap
    turns both mutants into a wrong probe count a test sees.

**`tokio::select!` drops the branch futures before a handler runs**
(tokio `macros/select.rs`: the futures live inside the `let output = {…}`
block, and the handlers run in the `match output` after it). So a handler may
take `&mut` of a receiver that a branch future borrowed, e.g.
`event = events.recv() => … task.resync(&mut events).await`.

## Untrusted input never goes through `serde_json::Value`'s own `Deserialize` (#221 L2b review)

serde_json's `raw_value` feature is ALWAYS on in this workspace (sp-server's
`api/preview.rs`, axum's `json`, sqlx-core — feature unification). With it,
`Value`'s `Deserialize` treats a map whose first key is
`$serde_json::private::RawValue` as a raw value and re-parses its string as
JSON with a FRESH 128-level recursion budget, so nested strings escape the
depth limit (~18 × 127 levels in 1 MiB) — a stack exhaustion that aborts the
whole process. For a frame from an untrusted peer, decode into a typed
struct with NO `serde_json::Value` anywhere inside it (a `Value` /
`Option<Value>` / `Vec<Value>` field goes through `Value`'s own
`Deserialize` again; unknown fields are skipped by serde_json without
building a `Value`) or through `remote::codec::Codec::Json.decode_text`
(its private `PlainValue` visitor keeps every key a plain string; make
`PlainValue` `pub(crate)` when a second module needs the visitor itself),
never `serde_json::from_str::<Value>` / `Json<Value>` / `Value::deserialize`.
An axum body is a typed struct (`api/ai.rs` `CompleteLoginRequest`, #221
L4a: the last `Json<serde_json::Value>` on the LAN HTTP API; its test first
shows the key re-parses through `Value`, then that the route ignores it).

## Binary test fixtures: byte-string literals, not long hex strings (#221 L2b)

The staging hook `block-sensitive-staging.sh` refuses any file with a 40+
character hex blob ("possible key/token") — a MessagePack / protocol fixture
written as hex trips it. Write the bytes as a byte-string literal with the
markers as escapes and the text as text
(`b"\x82\xa2op\x01\xa1d\x81..."`, `\xHH` takes exactly two hex digits), and
check it against the reference encoder's hex once in a scratch script. It
reads better too: the map keys are visible.

## `-D warnings` rejects `temporary.as_ptr()` in tests — bind the value first (#203 r2b)

`assert_eq!(take(cap).as_ptr(), p, …)` is a compile ERROR under CI's
`clippy --all-targets -D warnings`: rustc's `dangling_pointers_from_temporaries`
lint fires because the `Vec` temporary dies at the end of the statement while
the pointer is compared. The no-compile box cannot see it (it is a lint of the
lib TEST target). Write `let again = take(cap); assert_eq!(again.as_ptr(), p, …)`
— any pointer-identity assertion on a fresh value needs the value bound to a
local for the statement's lifetime. (`sub.prev_frame.as_ref().unwrap().as_ptr()`
on a LIVE field is fine — only temporaries trip it.)

## Diff-scoped mutation gate runs PER PACKAGE — a `test_util` accessor needs a test in ITS OWN crate (#203)

`cargo mutants` tests each mutant with the mutated crate's OWN test target. A
new accessor on `sp_ndi::test_util::MockNdiBackend` (e.g. `last_sync_video_len`)
that is exercised only by a `crates/sp-server` test SURVIVES every mutant
(`Some(0)` / `Some(1)` / `None` — "0s test", nothing in sp-ndi calls it) and
fails the gate, even though sp-server's test would catch the wrong value. When
you add a mock recorder for a downstream crate's test, ALSO assert it in an
sp-ndi test (`None` before the first call, the exact recorded value after —
never a constant that a `Some(0)`/`Some(1)` mutant could match). Same for any
cross-crate test-only seam.

## Diff-scoped mutation gate: a new `pub fn` reachable only from `#[cfg(windows)]` needs a direct Linux test (#203)

The CI mutation gate is `--in-diff` and strict. A NEW `pub fn` whose ONLY caller
is in a `#[cfg(windows)]` module (stripped on the Linux mutation runner) has no
Linux test exercising it, so its whole-fn `-> ()` mutant SURVIVES and fails the
gate — even though the function is "obviously" covered on Windows. `#203`'s
`FrameSubmitter::submit_shared` (called only from the `#[cfg(windows)]` idle loop)
needed an explicit Linux unit test calling it through `MockNdiBackend`. When you
add a pub fn during a diff, ask "does a LINUX `#[test]` actually call this?" — if
not, add one or the mutation gate reddens.

**Extracting pure logic OUT of a `#[cfg(windows)]` module: mind the file NAME
(#147 spin budget).** The `.cargo/mutants.toml` `exclude_re` entries are
SUBSTRING regexes. `'sp-server/src/playback/pipeline_paced'` excludes
`pipeline_paced.rs` AND every `pipeline_paced_*.rs` sibling. So a helper split
into `pipeline_paced_spin.rs` would compile on Linux but never be
mutation-tested. Name it outside every excluded prefix: `pacer_spin.rs`, with
`pub mod` in `playback/mod.rs`. Then `grep` the exclude list for your new path
before committing.

## Inserting a `mod` before a `#[cfg(test)]` test module STEALS the gate (#192 r5)

Attributes attach to the NEXT item. A `#[path] mod audio_emitter_tests;` at the
bottom of a file is preceded by `#[cfg(test)]`; inserting a NEW production
`#[path = "sibling.rs"] mod sibling;` right BEFORE it (e.g. to register a new
pure module) lands the pre-existing `#[cfg(test)]` onto the NEW `mod` — gating a
PRODUCTION module out of every non-test build — and leaves the tests module
UNGATED (dragging `MockNdiBackend`/`#[test]` into the lib). `cargo test` passes
(cfg(test) on) and `cargo fmt` cannot see it, so the no-compile box ships it;
`cargo build` / the release Tauri compile / `clippy --workspace --all-targets`
(lib target, cfg(test) OFF) then fail with `unresolved import`/`E0432`. Put the
new `mod` AFTER the test module, or move the `#[cfg(test)]` explicitly back onto
the test `mod` — and grep the insertion point for a `#[cfg(test)]` line directly
above your `old_string` anchor before an Edit that adds a sibling `mod`.

## An HTTP handler test goes through the real router, never a copy of its SQL (#144)

- Drive a handler with `crate::api::router(state, None)` + `tower::ServiceExt::oneshot`, as in `api/lyrics_tests.rs::send`, and assert the row / response afterwards.
- A test that runs its OWN copy of the handler's UPDATE ("mirror the handler's SQL") can never fail on a change to the handler. #144 deleted two of these: `reprocess_video_ids_sets_manual_priority` and `reprocess_all_stale_only_flags_stale_rows`.
- A test for a DELETED route stays useful as a regression guard. `router(state, None)` has no SPA fallback (that needs a `dist_dir`), so a removed path answers 404. Assert the harmful effect is absent FIRST, so the RED fails for the right reason, and the 404 last.

## A cross-crate test-only helper must be `#[doc(hidden)] pub`, NOT `#[cfg(test)]` (#203 2b)

`#[cfg(test)]` is per-crate: an item gated `#[cfg(test)]` in crate A is NOT
compiled when crate B's test target builds (each test binary is its own
process/compilation). So a test-only peek/reset helper that BOTH the owning
crate's tests AND a downstream crate's tests must call cannot be `#[cfg(test)]`.
`sp_decoder::frame_pool::{pool_len, clear_pool}` are read by sp-decoder's own
tests AND by sp-server's `frame_buf` recycle test, so they are `#[doc(hidden)]
pub` (a `pub` fn in a lib crate is never `dead_code`, even with no non-test
caller, so it passes `clippy -D warnings`). A `#[cfg(test)]` version would fail
sp-server's compile with `unresolved import`.

## Testing a process-global static pool shared across a whole test binary (#203 2b)

`frame_pool`'s free-list is a `static`, so within ONE test binary EVERY test
shares it and they run in parallel threads. Two safe patterns:

- **Owning crate (sp-decoder):** serialise the global-state tests on a private
  `static SERIAL: Mutex<()>` and `clear_pool()` at the start of each — the same
  pattern the repo's other global-state tests use. `clear_pool` is safe there
  because the serial lock makes the tests mutually exclusive.
- **Downstream crate (sp-server), or any test that must NOT nuke siblings:** do
  NOT call `clear_pool` (it would empty a concurrent test's buffers). Instead
  pick a UNIQUE, LARGE capacity no other test allocates (e.g. `CAP =
  1_500_007`) and assert `pool_len(CAP)` directly — the class is yours alone, so
  concurrency is a non-issue and no serialisation is needed.

A recycling-pool identity test is only deterministic if the recycled buffer
stays ALIVE in the pool between `recycle` and `take` (a `BTreeMap`/`Vec`
free-list keeps it), so `take(cap).as_ptr() == recycled_ptr` can never pass by
address-reuse luck. A RED that FREES instead of recycling must be caught by a
`pool_len` assertion (freeing never touches the pool, regardless of the
allocator), NOT by a pointer-equality assertion (a freed address can be reused).

## A test-only serial lock held across `.await` must be a `tokio::sync::Mutex` (#184 G2 + G0.1, twice in one night)

`clippy --all-targets -D warnings` runs `clippy::await_holding_lock` on the lib
TEST target too: a `static SERIAL: std::sync::Mutex<()>` guard taken at the top
of a `#[tokio::test]` and held across the handler's/worker's `.await`s is a
compile ERROR the no-compile box cannot see (it reddened `api/mix_tests.rs`
and then `heavy_slot_tests.rs` + `worker_tests_idle_gate.rs` + the
`stems/worker.rs` test module the same night). Declare the serializer as
`static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());`
and take it with `let _g = SERIAL.lock().await;`. Keep the `std::sync::Mutex`
form only for a PLAIN `#[test]` with no await (e.g. `frame_pool`'s serial).

## Adding a path dependency between workspace crates on the Tier-0 box (#184 G4)

`Cargo.lock` is tracked but its workspace-member versions are long stale (e.g.
`sp-decoder 0.49.0-dev.6`), and CI builds without `--locked`. So when a crate gains
a path dep (G4: `sp-decoder` → `sp-core`), add ONE line to that crate's
`dependencies = [...]` list in `Cargo.lock` by hand (alphabetical). Do NOT let
`cargo tree` / `cargo metadata` resolve it: they rewrite every stale member
version and bump unrelated deps (a ~20-line lockfile diff riding in a feature
PR). Also check the new edge adds no cycle (`sp-core` depends only on serde /
thiserror, so anything may depend on it).

## Scripted edits after `cargo fmt`: assert the anchor, never a silent `str.replace` (#212)

On the no-compile box every change goes through editors/scripts, and `cargo fmt`
re-wraps long lines. A later scripted `s.replace(old, new)` whose `old` is the
PRE-fmt text silently does nothing — #212 shipped a test asserting the old 2×2
standby while the rig built 2×4, caught only by a review pass (it would have
reddened 6 CI tests). Fail loudly when an anchor is missing
(`if old not in s: sys.exit(...)`), or use the Edit tool, and re-read the result.

In a worktree lane the Bash guard may refuse a long `python3 - <<'EOF' … EOF`
edit chained with `git` or `cargo` ("too complex to verify that it stays inside
the worktree"), and not every time (#221 L4a). Write the anchor-asserted edit
to a script in the lane's scratchpad and run `python3 <script> <path>` as its
own command, then fmt / commit in separate commands.

## "Never blocks / never waits" tests: gates and thread names, never wall-time thresholds (#212 follow-up)

The gating Coverage job runs every test under `cargo tarpaulin`'s ptrace, and
under ptrace a thread can stall for a long time on a breakpoint. So a
real-time grid, a "this call took < N ms" budget, or a read of a value some
other thread is still publishing can flake there. The no-compile box never
sees that. What held up across five review rounds:

- **Prove WHERE a call ran.** Have the mock record `thread::current().name()`
  per call (`MockNdiReceiveBackend::calls_by_thread`) and assert the helper's
  thread name. A regression that runs the call inline then fails
  deterministically.
- **Prove the caller does not WAIT.** Hold the slow call in the mock behind a
  `Mutex<bool>` + `Condvar` gate (`set_held`). Require the caller to make N
  more steps within a bounded `wait_for`, then release. Any wait on the
  caller's side stalls and fails the bound.
- **An awaited `mpsc` send to a queue that drains only while a peer is
  connected (cg OBS's command queue) is a stall** (#217 addendum 3, review
  rounds 3–4: it parked the engine loop, then the title timers). Send what
  matters first, then `try_send` the rest. Test it with a capacity-1
  channel filled by one `try_send`, the receiver KEPT ALIVE (`_rx`: a
  dropped receiver makes the send fail at once, so the test passes
  vacuously), the call under `tokio::time::timeout(5 s)`, then assert the
  important command arrived.
- **Use "must NOT happen yet" windows only in the safe direction.** For
  example, `recv_timeout(200 ms).is_err()` while the gate is held. Correct
  code can never fail it; a slow runner only makes it pass vacuously.
- **Pace virtual time for loop tests.** Every wait really sleeps, but only the
  waits advance the clock. A stall then cannot fake a missed boundary; the
  gate proves the waiting part.
- **On a paused clock a helper's OWN timeout is a timer too** (#221 review
  round 1). A `recv()` helper bounded at 10 s, awaiting an event the code
  sends at 15 s of virtual time, fails every run: auto-advance reaches the
  helper's 10 s deadline first. Bound such a wait above the virtual time it
  must outlast (`timeout(MAX * 2, rx.recv())` — it costs no real time).
- **A production bound a wire test must never reach is a parameter**
  (#221 review round 2). A real-socket test that holds a "no Ended yet"
  window races the 15 s production bound under a ptrace stall; the facade
  carries it (`Facade::transition_end_max`) and `Facade::for_test` sets
  10 minutes, like `Upstream::with_timeout`.
- **A spin / wait loop on a real clock: witness each step, never time it**
  (#147, `pacer_spin_tests.rs`).
  - Give the loop an observer hook `FnMut(step, elapsed)` and return a tally.
  - The test runs the loop on its own thread over a `WallClock::settable`
    wall that it holds frozen. The observer sends each Yield on a channel, and
    the test waits for N witnesses with `recv_timeout(20 s)`.
  - Assert only invariants that hold whatever the stall:
    - the tally equals the observer's counts;
    - the elapsed seen at each step is on the right side of the budget;
    - the loop never spins again after it starts yielding;
    - `yields × sleep ≤` the real time taken.
  - A `Drop` guard that moves the wall far past the boundary frees the
    spinner on every failure path, so the test never leaves a spinning thread
    behind.
- **FFI lock scope** (`mutants::skip` code that needs a runtime). Move the SDK
  calls into a struct built from the `unsafe extern "C" fn` pointer table
  (`receive.rs` `RecvHandles`). Test it with fake `unsafe extern "C" fn`s whose
  gate is a `static Mutex/Condvar`. Only one test may use those statics.
  Never let a fake panic: unwinding out of `extern "C"` aborts the whole test
  binary.
- **A value published by another thread is read only after waiting for THIS
  event's value** (e.g. `last_connect_ms >= 400`), never for "any value". An
  earlier event's late write can overwrite it.
- **A server-side deadline is tested with a WITNESS, not a sleep window**
  (#213, `remote/session_tests.rs`).
  - To prove "an identified client is NOT closed at the identify deadline",
    connect a LATER client that never identifies. Its close, or its dropped
    handshake, proves the earlier deadline passed; the first client must
    still be served.
  - On a short-deadline rig, RETRY a connect / identify that the deadline
    won under a stall (`connect_in_time`, `identified_in_time`, bounded by
    the test timeout). Retry only the errors the deadline really produces:
    `tungstenite::Error::Io` / `ProtocolError::HandshakeIncomplete`, a 4007,
    or (Windows) a reset that discarded the close frame. Anything else
    panics.
  - A 600 ms "must still work" sleep is exactly the window a ptrace stall
    fails on correct code.
