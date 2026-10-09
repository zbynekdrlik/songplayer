# SongPlayer GitHub Actions Runner - One-line installer
# Usage: irm https://raw.githubusercontent.com/zbynekdrlik/songplayer/dev/scripts/setup-runner.ps1 | iex

$ErrorActionPreference = "Stop"

Write-Host ""
Write-Host "  SongPlayer GitHub Actions Runner Setup" -ForegroundColor Cyan
Write-Host "  Self-hosted runner for deploy + E2E tests" -ForegroundColor Gray
Write-Host ""

# --- Check admin ---
$isAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "  [!] Not running as admin — scheduled task creation may fail" -ForegroundColor Yellow
}

# --- Detect desktop user ---
$desktopUser = $null
try {
    $sessions = query user 2>&1
    foreach ($line in $sessions) {
        if ($line -match "console" -and $line -match "Active") {
            $desktopUser = ($line.Trim() -split "\s+")[0].TrimStart(">")
            break
        }
    }
} catch {}
if (-not $desktopUser) { $desktopUser = $env:USERNAME }
Write-Host "  Target user: $desktopUser" -ForegroundColor Gray

# --- Config ---
$RunnerDir = "C:\actions-runner"
$RepoUrl = "https://github.com/zbynekdrlik/songplayer"
$RunnerName = $env:COMPUTERNAME.ToLower()
# #229: the PP site's runner takes its own label (RUNNER_LABELS=self-hosted,windows,resolume-pp)
# so SNV's jobs ([self-hosted, windows, resolume]) never land on it.
$Labels = if ($env:RUNNER_LABELS) { $env:RUNNER_LABELS } else { "self-hosted,windows,resolume" }
$labelList = @($Labels -split "," | ForEach-Object { $_.Trim() })
if ($env:COMPUTERNAME -ieq "RESOLUME-PP" -and ($labelList -contains "resolume")) {
    throw "RESOLUME-PP must not carry the label 'resolume' (SNV's deploy and E2E jobs would run at PP). Set RUNNER_LABELS=self-hosted,windows,resolume-pp and run this again."
}

# --- Check if already installed ---
if (Test-Path "$RunnerDir\.runner") {
    Write-Host "  Runner already configured at $RunnerDir" -ForegroundColor Yellow
    Write-Host "  To reconfigure, delete $RunnerDir and re-run this script" -ForegroundColor Yellow
    return
}

# --- Download runner ---
Write-Host "  [1/5] Downloading GitHub Actions runner..." -ForegroundColor White
# GitHub refuses runners it has retired ("out of date and can no longer register"),
# so take the current release rather than a pinned version (#229).
$latest = Invoke-RestMethod -Uri "https://api.github.com/repos/actions/runner/releases/latest" -Headers @{ "User-Agent" = "songplayer-setup-runner" }
$runnerVersion = $latest.tag_name.TrimStart("v")
if ($runnerVersion -notmatch '^\d+\.\d+\.\d+$') { throw "Could not read the latest runner version (got '$($latest.tag_name)')" }
$downloadUrl = "https://github.com/actions/runner/releases/download/v$runnerVersion/actions-runner-win-x64-$runnerVersion.zip"
New-Item -ItemType Directory -Path $RunnerDir -Force | Out-Null
$zipPath = "$RunnerDir\runner.zip"
Invoke-WebRequest -Uri $downloadUrl -OutFile $zipPath
Expand-Archive -Path $zipPath -DestinationPath $RunnerDir -Force
Remove-Item $zipPath -Force
Write-Host "        Runner v$runnerVersion extracted to $RunnerDir" -ForegroundColor Green

# --- Get registration token ---
Write-Host "  [2/5] Registration token..." -ForegroundColor White
$token = $env:RUNNER_TOKEN
if (-not $token) {
    Write-Host "        No RUNNER_TOKEN env var found." -ForegroundColor Yellow
    Write-Host "        Generate one at: $RepoUrl/settings/actions/runners/new" -ForegroundColor Yellow
    $token = Read-Host "        Enter registration token"
}

