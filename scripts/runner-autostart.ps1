# SongPlayer GitHub Actions runner - the auto-start every deploy delivers (issue 274)
#
# The ONE source of the runner directory's start-runner.bat and
# start-runner.vbs. The scheduled task "GitHubActionsRunner" (setup-runner.ps1)
# runs the vbs at logon and every 5 minutes; the vbs starts the bat only when
# this directory has no listener (#223 follow-up, 9.10.2026: an unguarded vbs
# started another listener every 5 minutes).
#
# setup-runner.ps1 returns at once on a box whose runner is configured, so a
# change to these files never reached a box installed before it: PP kept the
# unguarded vbs, four listeners were alive on 14.10.2026, and a duplicate
# cancelled the 0.82.0 PP E2E (its job renewals were refused). So the
# installer AND both deploy jobs (ci.yml, deploy-pp.yml) run this script:
#
#   1. writes the two files when they differ from the repo's text;
#   2. ends every listener of this directory except the one running this job
#      (an ancestor of this process); outside a job it keeps the oldest.
#
# ASCII only: Windows PowerShell 5.1 reads a script without a BOM as cp1252.
# Usage: powershell.exe -NoProfile -ExecutionPolicy Bypass -File runner-autostart.ps1 [-RunnerDir C:\actions-runner]

param(
    [string]$RunnerDir = 'C:\actions-runner'
)

$ErrorActionPreference = 'Stop'

function Get-RunnerStartBat {
    param([string]$RunnerDir)
    return (@(
            '@echo off',
            'title GitHub Actions Runner (songplayer)',
            "cd /d $RunnerDir",
            '.\run.cmd'
        ) -join "`r`n")
}

function Get-RunnerStartVbs {
    param([string]$RunnerDir)
    $listener = "$RunnerDir\bin\Runner.Listener.exe"
    $bat = "$RunnerDir\start-runner.bat"
    return (@(
            'Set wmi = GetObject("winmgmts:\\.\root\cimv2")',
            'For Each p In wmi.ExecQuery("SELECT Name, ExecutablePath, CommandLine FROM Win32_Process WHERE Name=''Runner.Listener.exe'' OR Name=''cmd.exe''")',
            '  If LCase(p.Name) = "runner.listener.exe" Then',
            "    If LCase(p.ExecutablePath & `"`") = LCase(`"$listener`") Then WScript.Quit 0",
            "  ElseIf InStr(1, p.CommandLine & `"`", `"$bat`", vbTextCompare) > 0 Then",
            '    WScript.Quit 0',
            '  End If',
            'Next',
            'Set WshShell = CreateObject("WScript.Shell")',
            "WshShell.Run `"`"`"$bat`"`"`", 0, False"
        ) -join "`r`n")
}

# Line ends and a trailing newline do not count as a difference.
function ConvertTo-RunnerFileText {
    param([string]$Text)
    return (($Text -replace "`r`n", "`n").TrimEnd("`n"))
}

function Set-RunnerFile {
    param([string]$Path, [string]$Text)
    if (Test-Path -LiteralPath $Path) {
        $old = Get-Content -Raw -LiteralPath $Path
        if ($null -ne $old -and (ConvertTo-RunnerFileText $old) -ceq (ConvertTo-RunnerFileText $Text)) {
            return 'unchanged'
        }
    }
    Set-Content -LiteralPath $Path -Value $Text -Encoding Ascii
    return 'updated'
}

# The listeners to end: every one but the listener running this job (the one
# among $Ancestor), or, outside a job, the oldest (then the lowest id).
function Select-DuplicateListener {
    param([object[]]$Listener, [int[]]$Ancestor)
    $all = @($Listener)
    if ($all.Count -lt 2) { return @() }
    $mine = @($all | Where-Object { @($Ancestor) -contains $_.ProcessId })
    if ($mine.Count -gt 0) {
        $keep = $mine[0]
    } else {
        $keep = @($all | Sort-Object -Property StartTicks, ProcessId)[0]
    }
    return @($all | Where-Object { $_.ProcessId -ne $keep.ProcessId })
}

function Get-AncestorId {
    param([int]$ProcessId)
    $ids = New-Object System.Collections.Generic.List[int]
    $current = $ProcessId
    for ($depth = 0; $depth -lt 32; $depth++) {
        $process = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId=$current" -ErrorAction SilentlyContinue
        if ($null -eq $process) { break }
        $ids.Add([int]$process.ProcessId)
        $parent = [int]$process.ParentProcessId
        if ($parent -le 0 -or $ids.Contains($parent)) { break }
        $current = $parent
    }
    return $ids.ToArray()
}

