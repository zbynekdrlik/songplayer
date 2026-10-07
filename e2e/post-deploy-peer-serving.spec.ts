/**
 * #229: SNV serves the node exchange (PP is to read its catalog through
 * Cloudflare). Read-only, and no key is needed:
 *
 * - `GET /api/v1/exchange/status` (LAN API): this node is `snv`, its
 *   exchange settings hold, it serves (`peer_api_key` set), and its rows name
 *   cached songs for the catalog (`catalog.files` > 0; the hasher lists them
 *   as it hashes, `catalog.listed`, so that one is only logged).
 * - `GET /api/v1/peer/catalog` WITHOUT the key answers 401 with no body and
 *   `Cache-Control: no-store`: the peer API is live behind its key guard
 *   (a 200 here would be the dashboard's SPA, i.e. no peer API at all).
 *
 * The live PP → SNV read through Cloudflare is the PP side's gate (its
 * probe, `POST /api/v1/exchange/probe`).
 */
import { test, expect } from "@playwright/test";

interface ExchangeStatus {
  node_name: string | null;
  serving: boolean;
  config_error: string | null;
  catalog: { files: number; listed: number; queued: number } | null;
}

test("SNV serves the node exchange (#229)", async ({ request }) => {
  const resp = await request.get("/api/v1/exchange/status", {
    timeout: 10_000,
  });
  expect(resp.status(), "GET /api/v1/exchange/status").toBe(200);
  const s = (await resp.json()) as ExchangeStatus;
  console.log(`[#229 exchange] ${JSON.stringify(s)}`);
  expect(s.config_error, "the exchange settings hold").toBeNull();
  expect(s.node_name, "SNV's node_name").toBe("snv");
  expect(s.serving, "peer_api_key is set at SNV").toBe(true);
  expect(s.catalog, "the catalog's rows are readable").not.toBeNull();
  expect(s.catalog?.files ?? 0, "SNV's rows name cached songs").toBeGreaterThan(
    0,
  );

  const refused = await request.get("/api/v1/peer/catalog", {
    timeout: 10_000,
  });
  expect(refused.status(), "the peer API refuses a request without the key").toBe(
    401,
  );
  expect(refused.headers()["cache-control"]).toBe("no-store");
  expect((await refused.body()).length, "a refusal has no body").toBe(0);
});
