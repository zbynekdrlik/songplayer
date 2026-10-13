# SongPlayer: apply the production PC profile (issue 255, lane 2).
#
# The owner's directive (12.10.2026): everything found and set by hand on PP
# and SNV is built into SongPlayer, so a reinstalled or new PC is never set
# up from memory. SongPlayer CHECKS the profile (lane 1: crates/sp-server
# prod_env*.rs, the HealthBar, the post-deploy gate
# e2e/post-deploy-environment.spec.ts); this script APPLIES it. SongPlayer
# itself never changes a system setting: this script does, while SongPlayer
# is stopped (every deploy runs it between the stop and the start, so an
# adapter restart costs nothing extra).
#
# What it applies, in this order (the evidence: .claude/rules/prod-environment.md):
#   1. Windows Update: policies NoAutoUpdate = 1 (...\WindowsUpdate\AU) and
#      ExcludeWUDriversInQualityUpdate = 1 (...\WindowsUpdate). A Windows
#      Update driver push would undo PP's NDIS driver swap (issue 233).
#   2. MMCSS: SystemResponsiveness 0, NetworkThrottlingIndex 0xFFFFFFFF, the
#      Pro Audio task Priority 6 and Scheduling Category High (issue 210
#      part 2, .claude/rules/vban-out.md).
#   3. The SoundGrid adapter (only when the SoundGridProtocol service is
#      installed): the adapter whose MAC is <LocalLanPort> in
#      SoundGrid.Driver.preferences of the user SongPlayer runs as (the
#      SongPlayer scheduled task's principal; with no task yet, the user
#      running this script). *InterruptModeration 0, *FlowControl 0, and
#      where the driver has them *EEE / EnableGreenEthernet (GreenEthernet) /
#      PowerSavingMode (*PowerSavingMode) / GigaLite 0
#      (Set-NetAdapterAdvancedProperty), PnPCapabilities with bit 0x08
#      (NDIS_DEVICE_DISABLE_PM) set, then ONE adapter restart. Waves'
#      SoundGrid PC guide (5.11.2025); PP's values of 12.10.2026.
#   4. Power: the ACTIVE scheme's processor core parking minimum
#      (SUB_PROCESSOR CPMINCORES, AC) = 100 %, never another scheme (Waves'
#      guide; PP Bitsum Highest Performance 100, SNV High performance 100).
#   5. Defender and the firewall (issue 233: Defender's scan of every
#      process SongPlayer launched made SoundGrid resync, A/B/A on PP), as
#      PP and SNV were set on 11.10.2026: Set-MpPreference
#      DisableRealtimeMonitoring / DisableBehaviorMonitoring /
#      DisableIOAVProtection / DisableScriptScanning true, MAPSReporting 0
#      (off), SubmitSamplesConsent 2 (never send); the policy keys
#      HKLM\SOFTWARE\Policies\Microsoft\Windows Defender\Real-Time Protection
#      Disable{RealtimeMonitoring,BehaviorMonitoring,OnAccessProtection,
#      ScanOnRealtimeEnable,IOAVProtection} = 1; every firewall profile off
#      (Set-NetFirewallProfile). Tamper Protection blocks the Defender
#      changes: it is turned off by hand in Windows Security (Virus & threat
#      protection > Manage settings), and this script says so and fails.
# Not here: the NIC driver swap (lane 3), the Dante adapter (report only),
# CPU reservation (unmeasured), the Windows edition.
#
# How it works: READ the facts into one plain object, PLAN (Get-ProdPcPlan,
# pure: only what differs, in the order above), APPLY each action (one line
# per action, before -> after), RE-READ and verify every applied value; any
# error, mismatch or blocker (something the script cannot do for the
# operator, e.g. no SoundGrid port chosen) = a non-zero exit naming it. A
# second run plans nothing.
#
# Modes:
#   (default)          apply. Must run elevated (as Administrator).
#   -Plan              read and print the plan, change nothing, exit 0.
#   -Plan -FactsJson f plan from a facts file instead of reading this PC
#                      (the tests' fixtures; never used by a deploy).
#   -SaveFacts f       also write the facts as read to f (JSON), e.g. to
#                      capture a box for a fixture.
# Exit codes: 0 done (or nothing to change), 1 failed, 2 wrong arguments,
# 3 not elevated / not Windows.
#
# A new or reinstalled PC (elevated Windows PowerShell):
#   irm https://raw.githubusercontent.com/zbynekdrlik/songplayer/main/scripts/setup-prod-pc.ps1 | iex
# iex passes no arguments; for the plan only, use the scriptblock form:
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/zbynekdrlik/songplayer/main/scripts/setup-prod-pc.ps1))) -Plan
# Run either in a fresh window: the script sets strict mode in that session.
#
# A deploy step runs it in a child process (a throw never ends the step
# before SongPlayer is started again):
#   powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$env:GITHUB_WORKSPACE\scripts\setup-prod-pc.ps1"
#
# Windows PowerShell 5.1 reads this file as cp1252: keep it ASCII.

