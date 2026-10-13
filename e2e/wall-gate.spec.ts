import { test, expect } from "@playwright/test";
import {
  FIRST_CHECK_WAIT_MS,
  wallChecked,
  wallFailures,
  wallSummary,
  type WallStatus,
} from "./wall-gate";

// #259: the post-deploy wall gate's pure decision (`wall-gate.ts`), run in
// the mock suite. The box gate itself is `post-deploy-wall.spec.ts` (SNV and
// PP). The wall's state never fails a deploy (the owner's rule: the
// operator's setup is the truth); only a report that cannot read does.

const SPAN_KEY =
  "\\Registry\\Machine\\System\\CurrentControlSet\\Control\\Video\\{49D71F75-AF5A-11EE-BACA-806E6F6E6963}\\0000";

const snv = (over: Partial<WallStatus> = {}): WallStatus => ({
  checked: true,
  state: "ok",
  problems: [],
  read: {
    arena_running: true,
    arena_windows: [{ title: "Display", visible: true }],
    arena_output: {
      advanced: true,
      preset: "snv surround 7680",
      displays: [
        { name: "Display 1", device_id: SPAN_KEY, width: 7680, height: 1080, fullscreen: true },
      ],
    },
    displays: [
      {
        name: "\\\\.\\DISPLAY5",
        device_key: "vdd",
        primary: true,
        x: 0,
        y: 0,
        width: 3840,
        height: 2160,
        hz: 60,
      },
      {
        name: "\\\\.\\DISPLAY1",
        device_key: SPAN_KEY,
        primary: false,
        x: 3840,
        y: 0,
        width: 7680,
        height: 1080,
        hz: 60,
      },
    ],
    nvidia: true,
    connected: [
      { id: "0x80061083", connector: "HDMI" },
      { id: "0x80061086", connector: "DisplayPort" },
      { id: "0x80061087", connector: "DisplayPort" },
    ],
    grids: [{ rows: 1, columns: 3, displays: ["0x80061086", "0x80061087", "0x80061083"] }],
  },
  read_errors: [],
  checked_at_ms: 1_791_608_681_366,
  ...over,
});

test("every state the wall can be in passes: it is the operator's setup", () => {
  for (const state of [
    "ok",
    "arena_off",
    "simple_output",
    "no_display_output",
    "screens_off",
    "display_missing",
    "arena_not_drawing",
  ]) {
    expect(wallFailures(snv({ state })), state).toEqual([]);
  }
});

test("a report that cannot read the box fails, naming why", () => {
  const unknown = snv({
    state: "unknown",
    problems: [
      {
        code: "unreadable:arena_output",
        short_sk: "nedá sa overiť",
        text_sk: "uložený výstup Resolume Arena sa nepodarilo zistiť",
      },
    ],
    read_errors: ["C:\\Users\\Resolume\\Documents\\Resolume Arena\\Presets\\x.xml does not exist"],
  });
  expect(wallFailures(unknown)).toEqual([
    "the wall report could not read the box (state unknown)",
    "unreadable:arena_output: uložený výstup Resolume Arena sa nepodarilo zistiť",
    "read error: C:\\Users\\Resolume\\Documents\\Resolume Arena\\Presets\\x.xml does not exist",
  ]);
  expect(wallFailures(null)).toEqual([
    "/api/v1/wall did not answer (a server without the #259 report?)",
  ]);
  expect(wallFailures(snv({ checked: false }))).toEqual(["the wall report has not run yet"]);
});

test("the gate polls until the first check ran", () => {
  expect(wallChecked(null)).toBe(false);
  expect(wallChecked(undefined)).toBe(false);
  expect(wallChecked(snv({ checked: false }))).toBe(false);
  expect(wallChecked(snv())).toBe(true);
  expect(FIRST_CHECK_WAIT_MS).toBeGreaterThanOrEqual(15_000);
});

test("the summary line tells what Resolume wants and what the box shows", () => {
  expect(wallSummary(snv())).toBe(
    'state=ok arena=true advanced preset="snv surround 7680" wants=[Display 1 7680x1080] ' +
      "windows=[\\\\.\\DISPLAY5 3840x2160@0,0, \\\\.\\DISPLAY1 7680x1080@3840,0] " +
      "nvidia=3 connected, grids=[1x3]",
  );
  const night = snv({
    state: "screens_off",
    problems: [{ code: "screens_off", short_sk: "obrazovky vypnuté", text_sk: "…" }],
    read: { arena_running: true, displays: null, nvidia: false },
  });
  expect(wallSummary(night)).toBe(
    "state=screens_off arena=true output=? windows=? nvidia=none problems=[screens_off]",
  );
  expect(wallSummary(null)).toBe("no answer");
});

test("the mock serves a report the gate passes", async ({ request }) => {
  await request.post("/__mock/wall-reset");
  const resp = await request.get("/api/v1/wall");
  expect(resp.status()).toBe(200);
  const status = (await resp.json()) as WallStatus;
  expect(wallChecked(status)).toBe(true);
  expect(status.state).toBe("simple_output");
  expect(wallFailures(status)).toEqual([]);
  await request.post("/__mock/wall", { data: { state: "unknown" } });
  const unknown = (await (await request.get("/api/v1/wall")).json()) as WallStatus;
  expect(wallFailures(unknown)).toEqual(["the wall report could not read the box (state unknown)"]);
  await request.post("/__mock/wall-reset");
});
