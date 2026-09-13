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
#     --stateful gives session ids so one client == one child, and DELETE works.
#   * the old Stop-Gateway killed only the port-8091 owner; its cmd/npx/node
#     child chain leaked on every restart. Stop-Gateway now tree-kills.
#   * obs-mcp (npm, npx -y, not forkable here) connects once and never retries;
#     the 5-min task is the watchdog. Restarts now verify /healthz + a real
#     obs-identify probe, and back off exponentially after repeated failures
#     instead of restarting (and leaking) every 5 minutes.
#
# HARD RULES (win-resolume is the LIVE event PC): only obs-mcp / supergateway
# node processes are ever killed here — never OBS, SongPlayer, the runner, or
# any unrelated node. All launched work runs at BELOW_NORMAL priority. No
# secret values are ever read or logged (OBS WebSocket auth is not required on
# this box).

$ErrorActionPreference = "Stop"

# --------------------------------------------------------------------------- #
# Config
# --------------------------------------------------------------------------- #
$ScriptRoot        = $PSScriptRoot
$LogFile           = Join-Path $ScriptRoot "log.txt"
$StateFile         = Join-Path $ScriptRoot "state.json"

$ObsWsPort         = 4455
$GwPort            = 8091
$HealthPath        = "/healthz"
$ObsWsUrl          = "ws://127.0.0.1:$ObsWsPort"   # explicit IPv4 — no localhost dual-stack ambiguity
$SessionTimeoutMs  = 300000                        # 5 min idle session cleanup (state, best-effort)

$NodeCap           = 10                            # obs-mcp/supergateway node procs before a forced teardown
$IdentifyTimeoutSec = 45                           # how long to wait for obs-identify after a (re)start
$HealthWaitSec     = 120                           # how long to wait for /healthz after a (re)start.
                                                   # Cold start is normally ~2-3s but can spike to >60s
                                                   # while node/supergateway modules are cache-cold +
                                                   # AV-scanned under load; a late bind is still handled
                                                   # (see the final-verdict recheck below), so this only
                                                   # bounds how long a genuinely dead launch waits.

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

function Test-Port([int]$p) {
    $c = New-Object System.Net.Sockets.TcpClient
    try { $c.Connect("127.0.0.1", $p); return $true } catch { return $false } finally { $c.Close() }
}

# Count node processes that belong to the obs-mcp gateway (never OBS/SongPlayer/runner).
function Get-GatewayNodeCount {
    @(Get-CimInstance Win32_Process -Filter "Name='node.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -match $GwMatch }).Count
}

function Test-Healthz {
    try {
        $r = Invoke-WebRequest -Uri "http://127.0.0.1:$GwPort$HealthPath" -TimeoutSec 8 -UseBasicParsing
        return ($r.Content.Trim() -eq "ok")
    } catch { return $false }
}

# One MCP HTTP round trip. Returns the WebResponse object (or throws).
function Invoke-Mcp($body, $session, $method = "Post") {
    $headers = @{ "Accept" = "application/json, text/event-stream" }
    if ($session) { $headers["Mcp-Session-Id"] = $session }
    $p = @{
        Uri             = "http://127.0.0.1:$GwPort/mcp"
        Method          = $method
        Headers         = $headers
        TimeoutSec      = 15
        UseBasicParsing = $true
    }
    if ($null -ne $body) { $p["Body"] = $body; $p["ContentType"] = "application/json" }
    return Invoke-WebRequest @p
}

# Drive a real MCP session (initialize -> initialized -> tools/call) and confirm
# obs-mcp is connected AND identified with OBS. Reaps the child THIS probe spawned
# so the health check never leaks (safe: only called right after a teardown, when
# no other session exists).
function Test-ObsIdentify {
    $before = @(Get-CimInstance Win32_Process -Filter "Name='node.exe' OR Name='cmd.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -match 'obs-mcp' } | Select-Object -ExpandProperty ProcessId)
    $sid = $null
    $ok  = $false
    try {
        $init = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"obs-mcp-watchdog","version":"1.0"}}}'
        $r = Invoke-Mcp $init $null
        $sid = $r.Headers["Mcp-Session-Id"]
        if ($sid -is [array]) { $sid = $sid[0] }
        if ($sid) { Invoke-Mcp '{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}' $sid | Out-Null }
        $call = '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"obs-get-scene-list","arguments":{}}}'
        $deadline = (Get-Date).AddSeconds($IdentifyTimeoutSec)
        while ((Get-Date) -lt $deadline) {
            try {
                $cr = Invoke-Mcp $call $sid
                if (($cr.Content -match "scene") -and ($cr.Content -notmatch "Not connected or identified")) { $ok = $true; break }
            } catch {}
            Start-Sleep -Seconds 3
        }
    } catch {}
    if ($sid) { try { Invoke-Mcp $null $sid "Delete" | Out-Null } catch {} }
    Start-Sleep -Milliseconds 500
    # Reap only the obs-mcp children THIS probe spawned (new PIDs), never a pre-existing one.
    Get-CimInstance Win32_Process -Filter "Name='node.exe' OR Name='cmd.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -match 'obs-mcp' -and ($before -notcontains $_.ProcessId) } |
        ForEach-Object { try { & taskkill /T /F /PID $($_.ProcessId) 2>$null | Out-Null } catch {} }
    return $ok
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
    $proc = Start-Process -FilePath $file -ArgumentList $argline -WindowStyle Hidden -PassThru
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

