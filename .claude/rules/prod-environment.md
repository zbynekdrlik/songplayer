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
  - "scripts/setup-prod-pc.ps1"
  - "scripts/tests/setup-prod-pc.Tests.ps1"
  - "scripts/tests/test_setup_prod_pc_workflows.py"
  - "scripts/tests/fixtures/prod-pc/**"
---

# The production environment (#233): LTSC or Windows 11 IoT Enterprise, Defender / antivirus / firewall OFF; the audio profile (#255)

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

## The audio profile (#255 lane 1: the checks and the gate)

The owner (12.10.2026): what was found and set by hand on PP and SNV is
built into SongPlayer, so a reinstalled or new PC is checked by SongPlayer
itself and the deploy gate, never set up from memory. Lane 1 checks; lane 2
APPLIES: `scripts/setup-prod-pc.ps1` (below, "The setup script") runs
between SongPlayer's stop and its start in every deploy, and alone on a new
PC. SongPlayer itself never changes a system setting.

**Files.** Pure: `sp_core::prod_env::audio` (`prod_env_audio.rs`: the
vocabulary, the wire shape, `notes`), sp-server `prod_env::audio`
(`prod_env_audio.rs`: the facts, the verdict, the view) and
`prod_env::audio::read` (`prod_env_audio_read.rs`: the walk). Glue:
`prod_env_audio_win.rs`, read once per check from `read_direct`
(`DirectRead.audio`: registry-fast, so read directly, not behind the WMI
bound); `facts_from` takes it and appends its reasons to `read_errors`
(each once).

