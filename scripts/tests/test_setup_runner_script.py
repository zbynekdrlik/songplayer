"""#229 lane 6 ops: `scripts/setup-runner.ps1` must register a runner GitHub
still accepts, and must stop when the registration fails.

Found registering PP's runner (7.10.2026): the script pinned runner 2.325.0,
GitHub refused it ("Your runner version is out of date and can no longer
register with GitHub", HTTP 404), and the script still printed "Runner
configured" and registered the auto-start task for a runner that did not
exist."""

import re
from pathlib import Path

_SETUP = Path(__file__).resolve().parents[2] / "scripts" / "setup-runner.ps1"


def _script() -> str:
    return _SETUP.read_text(encoding="utf-8")


def test_the_runner_version_comes_from_the_latest_release():
    s = _script()
    assert "api.github.com/repos/actions/runner/releases/latest" in s
    # No hard-coded version that GitHub can retire under us.
    assert not re.search(r'\$runnerVersion\s*=\s*"\d+\.\d+\.\d+"', s)


def test_a_failed_registration_stops_the_script():
    s = _script()
    config = s.index(".\\config.cmd")
    after = s[config:]
    check = after.find("$LASTEXITCODE")
    assert check != -1, "config.cmd's exit code is never checked"
    configured = after.find("Runner configured")
    assert check < configured, "the exit code must be checked before claiming success"
    assert "throw" in after[check:configured]
