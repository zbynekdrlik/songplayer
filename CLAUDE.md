<!-- Global rules inherited from ~/.claude/CLAUDE.md (managed by airuleset) -->
# CLAUDE.md

This file provides guidance to Claude Code when working with code in this repository.

## Project Overview

SongPlayer is a standalone Windows desktop application that plays YouTube playlists with loudness normalization and NDI output. Built with Rust using Tauri 2 (shell), Leptos 0.7 (WASM UI), and Axum 0.8 (embedded HTTP/WebSocket server). Videos are downloaded via yt-dlp, normalized to -14 LUFS with FFmpeg, and can be output via NDI.

## Workspace Structure

The Cargo workspace root manages 4 crates. Two additional crates are excluded from the workspace because they have different build toolchains.

```
songplayer/
├── Cargo.toml              # Workspace root (members: sp-core, sp-ndi, sp-decoder, sp-server)
├── VERSION                 # Single source of truth for version (e.g. 0.1.0-dev.1)
├── scripts/
│   └── sync-version.sh    # Reads VERSION, updates all Cargo.toml + tauri.conf.json
├── crates/
│   ├── sp-core/          # Shared types, database (SQLite/sqlx), domain logic — WASM-safe
│   ├── sp-ndi/           # NDI output via libloading (runtime-linked, no compile-time dep)
│   ├── sp-decoder/       # Windows Media Foundation decoder (cfg(windows) only)
│   └── sp-server/        # Axum HTTP + WebSocket server, yt-dlp/FFmpeg orchestration
├── sp-ui/                # Leptos 0.7 WASM frontend (excluded from workspace, built with Trunk)
└── src-tauri/             # Tauri 2 shell (excluded from workspace, built with cargo tauri)
```

### Crate Descriptions

| Crate | Purpose |
|-------|---------|
| `sp-core` | Shared types, SQLite database via sqlx, domain models. Must be WASM-safe (no tokio, no std-only I/O). |
| `sp-ndi` | NDI SDK integration via `libloading`. Loads the NDI shared library at runtime to avoid compile-time dependency. |
| `sp-decoder` | Windows Media Foundation video decoder. Entire crate is `cfg(windows)` — will not compile on Linux. |
| `sp-server` | Axum 0.8 server with HTTP REST + WebSocket. Runs yt-dlp and FFmpeg as subprocesses. Main async binary. |
| `sp-ui` | Leptos 0.7 CSR frontend compiled to WASM via Trunk. Communicates with sp-server via HTTP/WebSocket. |
| `src-tauri` | Tauri 2 application shell. Embeds `dist/` from sp-ui build and spawns sp-server in background. |

## Dev Commands

**Check the workspace (fast, no output):**
```bash
cargo check
```

**Run tests:**
```bash
cargo test
```

**Format code:**
```bash
cargo fmt --all
```

**Check formatting (for CI):**
```bash
cargo fmt --all --check
```

**Lint:**
```bash
cargo clippy -- -D warnings
```

**Build the WASM frontend (requires trunk):**
```bash
cd sp-ui && trunk build
```

**Build the full Tauri app (requires trunk output in dist/):**
```bash
cd src-tauri && cargo tauri build
```

**Sync version from VERSION file:**
```bash
./scripts/sync-version.sh
```

## Branch Strategy

Two branches: `dev` + `main`. After merge: recreate `dev` with next `-dev.N` version.

## Version Management

`VERSION` is the single source of truth. Run `./scripts/sync-version.sh` after changing it.

| Branch | VERSION format | Example |
|--------|---------------|---------|
| `dev`  | `X.Y.Z-dev.N` | `0.1.0-dev.1` |
| `main` | `X.Y.Z`       | `0.1.0` |

**Workflow:**
1. Start work on `dev` with VERSION like `0.1.0-dev.1`
2. Run `./scripts/sync-version.sh` to propagate to all Cargo.toml files
3. Before PR merge: change VERSION to `0.1.0`, run sync-version.sh
4. After merge: recreate dev with `0.2.0-dev.1`

Note: The 4 workspace crates use `version.workspace = true` — only the root `Cargo.toml`, `src-tauri/Cargo.toml`, `sp-ui/Cargo.toml`, and `src-tauri/tauri.conf.json` need updating.

## Database

SQLite via sqlx with manual migrations. Migration logic lives in `crates/sp-core/src/db/mod.rs`. No external migration files — schema is applied programmatically at startup.

- Database file: configurable path, defaults to `songplayer.db` in app data dir
- Connection type: `SqlitePool` with `sqlx::sqlite`
- No compile-time query checking (`query!` macro requires DATABASE_URL) — use `query_as` with runtime checking

## Key Patterns

**sp-core must be WASM-safe:**
- No `tokio` in sp-core (use `futures` traits only)
- No `std::fs` or OS-specific I/O
- No `cfg(windows)` — platform code belongs in sp-decoder or sp-server

**sp-decoder is Windows-only:**
- All code wrapped in `#[cfg(target_os = "windows")]` or `#[cfg(windows)]`
- Uses the `windows` crate for WMF (Windows Media Foundation) APIs
- Not compiled on Linux/macOS CI — use feature flags if needed for cross-platform CI

**NDI via libloading (runtime linking):**
- NDI SDK not required at compile time
- `sp-ndi` loads `Processing.NDI.Lib.x64.dll` at runtime via `libloading`
- Gracefully degrades if NDI is not installed

**Circular import avoidance:**
Use local imports inside functions when needed to break cycles:
```rust
fn some_fn() {
    use crate::other_module::Thing;
    // ...
}
```

**Windows subprocess (hide console window):**
All `std::process::Command` calls for yt-dlp/FFmpeg must use `CREATE_NO_WINDOW`:
```rust
use std::os::windows::process::CommandExt;
command.creation_flags(0x08000000); // CREATE_NO_WINDOW
```

**Follow existing patterns** from similar projects (restreamer, iem-mixer) for consistency in error handling, logging (tracing), and state management.
