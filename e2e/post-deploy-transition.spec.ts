/**
 * Post-deploy gate (#347): a playlist → playlist cut crossfades the program
 * audio with no hole and no dip, and the outgoing playlist keeps playing
 * through its fade.
 *
 * SNV's suite (every `post-deploy*.spec.ts`). A temporary VBAN output
 * `e2e-xfade` (48 kHz INT16) to a loopback socket records the program — the
 * real fan-out after the crossfade and the limiter, the path FOH and the
 * in-ears take — from 3 s before a dashboard cut until 3 s after its fade.
 * The verdict is `transition-gate.ts`. The program and the output list are
 * put back afterwards; the probe output is removed before its socket closes
 * (#257: a send to a closed port is a loss on the ledger).
 */

import { test, expect, type APIRequestContext } from "@playwright/test";
import * as dgram from "node:dgram";
import {
  CUT_MAP_OFFSET_MS,
  RATE,
  TRANSPORT_FRACTION,
  decode,
  frameAt,
  medianLevelDb,
  transitionFailures,
  type Arrival,
  type CutRead,
} from "./transition-gate";

const STREAM = "sp-e2e-xfade";
const SLOT_FRAMES = 1600; // one 30 fps boundary at 48 kHz
/** Playlists the gate prefers for its cut, by their NDI name (both play songs on SNV). */
const PREFERRED = ["SP-slow", "SP-fast", "SP-worship", "SP-90s", "SP-presence"];

interface Program {
  source: number | null;
  transition: {
    kind: string;
    duration_ms: number;
    side_fills: number;
    cue_timeouts: number;
    cue_wait_boundaries: number;
  };
  health: { filled_owned: number };
  cut_refused?: { source: number }[] | null;
}

async function program(request: APIRequestContext): Promise<Program> {
  const r = await request.get("/api/v1/program", { timeout: 10_000 });
  expect(r.status(), "GET /api/v1/program").toBe(200);
  return (await r.json()) as Program;
}

async function transport(request: APIRequestContext, pid: number): Promise<string | null> {
  try {
    const r = await request.get("/api/v1/ndi/health", { timeout: 5_000 });
    const rows = (await r.json()) as { playlist_id: number; transport?: string }[];
    return rows.find((x) => x.playlist_id === pid)?.transport ?? null;
  } catch {
    return null;
  }
}

async function cut(request: APIRequestContext, source: number): Promise<void> {
  const r = await request.post("/api/v1/program/cut", { data: { source }, timeout: 10_000 });
  expect(r.status(), `the cut to ${source}`).toBe(200);
}

const counters = (p: Program) => ({
  side_fills: p.transition.side_fills,
  cue_timeouts: p.transition.cue_timeouts,
  filled_owned: p.health.filled_owned,
});

