# box_commit_audit.ps1 (#207 phase-3) -- READ-ONLY commit / commit-over-RAM audit
# for win-resolume (no kills, no registry writes, no downloads). ASCII only, no
# Format-Table. Run: powershell -NoProfile -ExecutionPolicy Bypass -File <this>
$ErrorActionPreference = 'Stop'
$MB = 1MB

try {
    $procs = Get-Process -ErrorAction Stop
    $cim = Get-CimInstance Win32_Process -ErrorAction Stop
} catch {
    Write-Output ("n/a - cannot snapshot processes: {0}" -f $_.Exception.Message)
    exit 1
}
$parentOf = @{}
foreach ($p in $cim) { $parentOf[[int]$p.ProcessId] = [int]$p.ParentProcessId }
$livePids = @{}
foreach ($p in $procs) { $livePids[[int]$p.Id] = $true }

function CtrMB($path) {
    try { return [string][math]::Round((Get-Counter $path -ErrorAction Stop).CounterSamples[0].CookedValue / $MB) }
    catch { return 'n/a' }
}
function StartOf($p) {
    try { return $p.StartTime.ToString('yyyy-MM-dd HH:mm:ss') } catch { return 'n/a' }
}

# ---- commit ---------------------------------------------------------------
Write-Output '== commit =='
try {
    Write-Output ("committed_MB       = {0}" -f (CtrMB '\Memory\Committed Bytes'))
    Write-Output ("commit_limit_MB    = {0}" -f (CtrMB '\Memory\Commit Limit'))
    $os = Get-CimInstance Win32_OperatingSystem -ErrorAction Stop
    Write-Output ("free_virtual_MB    = {0}" -f [math]::Round($os.FreeVirtualMemory / 1024))
    Write-Output ("free_physical_MB   = {0}" -f [math]::Round($os.FreePhysicalMemory / 1024))
    try {
        # @() so a single page file is still an array; sum across all page files.
        $pf = @(Get-CimInstance Win32_PageFileUsage -ErrorAction Stop)
        if ($pf.Count -gt 0) {
            Write-Output ("pagefile_alloc_MB  = {0}" -f ($pf | Measure-Object AllocatedBaseSize -Sum).Sum)
            Write-Output ("pagefile_used_MB   = {0}" -f ($pf | Measure-Object CurrentUsage -Sum).Sum)
            Write-Output ("pagefile_peak_MB   = {0}" -f ($pf | Measure-Object PeakUsage -Sum).Sum)
        } else { Write-Output 'pagefile           = n/a - no page file configured' }
    } catch { Write-Output ("pagefile           = n/a - {0}" -f $_.Exception.Message) }
    Write-Output ("pool_paged_MB      = {0}" -f (CtrMB '\Memory\Pool Paged Bytes'))
    Write-Output ("pool_nonpaged_MB   = {0}" -f (CtrMB '\Memory\Pool Nonpaged Bytes'))
    Write-Output ("driver_total_MB    = {0}" -f (CtrMB '\Memory\System Driver Total Bytes'))
    Write-Output ("cache_MB           = {0}" -f (CtrMB '\Memory\Cache Bytes'))
    Write-Output ("standby_norm_MB    = {0}" -f (CtrMB '\Memory\Standby Cache Normal Priority Bytes'))
    Write-Output ("standby_reserve_MB = {0}" -f (CtrMB '\Memory\Standby Cache Reserve Bytes'))
    Write-Output ("standby_core_MB    = {0}" -f (CtrMB '\Memory\Standby Cache Core Bytes'))
} catch { Write-Output ("n/a - commit section failed: {0}" -f $_.Exception.Message) }

# ---- processes ------------------------------------------------------------
Write-Output ''
Write-Output '== processes =='
$sumPrivMB = 'n/a'
try {
    $sumPriv = ($procs | Measure-Object -Property PagedMemorySize64 -Sum).Sum
    $sumPrivMB = [math]::Round($sumPriv / $MB)
    Write-Output ("sum_private_commit_MB = {0}" -f $sumPrivMB)
    Write-Output 'top 12 by private commit (name pid commit_MB ws_MB threads start):'
    $top = $procs | Sort-Object -Property PagedMemorySize64 -Descending | Select-Object -First 12
    foreach ($p in $top) {
        Write-Output ("  {0,-24} {1,6} {2,8} {3,8} {4,4}  {5}" -f `
            $p.ProcessName, $p.Id, [math]::Round($p.PagedMemorySize64 / $MB), `
            [math]::Round($p.WorkingSet64 / $MB), $p.Threads.Count, (StartOf $p))
    }
} catch { Write-Output ("n/a - processes section failed: {0}" -f $_.Exception.Message) }

# ---- gap ------------------------------------------------------------------
Write-Output ''
Write-Output '== gap =='
try {
    $committedMB = CtrMB '\Memory\Committed Bytes'
    $poolPagedMB = CtrMB '\Memory\Pool Paged Bytes'
    $poolNpMB = CtrMB '\Memory\Pool Nonpaged Bytes'
    if (($committedMB -ne 'n/a') -and ($sumPrivMB -ne 'n/a') -and ($poolPagedMB -ne 'n/a') -and ($poolNpMB -ne 'n/a')) {
        $gap = [int]$committedMB - [int]$sumPrivMB - [int]$poolPagedMB - [int]$poolNpMB
        Write-Output ("non_process_commit_MB = {0}  (committed - sum_private - pools; sections/driver/AWE)" -f $gap)
    } else {
        Write-Output 'non_process_commit_MB = n/a - a committed/private/pool input was unreadable'
    }
} catch { Write-Output ("n/a - gap section failed: {0}" -f $_.Exception.Message) }

# ---- zombies --------------------------------------------------------------
Write-Output ''
Write-Output '== zombies =='
try {
    $zombies = $procs | Where-Object { $_.Threads.Count -eq 0 }
    if (-not $zombies) {
        Write-Output 'none (no process with 0 threads)'
    } else {
        Write-Output 'pid name start commit_MB parent_pid parent_alive:'
        foreach ($p in $zombies) {
            $pp = $parentOf[[int]$p.Id]; if ($null -eq $pp) { $pp = -1 }
            $alive = 'no'; if ($livePids.ContainsKey([int]$pp)) { $alive = 'yes' }
            Write-Output ("  {0,6} {1,-22} {2} {3,8} {4,6} {5}" -f `
                $p.Id, $p.ProcessName, (StartOf $p), [math]::Round($p.PagedMemorySize64 / $MB), $pp, $alive)
        }
        Write-Output 'hint: PowerShell cannot enumerate handle owners; the operator can run'
        Write-Output '      handle64.exe -a -p <pid>   (Sysinternals; not downloaded by this script)'
    }
} catch { Write-Output ("n/a - zombies section failed: {0}" -f $_.Exception.Message) }

# ---- python children ------------------------------------------------------
Write-Output ''
Write-Output '== python children =='
try {
    $py = $procs | Where-Object { $_.ProcessName -eq 'python' -and ($_.WorkingSet64 / $MB) -gt 200 }
    if (-not $py) {
        Write-Output 'none (no python.exe with WS > 200 MB)'
    } else {
        Write-Output 'pid commit_MB ws_MB start:'
        foreach ($p in $py) {
            Write-Output ("  {0,6} {1,8} {2,8} {3}" -f `
                $p.Id, [math]::Round($p.PagedMemorySize64 / $MB), [math]::Round($p.WorkingSet64 / $MB), (StartOf $p))
        }
    }
} catch { Write-Output ("n/a - python-children section failed: {0}" -f $_.Exception.Message) }

exit 0
