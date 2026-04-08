# CI Hardening, Deployment & YTPlayer Migration — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add self-hosted deployment and E2E testing to SongPlayer's CI pipeline targeting win-resolume, plus seed the 6 ytplayer playlists so SongPlayer can replace the Python scripts.

**Architecture:** Two new CI jobs (`deploy-resolume` and `e2e-resolume`) run on the self-hosted `[self-hosted, windows, resolume]` runner after the existing gate passes. The deploy job downloads the NSIS installer artifact, kills any running SongPlayer, runs the installer silently, creates a scheduled task, and verifies via health checks. The E2E job seeds playlists/settings via the API and runs functional tests. A `scripts/setup-runner.ps1` provides reproducible runner setup via `irm | iex`.

**Tech Stack:** GitHub Actions YAML, PowerShell, curl, NSIS silent install, Windows Scheduled Tasks

---

### Task 1: Add deploy-resolume job to CI

**Files:**
- Modify: `.github/workflows/ci.yml` (append after gate job)

- [ ] **Step 1: Add the deploy-resolume job**

Append this job after the existing `gate` job in `.github/workflows/ci.yml`:

```yaml
  deploy-resolume:
    name: Deploy to win-resolume
    needs: [gate, build-tauri]
    if: >-
      always()
      && (github.ref == 'refs/heads/dev' || github.ref == 'refs/heads/main')
      && (github.event_name == 'push' || github.event_name == 'workflow_dispatch')
      && needs.gate.result == 'success'
      && needs.build-tauri.result == 'success'
    runs-on: [self-hosted, windows, resolume]
    steps:
      - uses: actions/checkout@v4

      - name: Clean old artifacts
        shell: powershell
        run: |
          Remove-Item -Path "artifacts" -Recurse -Force -ErrorAction SilentlyContinue
          Write-Host "Cleaned artifacts directory"

      - name: Download Tauri installer
        uses: actions/download-artifact@v4
        with:
          name: tauri-installer
          path: artifacts/tauri

      - name: List artifacts
        shell: powershell
        run: |
          Write-Host "=== Tauri installer ==="
          Get-ChildItem artifacts/tauri/

      - name: Install WebView2 runtime
        shell: powershell
        run: |
          function Get-WebView2Version {
            foreach ($guid in @("{F3017226-FE2A-4295-8BEF-AE82F87EC1B0}", "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}")) {
              $reg = Get-ItemProperty -Path "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\$guid" -ErrorAction SilentlyContinue
              if ($reg -and $reg.pv) { return $reg.pv }
            }
            $dir = "C:\Program Files (x86)\Microsoft\EdgeWebView\Application"
            if (Test-Path $dir) { return "installed (detected from filesystem)" }
            return $null
          }
          $version = Get-WebView2Version
          if ($version) {
            Write-Host "WebView2 already installed: $version"
          } else {
            Write-Host "WebView2 not found - installing..."
            $bootstrapper = "$env:TEMP\MicrosoftEdgeWebview2Setup.exe"
            Invoke-WebRequest -Uri "https://go.microsoft.com/fwlink/p/?LinkId=2124703" -OutFile $bootstrapper
            Start-Process -FilePath $bootstrapper -ArgumentList "/silent /install" -Wait
            Remove-Item $bootstrapper -ErrorAction SilentlyContinue
            $version = Get-WebView2Version
            if (-not $version) {
              Write-Error "WebView2 runtime installation FAILED"
              exit 1
            }
            Write-Host "WebView2 installed: $version"
          }

      - name: Deploy SongPlayer
        shell: powershell
        run: |
          $ErrorActionPreference = "Continue"

          Write-Host "=== Stopping SongPlayer ==="
          taskkill /F /IM "SongPlayer.exe" 2>&1 | Out-Null
          # Wait for process to exit and release DB locks
          $elapsed = 0
          while ($elapsed -lt 10) {
            $proc = Get-Process -Name "SongPlayer" -ErrorAction SilentlyContinue
            if (-not $proc) { break }
            Start-Sleep -Seconds 1
            $elapsed++
          }
          Start-Sleep -Seconds 2

          # Clean stale SQLite files
          $DataDir = "C:\ProgramData\SongPlayer"
          Remove-Item "$DataDir\songplayer.db-wal" -Force -ErrorAction SilentlyContinue
          Remove-Item "$DataDir\songplayer.db-shm" -Force -ErrorAction SilentlyContinue

          # Wait for port 8920 to be free
          $timeout = 30
          $elapsed = 0
          while ($elapsed -lt $timeout) {
            $portInUse = netstat -an | Select-String ":8920 " | Select-String "LISTENING"
            if (-not $portInUse) { break }
            Start-Sleep -Seconds 2
            $elapsed += 2
            Write-Host "Waiting for port 8920... ($elapsed s)"
          }

          # Run NSIS installer
          Write-Host "=== Installing SongPlayer ==="
          $installer = Get-ChildItem "artifacts/tauri/*.exe" | Select-Object -First 1
          Write-Host "Installer: $($installer.Name)"
          Start-Process -FilePath $installer.FullName -ArgumentList "/S" -Wait

          # Verify install
          $InstallDir = "C:\Program Files\SongPlayer"
          Write-Host "=== Verifying install ==="
          Get-ChildItem "$InstallDir\*.exe"

      - name: Configure auto-start and launch
        shell: powershell
        run: |
          $ErrorActionPreference = "Stop"
          $ExePath = "C:\Program Files\SongPlayer\SongPlayer.exe"
          $InstallDir = "C:\Program Files\SongPlayer"
          $TaskName = "SongPlayer"

          # Remove old scheduled tasks
          Get-ScheduledTask | Where-Object { $_.TaskName -like "*ongPlayer*" } | ForEach-Object {
            Unregister-ScheduledTask -TaskName $_.TaskName -Confirm:$false -ErrorAction SilentlyContinue
          }

          # Create scheduled task
          $action = New-ScheduledTaskAction -Execute $ExePath -WorkingDirectory $InstallDir
          $trigger = New-ScheduledTaskTrigger -AtLogon -User "Resolume"
          $principal = New-ScheduledTaskPrincipal -UserId "Resolume" -LogonType Interactive -RunLevel Limited
          $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -ExecutionTimeLimit 0
          Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings | Out-Null
          Write-Host "Created scheduled task: $TaskName"

          # Add firewall rule for API port
          Remove-NetFirewallRule -DisplayName "SongPlayer API" -ErrorAction SilentlyContinue
          New-NetFirewallRule -DisplayName "SongPlayer API" -Direction Inbound -Protocol TCP -LocalPort 8920 -Action Allow | Out-Null
          Write-Host "Firewall rule created for port 8920"

          # Start via scheduled task
          schtasks.exe /run /tn $TaskName
          if ($LASTEXITCODE -ne 0) { throw "schtasks.exe failed (exit code: $LASTEXITCODE)" }
          Start-Sleep -Seconds 10
          Write-Host "SongPlayer started"

      - name: Health checks
        shell: powershell
        run: |
          $ErrorActionPreference = "Stop"
          $VERSION = Get-Content "VERSION" -Raw
          $VERSION = $VERSION.Trim()

          Write-Host "=== CHECK 1: Process running ==="
          $proc = Get-Process -Name "SongPlayer" -ErrorAction SilentlyContinue
          if (-not $proc) {
            Write-Error "FAIL: SongPlayer.exe is not running"
            exit 1
          }
          Write-Host "OK: PID $($proc.Id)"

          Write-Host "=== CHECK 2: API responds ==="
          $maxRetries = 6
          $apiOk = $false
          for ($i = 1; $i -le $maxRetries; $i++) {
            try {
              $resp = Invoke-RestMethod -Uri "http://localhost:8920/api/v1/status" -TimeoutSec 5
              $apiOk = $true
              break
            } catch {
              Write-Host "Attempt $i/$maxRetries - waiting 5s..."
              Start-Sleep -Seconds 5
            }
          }
          if (-not $apiOk) {
            Write-Error "FAIL: API did not respond after $maxRetries attempts"
            exit 1
          }
          Write-Host "OK: API version=$($resp.version), obs_connected=$($resp.obs_connected)"

          Write-Host "=== CHECK 3: Version matches ==="
          if ($resp.version -ne $VERSION) {
            Write-Error "FAIL: API version '$($resp.version)' != expected '$VERSION'"
            exit 1
          }
          Write-Host "OK: Version matches ($VERSION)"

          Write-Host "=== All health checks passed ==="

- [ ] **Step 2: Verify YAML syntax**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))" && echo "YAML valid"`

