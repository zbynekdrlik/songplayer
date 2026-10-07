import { test, expect } from "@playwright/test";
import {
  peerSetupFailures,
  probeFailures,
  type ExchangeStatusView,
  type ProbeResult,
} from "./peer-probe-gate";

const HOST = "sp.newlevel.media";

const ok: ProbeResult = {
  name: "snv",
  base_url: "https://sp.newlevel.media",
  ok: true,
  artifacts: 4211,
  jobs: 0,
  latency_ms: 180,
  error: null,
};

const setUp: ExchangeStatusView = {
  node_name: "pp",
  config_error: null,
  peers: [{ name: "snv", base_url: "https://sp.newlevel.media", has_key: true, cf_access: true }],
};

test.describe("peer probe gate (#229)", () => {
  test("a live read through the public host passes", () => {
    expect(probeFailures([ok], "snv", HOST)).toEqual([]);
  });

  test("a missing peer fails", () => {
    expect(probeFailures([], "snv", HOST)).toEqual(["no peer named snv is configured"]);
  });

  test("a read over the LAN is not the Cloudflare path", () => {
    const lan = { ...ok, base_url: "http://10.77.9.201:8920" };
    expect(probeFailures([lan], "snv", HOST)).toEqual([
      "peer snv is not read through https://sp.newlevel.media (base_url http://10.77.9.201:8920)",
    ]);
  });

  test("a host that only starts with the public name is not it", () => {
    const other = { ...ok, base_url: "https://sp.newlevel.media.example.com" };
    expect(probeFailures([other], "snv", HOST)).toHaveLength(1);
    const plain = { ...ok, base_url: "http://sp.newlevel.media" };
    expect(probeFailures([plain], "snv", HOST)).toHaveLength(1);
    const path = { ...ok, base_url: "https://sp.newlevel.media/songplayer" };
    expect(probeFailures([path], "snv", HOST)).toEqual([]);
    const junk = { ...ok, base_url: "not a url" };
    expect(probeFailures([junk], "snv", HOST)).toHaveLength(1);
  });

  test("a refused read fails with its error", () => {
    const refused = {
      ...ok,
      ok: false,
      artifacts: 0,
      error: "refused by Cloudflare Access (HTTP 302) - check the service token",
    };
    expect(probeFailures([refused], "snv", HOST)).toEqual([
      "reading snv's catalog failed: refused by Cloudflare Access (HTTP 302) - check the service token",
    ]);
  });

  test("a refused read with no error text still fails", () => {
    const refused = { ...ok, ok: false, artifacts: 0, error: null };
    expect(probeFailures([refused], "snv", HOST)).toEqual([
      "reading snv's catalog failed: no error text",
    ]);
  });

  test("an empty catalog fails, one artifact passes", () => {
    expect(probeFailures([{ ...ok, artifacts: 0 }], "snv", HOST)).toEqual([
      "snv's catalog lists no artifact",
    ]);
    expect(probeFailures([{ ...ok, artifacts: 1 }], "snv", HOST)).toEqual([]);
  });

  test("another peer's read is not this one's", () => {
    expect(probeFailures([{ ...ok, name: "other" }], "snv", HOST)).toEqual([
      "no peer named snv is configured",
    ]);
  });
});

test.describe("PP's identity and its peer (#229)", () => {
  test("node pp with peer snv through Cloudflare passes", () => {
    expect(peerSetupFailures(setUp, "pp", "snv", HOST)).toEqual([]);
  });

  test("settings that do not hold fail with their reason alone", () => {
    const bad = { node_name: null, config_error: "peers: position 0: bad base_url", peers: [] };
    expect(peerSetupFailures(bad, "pp", "snv", HOST)).toEqual([
      "the exchange settings do not hold: peers: position 0: bad base_url",
    ]);
  });

  test("another node name, or none, fails", () => {
    expect(peerSetupFailures({ ...setUp, node_name: "snv" }, "pp", "snv", HOST)).toEqual([
      'node_name is "snv", not "pp" (PP\'s own DB keeps it; no deploy writes it)',
    ]);
    expect(peerSetupFailures({ ...setUp, node_name: null }, "pp", "snv", HOST)).toEqual([
      'node_name is not set, not "pp" (PP\'s own DB keeps it; no deploy writes it)',
    ]);
  });

  test("no peer snv fails", () => {
    expect(peerSetupFailures({ ...setUp, peers: [] }, "pp", "snv", HOST)).toEqual([
      "no peer named snv is configured",
    ]);
  });

  test("a peer snv off the public host fails", () => {
    const lan = { ...setUp.peers[0], base_url: "http://10.77.9.201:8920" };
    expect(peerSetupFailures({ ...setUp, peers: [lan] }, "pp", "snv", HOST)).toEqual([
      "peer snv is not read through https://sp.newlevel.media (base_url http://10.77.9.201:8920)",
    ]);
  });

  test("a peer snv with no Cloudflare token fails naming the token", () => {
    const bare = { ...setUp.peers[0], cf_access: false };
    const failures = peerSetupFailures({ ...setUp, peers: [bare] }, "pp", "snv", HOST);
    expect(failures).toHaveLength(1);
    expect(failures[0]).toContain("no Cloudflare Access service token");
    expect(failures[0]).toContain("cf_client_id + cf_client_secret");
  });
});
