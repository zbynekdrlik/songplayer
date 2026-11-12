/**
 * #233 (PP's SoundGrid pops, 11.10.2026): the production-environment gate.
 *
 * Windows Defender's real-time scan of the processes SongPlayer launches
 * made PP's Waves SoundGrid driver resync (proven A/B/A). The owner's rule:
 * a production PC runs Windows LTSC or Windows 11 IoT Enterprise, with
 * Defender's real-time protection (and any passive / EDR block mode), every
 * other antivirus OFF, the firewall off or (a PC with a Dante leg, #255)
 * dstack's two-network standard, and no deploy passes on a
 * box that runs otherwise. SongPlayer checks it itself
 * (`crate::prod_env`, at the start and every minute) and publishes the
 * verdict on `GET /api/v1/status.environment`; `post-deploy-environment.spec.ts`
 * (SNV's suite and PP's subset) reads it and fails through this pure
 * decision (unit-tested in the mock suite, `environment-gate.spec.ts`).
 * Every violation is fatal: there is no allowance.
 *
 * #255 adds the production PC's audio profile (`status.environment.audio`):
 * with SoundGrid installed, the adapter its driver chose as its port
 * (`LocalLanPort`) found and on an NDIS driver with Waves' settings off;
 * Windows Update's automatic updates off and (lane 2: scripts/setup-prod-pc.ps1
 * applies it at every deploy) its drivers excluded; MMCSS set for Pro Audio.
 * The power scheme is reported only (`audio.notes`), never a failure.
 *
 * #255 item D (dstack, the owner of the two-network standard, 10.11.2026):
 * a PC with a Dante leg (an enabled Dante output's `ip`) follows dstack's
 * `docs/two-networks.md` §6 items 1–3 (`status.environment.two_networks`):
 * one physical adapter on and connected, one default route not through the
 * leg, the leg with no gateway, metric 900 fixed, no DNS registration,
 * NetBIOS off, sharing and client unbound. No Dante leg (PP) = not judged.
 */

/** One problem as the server names it (`sp_core::prod_env::ProblemView`). */
export interface EnvProblemView {
  code: string;
  text_sk: string;
}

/** One adapter setting (`sp_core::prod_env::audio::NicSettingView`). */
export interface NicSettingView {
  setting: string;
  keyword?: string | null;
  value?: string | null;
  ok?: boolean | null;
}

/** One adapter of an audio transport (`sp_core::prod_env::audio::NicView`). */
export interface NicView {
  guid: string;
  name?: string | null;
  description?: string | null;
  service?: string | null;
  driver_model?: "ndis" | "netadaptercx" | null;
  kmdf_library_version?: string | null;
  netadaptercx_keywords?: boolean | null;
  settings?: NicSettingView[];
  settings_ok?: boolean | null;
}

/** #255: the production PC's audio profile (`sp_core::prod_env::audio::AudioProfile`). */
export interface AudioProfile {
  soundgrid?: "not_installed" | "unreadable" | "port_unset" | "port_missing" | "found" | null;
  soundgrid_port_mac?: string | null;
  soundgrid_nic?: NicView | null;
  windows_update?: { no_auto_update?: boolean | null; drivers_excluded?: boolean | null };
  /** #285: every wired adapter (`sp_core::prod_env::audio::WiredNicView`). */
  wired_nics?: { name?: string | null; guid?: string; ok?: boolean | null }[] | null;
  mmcss?: {
    system_responsiveness?: number | null;
    network_throttling_index?: number | null;
    pro_audio_priority?: number | null;
    pro_audio_scheduling_category?: string | null;
  };
  power?: {
    scheme_guid?: string | null;
    scheme_name?: string | null;
    core_parking_min_cores_pct?: number | null;
  };
  notes?: string[];
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
  defender_protections_on?: string[] | null;
  defender_mode?: string | null;
  defender_antivirus_enabled?: boolean | null;
  antivirus_on?: string[] | null;
  firewall?: FirewallStatus;
  audio?: AudioProfile;
  /** #255 item D (null/absent: an older server, or not checked). */
  two_networks?: TwoNetworks | null;
  problems: EnvProblemView[];
  read_errors?: string[];
  checked_at_ms?: number | null;
}

/** #255 item D: one Dante leg (`sp_core::prod_env::network::DanteLegView`). */
export interface DanteLeg {
  ip: string;
  nic?: string | null;
  gateways?: string[] | null;
  metric?: number | null;
  automatic_metric?: boolean | null;
  dns_registration?: boolean | null;
  netbios_options?: number | null;
  file_sharing_bound?: boolean | null;
  client_bound?: boolean | null;
}

/** #255 item D: `status.environment.two_networks`
 * (`sp_core::prod_env::network::TwoNetworksView`). */
