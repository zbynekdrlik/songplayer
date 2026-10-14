# Issue 274: scripts/runner-autostart.ps1, the runner's auto-start that every
# deploy delivers, and its choice of the duplicate listeners to end.
#
# Pester 5 under pwsh 7 on Linux (the CI job "Production PC setup (Pester +
# PSScriptAnalyzer)"). The script's functions are loaded from its syntax tree:
# nothing of the script runs at load (it reads processes and ends some).

BeforeAll {
    Set-StrictMode -Version Latest
    $script:ScriptPath = Join-Path (Split-Path -Parent $PSScriptRoot) 'runner-autostart.ps1'
    $tokens = $null
    $parseErrors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($script:ScriptPath, [ref]$tokens, [ref]$parseErrors)
    if (@($parseErrors).Count -gt 0) { throw "runner-autostart.ps1 does not parse: $($parseErrors[0])" }
    $definitions = $ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $false)
    foreach ($definition in $definitions) {
        . ([scriptblock]::Create($definition.Extent.Text))
    }

    function New-Listener {
        param([int]$ProcessId, [long]$StartTicks)
        return [pscustomobject]@{ ProcessId = $ProcessId; StartTicks = $StartTicks }
    }
}

Describe 'The files every deploy delivers' {
    It 'the bat starts run.cmd in the runner directory' {
        $bat = Get-RunnerStartBat -RunnerDir 'C:\actions-runner'
        $bat | Should -Match ([regex]::Escape('cd /d C:\actions-runner'))
        $bat | Should -Match ([regex]::Escape('.\run.cmd'))
    }

    It 'the vbs quits on this directory''s listener or its running bat before it starts the bat' {
        $vbs = Get-RunnerStartVbs -RunnerDir 'D:\runner two'
        $run = $vbs.IndexOf('WshShell.Run')
        $run | Should -BeGreaterThan 0
        $guard = $vbs.Substring(0, $run)
        $guard | Should -Match ([regex]::Escape('D:\runner two\bin\Runner.Listener.exe'))
        $guard | Should -Match ([regex]::Escape('D:\runner two\start-runner.bat'))
        $guard | Should -Match 'WScript\.Quit 0'
        $vbs.Substring($run) | Should -Match ([regex]::Escape('"""D:\runner two\start-runner.bat"""'))
    }

    It 'both files are ASCII (WScript and cmd read them in the ANSI code page)' {
        foreach ($text in @((Get-RunnerStartBat -RunnerDir 'C:\actions-runner'), (Get-RunnerStartVbs -RunnerDir 'C:\actions-runner'))) {
            ($text.ToCharArray() | Where-Object { [int]$_ -gt 127 }).Count | Should -Be 0
        }
    }
}

Describe 'Set-RunnerFile writes only a file that differs' {
    It 'writes a missing file, then leaves the same text alone' {
        $path = Join-Path $TestDrive 'start-runner.vbs'
        Set-RunnerFile -Path $path -Text "line one`r`nline two" | Should -Be 'updated'
        (Get-Content -Raw -LiteralPath $path).TrimEnd() | Should -Be "line one`r`nline two"
        Set-RunnerFile -Path $path -Text "line one`r`nline two" | Should -Be 'unchanged'
    }

    It 'replaces the old unguarded vbs PP kept' {
        $path = Join-Path $TestDrive 'old.vbs'
        Set-Content -LiteralPath $path -Value "Set WshShell = CreateObject(`"WScript.Shell`")`r`nWshShell.Run `"`"`"C:\actions-runner\start-runner.bat`"`"`", 0, False"
        $new = Get-RunnerStartVbs -RunnerDir 'C:\actions-runner'
        Set-RunnerFile -Path $path -Text $new | Should -Be 'updated'
        (Get-Content -Raw -LiteralPath $path) | Should -Match 'Win32_Process'
    }

    It 'reads LF and CRLF line ends as the same text' {
        $path = Join-Path $TestDrive 'lf.bat'
        [System.IO.File]::WriteAllText($path, "a`nb`n")
        Set-RunnerFile -Path $path -Text "a`r`nb" | Should -Be 'unchanged'
    }
}

Describe 'Select-DuplicateListener' {
    It 'keeps the listener that runs this job and ends every other one' {
        $listeners = @(
            (New-Listener -ProcessId 8768 -StartTicks 100),
            (New-Listener -ProcessId 3348 -StartTicks 300),
            (New-Listener -ProcessId 15588 -StartTicks 400)
        )
        $ends = @(Select-DuplicateListener -Listener $listeners -Ancestor @(4242, 3348, 1))
        ($ends | ForEach-Object { $_.ProcessId } | Sort-Object) | Should -Be @(8768, 15588)
    }

    It 'outside a job keeps the oldest listener' {
        $listeners = @(
            (New-Listener -ProcessId 3348 -StartTicks 300),
            (New-Listener -ProcessId 8768 -StartTicks 100),
            (New-Listener -ProcessId 12812 -StartTicks 500)
        )
        $ends = @(Select-DuplicateListener -Listener $listeners -Ancestor @(4242))
        ($ends | ForEach-Object { $_.ProcessId } | Sort-Object) | Should -Be @(3348, 12812)
    }

    It 'two listeners started at the same tick: the lower process id is kept' {
        $listeners = @((New-Listener -ProcessId 900 -StartTicks 100), (New-Listener -ProcessId 200 -StartTicks 100))
        $ends = @(Select-DuplicateListener -Listener $listeners -Ancestor @())
        $ends.Count | Should -Be 1
        $ends[0].ProcessId | Should -Be 900
    }

    It 'one listener, or none, ends nothing' {
        @(Select-DuplicateListener -Listener @((New-Listener -ProcessId 8768 -StartTicks 100)) -Ancestor @()).Count | Should -Be 0
        @(Select-DuplicateListener -Listener @() -Ancestor @(1)).Count | Should -Be 0
    }
}