test.describe("program transition post-deploy verification (#347)", () => {
  test("a playlist → playlist cut crossfades the program audio and the outgoing song plays through it", async ({
    page,
    request,
  }) => {
    test.setTimeout(150_000);
    const start = await program(request);
    const original = start.source;
    // Nothing on program cannot be put back through the cut API: change nothing then.
    expect(original, "a source on program to come back to (nothing was changed)").not.toBeNull();
    // A configured hard cut has no fade to judge: say so, never judge it as a broken fade.
    expect(start.transition.kind, "Nastavenia's program_transition is a fade (the gate judges a fade)").toBe("fade");
    const refused = new Set((start.cut_refused ?? []).map((x) => x.source));
    const lists = (await (await request.get("/api/v1/playlists")).json()) as {
      id: number;
      is_active: boolean;
      kind: string;
      ndi_output_name: string;
    }[];
    const pick = PREFERRED.map((n) =>
      lists.find((p) => p.ndi_output_name === n && p.is_active && p.kind === "youtube" && !refused.has(p.id)),
    ).filter((p): p is (typeof lists)[number] => !!p);
    expect(pick.length, `two playable playlists among ${PREFERRED.join(", ")}`).toBeGreaterThanOrEqual(2);
    // The outgoing side: the one on program now if it is a candidate, else the first.
    const a = pick.find((p) => p.id === original) ?? pick[0];
    const b = pick.find((p) => p.id !== a.id)!;
    console.log(`[#347 transition] ${a.ndi_output_name} (${a.id}) → ${b.ndi_output_name} (${b.id}), program was ${original}`);

    // The probe output.
    const stored = (await (await request.get("/api/v1/settings")).json()).audio_outputs as string | undefined;
    expect(stored?.trim(), "a stored output list").toBeTruthy();
    const kept = (JSON.parse(stored as string) as { id: string }[]).filter((e) => !e.id.startsWith("e2e-"));
    const socket = dgram.createSocket({ type: "udp4", recvBufferSize: 4 << 20 });
    const arrivals: Arrival[] = [];
    let recording = false;
    socket.on("message", (m) => {
      if (recording) arrivals.push({ at_ms: Date.now(), bytes: new Uint8Array(m) });
    });
    let seen = 0;
    socket.on("message", () => seen++);
    await new Promise<void>((resolve) => socket.bind(0, "127.0.0.1", () => resolve()));
    const probe = {
      id: "e2e-xfade",
      name: "E2E transition probe",
      type: "vban",
      enabled: true,
      rate: 48000,
      delay_ms: 0,
      vban: { host: "127.0.0.1", port: socket.address().port, stream_name: STREAM, format: "int16" },
    };
    try {
      const add = await request.patch("/api/v1/settings", {
        data: { audio_outputs: JSON.stringify([...kept, probe]) },
      });
      expect(add.status(), "the probe entry is accepted").toBe(204);
      await expect
        .poll(() => seen, { message: "the probe output starts (the list is re-read every 5 s)", timeout: 20_000 })
        .toBeGreaterThan(0);

      // A on program and playing a song for a while (a cut to it starts one).
      if ((await program(request)).source !== a.id) await cut(request, a.id);
      await expect
        .poll(() => transport(request, a.id), { message: `${a.ndi_output_name} plays`, timeout: 40_000 })
        .toBe("Playing");
      // Record until the outgoing song has been audible for the last 2.5 s
      // (a song may open with a silent intro), at most 45 s.
      recording = true;
      await page.waitForTimeout(3_500);
      await expect
        .poll(
          () => {
            const d = decode(arrivals, STREAM);
            const n = d.samples.length / 2;
            return medianLevelDb(d.samples, n - Math.round(2.5 * RATE), n);
          },
          { message: `${a.ndi_output_name}'s song is audible before the cut`, timeout: 45_000, intervals: [500] },
        )
        .toBeGreaterThan(-50);
      const before = await program(request);
      const fadeMs = before.transition.duration_ms;
      const cutAt = Date.now();
      await cut(request, b.id);
      await page.waitForTimeout(Math.round(fadeMs * TRANSPORT_FRACTION) + 200);
      const outgoing = await transport(request, a.id);
      await page.waitForTimeout(Math.round(fadeMs * (1 - TRANSPORT_FRACTION)) + 3_500);
      recording = false;
      const after = await program(request);

      const d = decode(arrivals, STREAM);
      const cutFrame = frameAt(d.marks, cutAt + CUT_MAP_OFFSET_MS);
      expect(cutFrame, "packets arrived after the cut").not.toBeNull();
      const read: CutRead = {
        cutFrame: cutFrame as number,
        fadeFrames: Math.round((fadeMs * RATE) / 1000),
        cueFrames: after.transition.cue_wait_boundaries * SLOT_FRAMES,
        before: counters(before),
        after: counters(after),
        outgoingTransport: outgoing,
      };
      const pre = medianLevelDb(d.samples, read.cutFrame - 3 * RATE, read.cutFrame - RATE / 2);
      console.log(
        `[#347 transition] ${d.samples.length / 2} frames, fade ${fadeMs} ms, cue wait ${after.transition.cue_wait_boundaries}, ` +
          `outgoing level ${pre.toFixed(1)} dBFS, outgoing transport ${outgoing}, counters ${JSON.stringify(read.before)} → ${JSON.stringify(read.after)}`,
      );
      expect([...d.failures, ...transitionFailures(d.samples, read)], "the cut's crossfade").toEqual([]);
    } finally {
      recording = false;
      // The program back where it was (a playlist, OBS manuál or Blank).
      let restoreError: unknown = null;
      if (original !== null) {
        try {
          if ((await program(request)).source !== original) await cut(request, original);
        } catch (e) {
          restoreError = e;
        }
      }
      let current = kept;
      try {
        const now = (await (await request.get("/api/v1/settings")).json()).audio_outputs as string | undefined;
        if (now && now.trim()) current = JSON.parse(now) as { id: string }[];
      } catch {
        current = kept;
      }
      const back = await request.patch("/api/v1/settings", {
        data: { audio_outputs: JSON.stringify(current.filter((e) => !e.id.startsWith("e2e-"))) },
      });
      let stillSending = -1;
      try {
        if (back.status() === 204) {
          // The probe stops before its socket closes (#257: a send to a closed
          // port is a loss on the ledger): wait until 2 s pass with no packet.
          const deadline = Date.now() + 25_000;
          do {
            const mark = seen;
            await page.waitForTimeout(2_000);
            stillSending = seen - mark;
          } while (stillSending > 0 && Date.now() < deadline);
        }
      } finally {
        socket.close();
      }
      expect(back.status(), "the stored list is restored").toBe(204);
      expect(stillSending, "the probe output stopped before its socket closed").toBe(0);
      expect(restoreError, `the program is back on ${original}`).toBeNull();
    }
  });
});
