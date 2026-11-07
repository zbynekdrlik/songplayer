# SongPlayer

A standalone Windows desktop application that plays YouTube playlists continuously with loudness normalization, synced lyrics and one program output: NDI `SP-program`, Spout `SP-program-MAX` / `SP-program` for Resolume Arena, and program audio over VBAN, ASIO and native Dante. Designed for broadcast and live production use.

## Architecture

- **Tauri 2** — native Windows shell, bundles the app as an NSIS installer
- **Leptos 0.7** — reactive WASM frontend compiled with Trunk
- **Axum 0.8** — embedded HTTP + WebSocket server (`sp-server`): playlists, playback, the program bus, the lyrics and stems workers, the Companion remote control
- **sp-core** — shared domain types, SQLite database (sqlx), WASM-safe
- **sp-ndi** — NDI output and input via runtime-loaded NDI SDK (`libloading`)
- **sp-decoder** — Windows Media Foundation video decoder (Windows-only) and the FLAC audio reader
- **sp-gpu** — the Direct3D 11 compositor and Spout senders (Windows; a stub elsewhere)
- **yt-dlp + FFmpeg** — video download and -14 LUFS loudness normalization

Project rules for contributors and agents: `CLAUDE.md` and `.claude/rules/`.

## Development

**Prerequisites:** Rust stable, Trunk, cargo-tauri

```bash
# Check workspace
cargo check

# Run tests
cargo test

# Format
cargo fmt --all

# Lint
cargo clippy -- -D warnings

# Build WASM frontend
cd sp-ui && trunk build

# Build full app
cd src-tauri && cargo tauri build

# Sync version from VERSION file
./scripts/sync-version.sh
```

## License

MIT
