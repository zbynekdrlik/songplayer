/**
 * #233 (PP's SoundGrid pops, 11.10.2026): the production-environment gate.
 *
 * Windows Defender's real-time scan of the processes SongPlayer launches
 * made PP's Waves SoundGrid driver resync (proven A/B/A). The owner's rule:
 * a production PC runs Windows LTSC or Windows 11 IoT Enterprise, with
 * Defender's real-time protection (and any passive / EDR block mode), every
 * other antivirus and every firewall profile OFF, and no deploy passes on a
 * box that runs otherwise. SongPlayer checks it itself
 * (`crate::prod_env`, at the start and every minute) and publishes the
 * verdict on `GET /api/v1/status.environment`; `post-deploy-environment.spec.ts`
 * (SNV's suite and PP's subset) reads it and fails through this pure
 * decision (unit-tested in the mock suite, `environment-gate.spec.ts`).
 * Every violation is fatal: there is no allowance.
 */

/** One problem as the server names it (`sp_core::prod_env::ProblemView`). */
export interface EnvProblemView {
  code: string;
  text_sk: string;
}

/** `GET /api/v1/status.environment` (`sp_core::prod_env::EnvironmentStatus`). */
export interface EnvironmentStatus {
  checked: boolean;
  ok: boolean;
  note?: string | null;
  edition_id?: string | null;
  product_name?: string | null;
  build_number?: number | null;
  ltsc?: boolean | null;
  edition_allowed?: boolean | null;
  defender_realtime?: boolean | null;
  defender_passive?: boolean | null;
  defender_mode?: string | null;
  defender_antivirus_enabled?: boolean | null;
  antivirus_on?: string[] | null;
  firewall?: { domain?: boolean | null; private?: boolean | null; public?: boolean | null };
  problems: EnvProblemView[];
  read_errors?: string[];
  checked_at_ms?: number | null;
}

/** How long the gate waits for SongPlayer's first check after a restart, ms
 * (a read may take up to the server's 30 s bound). */
export const FIRST_CHECK_WAIT_MS = 60_000;

/** The first check has run (the gate polls for it). */
export function environmentChecked(env: EnvironmentStatus | null | undefined): boolean {
  return env !== null && env !== undefined && env.checked === true;
}

/** The facts a production box shows, each by name, whatever `ok` says
 * (review finding 9: the gate never trusts `ok` alone): an allowed edition,
 * Defender's real-time protection and passive mode off, no other antivirus
 * on, every firewall profile off. A fact missing from the status (an older
 * server) reads null and fails. */
function factFailures(env: EnvironmentStatus): string[] {
  const f: string[] = [];
  const shown = (value: unknown) => JSON.stringify(value ?? null);
  const want = (name: string, value: unknown, expected: boolean) => {
    if (value !== expected) f.push(`${name} is ${shown(value)}, not ${expected}`);
  };
  want("edition_allowed", env.edition_allowed, true);
  want("defender_realtime", env.defender_realtime, false);
  want("defender_passive", env.defender_passive, false);
  for (const profile of ["domain", "private", "public"] as const) {
    want(`firewall.${profile}`, env.firewall?.[profile], false);
  }
  if (!Array.isArray(env.antivirus_on) || env.antivirus_on.length > 0) {
    f.push(`antivirus_on is ${shown(env.antivirus_on)}, not []`);
  }
  return f;
}

/** Why the box fails the gate, each problem by its code and Slovak text,
 * then every fact that is not the production one, then every fact the
 * check could not read; [] = the box holds. A status that is missing, not
 * checked yet, or not checked at all (a server that says it is no Windows
 * box) fails too: the box is never taken as fine unverified. */
export function environmentFailures(env: EnvironmentStatus | null | undefined): string[] {
  if (env === null || env === undefined) {
    return ["/api/v1/status has no environment (a server without the #233 guard?)"];
  }
  if (!env.checked) return ["the production environment has not been checked yet"];
  if (env.note) return [`the production environment is not checked here: ${env.note}`];
  const f = env.problems.map((p) => `${p.code}: ${p.text_sk}`);
  if (!env.ok && f.length === 0) f.push("the environment is not ok, but no problem is named");
  f.push(...factFailures(env));
  if (f.length > 0) {
    for (const e of env.read_errors ?? []) f.push(`read error: ${e}`);
  }
  return f;
}