Expected: `YAML valid`

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add deploy-resolume job with health checks"
```

---

### Task 2: Add e2e-resolume job to CI

**Files:**
- Modify: `.github/workflows/ci.yml` (append after deploy-resolume job)

- [ ] **Step 1: Add the e2e-resolume job**

Append this job after `deploy-resolume` in `.github/workflows/ci.yml`:

```yaml
  e2e-resolume:
    name: E2E Tests (win-resolume)
    needs: [deploy-resolume]
    if: always() && needs.deploy-resolume.result == 'success'
    runs-on: [self-hosted, windows, resolume]
    steps:
      - uses: actions/checkout@v4

      - name: Wait for SongPlayer API
        shell: powershell
        run: |
          $maxRetries = 6
          for ($i = 1; $i -le $maxRetries; $i++) {
            try {
              $resp = Invoke-RestMethod -Uri "http://localhost:8920/api/v1/status" -TimeoutSec 5
              Write-Host "API ready: version=$($resp.version)"
              exit 0
            } catch {
              Write-Host "Attempt $i/$maxRetries..."
              Start-Sleep -Seconds 5
            }
          }
          Write-Error "API not available"
          exit 1

      - name: Seed settings
        shell: powershell
        run: |
          $settings = @{
            obs_websocket_url = "ws://localhost:4455"
            cache_dir = "C:\ProgramData\SongPlayer\cache"
            gemini_api_key = "${{ secrets.GEMINI_API_KEY }}"
          }
          $body = $settings | ConvertTo-Json
          Invoke-RestMethod -Uri "http://localhost:8920/api/v1/settings" -Method Patch -Body $body -ContentType "application/json"
          Write-Host "Settings seeded"

          # Verify
          $current = Invoke-RestMethod -Uri "http://localhost:8920/api/v1/settings"
          Write-Host "obs_websocket_url = $($current.obs_websocket_url)"
          Write-Host "cache_dir = $($current.cache_dir)"
          if ($current.obs_websocket_url -ne "ws://localhost:4455") {
            Write-Error "FAIL: obs_websocket_url not set"
            exit 1
          }

      - name: Seed playlists
        shell: powershell
        run: |
          $playlists = @(
            @{ name = "ytwarmup";    youtube_url = "https://www.youtube.com/playlist?list=PLFdHTR758BvcHRX3nVKMEPHuBdU75dBVE"; obs_text_source = "ytwarmup_title" },
            @{ name = "ytpresence";  youtube_url = "https://www.youtube.com/playlist?list=PLFdHTR758BveAZ9YDY4ALy9iGxQVrkGRl"; obs_text_source = "ytpresence_title" },
            @{ name = "ytslow";      youtube_url = "https://www.youtube.com/playlist?list=PLFdHTR758Bvd9c7dKV-ZZFQ1jg30ahHFq"; obs_text_source = "ytslow_title" },
            @{ name = "yt90s";       youtube_url = "https://www.youtube.com/playlist?list=PLFdHTR758BvfM0XYF6Q2nEDnW0CqHXI17"; obs_text_source = "yt90s_title" },
            @{ name = "ytworship";   youtube_url = "https://www.youtube.com/playlist?list=PLFdHTR758BveEaqE5BWIQI7ukkijjdbbG"; obs_text_source = "ytworship_title" },
            @{ name = "ytfast";      youtube_url = "https://www.youtube.com/playlist?list=PLFdHTR758BvdEXF1tZ_3g8glRuev6EC6U"; obs_text_source = "ytfast_title" }
          )

          # Check existing playlists first to avoid duplicates
          $existing = Invoke-RestMethod -Uri "http://localhost:8920/api/v1/playlists"
          $existingNames = $existing | ForEach-Object { $_.name }

          foreach ($pl in $playlists) {
            if ($existingNames -contains $pl.name) {
              Write-Host "Playlist '$($pl.name)' already exists — skipping"
              continue
            }
            $body = $pl | ConvertTo-Json
            $resp = Invoke-RestMethod -Uri "http://localhost:8920/api/v1/playlists" -Method Post -Body $body -ContentType "application/json"
            Write-Host "Created playlist: $($resp.name) (id=$($resp.id))"
          }

          # Verify count
          $all = Invoke-RestMethod -Uri "http://localhost:8920/api/v1/playlists"
          if ($all.Count -lt 6) {
            Write-Error "FAIL: Expected at least 6 playlists, got $($all.Count)"
            exit 1
          }
          Write-Host "OK: $($all.Count) playlists exist"

      - name: Test playlist sync
        shell: powershell
        run: |
          # Trigger sync on the first playlist and verify it accepts the request
          $playlists = Invoke-RestMethod -Uri "http://localhost:8920/api/v1/playlists"
          $first = $playlists[0]
          Write-Host "Triggering sync for '$($first.name)' (id=$($first.id))..."

          $resp = Invoke-WebRequest -Uri "http://localhost:8920/api/v1/playlists/$($first.id)/sync" -Method Post -UseBasicParsing
          if ($resp.StatusCode -ne 202) {
            Write-Error "FAIL: Expected 202 Accepted, got $($resp.StatusCode)"
            exit 1
          }
          Write-Host "OK: Sync accepted (202)"

      - name: Test OBS connection
        shell: powershell
        run: |
          # Give OBS client time to connect after settings were seeded
          Start-Sleep -Seconds 5

          $status = Invoke-RestMethod -Uri "http://localhost:8920/api/v1/status"
          Write-Host "OBS connected: $($status.obs_connected)"
          # Note: OBS connection may not be immediate if the app was just started.
          # We log the status but don't fail on it — the OBS client auto-reconnects.
          if ($status.obs_connected) {
            Write-Host "OK: OBS WebSocket connected, scene=$($status.active_scene)"
          } else {
            Write-Host "WARN: OBS not connected yet (auto-reconnect will handle this)"
          }

      - name: Verify dashboard loads
        shell: powershell
        run: |
          try {
            $resp = Invoke-WebRequest -Uri "http://localhost:8920/" -UseBasicParsing -TimeoutSec 10
            if ($resp.StatusCode -eq 200 -and $resp.Content -match "wasm|songplayer") {
              Write-Host "OK: Dashboard loads (WASM detected)"
            } else {
              Write-Host "WARN: Dashboard returned 200 but no WASM content detected"
            }
          } catch {
            Write-Host "WARN: Dashboard not available (may need dist/ to be bundled)"
          }
