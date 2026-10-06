/**
 * The post-deploy scene driver speaks obs-websocket's MSGPACK subprotocol —
 * Companion's exact encoding (#221 L2b).
 *
 * In Node, `import OBSWebSocket from "obs-websocket-js"` resolves (package.json
 * `exports` → `import` / `require`) to the msgpack build: it offers only
 * `obswebsocket.msgpack`, sends and expects BINARY MessagePack frames, and
 * fails unless the server echoes that subprotocol. Companion's obs-studio
 * module runs in Node, so that is what Companion sends — the first cutover to
 * SongPlayer's facade was refused with HTTP 400 because the facade spoke JSON
 * only (#221 comment 5881650057). Since L2b the facade (:4456) speaks msgpack,
 * and cg OBS (:4455) always did, so the driver keeps the bare import and the
 * post-deploy suite drives the facade exactly like Companion. (L3's
 * workaround imported `obs-websocket-js/json` and hid the gap.)
 *
 * Runs in the ubuntu mock suite: a local msgpack-only obs-websocket stub
 * (Hello → Identify → Identified → one request), no browser, no box.
 */

import { test, expect } from "@playwright/test";
import { WebSocketServer } from "ws";
import type { AddressInfo } from "net";
import { decode, encode } from "@msgpack/msgpack";
import { ObsDriver } from "./obs-driver";
import { AV_PROBE_INPUT } from "./av-sync-probe";

type ObsMessage = { op: number; d: Record<string, unknown> };

test("the scene driver speaks obswebsocket.msgpack, Companion's encoding", async () => {
  const offered: string[] = [];
  const received: ObsMessage[] = [];
  const textFrames: string[] = [];
  const server = new WebSocketServer({
    port: 0,
    host: "127.0.0.1",
    // Like SongPlayer's facade for an obs-websocket-js client in Node: echo
    // msgpack. A JSON-only offer gets no echo, which the client rejects.
    handleProtocols: (protocols) => {
      offered.push(...protocols);
      return protocols.has("obswebsocket.msgpack") ? "obswebsocket.msgpack" : false;
    },
  });
  server.on("connection", (socket) => {
    socket.send(encode({ op: 0, d: { obsWebSocketVersion: "5.0.0", rpcVersion: 1 } }));
    socket.on("message", (raw, isBinary) => {
      if (!isBinary) {
        // The facade closes a text frame on a msgpack session with 4002.
        textFrames.push(raw.toString());
        socket.close(4002, "Your session encoding is set to MsgPack, but a text message was received.");
        return;
      }
      const msg = decode(raw as Buffer) as ObsMessage;
      received.push(msg);
      if (msg.op === 1) {
        socket.send(encode({ op: 2, d: { negotiatedRpcVersion: 1 } }));
      } else if (msg.op === 6) {
        socket.send(
          encode({
            op: 7,
            d: {
              requestType: msg.d.requestType,
              requestId: msg.d.requestId,
              requestStatus: { result: true, code: 100 },
              responseData: { sceneName: "sp-slow", currentProgramSceneName: "sp-slow" },
            },
          }),
        );
      }
    });
  });
  await new Promise<void>((resolve) => server.once("listening", () => resolve()));
  const port = (server.address() as AddressInfo).port;

  try {
    const driver = await ObsDriver.connect(`ws://127.0.0.1:${port}`);
    // A request and its binary response went through.
    expect(await driver.currentProgramScene()).toBe("sp-slow");
    await driver.disconnect();
  } finally {
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }

  expect(offered).toEqual(["obswebsocket.msgpack"]);
  expect(textFrames).toEqual([]);
  expect(received.map((m) => m.op)).toEqual([1, 6]);
  expect(received[1].d.requestType).toBe("GetCurrentProgramScene");
});

/**
 * #221 dev.18: the A/V gate waits for its probe's AUDIO before StartRecord
 * (`obs-audio-wait.ts`). `InputVolumeMeters` is a HIGH-VOLUME event (every
 * active input, every 50 ms) that obs-websocket sends only to a session that
 * asks for it: the recorder asks with a Reidentify for the wait alone and
 * drops it again after, whatever the outcome. A connect never asks for it, so
 * every other suite's connection keeps the default subscriptions.
 *
 * The stub speaks msgpack, like cg OBS. While the session subscribes to
 * InputVolumeMeters it sends one meter event every 10 ms, the probe's peak
 * taken from `peakAt` (by event number).
 */
