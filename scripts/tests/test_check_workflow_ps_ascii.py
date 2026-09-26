"""PowerShell run blocks must be ASCII (26.9.2026 E2E restart-step failure).

The win-resolume runner runs `shell: powershell` = Windows PowerShell 5.1, which
reads the step script as cp1252. An em dash inside a double-quoted string decodes
to bytes whose last one (0x94) is a curly quote PowerShell treats as a string
delimiter, so `Write-Error "... — ..."` failed with "A positional parameter
cannot be found that accepts argument 'investigate'" (E2E job 108408085281).
"""

from pathlib import Path

import check_workflow_ps_ascii as mod

_REPO = Path(__file__).resolve().parents[2]

PS_BAD = """jobs:
  e2e:
    runs-on: [self-hosted, resolume]
    steps:
      - name: Restart
        shell: powershell
        run: |
          # a comment with an em dash — is fine
          Write-Host "ok"
          Write-Error "FAIL: relaunch failed — investigate"
      - name: Next
        run: echo "— bash is fine"
"""

PS_SHELL_AFTER_RUN = """    steps:
      - name: Restart
        run: |
          Write-Host "x — y"
        shell: pwsh
"""

BASH_ONLY = """    steps:
      - name: Lint
        shell: bash
        run: |
          echo "ERROR: Found assert!(true) — tests must verify real behavior."
"""

PS_CLEAN = """    steps:
      - name: Restart
        shell: powershell
        run: |
          # comment — ok
          Write-Error "FAIL: relaunch failed - investigate"
"""


def test_em_dash_in_a_powershell_string_is_flagged_with_its_line():
    found = mod.violations(PS_BAD)
    assert [no for no, _ in found] == [10]
    assert "investigate" in found[0][1]


def test_shell_key_after_run_still_marks_the_step_powershell():
    assert [no for no, _ in mod.violations(PS_SHELL_AFTER_RUN)] == [4]


def test_non_powershell_steps_and_comments_are_ignored():
    assert mod.violations(BASH_ONLY) == []
    assert mod.violations(PS_CLEAN) == []


def test_main_exit_codes(tmp_path, capsys):
    bad = tmp_path / "bad.yml"
    bad.write_text(PS_BAD, encoding="utf-8")
    good = tmp_path / "good.yml"
    good.write_text(PS_CLEAN, encoding="utf-8")
    assert mod.main([str(bad)]) == 1
    assert "bad.yml:10:" in capsys.readouterr().out
    assert mod.main([str(good)]) == 0


def test_the_repo_workflows_have_ascii_powershell():
    ci = (_REPO / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    assert mod.violations(ci) == []
