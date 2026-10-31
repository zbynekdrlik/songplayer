---
paths:
  - "crates/sp-server/src/db/mod.rs"
  - "crates/sp-server/src/db/mod_tests*.rs"
  - "crates/sp-server/src/db/mod_test_helpers.rs"
---

# DB migrations (manual, in `db/mod.rs`)

Migrations are `(version, &str SQL)` tuples in `MIGRATIONS`; `run_migrations`
applies every not-yet-applied one in order, splitting each SQL on `;`. Add a new
`(N, MIGRATION_VN)` tuple + the `const MIGRATION_VN` (keep it inline unless
`mod.rs` nears the 1000-line cap) + a `#[path = "mod_tests_vN.rs"] #[cfg(test)]
mod tests_vN;` hook. NEVER edit an already-shipped migration.

## A NEW migration that mutates rows a PRIOR version's test asserts breaks that
## test's isolation (#184 round G1)

Each `mod_tests_vN.rs` seeds fixtures, fires the migration, and asserts the rows.
The pattern is `apply_first_n(&pool, N-1).await` (applies V1..=N-1 directly), seed
the fixtures V(N) reads, then `run_migrations(&pool)` to fire V(N). The trap:
`run_migrations` runs ALL pending migrations, so if a LATER V(N+1) deletes or
rewrites the rows V(N)'s test asserts, adding V(N+1) silently reddens V(N)'s test.

The incident: V28 splits + DELETEs `mix_vokaly`/`mix_podklad`/`mix_dabing`, which
V27's `derive` test asserts are present after migration. Fix: fire ONLY the version
under test with the `apply_upto(&pool, N)` helper (`mod_test_helpers.rs`) — it
applies unapplied migrations `<= N` and stops, so V(N+1) never runs inside V(N)'s
test. Use `apply_upto` (not `run_migrations`) in any per-version test that asserts
rows a later migration could touch. `migration_vN_advances_schema_version` may
keep `run_migrations` (it only checks `current_schema_version == MIGRATIONS.last()`).

When you add a migration that changes settings/rows an earlier `mod_tests_vN`
asserts, GREP the earlier test files for those keys and switch them to `apply_upto`
in the SAME commit.

## Check a migration or a query with Python sqlite3 on the REAL schema (#261)

Every statement can run outside cargo: a
~40-line scratch script reads `db/mod.rs`, takes each `const MIGRATION_VN:
&str = "…";` body (regex, `re.S`) in the order of the `MIGRATIONS` list,
splits it on `;` exactly as `run_migrations` does and executes it into
`sqlite3.connect(":memory:")` with `PRAGMA foreign_keys = ON`. Then seed the
test's own fixture, run the Rust SQL text (strip `\` line continuations
from the literal) and assert the test's pins; `EXPLAIN QUERY PLAN` shows
whether a new index is used, and a bench at the boxes' sizes shows how long
a DELETE holds the write lock (#261 review round 1: a correlated scan per
row held it for 10 s, past the pool's 5 s busy timeout). Keep the script in
the scratchpad, never in the repo; a review dispatch reuses it.

A version test pins its migration's place in the list as `assert!(latest >= N)`,
never `assert_eq!(MIGRATIONS.last().unwrap().0, N)`: an exact pin fails the next
lane's V(N+1) on correct code (#233 ruling 4's V32 shipped one; review round 15).
