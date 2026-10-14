---
paths:
  - "crates/sp-core/src/wall*.rs"
  - "crates/sp-server/src/wall/**"
  - "crates/sp-server/src/api/routes_status.rs"
  - "sp-ui/src/components/health_bar.rs"
  - "e2e/health-wall.spec.ts"
  - "e2e/wall-gate*.ts"
  - "e2e/post-deploy-wall.spec.ts"
  - "e2e/post-deploy-pp.config.ts"
  - "e2e/mock-api.mjs"
---

# The LED wall report (#259 lane 1: read and report; #254: the guard)

## The owner's rule — read this first

ROZHODNUTÉ 13.10.2026 (#259 comment 6142937258), binding for every lane:

- **Our code never enforces a fixed display or Resolume configuration.**
  Whatever the operator sets up — cabling, NVIDIA Surround, Windows'
  layout, Arena's outputs — is the truth, and it changes at will. No
  hard-coded grid, port order, preset or setting anywhere (SNV's A/B/C
  table on the ticket is one day's observation, never a target). The
  `wall_layout` setting idea is DROPPED.
- **Off on every node by default; a deploy to PP (another setup) changes
  nothing on its displays.** This lane acts on NOTHING (#254's guard,
  below, is the one exception, for a DEAD Arena only): it reads and
  reports. A later lane that acts may only restore what a power cycle took,
  once per return, from a setup it learned itself (rules 3-4 of the
  ruling); never a loop, never a takeover of Arena's output.
- **A deliberate change is never an error.** The report says what it sees;
  the post-deploy gate fails only when SongPlayer cannot read the box.
- **`unknown` = only SongPlayer's OWN read failing** (review round 1): an
  I/O error on a file that exists, a file that is no XML Arena writes, a
  Win32 / NVAPI call that fails, a read past its bound. Anything the
  operator's Arena setup can be — no `AdvancedOutput.xml`, no preset named,
  a preset file that does not exist — is a calm state (`no_preset`), never
  `unknown` and never a gate failure.

## #254: the guard ends a crashed or hung Arena (`wall/guard.rs`)

ROZHODNUTÉ 14.10.2026 (#254 comment 6169316862, the owner's "1"): the one
exception to "acts on nothing". Why (SNV 14.10.2026, comment 6167739543):
Arena 7.27.1's render thread crashed, BugSplat's monitor (`BugSplatMonitor.exe`,
Arena's CHILD) showed a window "Crash Report" and waited for a click, the
crashed thread kept the locks the main thread waits on, so Arena never
exited; the operator's hotkey script relaunches only a GONE Arena, and the
wall report read `ok`. Design: comment 6169337303.

- **Reads** (each 5 s check, same walk, `read::guard_facts`): every
  `Arena.exe` (never `Arena-Bridge.exe`, PP's other Resolume) with its
  `GetProcessTimes` (start + kernel/user CPU, a handle opened for
  `PROCESS_QUERY_LIMITED_INFORMATION` only), whether a VISIBLE top-level
  window of it is `IsHungAppWindow` (sends nothing to it), and its crash
  reporters = each `BugSplatMonitor.exe` whose Toolhelp PARENT is that
  Arena (Arena-Bridge's BugSplat window never counts) and whether one shows
  a visible window titled exactly `Crash Report`. Each read keeps its own
  error (`read_errors` lists them; never a state change).
- **The rule** (`guard::decide`, pure): its crash reporter's window → end
  at once; hung AND its CPU time unchanged since the check that first saw
  it hung, for `HUNG_FOR_MS` 30 s → end; NEVER while the CPU time moves (a
  composition load hangs the UI too: every move restarts the watch, so
  does an answer in between, so does a check that read nothing); at most
  ONCE per process = `(pid, start time)` (an id is reused); an Arena whose
  times cannot be read is never acted on; one end per check.
- **The act** (`guard_win.rs`, the ONLY wall file holding
  `TerminateProcess`, pinned by the source scan in `mod_tests.rs`, which
  also bans Arena's clip routes `/clips/`, `/connect`, `/trigger` in every
  wall file): each crash reporter, then Arena; each opened by id and
  checked first (its image's file name, Arena's start time too); a process
  gone already counts as ended; exit code 1. Failures are joined into the
  end's `error`, never retried (once per process).
- **After an end** (`guard::follow`): `relaunching` until a NEW Arena (a key
  not ended) runs → `idle` + one INFO; none `RELAUNCH_WAIT_MS` 60 s after
  the end → `not_back` + one WARN (the state itself is `arena_off`). One
  ERROR per end naming why (`wall guard: ended …` / `could not end …`).
- **Status:** `GET /api/v1/wall` `guard: {ends, last: {at_ms, why:
  crash_report|hung, pid, monitors, error}, phase:
  idle|relaunching|not_back}` (`sp_core::wall::WallGuard`, `serde(default)`:
  an older server reads nothing ended). The HealthBar segment, while the
  last end is pending (`phase != idle`) and the state shows a segment at
  all, reads RED `Stena: Resolume spadol|zamrzol — ukončený, čaká na nové
  spustenie` / `— ukončený, ale znova sa nespustil` / `— nepodarilo sa ho
  ukončiť`, its tooltip ends with the full sentence, `data-guard` = the
  phase. `RULE_SK` names the exception. The gate's `wallSummary` logs
  `guard=ended Nx …` once the guard ended one.
- **Never:** a clip (clips run only from the operator's Stream Deck; after
  a relaunch none runs until he presses a scene — that report is #275), a
  Resolume or display setting, a relaunch of our own (the operator's
  script does it), `Arena-Bridge.exe`.
- **Known effects:** a debugger that suspends Arena (`cdb -pv`) for 30 s
  reads hung + CPU still: the guard ends it — inspect a hung Arena within
  30 s, or before SongPlayer's next check after a crash window shows (it
  ends at once). A BugSplat monitor that is not Arena's child is not
  attributed: only the 30 s hang rule catches that crash.
- **Tests:** `guard_tests.rs` (every branch: the crash window, a hang still
  30 s at its boundary, a hang while the CPU time moves, an answer in
  between, reads that fail, once per process, a reused id, the relaunch
  wait at 59 999 / 60 000 ms, the lingering old process, a failed end),
  `read_tests.rs` (the parent rule, a hidden or other-titled window, each
  read's error), `mod_tests.rs` (the check over a recorder, the status's
  `guard`, the incident end to end from a scripted box), `win_tests.rs`
  (Windows job: a copy of the test binary run as `Arena.exe` /
  `Arena-Bridge.exe` — its parent, times, no hung window, ended only when
  the image and start time match, exit code 1), `sp-core wall_tests.rs`
  (texts, label, wire), `e2e/health-wall.spec.ts` (red until it runs
  again, console clean), `e2e/wall-gate.spec.ts` (the summary).

## The reference: Resolume Arena's OWN saved output setup (`wall/resolume.rs`)

Read from the Documents folder of SongPlayer's own user
(`%USERPROFILE%\Documents`): on SNV SongPlayer runs as the same Windows
user as Arena. A box where they differ reads files that are not Arena's
(none: the simple output is assumed). A Documents folder redirected
elsewhere (OneDrive) is not followed (known limit).

- `Resolume Arena\Preferences\SimpleOutput.xml`: `<SimpleSetup
  advancedModeEnabled="1">` = the advanced output is on; `0` = off; no file =
  Arena's default, the simple output; another value = unreadable.
- `Resolume Arena\Preferences\AdvancedOutput.xml`: `<ScreenSetup
  presetFile="NAME">`, the preset the advanced output was saved as
  (measured on SNV 13.10.2026: 231 bytes, only `<ScreenSetup
  name="ScreenSetup" presetFile="snv surround 7680">` and its version). No
  file, no `<ScreenSetup>`, no or an empty `presetFile`, a NAME that is no
  plain file name (`/`, `\`, `:`, `.`, `..`: never a path out of the
  folder), or no such preset file = no preset saved: `preset_found` false,
  the state `no_preset`.
- `Resolume Arena\Presets\Advanced Output\NAME.xml` (Arena's Advanced
  Output "Save & Close" writes the current setup into the active preset
  file, so it IS Arena's saved setup): every
  `<OutputDeviceDisplay name="Display 1" deviceId="\Registry\Machine\System\CurrentControlSet\Control\Video\{GUID}\0000"
  idHash="…" fullscreen="1" width="7680" height="1080">` (anywhere in the
  tree) is a physical display Arena drives; `name`, `deviceId`, `width`,
  `height` required, `fullscreen` `1` / `0` / absent (= fullscreen). Each
  device id counts once (ASCII case ignored). Spout, NDI, virtual and
  DeckLink devices are other elements and are never read.
- Parsed with roxmltree 0.21.1 (pinned; a DTD is refused, so no entity
  expansion; its MIT notice in `THIRD-PARTY-NOTICES.txt`, pinned by
  `the_installer_notice_carries_the_pinned_roxmltrees_license`). Every file
  is read bounded (`read_bounded`: 1 MiB, UTF-8, a missing file = `None`).
  A parse error or an I/O error on an existing file names its file and is
  `unknown`.

## What else is read (`wall/win.rs`, Windows only, every fn `mutants::skip`)

Nothing spawns a process, nothing on the box changes (#254's one act lives
in `guard_win.rs`, never here).

- **Arena:** a Toolhelp snapshot of the processes (image file names, no
  handle opened); `Arena.exe` (any case) = running; then its top-level
  windows (`EnumWindows` → pid, title, visible, rectangle).
- **Windows' displays:** `EnumDisplayDevicesW(NULL, i)` attached to the
  desktop → name, `DeviceKey` (= Arena's `deviceId`), primary flag;
  `EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS)` → position, size, refresh.
  A display whose settings cannot be read (it detached meanwhile) is left
  out with a DEBUG line, never the whole list failed.
- **Physical pixels:** the display and window reads each hold `DpiAware`
  (`SetThreadDpiAwarenessContext(PER_MONITOR_AWARE_V2)`, the thread's
  previous context restored on drop: the read runs on a pooled blocking
  thread; windows-sys feature `Win32_UI_HiDpi`), so window and display
  rectangles compare as read whatever the process's own awareness or a
  display's scaling. `win_tests.rs` pins the switch and the restore.
- **NVAPI** (`%SystemRoot%\System32\nvapi64.dll`, loaded by that FULL path
  at run time with `libloading`, as sp-ndi loads NDI; never a bare name a
  search order could resolve elsewhere; entry points through
  `nvapi_QueryInterface(id)`, `extern "C"`, status 0 = OK):
  `NvAPI_Initialize` 0x0150E828, `NvAPI_EnumPhysicalGPUs` 0xE5AC921F,
  `NvAPI_GPU_GetConnectedDisplayIds` 0x0078DBA2 (flags 0, a NULL first call
  for the count), `NvAPI_Mosaic_EnumDisplayGrids` 0xDF2887AF (NULL first
  call, then an array with each version set). No display-changing NVAPI id
  is kept (`mod_tests.rs` bans `Mosaic_Set` / `SetCurrentTopo` in every wall
  file). No such file = no NVIDIA driver (`no_driver`, not an error).
  - **Read only when the decision needs it** (`decide::needs_nvidia`: Arena
    runs an advanced output whose preset wants a display Windows does not
    show with its size). A healthy wall never calls the driver; the status
    says `nvidia: "not_read"`.
  - **Two reads, two results** (`read::NvidiaFacts::Read {connected,
    grids}`): a failed `EnumDisplayGrids` never discards the connected
    displays; `not_joined` is told only when BOTH answered.
  - **The session** (`nvapi::Session`, pure and Linux-tested; `win.rs`
    holds one in a static `Mutex`): the DLL is loaded once and kept;
    `NvAPI_Initialize` runs before the first read and again after any read
    that answered a non-OK status (`reset`, the DLL stays loaded); a failed
    load or Initialize is answered from memory, with its reason, for
    `SET_UP_RETRY` = 60 s (never a DLL loaded and freed every 5 s).
- **Layouts** (`wall/nvapi.rs`, pure, Linux-tested against the C header's
  offsets): `NV_MOSAIC_GRID_TOPO_V2` 1832 B, version 0x00020728
  (`MAKE_NVAPI_VERSION` = size + (v << 16), written as a sum: a `|` of
  disjoint bits leaves an equivalent mutant); displays at offset 20 (28 B
  each, 64), the settings at 1812 (`width`, `height`, `bpp`, `freq`);
  `NV_GPU_DISPLAYIDS` 16 B, version 0x00030010, flags bit 2 active, 4 OS
  visible, 6 connected, 17 physically connected (shown only). Connector 4 =
  HDMI, 7 = DisplayPort.
- The whole read runs on the blocking pool behind `prod_env::Reader`
  (`Reader::named(read_now, "wall read")`, bound 10 s): a hung driver reads
  `unknown` and the next check waits for that same read, never a second
  thread.

## The decision (`wall/decide.rs`, pure, exhaustive tests)

Each step reads only the facts it needs; a needed fact that failed =
`unknown`.

| state | when | segment |
|---|---|---|
| `unknown` | a read the decision needs failed | grey "Stena: nedá sa overiť" |
| `arena_off` | no `Arena.exe` | grey "Stena: Resolume nebeží" |
| `simple_output` | the advanced output off (or no `SimpleOutput.xml`) | none |
| `no_preset` | the advanced output on, no preset saved (no `AdvancedOutput.xml`, no `presetFile`, no such preset file) | grey "Stena: Resolume nemá uložený výstup" |
| `no_display_output` | the preset drives no physical display | none |
| `screens_off` | a wanted display has no Windows display with its key, and NVAPI is absent or reports no connected screen | grey "Stena: obrazovky vypnuté" |
| `display_missing` | a wanted display is absent while NVIDIA screens are connected, or present with another size | red, the first problem: "Stena: Resolume chce obrazovku 7680×1080 (Display 1), Windows ju nemá" / "…, Windows ju má 2560×1080" |
| `arena_not_drawing` | every wanted display is there with its size, but no visible Arena window titled exactly `Display` covers a FULLSCREEN one (a windowed output is never checked) | red "Stena: Resolume nekreslí na Display 1" |
| `ok` | Arena draws on every display it wants | green "Stena: OK" |

- The missing problems come first, then the switched-off ones; with any
  missing the state is `display_missing`.
- `not_joined` detail ("obrazovky sú pripojené, ale nie spojené
  (Surround)"): NVAPI's connected read reports screens and its grid read
  (it answered) has no grid of more than one screen. A size mismatch with
  NVAPI unread, absent or its grids unread gets no detail and is never
  `unknown` (the size alone tells it). A display gone with NVAPI's
  connected read failing is `unknown`.
- Known limit of the simple `screens_off` rule: a box whose NVIDIA card
  also drives a screen that stays on (an operator monitor on the card)
  reads `display_missing` while its wall is off. SNV's card drives only the
  wall (its primary, the VDD, is virtual).
- **Known limit, unmeasured: the screens-off rule rests on NVAPI's
  connected bit.** Two of SNV's three screens hang on DisplayPort→HDMI
  adapters; whether an adapter whose screen is powered off still reports
  the screen connected was not measured with the wall off. If it does, a
  night with the wall off reads `display_missing` (red) instead of
  `screens_off`. Measure it on the box (wall off, read `GET /api/v1/wall`'s
  `connected`) before a lane builds on the rule.
- Observed on SNV 13.10.2026 (never a target): preset `snv surround 7680`,
  `Display 1` 7680×1080 on `\\.\DISPLAY1` at x 3840 next to the VDD primary
  3840×2160 at 0,0; NVAPI 1×3 2560×1080 @ 60 Hz of `0x80061086`,
  `0x80061087` (DisplayPort), `0x80061083` (HDMI); Arena's `Display` window
  3840,0 – 11520,1080. After a screen power cycle: the span dissolved
  (three 2560×1080 displays) and Arena's output window did not reopen.

## Where it shows

- `wall::run` (spawned by `lib.rs::start`): a check at once, then every
  5 s, published into `wall::global()` (a `std::sync::RwLock` copied out).
  `Reporter`: ONE WARN when the state turns into a problem (`unknown`,
  `display_missing`, `arena_not_drawing`) or its problems change, ONE INFO
  when it holds again, one INFO between two holding states (the screens off
  or on, Arena started or quit), one INFO at the first check that holds.
- `GET /api/v1/wall` (`api/routes_status.rs::wall`):
  `sp_core::wall::WallStatus` = `{checked, state, problems: [{code,
  short_sk, text_sk}], read: {arena_running, arena_windows, arena_output:
  {advanced, preset, preset_found, displays: [{name, device_id, width,
  height, fullscreen}]}, displays: [{name, device_key, primary, x, y,
  width, height, hz}], nvidia: "not_read" | "no_driver" | "read",
  connected: [{id: "0x80061083", connector, flags}] | null, grids: [{rows,
  columns, displays, width, height, hz}] | null}, read_errors,
  checked_at_ms, guard}` (`guard`: #254 above; `connected` / `grids` null when not read or that read
  failed; its reason in `read_errors`). Problem codes: `unreadable:<arena|arena_output|displays|nvapi>`,
  `screens_off`, `display_absent`, `display_size`, `arena_not_drawing`.
- Dashboard: the HealthBar polls it every 5 s; the `health-wall` segment
  (`data-state` = the state; `sp_core::wall::label`: the first problem's
  `short_sk`, else the state's words; tooltip = the rule, what Arena wants,
  every problem) after the Resolume segment; the bar's `data-wall` =
  `sp_core::wall::marker` (`unchecked` until the first answer): a spec
  asserts it before a segment's absence. Mock: `/__mock/wall {…}` over the
  default `simple_output` (no segment, so no other spec sees the wall),
  `/__mock/wall-reset`; spec `e2e/health-wall.spec.ts`.
- Gate: `e2e/post-deploy-wall.spec.ts` (SNV's suite and PP's subset):
  `pollWall` reads the report every 2 s until a check reads the box (any
  state but `unknown`) or 30 s passed, LOGS the read it ended on
  (`[#259 wall] …`, `wallSummary`) and fails only on no answer / never
  checked / `unknown` for the whole window (`wallFailures`); then ONE
  `expect.poll` reads the bar's `data-wall` and the segment's `data-state`
  together and requires them to agree (`wallDashboardMismatch`: no segment
  for `simple_output` / `no_display_output`, else the same state), console
  clean. All pure, unit-tested by `wall-gate.spec.ts` (a scripted read and
  a fake clock).

## Tests

- `crates/sp-core/src/wall_tests.rs`: display ids, every state's name,
  words, tone, segment and hold rule, every problem's code and texts, the
  marker, the segment and its tooltip, the wire shape.
- `crates/sp-server/src/wall/`: `nvapi_tests.rs` (layouts by the header's
  byte offsets, versions, SNV's grid decoded from its bytes, the session's
  load once / initialise again / 60 s memory of a failed set-up),
  `resolume_tests.rs` (paths, the bounded read over real temp files, every
  attribute rule, a DTD refused), `read_tests.rs` (the walk over
  `read_fake.rs`'s scripted box: NVAPI never asked on SNV, asked once when
  the span is gone; every no-preset case; one NVAPI read failing keeps the
  other), `decide_tests.rs` (every state, every boundary of the window
  cover, the device key in any case, when NVAPI is needed),
  `mod_tests.rs` (status, log steps, the bounded read, the route),
  `win_tests.rs` (Windows job: the process list holds the test process,
  `EnumWindows` reads, the DPI context switched and given back, no Arena on
  the runner).
