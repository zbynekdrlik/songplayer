/**
 * The post-deploy scene driver must speak obs-websocket's JSON subprotocol
 * (#221 L3 regression, CI run 36497336926).
 *
 * Since L3 `ObsDriver` connects to SongPlayer's obs-websocket facade (:4456),
 * which, like Companion's obs-studio module, speaks JSON only: it echoes
 * `obswebsocket.json` and answers a msgpack-only offer with HTTP 400. In Node,
 * `import OBSWebSocket from "obs-websocket-js"` resolves (package.json
 * `exports` → `import`) to the MSGPACK build, which offers only
 * `obswebsocket.msgpack` — so every post-deploy spec's `beforeAll` died with
 * "Unexpected server response: 400". cg OBS itself accepts msgpack, which is
 * why the same driver worked against :4455.
 *
 * Runs in the ubuntu mock suite: a local JSON-only obs-websocket stub (Hello →
 * Identify → Identified), no browser, no box.
 */

import { test, expect } from "@playwright/test";
import { WebSocketServer } from "ws";
import type { AddressInfo } from "net";
import { ObsDriver } from "./obs-driver";

test("the scene driver offers obswebsocket.json and connects to a JSON-only facade", async () => {
  const offered: string[] = [];
  const server = new WebSocketServer({
    port: 0,
    host: "127.0.0.1",
    // Like SongPlayer's `remote::protocol::negotiate_subprotocol`: echo JSON,
    // never agree to msgpack.
    handleProtocols: (protocols) => {
      offered.push(...protocols);
      return protocols.has("obswebsocket.json") ? "obswebsocket.json" : false;
    },
  });
  server.on("connection", (socket) => {
    socket.send(JSON.stringify({ op: 0, d: { obsWebSocketVersion: "5.5.0", rpcVersion: 1 } }));
    socket.on("message", (raw) => {
      const msg = JSON.parse(raw.toString());
      if (msg.op === 1) {
        socket.send(JSON.stringify({ op: 2, d: { negotiatedRpcVersion: 1 } }));
      }
    });
  });
  await new Promise<void>((resolve) => server.once("listening", () => resolve()));
  const port = (server.address() as AddressInfo).port;

  try {
    const driver = await ObsDriver.connect(`ws://127.0.0.1:${port}`);
    await driver.disconnect();
  } finally {
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }

  expect(offered).toContain("obswebsocket.json");
  expect(offered).not.toContain("obswebsocket.msgpack");
});
