---
paths:
  - "scripts/obs-mcp/**"
---

# obs-mcp gateway watchdog (win-resolume) — hard-won gotchas (#128)

The `obs-resolume` MCP gateway = npm **`supergateway`** bridging npm **`obs-mcp`**
(an OBS-WebSocket MCP server) over Streamable-HTTP on port **8091**, run by the
**`ObsMCP`** scheduled task (logon + PT5M) via `C:\Users\Resolume\.obs-mcp\
start-obs-mcp.ps1`. This repo dir is the version-controlled mirror of that box
copy — keep them byte-identical. Deploy/read via the `win-resolume` MCP
`FileWrite`/`FileRead` only, never ssh/scp.

## supergateway MUST run with `--stateful`

Without `--stateful`, supergateway (streamable-http) is stateless: `initialize`
returns **no** `Mcp-Session-Id` and it spawns a **fresh, never-reaped `obs-mcp`
child per `/mcp` request** — that is the 36-proc / 780 MB leak in #128 (measured
+2 `node.exe` per request; `DELETE /mcp` → 405). With `--stateful` a client that
keeps one session == one child, `DELETE` works (200), and obs-mcp identifies
reliably. Trade-off: supergateway 3.4.3 `--stateful` has a crash bug
(`stdioToStatefulStreamableHttp.js`) on some ungraceful stdio/session edge — the
watchdog recovers within one 5-min cycle, but do not be surprised by an
occasional gateway death in the log.

## Health checks MUST be TcpClient, NEVER Invoke-WebRequest

On this box, **`Invoke-WebRequest` to localhost hangs for its full timeout in the
Task Scheduler / detached (`Win32_Process.Create`) session even when the endpoint
is up** — measured: a `/healthz` IWR probe stalled 120s while `/healthz` answered
`ok` from an interactive shell (no proxy is configured; `ProxyEnable=0`). The
same call works fine from the interactive MCP `Shell`. So the watchdog decides
health purely on a raw **`TcpClient.Connect`** ("port listening"), never an HTTP
call. `--stateful` + the OBS-4455 dependency gate make a listening gateway
identify for clients, so "8091 listening" is a sufficient health signal;
obs-identify is confirmed out-of-band (an interactive MCP call, or the real
`obs-resolume` clients). If you ever add an HTTP/identify probe to the watchdog,
it will silently hang the task — don't.

## A detached launched process MUST redirect stdout/stderr

`Start-Process node … -WindowStyle Hidden` with **no** `-RedirectStandardOutput`/
`-RedirectStandardError` blocks on its first large write to the dead inherited
console handle → supergateway startup stalled **>120s** (measured). Redirecting
both to files makes it bind in ~2-3s. Files truncate each launch, so they only
grow between restarts (and double as a debug log: `gateway-out.log` /
`gateway-err.log` next to the script).

## Stop-Gateway MUST tree-kill, not port-owner-only

Killing only the port-8091 owner (`Get-NetTCPConnection -LocalPort 8091 …
Stop-Process`) leaves the `cmd → npx → cmd → node obs-mcp/build/index.js` child
chain orphaned — that leaked every restart. Use `taskkill /T /F /PID <owner>`
plus a sweep of every `node`/`cmd` whose command line matches
`supergateway|obs-mcp`. That match is safe ONLY because it is applied to
`node.exe`/`cmd.exe` (never OBS `obs64.exe`, `SongPlayer.exe`, or the
`Runner.*` GitHub Actions processes). Verified: 2 orphan children → tree-kill →
node count back to 1, flat across restarts.

## Deploy a script to the box byte-exactly (avoids newline/transcription drift)

`FileWrite` silently truncates content over ~20,000 chars and can normalize
newlines. To deploy a ~11 KB script byte-identically: `base64 -w0` it locally,
`FileWrite` the base64 to a box temp file, then on the box
`[IO.File]::WriteAllBytes($dst, [Convert]::FromBase64String((Get-Content $b64 -Raw).Trim()))`,
and confirm `(Get-FileHash $dst -SHA256).Hash` == the local `sha256sum`. Parse-
check with `[System.Management.Automation.Language.Parser]::ParseFile(...)`.

## Verify via the TASK, not `Win32_Process.Create`

`Invoke-CimMethod Win32_Process Create` runs in a different session than Task
Scheduler and reproduces the IWR-hang differently — it is NOT a faithful proxy
for the task's context. To verify the watchdog, drive the real task
(`Enable-ScheduledTask` / `Start-ScheduledTask ObsMCP`) and read `log.txt` +
`state.json`. `MultipleInstances=IgnoreNew`, so manual triggers can overlap the
PT5M repetition — wait for `State=Ready` between checks.
