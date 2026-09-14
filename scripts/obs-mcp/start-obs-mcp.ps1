# start-obs-mcp.ps1 — OBS-MCP gateway watchdog for win-resolume (songplayer #128)
#
# Deployed to  C:\Users\Resolume\.obs-mcp\start-obs-mcp.ps1
# Invoked by   scheduled task "ObsMCP" (at logon + PT5M repetition) via
#              start-obs-mcp.bat.
#
# Publishes the local npm package `obs-mcp` (an MCP server that drives OBS
# WebSocket 4455) over Streamable-HTTP on port 8091 using `supergateway`.
#
# Why this rewrite (see issue #128):
#   * supergateway was run WITHOUT --stateful, so it was stateless and spawned
#     a fresh, never-reaped obs-mcp child PER /mcp request -> 36 procs / 780 MB.
#     --stateful gives session ids so one client == one child, and it is what
#     makes obs-mcp identify reliably (a fresh gateway that is listening on 8091
#     with OBS 4455 up identifies on the first client call — verified live).
#   * the old Stop-Gateway killed only the port-8091 owner; its cmd/npx/node
#     child chain leaked on every restart. Stop-Gateway now tree-kills.
#   * obs-mcp (npm, npx -y, not forkable here) connects once and never retries;
#     the 5-min task is the watchdog. It now backs off exponentially after
#     repeated failures instead of restarting (and leaking) every 5 minutes.
#
# Health signals are TcpClient-only ON PURPOSE. Invoke-WebRequest to localhost
# is unreliable in the Task Scheduler / detached session on this box (it hangs
# for the full timeout even when the endpoint is up — measured live: the health
# HTTP probe stalled 120s while /healthz answered "ok" from an interactive
# shell). TcpClient (a raw socket connect) works in every context, so the
# watchdog decides on "port listening", never on an HTTP call. obs-identify is
# verified out-of-band (an interactive MCP call, or the real clients).
#
# HARD RULES (win-resolume is the LIVE event PC): only obs-mcp / supergateway
# node processes are ever killed here — never OBS, SongPlayer, the runner, or
# any unrelated node. All launched work runs at BELOW_NORMAL priority. No
# secret values are ever read or logged (OBS WebSocket auth is not required on
# this box).

$ErrorActionPreference = "Stop"
$ProgressPreference    = "SilentlyContinue"

# --------------------------------------------------------------------------- #
# Config
# --------------------------------------------------------------------------- #
$ScriptRoot        = $PSScriptRoot
$LogFile           = Join-Path $ScriptRoot "log.txt"
$StateFile         = Join-Path $ScriptRoot "state.json"

$ObsWsPort         = 4455
$GwPort            = 8091
$HealthPath        = "/healthz"                    # supergateway's own liveness endpoint (for
                                                   # interactive / client checks — not called here)
$ObsWsUrl          = "ws://127.0.0.1:$ObsWsPort"   # explicit IPv4 — no localhost dual-stack ambiguity
$SessionTimeoutMs  = 300000                        # 5 min idle session cleanup (state, best-effort)

$NodeCap           = 10                            # obs-mcp/supergateway node procs before a forced teardown
$UpWaitSec         = 45                            # how long to wait for 8091 to listen after a (re)start

$BaseBackoffMin    = 5                             # 5 -> 10 -> 20 -> 40 -> 60 (cap)
$CapBackoffMin     = 60

$NpmModules = Join-Path $env:APPDATA "npm\node_modules"
if (-not (Test-Path $NpmModules)) { $NpmModules = "C:\Users\Resolume\AppData\Roaming\npm\node_modules" }
$SgEntry = Join-Path $NpmModules "supergateway\dist\index.js"

$NodeExe = (Get-Command node -ErrorAction SilentlyContinue).Source
if (-not $NodeExe) { $NodeExe = "C:\Program Files\nodejs\node.exe" }

# Only ever match / kill processes belonging to THIS gateway.
$GwMatch = 'supergateway|obs-mcp'

# --------------------------------------------------------------------------- #
# Helpers
# --------------------------------------------------------------------------- #
function Log($m) {
    $line = "$([DateTime]::Now.ToString('yyyy-MM-dd HH:mm:ss')) $m"
    try { $line | Out-File -FilePath $LogFile -Append -Encoding utf8 } catch {}
}

function Now-Epoch { [DateTimeOffset]::UtcNow.ToUnixTimeSeconds() }

# Raw socket connect — reliable in every session (unlike Invoke-WebRequest here).
function Test-Port([int]$p) {
    $c = New-Object System.Net.Sockets.TcpClient
    try { $c.Connect("127.0.0.1", $p); return $true } catch { return $false } finally { $c.Close() }
}

