# Issue 255 lane 2: scripts/setup-prod-pc.ps1, its pure plan and its parsing.
#
# Pester 5 under pwsh 7 on Linux (the CI job "Production PC setup (Pester +
# PSScriptAnalyzer)"). The script's functions are loaded from its syntax tree
# (nothing of the script runs at load: it reads and writes machine settings),
# the plan is computed from fixtures (scripts/tests/fixtures/prod-pc: PP and
# SNV as lane 1 read them live on 13.10.2026, a fresh PC), and the script
# itself runs in -Plan -FactsJson mode, the only mode that needs no Windows.

BeforeAll {
    Set-StrictMode -Version Latest
    $script:ScriptPath = Join-Path (Split-Path -Parent $PSScriptRoot) 'setup-prod-pc.ps1'
    $script:FixtureDir = Join-Path (Join-Path $PSScriptRoot 'fixtures') 'prod-pc'
    $tokens = $null
    $parseErrors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($script:ScriptPath, [ref]$tokens, [ref]$parseErrors)
    if (@($parseErrors).Count -gt 0) { throw "setup-prod-pc.ps1 does not parse: $($parseErrors[0])" }
    $definitions = $ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $false)
    if (@($definitions).Count -lt 20) { throw "setup-prod-pc.ps1 defines only $(@($definitions).Count) functions" }
    foreach ($definition in $definitions) {
        . ([scriptblock]::Create($definition.Extent.Text))
    }

    function Read-Fixture {
        param([string]$Name)
        return (Get-Content -Raw -LiteralPath (Join-Path $script:FixtureDir "$Name.json") | ConvertFrom-Json)
    }

    # The facts through the JSON round trip every live read takes.
    function Copy-Facts {
        param($Facts)
        return (ConvertTo-Json -InputObject $Facts -Depth 20 | ConvertFrom-Json)
    }

    function Get-PlanLines {
        param($Facts)
        return @(Get-ProdPcPlan -Facts $Facts | ForEach-Object { "$($_.Area) $($_.Name)" })
    }

    # What Windows would read after the plan's changes: each action's wanted
    # value written into the facts, the way the apply writes it (the
    # registry type included; a Defender status that follows its
    # preferences).
    function Set-FactsAsApplied {
        param($Facts, $Actions)
        foreach ($action in $Actions) {
            switch ($action.Kind) {
                'registry' {
                    $entry = Find-ProdPcRegistryFact -Facts $Facts -Path $action.Path -Name $action.Name
                    if ($null -eq $entry) {
                        $entry = [pscustomobject]@{ path = $action.Path; name = $action.Name; kind = $null; value = $null }
                        $Facts.registry = @($Facts.registry) + $entry
                    }
                    $entry.kind = $action.Type
                    $entry.value = $action.Wanted
                }
                { $_ -eq 'nic_keyword' -or $_ -eq 'nic_pnp' } {
                    $adapter = @($Facts.soundgrid.adapters | Where-Object { $_.guid -eq $action.AdapterGuid })[0]
                    $adapter.settings | Add-Member -NotePropertyName $action.Name -NotePropertyValue ([string]$action.Wanted) -Force
                }
                { $_ -eq 'nic_value' -or $_ -eq 'nic_advanced' } {
                    $adapter = @($Facts.adapters | Where-Object { $_.guid -eq $action.AdapterGuid })[0]
                    $adapter.settings | Add-Member -NotePropertyName $action.Name -NotePropertyValue ([string]$action.Wanted) -Force
                }
                'power' { $Facts.power.cpmincores_ac = $action.Wanted }
                'defender' { $Facts.defender.preferences.($action.Name) = $action.Wanted }
                default { throw "no fake apply for $($action.Kind)" }
            }
        }
        $Facts.defender.realtime_enabled = $false
    }

    $script:Pwsh = (Get-Process -Id $PID).Path
}

Describe 'ConvertTo-MacKey (the MAC rule of lane 1)' {
    It 'reads every spelling of one MAC as 12 lowercase hex digits' {
        foreach ($spelling in @('6c:24:08:e5:9c:91', '6C-24-08-E5-9C-91', '6C2408E59C91', ' 6c.24.08.e5.9c.91 ')) {
            ConvertTo-MacKey $spelling | Should -BeExactly '6c2408e59c91'
        }
    }
    It 'reads anything else as no MAC' {
        foreach ($text in @('', 'Ethernet 4', '6c:24:08:e5:9c', '6c:24:08:e5:9c:91:00', '6g:24:08:e5:9c:91', '6c 24 08 e5 9c 91')) {
            ConvertTo-MacKey $text | Should -BeNullOrEmpty
        }
        ConvertTo-MacKey $null | Should -BeNullOrEmpty
    }
    It 'shows a MAC with colons' {
        Format-MacKey '6c2408e59c91' | Should -BeExactly '6c:24:08:e5:9c:91'
    }
}

Describe 'Get-SoundGridPort (the preferences of lane 1)' {
    BeforeAll {
        $script:PpPreferences = (Read-Fixture 'pp').soundgrid.preferences
    }
    It "reads PP's preferences verbatim" {
        $port = Get-SoundGridPort $script:PpPreferences
        $port.State | Should -BeExactly 'set'
        $port.Mac | Should -BeExactly '6c2408e59c91'
    }
    It 'skips a byte order mark and takes any MAC spelling' {
        $text = [string][char]0xFEFF + $script:PpPreferences.Replace('6c:24:08:e5:9c:91', ' 6C-24-08-E5-9C-91 ')
        (Get-SoundGridPort $text).Mac | Should -BeExactly '6c2408e59c91'
    }
    It 'reads no port chosen as unset' {
        $element = '<LocalLanPort>6c:24:08:e5:9c:91</LocalLanPort>'
        foreach ($port in @('', '<LocalLanPort></LocalLanPort>', '<LocalLanPort>   </LocalLanPort>', '<LocalLanPort/>', '<LocalLanPort />')) {
            (Get-SoundGridPort $script:PpPreferences.Replace($element, $port)).State | Should -BeExactly 'unset' -Because $port
        }
        # A port outside <Configuration> is no choice of the driver's.
        $outside = '<Preferences><LocalLanPort>6c:24:08:e5:9c:91</LocalLanPort><Configuration><IOMode>0</IOMode></Configuration></Preferences>'
        (Get-SoundGridPort $outside).State | Should -BeExactly 'unset'
    }
    It 'reads malformed preferences as unreadable, saying why' {
        $element = '<LocalLanPort>6c:24:08:e5:9c:91</LocalLanPort>'
        $cases = @(
            @{ Text = ''; Why = 'no XML' },
            @{ Text = '<Preferences></Preferences>'; Why = '0 <Configuration>' },
            @{ Text = $script:PpPreferences.Replace('</Configuration>', ''); Why = 'no XML' },
            @{ Text = $script:PpPreferences.Replace('</Preferences>', '<Configuration></Configuration></Preferences>'); Why = '2 <Configuration>' },
            @{ Text = $script:PpPreferences.Replace($element, "$element<LocalLanPort>x</LocalLanPort>"); Why = 'twice' },
            @{ Text = $script:PpPreferences.Replace($element, '<LocalLanPort id="1">6c:24:08:e5:9c:91</LocalLanPort>'); Why = 'attributes' },
            @{ Text = $script:PpPreferences.Replace($element, "<!-- $element -->"); Why = 'comment' },
            @{ Text = $script:PpPreferences.Replace($element, '<LocalLanPort>Ethernet 4</LocalLanPort>'); Why = 'no MAC' }
        )
        foreach ($case in $cases) {
            $port = Get-SoundGridPort $case.Text
            $port.State | Should -BeExactly 'unreadable' -Because $case.Why
            $port.Why | Should -Match ([regex]::Escape($case.Why))
        }
    }
}