**The walk is pure over a registry reader.** `RegistryRead` has two
primitives (a value of any type, a key's subkeys under HKLM); the glue
implements them with `RegGetValueW(RRF_RT_ANY | RRF_NOEXPAND)` (the bytes
decoded by the pure `reg_value_from`) and `RegOpenKeyExW` /
`RegEnumKeyExW`; the tests with an in-memory registry
(`prod_env_audio_read_fake.rs`, PP's and SNV's layout as read on the boxes).
Nothing spawns a process: the network interfaces come from `GetIfTable2`,
power from the power API, DVS's file from `std::fs`. A source scan
(`the_environment_reads_spawn_no_process`) refuses `std::process`,
`Command::new`, `CreateProcess`, `ShellExecute` in every reader.

**An audio adapter is found by the MAC its own software names, never by
name** (a reinstalled PC with other adapter names is covered), matched by
`adapter_for_mac` (below, shared by both):

- **SoundGrid** (`prod_env_audio_soundgrid.rs`): installed = the
  `HKLM\SYSTEM\CurrentControlSet\Services\SoundGridProtocol` key exists (no
  key = not installed: no SoundGrid facts, whatever its files say). Its
  adapter = the one whose MAC is the SoundGrid ASIO driver's chosen port:
  `<Configuration><LocalLanPort>` in
  `%APPDATA%\Waves Audio\Preferences\SoundGrid.Driver.preferences` (this
  process's own `APPDATA`: SongPlayer and its ASIO host run as the user
  whose driver they host, so it is the file the hosted driver reads; PP:
  user newlevel, `6c:24:08:e5:9c:91` = `soundgrid-pcie`). The file is read
  strictly (`chosen_port` / `element_text`, no XML crate, no regex): one
  `<Configuration>`, at most one `<LocalLanPort>` in it (`<x/>` = empty), a
  BOM skipped; twice, unclosed, attributes, a longer name, a comment inside
  `<Configuration>` or a value that is no MAC is a typed `PortError` →
  `unreadable:soundgrid`. No file, no `LocalLanPort` or an empty one =
  `soundgrid_port_unset`; a MAC on no adapter = `soundgrid_port_missing`;
  several adapters = unreadable.
  **Why not `Linkage\Bind`** (lane 1's first cut, measured on PP 13.10.2026,
  #255 comment 6138104246): it lists 14 `\Device\{GUID}` — soundgrid-pcie,
  two Wi-Fi Direct virtual adapters, the not-present kernel debugger
  adapter and about ten adapters that no longer exist (the Dante adapter
  and Wi-Fi have the binding disabled) — so gating it failed PP on
  adapters SoundGrid never uses.
- **Dante Virtual Soundcard**: `%ProgramData%\Audinate\Dante Virtual
  Soundcard\manager.json` (no file = not installed; at most 256 KiB, a BOM
  skipped) read into a typed struct (never `serde_json::Value`; other
  fields skipped): `PrimaryInterfaceAddress` (MAC; `:`, `-`, `.` dropped,
  lowercase, 12 hex digits) and `PrimaryInterfaceName` (shown only). The
  adapter: the `GetIfTable2` row with that MAC that is a hardware interface
  and no filter module's (`InterfaceAndOperStatusFlags` bit 0 / bit 1; a
  filter repeats its adapter's MAC), the current address before the
  permanent one; none or several says why.
- **From the GUID** (`nic`): its instance in the network class
  (`Control\Class\{4d36e972-e325-11ce-bfc1-08002be10318}\<nnnn>`, only
  4-digit keys, `NetCfgInstanceId` equal, case ignored) holds `DriverDesc`,
  the settings and `DeviceInstanceID`; the connection
  (`Control\Network\{4D36E972-…}\{GUID}\Connection`) gives `Name` (shown)
  and `PnPInstanceId` (the instance when the class has none); the device's
  `Enum\<instance>\Service` is the driver service.

**The facts, their rule, evidence and gate status:**

| fact | read | rule | evidence | status |
|---|---|---|---|---|
| SoundGrid adapter's driver model | `Services\<svc>\Parameters\Wdf\KmdfLibraryVersion` | exists = NetAdapterCx, absent = NDIS (`driver_model`) | #233 A/B (4 loop events, +80 resyncs → 0 on NDIS); Microsoft's `executioncontext.cpp` (worker thread at priority 8); PP 13.10.2026: NDIS `rt640x64` has `Parameters\Wdf` with only `WdfMajor/MinorVersion`, NetAdapterCx `rtucx21x64` `KmdfLibraryVersion` 1.33 | **GATE** `soundgrid_nic_netadaptercx` |
| SoundGrid adapter's settings | the class instance: `*InterruptModeration`, `*FlowControl` (required: missing = `soundgrid_nic_setting:<name>` "chýba"), `*EEE`, `EnableGreenEthernet` / `GreenEthernet`, `PowerSavingMode` / `*PowerSavingMode`, `GigaLite` (the first name that exists; missing = fine), `PnPCapabilities` | every keyword 0; `PnPCapabilities` with `NDIS_DEVICE_DISABLE_PM` 0x08 set (ndis.h; the Power Management tab writes 24 = 0x18, KB 2740020 also 280 = 0x118; missing = 0 = Windows may turn it off); a value that is no number is wrong | Waves' SoundGrid PC guide (5.11.2025); PP's values 12.10.2026 (all 0, 24) | **GATE** `soundgrid_nic_setting:<interrupt_moderation\|flow_control\|eee\|green_ethernet\|power_saving\|gigalite\|power_management>` |
| SoundGrid adapter unreadable | any of the above failed / not found | — | — | **GATE** `unreadable:soundgrid_nic` |
| SoundGrid's chosen port | `SoundGrid.Driver.preferences` `LocalLanPort` (service installed) | a port chosen and on one hardware adapter | PP's file 13.10.2026 (above) | **GATE** `soundgrid_port_unset` ("SoundGrid nemá zvolený sieťový port — nastav ho v SoundGrid Studio / ovládači"), `soundgrid_port_missing` (names the MAC), `unreadable:soundgrid` (the service, the file, the interfaces) |
| `NoAutoUpdate` | `HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU` | DWORD 1 (missing, another number or type = on) | a Windows Update driver push would undo PP's NDIS swap; both boxes 1 (12.10.2026) | **GATE** `windows_auto_update_on`, `unreadable:windows_update` |
| `ExcludeWUDriversInQualityUpdate` | `…\WindowsUpdate` | DWORD 1 (missing, another number or type = not excluded) | a driver push would undo PP's NDIS swap; PP 1 (set 12.10.2026), SNV lacked it until lane 2's setup set it at its first deploy | **GATE** since lane 2 (the setup applies it before every start) `windows_update_drivers_on`, `unreadable:windows_update` (ONE for either policy) |
| MMCSS | `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile` `SystemResponsiveness` 0, `NetworkThrottlingIndex` 0xFFFFFFFF; `…\Tasks\Pro Audio` `Priority` 6, `Scheduling Category` High (case and padding ignored) | each exact; missing = `mmcss:<name>` "chýba"; another type = unreadable | #210 part 2 (`vban-out.md`); both boxes hold | **GATE** `mmcss:<system_responsiveness\|network_throttling_index\|pro_audio_priority\|pro_audio_scheduling_category>`, `unreadable:mmcss` |
| Dante adapter's model + settings | as for SoundGrid | NetAdapterCx = a note | the 12.10.2026 finding: BOTH boxes' DVS adapters are Realtek USB NetAdapterCx (PP `rtucx21x64`, SNV `rtucx22x64`); the effect on Dante is still being measured | **report only** |
| power | `PowerGetActiveScheme`, `PowerReadFriendlyName`, `PowerReadACValueIndex(SUB_PROCESSOR, CPMINCORES)` | `CPMINCORES` < 100 = a note | Waves' guide; PP Bitsum Highest Performance 100 %, SNV High performance 100 % | **report only** (the setup applies 100 on the active scheme; it stays a note here) |

Why the power API and not the registry: the registry holds only a scheme's
OWN values (`User\PowerSchemes\<scheme>\<sub>\<setting>`); a value left at
its default lives elsewhere, and a duplicated scheme (PP's Bitsum) has no
documented default. The power API is what `powercfg` itself calls, with no
process. `ERROR_FILE_NOT_FOUND` from the value read = the scheme has no such
setting: no read error, and the note "parkovanie jadier sa nepodarilo
zistiť".

**Report-only facts are notes, never problems** (`sp_core::prod_env::audio::notes`,
put on the status as `audio.notes` by the server): the Dante adapter on
NetAdapterCx or unreadable, DVS not determinable, no power scheme, core
parking under 100 % or unreadable. (The driver exclusion was a note in
lane 1; lane 2 made it a gate.) Their read errors still go to
`read_errors` (the gate prints read errors only next to a failure).

## Where it shows

- `run` (spawned by `lib.rs::start`): a check at once, then every
  `CHECK_EVERY` (60 s), published into the process store (`global()`, a
  `std::sync::RwLock` copied out: no lock across an await).
- `GET /api/v1/status.environment` and `GET /api/v1/environment`:
  `sp_core::prod_env::EnvironmentStatus` = `{checked, ok, note, edition_id,
  product_name, build_number, ltsc, edition_allowed, defender_realtime,
  defender_passive, defender_mode, defender_antivirus_enabled,
  antivirus_on, firewall: {domain, private, public}, audio: {soundgrid:
  "not_installed"|"unreadable"|"port_unset"|"port_missing"|"found",
  soundgrid_port_mac (12 hex digits), soundgrid_nic: NIC, dante: {installed, interface_address,
  interface_name, nic: NIC}, windows_update: {no_auto_update,
  drivers_excluded}, mmcss: {system_responsiveness, network_throttling_index,
  pro_audio_priority, pro_audio_scheduling_category}, power: {scheme_guid,
  scheme_name, core_parking_min_cores_pct}, notes}, problems: [{code,
  text_sk}], read_errors, checked_at_ms}`; NIC = `{guid, name, description,
  service, driver_model: "ndis"|"netadaptercx"|null, kmdf_library_version,
  netadaptercx_keywords, settings: [{setting, keyword, value, ok}],
  settings_ok}` (`netadaptercx_keywords`: the driver key holds
  `*IfConnectorPresent`, a keyword only a NetAdapterCx INF writes per
  Microsoft's "INF files for NetAdapterCx client drivers": evidence, shown
  only). Before the first check: not checked, not ok. Off Windows: checked,
  ok, `note` "not Windows: …", the audio profile all null and no note.
- Problem codes (stable), in this order: `edition_not_allowed`,
  `unreadable:edition`, `unreadable:build`, `defender_realtime_on`,
  `defender_passive_mode`, `unreadable:defender`, `antivirus_on`,
  `unreadable:antivirus`, `firewall_on:<domain|private|public>`,
  `unreadable:firewall:<profile>`; then #255's: SoundGrid's one of
  `unreadable:soundgrid` / `soundgrid_port_unset` / `soundgrid_port_missing`,
  or its adapter's `unreadable:soundgrid_nic`, `soundgrid_nic_netadaptercx`,
  `soundgrid_nic_setting:<name>` (in `NicSetting::ALL`'s order), then
  `unreadable:windows_update` (once, for either policy),
  `windows_auto_update_on`, `windows_update_drivers_on` (lane 2), then
  `unreadable:mmcss` (once, first) and `mmcss:<name>`; Slovak texts in
  `sp_core::prod_env::EnvProblem::text_sk` (an adapter named by its
  connection name, else its driver's description, else its GUID).
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
- #255: the report-only notes are the GREY `health-environment-notes`
  segment "Profil PC: N poznámka|poznámky|poznámok" (`notes_label`,
  `HealthTone::Off`), the notes and the power scheme as its tooltip, shown
  only while `audio.notes` is not empty; it never changes
  `data-environment` (the box still holds). Both boxes show it today (their
  DVS adapters are NetAdapterCx). The mock default sends no note; a spec
  sends `audio: {notes: […]}`.

## The gate

`e2e/post-deploy-environment.spec.ts`, in SNV's suite (`post-deploy.config.ts`
takes every `post-deploy*.spec.ts`) and PP's subset (`post-deploy-pp.config.ts`
lists it): polls `/api/v1/status` until `environment.checked` (60 s) and
judges THAT status (no second read), then FAILS (`environment-gate.ts`) on
every problem (`code: text`), on every fact that is not the production one
whatever `ok` says (`edition_allowed` true; `defender_realtime`,
`defender_passive` and every `firewall.*` false; `antivirus_on` `[]`; a fact
missing from an older server reads null and fails; #255: `audio.soundgrid`
"not_installed", or "found" with `audio.soundgrid_nic` `driver_model` "ndis"
and `settings_ok` true,
`audio.windows_update.no_auto_update` true, `audio.windows_update.drivers_excluded`
true (lane 2), `audio.mmcss` 0 / 4294967295 / 6 / "High"), with the read
errors next to them, and on a missing, unchecked or
"not Windows" status; then the dashboard's bar reads
`data-environment="ok"` and shows no segment, the notes segment shows the
count of `audio.notes` (or nothing), console clean. The report-only facts
are logged (`[#255 report-only] …`) and never fail. Unit-tested by
`environment-gate.spec.ts` (mock suite).

A red gate means the BOX is wrong: fix the box (Defender real-time off and
its policy keys, firewall off, the edition; #255: the SoundGrid adapter's
NDIS driver and settings, `NoAutoUpdate`, the driver exclusion, MMCSS),
never the gate. Everything but the edition and the NIC driver is what
`scripts/setup-prod-pc.ps1` applies: the deploy ran it before the start, so
read its log lines in the deploy job first ("Apply the production PC
profile": a BLOCKED line names what it cannot do for the operator). Defender's
real-time switch may need Tamper Protection off first (Windows Security
GUI); PP's and SNV's settings of 11.10.2026 are on the ticket; the NIC swap
and its rollback recipe are on #233 / PP `C:\ProgramData\SongPlayer\diag\nicswap\README.txt`.

## The setup script (#255 lane 2: `scripts/setup-prod-pc.ps1`)

Lane 1 CHECKS, the script APPLIES. Windows PowerShell 5.1 (the boxes'
shell; ASCII only, it is read as cp1252) and pwsh 7 (the Linux tests).
`$ErrorActionPreference = 'Stop'`, `Set-StrictMode -Version Latest`.

- **What it applies, in this order** (the plan's order):
  1. Windows Update: `…\WindowsUpdate\AU` `NoAutoUpdate` and
     `…\WindowsUpdate` `ExcludeWUDriversInQualityUpdate`, DWORD 1;
  2. MMCSS: `SystemResponsiveness` 0, `NetworkThrottlingIndex` 0xFFFFFFFF
     (written as the Int32 -1), Pro Audio `Priority` 6, `Scheduling Category`
     REG_SZ `High` (a string that reads `High` with case and padding ignored
     holds, as in lane 1);
  3. the SoundGrid adapter, only with the `SoundGridProtocol` service key:
     found like lane 1 (the `<Configuration><LocalLanPort>` MAC; the current
     address before the permanent one; `Get-NetAdapter -Physical`; its
     instance key in the network class by `NetCfgInstanceId`, only the
     4-digit subkeys are opened: an administrator cannot read `Properties`),
     then `*InterruptModeration` / `*FlowControl` (required) and
     `*EEE` / `EnableGreenEthernet`|`GreenEthernet` /
     `PowerSavingMode`|`*PowerSavingMode` / `GigaLite` (first name that
     exists) to 0 with `Set-NetAdapterAdvancedProperty -RegistryKeyword …
     -RegistryValue 0 -NoRestart`, `PnPCapabilities` with 0x08 set (missing,
     0 or no number → 24, else the value | 8, DWORD in the instance key),
     then ONE `Restart-NetAdapter` per adapter that changed;
  4. power: `powercfg /setacvalueindex SCHEME_CURRENT SUB_PROCESSOR
     CPMINCORES 100` + `/setactive SCHEME_CURRENT` (never another scheme);
     read with `powercfg /qh` (the English label, else the second-to-last
     hex value: AC then DC end every language's output);
  5. Defender, as #233 recorded PP and SNV (11.10.2026): `Set-MpPreference`
     `DisableRealtimeMonitoring` / `DisableBehaviorMonitoring` /
     `DisableIOAVProtection` / `DisableScriptScanning` true,
     `MAPSReporting` 0, `SubmitSamplesConsent` 2 (only while the `WinDefend`
     service runs: stopped or missing = Defender off, nothing to set; another
     state = a blocker); the policy keys `…\Windows Defender\Real-Time
     Protection` `Disable{RealtimeMonitoring,BehaviorMonitoring,
     OnAccessProtection,ScanOnRealtimeEnable,IOAVProtection}` = 1; the
     firewall: each profile ON in the ACTIVE store → `Set-NetFirewallProfile
     -Profile <it> -Enabled False` (a group policy that keeps it on fails the
     verify).
  Not here: the NIC driver swap (lane 3), the Dante adapter, CPU
  reservation, the edition.
- **Shape:** READ every fact into one object (`Read-ProdPcFacts`, then a
  JSON round trip, so a live read has exactly a fixture's shape; the round
  trip is checked: a list that came back as no list throws, and Windows
  PowerShell 5.1's `System.Array` type data is removed first: it writes a
  wrapped array as `{"value": …, "Count": n}`), PLAN with the pure
  `Get-ProdPcPlan` (facts → ordered `{Area, Name, Current, Wanted, Reason}`
  + how to apply, ONLY what differs) and `Get-ProdPcBlockers` (what it
  cannot do for the operator, with lane 1's code: `soundgrid_port_unset`,
  `soundgrid_port_missing`, `unreadable:soundgrid`, `unreadable:soundgrid_nic`,
  `soundgrid_nic_setting:<required>` the driver lacks, `unreadable:defender`,
  `defender_realtime_on` with every setting off — Tamper Protection named
  when it is on), APPLY each action (`SET` before → after, one line each; an
  error is recorded and the rest goes on), RE-READ and verify (`OK` / `FAIL`
  per action; at most 3 reads 5 s apart while something applied reads
  otherwise or a new blocker shows: Defender's status lags
  `Set-MpPreference`). Any error, mismatch or blocker = exit 1 naming it; a
  second run plans nothing.
- **Modes:** default apply (elevated, else exit 3; not Windows = exit 3);
  `-Plan` reads and prints, changes nothing, exit 0 (blockers printed, still
  0); `-Plan -FactsJson <file>` plans from a fixture (never applies: without
  `-Plan` exit 2); `-SaveFacts <file>` writes the facts read (to capture a
  box as a fixture). Under iex / a scriptblock it never `exit`s (that would
  close the operator's window): the code goes to `$LASTEXITCODE`.
- **A new PC** (elevated Windows PowerShell, a fresh window: it sets strict
  mode in that session):
  `irm https://raw.githubusercontent.com/zbynekdrlik/songplayer/main/scripts/setup-prod-pc.ps1 | iex`;
  the plan only: `& ([scriptblock]::Create((irm <same url>))) -Plan`.
- **Whose `%APPDATA%`:** lane 1 reads SongPlayer's own (the driver hosted
  in SongPlayer's ASIO host reads that file). The deploy runs as the
  runner's account: `scripts/setup-runner.ps1` registers the runner task as
  the console user (Interactive, Highest), SongPlayer's task runs as
  `Resolume` on SNV (ci.yml registers it) and `newlevel` at PP (phase 0) —
  which user the runner is on each box was not read (no box is touched by
  this lane). So the script never assumes: it takes the `SongPlayer`
  scheduled task's principal (`UserId`, a SID or an account translated to
  one), and that user's AppData: this process's own when it is that user,
  else the loaded hive's `User Shell Folders\AppData` with `%USERPROFILE%`
  = `ProfileList\<SID>\ProfileImagePath`, else `<profile>\AppData\Roaming`.
  No task yet (a new PC): the user running the script. Two tasks named
  SongPlayer: unreadable.
- **Where it runs** (between the stop and the start, never while SongPlayer
  runs; `scripts/tests/test_setup_prod_pc_workflows.py` walks every
  PowerShell step of both workflows in order and fails on a start after a
  stop with no run in between, plus the shapes below):
  - ci.yml "Deploy to win-resolume": the step "Apply the production PC
    profile (scripts/setup-prod-pc.ps1)" after "Deploy SongPlayer" (stop +
    install, `id: deploy`), a child `powershell.exe -File` with
    `timeout-minutes: 5`; "Configure auto-start and launch" is
    `if: ${{ !cancelled() && steps.deploy.outcome == 'success' }}`, so a
    failed or hung profile step still starts SongPlayer and the job fails;
  - ci.yml "Restart SongPlayer" (E2E): after the stop, a child process
    started from a `ProcessStartInfo` (`UseShellExecute = $false`: no window
    on the live box) waited for 5 min at most (the step has no bound of its
    own), killed past it; the relaunch always runs, then the step fails on a
    non-zero `$prodPcExit`; the fallback's second stop runs it again;
  - deploy-pp.yml: its own step after "Install SongPlayer…", before the
    `always()` "Start SongPlayer", `timeout-minutes: 5`. PP's dispatch runs
    main's workflow: PP gets the step with the next release.
- **Tests:** `scripts/tests/setup-prod-pc.Tests.ps1` (Pester 5, pwsh 7 on
  ubuntu; the functions are loaded from the script's syntax tree, nothing
  runs at load): the MAC rule, PP's preferences verbatim and every
  malformed shape, the PnPCapabilities bit math, the DWORD conversions,
  `powercfg`'s AC index, the plan on PP (empty), SNV (exactly
  `ExcludeWUDriversInQualityUpdate`), a fresh PC (all 28 actions, in order),
  a second run after its own changes (empty, through a JSON round trip),
  each blocker, Defender's service states, and the script itself in
  `-Plan -FactsJson` mode (exit codes 0 / 2 / 3, `-SaveFacts`); fixtures
  `scripts/tests/fixtures/prod-pc/{pp,snv,fresh}.json` (PP and SNV from
  lane 1's live values of 13.10.2026; capture a box with `-Plan -SaveFacts`
  to refresh one). CI: the job "Production PC setup (Pester +
  PSScriptAnalyzer)" (in the Gate's needs; fails when no test ran, any
  skipped, or PSScriptAnalyzer reports an error or syntax Windows
  PowerShell 5.1 cannot run, `PSUseCompatibleSyntax`), and Build (Windows)
  runs `-Plan` on the runner under 5.1 (the read glue, read only) and the
  adapter read + the other-user AppData branch directly.
- **Known limits:** the Defender MAPS / sample-submission values and the
  Defender policy keys were not read back by lane 1 on the boxes; PP and
  SNV were set that way on 11.10.2026 (#233), so their first run is
  expected to plan nothing there — if one does, it is applied (and logged).
  A keyword with no advanced property (no `ndi\params` entry) fails
  `Set-NetAdapterAdvancedProperty` and is reported, never written around.

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
- #255: `crates/sp-core/src/prod_env_audio_tests.rs` (the names, labels and
  production values, the adapter's name, every note and its order, the
  wire shape) and `prod_env_tests.rs` (every audio code and Slovak text,
  the rule, the notes segment and its Slovak count);
  `crates/sp-server/src/prod_env_audio_tests.rs` (the settings' rules,
  `PnPCapabilities`' bit, the driver model, one adapter's problems in
  order, MMCSS, Windows Update's two policies (lane 2: every pair of
  read / not set / unread), the whole profile, the view);
  `prod_env_audio_read_tests.rs` (the raw values, the network class, one
  adapter over the in-memory registry, every way a read fails, the
  no-process scan), `prod_env_audio_soundgrid_tests.rs` (PP's preferences
  verbatim, every malformed shape, unset / missing / unreadable / found,
  not installed whatever the file) and `prod_env_audio_read_tests_dvs.rs` (DVS's file and config,
  the MAC, the interfaces, the policies, MMCSS, power, PP's and SNV's
  whole profile) over `prod_env_audio_read_fake.rs`; and on the Windows
  job `the_audio_glue_reads_what_windows_itself_reports`: the same CI step
  also reads MMCSS and Windows Update (`Get-ItemProperty`), the active
  scheme and `CPMINCORES` (`powercfg /getactivescheme`, `powercfg /qh`),
  one enabled physical adapter (`Win32_NetworkAdapter`: GUID, service, MAC,
  connection name) and its service's `Parameters\Wdf` into
  `SP_ENV_EXPECT_{MMCSS,WU,POWER,NIC}`; the glue must read the same, find
  that adapter by GUID with no error and list its MAC in `GetIfTable2`
  (the runner has neither SoundGrid nor DVS).

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
- #255: the driver-model rule (`KmdfLibraryVersion` = NetAdapterCx) is the
  one measured on PP (13.10.2026). An NDIS-WDF miniport whose INF declares
  a KMDF service would read NetAdapterCx too (a false gate failure, never a
  false pass); `kmdf_library_version` and `netadaptercx_keywords` are on
  the status to confirm the model on SNV and PP.
- #255: the SoundGrid preferences are read from SongPlayer's own
  `%APPDATA%` (the env var): a SongPlayer started without the user's
  environment reads "%APPDATA% is not set" → `unreadable:soundgrid` (a
  visible failure, never a pass). A user who chooses another port in
  SoundGrid Studio moves the gate with it at the next minute's check.
- #255: only `PrimaryInterfaceAddress` / `PrimaryInterfaceName` of
  `manager.json` are read, as top-level strings (read on both boxes
  12.10.2026); another shape is a read error and a note, never a problem.

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