async function meterStub(peakAt: (event: number) => number) {
  const identify: unknown[] = [];
  const reidentify: number[] = [];
  const state = { metersOn: false, sent: 0 };
  const timers: NodeJS.Timeout[] = [];
  const server = new WebSocketServer({
    port: 0,
    host: "127.0.0.1",
    handleProtocols: (protocols) =>
      protocols.has("obswebsocket.msgpack") ? "obswebsocket.msgpack" : false,
  });
  server.on("connection", (socket) => {
    socket.send(encode({ op: 0, d: { obsWebSocketVersion: "5.0.0", rpcVersion: 1 } }));
    socket.on("message", (raw) => {
      const msg = decode(raw as Buffer) as ObsMessage;
      if (msg.op === 1) {
        identify.push(msg.d.eventSubscriptions);
        socket.send(encode({ op: 2, d: { negotiatedRpcVersion: 1 } }));
      } else if (msg.op === 3) {
        const subs = msg.d.eventSubscriptions as number;
        reidentify.push(subs);
        state.metersOn = (subs & 65536) !== 0;
        socket.send(encode({ op: 2, d: { negotiatedRpcVersion: 1 } }));
      }
    });
    timers.push(
      setInterval(() => {
        if (!state.metersOn) return;
        const peak = peakAt(state.sent++);
        const probe = [peak * 0.7, peak, peak];
        socket.send(
          encode({
            op: 5,
            d: {
              eventType: "InputVolumeMeters",
              eventIntent: 65536,
              eventData: {
                inputs: [
                  { inputName: "cam", inputUuid: "c", inputLevelsMul: [[0.5, 0.6, 0.6]] },
                  { inputName: AV_PROBE_INPUT, inputUuid: "p", inputLevelsMul: [probe, probe] },
                ],
              },
            },
          }),
        );
      }, 10),
    );
  });
  await new Promise<void>((resolve) => server.once("listening", () => resolve()));
  const port = (server.address() as AddressInfo).port;
  return {
    url: `ws://127.0.0.1:${port}`,
    identify,
    reidentify,
    state,
    /** Stop the events and drop every client, so a failed test never
     *  waits on `server.close` for a socket it left open. */
    async close(): Promise<void> {
      for (const t of timers) clearInterval(t);
      for (const client of server.clients) client.terminate();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

test("the audio wait subscribes to InputVolumeMeters only around the wait (#221 dev.18)", async () => {
  // The receiver's warm-up: 20 silent events (~200 ms), then -12 dBFS.
  const stub = await meterStub((n) => (n < 20 ? 0 : 0.25));
  let driver: ObsDriver | null = null;
  try {
    driver = await ObsDriver.connect(stub.url);
    const report = await driver.waitForInputAudio(AV_PROBE_INPUT, {
      holdMs: 100,
      timeoutMs: 5_000,
    });
    // The drop is answered (op 2) before the wait returns: no event after it.
    expect(stub.state.metersOn).toBe(false);
    const sentAtEnd = stub.state.sent;
    await new Promise((r) => setTimeout(r, 60));
    expect(stub.state.sent, "no meter event after the wait").toBe(sentAtEnd);

    expect(stub.identify, "a connect asks for no high-volume event").toEqual([undefined]);
    expect(stub.reidentify).toEqual([4095 | 65536, 4095]);
    expect(report.withInput).toBeGreaterThanOrEqual(20 + 10);
    expect(report.longestStreakMs).toBeGreaterThanOrEqual(100);
    expect(report.loudestDbfs).toBeCloseTo(-12.04, 1);
  } finally {
    await driver?.disconnect();
    await stub.close();
  }
});

test("a failed audio wait drops InputVolumeMeters too, and names the probe (#221 dev.18)", async () => {
  const stub = await meterStub(() => 0); // the probe's audio never flows
  let driver: ObsDriver | null = null;
  try {
    driver = await ObsDriver.connect(stub.url);
    const err = await driver.waitForInputAudio(AV_PROBE_INPUT, { holdMs: 100, timeoutMs: 200 }).then(
      () => null,
      (e: Error) => e,
    );
    expect(err?.message).toContain(`"${AV_PROBE_INPUT}"`);
    expect(err?.message).toContain("did not flow");
    expect(stub.reidentify).toEqual([4095 | 65536, 4095]);
    expect(stub.state.metersOn).toBe(false);
  } finally {
    await driver?.disconnect();
    await stub.close();
  }
});
