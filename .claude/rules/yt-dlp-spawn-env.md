---
paths:
  - "crates/sp-server/src/downloader/**"
  - "crates/sp-server/src/playlist/**"
---

# Every yt-dlp spawn needs BOTH `hide_console_window` and `apply_utf8_env`

`yt-dlp.exe` is a frozen-Python program. On Windows its stdout is encoded with
the process ANSI codepage (cp1252 on `win-resolume`), **not** UTF-8 — so a
`--dump-json` title containing a character outside cp1252's range (e.g.
"Vámonos") is mangled into the DB before `String::from_utf8_lossy` ever reads
it, and the mangled title becomes wrong song/artist on the wall (#136 T4).

**When you add a NEW `tokio::process::Command::new(<ytdlp>)` spawn, call both
helpers before `.output()`/`.spawn()`:**

```rust
crate::downloader::hide_console_window(&mut cmd); // no cmd-window flash on Windows
crate::downloader::apply_utf8_env(&mut cmd);      // PYTHONUTF8=1 + PYTHONIOENCODING=utf-8
```

- `apply_utf8_env` lives in `downloader/mod.rs` next to `hide_console_window`.
- It matters most for the **title-emitting** spawns — the flat-playlist sync
  (`playlist/mod.rs::sync_playlist`) and `downloader/tools.rs::fetch_video_metadata`,
  because those titles flow into `metadata::get_metadata` → `videos.song`/`artist`.
  The download-stream spawns get it too (harmless, keeps the pattern uniform).
- The yt-dlp `--version` / `--update` probes in `tools.rs` emit no title text,
  so they intentionally skip it — don't "fix" them.
- Same mechanism `lyrics::mtl_aligner` uses for its Python child (#137). The
  env test lives in `downloader/mod.rs`'s test module
  (`apply_utf8_env_sets_pythonutf8_and_ioencoding`).

(Related: `youtube-cookies.md` covers the `--cookies` requirement on the same
paths — a yt-dlp spawn usually needs all three concerns.)
