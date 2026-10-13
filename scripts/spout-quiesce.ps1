# Issue 243 follow-up: run by every workflow step that kills SongPlayer
# (taskkill /F, or Stop-ScheduledTask, which ends its process the same way),
# right before the kill.
#
# A kill that lands while one of SongPlayer's threads holds a Spout lock
# leaves that lock ABANDONED. Spout 2.007.017 never gives back a lock it was
# handed that way, so the receiver it goes to (Resolume Arena) keeps Spout's
# sender list for good and every new SongPlayer sender is refused (PP,
# 12.10.2026, after deploy-pp 38239301748: MAX and SP-program down until a
# manual rescue).
#
# So SongPlayer is asked first to drop both Spout senders and make no Spout
# call while the quiesce holds (60 s; the kill comes long before):
# POST /api/v1/program/spout/quiesce (at most 3 s), then GET /api/v1/program
# until max.state reads "quiesced", at most $TimeoutSec seconds (no read
# outlasts them). So the script takes at most ~8 s.
#
# Best effort: the caller kills SongPlayer whatever happens here. A
# SongPlayer that is not running, does not answer, answers 404 (a version
# before the route) or is not quiesced in time is only reported. The script
# never throws out of the step and never sets an exit code.
#
# Usage (in a `shell: powershell` step):
#   & "$env:GITHUB_WORKSPACE\scripts\spout-quiesce.ps1"

param(
    [string]$BaseUrl = "http://localhost:8920",
    [int]$TimeoutSec = 5
)

try {
    if (-not (Get-Process -Name "SongPlayer" -ErrorAction SilentlyContinue)) {
        Write-Host "Spout quiesce: SongPlayer is not running - nothing to quiesce"
        return
    }
    try {
        $answer = Invoke-RestMethod -Method Post -Uri "$BaseUrl/api/v1/program/spout/quiesce" -TimeoutSec 3 -ErrorAction Stop
        Write-Host "Spout quiesce: asked (max.state was '$($answer.state)')"
    } catch {
        Write-Host "Spout quiesce: SongPlayer did not take it ($($_.Exception.Message)) - it is stopped anyway"
        return
    }
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    $state = "unread"
    while ((Get-Date) -lt $deadline) {
        # Each read is bounded by the time left: whole seconds (-TimeoutSec
        # takes an int, and 0 would mean no limit at all), at most 2. With
        # under a second left no read starts, so no read outlasts the
        # deadline: the whole call takes at most the POST's 3 s plus
        # $TimeoutSec.
        $left = ($deadline - (Get-Date)).TotalSeconds
        $readFor = [int][Math]::Min(2, [Math]::Floor($left))
        if ($readFor -lt 1) {
            break
        }
        try {
            $program = Invoke-RestMethod -Uri "$BaseUrl/api/v1/program" -TimeoutSec $readFor -ErrorAction Stop
            $state = $program.max.state
            if ($state -eq "quiesced") {
                Write-Host "Spout quiesce: done - its Spout senders are dropped, no Spout call for the next 60 s"
                return
            }
        } catch {
            $state = "unreadable ($($_.Exception.Message))"
        }
        Start-Sleep -Milliseconds 100
    }
    Write-Host "Spout quiesce: not quiesced within $TimeoutSec s (max.state '$state') - it is stopped anyway"
} catch {
    Write-Host "Spout quiesce: $($_.Exception.Message) - it is stopped anyway"
}