param(
    [switch]$Plan,
    [string]$FactsJson = '',
    [string]$SaveFacts = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# ---------------------------------------------------------------------------
# Pure helpers (Linux-tested with Pester: scripts/tests/setup-prod-pc.Tests.ps1)
# ---------------------------------------------------------------------------

# A member of a facts object (a PSCustomObject from JSON, or a dictionary),
# found by its exact name (case ignored, never a wildcard: '*EEE' is a
# registry name); $null when it has none.
function Find-ProdPcMember {
    param($Object, [string]$Name)
    if ($null -eq $Object) { return $null }
    if ($Object -is [System.Collections.IDictionary]) {
        if ($Object.Contains($Name)) {
            return [pscustomobject]@{ Name = $Name; Value = $Object[$Name] }
        }
        return $null
    }
    foreach ($member in $Object.PSObject.Properties) {
        if ($member.Name -eq $Name) { return $member }
    }
    return $null
}

# A scalar field of a facts object ($null when absent).
function Get-ProdPcField {
    param($Object, [string]$Name)
    $member = Find-ProdPcMember -Object $Object -Name $Name
    if ($null -eq $member) { return $null }
    return $member.Value
}

# A list field of a facts object, its items emitted one by one (callers
# collect them with @(...)); nothing when absent or null.
function Get-ProdPcList {
    param($Object, [string]$Name)
    $member = Find-ProdPcMember -Object $Object -Name $Name
    if ($null -eq $member -or $null -eq $member.Value) { return }
    return $member.Value
}

# A number as a DWORD (0..4294967295), or $null when it is none. A DWORD the
# registry gave as a signed Int32 (0xFFFFFFFF reads -1) is taken as its
# unsigned value; a string must be decimal digits (lane 1's u32 parse).
function ConvertTo-ProdPcUInt32 {
    param($Value)
    if ($null -eq $Value -or $Value -is [bool]) { return $null }
    if ($Value -is [string]) {
        $text = $Value.Trim()
        if ($text -notmatch '^\+?[0-9]{1,10}$') { return $null }
        $number = [decimal]::Parse($text, [System.Globalization.CultureInfo]::InvariantCulture)
    } else {
        try { $number = [decimal]$Value } catch { return $null }
        if ($number -lt 0 -and $number -ge -2147483648) { $number = $number + 4294967296 }
    }
    if ($number -lt 0 -or $number -gt 4294967295 -or $number -ne [math]::Floor($number)) { return $null }
    return [uint32]$number
}

# A DWORD's 32 bits as the signed Int32 New-ItemProperty writes.
function ConvertTo-ProdPcInt32 {
    param($Value)
    $dword = ConvertTo-ProdPcUInt32 $Value
    if ($null -eq $dword) { throw "'$Value' is no DWORD" }
    return [BitConverter]::ToInt32([BitConverter]::GetBytes([uint32]$dword), 0)
}

# A MAC as 12 lowercase hex digits ('6c:24:08:e5:9c:91', '6C-24-08-E5-9C-91'
# and '6C2408E59C91' all read '6c2408e59c91'); $null for anything else.
# The same rule as lane 1's normalize_mac.
function ConvertTo-MacKey {
    param($Text)
    if ($null -eq $Text) { return $null }
    $hex = ([string]$Text).Trim() -replace '[:\-.]', ''
    if ($hex -notmatch '^[0-9a-fA-F]{12}$') { return $null }
    return $hex.ToLowerInvariant()
}

# A MAC as an operator reads it: 6c:24:08:e5:9c:91.
function Format-MacKey {
    param([string]$Mac)
    if ($Mac -notmatch '^[0-9a-f]{12}$') { return $Mac }
    return (($Mac -split '(..)' | Where-Object { $_ }) -join ':')
}

# The port the SoundGrid driver chose: <Configuration><LocalLanPort> of its
# preferences (lane 1's chosen_port): state 'set' with the MAC, 'unset' (no
# or an empty LocalLanPort), or 'unreadable' with why (not XML, no or two
# <Configuration>, a comment inside it, two ports, attributes, no MAC).
function Get-SoundGridPort {
    param([string]$Text)
    $unreadable = { param($why) [pscustomobject]@{ State = 'unreadable'; Mac = $null; Why = $why } }
    $document = New-Object System.Xml.XmlDocument
    $document.XmlResolver = $null
    try {
        $document.LoadXml($Text.TrimStart([char]0xFEFF))
    } catch {
        return (& $unreadable "the preferences are no XML ($($_.Exception.Message))")
    }
    $configurations = @($document.SelectNodes('//Configuration'))
    if ($configurations.Count -ne 1) {
        return (& $unreadable "$($configurations.Count) <Configuration> elements, not 1")
    }
    $configuration = $configurations[0]
    if (@($configuration.SelectNodes('.//comment()')).Count -gt 0) {
        return (& $unreadable 'a comment inside <Configuration>')
    }
    $ports = @($configuration.SelectNodes('.//LocalLanPort'))
    if ($ports.Count -eq 0) {
        return [pscustomobject]@{ State = 'unset'; Mac = $null; Why = 'no <LocalLanPort>' }
    }
    if ($ports.Count -gt 1) { return (& $unreadable '<LocalLanPort> twice') }
    if ($ports[0].Attributes.Count -gt 0) { return (& $unreadable '<LocalLanPort> with attributes') }
    $value = $ports[0].InnerText.Trim()
    if ($value -eq '') {
        return [pscustomobject]@{ State = 'unset'; Mac = $null; Why = 'an empty <LocalLanPort>' }
    }
    $mac = ConvertTo-MacKey $value
    if ($null -eq $mac) { return (& $unreadable "<LocalLanPort> '$value' is no MAC") }
    return [pscustomobject]@{ State = 'set'; Mac = $mac; Why = $null }
}

# The PnPCapabilities to write so that NDIS_DEVICE_DISABLE_PM (0x08) is set
# ("Allow the computer to turn off this device to save power" cleared), or
# $null when it already is. Missing, 0 or no number: 24 (0x18, what the
# adapter's Power Management tab writes); any other value keeps its bits
# and gets 0x08 too.
function Get-PnPCapabilitiesWanted {
    param($Value)
    if ($null -eq $Value) { return [uint32]24 }
    $number = ConvertTo-ProdPcUInt32 ([string]$Value)
    if ($null -eq $number -or $number -eq 0) { return [uint32]24 }
    if ((([int64]$number) -band ([int64]8)) -ne 0) { return $null }
    return [uint32](([int64]$number) -bor ([int64]8))
}

# The CPMINCORES AC index from `powercfg /qh SCHEME_CURRENT SUB_PROCESSOR
# CPMINCORES`: the English label, else the second-to-last hex value (the
# output always ends with the AC then the DC index, in any language).
function Get-PowercfgAcIndex {
    param([string]$Text)
    $label = [regex]::Match($Text, 'Current AC Power Setting Index:\s*0x([0-9a-fA-F]+)')
    if ($label.Success) { return [Convert]::ToUInt32($label.Groups[1].Value, 16) }
    $values = [regex]::Matches($Text, '0x([0-9a-fA-F]{8})')
    if ($values.Count -ge 2) { return [Convert]::ToUInt32($values[$values.Count - 2].Groups[1].Value, 16) }
    return $null
}

# The registry values the profile sets, in the plan's order (Windows Update,
# MMCSS, Defender's policy keys). The read reads exactly these.
function Get-ProdPcRegistryWants {
    $wu = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate'
    $mm = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile'
    $pa = "$mm\Tasks\Pro Audio"
    $rtp = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows Defender\Real-Time Protection'
    $whyWu = 'a Windows Update driver push would undo the NDIS driver swap (issues 233, 255)'
    $whyMm = 'MMCSS Pro Audio for the audio threads (issue 210 part 2)'
    $whyDef = 'Defender scanned every process SongPlayer launched: SoundGrid resyncs (issue 233)'
    $want = {
        param($area, $path, $name, $type, $wanted, $reason)
        [pscustomobject]@{ Area = $area; Path = $path; Name = $name; Type = $type; Wanted = $wanted; Reason = $reason }
    }
    & $want 'windows_update' "$wu\AU" 'NoAutoUpdate' 'DWord' ([uint32]1) $whyWu
    & $want 'windows_update' $wu 'ExcludeWUDriversInQualityUpdate' 'DWord' ([uint32]1) $whyWu
    & $want 'mmcss' $mm 'SystemResponsiveness' 'DWord' ([uint32]0) $whyMm
    & $want 'mmcss' $mm 'NetworkThrottlingIndex' 'DWord' ([uint32]4294967295) $whyMm
    & $want 'mmcss' $pa 'Priority' 'DWord' ([uint32]6) $whyMm
    & $want 'mmcss' $pa 'Scheduling Category' 'String' 'High' $whyMm
    foreach ($name in @('DisableRealtimeMonitoring', 'DisableBehaviorMonitoring', 'DisableOnAccessProtection', 'DisableScanOnRealtimeEnable', 'DisableIOAVProtection')) {
        & $want 'defender_policy' $rtp $name 'DWord' ([uint32]1) $whyDef
    }
}

# Defender's preferences (Set-MpPreference), in the plan's order.
function Get-ProdPcDefenderWants {
    $why = 'Defender scanned every process SongPlayer launched: SoundGrid resyncs (issue 233)'
    foreach ($name in @('DisableRealtimeMonitoring', 'DisableBehaviorMonitoring', 'DisableIOAVProtection', 'DisableScriptScanning')) {
        [pscustomobject]@{ Name = $name; Wanted = $true; Reason = $why }
    }
    [pscustomobject]@{ Name = 'MAPSReporting'; Wanted = [uint32]0; Reason = "$why; MAPS reporting off" }
    [pscustomobject]@{ Name = 'SubmitSamplesConsent'; Wanted = [uint32]2; Reason = "$why; never send samples" }
}

# The SoundGrid adapter's settings (lane 1's keywords: the first name that
# exists is the setting's; a required one the driver lacks is a blocker).
function Get-ProdPcNicGroups {
    $why = "Waves' SoundGrid PC guide"
    [pscustomobject]@{ Setting = 'interrupt_moderation'; Keywords = @('*InterruptModeration'); Required = $true; Reason = $why }
    [pscustomobject]@{ Setting = 'flow_control'; Keywords = @('*FlowControl'); Required = $true; Reason = $why }
    [pscustomobject]@{ Setting = 'eee'; Keywords = @('*EEE'); Required = $false; Reason = "$why (EEE breaks SoundGrid's sync)" }
    [pscustomobject]@{ Setting = 'green_ethernet'; Keywords = @('EnableGreenEthernet', 'GreenEthernet'); Required = $false; Reason = $why }
    [pscustomobject]@{ Setting = 'power_saving'; Keywords = @('PowerSavingMode', '*PowerSavingMode'); Required = $false; Reason = $why }
    [pscustomobject]@{ Setting = 'gigalite'; Keywords = @('GigaLite'); Required = $false; Reason = $why }
}

# Whether a registry fact holds what a want sets: a DWORD of that value (a
# value of another type does not), or a string that reads the same with
# case and padding ignored (lane 1's rule for Scheduling Category).
function Test-ProdPcRegistryHolds {
    param($Want, $Fact)
    if ($null -eq $Fact) { return $false }
    $kind = [string](Get-ProdPcField $Fact 'kind')
    $value = Get-ProdPcField $Fact 'value'
    if ($Want.Type -eq 'DWord') {
        if ($kind -ne 'DWord') { return $false }
        $number = ConvertTo-ProdPcUInt32 $value
        return ($null -ne $number -and $number -eq [uint32]$Want.Wanted)
    }
    if ($kind -ne 'String' -and $kind -ne 'ExpandString') { return $false }
    return (([string]$value).Trim() -ieq [string]$Want.Wanted)
}

# A registry fact as the plan shows it: '(none)', the value, or the value
# and its type when the type is not the one wanted.
function Format-ProdPcRegistryFact {
    param($Want, $Fact)
    $kind = [string](Get-ProdPcField $Fact 'kind')
    if ($null -eq $Fact -or $kind -eq '') { return '(none)' }
    $value = Get-ProdPcField $Fact 'value'
    if ($kind -ne $Want.Type) { return "$value ($kind)" }
    return "$value"
}

# The registry fact the read recorded for a path and a value name.
function Find-ProdPcRegistryFact {
    param($Facts, [string]$Path, [string]$Name)
    foreach ($entry in @(Get-ProdPcList $Facts 'registry')) {
        if ((Get-ProdPcField $entry 'path') -eq $Path -and (Get-ProdPcField $entry 'name') -eq $Name) {
            return $entry
        }
    }
    return $null
}

# One planned action. Area + Name (+ the adapter) identify it; Current and
# Wanted are what the log shows; Kind and the rest say how to apply it.
function New-ProdPcAction {
    param([string]$Area, [string]$Name, $Current, $Wanted, [string]$Reason, [string]$Kind, [hashtable]$Target = @{})
    $action = [ordered]@{
        Area = $Area; Name = $Name; Current = $Current; Wanted = $Wanted; Reason = $Reason; Kind = $Kind
        Path = $null; Type = $null; Adapter = $null; AdapterGuid = $null
    }
    foreach ($key in @($Target.Keys)) { $action[$key] = $Target[$key] }
    return [pscustomobject]$action
}

# Something the script cannot do for the operator: a code (lane 1's
# problem code where there is one) and its text.
function New-ProdPcBlocker {
    param([string]$Code, [string]$Text)
    return [pscustomobject]@{ Code = $Code; Text = $Text }
}

# The adapters with a MAC: those whose current address is it, else those
# whose permanent address is it (lane 1: the current address first).
function Find-ProdPcAdapterByMac {
    param($Adapters, [string]$Mac)
    $current = @()
    $permanent = @()
    foreach ($adapter in @($Adapters)) {
        if ((ConvertTo-MacKey (Get-ProdPcField $adapter 'mac')) -eq $Mac) {
            $current += $adapter
        } elseif ((ConvertTo-MacKey (Get-ProdPcField $adapter 'permanent_mac')) -eq $Mac) {
            $permanent += $adapter
        }
    }
    if ($current.Count -gt 0) { return $current }
    return $permanent
}

# The SoundGrid adapter's actions and blockers.
function Add-ProdPcSoundGridReview {
    param($Facts, $Actions, $Blockers)
    $soundgrid = Get-ProdPcField $Facts 'soundgrid'
    if ($true -ne (Get-ProdPcField $soundgrid 'installed')) { return }
    $why = Get-ProdPcField $soundgrid 'preferences_error'
    $text = Get-ProdPcField $soundgrid 'preferences'
    $unsetText = 'SoundGrid nema zvoleny sietovy port - nastav ho v SoundGrid Studio / ovladaci (this script cannot choose the port for the operator)'
    if ($null -ne $why) {
        $Blockers.Add((New-ProdPcBlocker 'unreadable:soundgrid' "SoundGrid's preferences could not be read: $why"))
        return
    }
    if ($null -eq $text) {
        $Blockers.Add((New-ProdPcBlocker 'soundgrid_port_unset' "$unsetText; no file $(Get-ProdPcField $soundgrid 'preferences_path')"))
        return
    }
    $port = Get-SoundGridPort ([string]$text)
    if ($port.State -eq 'unreadable') {
        $Blockers.Add((New-ProdPcBlocker 'unreadable:soundgrid' "SoundGrid's preferences: $($port.Why)"))
        return
    }
    if ($port.State -eq 'unset') {
        $Blockers.Add((New-ProdPcBlocker 'soundgrid_port_unset' "$unsetText; $($port.Why)"))
        return
    }
    $found = @(Find-ProdPcAdapterByMac -Adapters @(Get-ProdPcList $soundgrid 'adapters') -Mac $port.Mac)
    $shown = Format-MacKey $port.Mac
    if ($found.Count -eq 0) {
        $Blockers.Add((New-ProdPcBlocker 'soundgrid_port_missing' "sietova karta s MAC $shown, ktoru ma ovladac SoundGrid zvolenu ako port, sa nenasla"))
        return
    }
    if ($found.Count -gt 1) {
        $Blockers.Add((New-ProdPcBlocker 'unreadable:soundgrid' "$($found.Count) adapters have SoundGrid's MAC $shown"))
        return
    }
    $adapter = $found[0]
    $name = [string](Get-ProdPcField $adapter 'name')
    $guid = [string](Get-ProdPcField $adapter 'guid')
    $settings = Get-ProdPcField $adapter 'settings'
    $classKey = Get-ProdPcField $adapter 'class_key'
    if ($null -eq $settings -or $null -eq $classKey) {
        $Blockers.Add((New-ProdPcBlocker 'unreadable:soundgrid_nic' "the SoundGrid adapter '$name' ($guid) has no instance in the network class"))
        return
    }
    $target = @{ Adapter = $name; AdapterGuid = $guid; Path = $classKey }
    foreach ($group in @(Get-ProdPcNicGroups)) {
        $keyword = $null
        foreach ($candidate in $group.Keywords) {
            if ($null -ne (Find-ProdPcMember -Object $settings -Name $candidate)) { $keyword = $candidate; break }
        }
        if ($null -eq $keyword) {
            if ($group.Required) {
                $Blockers.Add((New-ProdPcBlocker "soundgrid_nic_setting:$($group.Setting)" "sietova karta SoundGrid '$name': nastavenie $($group.Keywords[0]) chyba - its driver does not offer it, so it cannot be turned off"))
            }
            continue
        }
        $value = ([string](Get-ProdPcField $settings $keyword)).Trim()
        $number = ConvertTo-ProdPcUInt32 $value
        if ($null -ne $number -and $number -eq 0) { continue }
        $Actions.Add((New-ProdPcAction -Area 'soundgrid_nic' -Name $keyword -Current $value -Wanted '0' -Reason $group.Reason -Kind 'nic_keyword' -Target $target))
    }
    $pnp = Get-ProdPcField $settings 'PnPCapabilities'
    $pnpWanted = Get-PnPCapabilitiesWanted $pnp
    if ($null -ne $pnpWanted) {
        $current = if ($null -eq $pnp) { '(none)' } else { ([string]$pnp).Trim() }
        $Actions.Add((New-ProdPcAction -Area 'soundgrid_nic' -Name 'PnPCapabilities' -Current $current -Wanted $pnpWanted -Reason "Windows must never turn the SoundGrid adapter off (Waves' guide)" -Kind 'nic_pnp' -Target $target))
    }
}

# Every action the profile needs on this PC (only what differs, in the
# order of the header) and every blocker.
function Get-ProdPcReview {
    param($Facts)
    $actions = New-Object System.Collections.Generic.List[object]
    $blockers = New-Object System.Collections.Generic.List[object]
    $wants = @(Get-ProdPcRegistryWants)
    $addRegistry = {
        param($area)
        foreach ($want in $wants) {
            if ($want.Area -ne $area) { continue }
            $fact = Find-ProdPcRegistryFact -Facts $Facts -Path $want.Path -Name $want.Name
            if (Test-ProdPcRegistryHolds -Want $want -Fact $fact) { continue }
            $actions.Add((New-ProdPcAction -Area $want.Area -Name $want.Name -Current (Format-ProdPcRegistryFact -Want $want -Fact $fact) -Wanted $want.Wanted -Reason $want.Reason -Kind 'registry' -Target @{ Path = $want.Path; Type = $want.Type }))
        }
    }
    & $addRegistry 'windows_update'
    & $addRegistry 'mmcss'
    Add-ProdPcSoundGridReview -Facts $Facts -Actions $actions -Blockers $blockers

    $power = Get-ProdPcField $Facts 'power'
    $cores = ConvertTo-ProdPcUInt32 (Get-ProdPcField $power 'cpmincores_ac')
    if ($null -eq $cores -or $cores -ne 100) {
        $current = if ($null -eq $cores) { '(none)' } else { $cores }
        $actions.Add((New-ProdPcAction -Area 'power' -Name 'CPMINCORES' -Current $current -Wanted ([uint32]100) -Reason "no core parking on the active scheme (Waves' guide)" -Kind 'power'))
    }

    $defender = Get-ProdPcField $Facts 'defender'
    $service = [string](Get-ProdPcField $defender 'service')
    $before = $actions.Count
    if ($service -eq 'Running') {
        $preferences = Get-ProdPcField $defender 'preferences'
        foreach ($want in @(Get-ProdPcDefenderWants)) {
            $value = Get-ProdPcField $preferences $want.Name
            if ($want.Wanted -is [bool]) {
                $holds = ($value -is [bool] -and $value -eq $want.Wanted)
            } else {
                $number = ConvertTo-ProdPcUInt32 $value
                $holds = ($null -ne $number -and $number -eq $want.Wanted)
            }
            if ($holds) { continue }
            $current = if ($null -eq $value) { '(none)' } else { $value }
            $actions.Add((New-ProdPcAction -Area 'defender' -Name $want.Name -Current $current -Wanted $want.Wanted -Reason $want.Reason -Kind 'defender'))
        }
    } elseif ($service -ne 'Stopped' -and $service -ne 'missing') {
        $blockers.Add((New-ProdPcBlocker 'unreadable:defender' "the WinDefend service is '$service': its preferences cannot be read now; run this again"))
    }
    & $addRegistry 'defender_policy'
    $defenderPending = $actions.Count -gt $before
    if ($service -eq 'Running' -and $true -eq (Get-ProdPcField $defender 'realtime_enabled') -and -not $defenderPending) {
        $text = 'ochrana Windows Defender v realnom case je stale zapnuta, hoci jej nastavenia su vypnute'
        if ($true -eq (Get-ProdPcField $defender 'tamper_protected')) {
            $text = "$text - Tamper Protection is on: turn it off in Windows Security (Virus & threat protection > Manage settings > Tamper Protection), then run this again"
        }
        $blockers.Add((New-ProdPcBlocker 'defender_realtime_on' $text))
    }

    $firewall = Get-ProdPcField $Facts 'firewall'
    foreach ($profileName in @('Domain', 'Private', 'Public')) {
        $enabled = Get-ProdPcField $firewall $profileName
        if ($enabled -is [bool] -and -not $enabled) { continue }
        $current = if ($enabled -is [bool]) { 'on' } elseif ($null -eq $enabled) { '(unread)' } else { "$enabled" }
        $actions.Add((New-ProdPcAction -Area 'firewall' -Name $profileName -Current $current -Wanted 'off' -Reason 'the production rule: firewall off (issue 233, the owner, 11.10.2026)' -Kind 'firewall'))
    }
    return [pscustomobject]@{ Actions = $actions.ToArray(); Blockers = $blockers.ToArray() }
}

# The plan: facts -> the ordered actions {Area, Name, Current, Wanted,
# Reason} (plus how to apply each), ONLY what differs. Emitted one by one:
# collect with @(Get-ProdPcPlan $facts).
function Get-ProdPcPlan {
    param($Facts)
    return (Get-ProdPcReview -Facts $Facts).Actions
}

# What the script cannot do for the operator (emitted one by one).
function Get-ProdPcBlockers {
    param($Facts)
    return (Get-ProdPcReview -Facts $Facts).Blockers
}

# The key that identifies an action across two reads.
function Get-ProdPcActionKey {
    param($Action)
    return "$($Action.Area)|$($Action.AdapterGuid)|$($Action.Name)"
}

# One action as the log shows it.
function Format-ProdPcAction {
    param($Action)
    $on = if ($Action.Adapter) { "adapter '$($Action.Adapter)' " } else { '' }
    return "$($Action.Area) $on$($Action.Name): $($Action.Current) -> $($Action.Wanted)"
}

# ---------------------------------------------------------------------------
# Windows glue: the read and the apply (run on the boxes; the read also on
# the Windows CI job, plan only)
# ---------------------------------------------------------------------------

# A native command's exit code and output (stderr included) as text. Native
# stderr must never become a terminating error here: ErrorActionPreference
# is Continue around the call.
function Invoke-ProdPcNative {
    param([string]$File, [string[]]$Arguments)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $lines = @(& $File @Arguments 2>&1 | ForEach-Object { "$_" })
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
    return [pscustomobject]@{ Code = $code; Text = ($lines -join "`n") }
}

function Test-ProdPcWindows {
    return ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT)
}

