/**
 * #229: the PP post-deploy gate's decisions on the node exchange, pure so the
 * mock suite tests them (`peer-probe-gate.spec.ts`); the box reads are
 * `post-deploy-pp.spec.ts`.
 *
 * - `peerSetupFailures`: PP's own identity (`node_name` `pp`, read from PP's
 *   own DB, which no deploy writes) and its peer `snv`, read through
 *   `https://sp.newlevel.media` with a Cloudflare Access service token
 *   (`GET /api/v1/exchange/status`).
 * - `probeFailures`: PP's live read of that peer's catalog
 *   (`POST /api/v1/exchange/probe`).
 * - `transferFailures`: PP's real transfer of that peer's smallest file
 *   artifact, its byte count and sha256 read back at PP
 *   (`POST /api/v1/exchange/probe/transfer`).
 */

/** One entry of `POST /api/v1/exchange/probe` (sp-server `lan::ProbeResult`). */
export interface ProbeResult {
  name: string;
  base_url: string;
  ok: boolean;
  artifacts: number;
  jobs: number;
  latency_ms: number;
  error: string | null;
}

/** One peer of `GET /api/v1/exchange/status` (sp-server `lan::PeerStatus`),
 *  the fields this gate reads. */
export interface PeerStatusView {
  name: string;
  base_url: string;
  has_key: boolean;
  /** A Cloudflare Access service token (`cf_client_id` + `cf_client_secret`)
   *  is configured for this peer. */
  cf_access: boolean;
}

/** `GET /api/v1/exchange/status` (sp-server `lan::ExchangeStatus`), the
 *  fields this gate reads. */
export interface ExchangeStatusView {
  node_name: string | null;
  config_error: string | null;
  peers: PeerStatusView[];
}

/** `url` is `https://<host>[/path]`: the host exactly, over TLS, on the
 *  default port (Cloudflare's public edge). */
function through(url: string, host: string): boolean {
  try {
    const u = new URL(url);
    return u.protocol === "https:" && u.hostname === host && u.port === "";
  } catch {
    return false;
  }
}

function notThrough(peer: string, viaHost: string, baseUrl: string): string {
  return `peer ${peer} is not read through https://${viaHost} (base_url ${baseUrl})`;
}

/**
 * Why PP's exchange settings fail the gate; empty when they pass: the
 * settings hold, this node is `node`, and `peer` is configured with its key,
 * read through `https://<viaHost>` (Cloudflare; that host exactly, TLS, the
 * default port) with a Cloudflare Access service token.
 * PP cannot open a connection to SNV, so the public host is its only path,
 * and Cloudflare Access refuses a request with no token (a 302 to its login
 * page). Settings that do not hold read as the exchange OFF, so their reason
 * is the only failure then.
 */
export function peerSetupFailures(
  s: ExchangeStatusView,
  node: string,
  peer: string,
  viaHost: string,
): string[] {
  if (s.config_error !== null) return [`the exchange settings do not hold: ${s.config_error}`];
  const failures: string[] = [];
  if (s.node_name !== node) {
    const now = s.node_name === null ? "not set" : `"${s.node_name}"`;
    failures.push(`node_name is ${now}, not "${node}" (PP's own DB keeps it; no deploy writes it)`);
  }
  const p = s.peers.find((x) => x.name === peer);
  if (!p) {
    failures.push(`no peer named ${peer} is configured`);
    return failures;
  }
  if (!through(p.base_url, viaHost)) failures.push(notThrough(peer, viaHost, p.base_url));
  if (!p.has_key) failures.push(`peer ${peer} has no key (SNV's peer_api_key)`);
  if (!p.cf_access) {
    failures.push(
      `peer ${peer} has no Cloudflare Access service token (cf_client_id + cf_client_secret ` +
        `are not set): Cloudflare Access refuses PP's read of https://${viaHost} without one ` +
        `(the token is minted by the owner, issue 229)`,
    );
  }
  return failures;
}

/** Why the live read of `peer` fails the gate; empty when it passes: the
 *  peer is configured, read through `https://<viaHost>` (Cloudflare), the
 *  read worked and the catalog lists something. */
export function probeFailures(results: ProbeResult[], peer: string, viaHost: string): string[] {
  const r = results.find((x) => x.name === peer);
  if (!r) return [`no peer named ${peer} is configured`];
  const failures: string[] = [];
  if (!through(r.base_url, viaHost)) failures.push(notThrough(peer, viaHost, r.base_url));
  if (!r.ok) failures.push(`reading ${peer}'s catalog failed: ${r.error ?? "no error text"}`);
  else if (r.artifacts < 1) failures.push(`${peer}'s catalog lists no artifact`);
  return failures;
}

/** The catalog's entry of the artifact a transfer probe fetched (sp-server
 *  `wire::Artifact`), the fields this gate reads. */
export interface ProbedArtifact {
  youtube_id: string;
  kind: string;
  size: number;
  sha256: string;
}

/** One entry of `POST /api/v1/exchange/probe/transfer` (sp-server
 *  `transfer_probe::TransferProbe`): the peer's smallest file artifact,
 *  fetched by PP's own client into a temp dir outside its cache, then read
 *  back (its byte count and sha256). */
export interface TransferProbe {
  name: string;
  base_url: string;
  ok: boolean;
  artifact: ProbedArtifact | null;
  bytes: number | null;
  sha256: string | null;
  latency_ms: number;
  error: string | null;
}

/** Why PP's real transfer from `peer` fails the gate; empty when it passes:
 *  the peer is read through `https://<viaHost>` (Cloudflare), a non-empty
 *  FILE artifact came through whole (the catalog's byte count) and its
 *  sha256 read back at PP is the catalog's. A `metadata` entry is answered
 *  from a row, so it would not prove the file path through Cloudflare. */
export function transferFailures(results: TransferProbe[], peer: string, viaHost: string): string[] {
  const r = results.find((x) => x.name === peer);
  if (!r) return [`no peer named ${peer} is configured`];
  const failures: string[] = [];
  if (!through(r.base_url, viaHost)) failures.push(notThrough(peer, viaHost, r.base_url));
  if (!r.ok) {
    failures.push(`transferring an artifact from ${peer} failed: ${r.error ?? "no error text"}`);
    return failures;
  }
  const a = r.artifact;
  if (a === null) return [...failures, `${peer}: the probe names no artifact it transferred`];
  const what = `${peer}'s ${a.kind} of ${a.youtube_id}`;
  if (a.kind === "metadata") failures.push(`${what} is not a file artifact`);
  if (a.size < 1) failures.push(`${what} is empty: no bytes were transferred`);
  if (r.bytes !== a.size) {
    failures.push(`${what}: ${r.bytes ?? "no"} bytes arrived, the catalog says ${a.size}`);
  }
  if (r.sha256 !== a.sha256) {
    failures.push(`${what}: sha256 ${r.sha256 ?? "none"} arrived, the catalog says ${a.sha256}`);
  }
  return failures;
}
