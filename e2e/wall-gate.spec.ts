import { test, expect } from "@playwright/test";
import {
  CLIP_WAIT_MS,
  FIRST_CHECK_WAIT_MS,
  NO_SEGMENT_STATES,
  clipFailures,
  clipSettled,
  clipSummary,
  pollClip,
  POLL_INTERVAL_MS,
  pollWall,
  wallChecked,
  wallDashboardMismatch,
  wallFailures,
  wallReadable,
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
      preset_found: true,
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
    // A healthy wall never asks NVAPI.
    nvidia: "not_read",
    connected: null,
    grids: null,
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
    "no_preset",
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
    read_errors: [
      "C:\\Users\\Resolume\\Documents\\Resolume Arena\\Presets\\Advanced Output\\x.xml: <OutputDeviceDisplay> has no width",
    ],
  });
  expect(wallFailures(unknown)).toEqual([
    "the wall report could not read the box (state unknown)",
    "unreadable:arena_output: uložený výstup Resolume Arena sa nepodarilo zistiť",
    "read error: C:\\Users\\Resolume\\Documents\\Resolume Arena\\Presets\\Advanced Output\\x.xml: <OutputDeviceDisplay> has no width",
  ]);
  expect(wallFailures(null)).toEqual([
    "/api/v1/wall did not answer (a server without the #259 report?)",
  ]);
  expect(wallFailures(snv({ checked: false }))).toEqual(["the wall report has not run yet"]);
});

test("the gate polls until a check read the box", () => {
  expect(wallChecked(null)).toBe(false);
  expect(wallChecked(undefined)).toBe(false);
  expect(wallChecked(snv({ checked: false }))).toBe(false);
  expect(wallChecked(snv())).toBe(true);
  expect(wallReadable(null)).toBe(false);
  expect(wallReadable(snv({ checked: false }))).toBe(false);
  expect(wallReadable(snv({ state: "unknown" }))).toBe(false);
  expect(wallReadable(snv({ state: "no_preset" }))).toBe(true);
  expect(wallReadable(snv())).toBe(true);
  expect(FIRST_CHECK_WAIT_MS).toBeGreaterThanOrEqual(15_000);
});

/** A fake clock and a script of reads: each read answers the next status
 * (the last repeated), each wait moves the clock. */
function scripted(reads: (WallStatus | null)[]) {
  let clock = 0;
  let asked = 0;
  const waits: number[] = [];
  return {
    read: async () => reads[Math.min(asked++, reads.length - 1)],
    wait: async (ms: number) => {
      waits.push(ms);
      clock += ms;
    },
    now: () => clock,
    asked: () => asked,
    waits,
  };
}

test("an unknown read is polled past; only a whole window of unknown is judged", async () => {
  const unknown = snv({ state: "unknown" });
  // One failed read, then the box read: the readable one is judged.
  const once = scripted([null, unknown, snv()]);
  const read = await pollWall(once.read, 30_000, once.wait, once.now);
  expect(read?.state).toBe("ok");
  expect(wallFailures(read)).toEqual([]);
  expect(once.asked()).toBe(3);
  expect(once.waits).toEqual([POLL_INTERVAL_MS, POLL_INTERVAL_MS]);
  // A first read that reads is taken at once.
  const first = scripted([snv({ state: "screens_off" })]);
  expect((await pollWall(first.read, 30_000, first.wait, first.now))?.state).toBe("screens_off");
  expect(first.asked()).toBe(1);
  // Unknown for the whole window: read until the deadline, then judged.
  const always = scripted([unknown]);
  const last = await pollWall(always.read, 10_000, always.wait, always.now, 2_000);
  expect(last?.state).toBe("unknown");
  expect(always.asked()).toBe(6);
  expect(always.now()).toBe(10_000);
  expect(wallFailures(last)).toEqual(["the wall report could not read the box (state unknown)"]);
});