function Get-RunnerListener {
    param([string]$RunnerDir)
    $exe = "$RunnerDir\bin\Runner.Listener.exe"
    $found = @(Get-CimInstance -ClassName Win32_Process -Filter "Name='Runner.Listener.exe'" -ErrorAction Stop |
            Where-Object { "$($_.ExecutablePath)" -ieq $exe })
    return @($found | ForEach-Object {
            [pscustomobject]@{
                ProcessId = [int]$_.ProcessId
                ParentProcessId = [int]$_.ParentProcessId
                StartTicks = $(if ($_.CreationDate) { ([datetime]$_.CreationDate).Ticks } else { [long]0 })
                Started = $_.CreationDate
            }
        })
}

# A duplicate's own tree: the cmd.exe that ran start-runner.bat / run.cmd for
# it (else run-helper could start it again), or the listener alone.
function Stop-DuplicateListener {
    param([object]$Duplicate)
    $root = $Duplicate.ProcessId
    $parent = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId=$($Duplicate.ParentProcessId)" -ErrorAction SilentlyContinue
    if ($null -ne $parent -and "$($parent.Name)" -ieq 'cmd.exe') { $root = [int]$parent.ProcessId }
    # taskkill reports a failure on stderr: with 'Stop', Windows PowerShell
    # 5.1 would throw on that line before the exit code is read.
    $ErrorActionPreference = 'Continue'
    $output = & taskkill.exe /PID $root /T /F 2>&1
    foreach ($line in @($output)) { Write-Host "runner-autostart:   $line" }
    return $LASTEXITCODE
}

function Invoke-RunnerAutostart {
    param([string]$RunnerDir)
    try {
        if (-not (Test-Path -LiteralPath "$RunnerDir\.runner")) {
            throw "$RunnerDir holds no configured runner (.runner is missing)"
        }
        $bat = Set-RunnerFile -Path "$RunnerDir\start-runner.bat" -Text (Get-RunnerStartBat -RunnerDir $RunnerDir)
        $vbs = Set-RunnerFile -Path "$RunnerDir\start-runner.vbs" -Text (Get-RunnerStartVbs -RunnerDir $RunnerDir)
        Write-Host "runner-autostart: start-runner.bat $bat, start-runner.vbs $vbs ($RunnerDir)"
        if ($vbs -eq 'updated') {
            Write-Host "::warning::runner-autostart: $env:COMPUTERNAME had another start-runner.vbs - the repo's is written now (issue 274)"
        }

        $listeners = @(Get-RunnerListener -RunnerDir $RunnerDir)
        $ancestors = @(Get-AncestorId -ProcessId $PID)
        $duplicates = @(Select-DuplicateListener -Listener $listeners -Ancestor $ancestors)
        Write-Host "runner-autostart: $($listeners.Count) listener(s) of $RunnerDir, $($duplicates.Count) duplicate(s)"
        $failed = 0
        foreach ($duplicate in $duplicates) {
            Write-Host "::warning::runner-autostart: ending a duplicate runner listener on $env:COMPUTERNAME (pid $($duplicate.ProcessId), started $($duplicate.Started)) - a duplicate takes jobs it cannot keep (issue 274)"
            if ((Stop-DuplicateListener -Duplicate $duplicate) -ne 0) { $failed++ }
        }
        $left = @(Get-RunnerListener -RunnerDir $RunnerDir)
        if ($failed -gt 0 -or $left.Count -gt 1) {
            Write-Host "runner-autostart: FAILED - $($left.Count) listener(s) of $RunnerDir still run: $(($left | ForEach-Object { $_.ProcessId }) -join ', ')"
            return 1
        }
        return 0
    } catch {
        Write-Host "runner-autostart: FAILED - $($_.Exception.Message)"
        return 1
    }
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
$exitCode = [int](@(Invoke-RunnerAutostart -RunnerDir $RunnerDir)[-1])
$scriptFile = Get-Variable -Name PSCommandPath -ValueOnly -ErrorAction SilentlyContinue
if ($scriptFile) { exit $exitCode }
# Run through iex or a scriptblock: never `exit` (it would close the
# operator's window); the code is in $LASTEXITCODE.
$global:LASTEXITCODE = $exitCode