export interface TwoNetworks {
  /** The enabled Dante outputs' addresses ([]: no Dante leg; null: unread). */
  dante_ips?: string[] | null;
  physical_enabled?: string[] | null;
  physical_up?: string[] | null;
  default_routes?: string[] | null;
  legs?: DanteLeg[];
}

/** #255 item D: dstack's two-network standard §6 items 1–3, each fact by
 * name, on a PC with a Dante leg; a status without the facts, or whose
 * Dante outputs could not be read, fails. */
function twoNetworkFailures(two: TwoNetworks | null | undefined, shown: (v: unknown) => string): string[] {
  if (two === null || two === undefined) return ["two_networks is missing (a server without #255 item D?)"];
  const ips = two.dante_ips;
  if (!Array.isArray(ips)) return [`two_networks.dante_ips is ${shown(ips)}, not a list`];
  if (ips.length === 0) return [];
  const f: string[] = [];
  const enabled = two.physical_enabled;
  if (!Array.isArray(enabled) || enabled.length !== 1) {
    f.push(`two_networks.physical_enabled is ${shown(enabled)}, not exactly one adapter`);
  } else if (JSON.stringify(two.physical_up) !== JSON.stringify(enabled)) {
    f.push(`two_networks.physical_up is ${shown(two.physical_up)}, not ${shown(enabled)}`);
  }
  const routes = two.default_routes;
  if (!Array.isArray(routes) || routes.length !== 1) {
    f.push(`two_networks.default_routes is ${shown(routes)}, not exactly one`);
  }
  const legs = two.legs ?? [];
  if (legs.length !== ips.length) f.push(`two_networks.legs has ${legs.length}, not ${ips.length}`);
  for (const leg of legs) {
    const at = `two_networks.legs[${leg.ip}]`;
    if (typeof leg.nic !== "string") {
      f.push(`${at}.nic is ${shown(leg.nic)}: no interface holds it`);
      continue;
    }
    if (Array.isArray(routes) && routes.includes(leg.nic)) f.push(`${at}: a default route goes through ${leg.nic}`);
    if (!Array.isArray(leg.gateways) || leg.gateways.length > 0) f.push(`${at}.gateways is ${shown(leg.gateways)}, not []`);
    const want: [keyof DanteLeg, unknown][] = [
      ["metric", 900],
      ["automatic_metric", false],
      ["dns_registration", false],
      ["netbios_options", 2],
      ["file_sharing_bound", false],
      ["client_bound", false],
    ];
    for (const [key, expected] of want) {
      if (leg[key] !== expected) f.push(`${at}.${key} is ${shown(leg[key])}, not ${JSON.stringify(expected)}`);
    }
  }
  return f;
}

/** #255 (dstack#155): a profile's default actions (2 = Allow, 4 = Block). */
export interface FirewallDefaults {
  inbound: number;
  outbound: number;
}

/** `status.environment.firewall` (`sp_core::prod_env::FirewallState`). */
export interface FirewallStatus {
  domain?: boolean | null;
  private?: boolean | null;
  public?: boolean | null;
  /** #255: "off" | "dstack155" | "other" (null: a profile not read). */
  mode?: string | null;
  defaults?: (FirewallDefaults | null)[];
  dstack_group_rules?: number | null;
  block_rules_outside_group?: string[] | null;
}

/** #255 (dstack#155, the owner's ruling of 10.11.2026): the firewall is in
 * one of two states. "off": every profile off (a PC with no Dante leg).
 * "dstack155": dstack's two-network standard, every profile on, each default
 * action Allow (2), every enabled Block rule in the group dstack155, which
 * holds exactly 14. Each fact by name, whatever the mode says. */
function firewallFailures(fw: FirewallStatus | undefined, shown: (v: unknown) => string): string[] {
  const f: string[] = [];
  const mode = fw?.mode ?? null;
  const profiles = ["domain", "private", "public"] as const;
  if (mode === "off") {
    for (const p of profiles) {
      if (fw?.[p] !== false) f.push(`firewall.${p} is ${shown(fw?.[p])}, not false`);
    }
  } else if (mode === "dstack155") {
    profiles.forEach((p, i) => {
      if (fw?.[p] !== true) f.push(`firewall.${p} is ${shown(fw?.[p])}, not true`);
      const d = fw?.defaults?.[i];
      if (d?.inbound !== 2 || d?.outbound !== 2) {
        f.push(`firewall.defaults.${p} is ${shown(d)}, not {"inbound":2,"outbound":2}`);
      }
    });
    if (fw?.dstack_group_rules !== DSTACK_GROUP_RULES) {
      f.push(`firewall.dstack_group_rules is ${shown(fw?.dstack_group_rules)}, not ${DSTACK_GROUP_RULES}`);
    }
    const outside = fw?.block_rules_outside_group;
    if (!Array.isArray(outside) || outside.length > 0) {
      f.push(`firewall.block_rules_outside_group is ${shown(outside)}, not []`);
    }
  } else {
    f.push(`firewall.mode is ${shown(mode)}, not "off" or "dstack155"`);
  }
  return f;
}

