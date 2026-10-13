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

# The LED wall report (#259 lane 1: read and report)

## The owner's rule — read this first

ROZHODNUTÉ 13.10.2026 (#259 comment 6142937258), binding for every lane:

- **Our code never enforces a fixed display or Resolume configuration.**
  Whatever the operator sets up — cabling, NVIDIA Surround, Windows'
  layout, Arena's outputs — is the truth, and it changes at will. No
  hard-coded grid, port order, preset or setting anywhere (SNV's A/B/C
  table on the ticket is one day's observation, never a target). The
  `wall_layout` setting idea is DROPPED.
- **Off on every node by default; a deploy to PP (another setup) changes
  nothing on its displays.** This lane acts on NOTHING: it reads and
  reports. A later lane that acts may only restore what a power cycle took,
  once per return, from a setup it learned itself (rules 3-4 of the
  ruling); never a loop, never a takeover of Arena's output.
- **A deliberate change is never an error.** The report says what it sees;
  the post-deploy gate fails only when SongPlayer cannot read the box.

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
  presetFile="NAME">`, the preset the advanced output was saved as; NAME
  must be a plain file name (no `/`, `\`, `:`, not `.` / `..`).
- `Resolume Arena\Presets\Advanced Output\NAME.xml`: every
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
  A parse error names its file.
- Assumption to confirm on the box (the coordinator read SNV's files,
  lane 1 never touched the box): the live advanced setup is the preset
  `AdvancedOutput.xml` names. If Arena turns out to keep unsaved changes
  only in `AdvancedOutput.xml` itself, read the displays from there.

## What else is read (`wall/win.rs`, Windows only, every fn `mutants::skip`)

Nothing spawns a process, nothing on the box changes.

- **Arena:** a Toolhelp snapshot of the processes (image file names, no
  handle opened); `Arena.exe` (any case) = running; then its top-level
  windows (`EnumWindows` → pid, title, visible, rectangle).
- **Windows' displays:** `EnumDisplayDevicesW(NULL, i)` attached to the
  desktop → name, `DeviceKey` (= Arena's `deviceId`), primary flag;
  `EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS)` → position, size, refresh.
  Physical pixels: SongPlayer's process is per-monitor DPI aware (the Tauri
  shell), so window and display rectangles compare as read.
- **NVAPI** (`nvapi64.dll` loaded at run time with `libloading`, as sp-ndi
  loads NDI; entry points through `nvapi_QueryInterface(id)`, `extern "C"`,
  status 0 = OK): `NvAPI_Initialize` 0x0150E828, `NvAPI_EnumPhysicalGPUs`
  0xE5AC921F, `NvAPI_GPU_GetConnectedDisplayIds` 0x0078DBA2 (flags 0, a
  NULL first call for the count), `NvAPI_Mosaic_EnumDisplayGrids`
  0xDF2887AF (NULL first call, then an array with each version set).
  Defined, never called here: `NvAPI_Mosaic_SetDisplayGrids` 0x4D959A89,
  `NvAPI_DISP_GetDisplayIdByDisplayName` 0xAE457190. No
  `System32\nvapi64.dll` = no NVIDIA driver (`None`, not an error). The
  loaded DLL is kept for the process; a failed load or Initialize is tried
  again at the next read.
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
| `no_display_output` | the preset drives no physical display | none |
| `screens_off` | a wanted display has no Windows display with its key, and NVAPI is absent or reports no connected screen | grey "Stena: obrazovky vypnuté" |
| `display_missing` | a wanted display is absent while NVIDIA screens are connected, or present with another size | red, the first problem: "Stena: Resolume chce obrazovku 7680×1080 (Display 1), Windows ju nemá" / "…, Windows ju má 2560×1080" |
| `arena_not_drawing` | every wanted display is there with its size, but no visible Arena window titled exactly `Display` covers a FULLSCREEN one (a windowed output is never checked) | red "Stena: Resolume nekreslí na Display 1" |
| `ok` | Arena draws on every display it wants | green "Stena: OK" |

- The missing problems come first, then the switched-off ones; with any
  missing the state is `display_missing`.
- `not_joined` detail ("obrazovky sú pripojené, ale nie spojené
  (Surround)"): NVAPI reports screens connected and no grid holds more than
  one screen. A size mismatch with NVAPI unread or absent gets no detail and
  is never `unknown` (the size alone tells it).
- Known limit of the simple `screens_off` rule: a box whose NVIDIA card
  also drives a screen that stays on (an operator monitor on the card)
  reads `display_missing` while its wall is off. SNV's card drives only the
  wall (its primary, the VDD, is virtual).
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
  {advanced, preset, displays: [{name, device_id, width, height,
  fullscreen}]}, displays: [{name, device_key, primary, x, y, width,
  height, hz}], nvidia, connected: [{id: "0x80061083", connector, flags}],
  grids: [{rows, columns, displays, width, height, hz}]}, read_errors,
  checked_at_ms}`. Problem codes: `unreadable:<arena|arena_output|displays|nvapi>`,
  `screens_off`, `display_absent`, `display_size`, `arena_not_drawing`.
- Dashboard: the HealthBar polls it every 5 s; the `health-wall` segment
  (`data-state` = the state; `sp_core::wall::label`: the first problem's
  `short_sk`, else the state's words; tooltip = the rule, what Arena wants,
  every problem) after the Resolume segment; the bar's `data-wall` =
  `sp_core::wall::marker` (`unchecked` until the first answer): a spec
  asserts it before a segment's absence. Mock: `/__mock/wall {…}` over the
  default `simple_output` (no segment, so no other spec sees the wall),
  `/__mock/wall-reset`; spec `e2e/health-wall.spec.ts`.
- Gate: `e2e/post-deploy-wall.spec.ts` (SNV's suite and PP's subset): polls
  until checked (30 s), LOGS the state (`[#259 wall] …`, `wall-gate.ts`
  `wallSummary`) and fails only on no answer / never checked / `unknown`
  (`wallFailures`, unit-tested by `wall-gate.spec.ts`); then the bar's
  marker and the segment agree, console clean. A first deploy whose
  readers cannot find Arena's files on a box where Arena runs reads
  `unknown` and fails: that is the reader's assumption being wrong (above),
  never the operator's setup.

## Tests

- `crates/sp-core/src/wall_tests.rs`: display ids, every state's name,
  words, tone, segment and hold rule, every problem's code and texts, the
  marker, the segment and its tooltip, the wire shape.
- `crates/sp-server/src/wall/`: `nvapi_tests.rs` (layouts by the header's
  byte offsets, versions, SNV's grid decoded from its bytes),
  `resolume_tests.rs` (paths, the bounded read over real temp files, every
  attribute rule, a DTD refused), `read_tests.rs` (the walk over
  `read_fake.rs`'s scripted box), `decide_tests.rs` (every state, every
  boundary of the window cover, the device key in any case),
  `mod_tests.rs` (status, log steps, the bounded read, the route),
  `win_tests.rs` (Windows job: the process list holds the test process,
  `EnumWindows` reads, no Arena on the runner).
