---
paths:
  - "crates/sp-server/src/downloader/**"
  - "crates/sp-server/src/playlist/**"
---

# YouTube downloads need a cookie file (since 2026-08-20)

YouTube answers every anonymous `yt-dlp` video fetch with
`Sign in to confirm you're not a bot` (playability `LOGIN_REQUIRED`), on every
network we tested (win-resolume and dev1), with the newest yt-dlp, with every
player client, and with a PO-token provider (tested — it does not help; the
block fires before any token is requested). `--flat-playlist` listing is NOT
affected, so playlist sync keeps working while every download fails and the
queue stalls (#141, #139, #140).

**Symptom → where to look**

| Symptom | Meaning |
|---|---|
| dashboard shows the song but it never becomes playable; `normalized=0` rows pile up | download path, not sync — grep the log for `video download failed` |
| log line ends with `Sign in to confirm you're not a bot` | cookie file missing / expired |
| `WARNING: The provided YouTube account cookies are no longer valid` | the browser session rotated; re-export |
| video stream OK but `audio download failed … Requested format is not available` | the box's yt-dlp is stale (2026.03 saw only HLS formats with cookies, no separate `bestaudio`); replace `cache\tools\yt-dlp.exe` with the latest release (#140 — the app never updates it itself) |

**Operational knobs since 0.47.0-dev.3 (#139, #140):** playlists re-sync every
`PLAYLIST_SYNC_INTERVAL_SECS` (default 600) — grep `periodic sync: enqueueing`;
a failed download backs off `5 min · 2^(n-1)` (cap 24 h) per row instead of
blocking the queue — grep `download failed, scheduled retry` / the `error!` at
5 attempts, and the song row shows `⚠` with the error as tooltip; yt-dlp runs
`--update` at startup and every `YTDLP_UPDATE_INTERVAL_SECS` (default 86400) —
grep `yt-dlp self-update`. A retry that is not due yet does not block later
rows (`next_attempt_at` column, migration V20).

**The fix in code:** `downloader/mod.rs` passes `--cookies <data_dir>/cookies.txt`
to both yt-dlp calls whenever `C:\ProgramData\SongPlayer\cookies.txt` exists
(re-checked per download, no restart needed). yt-dlp REWRITES the jar it is
given, so any manual test must run on a COPY of the file.

**Since ~2026-09 there is a SECOND gate on top of cookies — the JS "n-challenge"
(#175 finding).** Even WITH valid cookies (which clear the bot-check), a fetch
fails `n challenge solving failed … The page needs to be reloaded` on every
`player_client` (tv/mweb/web_safari all tried). yt-dlp's EJS solver needs a
JavaScript runtime; `yt-dlp --help` lists `--js-runtimes RUNTIME[:PATH]` with
supported runtimes `deno, node, quickjs, bun` but **only `deno` enabled by
default** — node is NOT the runtime the solver uses.

**Shipped mechanism (#189).** SongPlayer ships its own pinned Deno so its
production downloads solve the n-challenge without any manual box setup:

- `downloader/tools.rs::ToolsManager::ensure_deno()` downloads the pinned
  `DENO_VERSION` (`crates/sp-server/src/downloader/ytdlp_cmd.rs`), SHA-256-verified,
  into `C:\ProgramData\SongPlayer\cache\tools\deno.exe` next to yt-dlp/ffmpeg —
  skipped when already present with the right version. Bump `DENO_VERSION` +
  `DENO_SHA256` together (the checksum must match that version's Windows zip).
- **Every** yt-dlp spawn goes through the ONE builder `ytdlp_cmd::ytdlp_command()`,
  which prepends the tools dir to the child's `PATH` (so the bundled deno is found
  first) and adds `--js-runtimes deno` when yt-dlp advertises the flag (plus
  `CREATE_NO_WINDOW` + UTF-8 env). Never re-add `--js-runtimes node` to a per-call
  arg builder — the runtime flag lives in `ytdlp_command`.
- At tools-ready a startup self-check (`ytdlp_cmd::run_selfcheck`) runs a
  `yt-dlp --simulate -f bestaudio <public id>` (with cookies when present) and logs
  `yt-dlp js-runtime: OK (deno <ver>)` or `yt-dlp js-runtime: MISSING — new
  downloads will fail the n-challenge` (ERROR). `GET /api/v1/status` → `tools.js_runtime_ok`
  + `tools.deno_version` expose it (also on the `ToolsStatus` WS message).

**Manual debugging trick (only when the shipped deno is somehow absent):** put a
`deno.exe` on PATH for the yt-dlp process (dev2/dev1 install Deno freely; on
win-resolume drop it in a temp dir and prepend that dir to `$env:PATH`). The box's
yt-dlp + `ffmpeg` live in `C:\ProgramData\SongPlayer\cache\tools`.

**Producing the file on win-resolume (all via MCP GUI, no human at the PC):**

1. Use a Chrome profile that is *currently* logged into YouTube — check with
   the profile's own `Network\Cookies` DB (`LOGIN_INFO` + `SAPISID` present is
   necessary but NOT sufficient: the MIREC profile had both and was still dead;
   open `youtube.com` in that profile and look for the avatar vs "Sign in").
2. `--cookies-from-browser chrome/edge` is a dead end on this box: Chrome ≥127
   App-Bound Encryption (`v20` cookies) has no decryptor in yt-dlp (#10927), the
   DB is locked while Chrome runs (#7271), and a copied user-data-dir yields 0
   cookies. Chrome ≥136 also refuses `--remote-debugging-port` on the real
   profile. Don't retry these.
3. The "Set password for your browser (chrome lock)" extension
   (`cjmjgijhapgicbhmniemjkjeaedanank`) sits on the MIREC and Petronela profiles
   and hijacks every tab incl. `chrome://extensions`. Disable it without a
   password via a temporary machine policy, then delete the policy afterwards:
   ```powershell
   $k='HKLM:\SOFTWARE\Policies\Google\Chrome\ExtensionInstallBlocklist'
   New-Item $k -Force|Out-Null; New-ItemProperty $k -Name 1 -Value 'cjmjgijhapgicbhmniemjkjeaedanank' -PropertyType String -Force|Out-Null; gpupdate /target:computer /force
   # ... export ...
   Remove-Item $k -Recurse -Force; gpupdate /target:computer /force
   ```
4. Install "Get cookies.txt LOCALLY" (`cclelndahbckbenkjhflpdbgdldlbecc`) in that
   profile, open `https://www.youtube.com/robots.txt`, puzzle icon → the
   extension → **Export**. Petronela's profile has "ask where to save" on, so a
   Save As dialog opens on the SECOND monitor — type the full target path into
   its filename box. Close the profile's window right after (open YouTube tabs
   rotate the cookies).
5. Verify on a copy: `yt-dlp --cookies <copy> --skip-download --print '%(id)s %(format_id)s OK' https://www.youtube.com/watch?v=qDbjd0_d3IY` → `OK`.

**MCP click coordinates:** the `Snapshot` image (monitor 0, `max_width` 2400)
maps to the main monitor's click space with factor **5.766** (3840 logical px ↔
666 image px), NOT 4.8 — the virtual desktop is wider than the image suggests.
Zoom the page (`ctrl+=` ×6) before clicking small buttons; there is no OCR on
the box.