test("the dashboard agrees when its marker and its segment are one state", () => {
  expect(NO_SEGMENT_STATES).toEqual(["simple_output", "no_display_output"]);
  expect(wallDashboardMismatch("ok", "ok")).toBeNull();
  expect(wallDashboardMismatch("no_preset", "no_preset")).toBeNull();
  expect(wallDashboardMismatch("simple_output", null)).toBeNull();
  expect(wallDashboardMismatch("no_display_output", null)).toBeNull();
  expect(wallDashboardMismatch("unchecked", null)).toBe(
    'data-wall="unchecked": the bar\'s first wall poll has not landed',
  );
  expect(wallDashboardMismatch("", null)).toBe(
    'data-wall="": the bar\'s first wall poll has not landed',
  );
  expect(wallDashboardMismatch("ok", "screens_off")).toBe(
    "data-wall=ok but the segment shows screens_off",
  );
  expect(wallDashboardMismatch("ok", null)).toBe("data-wall=ok but no segment");
  expect(wallDashboardMismatch("simple_output", "ok")).toBe(
    "data-wall=simple_output but a segment shows ok",
  );
});

test("the summary line tells what Resolume wants and what the box shows", () => {
  expect(wallSummary(snv())).toBe(
    'state=ok arena=true advanced preset="snv surround 7680" wants=[Display 1 7680x1080] ' +
      "windows=[\\\\.\\DISPLAY5 3840x2160@0,0, \\\\.\\DISPLAY1 7680x1080@3840,0] " +
      "nvidia=not read",
  );
  // The span gone: NVAPI read, both its reads; or the grids unread.
  const spanGone = snv({
    state: "display_missing",
    read: {
      arena_running: true,
      displays: null,
      nvidia: "read",
      connected: [
        { id: "0x80061083", connector: "HDMI" },
        { id: "0x80061086", connector: "DisplayPort" },
        { id: "0x80061087", connector: "DisplayPort" },
      ],
      grids: [{ rows: 1, columns: 3, displays: ["0x80061086", "0x80061087", "0x80061083"] }],
    },
  });
  expect(wallSummary(spanGone)).toBe(
    "state=display_missing arena=true output=? windows=? nvidia=3 connected, grids=[1x3]",
  );
  const gridsUnread = snv({
    state: "display_missing",
    read: { ...spanGone.read, grids: null },
  });
  expect(wallSummary(gridsUnread)).toBe(
    "state=display_missing arena=true output=? windows=? nvidia=3 connected, grids=?",
  );
  const night = snv({
    state: "screens_off",
    problems: [{ code: "screens_off", short_sk: "obrazovky vypnuté", text_sk: "…" }],
    read: { arena_running: true, displays: null, nvidia: "no_driver" },
  });
  expect(wallSummary(night)).toBe(
    "state=screens_off arena=true output=? windows=? nvidia=none problems=[screens_off]",
  );
  expect(wallSummary(null)).toBe("no answer");
  // #254: the guard's ends, only once it ended one.
  const nothingEnded = snv({ guard: { ends: 0, last: null, phase: "idle" } });
  expect(wallSummary(nothingEnded)).toBe(wallSummary(snv()));
  const ended = snv({
    state: "arena_off",
    read: { arena_running: false },
    guard: {
      ends: 2,
      last: { at_ms: 1, why: "crash_report", pid: 15176, error: null },
      phase: "relaunching",
    },
  });
  expect(wallSummary(ended)).toBe(
    "state=arena_off arena=false output=? windows=? nvidia=? connected, grids=? " +
      "guard=ended 2x last=crash_report pid 15176 phase=relaunching",
  );
  const refused = snv({
    state: "arena_off",
    read: { arena_running: false },
    guard: {
      ends: 1,
      last: { at_ms: 1, why: "hung", pid: 15176, error: "access denied" },
      phase: "not_back",
    },
  });
  expect(wallSummary(refused)).toBe(
    "state=arena_off arena=false output=? windows=? nvidia=? connected, grids=? " +
      "guard=ended 1x last=hung pid 15176 (failed: access denied) phase=not_back",
  );
});

// #275: SongPlayer's clip in Resolume is logged, never a failure; only a
// server that does not carry the fact fails.
test("the clip's state never fails the gate: only its absence or a reader fault does", () => {
  for (const state of ["running", "none", "unknown"]) {
    expect(clipFailures(snv({ songplayer_clip: { state } })), state).toEqual([]);
  }
  expect(clipFailures(snv())).toEqual([
    "the wall report carries no songplayer_clip (a server without #275?)",
  ]);
  // Nothing to judge before a check ran (wallFailures says why).
  expect(clipFailures(snv({ checked: false }))).toEqual([]);
  expect(clipFailures(null)).toEqual([]);
  // SongPlayer's own reader cannot read Arena's answer: that fails, with why.
  const fault = {
    state: "unknown",
    reader_fault: true,
    read_error: "param 1681395963139: error decoding response body",
  };
  expect(clipFailures(snv({ songplayer_clip: fault }))).toEqual([
    "SongPlayer cannot read Resolume's clip params: param 1681395963139: error decoding response body",
  ]);
  // Arena not answering is no reader fault.
  const noAnswer = {
    state: "unknown",
    reader_fault: false,
    read_error: "Arena does not answer (the circuit breaker is open)",
  };
  expect(clipFailures(snv({ songplayer_clip: noAnswer }))).toEqual([]);
});

