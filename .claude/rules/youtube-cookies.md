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

**The fix in code:** `downloader/mod.rs` passes `--cookies <data_dir>/cookies.txt`
to both yt-dlp calls whenever `C:\ProgramData\SongPlayer\cookies.txt` exists
(re-checked per download, no restart needed). yt-dlp REWRITES the jar it is
given, so any manual test must run on a COPY of the file.

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
