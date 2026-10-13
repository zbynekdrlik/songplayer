---
paths:
  - "crates/sp-server/src/prod_env*.rs"
  - "crates/sp-core/src/prod_env*.rs"
  - "crates/sp-core/src/health.rs"
  - "crates/sp-server/src/api/routes_status.rs"
  - "sp-ui/src/components/health_bar.rs"
  - "e2e/environment-gate*.ts"
  - "e2e/post-deploy-environment.spec.ts"
  - "e2e/health-environment.spec.ts"
  - "e2e/post-deploy-pp.config.ts"
---

# The production environment (#233): LTSC or Windows 11 IoT Enterprise, Defender / antivirus / firewall OFF

## Why

PP's Waves SoundGrid output popped (LV1 counted errors) on every child
process SongPlayer launched (the 10-min sync's yt-dlp, `/youtube/probe`).
Proven on PP, 11.10.2026 (the last comments of #233):

- an xperf kernel trace: the driver's resync came ~60 ms after SongPlayer
  created yt-dlp; the audio thread's DPC wake-ups turned irregular
  (16.5 ms instead of 10.7) on CPUs busy with **MsMpEng.exe**;
- A/B/A on `/youtube/probe`: as is +2…+3 resyncs per probe; a Defender
  `ExclusionProcess` for yt-dlp + deno 0, 0, 0, 0; exclusion removed
  +2…+3 again. Every earlier hypothesis (handle inheritance, console,
  priority, CPU, network, the driver sharing SongPlayer's process) was
  ruled out by measurement;
- with Defender's real-time protection off on PP: 4 probes, 0 resyncs, 0
  hard re-centres.

**The owner's rule (11.10.2026):** a production PC runs Windows LTSC, with
Defender / antivirus / firewall OFF, and "nemôže nič len tak zasahovať do
production-grade PC/SW": no startup check and no E2E test may let a box run
otherwise. **Ruling of the same night:** Windows 11 IoT Enterprise
(`EditionID` `IoTEnterprise`, not LTSC: SNV) is allowed too. No allowance
(env var, skip) exists in the gate. The review of 12.10.2026 (5🟡 7🔵, all
fixed) closed the gaps listed under "Review 12.10.2026" below.

## The check (pure: `prod_env.rs` the verdict, `prod_env_read.rs` the facts; glue: `prod_env_win.rs`)

The glue only READS raw values; every decision is pure and Linux-tested.
Nothing spawns a process (a launched process is what Defender scanned):
registry through `RegGetValueW` and the service manager through
`OpenSCManagerW` / `QueryServiceStatusEx` (windows-sys `Win32_System_Registry`,
`Win32_System_Services`), WMI through the `windows` 0.58 crate
(`Win32_System_{Com,Rpc,Wmi}`). Never PowerShell, `wmic` or any other
process for this.

- **Two reads per check** (`Platform`, `next_check`):
  - `read_direct` (`DirectRead`), at each check, never bounded: the
    edition, product name and build (`HKLM\SOFTWARE\Microsoft\Windows
    NT\CurrentVersion` `EditionID`, `ProductName`, `CurrentBuildNumber`),
    the firewall's registry values (below), the `WinDefend` service's state;
  - `read_wmi` (`WmiRead`), behind the bounded `Reader<T>` (`READ_BOUND`
    30 s), on a thread of its own (COM MTA, never the blocking pool's
    apartment): Defender's status row, the firewall's active store, the
    Security Center's antivirus products. Every `ConnectServer` passes
    `WBEM_FLAG_CONNECT_USE_MAX_WAIT` (it gives up after 2 min instead of
    never), each proxy and enumerator is set to impersonate with
    `CoSetProxyBlanket` (never `CoInitializeSecurity`: it is
    process-wide), `Next` waits 10 s per row (`WBEM_S_TIMEDOUT` = an
    error), at most 64 rows.
  - A WMI read past the bound is unreadable and the NEXT check waits for
    that same read again (never a second thread); a read that FINISHED
    only after its call gave up is stale (up to a period old) and is
    dropped: the next call reads afresh. A WMI hang leaves every registry
    fact (and the firewall's registry fallback) fresh.
- **Edition** (`edition_allowed`): `LTSC_EDITIONS` (`EnterpriseS`,
  `EnterpriseSN`, `IoTEnterpriseS`, `IoTEnterpriseSK`) at any build;
  `IOT_ENTERPRISE_EDITIONS` (`IoTEnterprise`, `IoTEnterpriseK`, the
  subscription edition) only from `WINDOWS_11_BUILD` 22000 (the owner
  allowed "11 IoT Enterprise"); ASCII case ignored, trimmed. Anything else
  (`Enterprise`, `Professional`, `Core`, the evaluation `EnterpriseSEval`)
  is `edition_not_allowed`, naming the edition and the build. An IoT
  Enterprise edition whose build was not read is `unreadable:build`.
- **Firewall**, per profile (domain, private, public): its row in the
  ACTIVE policy store (`root\StandardCimv2` `MSFT_NetFirewallProfile` read
  with the `IWbemContext` value `PolicyStore = "ActiveStore"`: the local
  settings merged with every group policy), `Enabled` 0 off / 1 on
  (`active_firewall`); any other value (2 = not configured), no row or no
  value is no answer. Only then does the registry decide
  (`firewall_from_registry`), and `read_errors` says so ("…: the registry
  decided"): the group-policy value
  `HKLM\SOFTWARE\Policies\Microsoft\WindowsFirewall\<Domain|Private|Public>Profile\EnableFirewall`
  and, for the private and public profiles, the older
  Administrative-Templates policy `…\WindowsFirewall\StandardProfile\EnableFirewall`
  (with either set, ANY set policy saying on reads on: a fallback never
  passes a box a policy turns on), else the local
  `HKLM\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\<Domain|Standard|Public>Profile\EnableFirewall`.
- **Defender**: `root\Microsoft\Windows\Defender` `MSFT_MpComputerStatus`
  `RealTimeProtectionEnabled`, `AMRunningMode`, `AntivirusEnabled`
  (`defender_realtime`, `PASSIVE_MODES`):
  - real-time on = `defender_realtime_on`;
  - a mode that still scans files while another product leads ("Passive
    Mode", "Passive", "EDR Block Mode", "SxS Passive Mode", Microsoft's
    `Get-MpComputerStatus` doc) = `defender_passive_mode`, even with
    real-time off; `Normal` / `Not running` are fine;
  - `AntivirusEnabled` and the mode are shown only;
  - `WBEM_E_INVALID_NAMESPACE` / `WBEM_E_INVALID_CLASS` = not installed =
    off;
  - its status unreadable (no instance, no value, no mode, an error, the
    WMI read timed out): the `WinDefend` service decides
    (`defender_service_off`): stopped (a disabled service is stopped) or
    `ERROR_SERVICE_DOES_NOT_EXIST` = off; running, starting or unreadable =
    `unreadable:defender`, the read error naming both reasons.
- **Other antivirus products** (`antivirus_on`): `root\SecurityCenter2`
  `AntiVirusProduct` (`displayName`, `instanceGuid`, `productState`).
  Defender's own entry (GUID `DEFENDER_AV_GUID`
  `{D68DDC3A-831F-4fae-9E44-DA132C1ACF46}`, or the names "Windows Defender"
  / "Microsoft Defender Antivirus") is skipped: Defender is checked through
  its own status. Every other product whose `productState` has the
  scanner's on bit (`AV_STATE_ON` 0x1000) is `antivirus_on` (one problem
  per product, named). The word is undocumented but stable since Vista:
  bits 16-23 the provider kind, bits 8-15 the scanner (0x10 on, 0x00 off,
  0x01 snoozed / expired), bits 0-7 the definitions (0x00 up to date, 0x10
  out of date); e.g. 0x061100 Defender on, 0x060100 Defender off, 0x041000
  a product on. A product with no state, or a failed query, is
  `unreadable:antivirus`; no Security Center (a server SKU, the CI runner)
  lists none.
- **Unreadable is a problem**, never fine (`unreadable:<what>`, "… sa
  nepodarilo zistiť"); the why is in `read_errors` (each reason once).

## Where it shows

- `run` (spawned by `lib.rs::start`): a check at once, then every
  `CHECK_EVERY` (60 s), published into the process store (`global()`, a
  `std::sync::RwLock` copied out: no lock across an await).
- `GET /api/v1/status.environment` and `GET /api/v1/environment`:
  `sp_core::prod_env::EnvironmentStatus` = `{checked, ok, note, edition_id,
  product_name, build_number, ltsc, edition_allowed, defender_realtime,
  defender_passive, defender_mode, defender_antivirus_enabled,
  antivirus_on, firewall: {domain, private, public}, problems: [{code,
  text_sk}], read_errors, checked_at_ms}`. Before the first check: not
  checked, not ok. Off Windows: checked, ok, `note` "not Windows: …".
- Problem codes (stable), in this order: `edition_not_allowed`,
  `unreadable:edition`, `unreadable:build`, `defender_realtime_on`,
  `defender_passive_mode`, `unreadable:defender`, `antivirus_on`,
  `unreadable:antivirus`, `firewall_on:<domain|private|public>`,
  `unreadable:firewall:<profile>`; Slovak texts in
  `sp_core::prod_env::EnvProblem::text_sk`.
- Logs (`Reporter::step` → `LogStep`): ONE ERROR when the problems appear
  or change (another edition counts), ONE INFO when they clear, one INFO at
  the first check that holds, ONE WARN when only the read errors change
  (the same problems for another reason, a reason that comes or goes).
- Dashboard: the HealthBar polls `/api/v1/environment` every 10 s and shows
  the red `health-environment` segment "Prostredie: <first problem>"
  (`(+N)` for more), the rule + every problem as its tooltip
  (`sp_core::prod_env::label`, `HealthTone::Error` → `.health-error`). The
  bar's `data-environment` is the last poll's answer
  (`sp_core::prod_env::marker`: `unchecked` until the first answer, `ok`,
  `violated`): a spec asserts it BEFORE it asserts the segment's absence
  (an absent segment proves nothing before the WASM rendered the poll).
  Mock: `/__mock/environment {…}` over a holding default,
  `/__mock/environment-reset`; spec `e2e/health-environment.spec.ts`.

## The gate

`e2e/post-deploy-environment.spec.ts`, in SNV's suite (`post-deploy.config.ts`
takes every `post-deploy*.spec.ts`) and PP's subset (`post-deploy-pp.config.ts`
lists it): polls `/api/v1/status` until `environment.checked` (60 s) and
judges THAT status (no second read), then FAILS (`environment-gate.ts`) on
every problem (`code: text`), on every fact that is not the production one
whatever `ok` says (`edition_allowed` true; `defender_realtime`,
`defender_passive` and every `firewall.*` false; `antivirus_on` `[]`; a fact
missing from an older server reads null and fails), with the read errors
next to them, and on a missing, unchecked or "not Windows" status; then the
dashboard's bar reads `data-environment="ok"` and shows no segment, console
clean. Unit-tested by `environment-gate.spec.ts` (mock suite).

A red gate means the BOX is wrong: fix the box (Defender real-time off and
its policy keys, firewall off, the edition), never the gate. Defender's
real-time switch may need Tamper Protection off first (Windows Security
GUI); PP's and SNV's settings of 11.10.2026 are on the ticket.

## Tests

- `crates/sp-core/src/prod_env_tests.rs`: every code and Slovak text, the
  wire shape by name, a missing status, the label (none / one / +1 / +2),
  the marker.
- `crates/sp-server/src/prod_env_tests.rs`: the edition table (LTSC at any
  build; IoTEnterprise / IoTEnterpriseK at 22000 and up, not 21999 or
  19045, undecided with no build; the rest never), the verdict per problem
  and in order, the status of a check, the log steps (read errors
  included), `apply` into a private store, both routes through the real
  router (serialized on `GLOBAL`: they publish into the process store).
- `crates/sp-server/src/prod_env_read_tests.rs`: the active store's rows,
  the registry fallback (the policy over the local value, the older
  StandardProfile policy for private + public only), Defender's read, mode
  and service fallback, the productState decoder, Defender's own entry,
  the antivirus list's failures, the build, and `facts_from` end to end.
- `crates/sp-server/src/prod_env_reader_tests.rs`: the bounded reader (a
  hung read waited for three times and never started twice; a read that
  finished late dropped and read afresh; a panicking read), `next_check`,
  and on the Windows CI job `the_windows_glue_reads_what_windows_itself_reports`:
  Build (Windows) first reads the runner with Windows' own cmdlets
  (`Get-MpComputerStatus` RealTimeProtectionEnabled + AMRunningMode,
  `Get-NetFirewallProfile -PolicyStore ActiveStore`, `Win32_OperatingSystem`
  BuildNumber) into `SP_ENV_EXPECT_*` (read only, the runner is never
  changed), and the glue's facts must equal them, with no read error.

## Review 12.10.2026 (all fixed)

1. The older StandardProfile firewall policy was not read (policy on +
   local off read off): the active store decides now, the registry
   fallback counts that policy.
2. Defender in passive / EDR block mode and third-party antivirus were not
   checked: `defender_passive_mode`, `antivirus_on`.
3. Defender stopped / uninstalled read `unreadable:defender`: the service
   fallback.
4. Nothing proved the Defender read can say anything but "off": the
   Windows job compares the glue with Windows' own cmdlets.
5. The specs' "no segment" passed before the WASM rendered: the
   `data-environment` marker.
6. One 30 s bound covered the registry and WMI, and `ConnectServer` had no
   wait bound: the registry is read directly, only WMI is bounded,
   `WBEM_FLAG_CONNECT_USE_MAX_WAIT`.
7. A late read was published as fresh: a finished stale read is dropped.
8. A changed unreadable reason logged nothing: one WARN.
9. The gate trusted `ok` alone: every production fact by name.
10. The post-deploy spec read the status twice: it judges the polled one.
11. `IoTEnterpriseK` was refused and Windows 10 IoT Enterprise allowed:
    both IoT Enterprise editions on Windows 11 only.
12. The ASIO host's stalled-pipe test had lost its exact pin (bursts now).

## Known limits

- The productState word is undocumented (decoded as every tool does); a
  product that registers no state reads `unreadable:antivirus`, never fine.
- The registry fallback is conservative (any set policy on = on) and does
  not see MDM / CSP firewall settings; it decides only while the active
  store cannot be read, and says so.
- The Windows job proves the glue agrees with Windows on the RUNNER's
  values (whatever its Defender and firewall are); a box's own values are
  first checked by the post-deploy gate (SNV / PP).

## Diagnosing an audio disturbance on a box: the method that found this

- **Measure on the driver, not by ear:** the ASIO output's `resyncs`,
  `hard_recentres`, `callback_gap_max_ms`, `late_callbacks` and
  `fast_callbacks` on `GET /api/v1/program` `outputs[i].asio`. Read them
  before and after a single trigger (`POST /api/v1/youtube/probe` spawns one
  yt-dlp), and repeat 3–4 times: one sample proves nothing (one probe in
  four read 0).
- **A/B/A, or it is a guess:** the Defender verdict held only once the
  change (a Defender `ExclusionProcess`), its removal and the change again
  all read as predicted. Two earlier "proofs" (the driver sharing
  SongPlayer's process, handle inheritance) were single comparisons and
  were wrong.
- **The MCP shell is a confounded control:** `mcp__win-*__Shell` runs at
  **BelowNormal** (its python parent is BelowNormal) and its children
  inherit it. A shell-launched process is therefore not a like-for-like
  stand-in for one SongPlayer launches (Normal). Raise the shell first
  (`[Diagnostics.Process]::GetCurrentProcess().PriorityClass='Normal'`) and
  still treat the result with care: Defender treated the two launches
  differently anyway.
- **The kernel trace that pinned it:** the Windows Performance Toolkit is on
  PP (`C:\Program Files (x86)\Windows Kits\10\Windows Performance
  Toolkit\xperf.exe`, installed 11.10.2026).
  - Record: `xperf -on PROC_THREAD+LOADER+DPC+INTERRUPT+CSWITCH+DISPATCHER+PROFILE
    -stackwalk CSwitch+Profile -BufferSize 1024 -MaxBuffers 1024 -f k.etl`,
    trigger, then `xperf -stop -d trace.etl`.
  - Close LatencyMon first: it holds the NT Kernel Logger.
  - Starting or stopping a kernel trace itself causes SG resyncs, so do not
    time a measurement across it.
  - `-a tracestats` gives the absolute start, which lines the trace up with
    the host log's WARN times.
  - `-a process` gives the process starts. Never print the command lines:
    `-a dumper` includes them.
  - `-a dpcisr -range` gives per-module DPC/ISR time.
  - `-a dumper -range` gives CSwitch/ReadyThread per thread: the SG audio
    thread's wake-ups turned 13 / 7.5 / 4.2 / 3.4 / 16.5 ms instead of
    10.7, ~60 ms after the process start.
