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

# The production environment (#233): LTSC or IoT Enterprise, Defender and firewall OFF

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
(env var, skip) exists in the gate.

## The check (`crates/sp-server/src/prod_env.rs`, pure; `prod_env_win.rs`, glue)

- **Edition** (`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`
  `EditionID`, `ProductName` shown only): allowed = `LTSC_EDITIONS`
  (`EnterpriseS`, `EnterpriseSN`, `IoTEnterpriseS`, `IoTEnterpriseSK`) or
  `IoTEnterprise`, ASCII case ignored, trimmed. NOT allowed: `Enterprise`,
  `Professional`, `Core`, `IoTEnterpriseK`, and the evaluation
  `EnterpriseSEval` (it expires; a production box never runs it).
- **Firewall**, per profile (domain, private, public): the group-policy
  value `HKLM\SOFTWARE\Policies\Microsoft\WindowsFirewall\<Domain|Private|Public>Profile\EnableFirewall`
  WINS when it exists, else the local
  `HKLM\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\<Domain|Standard|Public>Profile\EnableFirewall`
  (the private profile is `Standard` there). Any value but 0 is on; a local
  value that is missing or a value that cannot be read is unreadable
  (`firewall_on`).
- **Defender real-time**: WMI `root\Microsoft\Windows\Defender`
  `MSFT_MpComputerStatus.RealTimeProtectionEnabled`, over COM, on a thread
  of its own (MTA, never the blocking pool's apartment), the services proxy
  set to impersonate with `CoSetProxyBlanket` (never `CoInitializeSecurity`:
  it is process-wide). `WBEM_E_INVALID_NAMESPACE` / `WBEM_E_INVALID_CLASS`
  = Defender not installed = off; no instance, no value, a WMI timeout
  (10 s, `WBEM_S_TIMEDOUT`) or any other error = unreadable
  (`defender_realtime`).
- **Nothing spawns a process** (a launched process is what Defender
  scanned): registry through `RegGetValueW` (windows-sys
  `Win32_System_Registry`), WMI through the `windows` 0.58 crate
  (`Win32_System_{Com,Rpc,Wmi}`, the same version sp-decoder / sp-gpu use).
  Never PowerShell, `wmic` or any other process for this.
- **Unreadable is a problem**, never fine (`unreadable:<what>`, "… sa
  nepodarilo zistiť"); the why is in `read_errors`.
- **Bounded**: one read may take `READ_BOUND` (30 s); a read past it reads
  as unreadable and the NEXT check waits for that same read again
  (`Reader`), so a hung WMI never piles up threads nor freezes a stale "ok".

## Where it shows

- `run` (spawned by `lib.rs::start`): a check at once, then every
  `CHECK_EVERY` (60 s), published into the process store (`global()`, a
  `std::sync::RwLock` copied out: no lock across an await).
- `GET /api/v1/status.environment` and `GET /api/v1/environment`:
  `sp_core::prod_env::EnvironmentStatus` = `{checked, ok, note, edition_id,
  product_name, ltsc, edition_allowed, defender_realtime, firewall: {domain,
  private, public}, problems: [{code, text_sk}], read_errors,
  checked_at_ms}`. Before the first check: not checked, not ok. Off Windows:
  checked, ok, `note` "not Windows: …" (Linux tests and the mock are stable).
- Problem codes (stable): `edition_not_allowed`, `defender_realtime_on`,
  `firewall_on:<domain|private|public>`, `unreadable:edition`,
  `unreadable:defender`, `unreadable:firewall:<profile>`; Slovak texts in
  `sp_core::prod_env::EnvProblem::text_sk`, in the verdict's fixed order.
- Logs: ONE ERROR (`production environment: VIOLATED - …`, codes, edition,
  read errors) when a problem appears or the set changes (another edition
  counts), ONE INFO when it clears, one INFO at the first check that holds
  (`Reporter::step` → `LogStep`).
- Dashboard: the HealthBar polls `/api/v1/environment` every 10 s and shows
  the red `health-environment` segment "Prostredie: <first problem>"
  (`(+N)` for more), the rule + every problem as its tooltip
  (`sp_core::prod_env::label`, `HealthTone::Error` → `.health-error`).
  Mock: `/__mock/environment {…}` over a holding default,
  `/__mock/environment-reset`; spec `e2e/health-environment.spec.ts`.

## The gate

`e2e/post-deploy-environment.spec.ts`, in SNV's suite (`post-deploy.config.ts`
takes every `post-deploy*.spec.ts`) and PP's subset (`post-deploy-pp.config.ts`
lists it): polls `/api/v1/status` until `environment.checked` (60 s), then
FAILS on every problem (`code: text`, read errors next to them), on a
missing status, an unchecked one, or a "not Windows" note; then the dashboard
shows no `health-environment` segment once its poll answered, console clean.
Decision `e2e/environment-gate.ts`, unit-tested by `environment-gate.spec.ts`.

A red gate means the BOX is wrong: fix the box (Defender real-time off and
its policy keys, firewall off, the edition), never the gate. Defender's
real-time switch may need Tamper Protection off first (Windows Security
GUI); PP's and SNV's settings of 11.10.2026 are on the ticket.

## Tests

- `crates/sp-core/src/prod_env_tests.rs`: every code and Slovak text, the
  wire shape by name, a missing status, the label (none / one / +1 / +2).
- `crates/sp-server/src/prod_env_tests.rs`: the edition table (IoTEnterprise,
  IoTEnterpriseS, EnterpriseS ok; Enterprise, Professional, Core not), the
  verdict per problem and in order, the firewall policy over the local value,
  Defender's codes, the status of a check, the log steps, `apply` into a
  private store, the bounded reader (a gated read: unreadable, waited for
  again, never started twice; a panicking read), `next_check`, and both
  routes through the real router (serialized on `GLOBAL`: they publish into
  the process store).

## Known limits

- Only Defender is checked as an antivirus; a third-party antivirus is not
  (a candidate: `root\SecurityCenter2` `AntiVirusProduct` lists the
  registered ones on client SKUs). The spawn gate
  (`post-deploy-audio-asio.spec.ts`) still catches its effect on the ASIO
  outputs.
- The Windows glue is compiled and run only on the boxes and the Windows CI
  job's build; its first live evidence is the post-deploy gate on SNV / PP.
