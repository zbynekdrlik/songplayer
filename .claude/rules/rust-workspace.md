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
