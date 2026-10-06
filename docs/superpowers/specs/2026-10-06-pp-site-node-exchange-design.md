# SongPlayer at the PP site, fed by SNV: node exchange design

Ticket: #229.

Decisions are recorded on #229:
- approach A (exchange between nodes) and "start copying now";
- the topology: PP is wired exactly as SNV;
- sections 1–3, all approved by the owner on 6.10.2026.

## Goal

The PP church site gets a SongPlayer that works like SNV's, soon. Its content comes from SNV: no days of re-downloading and re-processing.

Long term, every SongPlayer node first asks the other nodes before any heavy job. **No node redoes the processing another node already did.**

Owner, 6.10.2026:

> "treba skusit vymysliet koncepcne spravne riesenie ktore dava zmysel tak aby kazdy nod nemusel robit full prcessing vsetkeho co uz urobil iny nod"

PP may still process. The rule is "ask first, then process".

## Sites today

| | SNV (`resolume.lan`, 10.77.9.201) | PP (`resolume-pp`, 10.77.8.201, NATed from SNV as 10.76.8.201) |
|---|---|---|
| SongPlayer | yes, the dev rig (CI deploys every dev push) | not installed |
| LED wall Arena | "bridge" (7.28), every deck on `SP-program-MAX` (Spout) | "bridge" (`Arena-Bridge.exe` as user `bridge`, 7.22.1), clips on cg OBS (Spout `cg-obs` / NDI) |
| Lyrics Arena "songs" | on a separate PC | on the same PC (`Arena.exe` as `newlevel`), untouched |
| cg OBS | only the "OBS manuál" input to SongPlayer | portable OBS (`_APPS\cg_obs`), still runs the legacy Python YouTube player |
| Startup | scheduled tasks | `NL_STARTUP.ahk` keeps bridge, songs and cg OBS running |

**Network:**
- Both sites use `10.77.8.0/23`.
- SNV hosts that the router lets through (dev1) reach PP at `10.76.8.201`. PP cannot open connections to SNV. win-resolume itself cannot reach PP either.
- PP has internet.
- SNV's SongPlayer is public as `sp.newlevel.media` behind Cloudflare Access.

## Phase 0: fast start of PP, no new code

**1. Content copy (running since 6.10.2026 16:27Z).**
- `ops\229\sync_to_pp.ps1` on win-resolume copies the top-level files of `cache\` to PP over sftp, with dev1 as an ssh jump only (restricted key `songplayer-snv-pp-sync`).
- It leaves out `*_vocals16k.wav`, resumes partial files and ships a consistent DB snapshot.
- ~115 GB at ~50–85 Mbit/s. Procedure: #229 comment "Initial copy SNV → PP".

**2. Install** the current build with the CI's own steps:
- installer `/S`, the WASM `dist` into the install dir;
- the data dir ACL;
- an at-logon scheduled task (Interactive, `RunLevel Highest`, priority 4) for the PC's logged-on user;
- firewall 8920 and 4456;
- WebView2 if missing.

**3. Seed:**
- the DB snapshot becomes PP's `songplayer.db` before the first start;
- a final delta copy runs just before the start.

**4. PP settings,** written to PP's DB before the first start:
- `lyrics_worker_enabled=false`, `stem_worker_enabled=false` (no processing until phase 1);
- the OBS websocket of PP's cg OBS;
- `ndi_input_source` = PP's cg OBS NDI output;
- the Resolume host = Arena "bridge"'s REST port (its own webserver port, since songs runs too);
- the VBAN targets = PP's FOH;
- the Presenter URL for PP (or none);
- `remote_ws_enabled=true`.

Every value is read on site. Any setting copied from SNV that points at an SNV machine is changed or cleared, so that PP never drives SNV's devices.

**5. Rework at PP** (backups first: `Bridge PP.avc`, cg OBS's config dir):
- Arena "bridge": clips on `cg-obs` / `cg_obs` (Spout) and `OBS-*(CG_Obs)` (NDI) move to `SP-program-MAX`, by the offline `.avc` method proven at SNV (`win-resolume-ops` skill);
- add SongPlayer's `#sp-title`, `#sp-subs`, `#sp-subssk` clips as at SNV;
- cg OBS: stop the legacy YouTube player and its outputs (scripts, scenes, VBAN);
- Companion at PP → SongPlayer's facade (:4456).