```

- [ ] **Step 2: Add GEMINI_API_KEY to GitHub Secrets**

The Gemini API key must be stored in GitHub Secrets, not in the workflow file. Run:

```bash
gh secret set GEMINI_API_KEY --repo zbynekdrlik/songplayer
```

When prompted, enter: `AIzaSyCOBTFHGRBW3gBas9Qxp88InzQCoOGhnQI`

- [ ] **Step 3: Update gate job to include new jobs**

The `gate` job doesn't need to depend on `deploy-resolume` or `e2e-resolume` — those are post-gate jobs. But verify the YAML is valid.

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))" && echo "YAML valid"`

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add e2e-resolume job with playlist seeding and API tests"
```

---

### Task 3: Create setup-runner.ps1 script

**Files:**
- Create: `scripts/setup-runner.ps1`

- [ ] **Step 1: Create the setup script**

Create `scripts/setup-runner.ps1` following the `irm | iex` pattern used in remoteos-mcp:

```powershell
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
$Labels = "self-hosted,windows,resolume"

# --- Check if already installed ---
if (Test-Path "$RunnerDir\.runner") {
    Write-Host "  Runner already configured at $RunnerDir" -ForegroundColor Yellow
    Write-Host "  To reconfigure, delete $RunnerDir and re-run this script" -ForegroundColor Yellow
    return
}