# 4. Liveness (non-spawning): supergateway listening + /healthz ok.
if (-not $needRestart) {
    $gwUp = Test-Port $GwPort
    $healthy = $gwUp -and (Test-Healthz)
    if ($healthy) {
        Write-State 0 0            # healthy -> reset any back-off
        exit 0
    }
    Log "Gateway not healthy (8091 listening=$gwUp, /healthz ok=$(Test-Healthz)) - restarting"
    Stop-Gateway
    $needRestart = $true
}

# 5. (Re)start and verify: /healthz up AND a real obs-identify probe succeeds.
Log "Starting obs-mcp via supergateway (stateful streamable-http, port $GwPort)"
Start-Gateway | Out-Null

$healthOk = $false
$deadline = (Get-Date).AddSeconds($HealthWaitSec)
while ((Get-Date) -lt $deadline) { if (Test-Healthz) { $healthOk = $true; break }; Start-Sleep -Seconds 2 }

$identOk = $false
if ($healthOk) { $identOk = Test-ObsIdentify }

if ($healthOk -and $identOk) {
    # Full success: supergateway serving /healthz AND obs-mcp identified with OBS.
    Log "Gateway up: /healthz ok, OBS identified"
    Write-State 0 0
} elseif ($healthOk -and -not $identOk) {
    # supergateway is up but obs-mcp did not identify within the retry window -> a real
    # obs-mcp/OBS problem a restart storm would not fix. Back off.
    $fc = [int]$state.failCount + 1
    $backoffMin = [int][Math]::Min($CapBackoffMin, $BaseBackoffMin * [Math]::Pow(2, $fc - 1))
    Write-State $fc ((Now-Epoch) + ($backoffMin * 60))
    Log "Gateway up (/healthz ok) but obs-mcp did not identify within ${IdentifyTimeoutSec}s - backing off ~$backoffMin min (failCount=$fc)"
} elseif (Test-Healthz) {
    # /healthz did not come up within the wait window but is ok NOW: a slow cold start that
    # bound late. The gateway is up; the next cycle's liveness check confirms it. Not a failure.
    Log "Gateway up (/healthz ok after a slow cold start) - will confirm on the next cycle"
    Write-State 0 0
} else {
    # supergateway never came up -> genuine launch failure. Back off.
    $fc = [int]$state.failCount + 1
    $backoffMin = [int][Math]::Min($CapBackoffMin, $BaseBackoffMin * [Math]::Pow(2, $fc - 1))
    Write-State $fc ((Now-Epoch) + ($backoffMin * 60))
    Log "Gateway failed to start (/healthz not ok) - backing off ~$backoffMin min (failCount=$fc)"
}
exit 0
