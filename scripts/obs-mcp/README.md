# obs-mcp gateway watchdog (win-resolume)

Version-controlled copy of the OBS-MCP gateway watchdog that runs on the
**win-resolume** live event PC (10.77.9.201). This bridges the local npm
package `obs-mcp` (an MCP server that drives OBS WebSocket 4455) onto
Streamable-HTTP port **8091** via `supergateway`, so the repo's
`obs-resolume` MCP tools can reach OBS from dev boxes.

The box copy is the source of truth at runtime; this directory is the
version-controlled mirror. Keep them byte-identical.

## Deploy path & task

| | |
|---|---|
| Deployed script | `C:\Users\Resolume\.obs-mcp\start-obs-mcp.ps1` |
| Batch shim | `C:\Users\Resolume\.obs-mcp\start-obs-mcp.bat` (calls the `.ps1`) |
| Log | `C:\Users\Resolume\.obs-mcp\log.txt` |
| Back-off state | `C:\Users\Resolume\.obs-mcp\state.json` (created at runtime; not in git) |
| Scheduled task | `ObsMCP` — trigger: at logon + repeat every **5 min** (PT5M), `MultipleInstancesPolicy=IgnoreNew` |

Deploy/read **only** via the `win-resolume` MCP `FileWrite`/`FileRead` tools —
never ssh/scp to this box.

## What the watchdog does each 5-minute cycle

1. **Back-off gate** — if the previous restarts kept failing, wait (5→10→20→40→60 min cap) instead of hammering.
2. **OBS dependency gate** — if OBS WebSocket 4455 is not listening, do nothing (obs-mcp only connects at startup); the scheduler retries next cycle.
3. **Leak guard** — if more than `NodeCap` (10) obs-mcp/supergateway `node` processes exist, tear the gateway down fully and start one fresh.
4. **Liveness** — if supergateway is listening on 8091 and `/healthz` returns `ok`, the gateway is healthy → no-op.
5. **(Re)start + verify** — otherwise tree-kill the old gateway and start a fresh **stateful** supergateway, then verify `/healthz` **and** a real MCP `obs-get-scene-list` identify probe before declaring success; on failure, record a back-off.

### Why `--stateful` matters (issue #128)

Stateless supergateway spawns a **new, never-reaped obs-mcp child per `/mcp`
request** — the engine of the 36-process / 780 MB leak observed 2026-09-13,
and it makes a naive health probe a leak source (each fresh child answers
"Not connected or identified" before it finishes connecting). `--stateful`
issues MCP session ids, so a well-behaved client reuses **one** child and
`DELETE /mcp` works. `Stop-Gateway` also tree-kills (`taskkill /T /F`) so the
`cmd → npx → cmd → node` child chain no longer leaks on restart.

OBS WebSocket auth is **not required** on this box (`auth_required=false`), so
no password is read or logged; the watchdog passes only
`OBS_WEBSOCKET_URL=ws://127.0.0.1:4455`.

## How to verify (from a dev box, via the `win-resolume` MCP)

```powershell
# task enabled?
Get-ScheduledTask -TaskName ObsMCP | Select State           # -> Ready (not Disabled)

# supergateway liveness (leak-free)
Invoke-WebRequest http://127.0.0.1:8091/healthz -UseBasicParsing | % Content   # -> ok

# obs identify end-to-end (proper MCP session)
#   initialize -> capture Mcp-Session-Id -> notifications/initialized ->
#   tools/call obs-get-scene-list  ==> returns the scene list, not
#   "Not connected or identified with OBS WebSocket server"

# leak check: node process count stays flat across watchdog restarts
Get-CimInstance Win32_Process -Filter "Name='node.exe'" |
    Where-Object CommandLine -match 'supergateway|obs-mcp' | Measure-Object | % Count
```

From the repo side, the `obs-resolume` MCP tool `obs-get-version` (or
`obs-get-scene-list`) should respond with live data.

### Re-enable the task after a fix

```powershell
Enable-ScheduledTask -TaskName ObsMCP
Start-ScheduledTask  -TaskName ObsMCP
```

## Safety rules on this box

- Only `obs-mcp` / `supergateway` `node`/`cmd` processes are ever killed here — **never** OBS, SongPlayer, the GitHub Actions runner, or any unrelated `node`.
- All launched work runs at `BELOW_NORMAL` priority (win-resolume is the live event PC).
- No secret values are read or logged.

## Not fixed here (upstream)

`obs-mcp` (npm, run via `npx -y`, no fork in any repo) connects to OBS once at
startup with no reconnect, and `supergateway` leaks the stdio child *process*
even after a session ends (verified: `--sessionTimeout` cleans session state
but not the process). The watchdog works around both; a real fix belongs
upstream (`obs-mcp` reconnect; `supergateway` reap-on-session-end).
