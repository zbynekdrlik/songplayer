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
CONSTANT (e.g. `const ABORT_CONSECUTIVE_BUSY: u32 = u32::MAX;` → GREEN sets `2`).
The logic still reads/writes every field, so nothing is dead_code, and the
"must-abort" tests fail cleanly; a stub function body that ignores its fields
would instead trip `dead_code`/`unused`. For timing tests (interval + sleep),
use `#[tokio::test(start_paused = true)]` (tokio `test-util` is a dev-dep) so the
1 s poll and a 30 s mock future advance deterministically with no real wait.

## Linux clippy `-D warnings` traps a no-compile box can't catch locally (#162)
The ubuntu job runs `clippy --workspace --all-targets -D warnings`, so these
compile CLEAN on Windows but FAIL on Linux — reason them out before pushing:

- **A `pub(crate)` fn called ONLY inside a `#[cfg(windows)]` block is dead_code
  on Linux** (the cfg block is stripped, so the fn has no non-test caller in the
  Linux lib target — tests don't count for the lib target). It fails `-D
  warnings`. Fix: `#[cfg_attr(not(windows), allow(dead_code))]` on the fn (see
  `HeavyStepPlan::creation_flags` in `lyrics/heavy_plan.rs`). Same idea for any
  item live only on one platform.
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
- **`clippy::manual_div_ceil`** (warn-by-default → `-D warnings`): a hand-rolled
  ceil-division `(a + b - 1) / b` (or the `(a * p + 99) / 100` form) must be
  `a.div_ceil(b)`. `u64::div_ceil` is **const-fn since 1.73**, so it works inside
  a `const fn` too — don't avoid it there. The no-compile box can't see it; it
  cost #192 round 3 a whole review round (three ceil-divs in `audio_emitter.rs`
  `block_ms`/`ring_capacity_blocks` + `loop_stats.rs` `percentile_ceil`). The tree
  already uses `.div_ceil()` (`chunking.rs`, `burn_overlay.rs`) — grep before
  hand-rolling a ceil.

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