# Count node processes that belong to the obs-mcp gateway (never OBS/SongPlayer/runner).
function Get-GatewayNodeCount {
    @(Get-CimInstance Win32_Process -Filter "Name='node.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -match $GwMatch }).Count
}

# Tree-kill the supergateway (port-8091 owner) INCLUDING its cmd/npx/node child
# chain, then sweep any orphaned supergateway/obs-mcp node+cmd processes.
function Stop-Gateway {
    $owners = @(Get-NetTCPConnection -LocalPort $GwPort -State Listen -ErrorAction SilentlyContinue |
        Select-Object -ExpandProperty OwningProcess -Unique)
    foreach ($procId in $owners) { try { & taskkill /T /F /PID $procId 2>$null | Out-Null } catch {} }
    Get-CimInstance Win32_Process -Filter "Name='node.exe' OR Name='cmd.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -match $GwMatch } |
        ForEach-Object { try { & taskkill /T /F /PID $($_.ProcessId) 2>$null | Out-Null } catch {} }
    Start-Sleep -Seconds 2
}

# Launch a fresh stateful gateway, detached, at BELOW_NORMAL priority.
#
# stdout/stderr MUST be redirected: a detached process launched with no console
# and no redirect blocks on its first large write to the dead inherited handle,
# which stalled supergateway startup for >120s (measured). Redirecting to files
# makes it bind in ~2-3s and gives a debuggable log. The files are truncated on
# each launch (Start-Process overwrites), so they only grow between restarts.
function Start-Gateway {
    $env:OBS_WEBSOCKET_URL = $ObsWsUrl
    $common = "--stdio `"npx -y obs-mcp`" --port $GwPort --outputTransport streamableHttp --stateful --sessionTimeout $SessionTimeoutMs --healthEndpoint $HealthPath --cors"
    if (Test-Path $SgEntry) {
        $file = $NodeExe
        $argline = "`"$SgEntry`" $common"
    } else {
        # Fallback if the global install layout ever changes.
        $file = "cmd.exe"
        $argline = "/c npx -y supergateway $common"
    }
    $outLog = Join-Path $ScriptRoot "gateway-out.log"
    $errLog = Join-Path $ScriptRoot "gateway-err.log"
    $proc = Start-Process -FilePath $file -ArgumentList $argline -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput $outLog -RedirectStandardError $errLog
    try { $proc.PriorityClass = 'BelowNormal' } catch {}
    return $proc
}

function Read-State {
    if (Test-Path $StateFile) {
        try { return (Get-Content $StateFile -Raw | ConvertFrom-Json) } catch {}
    }
    return [pscustomobject]@{ failCount = 0; nextAttemptEpoch = 0 }
}

function Write-State([int]$failCount, [long]$nextAttemptEpoch) {
    try {
        [pscustomobject]@{ failCount = $failCount; nextAttemptEpoch = $nextAttemptEpoch } |
            ConvertTo-Json | Out-File -FilePath $StateFile -Encoding utf8
    } catch {}
}

function Record-Failure($state) {
    $fc = [int]$state.failCount + 1
    $backoffMin = [int][Math]::Min($CapBackoffMin, $BaseBackoffMin * [Math]::Pow(2, $fc - 1))
    Write-State $fc ((Now-Epoch) + ($backoffMin * 60))
    return $backoffMin
}

# --------------------------------------------------------------------------- #
# Main
# --------------------------------------------------------------------------- #
$state = Read-State
$nowEpoch = Now-Epoch

# 1. Back-off gate — after repeated failed restarts, do not hammer.
if ([long]$state.nextAttemptEpoch -gt $nowEpoch) {
    $waitMin = [Math]::Ceiling(([long]$state.nextAttemptEpoch - $nowEpoch) / 60)
    Log "Backing off after $($state.failCount) consecutive failures - next attempt in ~$waitMin min"
    exit 0
}

# 2. OBS WebSocket (4455) is a hard dependency — obs-mcp only connects at startup.
if (-not (Test-Port $ObsWsPort)) {
    Log "OBS WebSocket $ObsWsPort not listening - leaving gateway alone, scheduler retries in 5 min"
    exit 0
}

# 3. Leak guard — bound the process count no matter how sessions accumulate.
$count = Get-GatewayNodeCount
$needRestart = $false
if ($count -gt $NodeCap) {
    Log "Leak guard: $count obs-mcp/supergateway node procs (> cap $NodeCap) - full teardown"
    Stop-Gateway
    $needRestart = $true
}

# 4. Liveness (TcpClient, reliable in any session): supergateway listening on 8091.
#    --stateful + the 4455 gate above make a listening gateway identify for clients,
#    so "listening" is the health signal; no HTTP call is made here.
if (-not $needRestart) {
    if (Test-Port $GwPort) {
        Write-State 0 0            # healthy -> reset any back-off
        exit 0
    }
    Log "Gateway not listening on $GwPort - restarting"
    Stop-Gateway
    $needRestart = $true
}

# 5. (Re)start and verify the supergateway comes up listening on 8091.
Log "Starting obs-mcp via supergateway (stateful streamable-http, port $GwPort)"
Start-Gateway | Out-Null

$upOk = $false
$deadline = (Get-Date).AddSeconds($UpWaitSec)
while ((Get-Date) -lt $deadline) { if (Test-Port $GwPort) { $upOk = $true; break }; Start-Sleep -Seconds 2 }

if ($upOk) {
    Log "Gateway up: supergateway listening on $GwPort (stateful)"
    Write-State 0 0
} else {
    $backoffMin = Record-Failure $state
    Log "Gateway failed to start ($GwPort not listening after ${UpWaitSec}s) - backing off ~$backoffMin min (failCount=$([int]$state.failCount + 1))"
}
exit 0