Describe 'Get-PnPCapabilitiesWanted (NDIS_DEVICE_DISABLE_PM = 0x08)' {
    It 'writes 24 for a missing value, 0 or no number' {
        foreach ($value in @($null, '0', 0, 'abc', '-8', '')) {
            Get-PnPCapabilitiesWanted $value | Should -Be 24 -Because "value '$value'"
        }
    }
    It 'holds any value with the bit set' {
        foreach ($value in @('8', '24', '280', 24, ' 24 ', '4294967295')) {
            Get-PnPCapabilitiesWanted $value | Should -BeNullOrEmpty -Because "value '$value'"
        }
    }
    It 'keeps the other bits and adds 0x08' {
        Get-PnPCapabilitiesWanted '16' | Should -Be 24
        Get-PnPCapabilitiesWanted '256' | Should -Be 264
        Get-PnPCapabilitiesWanted '1' | Should -Be 9
        Get-PnPCapabilitiesWanted '4294967287' | Should -Be 4294967295
    }
}

Describe 'ConvertTo-ProdPcUInt32 and ConvertTo-ProdPcInt32 (registry DWORDs)' {
    It 'reads a DWORD the registry gave as a signed Int32 as unsigned' {
        ConvertTo-ProdPcUInt32 ([int]-1) | Should -Be 4294967295
        ConvertTo-ProdPcUInt32 ([int64]4294967295) | Should -Be 4294967295
        ConvertTo-ProdPcUInt32 '6' | Should -Be 6
        ConvertTo-ProdPcUInt32 '+6' | Should -Be 6
    }
    It 'reads anything that is no DWORD as none' {
        foreach ($value in @($null, $true, 'High', '-1', '1.5', '4294967296', '0x10')) {
            ConvertTo-ProdPcUInt32 $value | Should -BeNullOrEmpty -Because "value '$value'"
        }
        ConvertTo-ProdPcUInt32 ([int64]4294967296) | Should -BeNullOrEmpty
        ConvertTo-ProdPcUInt32 ([double]1.5) | Should -BeNullOrEmpty
    }
    It 'writes 0xFFFFFFFF as the Int32 -1 New-ItemProperty takes' {
        ConvertTo-ProdPcInt32 ([uint32]4294967295) | Should -Be -1
        ConvertTo-ProdPcInt32 6 | Should -Be 6
        { ConvertTo-ProdPcInt32 'High' } | Should -Throw
    }
}

Describe 'Get-PowercfgAcIndex' {
    It 'reads the AC index by its label' {
        $text = @(
            'Power Scheme GUID: 8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c  (High performance)',
            '  Subgroup GUID: 54533251-82be-4824-96c1-47b60b740d00  (Processor power management)',
            '    Power Setting GUID: 0cc5b647-c1df-4637-891a-dec35c318583  (Processor performance core parking min cores)',
            '      Minimum Possible Setting: 0x00000000',
            '      Maximum Possible Setting: 0x00000064',
            '      Possible Settings increment: 0x00000001',
            '      Possible Settings units: %',
            '    Current AC Power Setting Index: 0x00000032',
            '    Current DC Power Setting Index: 0x00000064'
        ) -join "`n"
        Get-PowercfgAcIndex $text | Should -Be 50
    }
    It 'reads it by position on a localized Windows (AC, then DC, last)' {
        $text = "Minimalna hodnota: 0x00000000`nMaximalna hodnota: 0x00000064`nKrok: 0x00000001`nAC: 0x00000064`nDC: 0x0000000a"
        Get-PowercfgAcIndex $text | Should -Be 100
    }
    It 'reads nothing from output with no index' {
        Get-PowercfgAcIndex 'The power scheme, subgroup or setting specified does not exist.' | Should -BeNullOrEmpty
    }
}

