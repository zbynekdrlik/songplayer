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
5. After Arena restart, SongPlayer clip mapping refreshes automatically every 10s —
   confirm `/api/v1/product` answers and the `no Resolume subtitle clips found`
   warnings stop before re-running E2E.

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

## win-resolume is always free when user prompts

When the user gives a new prompt, win-resolume is ALWAYS free. Never defer
interactive work citing "wall idle window" or "event might be in progress".
If the user wanted to stop, they would stop Claude. CI uses the machine without
an idle check; Claude during active prompts can too.

## Genlock soak workflow (`.github/workflows/genlock-soak.yml`, #149)

Receiver-side ground-truth soak for the genlock chain (Genlock 4/6 lane 2).
`workflow_dispatch`-only for now (the daily `schedule` line is committed
commented-out; enable it when camera-box#1295 puts cg OBS on the genlock
build). It is NOT wired to push/PR — it deliberately waits `minutes`, which is
allowed only outside the PR pipeline (CLAUDE.md "CI architecture").

**Run it:**
```bash
gh workflow run genlock-soak.yml --ref dev -f minutes=5
# inputs: minutes (default 20), skew_bound_ms (default 20), hops (default cg-obs)
```
One job `soak` on the `[self-hosted, windows, resolume]` runner. It:
1. checks out camera-box at a PINNED commit (`fdd68e47c…`) for the verifier;
2. preflights SongPlayer `/api/v1/status` + OBS WebSocket 4455, notes the
   `genlock_pacing` setting + per-output `lock_state` into the job summary;
3. plays the first playlist with videos (no OBS scene switch — SongPlayer just
   emits NDI on that SP-* stream, which cg OBS ingests; the live wall is
   untouched) and, during the `minutes` window, samples `/api/v1/ndi/health`
   once per minute into `sp-health.csv`;
4. dumps the newest RESOLUME-SNV OBS log tail (last 4000 lines, byte-safe) to
   `cg-obs.log`;
5. runs `camera-box/scripts/cg-chain-verify.sh --hops cg-obs` against that log
   with `CG_CHAIN_CG_OBS_LOG=…`. **The job PASS/FAIL IS the verifier's exit
   code (exit 3 = FAIL), never SongPlayer's own counters.**

**Artifacts** (`genlock-soak-<run_id>`, uploaded `if: always()`):
- `sp-health.csv` — send-side evidence only (NOT the gate): per output per
  minute `seq, late_frames, lag_slots, repeats, resyncs, audio.residual_ppm,
  lock_state, lock_reason`.
- `cg-chain.csv` — the verifier's per-hop/per-source verdict rows.
- `cg-obs.log` — the receiver log tail the verdict was computed from.
- `cg-verdict.txt` — the verifier's printed per-hop table + OVERALL PASS/FAIL.

**Expected result TODAY = FAIL (count-gate BEFORE picture).** Until
camera-box#1295 lands, cg OBS is not on the genlock build, so its log carries
NO `genlock-fifo audit 'sp-*_video'` lines. The verifier reports the `cg-obs`
hop as `NO SOURCES` / `UNREADABLE` and exits 3 → the job is RED. Equivalently:
`locked=0` on every `sp-*` input is the honest count-gate signal that there is
no genlock picture yet — that is the correct, expected state, not a regression.
The workflow flips to a real PASS/FAIL verdict only once the receiver is on the
genlock build (add the `schedule` line then and add the `strih`/`stream` hops
once the runner has the ssh/bundle-state reader, camera-box#1294 Q10).

**Bumping the pinned camera-box ref:** replace the full 40-char SHA in the
`Checkout camera-box` step with a newer camera-box commit that still ships
`scripts/cg-chain-verify.sh` + `scripts/lib/cg-chain-verify.sh` with the
`CG_CHAIN_<HOP>_LOG` reader seam (a full SHA is required for checkout's
fetch-by-commit).
