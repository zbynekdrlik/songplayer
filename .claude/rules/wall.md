---
paths:
  - "crates/sp-core/src/wall*.rs"
  - "crates/sp-server/src/wall/**"
  - "crates/sp-server/src/resolume/sp_clip*.rs"
  - "crates/sp-server/src/resolume/driver_sp_clips*.rs"
  - "crates/sp-server/src/api/routes_status.rs"
  - "sp-ui/src/components/health_bar.rs"
  - "e2e/health-wall.spec.ts"
  - "e2e/wall-gate*.ts"
  - "e2e/post-deploy-wall.spec.ts"
  - "e2e/post-deploy-pp.config.ts"
  - "e2e/mock-api.mjs"
---

# The LED wall report (#259 lane 1: read and report; #254: the guard; #275: SongPlayer's clip)

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
  Arena AND that started no earlier than it (review round 1: a parent id
  is never updated and outlives its parent, ids are reused, so a reporter
  Arena-Bridge left up could read as the child of a later Songs Arena with
  the Bridge's old id; a child is never older than its parent; a reporter
  whose start is unread, or any reporter of an Arena whose start is unread,
  is not counted), and whether one shows a visible window titled exactly
  `Crash Report`. Each read keeps its own
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
  checked first (its image's file name AND its start time, as read); a process
  gone already counts as ended; exit code 1. Failures are joined into the
  end's `error`, never retried (once per process).
- **After an end** (`guard::follow`): `relaunching` until a NEW Arena (a key
  not ended; one whose start is unread: an id no ended process had) runs →
  `idle` + one INFO; none `RELAUNCH_WAIT_MS` 60 s after
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
  a relaunch none runs until he presses a scene — #275 reports it, below), a
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
  wait at 59 999 / 60 000 ms, the lingering old process, a failed end, an
  unread relaunch), `read_tests.rs` (the parent rule, a reporter older than
  Arena or with its start unread, a hidden or other-titled window, each
  read's error), `mod_tests.rs` (the check over a recorder, the status's
  `guard`, the incident end to end from a scripted box), `win_tests.rs`
  (Windows job: a copy of the test binary run as `Arena.exe` /
  `Arena-Bridge.exe` / `BugSplatMonitor.exe` — its parent, times, no hung
  window, ended only when the image and start time match, the reporter
  first, exit code 1; the copy needs the test binary's side-by-side
  `<exe>.manifest` next to it, or Windows refuses it at load, Common-Controls
  6.0, `gpu-max.md`; each child is read only after its harness printed
  `running 1 test`), `sp-core wall_tests.rs`
  (texts, label, wire), `e2e/health-wall.spec.ts` (red until it runs
  again, console clean), `e2e/wall-gate.spec.ts` (the summary).

## #275: SongPlayer's clip on the wall (`songplayer_clip`)

SNV 14.10.2026: Arena crashed, its script relaunched it at 11:51Z, and no
clip runs after a relaunch until the operator presses a Stream Deck scene.
The wall was black for 5.5 h with songs on program, and the report read
`ok` (it reads WHERE Arena draws, never what). Design: #275 comment
6184990972 (Approach 1).

- **Read by the Resolume host driver**, not by `wall/` (it owns the Arena
  client, the breaker and the 404 = stale map path): each full refresh
  evaluates its own body, a 30 s by-id poll keeps it fresh
  (`resolume-driver.md` "#275"). The pure rule is `resolume/sp_clips.rs`:
  - SongPlayer's clip = `video.description` exactly `SP-program-MAX` or
    `SP-program` (`sp_core::wall_clip::SPOUT_SOURCES`); an NDI clip
    (`NDI Source`) is listed by name only — Arena does not name an NDI
    source, so PP's `SP-program` over NDI reads `none` (the residual; the
    note says "Spout");
  - `running` = one is `connected` (`Connected`, `Connected & previewing`)
    on a layer not bypassed with `video.opacity` > 0 AND its `master` fader
    > 0, its layer group (if any) likewise, the composition's
    `video.opacity` and `master` (each when the body has it) > 0; `none` =
    no such clip (also a composition with none); `unknown` = not loaded (no
    layers), a needed param with no id, a value that did not read, the
    breaker open, or no Resolume host;
  - the masters: read on SNV 15.10.2026 (Arena 7.28.1) by the coordinator —
    every level carries a `master` ParamRange 0..1 with an id (the group
    "G# Kontent" stands at 0.96484375, which shows);
  - **NOT read: `solo` and the crossfader** (both unset on SNV: `solo`
    false on every layer and group, the SP layers' `crossfadergroup`
    "None"). A solo'd OTHER layer, or the composition's crossfader away
    from an A/B-assigned SongPlayer layer, still reads `running`.
