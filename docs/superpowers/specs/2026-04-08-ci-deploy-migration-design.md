# CI Hardening, Deployment & YTPlayer Migration

**Date:** 2026-04-08
**Status:** Draft

## Summary

Add self-hosted deployment and E2E testing to SongPlayer CI pipeline, targeting win-resolume (10.77.9.201). Migrate existing OBS YouTube Player configuration (6 playlists, settings, OBS scenes) into SongPlayer so it can fully replace the Python script.

## Background

SongPlayer v0.2.0 is built and CI-green but has no deployment pipeline. The win-resolume machine runs:
- OBS Studio with 6 ytplayer Python script instances (6 playlists, ~30GB cached videos)
- Resolume Arena
- RemoteOS MCP (port 8090)
- OBS MCP via supergateway (port 8091, just installed)
- GitHub Actions runner (just registered, labels: `self-hosted, Windows, X64, resolume`)

## 1. CI Pipeline — Deploy Job

### Job: `deploy-resolume`

- **Runs on:** `[self-hosted, windows, resolume]`
- **Depends on:** `build-tauri` (success) + `gate` (success)
- **Trigger:** push to `dev` or `main` branch (not PRs)
- **Concurrency:** cancel-in-progress within the deploy group

**Steps:**
1. Clean previous artifacts (self-hosted runner hygiene)
2. Download `tauri-installer` artifact from `build-tauri` job
3. Stop SongPlayer if running: `taskkill /F /IM SongPlayer.exe` (ignore errors)
4. Wait for port 8920 (sp-server default) to be free
5. Run NSIS installer with `/S` (silent) flag
6. Create/update scheduled task `SongPlayer` for auto-start at logon (Interactive, user `Resolume`)
7. Start via `schtasks /Run /TN SongPlayer`
8. **Health checks** (up to 30s):
   - Process `SongPlayer.exe` running
   - API responds: `GET http://localhost:8920/api/v1/status` returns version matching VERSION file
   - OBS WebSocket connection status (from `/api/v1/status` response)

### Job: `e2e-resolume`

- **Runs on:** `[self-hosted, windows, resolume]`
- **Depends on:** `deploy-resolume` (success)
- **Condition:** `always() && needs.deploy-resolume.result != 'failure'`

**Steps:**
1. Verify SongPlayer API is responding
2. Create a test playlist via API, trigger sync, verify videos appear
3. Verify OBS WebSocket connection is active (settings must be seeded)
4. Verify OBS text source can be updated (write test text, read back)
5. Cleanup test data

## 2. Self-Hosted Runner Setup Script

Create `scripts/setup-runner.ps1` following the `irm | iex` pattern:
```
irm https://raw.githubusercontent.com/zbynekdrlik/songplayer/dev/scripts/setup-runner.ps1 | iex
```

The script handles:
- Download and extract GitHub Actions runner
- Register with repo using a token (prompted or from env)
- Create scheduled task for auto-start
- Firewall rules
- Verify runner comes online

**Note:** Runner is already manually installed and online. The script is for reproducibility and documentation.

## 3. YTPlayer Migration

### Data to Migrate

**6 Playlists** (each becomes a SongPlayer playlist record):

| Name | YouTube Playlist ID | OBS Scene | OBS Video Source | OBS Text Source |
|------|-------------------|-----------|-----------------|----------------|
| ytwarmup | PLFdHTR758BvcHRX3nVKMEPHuBdU75dBVE | ytwarmup | ytwarmup_video | ytwarmup_title |
| ytpresence | PLFdHTR758BveAZ9YDY4ALy9iGxQVrkGRl | ytpresence | ytpresence_video | ytpresence_title |
| ytslow | PLFdHTR758Bvd9c7dKV-ZZFQ1jg30ahHFq | ytslow | ytslow_video | ytslow_title |
| yt90s | PLFdHTR758BvfM0XYF6Q2nEDnW0CqHXI17 | yt90s | yt90s_video | yt90s_title |
| ytworship | PLFdHTR758BveEaqE5BWIQI7ukkijjdbbG | ytworship | ytworship_video | ytworship_title |
| ytfast | PLFdHTR758BvdEXF1tZ_3g8glRuev6EC6U | ytfast | ytfast_video | ytfast_title |

**Settings:**
- `gemini_api_key`: `AIzaSyCOBTFHGRBW3gBas9Qxp88InzQCoOGhnQI` (shared by 5/6 instances, ytfast has none)
- `obs_websocket_url`: `ws://localhost:4455` (auth not required)
- `cache_dir`: `C:\ProgramData\SongPlayer\cache` (new centralized location vs per-instance `cache/` dirs)
- Playback mode: all use `continuous`

**Cached Videos (~30GB):**
- Existing normalized MP4s can be migrated or re-downloaded
- Re-downloading is simpler (SongPlayer will sync and process them) but slow
- Migration script could copy existing cache files and register them in the DB

### Migration Approach

**Option chosen: Seed playlists + settings, let SongPlayer re-sync and re-download.**

Rationale:
- Cached video filenames may not match SongPlayer's naming convention
- Re-downloading ensures consistent metadata extraction
- 30GB download over good internet is a one-time cost
- Avoids complex file-to-DB mapping logic

**Migration script** (`scripts/seed-data.ps1` or API calls in E2E):
1. POST 6 playlists via `/api/v1/playlists`
2. PATCH settings via `/api/v1/settings` (gemini key, OBS WebSocket URL, cache dir)
3. Trigger sync for each playlist via `/api/v1/playlists/{id}/sync`

### Cutover Plan

1. Deploy SongPlayer, seed playlists, let it sync and download
2. Verify SongPlayer can play videos and update OBS sources
3. Disable ytplayer Python scripts in OBS (Tools → Scripts → uncheck each)
4. SongPlayer takes over OBS source control
5. Old ytplayer cache dirs can be cleaned up after verification

## 4. OBS MCP Server

Already installed and running:
- `obs-mcp` + `supergateway` on port 8091
- Scheduled task `ObsMCP` for auto-start
- Firewall rule `OBS MCP SSE` for port 8091
- Config at `.mcp.json`: `obs-resolume` with SSE transport

## Architecture Decisions

- **NSIS installer for deployment** — consistent with restreamer, handles shortcuts/registry/uninstall
- **Scheduled task for auto-start** — Interactive logon type for GUI access (Tauri window)
- **Self-hosted runner on same machine** — deploy + E2E on win-resolume directly
- **Re-download over migrate cache** — simpler, more reliable, one-time cost
- **Incremental DB migrations** — once deployed with real playlists, schema changes must be incremental