test("the clip is polled until it reads or its reader fails", async () => {
  expect(CLIP_WAIT_MS).toBeGreaterThanOrEqual(60_000);
  expect(clipSettled(null)).toBe(false);
  expect(clipSettled(snv())).toBe(false);
  expect(clipSettled(snv({ songplayer_clip: { state: "unknown" } }))).toBe(false);
  expect(clipSettled(snv({ songplayer_clip: { state: "none" } }))).toBe(true);
  expect(clipSettled(snv({ songplayer_clip: { state: "running" } }))).toBe(true);
  expect(clipSettled(snv({ songplayer_clip: { state: "unknown", reader_fault: true } }))).toBe(
    true,
  );
  // Unknown (the composition loading), then it reads: the read is judged.
  const loading = snv({ songplayer_clip: { state: "unknown" } });
  const reads = scripted([loading, loading, snv({ songplayer_clip: { state: "none" } })]);
  const read = await pollClip(reads.read, 90_000, reads.wait, reads.now);
  expect(read?.songplayer_clip?.state).toBe("none");
  expect(reads.asked()).toBe(3);
  // Unknown for the whole window: the last read is judged (and passes).
  const always = scripted([loading]);
  const last = await pollClip(always.read, 10_000, always.wait, always.now, 2_000);
  expect(always.asked()).toBe(6);
  expect(clipFailures(last)).toEqual([]);
});

test("the summary line logs SongPlayer's clip: its state, how long, the clips", () => {
  const yt = { layer: "L# Fill", clip: "Yt music", source: "SP-program-MAX" };
  const bg = { layer: "L # BG", clip: "SNV BG", source: "NDI Source" };
  const running = {
    state: "running",
    clips: [yt],
    ndi_clips: [bg],
    since_ms: 1_000_000,
    now_ms: 1_754_400,
  };
  expect(clipSummary(running)).toBe(
    "clip=running for 754 s [L# Fill / Yt music] ndi=[L # BG / SNV BG]",
  );
  expect(clipSummary({ state: "none", clips: [], ndi_clips: [], since_ms: null, now_ms: 5 })).toBe(
    "clip=none",
  );
  expect(clipSummary({ state: "unknown" })).toBe("clip=unknown");
  expect(
    clipSummary({ state: "unknown", reader_fault: true, read_error: "param 7: no value" }),
  ).toBe('clip=unknown reader_fault error="param 7: no value"');
  expect(clipSummary(undefined)).toBe("");
  expect(wallSummary(snv({ songplayer_clip: running }))).toBe(
    `${wallSummary(snv())} clip=running for 754 s [L# Fill / Yt music] ndi=[L # BG / SNV BG]`,
  );
});

test("the mock serves a report the gate passes", async ({ request }) => {
  await request.post("/__mock/wall-reset");
  const resp = await request.get("/api/v1/wall");
  expect(resp.status()).toBe(200);
  const status = (await resp.json()) as WallStatus;
  expect(wallChecked(status)).toBe(true);
  expect(status.state).toBe("simple_output");
  expect(status.guard).toEqual({ ends: 0, last: null, phase: "idle" });
  expect(wallFailures(status)).toEqual([]);
  // #275: the mock's clip is not read, stamped with the mock's clock.
  expect(status.songplayer_clip?.state).toBe("unknown");
  expect(typeof status.songplayer_clip?.now_ms).toBe("number");
  expect(clipFailures(status)).toEqual([]);
  await request.post("/__mock/wall", { data: { state: "unknown" } });
  const unknown = (await (await request.get("/api/v1/wall")).json()) as WallStatus;
  expect(wallFailures(unknown)).toEqual(["the wall report could not read the box (state unknown)"]);
  await request.post("/__mock/wall-reset");
});