function Test-ProdPcElevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# One registry value as a fact: {path, name, kind, value}; kind and value
# null when the key or the value does not exist. A DWORD is kept unsigned.
function Read-ProdPcRegistryValue {
    param([string]$Path, [string]$Name)
    $entry = [ordered]@{ path = $Path; name = $Name; kind = $null; value = $null }
    if (-not (Test-Path -LiteralPath $Path)) { return [pscustomobject]$entry }
    $key = Get-Item -LiteralPath $Path
    if (@($key.GetValueNames()) -notcontains $Name) { return [pscustomobject]$entry }
    $kind = [string]$key.GetValueKind($Name)
    $raw = $key.GetValue($Name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $entry.kind = $kind
    if ($kind -eq 'DWord') {
        $entry.value = ConvertTo-ProdPcUInt32 $raw
    } elseif ($kind -eq 'MultiString') {
        $entry.value = (@($raw) -join ' ')
    } else {
        $entry.value = "$raw"
    }
    return [pscustomobject]$entry
}

# A network adapter instance's settings (only the keywords the profile
# reads, each as text: a DWORD in decimal, a multi-string joined).
function Read-ProdPcNicSettings {
    param([string]$KeyPath)
    $key = Get-Item -LiteralPath $KeyPath
    $names = @($key.GetValueNames())
    $keywords = New-Object System.Collections.Generic.List[string]
    foreach ($group in @(Get-ProdPcNicGroups)) { foreach ($keyword in $group.Keywords) { $keywords.Add($keyword) } }
    $keywords.Add('PnPCapabilities')
    $settings = [ordered]@{}
    foreach ($keyword in $keywords) {
        if ($names -notcontains $keyword) { continue }
        $kind = [string]$key.GetValueKind($keyword)
        $raw = $key.GetValue($keyword, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if ($kind -eq 'DWord') {
            $settings[$keyword] = [string](ConvertTo-ProdPcUInt32 $raw)
        } elseif ($kind -eq 'MultiString') {
            $settings[$keyword] = (@($raw) -join ' ')
        } else {
            $settings[$keyword] = ("$raw").Trim()
        }
    }
    return [pscustomobject]$settings
}

# Every physical adapter: names, addresses, its instance key in the network
# class (found by NetCfgInstanceId; only the 4-digit keys are opened: the
# class key also holds a 'Properties' key an administrator cannot read) and
# its settings.
function Read-ProdPcAdapters {
    $classPath = 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e972-e325-11ce-bfc1-08002be10318}'
    $class = Get-Item -LiteralPath $classPath
    $instances = @{}
    foreach ($subName in @($class.GetSubKeyNames())) {
        if ($subName -notmatch '^[0-9]{4}$') { continue }
        $sub = $class.OpenSubKey($subName)
        if ($null -eq $sub) { continue }
        try {
            $id = $sub.GetValue('NetCfgInstanceId')
            if ($null -ne $id -and "$id" -ne '') { $instances[("$id").ToUpperInvariant()] = "$classPath\$subName" }
        } finally {
            $sub.Close()
        }
    }
    foreach ($adapter in @(Get-NetAdapter -Physical)) {
        $guid = [string]$adapter.InterfaceGuid
        $keyPath = $instances[$guid.ToUpperInvariant()]
        $settings = $null
        if ($null -ne $keyPath) { $settings = Read-ProdPcNicSettings -KeyPath $keyPath }
        [pscustomobject][ordered]@{
            name = [string]$adapter.Name
            guid = $guid
            description = [string]$adapter.InterfaceDescription
            mac = [string]$adapter.MacAddress
            permanent_mac = [string]$adapter.PermanentAddress
            class_key = $keyPath
            settings = $settings
        }
    }
}

# A user's roaming AppData by SID: the user's own shell folder when the
# user's hive is loaded (the SongPlayer task logs on interactively), else
# the profile's AppData\Roaming.
function Get-ProdPcUserAppData {
    param([string]$Sid)
    $profileKey = "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\$Sid"
    if (-not (Test-Path -LiteralPath $profileKey)) { throw "the user $Sid has no profile on this PC" }
    $profileDir = [string](Get-Item -LiteralPath $profileKey).GetValue('ProfileImagePath')
    if ($profileDir -eq '') { throw "the user $Sid's profile has no ProfileImagePath" }
    $shellFolders = "Registry::HKEY_USERS\$Sid\Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders"
    if (Test-Path -LiteralPath $shellFolders) {
        $raw = (Get-Item -LiteralPath $shellFolders).GetValue('AppData', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if ($null -ne $raw -and "$raw" -ne '') {
            $path = "$raw" -ireplace '%USERPROFILE%', $profileDir.Replace('$', '$$')
            return [Environment]::ExpandEnvironmentVariables($path)
        }
    }
    return (Join-Path $profileDir 'AppData\Roaming')
}

# Where the SoundGrid driver hosted by SongPlayer reads its preferences:
# the %APPDATA% of the user the SongPlayer scheduled task runs as (the
# deploy's runner may be another user); with no task yet (a new PC), the
# user running this script.
function Resolve-SoundGridPreferencesPath {
    $relative = 'Waves Audio\Preferences\SoundGrid.Driver.preferences'
    $me = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $tasks = @(Get-ScheduledTask -TaskName 'SongPlayer' -ErrorAction SilentlyContinue)
    if ($tasks.Count -gt 1) { throw "$($tasks.Count) scheduled tasks are named SongPlayer: which user SongPlayer runs as is unclear" }
    if ($tasks.Count -eq 0) {
        return [pscustomobject]@{
            User = "$($me.Name) (no SongPlayer task yet: the user running this script)"
            Path = (Join-Path ([Environment]::GetFolderPath('ApplicationData')) $relative)
        }
    }
    $userId = [string]$tasks[0].Principal.UserId
    if ($userId -eq '') { throw 'the SongPlayer task names no user' }
    if ($userId -match '^S-1-') {
        $sid = $userId
    } else {
        $sid = (New-Object System.Security.Principal.NTAccount($userId)).Translate([System.Security.Principal.SecurityIdentifier]).Value
    }
    if ($sid -eq $me.User.Value) {
        $appData = [Environment]::GetFolderPath('ApplicationData')
    } else {
        $appData = Get-ProdPcUserAppData -Sid $sid
    }
    return [pscustomobject]@{ User = "$userId (the SongPlayer task's user)"; Path = (Join-Path $appData $relative) }
}

function Read-ProdPcSoundGrid {
    $installed = Test-Path -LiteralPath 'HKLM:\SYSTEM\CurrentControlSet\Services\SoundGridProtocol'
    $soundgrid = [ordered]@{
        installed = $installed; preferences_user = $null; preferences_path = $null
        preferences = $null; preferences_error = $null; adapters = @()
    }
    if (-not $installed) { return $soundgrid }
    try {
        $where = Resolve-SoundGridPreferencesPath
        $soundgrid.preferences_user = $where.User
        $soundgrid.preferences_path = $where.Path
        if (Test-Path -LiteralPath $where.Path -PathType Leaf) {
            $soundgrid.preferences = [System.IO.File]::ReadAllText($where.Path)
        }
    } catch {
        $soundgrid.preferences_error = $_.Exception.Message
    }
    $soundgrid.adapters = @(Read-ProdPcAdapters)
    return $soundgrid
}

function Read-ProdPcPower {
    $active = Invoke-ProdPcNative -File 'powercfg.exe' -Arguments @('/getactivescheme')
    $scheme = [regex]::Match($active.Text, '([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})')
    if ($active.Code -ne 0 -or -not $scheme.Success) { throw "powercfg /getactivescheme names no scheme: $($active.Text)" }
    $name = [regex]::Match($active.Text, '\(([^)]*)\)\s*$')
    $query = Invoke-ProdPcNative -File 'powercfg.exe' -Arguments @('/qh', 'SCHEME_CURRENT', 'SUB_PROCESSOR', 'CPMINCORES')
    $cores = $null
    if ($query.Code -eq 0) {
        $cores = Get-PowercfgAcIndex $query.Text
        if ($null -eq $cores) { throw "powercfg /qh gave no CPMINCORES AC index: $($query.Text)" }
    }
    return [ordered]@{
        scheme_guid = $scheme.Groups[1].Value.ToLowerInvariant()
        scheme_name = $(if ($name.Success) { $name.Groups[1].Value } else { $null })
        cpmincores_ac = $cores
    }
}

function Read-ProdPcDefender {
    $defender = [ordered]@{ service = 'missing'; realtime_enabled = $null; tamper_protected = $null; preferences = $null }
    $service = Get-Service -Name 'WinDefend' -ErrorAction SilentlyContinue
    if ($null -eq $service) { return $defender }
    $defender.service = [string]$service.Status
    if ($defender.service -ne 'Running') { return $defender }
    $status = Get-MpComputerStatus
    $defender.realtime_enabled = [bool]$status.RealTimeProtectionEnabled
    $tamper = $status.PSObject.Properties['IsTamperProtected']
    if ($null -ne $tamper) { $defender.tamper_protected = [bool]$tamper.Value }
    $mp = Get-MpPreference
    $preferences = [ordered]@{}
    foreach ($want in @(Get-ProdPcDefenderWants)) {
        $value = $mp.($want.Name)
        if ($want.Wanted -is [bool]) { $preferences[$want.Name] = [bool]$value } else { $preferences[$want.Name] = [int]$value }
    }
    $defender.preferences = $preferences
    return $defender
}

# The firewall's ACTIVE store (the local settings merged with every group
# policy): what lane 1 gates.
function Read-ProdPcFirewall {
    $firewall = [ordered]@{}
    foreach ($fwProfile in @(Get-NetFirewallProfile -PolicyStore ActiveStore)) {
        $enabled = [string]$fwProfile.Enabled
        if ($enabled -eq 'True') { $firewall[[string]$fwProfile.Name] = $true }
        elseif ($enabled -eq 'False') { $firewall[[string]$fwProfile.Name] = $false }
        else { $firewall[[string]$fwProfile.Name] = $enabled }
    }
    return $firewall
}

# Every fact the plan needs, read from this PC, in the same shape as a
# -FactsJson file (a JSON round trip: the plan always reads that shape).
function Read-ProdPcFacts {
    # Windows PowerShell 5.1's System.Array type data makes ConvertTo-Json
    # write a wrapped array as {"value": [...], "Count": n}: drop it (the
    # documented workaround; PowerShell 6+ has none).
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        Remove-TypeData -TypeName System.Array -ErrorAction SilentlyContinue
    }
    $registry = New-Object System.Collections.Generic.List[object]
    foreach ($want in @(Get-ProdPcRegistryWants)) {
        $registry.Add((Read-ProdPcRegistryValue -Path $want.Path -Name $want.Name))
    }
    $facts = [ordered]@{
        registry = $registry.ToArray()
        soundgrid = Read-ProdPcSoundGrid
        power = Read-ProdPcPower
        defender = Read-ProdPcDefender
        firewall = Read-ProdPcFirewall
    }
    $back = ConvertTo-Json -InputObject $facts -Depth 20 | ConvertFrom-Json
    # The plan reads only this shape: a list that did not come back as a
    # list would read as nothing set (or no adapter), never as an error.
    $registryBack = @(Get-ProdPcList $back 'registry')
    $adaptersRead = @($facts.soundgrid.adapters).Count
    $adaptersBack = @(Get-ProdPcList (Get-ProdPcField $back 'soundgrid') 'adapters')
    if ($registryBack.Count -ne $registry.Count -or $null -eq (Get-ProdPcField $registryBack[0] 'path') -or $adaptersBack.Count -ne $adaptersRead) {
        throw "the facts did not come back from their JSON round trip in shape ($($registryBack.Count) of $($registry.Count) registry values, $($adaptersBack.Count) of $adaptersRead adapters)"
    }
    return $back
}

function Set-ProdPcRegistryValue {
    param([string]$Path, [string]$Name, [string]$Type, $Value)
    if (-not (Test-Path -LiteralPath $Path)) { New-Item -Path $Path -Force | Out-Null }
    if ($Type -eq 'DWord') { $data = ConvertTo-ProdPcInt32 $Value } else { $data = [string]$Value }
    New-ItemProperty -LiteralPath $Path -Name $Name -PropertyType $Type -Value $data -Force | Out-Null
}

# Apply one action (an adapter restart follows all of an adapter's
# changes, once: see Invoke-ProdPcSetup).
function Invoke-ProdPcAction {
    param($Action)
    switch ($Action.Kind) {
        'registry' { Set-ProdPcRegistryValue -Path $Action.Path -Name $Action.Name -Type $Action.Type -Value $Action.Wanted }
        'nic_keyword' { Set-NetAdapterAdvancedProperty -Name $Action.Adapter -RegistryKeyword $Action.Name -RegistryValue ([string]$Action.Wanted) -NoRestart }
        'nic_pnp' { Set-ProdPcRegistryValue -Path $Action.Path -Name 'PnPCapabilities' -Type 'DWord' -Value $Action.Wanted }
        'power' {
            $set = Invoke-ProdPcNative -File 'powercfg.exe' -Arguments @('/setacvalueindex', 'SCHEME_CURRENT', 'SUB_PROCESSOR', 'CPMINCORES', [string]$Action.Wanted)
            if ($set.Code -ne 0) { throw "powercfg /setacvalueindex exited $($set.Code): $($set.Text)" }
            $activate = Invoke-ProdPcNative -File 'powercfg.exe' -Arguments @('/setactive', 'SCHEME_CURRENT')
            if ($activate.Code -ne 0) { throw "powercfg /setactive exited $($activate.Code): $($activate.Text)" }
        }
        'defender' {
            $preference = @{}
            $preference[$Action.Name] = $Action.Wanted
            Set-MpPreference @preference
        }
        'firewall' { Set-NetFirewallProfile -Profile $Action.Name -Enabled False }
        default { throw "unknown action kind '$($Action.Kind)'" }
    }
}

function Write-ProdPcReview {
    param($Actions, $Blockers, [string]$Prefix)
    foreach ($action in $Actions) {
        Write-Host ("{0} {1}  [{2}]" -f $Prefix, (Format-ProdPcAction $action), $action.Reason)
    }
    foreach ($blocker in $Blockers) {
        Write-Host ("BLOCKED {0}: {1}" -f $blocker.Code, $blocker.Text)
    }
}

# The whole run; returns the exit code.
function Invoke-ProdPcSetup {
    param([bool]$PlanOnly, [string]$FactsPath, [string]$SavePath)
    try {
        if ($FactsPath -ne '') {
            if (-not $PlanOnly) {
                Write-Host 'setup-prod-pc: -FactsJson plans from a facts file and never applies: add -Plan'
                return 2
            }
            $facts = Get-Content -Raw -LiteralPath $FactsPath | ConvertFrom-Json
            Write-Host "setup-prod-pc: the production PC profile (issue 255), planned from $FactsPath"
        } else {
            if (-not (Test-ProdPcWindows)) {
                Write-Host 'setup-prod-pc: this is no Windows PC (plan from a facts file with -Plan -FactsJson)'
                return 3
            }
            if (-not (Test-ProdPcElevated)) {
                Write-Host 'setup-prod-pc: run it elevated (as Administrator): it reads and writes machine settings'
                return 3
            }
            Write-Host "setup-prod-pc: the production PC profile (issue 255) on $env:COMPUTERNAME"
            $facts = Read-ProdPcFacts
        }
        if ($SavePath -ne '') {
            ConvertTo-Json -InputObject $facts -Depth 20 | Set-Content -LiteralPath $SavePath -Encoding UTF8
            Write-Host "setup-prod-pc: the facts are in $SavePath"
        }
        $soundgrid = Get-ProdPcField $facts 'soundgrid'
        if ($true -eq (Get-ProdPcField $soundgrid 'installed')) {
            Write-Host "setup-prod-pc: SoundGrid's preferences: $(Get-ProdPcField $soundgrid 'preferences_path') (user $(Get-ProdPcField $soundgrid 'preferences_user'))"
        }
        $review = Get-ProdPcReview -Facts $facts
        $actions = @($review.Actions)
        $blockers = @($review.Blockers)
        if ($PlanOnly) {
            Write-ProdPcReview -Actions $actions -Blockers $blockers -Prefix 'PLAN '
            if ($actions.Count -eq 0 -and $blockers.Count -eq 0) {
                Write-Host 'setup-prod-pc: nothing to change - this PC has the production profile'
            } else {
                Write-Host "setup-prod-pc: plan: $($actions.Count) change(s), $($blockers.Count) blocker(s); nothing was changed (-Plan)"
            }
            return 0
        }
        if ($actions.Count -eq 0 -and $blockers.Count -eq 0) {
            Write-Host 'setup-prod-pc: nothing to change - this PC has the production profile'
            return 0
        }
        if ($actions.Count -eq 0) {
            Write-ProdPcReview -Actions $actions -Blockers $blockers -Prefix 'SET  '
            Write-Host "setup-prod-pc: FAILED - nothing to change, but $($blockers.Count) blocker(s) the operator must resolve (see the lines above)"
            return 1
        }

        $failures = New-Object System.Collections.Generic.List[string]
        $restart = [ordered]@{}
        foreach ($action in $actions) {
            Write-Host ("SET   {0}  [{1}]" -f (Format-ProdPcAction $action), $action.Reason)
            try {
                Invoke-ProdPcAction -Action $action
                if ($action.Kind -like 'nic_*') { $restart[$action.Adapter] = $true }
            } catch {
                $failures.Add("$(Format-ProdPcAction $action): $($_.Exception.Message)")
                Write-Host "FAIL  $(Format-ProdPcAction $action): $($_.Exception.Message)"
            }
        }
        foreach ($adapterName in @($restart.Keys)) {
            Write-Host "RESTART adapter '$adapterName' (once, for its new settings)"
            try {
                Restart-NetAdapter -Name $adapterName -Confirm:$false
            } catch {
                $failures.Add("the restart of adapter '$adapterName': $($_.Exception.Message)")
                Write-Host "FAIL  the restart of adapter '$adapterName': $($_.Exception.Message)"
            }
        }

        # Re-read and verify. Windows applies some changes a moment later
        # (Defender's status after Set-MpPreference): at most 3 reads, 5 s
        # apart, while something applied still reads otherwise or a new
        # blocker shows.
        $applied = @{}
        foreach ($action in $actions) { $applied[(Get-ProdPcActionKey $action)] = $action }
        $blockedBefore = @($blockers | ForEach-Object { $_.Code })
        $left = @()
        $leftBlockers = @()
        $afterFacts = $null
        for ($read = 1; $read -le 3; $read++) {
            $afterFacts = Read-ProdPcFacts
            $after = Get-ProdPcReview -Facts $afterFacts
            $left = @($after.Actions)
            $leftBlockers = @($after.Blockers)
            $pending = @($left | Where-Object { $applied.ContainsKey((Get-ProdPcActionKey $_)) })
            $newBlockers = @($leftBlockers | Where-Object { $blockedBefore -notcontains $_.Code })
            if ($pending.Count -eq 0 -and $newBlockers.Count -eq 0) { break }
            if ($read -lt 3) {
                Write-Host 'setup-prod-pc: something still reads otherwise - reading again in 5 s'
                Start-Sleep -Seconds 5
            }
        }
        $leftByKey = @{}
        foreach ($action in $left) { $leftByKey[(Get-ProdPcActionKey $action)] = $action }
        $mismatches = 0
        foreach ($action in $actions) {
            $still = $leftByKey[(Get-ProdPcActionKey $action)]
            if ($null -eq $still) {
                Write-Host "OK    $(Format-ProdPcAction $action)"
            } else {
                $mismatches++
                Write-Host "FAIL  $($action.Area) $($action.Name): reads $($still.Current) after the change, wanted $($action.Wanted)"
            }
        }
        foreach ($action in $left) {
            if (-not $applied.ContainsKey((Get-ProdPcActionKey $action))) {
                $mismatches++
                Write-Host "FAIL  $(Format-ProdPcAction $action): it differs now, it did not before"
            }
        }
        foreach ($blocker in $leftBlockers) {
            Write-Host ("BLOCKED {0}: {1}" -f $blocker.Code, $blocker.Text)
        }
        $defenderFailed = @($left | Where-Object { $_.Area -eq 'defender' }).Count -gt 0
        $tamper = Get-ProdPcField (Get-ProdPcField $afterFacts 'defender') 'tamper_protected'
        if ($defenderFailed -and $true -eq $tamper) {
            Write-Host 'setup-prod-pc: Tamper Protection is on and keeps Defender as it is: turn it off in Windows Security (Virus & threat protection > Manage settings > Tamper Protection), then run this again'
        }
        if ($failures.Count -eq 0 -and $mismatches -eq 0 -and $leftBlockers.Count -eq 0) {
            Write-Host "setup-prod-pc: $($actions.Count) change(s) applied and verified"
            return 0
        }
        Write-Host "setup-prod-pc: FAILED - $($failures.Count) error(s), $mismatches setting(s) not as wanted, $($leftBlockers.Count) blocker(s) (see the lines above)"
        return 1
    } catch {
        Write-Host "setup-prod-pc: FAILED - $($_.Exception.Message)"
        Write-Host "$($_.ScriptStackTrace)"
        return 1
    }
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
# The run's return code is its last output (nothing else is written to the
# output stream; a stray object would only precede it).
$exitCode = [int](@(Invoke-ProdPcSetup -PlanOnly $Plan.IsPresent -FactsPath $FactsJson -SavePath $SaveFacts)[-1])
$scriptFile = Get-Variable -Name PSCommandPath -ValueOnly -ErrorAction SilentlyContinue
if ($scriptFile) { exit $exitCode }
# Run through iex or a scriptblock: never `exit` (it would close the
# operator's window); the code is in $LASTEXITCODE.
$global:LASTEXITCODE = $exitCode
if ($exitCode -ne 0) { Write-Host "setup-prod-pc: exit code $exitCode" }