Describe 'Get-ProdPcPlan' {
    It "plans nothing on PP: every value is already the production one" {
        $facts = Read-Fixture 'pp'
        @(Get-ProdPcPlan -Facts $facts) | Should -HaveCount 0
        @(Get-ProdPcBlockers -Facts $facts) | Should -HaveCount 0
    }

    It "plans exactly the Windows Update driver exclusion on SNV" {
        $facts = Read-Fixture 'snv'
        $plan = @(Get-ProdPcPlan -Facts $facts)
        $plan | Should -HaveCount 1
        $plan[0].Area | Should -BeExactly 'windows_update'
        $plan[0].Name | Should -BeExactly 'ExcludeWUDriversInQualityUpdate'
        $plan[0].Current | Should -BeExactly '(none)'
        $plan[0].Wanted | Should -Be 1
        $plan[0].Kind | Should -BeExactly 'registry'
        $plan[0].Path | Should -BeExactly 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate'
        $plan[0].Type | Should -BeExactly 'DWord'
        $plan[0].Reason | Should -Not -BeNullOrEmpty
        @(Get-ProdPcBlockers -Facts $facts) | Should -HaveCount 0
    }

    It 'plans every action on a fresh PC, in order' {
        $facts = Read-Fixture 'fresh'
        Get-PlanLines $facts | Should -Be @(
            'windows_update NoAutoUpdate',
            'windows_update ExcludeWUDriversInQualityUpdate',
            'mmcss SystemResponsiveness',
            'mmcss NetworkThrottlingIndex',
            'mmcss Priority',
            'mmcss Scheduling Category',
            'soundgrid_nic *InterruptModeration',
            'soundgrid_nic *FlowControl',
            'soundgrid_nic *EEE',
            'soundgrid_nic EnableGreenEthernet',
            'soundgrid_nic PowerSavingMode',
            'soundgrid_nic GigaLite',
            'soundgrid_nic PnPCapabilities',
            'power CPMINCORES',
            'defender DisableRealtimeMonitoring',
            'defender DisableBehaviorMonitoring',
            'defender DisableIOAVProtection',
            'defender DisableScriptScanning',
            'defender MAPSReporting',
            'defender SubmitSamplesConsent',
            'defender_policy DisableRealtimeMonitoring',
            'defender_policy DisableBehaviorMonitoring',
            'defender_policy DisableOnAccessProtection',
            'defender_policy DisableScanOnRealtimeEnable',
            'defender_policy DisableIOAVProtection'
        )
        @(Get-ProdPcBlockers -Facts $facts) | Should -HaveCount 0
    }

    It "plans each fresh value's wanted one" {
        $plan = @(Get-ProdPcPlan -Facts (Read-Fixture 'fresh'))
        $byName = @{}
        foreach ($action in $plan) { $byName["$($action.Area) $($action.Name)"] = $action }
        $byName['mmcss NetworkThrottlingIndex'].Current | Should -BeExactly '10'
        $byName['mmcss NetworkThrottlingIndex'].Wanted | Should -Be 4294967295
        $byName['mmcss Scheduling Category'].Wanted | Should -BeExactly 'High'
        $byName['mmcss Scheduling Category'].Type | Should -BeExactly 'String'
        $byName['soundgrid_nic *FlowControl'].Current | Should -BeExactly '3'
        $byName['soundgrid_nic *FlowControl'].Wanted | Should -BeExactly '0'
        $byName['soundgrid_nic *FlowControl'].Adapter | Should -BeExactly 'Ethernet'
        $byName['soundgrid_nic *FlowControl'].Kind | Should -BeExactly 'nic_keyword'
        $byName['soundgrid_nic PnPCapabilities'].Current | Should -BeExactly '(none)'
        $byName['soundgrid_nic PnPCapabilities'].Wanted | Should -Be 24
        $byName['soundgrid_nic PnPCapabilities'].Path | Should -Match '\\0001$'
        $byName['power CPMINCORES'].Current | Should -Be 10
        $byName['power CPMINCORES'].Wanted | Should -Be 100
        $byName['defender DisableRealtimeMonitoring'].Wanted | Should -BeTrue
        $byName['defender MAPSReporting'].Wanted | Should -Be 0
        $byName['defender SubmitSamplesConsent'].Wanted | Should -Be 2
        $byName['defender_policy DisableOnAccessProtection'].Path | Should -BeExactly 'HKLM:\SOFTWARE\Policies\Microsoft\Windows Defender\Real-Time Protection'
    }

    It 'plans nothing once its own changes are applied (a second run plans nothing)' {
        $facts = Read-Fixture 'fresh'
        Set-FactsAsApplied -Facts $facts -Actions @(Get-ProdPcPlan -Facts $facts)
        $after = Copy-Facts $facts
        @(Get-ProdPcPlan -Facts $after) | Should -HaveCount 0
        @(Get-ProdPcBlockers -Facts $after) | Should -HaveCount 0
    }

    It 'takes a value of another registry type as different' {
        $facts = Read-Fixture 'pp'
        $entry = Find-ProdPcRegistryFact -Facts $facts -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' -Name 'NoAutoUpdate'
        $entry.kind = 'String'
        $entry.value = '1'
        $plan = @(Get-ProdPcPlan -Facts $facts)
        $plan | Should -HaveCount 1
        $plan[0].Current | Should -BeExactly '1 (String)'
    }

    It "holds NetworkThrottlingIndex read as the Int32 -1 and a padded ' high '" {
        $facts = Read-Fixture 'pp'
        (Find-ProdPcRegistryFact -Facts $facts -Path 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile' -Name 'NetworkThrottlingIndex').value = -1
        (Find-ProdPcRegistryFact -Facts $facts -Path 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile\Tasks\Pro Audio' -Name 'Scheduling Category').value = ' high '
        @(Get-ProdPcPlan -Facts $facts) | Should -HaveCount 0
    }

    It 'plans a registry value a fixture does not list as missing' {
        $facts = Read-Fixture 'pp'
        $facts.registry = @($facts.registry | Where-Object { $_.name -ne 'Priority' })
        $plan = @(Get-ProdPcPlan -Facts $facts)
        $plan | Should -HaveCount 1
        $plan[0].Name | Should -BeExactly 'Priority'
        $plan[0].Current | Should -BeExactly '(none)'
    }

    It 'plans power with no core-parking value, and never a firewall profile that is on (issue 255)' {
        $facts = Read-Fixture 'pp'
        $facts.firewall.Public = $true
        $facts.power.cpmincores_ac = $null
        Get-PlanLines $facts | Should -Be @('power CPMINCORES')
        @(Get-ProdPcPlan -Facts $facts)[0].Current | Should -BeExactly '(none)'
    }
}

Describe 'Get-ProdPcPlan: the SoundGrid adapter' {
    It 'is not looked at when SoundGrid is not installed, whatever its preferences say' {
        $facts = Read-Fixture 'fresh'
        $facts.soundgrid.installed = $false
        $facts.soundgrid.preferences = $null
        @(Get-PlanLines $facts | Where-Object { $_ -like 'soundgrid_nic *' }) | Should -HaveCount 0
        @(Get-ProdPcBlockers -Facts $facts) | Should -HaveCount 0
    }

    It 'blocks when no port is chosen (no file), with lane 1 meaning' {
        $facts = Read-Fixture 'pp'
        $facts.soundgrid.preferences = $null
        $blockers = @(Get-ProdPcBlockers -Facts $facts)
        $blockers | Should -HaveCount 1
        $blockers[0].Code | Should -BeExactly 'soundgrid_port_unset'
        $blockers[0].Text | Should -Match 'SoundGrid nema zvoleny sietovy port - nastav ho v SoundGrid Studio / ovladaci'
        @(Get-ProdPcPlan -Facts $facts) | Should -HaveCount 0
    }

    It 'blocks when the chosen port is empty' {
        $facts = Read-Fixture 'pp'
        $facts.soundgrid.preferences = $facts.soundgrid.preferences.Replace('6c:24:08:e5:9c:91', '')
        @(Get-ProdPcBlockers -Facts $facts)[0].Code | Should -BeExactly 'soundgrid_port_unset'
    }

    It "blocks when the port's MAC is on no adapter, naming it" {
        $facts = Read-Fixture 'pp'
        $facts.soundgrid.preferences = $facts.soundgrid.preferences.Replace('6c:24:08:e5:9c:91', '6c:24:08:e5:9c:92')
        $blockers = @(Get-ProdPcBlockers -Facts $facts)
        $blockers | Should -HaveCount 1
        $blockers[0].Code | Should -BeExactly 'soundgrid_port_missing'
        $blockers[0].Text | Should -Match 'sietova karta s MAC 6c:24:08:e5:9c:92, ktoru ma ovladac SoundGrid zvolenu ako port, sa nenasla'
    }

    It 'blocks when the preferences cannot be read' {
        $facts = Read-Fixture 'pp'
        $facts.soundgrid.preferences = '<Preferences>'
        @(Get-ProdPcBlockers -Facts $facts)[0].Code | Should -BeExactly 'unreadable:soundgrid'
        $facts = Read-Fixture 'pp'
        $facts.soundgrid.preferences_error = 'the user S-1-5-21-1 has no profile on this PC'
        $blockers = @(Get-ProdPcBlockers -Facts $facts)
        $blockers[0].Code | Should -BeExactly 'unreadable:soundgrid'
        $blockers[0].Text | Should -Match 'has no profile'
    }

    It 'blocks when two adapters carry the MAC' {
        $facts = Read-Fixture 'pp'
        $facts.soundgrid.adapters[1].mac = '6C-24-08-E5-9C-91'
        @(Get-ProdPcBlockers -Facts $facts)[0].Code | Should -BeExactly 'unreadable:soundgrid'
    }

    It "finds the adapter by its permanent address when no current one matches" {
        $facts = Read-Fixture 'pp'
        $facts.soundgrid.adapters[0].mac = '02-00-00-00-00-01'
        $facts.soundgrid.adapters[0].settings.'*EEE' = '1'
        $plan = @(Get-ProdPcPlan -Facts $facts)
        $plan | Should -HaveCount 1
        $plan[0].Adapter | Should -BeExactly 'soundgrid-pcie'
        $plan[0].Name | Should -BeExactly '*EEE'
    }

    It "looks only at SoundGrid's own adapter (the Dante adapter's EEE is not its business)" {
        $facts = Read-Fixture 'pp'
        $facts.soundgrid.adapters[1].settings.'*EEE' | Should -BeExactly '1'
        @(Get-ProdPcPlan -Facts $facts) | Should -HaveCount 0
    }

    It 'blocks when the adapter has no instance in the network class' {
        $facts = Read-Fixture 'pp'
        $facts.soundgrid.adapters[0].class_key = $null
        $facts.soundgrid.adapters[0].settings = $null
        @(Get-ProdPcBlockers -Facts $facts)[0].Code | Should -BeExactly 'unreadable:soundgrid_nic'
    }

    It 'blocks on a required setting the driver lacks, and skips an optional one' {
        $facts = Read-Fixture 'pp'
        $settings = $facts.soundgrid.adapters[0].settings
        $settings.PSObject.Properties.Remove('*FlowControl')
        $settings.PSObject.Properties.Remove('GigaLite')
        $blockers = @(Get-ProdPcBlockers -Facts $facts)
        $blockers | Should -HaveCount 1
        $blockers[0].Code | Should -BeExactly 'soundgrid_nic_setting:flow_control'
        $blockers[0].Text | Should -Match '\*FlowControl chyba'
        @(Get-ProdPcPlan -Facts $facts) | Should -HaveCount 0
    }

    It "takes a setting's first name that exists, and a value that is no number as wrong" {
        $facts = Read-Fixture 'pp'
        $settings = $facts.soundgrid.adapters[0].settings
        $settings.PSObject.Properties.Remove('EnableGreenEthernet')
        $settings | Add-Member -NotePropertyName 'GreenEthernet' -NotePropertyValue 'on'
        $settings.PSObject.Properties.Remove('PowerSavingMode')
        $settings | Add-Member -NotePropertyName '*PowerSavingMode' -NotePropertyValue ' 0 '
        $plan = @(Get-ProdPcPlan -Facts $facts)
        $plan | Should -HaveCount 1
        $plan[0].Name | Should -BeExactly 'GreenEthernet'
        $plan[0].Current | Should -BeExactly 'on'
        $plan[0].Wanted | Should -BeExactly '0'
    }

    It 'ORs the power-management bit into another PnPCapabilities' {
        $facts = Read-Fixture 'pp'
        $facts.soundgrid.adapters[0].settings.PnPCapabilities = '256'
        $plan = @(Get-ProdPcPlan -Facts $facts)
        $plan | Should -HaveCount 1
        $plan[0].Kind | Should -BeExactly 'nic_pnp'
        $plan[0].Current | Should -BeExactly '256'
        $plan[0].Wanted | Should -Be 264
    }
}

Describe 'Get-ProdPcPlan: every wired adapter (issue 285, the owner''s rule)' {
    BeforeAll {
        function New-Adapter {
            param([string]$Name, [string]$Guid, [string]$Media = '802.3', [string]$Status = 'Up', $Settings = $null, [string]$PnpDeviceId = 'USB\VID_0BDA&PID_8153\000001')
            if ($null -eq $Settings) { $Settings = [pscustomobject]@{} }
            return [pscustomobject]@{
                name = $Name; guid = $Guid; description = $Name; mac = ''; permanent_mac = ''; media = $Media; status = $Status
                pnp_device_id = $PnpDeviceId
                class_key = "HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e972-e325-11ce-bfc1-08002be10318}\$Guid"
                settings = $Settings
            }
        }
        function Get-NicActions {
            param($Facts)
            return @(Get-ProdPcPlan -Facts $Facts | Where-Object { $_.Area -eq 'nic' })
        }
        $script:Always = @('*FlowControl', '*EEE', 'EEE', 'EnableGreenEthernet', 'AdvancedEEE', 'EnableExtraPowerSaving', 'GigaLite', '*SelectiveSuspend')
        # resolume-snv's 'Ethernet 4' (PnPDeviceID as read on 10.11.2026).
        $script:Rtl8156 = 'USB\VID_0BDA&PID_8156\4F13005C8C'
        function Get-Rtl8156Actions {
            param($Facts)
            return @(Get-NicActions $Facts | Where-Object { $_.Kind -eq 'nic_advanced' })
        }
    }

    It 'knows an RTL8156 by its hardware id' {
        foreach ($case in @(
                @($script:Rtl8156, $true),
                @('usb\vid_0bda&pid_8156\4f13005c8c', $true),
                @(' USB\VID_0BDA&PID_8156 ', $true),
                @('USB\VID_0BDA&PID_8156&MI_00\7&1', $true),
                @('USB\VID_0BDA&PID_81560\1', $false),
                @('USB\VID_0BDA&PID_8153\000001', $false),
                @('PCI\VEN_10EC&DEV_8125&SUBSYS_0', $false),
                @('XUSB\VID_0BDA&PID_8156\1', $false),
                @('', $false))) {
            Test-ProdPcRtl8156 $case[0] | Should -Be $case[1] -Because $case[0]
        }
    }

    It 'sets an RTL8156 to 8 transmit URBs and 64 transmit buffers through its driver' {
        $settings = [pscustomobject]@{ PendingTransmits = '3'; TransmitBufferLen = '18' }
        $facts = Read-Fixture 'fresh'
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @(New-Adapter 'Ethernet 4' '0014' -Settings $settings -PnpDeviceId $script:Rtl8156) -Force
        $facts | Add-Member -NotePropertyName dante_leg -NotePropertyValue $true -Force
        $own = Get-Rtl8156Actions $facts
        @($own | ForEach-Object { "$($_.Name) $($_.Current) $($_.Wanted)" }) | Should -Be @('PendingTransmits 3 8', 'TransmitBufferLen 18 64')
        $own[0].Adapter | Should -BeExactly 'Ethernet 4'
        $own[0].AdapterGuid | Should -BeExactly '0014'
        # after every other value of the adapter, its restart covers both
        @(Get-NicActions $facts)[-1].Name | Should -BeExactly 'TransmitBufferLen'
    }

    It 'leaves an RTL8156 already at 8 and 64 alone, and sets one that is missing' {
        $settings = [pscustomobject]@{ PendingTransmits = '8'; TransmitBufferLen = '64' }
        $facts = Read-Fixture 'fresh'
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @(New-Adapter 'Ethernet 4' '0014' -Settings $settings -PnpDeviceId $script:Rtl8156) -Force
        $facts | Add-Member -NotePropertyName dante_leg -NotePropertyValue $true -Force
        Get-Rtl8156Actions $facts | Should -HaveCount 0
        $settings.PSObject.Properties.Remove('TransmitBufferLen')
        $settings.PendingTransmits = '128'
        @(Get-Rtl8156Actions $facts | ForEach-Object { "$($_.Name) $($_.Current) $($_.Wanted)" }) | Should -Be @('PendingTransmits 128 8', 'TransmitBufferLen (none) 64')
    }

    It 'never sets the RTL8156 values on another adapter' {
        $settings = [pscustomobject]@{ PendingTransmits = '3'; TransmitBufferLen = '18' }
        $facts = Read-Fixture 'fresh'
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @(
            (New-Adapter 'Dante' '0014' -Settings $settings),
            (New-Adapter 'NoId' '0015' -Settings $settings -PnpDeviceId '')
        ) -Force
        $facts | Add-Member -NotePropertyName dante_leg -NotePropertyValue $true -Force
        Get-Rtl8156Actions $facts | Should -HaveCount 0
        @(Get-NicActions $facts | Where-Object { $_.Name -in @('PendingTransmits', 'TransmitBufferLen') }) | Should -HaveCount 0
    }

    It 'sets the RTL8156 values on an adapter whose PnPCapabilities the SoundGrid review sets' {
        $facts = Read-Fixture 'pp'
        $sg = $facts.soundgrid.adapters[0]
        $sg.settings.PSObject.Properties.Remove('PnPCapabilities')
        $copy = $sg | ConvertTo-Json -Depth 5 | ConvertFrom-Json
        $copy | Add-Member -NotePropertyName media -NotePropertyValue '802.3' -Force
        $copy | Add-Member -NotePropertyName status -NotePropertyValue 'Up' -Force
        $copy | Add-Member -NotePropertyName pnp_device_id -NotePropertyValue $script:Rtl8156 -Force
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @($copy) -Force
        $facts | Add-Member -NotePropertyName dante_leg -NotePropertyValue $true -Force
        @(Get-ProdPcPlan -Facts $facts | Where-Object { $_.Name -eq 'PnPCapabilities' }) | Should -HaveCount 1
        @(Get-Rtl8156Actions $facts | ForEach-Object { $_.Name }) | Should -Be @('PendingTransmits', 'TransmitBufferLen')
    }

    It 'plans no RTL8156 value on the second run' {
        $settings = [pscustomobject]@{ PendingTransmits = '3' }
        $facts = Read-Fixture 'fresh'
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @(New-Adapter 'Ethernet 4' '0014' -Settings $settings -PnpDeviceId $script:Rtl8156) -Force
        $facts | Add-Member -NotePropertyName dante_leg -NotePropertyValue $true -Force
        $facts = Copy-Facts $facts
        $own = Get-Rtl8156Actions $facts
        $own | Should -HaveCount 2
        Set-FactsAsApplied -Facts $facts -Actions $own
        Get-Rtl8156Actions (Copy-Facts $facts) | Should -HaveCount 0
    }

    It 'knows a Dante leg only by the facts'' dante_leg true' {
        foreach ($case in @(@($true, $true), @($false, $false), @('true', $false), @($null, $false))) {
            $facts = [pscustomobject]@{ dante_leg = $case[0] }
            Test-ProdPcDanteLeg $facts | Should -Be $case[1] -Because "$($case[0])"
        }
        Test-ProdPcDanteLeg ([pscustomobject]@{}) | Should -BeFalse
    }

    # PP (issue 255, deploy run 42981086181, 11.11.2026): its 'Ethernet 4' is an
    # RTL8156 on driver 11.4.211.2022, which offers neither keyword, and PP
    # has no Dante leg: the values are dstack's Dante-TX tuning, so nothing.
    It 'leaves an RTL8156 alone on a PC with no Dante leg (PP: its driver offers neither keyword)' {
        $facts = Read-Fixture 'pp'
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @(New-Adapter 'Ethernet 4' '0014' -PnpDeviceId 'USB\VID_0BDA&PID_8156\4F1300131B') -Force
        $facts.adapters[0] | Add-Member -NotePropertyName driver_keywords -NotePropertyValue @('*FlowControl', 'PendingReceives', 'ReceiveBufferLen') -Force
        foreach ($read in @($facts, (Copy-Facts $facts))) {
            Get-Rtl8156Actions $read | Should -HaveCount 0
            @(Get-NicActions $read | Where-Object { $_.Name -in @('PendingTransmits', 'TransmitBufferLen') }) | Should -HaveCount 0
            @(Get-ProdPcBlockers -Facts $read | Where-Object { $_.Code -like 'wired_nic*' }) | Should -HaveCount 0
        }
        $facts | Add-Member -NotePropertyName dante_leg -NotePropertyValue $false -Force
        Get-Rtl8156Actions $facts | Should -HaveCount 0
    }

    It 'sets only what the driver offers on the adapter carrying the Dante leg, and passes a keyword it lacks' {
        $facts = Read-Fixture 'fresh'
        $facts | Add-Member -NotePropertyName dante_leg -NotePropertyValue $true -Force
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @(New-Adapter 'Ethernet 4' '0014' -PnpDeviceId $script:Rtl8156) -Force
        $facts.adapters[0] | Add-Member -NotePropertyName driver_keywords -NotePropertyValue @('pendingtransmits', 'PendingReceives') -Force
        foreach ($read in @($facts, (Copy-Facts $facts))) {
            @(Get-Rtl8156Actions $read | ForEach-Object { "$($_.Name) $($_.Current) $($_.Wanted)" }) | Should -Be @('PendingTransmits (none) 8')
            @(Get-ProdPcBlockers -Facts $read | Where-Object { $_.Code -like 'wired_nic*' }) | Should -HaveCount 0
        }
        # A value the registry holds is set through the driver as before.
        $facts.adapters[0].settings | Add-Member -NotePropertyName TransmitBufferLen -NotePropertyValue '18' -Force
        @(Get-Rtl8156Actions $facts | ForEach-Object { $_.Name }) | Should -Be @('PendingTransmits', 'TransmitBufferLen')
    }

    It 'sets the transmit values only on the adapter carrying the Dante leg' {
        $settings = [pscustomobject]@{ PendingTransmits = '3'; TransmitBufferLen = '18' }
        $facts = Read-Fixture 'fresh'
        $facts | Add-Member -NotePropertyName dante_leg -NotePropertyValue $true -Force
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @(
            (New-Adapter 'Ethernet 4' '0014' -Settings $settings -PnpDeviceId $script:Rtl8156),
            (New-Adapter 'Ethernet 5' '0015' -Status 'Disconnected' -Settings ($settings | ConvertTo-Json | ConvertFrom-Json) -PnpDeviceId 'USB\VID_0BDA&PID_8156\2')
        ) -Force
        Get-ProdPcDanteCarrier $facts | Should -BeExactly '0014'
        @(Get-Rtl8156Actions $facts | ForEach-Object { "$($_.AdapterGuid) $($_.Name)" }) | Should -Be @('0014 PendingTransmits', '0014 TransmitBufferLen')
        # Two adapters up: which one carries the leg is not known, none is set.
        $facts.adapters[1].status = 'Up'
        Get-ProdPcDanteCarrier $facts | Should -BeNullOrEmpty
        Get-Rtl8156Actions $facts | Should -HaveCount 0
        # The leg's own adapter (a native NIC named Dante70) carries it.
        $facts.adapters[1].name = 'Dante70'
        Get-ProdPcDanteCarrier $facts | Should -BeExactly '0015'
        @(Get-Rtl8156Actions $facts | ForEach-Object { $_.AdapterGuid }) | Should -Be @('0015', '0015')
        # No leg: none.
        $facts.dante_leg = $false
        Get-ProdPcDanteCarrier $facts | Should -BeNullOrEmpty
    }

    # PP as read on 11.11.2026 (issue 255, deploy run 42981086181): no Dante
    # output and no Dante70, soundgrid-pcie (Realtek PCIe, rt640x64)
    # Disconnected, 'Ethernet 4' an RTL8156 on driver 11.4.211.2022 without
    # PendingTransmits / TransmitBufferLen, Up (its LAN, the default route),
    # Wi-Fi Disconnected, every firewall profile off. Dante is only one audio
    # path (the owner): nothing of Dante, the VLAN or the two networks acts.
    It 'plans nothing and blocks nothing on PP (no Dante leg)' {
        $facts = Read-Fixture 'pp'
        $zero = [ordered]@{}
        foreach ($k in $script:Always) { $zero[$k] = '0' }
        $zero['PnPCapabilities'] = '24'
        $sg = $facts.soundgrid.adapters[0] | ConvertTo-Json -Depth 5 | ConvertFrom-Json
        $sg | Add-Member -NotePropertyName media -NotePropertyValue '802.3' -Force
        $sg | Add-Member -NotePropertyName status -NotePropertyValue 'Disconnected' -Force
        $sg | Add-Member -NotePropertyName pnp_device_id -NotePropertyValue 'PCI\VEN_10EC&DEV_8168&SUBSYS_0' -Force
        foreach ($k in $script:Always) { $sg.settings | Add-Member -NotePropertyName $k -NotePropertyValue '0' -Force }
        $usb = New-Adapter 'Ethernet 4' '{0B5E0000-0000-4000-8000-000000000002}' -Settings ([pscustomobject]$zero) -PnpDeviceId 'USB\VID_0BDA&PID_8156\4F1300131B'
        $usb | Add-Member -NotePropertyName driver_keywords -NotePropertyValue @('*FlowControl', '*EEE', 'PendingReceives', 'ReceiveBufferLen') -Force
        $wifi = New-Adapter 'Wi-Fi' '{F1000000-0000-4000-8000-000000000003}' -Media 'Native 802.11' -Status 'Disconnected'
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @($sg, $usb, $wifi) -Force
        $facts | Add-Member -NotePropertyName dante_leg -NotePropertyValue $false -Force
        foreach ($read in @($facts, (Copy-Facts $facts))) {
            Get-ProdPcDanteCarrier $read | Should -BeNullOrEmpty
            @(Get-ProdPcPlan -Facts $read) | Should -HaveCount 0
            @(Get-ProdPcBlockers -Facts $read) | Should -HaveCount 0
        }
        # Even with the driver's keyword list unread, nothing of Dante acts.
        $usb.PSObject.Properties.Remove('driver_keywords')
        @(Get-ProdPcPlan -Facts $facts) | Should -HaveCount 0
    }

    It 'reads a Dante leg only from a Dante70 that is there and on' {
        foreach ($case in @(
                @('Up', $true), @('Disconnected', $true), @('Not Present', $false), @('Disabled', $false))) {
            $script:Status = $case[0]
            function Get-NetAdapter {
                param([switch]$IncludeHidden)
                [pscustomobject]@{ Name = 'Ethernet 4'; Status = 'Up' }
                [pscustomobject]@{ Name = 'Dante70'; Status = $script:Status }
            }
            Read-ProdPcDanteLeg | Should -Be $case[1] -Because $case[0]
        }
        function Get-NetAdapter { param([switch]$IncludeHidden) [pscustomobject]@{ Name = 'Ethernet 4'; Status = 'Up' } }
        Read-ProdPcDanteLeg | Should -BeFalse
    }

    It 'applies an RTL8156 value through the driver, never around it' {
        $script:Set = @()
        function Set-NetAdapterAdvancedProperty {
            param([string]$Name, [string]$RegistryKeyword, [string]$RegistryValue, [switch]$NoRestart)
            $script:Set += "$Name|$RegistryKeyword|$RegistryValue|$NoRestart"
        }
        $action = New-ProdPcAction -Area 'nic' -Name 'PendingTransmits' -Current '3' -Wanted '8' -Reason 'r' -Kind 'nic_advanced' -Target @{ Adapter = 'Ethernet 4'; AdapterGuid = '0014'; Path = 'HKLM:\x'; Type = 'String' }
        Invoke-ProdPcAction -Action $action
        $script:Set | Should -Be @('Ethernet 4|PendingTransmits|8|True')
    }

    It 'turns off the values a USB driver hides, on a PC with no SoundGrid, and stops Windows turning it off' {
        $facts = Read-Fixture 'fresh'
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @(New-Adapter 'Dante' '0014') -Force
        $actions = Get-NicActions $facts
        $values = @($actions | Where-Object { $_.Kind -eq 'nic_value' })
        @($values | ForEach-Object { $_.Name }) | Should -Be $script:Always
        @($values | Where-Object { $_.Wanted -ne '0' }) | Should -HaveCount 0
        $values[0].Adapter | Should -BeExactly 'Dante'
        $values[0].Path | Should -BeLike '*\0014'
        @($actions | Where-Object { $_.Kind -eq 'nic_pnp' -and $_.Name -eq 'PnPCapabilities' -and $_.Wanted -eq 24 }) | Should -HaveCount 1
    }

    It 'sets a value that is on, and leaves one already off' {
        $settings = [pscustomobject]@{ '*EEE' = '1'; '*FlowControl' = '0'; PnPCapabilities = '24' }
        $facts = Read-Fixture 'fresh'
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @(New-Adapter 'Dante' '0014' -Settings $settings) -Force
        $actions = Get-NicActions $facts
        @($actions | Where-Object { $_.Name -eq '*EEE' })[0].Current | Should -BeExactly '1'
        @($actions | Where-Object { $_.Name -eq '*FlowControl' }) | Should -HaveCount 0
        @($actions | Where-Object { $_.Name -eq 'PnPCapabilities' }) | Should -HaveCount 0
    }

    It 'turns off the where-present values only where the driver has them' {
        $settings = [pscustomobject]@{ '*InterruptModeration' = '1' }
        $facts = Read-Fixture 'fresh'
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @(
            (New-Adapter 'Ethernet' '0001' -Settings $settings),
            (New-Adapter 'Dante' '0014')
        ) -Force
        $moderation = @(Get-NicActions $facts | Where-Object { $_.Name -eq '*InterruptModeration' })
        $moderation | Should -HaveCount 1
        $moderation[0].Adapter | Should -BeExactly 'Ethernet'
        @(Get-NicActions $facts | Where-Object { $_.Name -in @('GreenEthernet', 'PowerSavingMode', '*PowerSavingMode') }) | Should -HaveCount 0
    }

    It 'leaves a radio and a disabled adapter alone' {
        $facts = Read-Fixture 'fresh'
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @(
            (New-Adapter 'Wi-Fi' '0002' -Media 'Native 802.11'),
            (New-Adapter 'Off' '0003' -Status 'Disabled')
        ) -Force
        Get-NicActions $facts | Should -HaveCount 0
    }

    It 'blocks on a wired adapter with no key in the network class' {
        $adapter = New-Adapter 'Dante' '0014'
        $adapter.class_key = $null
        $facts = Read-Fixture 'fresh'
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @($adapter) -Force
        @(Get-ProdPcBlockers -Facts $facts | ForEach-Object { $_.Code }) | Should -Contain 'unreadable:wired_nic'
    }

    It 'leaves an adapter that is not present alone (an unplugged USB adapter)' {
        $facts = Read-Fixture 'fresh'
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @(New-Adapter 'Dante' '0014' -Status 'Not Present') -Force
        Get-NicActions $facts | Should -HaveCount 0
    }

    It 'never writes a required SoundGrid setting the driver lacks: it stays a blocker' {
        $facts = Read-Fixture 'pp'
        $sg = $facts.soundgrid.adapters[0]
        $sg.settings.PSObject.Properties.Remove('*FlowControl')
        $copy = [pscustomobject]@{}
        foreach ($p in $sg.settings.PSObject.Properties) { $copy | Add-Member -NotePropertyName $p.Name -NotePropertyValue $p.Value }
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @(
            [pscustomobject]@{ name = $sg.name; guid = $sg.guid; description = $sg.name; mac = ''; permanent_mac = ''
                media = '802.3'; status = 'Up'; class_key = $sg.class_key; settings = $copy }
        ) -Force
        @(Get-ProdPcBlockers -Facts $facts | ForEach-Object { $_.Code }) | Should -Contain 'soundgrid_nic_setting:flow_control'
        @(Get-NicActions $facts | Where-Object { $_.Name -eq '*FlowControl' }) | Should -HaveCount 0
    }

    It 'plans nothing on the second run' {
        $facts = Read-Fixture 'fresh'
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @(New-Adapter 'Dante' '0014') -Force
        $facts = Copy-Facts $facts
        foreach ($action in @(Get-NicActions $facts)) {
            $adapter = @($facts.adapters | Where-Object { $_.guid -eq $action.AdapterGuid })[0]
            $adapter.settings | Add-Member -NotePropertyName $action.Name -NotePropertyValue ([string]$action.Wanted) -Force
        }
        Get-NicActions $facts | Should -HaveCount 0
    }

    It 'does not act twice on what the SoundGrid review already acts on' {
        $facts = Read-Fixture 'pp'
        $port = @($facts.soundgrid.adapters | Where-Object { $_.mac -match '6[Cc].24.08.[Ee]5.9[Cc].91' })[0]
        $copy = $port | ConvertTo-Json -Depth 5 | ConvertFrom-Json
        $copy | Add-Member -NotePropertyName media -NotePropertyValue '802.3' -Force
        $copy | Add-Member -NotePropertyName status -NotePropertyValue 'Up' -Force
        $copy.settings | Add-Member -NotePropertyName '*EEE' -NotePropertyValue '1' -Force
        $port.settings | Add-Member -NotePropertyName '*EEE' -NotePropertyValue '1' -Force
        $facts | Add-Member -NotePropertyName adapters -NotePropertyValue @($copy) -Force
        $eee = @(Get-ProdPcPlan -Facts $facts | Where-Object { $_.Name -eq '*EEE' })
        $eee | Should -HaveCount 1
        $eee[0].Area | Should -BeExactly 'soundgrid_nic'
    }
}

Describe 'Get-ProdPcPlan: Defender' {
    It "leaves Defender's preferences alone when its service is stopped or missing, its policy keys not" {
        foreach ($service in @('Stopped', 'missing')) {
            $facts = Read-Fixture 'fresh'
            $facts.defender.service = $service
            $lines = @(Get-PlanLines $facts)
            @($lines | Where-Object { $_ -like 'defender *' }) | Should -HaveCount 0 -Because $service
            @($lines | Where-Object { $_ -like 'defender_policy *' }) | Should -HaveCount 5 -Because $service
            @(Get-ProdPcBlockers -Facts $facts) | Should -HaveCount 0
        }
    }

    It 'blocks on a WinDefend service that is neither running nor stopped' {
        $facts = Read-Fixture 'pp'
        $facts.defender.service = 'StartPending'
        @(Get-ProdPcBlockers -Facts $facts)[0].Code | Should -BeExactly 'unreadable:defender'
    }

    It 'blocks when real-time protection stays on with every setting off, naming Tamper Protection' {
        $facts = Read-Fixture 'pp'
        $facts.defender.realtime_enabled = $true
        $facts.defender.tamper_protected = $true
        $blockers = @(Get-ProdPcBlockers -Facts $facts)
        $blockers | Should -HaveCount 1
        $blockers[0].Code | Should -BeExactly 'defender_realtime_on'
        $blockers[0].Text | Should -Match 'Tamper Protection is on'
        $facts.defender.tamper_protected = $false
        @(Get-ProdPcBlockers -Facts $facts)[0].Text | Should -Not -Match 'Tamper'
    }

    It 'does not block real-time protection while a Defender change is still planned' {
        $facts = Read-Fixture 'fresh'
        $facts.defender.realtime_enabled | Should -BeTrue
        @(Get-ProdPcBlockers -Facts $facts) | Should -HaveCount 0
    }

    It 'takes a preference of the wrong type as different' {
        $facts = Read-Fixture 'pp'
        $facts.defender.preferences.DisableScriptScanning = 1
        $facts.defender.preferences.SubmitSamplesConsent = 0
        Get-PlanLines $facts | Should -Be @('defender DisableScriptScanning', 'defender SubmitSamplesConsent')
    }
}

Describe 'Get-ProdPcBlockers: the firewall (issue 255, dstack#155)' {
    BeforeAll {
        # The facts' blocker codes.
        function Get-BlockerCodes {
            param($Facts)
            return @(Get-ProdPcBlockers -Facts $Facts | ForEach-Object { $_.Code })
        }
        # SNV with a firewall that is on and follows nothing of the standard
        # (Windows' own default actions, no group rule).
        function Get-PlainOnFacts {
            $facts = Read-Fixture 'snv'
            foreach ($p in @('Domain', 'Private', 'Public')) {
                $facts.firewall_standard.defaults.$p.inbound = 'Block'
            }
            $facts.firewall_standard.block_rules = @()
            $facts.firewall_standard.group_rules = @()
            return $facts
        }
    }

    It 'never plans a firewall action, whatever the state' {
        $mixed = Read-Fixture 'snv'
        $mixed.firewall.Public = $false
        foreach ($facts in @((Read-Fixture 'pp'), (Read-Fixture 'snv'), (Read-Fixture 'fresh'), (Get-PlainOnFacts), $mixed)) {
            @(Get-PlanLines $facts | Where-Object { $_ -like 'firewall *' }) | Should -HaveCount 0
        }
    }

    It "holds dstack's standard as SNV read it on 10.11.2026" {
        Get-BlockerCodes (Read-Fixture 'snv') | Should -HaveCount 0
    }

    It 'holds every profile off whatever its rules (PP; no firewall_standard needed)' {
        Get-BlockerCodes (Read-Fixture 'pp') | Should -HaveCount 0
        $facts = Read-Fixture 'snv'
        foreach ($p in @('Domain', 'Private', 'Public')) { $facts.firewall.$p = $false }
        $facts.firewall_standard.group_rules = @()
        Get-BlockerCodes $facts | Should -HaveCount 0
    }

    It 'blocks a firewall that is on and follows nothing of the standard, never turning it off' {
        $facts = Get-PlainOnFacts
        Get-BlockerCodes $facts | Should -Be @('firewall_on:domain', 'firewall_on:private', 'firewall_on:public')
        @(Get-ProdPcBlockers -Facts $facts)[0].Text | Should -Match 'never turns a profile off'
        $facts.firewall.Domain = $false
        $facts.firewall.Private = $false
        # An OFF profile's Allow defaults are no attempt at the standard.
        $facts.firewall_standard.defaults.Domain.inbound = 'Allow'
        Get-BlockerCodes $facts | Should -Be @('firewall_on:public')
    }

    It 'blocks a profile that is on when its rules were not read' {
        $facts = Read-Fixture 'pp'
        $facts.firewall.Private = $true
        Get-BlockerCodes $facts | Should -Be @('unreadable:firewall_rules')
    }

    It 'blocks an unread profile and judges nothing else' {
        $facts = Read-Fixture 'snv'
        $facts.firewall.Private = 'NotConfigured'
        $facts.firewall_standard.group_rules = @()
        $blockers = @(Get-ProdPcBlockers -Facts $facts)
        @($blockers | ForEach-Object { $_.Code }) | Should -Be @('unreadable:firewall:private')
        $blockers[0].Text | Should -Match 'reads NotConfigured'
    }

    It "blocks a profile that is off while dstack's standard is set up" {
        $facts = Read-Fixture 'snv'
        $facts.firewall.Public = $false
        Get-BlockerCodes $facts | Should -Be @('firewall_off:public')
    }

    It 'blocks a default action that is not Allow, naming both' {
        $facts = Read-Fixture 'snv'
        $facts.firewall_standard.defaults.Private.outbound = 'Block'
        $blockers = @(Get-ProdPcBlockers -Facts $facts)
        @($blockers | ForEach-Object { $_.Code }) | Should -Be @('firewall_default_block:private')
        $blockers[0].Text | Should -Match 'inbound Allow, outbound Block'
        # Group rules alone are an attempt: Windows' defaults then block.
        $facts = Read-Fixture 'snv'
        foreach ($p in @('Domain', 'Private', 'Public')) { $facts.firewall_standard.defaults.$p.inbound = 'Block' }
        Get-BlockerCodes $facts | Should -Be @('firewall_default_block:domain', 'firewall_default_block:private', 'firewall_default_block:public')
    }

    It 'blocks an enabled Block rule outside the group, by its display name else its name' {
        $facts = Read-Fixture 'snv'
        $facts.firewall_standard.block_rules = @($facts.firewall_standard.block_rules) + @(
            [pscustomobject]@{ name = '{1}'; display_name = 'Block X'; group = ''; display_group = '' },
            [pscustomobject]@{ name = '{2}'; display_name = ''; group = 'Other'; display_group = 'Other' },
            [pscustomobject]@{ name = '{3}'; display_name = 'by display group'; group = ''; display_group = 'dstack155' }
        )
        $blockers = @(Get-ProdPcBlockers -Facts $facts)
        @($blockers | ForEach-Object { $_.Code }) | Should -Be @('firewall_block_outside_group')
        $blockers[0].Text | Should -Match 'dstack155: Block X, \{2\}$'
    }

    It 'blocks a group that does not hold exactly 16 enabled Block rules' {
        $facts = Read-Fixture 'snv'
        $facts.firewall_standard.group_rules = @($facts.firewall_standard.group_rules | Select-Object -First 15)
        Get-BlockerCodes $facts | Should -Be @('firewall_group_incomplete')
        $facts = Read-Fixture 'snv'
        @($facts.firewall_standard.group_rules)[3].enabled = $false
        Get-BlockerCodes $facts | Should -Be @('firewall_group_incomplete')
        $facts = Read-Fixture 'snv'
        @($facts.firewall_standard.group_rules)[5].action = 'Allow'
        $blockers = @(Get-ProdPcBlockers -Facts $facts)
        @($blockers | ForEach-Object { $_.Code }) | Should -Be @('firewall_group_incomplete')
        $blockers[0].Text | Should -Match 'holds 16 rule\(s\), 15 of them enabled Block'
        # Group rules but none left and default Allow: still the standard, incomplete.
        $facts = Read-Fixture 'snv'
        $facts.firewall_standard.group_rules = @()
        $facts.firewall_standard.block_rules = @()
        Get-BlockerCodes $facts | Should -Be @('firewall_group_incomplete')
    }

    It 'blocks a group rule not bound to the Dante leg alone' {
        $facts = Read-Fixture 'snv'
        $rules = @($facts.firewall_standard.group_rules)
        $rules[0].interface_alias = @('Any')
        $rules[1].interface_alias = @('Dante70', 'Ethernet 4')
        $rules[2].interface_alias = @()
        $blockers = @(Get-ProdPcBlockers -Facts $facts)
        @($blockers | ForEach-Object { $_.Code }) | Should -Be @('firewall_group_interface')
        $blockers[0].Text | Should -Match "\(Dante70\) alone: $([regex]::Escape($rules[0].name)), $([regex]::Escape($rules[1].name)), $([regex]::Escape($rules[2].name))$"
    }

    It 'blocks a group rule that names SongPlayer.exe' {
        $facts = Read-Fixture 'snv'
        $rules = @($facts.firewall_standard.group_rules)
        $rules[4].programs = @('C:\Program Files\SongPlayer\songplayer.exe')
        $rules[6].programs = @('SongPlayer.exe.bak')
        $blockers = @(Get-ProdPcBlockers -Facts $facts)
        @($blockers | ForEach-Object { $_.Code }) | Should -Be @('firewall_group_songplayer')
        $blockers[0].Text | Should -Match "Dante device\): $([regex]::Escape($rules[4].name))$"
    }
}

Describe 'setup-prod-pc.ps1 itself' {
    It 'plans from a facts file and changes nothing (-Plan -FactsJson), exit 0' {
        $output = & $script:Pwsh -NoProfile -NonInteractive -File $script:ScriptPath -Plan -FactsJson (Join-Path $script:FixtureDir 'snv.json') 2>&1
        $LASTEXITCODE | Should -Be 0
        $text = ($output | ForEach-Object { "$_" }) -join "`n"
        $text | Should -Match 'PLAN  windows_update ExcludeWUDriversInQualityUpdate: \(none\) -> 1  \['
        $text | Should -Match 'plan: 1 change\(s\), 0 blocker\(s\); nothing was changed'
    }

    It "says there is nothing to change on PP" {
        $output = & $script:Pwsh -NoProfile -NonInteractive -File $script:ScriptPath -Plan -FactsJson (Join-Path $script:FixtureDir 'pp.json') 2>&1
        $LASTEXITCODE | Should -Be 0
        (($output | ForEach-Object { "$_" }) -join "`n") | Should -Match 'nothing to change'
    }

    It 'prints every fresh action and still exits 0 in -Plan' {
        $output = & $script:Pwsh -NoProfile -NonInteractive -File $script:ScriptPath -Plan -FactsJson (Join-Path $script:FixtureDir 'fresh.json') 2>&1
        $LASTEXITCODE | Should -Be 0
        @($output | Where-Object { "$_" -like 'PLAN  *' }) | Should -HaveCount 25
        (($output | ForEach-Object { "$_" }) -join "`n") | Should -Match "PLAN  soundgrid_nic adapter 'Ethernet' \*FlowControl: 3 -> 0"
    }

    It "names each firewall blocker in -Plan and still exits 0 (issue 255)" {
        $facts = Read-Fixture 'snv'
        $facts.firewall.Public = $false
        $path = Join-Path ([System.IO.Path]::GetTempPath()) "setup-prod-pc-fw-$PID.json"
        try {
            ConvertTo-Json -InputObject $facts -Depth 20 | Set-Content -LiteralPath $path -Encoding UTF8
            $output = & $script:Pwsh -NoProfile -NonInteractive -File $script:ScriptPath -Plan -FactsJson $path 2>&1
            $LASTEXITCODE | Should -Be 0
            $text = ($output | ForEach-Object { "$_" }) -join "`n"
            $text | Should -Match 'BLOCKED firewall_off:public: '
            $text | Should -Not -Match 'PLAN  firewall'
        } finally {
            Remove-Item -LiteralPath $path -ErrorAction SilentlyContinue
        }
    }

    It 'refuses -FactsJson without -Plan (a fixture is never applied), exit 2' {
        $output = & $script:Pwsh -NoProfile -NonInteractive -File $script:ScriptPath -FactsJson (Join-Path $script:FixtureDir 'snv.json') 2>&1
        $LASTEXITCODE | Should -Be 2
        (($output | ForEach-Object { "$_" }) -join "`n") | Should -Match 'add -Plan'
    }

    It 'refuses to read a PC that is no Windows, exit 3' {
        $output = & $script:Pwsh -NoProfile -NonInteractive -File $script:ScriptPath -Plan 2>&1
        $LASTEXITCODE | Should -Be 3
        (($output | ForEach-Object { "$_" }) -join "`n") | Should -Match 'no Windows PC'
    }

    It 'saves the facts it planned from (-SaveFacts) in the shape it reads' {
        $saved = Join-Path ([System.IO.Path]::GetTempPath()) "setup-prod-pc-facts-$PID.json"
        try {
            & $script:Pwsh -NoProfile -NonInteractive -File $script:ScriptPath -Plan -FactsJson (Join-Path $script:FixtureDir 'fresh.json') -SaveFacts $saved | Out-Null
            $LASTEXITCODE | Should -Be 0
            $again = Get-Content -Raw -LiteralPath $saved | ConvertFrom-Json
            Get-PlanLines $again | Should -Be (Get-PlanLines (Read-Fixture 'fresh'))
        } finally {
            Remove-Item -LiteralPath $saved -ErrorAction SilentlyContinue
        }
    }

    It 'is ASCII (Windows PowerShell 5.1 reads it as cp1252) and names its irm line' {
        $bytes = [System.IO.File]::ReadAllBytes($script:ScriptPath)
        @($bytes | Where-Object { $_ -gt 127 }) | Should -HaveCount 0
        $text = [System.IO.File]::ReadAllText($script:ScriptPath)
        $text | Should -Match ([regex]::Escape('irm https://raw.githubusercontent.com/zbynekdrlik/songplayer/main/scripts/setup-prod-pc.ps1 | iex'))
        $text | Should -Match ([regex]::Escape("`$ErrorActionPreference = 'Stop'"))
        $text | Should -Match 'Set-StrictMode -Version Latest'
    }
}
