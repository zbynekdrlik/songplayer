---
paths:
  - "crates/sp-server/src/panic_hook.rs"
  - "src-tauri/src/lib.rs"
  - "src-tauri/Cargo.toml"
  - "crates/sp-server/build.rs"
---

# Crash diagnostics — panic hook + durable crash record (#156)

**The shipped `SongPlayer.exe` UNWINDS on a Rust panic — it does NOT abort.**
The root `Cargo.toml [profile.release]` (`lto`, `codegen-units = 1`,
`panic = "abort"`) applies ONLY to standalone `sp-server` builds/tests. The
shipped binary is built from the **excluded** `src-tauri` package, which has its
own `[profile.release]` (only `debug = "line-tables-only"`, #156) and otherwise
uses cargo's DEFAULT release profile → `panic = "unwind"`. So a Rust panic in the
shipped exe unwinds, the panic hook runs, and it writes `songplayer-panic.log`
(a main-thread panic then exits `101`; a worker-thread panic kills only that
thread). **Therefore: a `0xc0000409` WITH a `songplayer-panic.log` is a Rust
panic; a `0xc0000409` WITHOUT one is a NON-panic abort** — `handle_alloc_error`
→ `std::process::abort()` → `__fastfail(FAST_FAIL_FATAL_APP_EXIT = 7)` (Rust
aborts on allocation failure under BOTH panic strategies), or a native
`__fastfail` (stack-cookie check = 2, etc.). Do NOT add `panic = "abort"` to
`src-tauri` — changing the panic strategy is out of scope; only debuginfo was added.

**A tracing-only crash record is NOT durable — this is the trap.**
`src-tauri/src/lib.rs::setup_logging()` wires the file layer through
`tracing_appender::non_blocking`, whose `WorkerGuard` drains the buffered channel
only on **Drop**. A non-panic `abort()` (alloc failure / fast-fail) skips Drop
entirely, so a message sent to `tracing::error!` can sit in the channel and never
reach disk. Therefore the panic hook
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

## Aborts the panic hook cannot see — WER LocalDumps (2026-09-15, #156)
A `0xc0000409` with NO `songplayer-panic.log` is a NON-panic abort (see the top
of this file): an allocation failure (`handle_alloc_error` → `abort()` →
fast-fail `7`), a stack overflow, or a native `__fastfail` (stack-cookie check
`2`, …) aborts the process WITHOUT running the panic hook — 2026-09-14 09:12:08
UTC SongPlayer 0.49.0-dev, and again 2026-09-21 13:15:40 UTC 0.47.0 (32 MB
`SongPlayer.exe.13172.dmp`), both aborted with the hook armed and no panic log.
The box therefore has Windows Error Reporting LocalDumps for `SongPlayer.exe`:
`HKLM\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\SongPlayer.exe`
(`DumpFolder=C:\ProgramData\SongPlayer\dumps`, `DumpType=1` mini, `DumpCount=5`).
**Never ship a "fix" for an abort without one of the two artifacts** (a panic log
or a dump read).

**After any abort, read (in order):**

1. **Both artifacts exist?** `songplayer-panic.log` in `C:\ProgramData\SongPlayer\`
   (a panic → thread/`file:line:col`/message/backtrace), and `dumps\*.dmp`
   (`dir C:\ProgramData\SongPlayer\dumps`).
2. **Windows Application Event 1000** — the OS's own crash record (module,
   exception code, fault offset), even when no dump was written:
   ```powershell
   Get-WinEvent -FilterHashtable @{LogName='Application'; Id=1000} -MaxEvents 20 |
     Where-Object { $_.Message -match '^Faulting application name: SongPlayer' } |
     Select-Object TimeCreated, Message | Format-List
   ```
3. **Read the dump with the pure-Python parser — there is NO `cdb.exe`/WinDbg on
   the box** (do not install one; `scripts/analyze_minidump.py` does the read).
   Use an ISOLATED venv — NEVER the lyrics venv, never the system Python's
   site-packages:
   ```powershell
   & "C:\Program Files\Python312\python.exe" -m venv C:\ProgramData\SongPlayer\verify\dumpenv
   & C:\ProgramData\SongPlayer\verify\dumpenv\Scripts\pip.exe install minidump==0.0.24
   & C:\ProgramData\SongPlayer\verify\dumpenv\Scripts\python.exe `
       scripts\analyze_minidump.py C:\ProgramData\SongPlayer\dumps\<file>.dmp
   ```
   It prints the exception record (code + named fast-fail subcode), the faulting
   thread's return-address scan (`module+offset` frames — system DLLs and
   `SongPlayer.exe` are recognisable by module even without symbols), the module
   list, and the SystemInfo/MiscInfo streams. **Symbols:** the CI run's
   `songplayer-pdb` artifact (retention 30 d) matches the DEPLOYED build, so a
   `SongPlayer.exe+<offset>` frame now resolves to `file:line` against that PDB.
4. **The main log** lives at `C:\ProgramData\SongPlayer\songplayer.<YYYY-MM-DD>.log`
   (the data-dir ROOT, NOT a `logs\` subfolder); grep it around the crash time and
   for `target=panic`.