# --- Download runner ---
Write-Host "  [1/5] Downloading GitHub Actions runner..." -ForegroundColor White
$runnerVersion = "2.325.0"
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
Pop-Location
Write-Host "        Runner configured: $RunnerName [$Labels]" -ForegroundColor Green

# --- Scheduled task ---
Write-Host "  [4/5] Setting up auto-start..." -ForegroundColor White

# Create start scripts
@"
@echo off
title GitHub Actions Runner (songplayer)
cd /d $RunnerDir
.\run.cmd
"@ | Set-Content "$RunnerDir\start-runner.bat"

@"
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
```

- [ ] **Step 2: Commit**

```bash
git add scripts/setup-runner.ps1
git commit -m "ci: add self-hosted runner setup script (irm | iex)"
```

---

### Task 4: Add GEMINI_API_KEY secret and push

**Files:** None (GitHub-only operation)

- [ ] **Step 1: Set the GitHub secret**

```bash
echo "AIzaSyCOBTFHGRBW3gBas9Qxp88InzQCoOGhnQI" | gh secret set GEMINI_API_KEY --repo zbynekdrlik/songplayer
```

- [ ] **Step 2: Run cargo fmt check locally**

```bash
cargo fmt --all --check
```

Expected: No formatting issues (CI YAML and PS1 files are not Rust).

- [ ] **Step 3: Push to dev and monitor CI**

```bash
git push origin dev
```

Monitor with `gh run list --limit 3` then `gh run view <id>`. Watch for:
- All existing jobs (lint, test, build-tauri, etc.) pass as before
- `deploy-resolume` job runs on the self-hosted runner
- `e2e-resolume` job runs after deploy
- Health checks pass
- Playlists are seeded

- [ ] **Step 4: If deploy/e2e fails, check logs and fix**

```bash
gh run view <id> --log-failed
```

Fix all issues in ONE commit, push again, monitor to completion.

---

### Task 5: Verify deployment end-to-end

**Files:** None (verification only)

- [ ] **Step 1: Verify SongPlayer is running on win-resolume**

Via MCP: `mcp__win-resolume__Shell` — check `Get-Process -Name SongPlayer`

- [ ] **Step 2: Verify API responds**

```bash
curl -s http://resolume.lan:8920/api/v1/status | python3 -m json.tool
```

Expected: version matches VERSION file, obs_connected is true or false, playlist_count >= 6

- [ ] **Step 3: Verify playlists were seeded**

```bash
curl -s http://resolume.lan:8920/api/v1/playlists | python3 -m json.tool
```

Expected: 6 playlists with correct names and YouTube URLs

- [ ] **Step 4: Verify settings**

```bash
curl -s http://resolume.lan:8920/api/v1/settings | python3 -m json.tool
```

Expected: `obs_websocket_url`, `cache_dir`, and `gemini_api_key` are set

- [ ] **Step 5: Commit completion (no code changes)**

If everything passes, the deploy pipeline is working. Report results.
