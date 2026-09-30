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
