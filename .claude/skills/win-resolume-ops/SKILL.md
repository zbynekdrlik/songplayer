---
name: win-resolume-ops
description: >
  Songplayer operations on the win-resolume machine (10.77.9.201). Load when
  doing deployments, CI monitoring, runner checks, Resolume diagnostics, or any
  work that touches the live Windows machine — covers OBS/Resolume safety rules,
  runner health, CI cancel policy, and shared-machine discipline.
user-invocable: false
triggers:
  - win-resolume
  - resolume.lan
  - OBS
  - Resolume Arena
  - 10.77.9.201
  - CI deploy
  - runner
  - SongPlayer.exe
---

# win-resolume Machine Operations

## Machine config

- **IP:** 10.77.9.201 (resolume.lan)
- **User:** Resolume
- **Services:** OBS (port 4455 WebSocket), Resolume Arena, RemoteOS MCP (port 8090),
  OBS MCP via supergateway (port 8091), GitHub Actions runner, SongPlayer (port 8920)
- **SongPlayer data:** `C:\ProgramData\SongPlayer\`
- **SongPlayer install:** `C:\Program Files\SongPlayer\`

## MCP tool traps (cost two agents hours on 2026-08-05)

- **`mcp__win-resolume__FileWrite` SILENTLY TRUNCATES `content` over ~20,000
  characters.** It raises no error — it writes less than you passed and reports
  the smaller count, so the file looks written and then fails at runtime in a
  confusing way. Write large files in `append: true` chunks and VERIFY the remote
  size/line count afterwards.
- **PowerShell mangles inline python.** `python -c "..."` breaks on `$`, quotes
  and backticks (a SQL `length(value)` became *"The term 'value' is not
  recognized as the name of a cmdlet"*). ALWAYS `FileWrite` a `.py` file, then
  run it with `Shell`.
- MCP is the ONLY sanctioned channel here — never ssh/scp to this box. If an MCP
  call fails with a connection/timeout error, STOP and tell the user.

## This box IS the local-model machine (not dev2)

Local ML models run HERE, not on any other box. Established venvs under
`C:\ProgramData\SongPlayer\cache\tools\`: `whisperx_venv`, `crisper_venv`,
`parakeet_venv`, `vibevoice_venv`, `lyrics_venv`, plus an `hf_models` HuggingFace
cache. When a task needs a local model, inspect those first and mirror a proven
torch/CUDA build rather than starting from zero — and never propose moving this
project's model work to another machine.

Also here: system python `C:\Program Files\Python312\python.exe` (3.12.10),
eval fixtures at `C:\ProgramData\SongPlayer\eval-cache\`, eval scaffolding at
`C:\ProgramData\SongPlayer\eval-run\`, API keys in the `settings` table of
`C:\ProgramData\SongPlayer\songplayer.db`.

## Subprocess priority — never saturate the machine

The Windows machine running OBS + Resolume + SongPlayer is the LIVE event PC.
Demucs, Gemini processing, and heavy subprocess work cause hardware overload and
reboots (2026-04-21 incident). Always use `BELOW_NORMAL` priority for background
subprocesses. Leave headroom for OBS + Resolume.

## OBS — never kill

NEVER `taskkill /F` OBS. Doing so causes a "restore dialog" on next start that
gets stuck. Use OBS's graceful shutdown only. If OBS is unresponsive and needs
a restart, inform the user.

## SongPlayer — graceful shutdown only

Never `taskkill /F /IM SongPlayer.exe` without explicit user approval. Force-
killing SongPlayer can send the wall dark mid-event. Use the graceful stop
endpoint or ask the user to restart.

Exception: when user has explicitly stated win-resolume is dedicated for dev
(no event running), restart-class actions can proceed without per-call approval.
Force-kill and machine reboot still always require approval.

## Resolume Arena — overnight hang pattern

Resolume Arena on win-resolume frequently becomes unresponsive after a night of
CI activity. The window still paints but the REST server at
`http://127.0.0.1:8090/api/v1/composition` returns "connection refused".

When user reports "no lyrics on Resolume" / "no title" in the morning:
1. Check Arena REST FIRST: `Invoke-WebRequest http://127.0.0.1:8090/api/v1/composition -UseBasicParsing -TimeoutSec 3`
2. Check process: `Get-Process Arena | Format-List Name, Id, Responding`
3. If `Responding=False` or REST is down — Arena is hung. Report to user.
4. Never force-kill Arena without asking. It is the user's live VJ tool.
5. After Arena restart, SongPlayer clip mapping refreshes automatically every 10s.

When E2E CI cancels mid-step: default first hypothesis is Resolume Arena stuck.
Diagnose with `Get-Process Arena | Format-List Responding` and
`curl 127.0.0.1:8090` before assuming SongPlayer code bug. Fix Resolume, then
re-run CI.

## CI deploy monitoring — runner health

When `Deploy to win-resolume` stays `queued` for >2 minutes:
1. `ping 10.77.9.201` — is the machine up?
2. `gh api repos/zbynekdrlik/songplayer/actions/runners` — is the runner online?
3. If ping fails or runner offline: tell user IMMEDIATELY. Do NOT wait silently.

Self-hosted runners pick up queued jobs within seconds when online. >2 min
queued = machine powered off or runner service stopped.

## CI cancel policy — event window only

In this repo, during **active live-wall events** (user has explicitly said an
event is running), cancel remaining CI jobs the moment `Deploy to win-resolume`
reports `success`:
```bash
gh run cancel <run_id>
```

Do NOT auto-cancel outside event windows. When there is no live wall running,
let the full pipeline (E2E, 30-min snapshot) complete — the data is useful.

**Event status is user-authoritative.** NEVER infer "event in progress" from
OBS scene state (`sp-live`, `sp-fast`, etc.), playlist activity, or wall
traffic. The user's explicit statement is the only signal.

## CI polling — short intervals

Never `sleep 1800` or `sleep 2400` as a single blocking wait for CI. Use a
Monitor or background poll that wakes every 60-300s and catches BOTH success
AND failure states. A failure at minute 5 must surface in minutes, not 40.

## Windows installs — irm | iex pattern

Use the one-line PowerShell installer pattern for setting up services:
```powershell
irm https://raw.githubusercontent.com/owner/repo/branch/scripts/install.ps1 | iex
```
Create an `install.ps1` in the repo that handles download, config, scheduled
task, firewall, and verification. Not manual multi-step commands.

## win-resolume is always free when user prompts

When the user gives a new prompt, win-resolume is ALWAYS free. Never defer
interactive work citing "wall idle window" or "event might be in progress".
If the user wanted to stop, they would stop Claude. CI uses the machine without
an idle check; Claude during active prompts can too.