- **The record** (`resolume/sp_clip_store.rs`, `global()`): each host's fact,
  combined (running > unknown or no host > none); `since_ms` = when the
  combined state began, on the server's clock. A `running` / `none` counts
  from its start ACROSS `unknown` gaps (`Record::since`, review round 1:
  one unread poll must not restart the note's "(12 min)"); an `unknown`
  counts from when it began. The note itself is hidden for the ~30 s an
  unread poll stands (the state is `unknown` then). Known limit (review
  round 2): the start carries across a gap of ANY length, so `none`, a
  2 h Arena outage, `none` again reads the whole span (a clip that ran
  only while Arena's REST hung but Arena drew is not seen). `read_error` = the
  first host's reason for an unknown, `reader_fault` = any host's reader
  fault.
- **Wire:** `GET /api/v1/wall` adds `songplayer_clip: {state:
  running|none|unknown, clips: [{layer, clip, source}], ndi_clips: […],
  since_ms, now_ms, read_error, reader_fault}`
  (`sp_core::wall_clip::SongplayerClip`, `serde(default)`:
  an older server reads `unknown`). The route copies the record in and
  stamps `now_ms` at the answer. `wall::decide`, the state, the problem
  codes and the wall's logs are UNCHANGED: the clip is never a fault.
- **HealthBar:** amber `Stena: nebeží Spout klip SongPlayeru (12 min)`
  (`HealthTone::Warn`, never red; `SongplayerClip::short_sk`, the span by
  `audio_faults::duration_sk` against `now_ms`) only while (`wall::clip_note`)
  a song plays on program (`wall_clip::song_on_program`: a told playlist,
  WS state `Playing`, with a song — a `Memo` in sp-ui), the state is `none`,
  the wall's state lets it through (`WallState::clip_note_shown`: `ok`,
  `no_preset`, `unknown`; a red state, the screens off, Resolume not
  running keep their words) and no guard end is pending. The tooltip ends
  with the clip's sentence (running: which clips; none: how long, only the
  operator starts clips, NDI cannot be told, the visible NDI clips),
  nothing for `unknown`. The bar's `data-clip` = the state, the segment's
  `data-clip-missing` = the note.
- **Never acts:** no request of the clip read writes anything
  (`sp_clips_tests.rs::the_clip_read_writes_nothing_to_arena` scans
  `sp_clips.rs`, `sp_clip_store.rs`, `driver_sp_clips.rs` for `.put(`,
  `.post(`, `/connect`, `/trigger`, `/clips/`, `set_text`, …).
- **Gate:** `wall-gate.ts` `clipSummary` (in `wallSummary`; the spec's
  `[#275 clip]` lines) LOGS the state; while Resolume runs, `pollClip`
  reads the report until the clip reads or a reader fault shows
  (`CLIP_WAIT_MS` 90 s: the driver reads at its start, a failed fetch is
  retried after 60 s); `clipFailures` fails only on a report with no
  `songplayer_clip` (a server without #275) or a `reader_fault` (SongPlayer
  cannot read Arena's answer: the box is the reader's first real contact
  with Arena's REST, review round 1). `running`, `none`, Arena not
  answering and a composition still loading never fail.
- **Mock:** `WALL_DEFAULT.songplayer_clip` is `unknown` (no existing spec
  sees a note or a sentence); `/__mock/wall {songplayer_clip: {…}}`; the GET
  stamps `now_ms` with the mock's clock. The default replay has playlist 1
  on program with a song; `/__mock/set-playing` takes it off and back.
- **Tests:** `sp-core wall_clip_tests.rs` (states, note, texts, wire, song
  on program), `wall_tests.rs` (the note's precedence, tone, tooltip),
  `resolume/sp_clips_tests.rs` (a composition trimmed from SNV's of
  15.10.2026, `resolume/testdata/`: real names, states and values, 8 of the
  18 Spout clips, param objects and ids derived — the masters the
  coordinator read keep their real id and value (the test file's header):
  running, all disconnected, a bypassed / opacity-0 /
  master-0 layer, the group (bypassed, opacity 0, master 0, SNV's 0.96
  shows), the composition (opacity 0, master 0), no group / no composition
  levels, not loaded, a missing
  id, a value that did not read or has another shape, the answer's typed
  decode, the 30 s due), `sp_clip_store_tests.rs`,
  `driver_sp_clips_tests.rs`, `api/routes_status_tests.rs` (the route),
  `e2e/health-wall.spec.ts` (#275 tests), `e2e/wall-gate.spec.ts`.

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

## The wall's live source: Spout, never SongPlayer's NDI as a "fallback" (#251, measured 18.10.2026)

- **Never switch SNV's wall clips to SongPlayer's NDI `SP-program` to "help" judder.** Measured on SNV's own two wall layers (a blurred backdrop + the sharp picture, Add) with `wall-30` through the CI gate's reader, in long blocks, 18.10.2026 (#251 comment 6231413198):
  - NDI `SP-program`: 61–252 jumps back and 400–660 uneven holds per 40 s;
  - Spout `SP-program-MAX`: 2–6 jumps back and 6–22 uneven.

  Doing it during the 16.10 event made the wall worse.
- **The two layers are the operator's design and stay** (owner, ROZHODNUTÉ 6229662077). They ran on cg OBS's NDI before Spout (Arena's own backups, comment 6231237165). Any fix of the two-read effect is SongPlayer's.
- **The levels come in waves lasting hours:** 436 jumps back with an independent Spout sender and no load on 16.10 morning, 2–6 with MAX on 18.10 morning. Compare arms only interleaved, never across hours.