# --- Configure ---
Write-Host "  [3/5] Configuring runner..." -ForegroundColor White
Push-Location $RunnerDir
.\config.cmd --url $RepoUrl --token $token --name $RunnerName --labels $Labels --runnergroup Default --work _work --unattended --replace
$configExit = $LASTEXITCODE
Pop-Location
if ($configExit -ne 0) { throw "Runner registration failed (config.cmd exit $configExit); see $RunnerDir\_diag. Nothing was registered." }
Write-Host "        Runner configured: $RunnerName [$Labels]" -ForegroundColor Green

# --- Scheduled task ---
Write-Host "  [4/5] Setting up auto-start..." -ForegroundColor White

@"
@echo off
title GitHub Actions Runner (songplayer)
cd /d $RunnerDir
.\run.cmd
"@ | Set-Content "$RunnerDir\start-runner.bat"

# The task below runs this every 5 minutes (and at logon): start the runner
# only when this runner directory has none running. Every run used to start
# another run.cmd, and two listeners of one directory collide on
# _diag\pages ("The file ... already exists": a job fails in seconds).
@"
Set wmi = GetObject("winmgmts:\\.\root\cimv2")
For Each p In wmi.ExecQuery("SELECT Name, ExecutablePath, CommandLine FROM Win32_Process WHERE Name='Runner.Listener.exe' OR Name='cmd.exe'")
  If LCase(p.Name) = "runner.listener.exe" Then
    If LCase(p.ExecutablePath & "") = LCase("$RunnerDir\bin\Runner.Listener.exe") Then WScript.Quit 0
  ElseIf InStr(1, p.CommandLine & "", "$RunnerDir\start-runner.bat", vbTextCompare) > 0 Then
    WScript.Quit 0
  End If
Next
Set WshShell = CreateObject("WScript.Shell")
WshShell.Run """$RunnerDir\start-runner.bat""", 0, False
"@ | Set-Content "$RunnerDir\start-runner.vbs"

try {
    $taskAction = New-ScheduledTaskAction -Execute "wscript.exe" -Argument "`"$RunnerDir\start-runner.vbs`""
    $triggerLogon = New-ScheduledTaskTrigger -AtLogon
    $triggerRepeat = New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Minutes 5)
    $taskPrincipal = New-ScheduledTaskPrincipal -UserId $desktopUser -RunLevel Highest -LogonType Interactive
    $taskSettings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew
    Register-ScheduledTask -TaskName "GitHubActionsRunner" -Action $taskAction -Trigger @($triggerLogon, $triggerRepeat) -Principal $taskPrincipal -Settings $taskSettings -Force | Out-Null
    Write-Host "        Task 'GitHubActionsRunner' registered" -ForegroundColor Green
} catch {
    Write-Host "        [!] Could not create scheduled task (need admin)" -ForegroundColor Yellow
}

# --- Start runner ---
Write-Host "  [5/5] Starting runner..." -ForegroundColor White
Start-Process -FilePath "wscript.exe" -ArgumentList "`"$RunnerDir\start-runner.vbs`""
Start-Sleep -Seconds 5

$running = Get-Process -Name "Runner.Listener" -ErrorAction SilentlyContinue
if ($running) {
    Write-Host "        Runner is running!" -ForegroundColor Green
} else {
    Write-Host "        [!] Runner may still be starting..." -ForegroundColor Yellow
}

# --- Summary ---
$localIP = (Get-NetIPAddress -AddressFamily IPv4 |
    Where-Object { $_.IPAddress -notlike "127.*" -and $_.IPAddress -notlike "169.254.*" -and $_.PrefixOrigin -ne "WellKnown" } |
    Sort-Object -Property InterfaceIndex |
    Select-Object -First 1).IPAddress

Write-Host ""
Write-Host "  ============================================" -ForegroundColor Cyan
Write-Host "  SETUP COMPLETE" -ForegroundColor Cyan
Write-Host "  ============================================" -ForegroundColor Cyan
Write-Host ""
Write-Host "  Computer:  $RunnerName ($localIP)" -ForegroundColor White
Write-Host "  User:      $desktopUser" -ForegroundColor White
Write-Host "  Labels:    $Labels" -ForegroundColor White
Write-Host "  Directory: $RunnerDir" -ForegroundColor White
Write-Host ""
Write-Host "  Verify at: $RepoUrl/settings/actions/runners" -ForegroundColor Gray
Write-Host ""
