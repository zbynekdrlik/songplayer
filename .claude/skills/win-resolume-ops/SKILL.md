---
name: win-resolume-ops
description: >
  Songplayer operations on the win-resolume machine (10.77.9.201). Load when
  doing deployments, CI monitoring, runner checks, Resolume diagnostics, or any
  work that touches the live Windows machine — covers OBS/Resolume safety rules,
  runner health, CI cancel policy, and shared-machine discipline.
user-invocable: false
triggers:
  - win-resolume
  - resolume.lan
  - OBS
  - Resolume Arena
  - 10.77.9.201
  - CI deploy
  - runner
  - SongPlayer.exe
---

# win-resolume Machine Operations

## Machine config

- **IP:** 10.77.9.201 (resolume.lan)
- **User:** Resolume
- **Services:** OBS (port 4455 WebSocket), Resolume Arena, RemoteOS MCP (port 8090),
  OBS MCP via supergateway (port 8091), GitHub Actions runner, SongPlayer (port 8920)
- **SongPlayer data:** `C:\ProgramData\SongPlayer\`
- **SongPlayer install:** `C:\Program Files\SongPlayer\`

## Public URL `sp.newlevel.media` — Cloudflare Tunnel, TCP-only on this network

The dashboard is published through a token-run Cloudflare Tunnel (`Cloudflared`
Windows service, tunnel `0242c8d3-…`, token file
`C:\ProgramData\cloudflared_tunnel_token.txt`). Ingress is managed remotely in
the Cloudflare dashboard, so there is no local ingress config to inspect.

**Symptom → cause map when the domain is down:**

| What you see | What it means |
|---|---|
| HTTP **530 / `error code: 1033`** | No connector registered — the tunnel is down. The origin is irrelevant; check the service, not SongPlayer. |
| `Cloudflared` service `Running` but the Application event log shows *"Cloudflared service starting"* every ~40 s | Crash loop. `sc.exe qfailure Cloudflared` shows `RESTART -- Delay = 20000 ms`, so a dead connector looks alive in `Get-Service`. |
| stderr `failed to dial to edge with quic: timeout: handshake did not complete in time` | **UDP 7844 is blocked on the current network** — the 2026-08-07 outage, which began at a reboot after the LAN was switched. |

**The fix (already applied, persists across reboots):** force the TCP transport.
The service `binPath` now carries `--protocol http2`:

```powershell
$tok = (Get-Content C:\ProgramData\cloudflared_tunnel_token.txt -Raw).Trim()
$bp  = '"C:\Program Files\cloudflared\cloudflared.exe" tunnel --no-autoupdate --protocol http2 run --token ' + $tok
(Get-WmiObject Win32_Service -Filter "Name='Cloudflared'").Change($null,$bp)   # 0 = OK
Restart-Service Cloudflared -Force
```

Read the token from that file — never type or echo it. Confirm with
`Test-NetConnection region1.v2.argotunnel.com -Port 7844` (TCP reachable while
QUIC is not) and expect four `Registered tunnel connection … protocol=http2`
lines within seconds. Verify from the dev side, not the box:
`curl -s -o /dev/null -w '%{http_code}' https://sp.newlevel.media/` → **`302`**
(since #155 the public hostname is behind Cloudflare Access — a 302 to
`newlevelchurch.cloudflareaccess.com` is the healthy "tunnel up" signal, NOT a
`200`; see the Cloudflare Access subsection below). To confirm the ORIGIN behind
the tunnel is serving, check the on-box LAN path instead:
`Invoke-WebRequest http://127.0.0.1:8920/api/v1/status` → `200`.

Diagnose the service's own stderr by launching a SECOND short-lived copy with
`Start-Process -RedirectStandardError` (extra connectors are harmless) — the
Windows service itself writes only "starting"/"stopped" to the event log and
discards cloudflared's real output.

### Public dashboard is behind Cloudflare Access (email OTP) — since 2026-09-14 (#155)

`sp.newlevel.media` is protected by a **Cloudflare Access** (Zero Trust) app with
a One-time PIN identity provider and an e-mail allowlist (the 3 owners). Only the
**public hostname** is gated — the LAN path (`http://10.77.9.201:8920`,
`sp.local`) is untouched. So the healthy signals differ by path:

- **Public** `curl -sI https://sp.newlevel.media/` → **302** to
  `newlevelchurch.cloudflareaccess.com/cdn-cgi/access/login/...` is now the
  CORRECT "up" signal, **not** a fault. A `200` from the public hostname without a
  logged-in `CF_Authorization` cookie would mean Access is OFF (the #155 bug).
- **LAN / on-box** `Invoke-WebRequest http://127.0.0.1:8920/api/v1/status` → **200**
  is the origin-health check (Access does not sit on the local port).
- Access covers the whole hostname with **no path exclusions**, so the dashboard
  WebSocket `/api/v1/ws` also rides the `CF_Authorization` cookie once logged in.

Full config (app/policy ids, add/remove an email, rollback, the service-token
option for automated public-hostname checks) is in `scripts/cloudflare/README.md`.
The Cloudflare API token lives at `~/.secrets/cloudflare-newlevel-access` on the
dev box — never echo or commit it.

## MCP tool traps (cost two agents hours on 2026-08-05)

- **`mcp__win-resolume__FileWrite` SILENTLY TRUNCATES `content` over ~20,000
  characters.** It raises no error — it writes less than you passed and reports
  the smaller count, so the file looks written and then fails at runtime in a
  confusing way. Write large files in `append: true` chunks and VERIFY the remote
  size/line count afterwards.
- **PowerShell mangles inline python.** `python -c "..."` breaks on `$`, quotes
  and backticks (a SQL `length(value)` became *"The term 'value' is not
  recognized as the name of a cmdlet"*). ALWAYS `FileWrite` a `.py` file, then
  run it with `Shell`.
- **`mcp__win-resolume__Shell` STRIPS the PowerShell `$_` automatic variable from
  the command string** (a `$_.Line` arrives as a bare `.Line` → *"Unexpected token
  '.Line'"*; `'STATUS_ERR: ' + $_.Exception.Message` → *"You must provide a value
  expression following the '+'"*, #171 2026-09-17). So NEVER use `$_` in a Shell
  one-liner: emit a property directly (`(Select-String … -Pattern 'x').Line | Select -Last 6`,
  not `… | %{ $_.Line }`), or `FileWrite` a `.ps1`/`.py` and run the file. Quick
  read-only probes that DO survive: `curl.exe -s http://127.0.0.1:8920/api/v1/…`,
  `Select-String -Path <log> -Pattern '…'`. Also: a RECURSIVE `Get-ChildItem` over
  `C:\ProgramData\SongPlayer` times out (270k+ cache files) — target the log dir
  (`songplayer.<date>.log`) or `-File` at the root directly.
- **`mcp__win-resolume__FileRead` truncates around ~100,000 characters** (also
  undocumented) and per-file MCP round trips are slow. To pull a BATCH of files
  back, start a temporary `python -m http.server` on the box, `curl` them from
  the dev side, then stop it.
- **`Shell` + `Start-Process -RedirectStandardOutput` BLOCKS until the child
  exits, and after the tool's timeout the server RE-RUNS the command with the
  system `C:\Program Files\Python312\python.exe`** — a long eval batch was
  running twice (double API calls, 429s) on 2026-09-12. Launch background work
  with `Invoke-CimMethod -ClassName Win32_Process -MethodName Create
  -Arguments @{CommandLine='cmd.exe /c ""<python>" -u "<script>" > "<log>" 2>&1"'}`,
  which returns at once, then poll the log. Kill strays via
  `Get-CimInstance Win32_Process | Where CommandLine -match '<script>'`.
- MCP is the ONLY sanctioned channel here — never ssh/scp to this box. If an MCP
  call fails with a connection/timeout error, STOP and tell the user.

## This box IS the local-model machine (not dev2)

Local ML models run HERE, not on any other box. Established venvs under
`C:\ProgramData\SongPlayer\cache\tools\`: `whisperx_venv`, `crisper_venv`,
`parakeet_venv`, `vibevoice_venv`, `lyrics_venv`, plus an `hf_models` HuggingFace
cache. When a task needs a local model, inspect those first and mirror a proven
torch/CUDA build rather than starting from zero — and never propose moving this
project's model work to another machine.

Also here: system python `C:\Program Files\Python312\python.exe` (3.12.10),
eval fixtures at `C:\ProgramData\SongPlayer\eval-cache\`, eval scaffolding at
`C:\ProgramData\SongPlayer\eval-run\`, API keys in the `settings` table of
`C:\ProgramData\SongPlayer\songplayer.db`.

## Subprocess priority — never saturate the machine

The Windows machine running OBS + Resolume + SongPlayer is the LIVE event PC.
Demucs, Gemini processing, and heavy subprocess work cause hardware overload and
reboots (2026-04-21 incident). Always use `BELOW_NORMAL` priority for background
subprocesses. Leave headroom for OBS + Resolume.

## OBS — never kill

NEVER `taskkill /F` OBS. Doing so causes a "restore dialog" on next start that
gets stuck. Use OBS's graceful shutdown only. If OBS is unresponsive and needs
a restart, inform the user.

## SongPlayer — graceful shutdown only

Never `taskkill /F /IM SongPlayer.exe` without explicit user approval. Force-
killing SongPlayer can send the wall dark mid-event. Use the graceful stop
endpoint or ask the user to restart.

Exception: when user has explicitly stated win-resolume is dedicated for dev
(no event running), restart-class actions can proceed without per-call approval.
Force-kill and machine reboot still always require approval.

## Resolume Arena — quit, crash and relaunch (2026-09-27/28)

- **Crash cause (7.27.1).** Arena 7.27.1 aborted about daily (`ucrtbase` 0xc0000409, fast-fail 7): an uncaught C++ exception in `WireLib.dll`, with an identical stack in 5 minidumps. It was upgraded to **7.28.0 rev 24303** on 27.9. The backup is `C:\Users\Resolume\ArenaBackup-20260927-pre-7.28.0` (#217).
- **Graceful quit.** Post `WM_CLOSE` to the main window titled `Resolume Arena - <composition>` (not the `Display` output windows), then click **Save & Quit** in the "Quit!" dialog.
  - 7.27.1 then crashed in teardown (BugSplat "Crash Report": close it with `WM_CLOSE`, never send).
  - 7.28.0 can hang in "Clean up Compositon". Once `Bridge.avc` is saved, kill it and run `SP-ArenaLaunch`.
- **After a relaunch:**
  - Arena's REST answers before its composition loads, and every param gets a new id. SongPlayer handles this (see `.claude/rules/resolume-driver.md`).
  - An Arena launch triggers Process Lasso's gaming mode, which once flipped the power plan to Balanced. camera-box fixed prolasso.ini, but tell camera-box the relaunch time.
- **DMX/ArtNet** is bound to `Localhost`. The old `ethernet_32775` adapter does not exist and made Arena retry every 1.5 s.
- **Screenshots.** MCP `Snapshot` shows the black output monitors. For the UI, use a PowerShell `CopyFromScreen` of the region (0,0 is the 3840×2160 UI screen), served via a fresh temp dir + `python -m http.server --bind 10.77.9.201`.

## Resolume Arena — overnight hang pattern

Resolume Arena on win-resolume frequently becomes unresponsive after a night of
CI activity. The window still paints but the REST server at
`http://127.0.0.1:8090/api/v1/composition` returns "connection refused".

When user reports "no lyrics on Resolume" / "no title" in the morning:
1. Check Arena REST FIRST: `Invoke-WebRequest http://127.0.0.1:8090/api/v1/composition -UseBasicParsing -TimeoutSec 3`
2. Check process: `Get-Process Arena | Format-List Name, Id, Responding`
3. If `Responding=False` or REST is down (connect refused / timeout with a
   live listener on 8090) — Arena's REST is hung.
4. Owner directive 2026-09-14: in a window where Claude is allowed to work,
   a hung Arena MAY be killed without asking (`Stop-Process -Name Arena -Force`
   via MCP). A hotkey script normally relaunches it — wait ~30 s and check
   `Get-Process Arena` before starting it yourself (`Start-Process
   'C:\Program Files\Resolume Arena*\Arena.exe'`). Never a second instance.
   OBS stays never-kill. Root cause of the hang: #157 (light polling).
5. After Arena restart, SongPlayer clip mapping refreshes automatically —
   confirm `/api/v1/product` answers and the `no Resolume subtitle clips found`
   warnings stop before re-running E2E.

**How SongPlayer polls Arena now (#157, v0.50.0-dev.2+):** the driver no longer
pulls the ~14 MB `/composition` every 10 s (that saturated Arena's single-thread
REST and caused the hang above). It probes the LIGHT `GET /api/v1/product` every
~10 s (±2 s jitter) for liveness, and does the full `/composition` clip-map
refresh only on start, on a `RefreshMapping` command, on a 5-min TTL, and once
when the circuit breaker closes. Measure the effect on the box via
`GET /api/v1/resolume/health`: each host snapshot now carries
`product_latency_ms` (last `/product` round-trip — expect low ms when Arena is
healthy, rising/`null` when its REST is saturating) and `last_full_refresh_ts`
(when the heavy fetch last succeeded — should tick roughly every 5 min, not
every 10 s). `consecutive_failures`/`circuit_breaker_open` now trip on the light
probe, so a wedged REST is detected without adding load.

Arena can also be GONE (no process at all, no crash event, 20.9.2026): the
AutoHotkey relaunch script only fires after a kill, not after a self-exit —
launch it yourself via the MCP `App` tool (`C:\Program Files\Resolume
Arena\Arena.exe`), poll `/api/v1/product` (≈8 s), then read
`/api/v1/resolume/health` on SongPlayer (`clips_by_token` non-zero) before
re-running the E2E job.

**NTFS stale metadata trap:** `Get-ChildItem` shows a log file that a running
process holds open with a FROZEN size/LastWriteTime (the directory entry is
updated only on close). `songplayer.<date>.log` looked dead for 6 h after a
restart; `Get-Content -Tail` / `Select-String` refreshes it. Never conclude
"the process is not logging" from a listing — read the file.

When E2E CI cancels mid-step: default first hypothesis is Resolume Arena stuck.
Diagnose with `Get-Process Arena | Format-List Responding` and
`curl 127.0.0.1:8090` before assuming SongPlayer code bug. Fix Resolume, then
re-run CI.

## CI deploy monitoring — runner health

When `Deploy to win-resolume` stays `queued` for >2 minutes:
1. `ping 10.77.9.201` — is the machine up?
2. `gh api repos/zbynekdrlik/songplayer/actions/runners` — is the runner online?
3. If ping fails or runner offline: tell user IMMEDIATELY. Do NOT wait silently.

Self-hosted runners pick up queued jobs within seconds when online. >2 min
queued = machine powered off or runner service stopped.

## CI cancel policy — event window only

In this repo, during **active live-wall events** (user has explicitly said an
event is running), cancel remaining CI jobs the moment `Deploy to win-resolume`
reports `success`:
```bash
gh run cancel <run_id>
```

Do NOT auto-cancel outside event windows. When there is no live wall running,
let the full pipeline (E2E, 30-min snapshot) complete — the data is useful.

**Event status is user-authoritative.** NEVER infer "event in progress" from
OBS scene state (`sp-live`, `sp-fast`, etc.), playlist activity, or wall
traffic. The user's explicit statement is the only signal.

## CI polling — short intervals

Never `sleep 1800` or `sleep 2400` as a single blocking wait for CI. Use a
Monitor or background poll that wakes every 60-300s and catches BOTH success
AND failure states. A failure at minute 5 must surface in minutes, not 40.

## Windows installs — irm | iex pattern

Use the one-line PowerShell installer pattern for setting up services:
```powershell
irm https://raw.githubusercontent.com/owner/repo/branch/scripts/install.ps1 | iex
```
Create an `install.ps1` in the repo that handles download, config, scheduled
task, firewall, and verification. Not manual multi-step commands.

## Dialogs hidden behind Arena's output windows — drive them with UI Automation

Arena's fullscreen "Display" windows cover monitors 2-4, so a Qt dialog that
opens there (OBS "Crash Detected" / Safe Mode prompt, 2026-09-12) is
invisible to `Snapshot` and `FocusWindow` fails. Do not click blind — from
the MCP `Shell`, read and press its buttons by name (never pick OBS Safe
Mode: it disables NDI and obs-websocket):

```powershell
Add-Type -AssemblyName UIAutomationClient; Add-Type -AssemblyName UIAutomationTypes
$root = [System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]<hwnd from the Snapshot window list>)
$c = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::NameProperty, 'Run in Normal Mode')
$root.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $c).GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
```

List buttons/text first (`ControlType.Button` / `.Text` with `FindAll`) when the
labels are unknown. Launch GUI apps that need a working directory via
`Invoke-CimMethod Win32_Process Create -Arguments @{CommandLine=…; CurrentDirectory=…}`
(OBS needs `bin\64bit`); the MCP `Shell` sometimes returns "(no output)" for
longer commands — redirect to a log file and read that instead.

## MCP `Shell` / `App` gotchas (box test 8, 21.9.2026)

- **A `Shell` call that runs > ~25 s is killed with "Command timed out after
  28s" and NO partial output.** `Get-CimInstance Win32_Process` (10+ s),
  `Start-Process` + a long `Start-Sleep`, and a TCP peer lookup per connection
  all blew it. Keep every call to ONE thing; put multi-step probes in a `.ps1`
  under `C:\ProgramData\SongPlayer\cache\tools\` (`FileWrite`, then
  `powershell -NoProfile -ExecutionPolicy Bypass -File …`). Existing probes
  there: `threads.ps1` (per-thread CPU/priority/state of a PID),
  `timerres.ps1` (timer resolution + PowerThrottling + window visibility),
  `hold_cuda.py` / `hold_mem.py` / `hold_cpu.py` (controlled load children).
- **A child launched from the MCP `Shell` (`Start-Process`) is CPU-THROTTLED to
  ~0.4 core** (it inherits the MCP server's job object) — `hold_cpu.py` with 3
  spinning threads measured 0.38 core, threads in `Ready`. A load experiment
  that needs real CPU must be launched via a scheduled task, never from the MCP
  shell. Memory-only / CUDA-context-only holds are unaffected.
- **Arena relaunched through the MCP `App` tool inherits `BelowNormal`
  priority class** (pid 20176 on 20.9. ran at BelowNormal for 22 h). After any
  MCP relaunch: `(Get-Process Arena).PriorityClass` → set `'Normal'` at runtime
  (`$p.PriorityClass = 'Normal'`, no restart).
- `ProcessThread.IdealProcessor` is WRITE-ONLY in PowerShell — reading it throws
  per thread and empties the row set; use `BasePriority`/`CurrentPriority`/
  `ThreadState`/`WaitReason` only.
- Per-process page faults, the box-wide demand-zero rate and interrupts are the
  first counters to read when "everything is slow but the CPU is idle":
  `Get-Counter '\Process(*)\Page Faults/sec','\Memory\Demand Zero Faults/sec','\Processor(_Total)\Interrupts/sec' -SampleInterval 2 -MaxSamples 2`.

## win-resolume is always free when user prompts

When the user gives a new prompt, win-resolume is ALWAYS free. Never defer
interactive work citing "wall idle window" or "event might be in progress".
If the user wanted to stop, they would stop Claude. CI uses the machine without
an idle check; Claude during active prompts can too.

## Genlock soak workflow (`.github/workflows/genlock-soak.yml`, #149)

Receiver-side ground-truth soak for the genlock chain (Genlock 4/6 lane 2).
`workflow_dispatch`-only for now (the daily `schedule` line is committed
commented-out; enable it only once pointing the A/V gate's probe for a soak
is automated, see "What it can see since #221 lane 3" below). It is NOT wired to push/PR — it deliberately waits `minutes`, which is
allowed only outside the PR pipeline (CLAUDE.md "CI architecture").

**Run it:**
```bash
gh workflow run genlock-soak.yml --ref dev -f minutes=5
# inputs: minutes (default 20), skew_bound_ms (default 20), hops (default cg-obs)
```
One job `soak` on the `[self-hosted, windows, resolume]` runner. It:
1. checks out camera-box at a PINNED commit (`fdd68e47c…`) for the verifier;
2. preflights SongPlayer `/api/v1/status` + OBS WebSocket 4455, notes what
   `SP-program` carries (source + receivers) + per-pipeline `lock_state` into
   the job summary;
3. plays the first playlist with videos (no scene switch) and, during the
   `minutes` window, samples `/api/v1/ndi/health` once per minute into
   `sp-health.csv`. #221 lane 3: SongPlayer's only NDI output is
   `SP-program`, so the receiver side sees the playlist only while it is
   SongPlayer's program source and cg OBS shows a scene receiving
   `SP-program` (e.g. the A/V gate's probe scene "A/V gate (SP-program)");
   run the soak in that state;
4. dumps the newest RESOLUME-SNV OBS log tail (last 4000 lines, byte-safe) to
   `cg-obs.log`;
5. runs `camera-box/scripts/cg-chain-verify.sh --hops cg-obs` against that log
   with `CG_CHAIN_CG_OBS_LOG=…`. **The job PASS/FAIL IS the verifier's exit
   code (exit 3 = FAIL), never SongPlayer's own counters.**

**Artifacts** (`genlock-soak-<run_id>`, uploaded `if: always()`):
- `sp-health.csv` — send-side evidence only (NOT the gate): per output per
  minute `seq, late_frames, lag_slots, repeats, resyncs, pacing.av_align_err_ms,
  lock_state, lock_reason`.
- `cg-chain.csv` — the verifier's per-hop/per-source verdict rows.
- `cg-obs.log` — the receiver log tail the verdict was computed from.
- `cg-verdict.txt` — the verifier's printed per-hop table + OVERALL PASS/FAIL.

**What it can see since #221 lane 3.** SongPlayer's only output is
`SP-program`, and cg OBS's only input of it is the A/V gate's probe
"A/V gate SP-program" (`CG_CHAIN_CGOBS_SRC_RE`; the verifier's default
`sp-.*_video` names inputs that receive nothing any more). The gate keeps that
probe IDLE outside its take, so an unattended soak reads `NO SOURCES` and exits
3 (RED): that is the probe being idle, not a regression. For a real verdict,
point the probe at `<HOST> (SP-program)` and put cg OBS on the probe scene by
hand, run the soak, then set the probe back to `""` and cg OBS back on its own
scene (the workflow header). Add the `schedule` line and the `strih`/`stream`
hops only once that is automated and the runner has the ssh/bundle-state reader
(camera-box#1294 Q10).

**Bumping the pinned camera-box ref:** replace the full 40-char SHA in the
`Checkout camera-box` step with a newer camera-box commit that still ships
`scripts/cg-chain-verify.sh` + `scripts/lib/cg-chain-verify.sh` with the
`CG_CHAIN_<HOP>_LOG` reader seam (a full SHA is required for checkout's
fetch-by-commit).

## Box verification via MCP (no ssh) — gotchas (#186)

- **`mcp__win-resolume__Shell` STRIPS PowerShell `$` variables** (a shell layer
  pre-expands `$_`, `$f`, … to empty). `ForEach-Object { $_.Line }` fails with
  "term '.Line' is not recognized"; `Write-Output ('x=' + $f.Name)` fails with
  "value expression following '+'". Write PowerShell with NO `$` vars: use
  `Select-Object -ExpandProperty Line` (not `ForEach-Object {$_...}`), and to
  filter a log by time embed the timestamp IN the `Select-String -Pattern` regex
  (`'2026-09-17T(21:59|22:00).*<msg>'`) instead of `Where-Object {$_.Line -gt …}`.
  For anything non-trivial, `FileWrite` a `.ps1` to the box then `Shell` it.
- **There is NO `GET /api/v1/playback/{id}`** — the playback routes are POST-only
  (`/play`,`/pause`,`/skip`,`/previous`,`/mode`) + `GET …/preview.jpg`. To prove
  a pipeline keeps advancing (e.g. #186 no-reload), read
  `GET /api/v1/ndi/health`: `frames_submitted_total` is MONOTONIC and stalls/
  resets on a decoder reopen, `observed_fps` dips on a dropout. Pair it with a log
  grep for the absence of the reopen message and the presence of the feature's
  own `info!` lines. curl to the box HTTP API (`http://10.77.9.201:8920/...`) is
  the sanctioned read path (the post-deploy suite uses the same), not ssh.

## Arena REST: switching a clip's NDI source (#221, 29.9.2026)

The wall's clips were repointed from `RESOLUME-SNV (cg-obs)` to `RESOLUME-SNV (SP-program)` over REST (#221 comment 5890254194). Three traps cost a dark wall for 16 s and a zoomed picture:

- **Percent-encode the source URI.** `POST /api/v1/composition/clips/by-id/<id>/open` takes a text/plain body `source:///video/RESOLUME-SNV%20%28SP-program%29`. The unencoded form (with a space and parentheses) answers HTTP 200 and changes NOTHING. For a batch, use `POST /api/v1/composition/clips/open` with `[{"target": "/composition/clips/by-id/<id>", "source": "<uri>"}]` (HTTP 204). `GET /api/v1/sources` lists the idstrings. Names, effects and params are kept.
- **Opening a source into a CONNECTED clip DISCONNECTS it** — the layer goes dark on the wall. Record which clips are connected first, then `POST /api/v1/composition/layers/<l>/clips/<c>/connect` for each of them straight after the open.
- **`resize: Original` draws the source in native pixels.** A new source with a different resolution zooms the picture (on 29.9 SP-program still carried the song's 2560×1440 against cg-obs's 1920×1080; since #223, release 0.70.0, SP-program is always 1920×1080, so this bites only a source of another size, e.g. the 3840×2160 `SP-program-MAX`). Put the clip's pre-switch size back: `PUT /api/v1/composition/clips/by-id/<id>` `{"video":{"width":{"value":1920},"height":{"value":1080}}}`.

Save the composition first (`GET /api/v1/composition` > file) so the rollback is exact.

- **A REST change lives only in Arena's MEMORY until the composition is saved (#221, 30.9.2026).** The 29.9 switch was never saved, so a Save & Quit + relaunch at 15:19Z brought back the file's cg-obs sources and the wall silently ran on cg OBS for a day. End EVERY Arena change with `POST /api/v1/composition/save` (Arena 7.28 REST, no body = the current file, 204 after ~4.6 s for the 36 MB `Bridge.avc`). Then check the file: its mtime, plus `Select-String -SimpleMatch 'SP-program'` count > 0. Back the `.avc` up to `C:\ProgramData\SongPlayer\backup\` first. The GUI path does not work over MCP: Arena will not take the foreground (SetForegroundWindow is refused even after an Alt press, and Ctrl+S never reached it), and the MCP screenshot of the desktop is black.

## Arena: moving clips in EVERY deck to a new source (#221/#223, 5.10.2026)

The REST deck-by-deck run failed. The safe path is an offline edit of the saved `.avc` plus a relaunch. Scripts are in `C:\ProgramData\SongPlayer\ops\223\`.

- **Identify a clip's source exactly.** In REST it is the first line of `clip.video.description` (`RESOLUME-SNV (cg-obs)\nNDI · 1920x1080 …`, or `SP-program-MAX`). In the `.avc` it is the clip's `<PrimarySource><VideoSource type="NDIVideoSource"><NDIVideoInfo sourceName="…"/>` (or `type="SpoutVideoSource"><SpoutInfo serverName="…"/>`).
  - NEVER match a name anywhere in the clip's video JSON. That hit clips by an effect's option list.
- **REST deck switching is not a batch tool.**
  - `POST /decks/by-id/<id>/select` lags. The deck's `selected` flag and the layers' clip ids update seconds apart: a first load takes ~18 s, a loaded deck ~1 s. Until then, `clips/by-id` of the new deck answers 404.
  - A select re-ids params, so SongPlayer's map goes stale and refreshes (expected 404 WARNs).
  - A save attempted during that churn answered **412**, and nothing was written.
- **Offline edit (`avc_to_max.py`).**
  - Write a COPY of `Bridge.avc`, replacing each matched clip's `VideoSource` with the exact form Arena itself saved for that source type.
  - Set Width/Height `default` to the new source size and keep the clip's own `value`. A value still equal to the NDI 640×480 placeholder default means "follow the live 1920×1080 source".
  - A clip named after its source (default = value = the source name) gets the new name, as Arena does on an open.
  - Prove the diff with `avc_diff.py`: everything outside the matched clips must be byte-identical, and both files must parse as XML.
- **Swap.**
  - Snapshot `/composition` first, to know which clips are live.
  - `Stop-Process Arena -Force` (no save), back up the live `.avc`, copy in the new one and check its hash, then `Start-ScheduledTask SP-ArenaLaunch`.
- **Arena does NOT reconnect clips after a relaunch.** Only Blank/BG layers came back live. Reconnect every clip that was live in the pre-kill snapshot (`reconnect.py`), or the wall shows no SongPlayer video.
- **Full `/composition` read (~15 MB):** PowerShell `Invoke-WebRequest` failed with "connection forcibly closed". Python `urllib` reads it in 0.4 s.
- **Wall check from session 0:** the scheduled task `SP-WallShot` (interactive) runs `shot.ps1` and grabs the output displays to `shots\wall_after.png`. `wallmean.ps1` gives the video area's brightness: 0 = pure black.
- **A black Spout clip:** re-triggering (`/connect`) does not bring the picture back. Re-open the source (`/open source:///video/SP-program-MAX`), then PUT the size back to 1920×1080 (#223 comment 5996266548).

## PP site — resolume-pp (#229 phase 0, 6.10.2026)

SongPlayer's second node, in Poprad. Wired like SNV: SongPlayer is the program, the
Arena "bridge" takes `SP-program-MAX`, and cg OBS is only the "OBS manuál" input.

- **Reach it:**
  - The MCP server `win-resolume-pp`, from `.mcp.json`. When the session did not
    load its tools, call it over JSON-RPC with a small client that reads
    `.mcp.json`; never print the key.
  - From SNV / dev1 the PP LAN 10.77.8.x is NATed as 10.76.8.x, e.g. the Companion
    `10.76.8.205:8000` and cg OBS `10.76.8.201:4455`.
  - Never print a process command line on the box: RemoteOS's own carries its
    auth key.
- **Machine:**
  - Windows 11 IoT Enterprise LTSC 24H2, RTX 3070 Ti Laptop GPU.
  - Arena "Bridge PP": `Arena-Bridge.exe` as user `bridge`, REST 8090, Arena 7.22.
    The composition is 1792×384, deck "Poprad", file
    `C:\Users\bridge\Documents\Resolume Arena\Compositions\Bridge PP.avc`.
  - Arena "Songs PP": `Arena.exe`, REST 8091. Leave it alone.
  - cg OBS: the INSTALLED OBS 32.1.2 (`%APPDATA%\obs-studio`, collection
    `Untitled`), websocket 4455 with no auth, NDI output `cg-obs`. The portable
    `_APPS\cg_obs` is a leftover.
- **The AHK safe loop (`NL_STARTUP.ahk`) relaunches Arena-Bridge, Arena and OBS
  one second after any of them is gone.**
  - Alt+Q pauses the loop and kills nothing; Alt+L resumes it and starts whatever
    is missing. Send them with the MCP `Shortcut` tool.
  - To edit OBS's scene collection: Alt+Q, `(Get-Process obs64).CloseMainWindow()`,
    edit the JSON, then Alt+L.
  - Never print the `.ahk` file: it holds a credential. Read it with those lines
    masked.
- **No VP9 / AV1 decoder out of the box.** LTSC ships without the Video
  Extensions, so Media Foundation fails every YouTube file ("No suitable
  transform") and a playlist on program skips about 6 songs a second (#229
  comment 6026302395). Install from the Store with `winget install --id
  9N4D0MSMP0PT --source msstore` (VP9) and `--id 9MVZQVXJBQ9V` (AV1), adding
  `--accept-package-agreements --accept-source-agreements --silent`. These are the
  same versions SNV runs. Check with `POST /api/v1/diag/decode-bench`.
- **Arena 7.22 has no REST save.** `POST /composition/save` answers 403, and
  Ctrl+S sent with `Shortcut` saved nothing. What works: `FocusWindow` "Resolume
  Arena - Bridge PP", then click the Composition menu (150,50) and Save (140,223).
  Coordinates are at 2560×1600. Unlike SNV, MCP input reaches the foreground here.
  Check the `.avc` afterwards: its mtime, and a `Select-String` count of the new
  clips.
- **Text Block clips over REST** (the `#sp-title` / `#sp-subssk` set):
  - Create one with `POST /composition/layers/<l>/clips/<c>/open`, text/plain
    body `source:///video/Text%20Block` (the name percent-encoded).
  - Name it with `PUT /composition/clips/by-id/<id>`
    `{"name":{"value":"#sp-subssk"}}`.
  - Set every source param with `PUT /parameter/by-id/<id>` from
    `clip.video.sourceparams`. Set Font first: the Style choices depend on it.
  - `Size` cannot go below 0.5. For smaller text, use the source `Scale`. It
    scales about the block centre, so a right-aligned text moves inwards.
  - Positive `Position Y` moves the text down.
  - The driver only writes text and opacity. The operator's column trigger
    connects the clips, so put them in every column that carries the MAX picture.
  - PP layout: L11 `#sp-title` (Advent Pro SemiBold, Size 0.5, Scale 0.6, right +
    top, Position X -25) and L12 `#sp-subssk` (Advent Pro Expanded ExtraBold, Size
    0.6, centre + bottom, line width 1040, Position Y -40), in columns 2, 3, 4 and
    16.
  - The video frame's inside is about x 338–1442, y 15–358. The bottom corners of
    the composition are empty.
  - PP has no Yu Mincho, SNV's title font.
- **Companion (10.77.8.205, Companion 5.0.6, obs-studio module 3.13.1):**
  - Export with `GET /int/export/full?format=json`, or one page with
    `/int/export/page/<n>?format=json`.
  - Import a page in Import / Export: set the file input (Playwright
    `setInputFiles`), choose the destination page, keep "Link to <connection>",
    then "Replace page N".
  - Press a button with `POST /api/location/<page>/<row>/<col>/press`.
  - Page 13's `cg_obs` connection points at the facade `10.77.8.201:4456`, with
    the SNV scene names (`sp-fast`…).
- **RemoteOS shell output must be ASCII.** A Python script printing UTF-8 under
  `PYTHONIOENCODING=utf-8` came back as "(no output)". Use
  `sys.stdout.reconfigure(encoding="ascii", errors="backslashreplace")`.