**6. Verify on site:**
- every playlist plays;
- the wall shows MAX with title and lyrics;
- a manual scene through Companion reaches the wall via "OBS manuál";
- FOH has audio.

**Until phase 1,** new songs at SNV reach PP by re-running the copy script; PP processes nothing.

## Phase 1: the node exchange

**Nodes and peers.**
- Settings: `node_name` (`snv`, `pp`) and a peer list. Each peer has a base URL, an optional Cloudflare Access service token (client id + secret) and an app-level peer key.
- PP → SNV goes through `https://sp.newlevel.media` with an Access service token: a non-identity policy on the existing Access app (`scripts/cloudflare/README.md`, "service token").
- SNV → PP needs a tunnel at PP. It is needed only once PP has content SNV lacks (phase 2).

**The peer API** (`/api/v1/peer/…`):
- Each request needs the app-level peer key header. On the public path, Cloudflare Access is in front as well.
- Keys travel only through the secret channel and are never logged.
- `GET /api/v1/peer/catalog?since=`: the artifacts the node has and the jobs it is running. An artifact is `{youtube_id, kind, version, size, sha256, updated_at}`; a job is `{youtube_id, kind, node, started_at}`.
- `GET /api/v1/peer/videos/{youtube_id}`: the row's metadata (song, artist, `metadata_source`, lyrics source and pipeline version), so a node adopts it without re-running the providers.
- `GET /api/v1/peer/artifact/{youtube_id}/{kind}`: the file, with HTTP Range (resumable).

**Artifact kinds:** `video`, `audio` (the normalized FLAC), `stem_vocals`, `stem_instrumental`, `lyrics` (JSON plus its pipeline version), `metadata`. Later `dub`.

**Ask first.** Before a heavy job (download, normalize, metadata, lyrics, stems, dub) for `(youtube_id, kind)`, the node asks its peers:
- **a peer has it** at the current version → fetch, check the sha256, name the files from the node's OWN row (#136 file-set rules, `cache::rename_song_files`), and mark the job done with `source = peer:<node>`;
- **a peer is running it** → defer: re-check with backoff, bounded at ~2 h, then process locally;
- **nobody has it** → process locally and announce the job in its own catalog while it runs.

**Playlists.** Each node syncs its own playlist membership from YouTube (cheap, flat). Only processed content is exchanged. That keeps PP's own playlists open for later.

**Thrift:**
- transfers run in the background at low priority and resume after an interruption;
- the #230 block ("5min" scene) pauses them too;
- one transfer at a time per peer.

**Tests:**
- a two-node rig in the test suite (two servers, two pools, real HTTP): a peer has the artifact, so the other node fetches and processes nothing; a peer is running the job, so the other waits; nobody has it, so the node processes and announces;
- sha256 mismatch → discard and retry;
- a resumed Range download;
- the key refusal paths;
- live post-deploy check: PP reads SNV's catalog through Cloudflare.

## Deploy and tests for PP

- A self-hosted GitHub Actions runner on `resolume-pp`, label `resolume-pp`.
- **PP gets main releases only**, never every dev push. SNV stays the dev rig; dev builds at PP only as an explicit option.
- Post-deploy at PP: the dashboard version, an SP-program receiver, playback and MAX, the facade and OBS manuál, the live peer check. The A/V gate comes later, once recording is set up at PP.
- PP's settings live in PP's DB; no deploy overwrites them.
- "produkcia beží" at PP stops CI and every touch at PP, as at SNV.

## Later (not in this design)

- PP's own playlists and dabing, served to SNV through the exchange (needs the PP tunnel).
- #230 at both sites.

## Risks

- **Settings copied from SNV that point at SNV devices** (Presenter, VBAN, OBS) must be rewritten before PP's first start. Phase 0 step 4 reads every key.
- **Two Arenas on one PC:** SongPlayer must target bridge's REST port, never songs'.
- **The Arena version gap** (7.22 at PP, 7.28 at SNV): the `.avc` edits and REST behaviour are verified on PP's version before relying on them.
- **Cloudflare upload from SNV:** phase 1 transfers ride SNV's uplink. They run at low priority and pause on #230.
