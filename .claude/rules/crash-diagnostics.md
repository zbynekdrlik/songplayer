---
paths:
  - "crates/sp-server/src/panic_hook.rs"
  - "src-tauri/src/lib.rs"
  - "crates/sp-server/build.rs"
---

# Crash diagnostics — panic hook + durable crash record (#156)

**Release builds abort on ANY panic.** `Cargo.toml [profile.release] panic = "abort"`
turns every Rust panic into `abort()` (Windows Application Event 1000, exception
`0xc0000409`). There is no unwinding, so `catch_unwind`/`Drop` do NOT run on the way out.

**A tracing-only panic hook is NOT durable under abort — this is the trap.**
`src-tauri/src/lib.rs::setup_logging()` wires the file layer through
`tracing_appender::non_blocking`, whose `WorkerGuard` drains the buffered channel
only on **Drop**. `abort()` skips Drop, so a panic message sent to `tracing::error!`
can sit in the channel and never reach disk. Therefore the panic hook
(`crates/sp-server/src/panic_hook.rs::install_panic_hook`) writes the crash record
**synchronously with an explicit `flush()`** to a dedicated `<data-dir>/songplayer-panic.log`
(append), and only ALSO emits `tracing::error!(target = "panic")` as best-effort. If you
change the logging writer, keep the synchronous crash-file path — do not "simplify" it to
tracing-only.

**Where to look after a crash:** `C:\ProgramData\SongPlayer\songplayer-panic.log` (thread,
`file:line:col`, message, backtrace, version+sha), and the main log for `target=panic`.
The startup INFO line (`SongPlayer v<ver> (<sha>) — panic hook installed`) confirms the hook
is active; the short git sha comes from `crates/sp-server/build.rs` (`git rev-parse`, falls
back to `unknown`).

**The hook is process-global and installed from BOTH entry points** (`sp_server::start()` and
the Tauri `run()`), idempotent via `Once` — install it as early as possible so a
startup-phase panic is captured too.

**Percentile guards (pacer):** `jitter_p99_us` / `prep_p99_us` / `iter_percentile_us` MUST keep
their `if len == 0 { return 0 }` guard — without it `.min(len - 1)` panics on an empty ring
(debug: subtraction overflow; release: wrap → out-of-bounds index). Empty-ring tests lock this
in `pacer_tests_mutants.rs` / `pacer_prepare_tests_mutants.rs`.
