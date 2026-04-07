# SongPlayer

A standalone Windows desktop application that plays YouTube playlists continuously with loudness normalization and NDI output. Designed for broadcast and live production use.

## Architecture

- **Tauri 2** — native Windows shell, bundles the app as an NSIS installer
- **Leptos 0.7** — reactive WASM frontend compiled with Trunk
- **Axum 0.8** — embedded HTTP + WebSocket server (`sp-server`) for playlist management and playback control
- **sp-core** — shared domain types, SQLite database (sqlx), WASM-safe
- **sp-ndi** — NDI output via runtime-loaded NDI SDK (`libloading`)
- **sp-decoder** — Windows Media Foundation video decoder (Windows-only)
- **yt-dlp + FFmpeg** — video download and -14 LUFS loudness normalization

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