/** How long the gate waits for SongPlayer's first check after a restart, ms
 * (a read may take up to the server's 30 s bound). */
/** The enabled Block rules dstack's group `dstack155` holds
 *  (`sp_core::prod_env::firewall::DSTACK_GROUP_RULES`; 16 since #309: Windows'
 *  own mDNS blocked on the Dante leg). */
export const DSTACK_GROUP_RULES = 16;

export const FIRST_CHECK_WAIT_MS = 60_000;

/** The first check has run (the gate polls for it). */
export function environmentChecked(env: EnvironmentStatus | null | undefined): boolean {
  return env !== null && env !== undefined && env.checked === true;
}

/** #255: MMCSS's production values (`vban-out.md`, #210 part 2). */
export const MMCSS_PRODUCTION = {
  system_responsiveness: 0,
  network_throttling_index: 0xffffffff,
  pro_audio_priority: 6,
} as const;

/** The facts a production box shows, each by name, whatever `ok` says
 * (review finding 9: the gate never trusts `ok` alone): an allowed edition,
 * Defender's real-time protection and passive mode off, no other antivirus
 * on, the firewall off or dstack's standard; #255: SoundGrid not installed, or its
 * chosen adapter found on NDIS with its settings off, automatic updates
 * off and drivers excluded from Windows Update (lane 2), MMCSS for Pro
 * Audio. A fact
 * missing from the status (an older server) reads null and fails. */
function factFailures(env: EnvironmentStatus): string[] {
  const f: string[] = [];
  const shown = (value: unknown) => JSON.stringify(value ?? null);
  const want = (name: string, value: unknown, expected: unknown) => {
    if (value !== expected) f.push(`${name} is ${shown(value)}, not ${JSON.stringify(expected)}`);
  };
  want("edition_allowed", env.edition_allowed, true);
  want("defender_realtime", env.defender_realtime, false);
  want("defender_passive", env.defender_passive, false);
  f.push(...firewallFailures(env.firewall, shown));
  if (!Array.isArray(env.antivirus_on) || env.antivirus_on.length > 0) {
    f.push(`antivirus_on is ${shown(env.antivirus_on)}, not []`);
  }
  // #255: behaviour monitoring, IOAV, on-access, by Defender's own status.
  if (!Array.isArray(env.defender_protections_on) || env.defender_protections_on.length > 0) {
    f.push(`defender_protections_on is ${shown(env.defender_protections_on)}, not []`);
  }
  const audio = env.audio;
  const soundgrid = audio?.soundgrid;
  if (soundgrid === "found") {
    want("audio.soundgrid_nic.driver_model", audio?.soundgrid_nic?.driver_model, "ndis");
    want("audio.soundgrid_nic.settings_ok", audio?.soundgrid_nic?.settings_ok, true);
  } else if (soundgrid !== "not_installed") {
    f.push(`audio.soundgrid is ${shown(soundgrid)}, not "found" or "not_installed"`);
  }
  want("audio.windows_update.no_auto_update", audio?.windows_update?.no_auto_update, true);
  // #255 lane 2: a gate since scripts/setup-prod-pc.ps1 applies it at every
  // deploy, before SongPlayer starts (a driver push would undo PP's NDIS swap).
  want("audio.windows_update.drivers_excluded", audio?.windows_update?.drivers_excluded, true);
  for (const [key, expected] of Object.entries(MMCSS_PRODUCTION)) {
    const mmcss = (audio?.mmcss ?? {}) as Record<string, unknown>;
    want(`audio.mmcss.${key}`, mmcss[key], expected);
  }
  const scheduling = audio?.mmcss?.pro_audio_scheduling_category;
  if (typeof scheduling !== "string" || scheduling.trim().toLowerCase() !== "high") {
    f.push(`audio.mmcss.pro_audio_scheduling_category is ${shown(scheduling)}, not "High"`);
  }
  // #285, the owner's rule: every wired adapter, USB ones too. A box always has
  // one (SongPlayer reaches this gate through it), so none read is a failure.
  const wired = audio?.wired_nics;
  if (!Array.isArray(wired) || wired.length === 0) {
    f.push(`audio.wired_nics is ${shown(wired)}, not a list of adapters`);
  } else {
    for (const nic of wired) {
      want(`audio.wired_nics[${nic.name ?? nic.guid}].ok`, nic.ok, true);
    }
  }
  f.push(...twoNetworkFailures(env.two_networks, shown));
  return f;
}

/** #255: the report-only facts that are not the production ones (shown, never
 * a failure): the server's Slovak notes. */
export function environmentNotes(env: EnvironmentStatus | null | undefined): string[] {
  return env?.audio?.notes ?? [];
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
